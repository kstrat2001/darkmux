//! The link from this process to the fleet hub's flow stream: how a sink
//! behaves when the hub goes away and comes back, what it re-sends, and what
//! it reports while the hub is away.
//!
//! A process is either [`SinkPolicy::OneShot`] (a CLI invocation: give up on the
//! hub after a few failures, for the rest of the process) or
//! [`SinkPolicy::LongLived`] (the serve daemon: keep probing on a capped
//! backoff, and when the hub answers again publish the records written to the
//! local day files while it was away).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What a process expects of its Redis sink, decided once per process where the
/// process starts (`darkmux serve` says `LongLived`; every other entry point
/// keeps the default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkPolicy {
    /// After `REDIS_DISABLE_THRESHOLD` consecutive failures the sink stays off
    /// for the rest of the process. A CLI run is short; a per-write timeout
    /// against an offline peer costs more than the records are worth.
    OneShot,
    /// The sink never gives up: it re-probes on a capped backoff and, once the
    /// hub answers, re-sends what the local day files hold from the outage.
    LongLived,
}

impl SinkPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            SinkPolicy::OneShot => "one_shot",
            SinkPolicy::LongLived => "long_lived",
        }
    }
}

/// Whether this process can currently publish to the hub's flow stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HubLink {
    /// Configured, and no write has been attempted yet.
    Unverified,
    /// The last write reached the hub.
    Connected,
    /// Writes have been failing since the record stamped `since` (RFC 3339 UTC).
    /// `reason` is the root cause of the most recent failure.
    Unreachable { since: String, reason: String },
}

/// Bounds on how often a disabled `LongLived` sink pays a connect attempt.
/// Each probe costs up to `REDIS_CONNECT_TIMEOUT` on the writing thread, so the
/// interval doubles from `min` to `max`: a hub that is down for an hour costs
/// well under 1% of the writer's time, and one that returns is noticed within
/// `max`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProbeBackoff {
    pub min: Duration,
    pub max: Duration,
}

impl Default for ProbeBackoff {
    fn default() -> Self {
        Self { min: Duration::from_secs(2), max: Duration::from_secs(60) }
    }
}

/// The mutable half of the link, behind one lock in the sink.
pub(crate) struct LinkState {
    link: HubLink,
    interval: Duration,
    next_probe: Option<Instant>,
}

impl LinkState {
    pub(crate) fn new() -> Self {
        Self { link: HubLink::Unverified, interval: Duration::ZERO, next_probe: None }
    }

    pub(crate) fn link(&self) -> HubLink {
        self.link.clone()
    }

    /// The `ts` of the first record that failed to publish in the current
    /// outage: the watermark the backfill starts from. `None` when nothing is
    /// owed to the hub.
    pub(crate) fn outage_since(&self) -> Option<&str> {
        match &self.link {
            HubLink::Unreachable { since, .. } => Some(since),
            HubLink::Unverified | HubLink::Connected => None,
        }
    }

    pub(crate) fn record_failure(&mut self, record_ts: &str, reason: String) {
        let since = self.outage_since().map(str::to_string).unwrap_or_else(|| record_ts.to_string());
        self.link = HubLink::Unreachable { since, reason };
    }

    pub(crate) fn record_success(&mut self) {
        self.link = HubLink::Connected;
        self.interval = Duration::ZERO;
        self.next_probe = None;
    }

    /// Schedule the next probe: `min` after the sink first disables, then the
    /// previous interval doubled, capped at `max`.
    pub(crate) fn schedule_probe(&mut self, now: Instant, bounds: ProbeBackoff) {
        self.interval = if self.interval.is_zero() {
            bounds.min
        } else {
            (self.interval * 2).min(bounds.max)
        };
        self.next_probe = Some(now + self.interval);
    }

    pub(crate) fn probe_due(&self, now: Instant) -> bool {
        self.next_probe.is_none_or(|t| now >= t)
    }
}

/// Which records the backfill wants: this machine's records in the current and
/// previous UTC day files with `ts >= since`, minus the record about to be
/// written normally.
///
/// Two day files, because an outage that spans UTC midnight has records in
/// both, and because anything older is past the stream's `MAXLEN ~` retention
/// anyway: re-sending it would only be trimmed again. The scan cost is bounded
/// by that too.
pub(crate) struct Backfill<'a> {
    pub dir: &'a Path,
    pub now_secs: i64,
    pub since: &'a str,
    pub own_uid: Option<&'a str>,
    /// `flow_record_identity` of the record the caller publishes next itself.
    pub skip_identity: &'a str,
    /// The stream's retention cap: re-sending more than it holds is wasted.
    pub cap: Option<usize>,
}

impl Backfill<'_> {
    /// The lines to publish, oldest first (stable within a second, so records
    /// keep their write order).
    pub(crate) fn lines(&self) -> Vec<String> {
        let days = [self.now_secs - 86_400, self.now_secs].map(crate::day_utc_at);
        let mut kept: Vec<(String, String)> = Vec::new();
        for day in days {
            let path: PathBuf = self.dir.join(format!("{day}.jsonl"));
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            kept.extend(text.lines().filter_map(|l| self.keep(l)));
        }
        kept.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some(cap) = self.cap {
            let drop = kept.len().saturating_sub(cap);
            kept.drain(..drop);
        }
        kept.into_iter().map(|(_, line)| line).collect()
    }

    fn keep(&self, line: &str) -> Option<(String, String)> {
        let v = crate::reader::parse_value(line)?;
        let ts = v.get("ts")?.as_str()?;
        let uid = v.get("machine_uid").and_then(|u| u.as_str());
        let ours = uid.is_none() || uid == self.own_uid;
        let publishable = crate::reader::action_of(&v).as_ref().is_none_or(crate::reaches_fleet_stream);
        let wanted = ts >= self.since
            && ours
            && publishable
            && crate::flow_record_identity(&v) != self.skip_identity;
        wanted.then(|| (ts.to_string(), v.to_string()))
    }
}
