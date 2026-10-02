//! The link from this process to the fleet hub's flow stream: how a sink
//! behaves when the hub goes away and comes back, what it re-sends, and what
//! it reports while the hub is away.
//!
//! A process is either [`SinkPolicy::OneShot`] (a CLI invocation: give up on the
//! hub after a few failures, for the rest of the process) or
//! [`SinkPolicy::LongLived`] (the serve daemon: keep probing on a capped
//! backoff, and when the hub answers again publish the records written to the
//! local day files while it was away).

use anyhow::{Context, Result};
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
    /// (#3035) A value a newer darkmux wrote that this build does not know.
    /// Never written by this build; read, never treated as any known value.
    #[serde(other)]
    Unknown,
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
            HubLink::Unverified | HubLink::Connected | HubLink::Unknown => None,
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
        // Work records only: a heartbeat stays local and a telemetry sample is
        // live state, so the next one supersedes any the hub missed (#2101).
        let publishable = crate::reader::action_of(&v)
            .as_ref()
            .is_none_or(|a| a.hub_stream() == Some(crate::HubStream::Work));
        let wanted = ts >= self.since
            && ours
            && publishable
            && crate::flow_record_identity(&v) != self.skip_identity;
        wanted.then(|| (ts.to_string(), v.to_string()))
    }
}

/// The earliest record `ts` that failed to reach the hub, kept in a small
/// state file so the outage outlives the process that saw it.
///
/// Any process whose hub write fails calls [`record`](Self::record) (a
/// one-shot CLI as much as the daemon), and keeps the earliest `ts` it has
/// seen. The long-lived sink reads it when it starts and when the hub answers
/// again, backfills from it, and [`clear_if`](Self::clear_if)s it once the
/// backfill lands. Writes are atomic (temp file + rename) and serialized
/// under an `flock` on a sibling lock file.
///
/// Each `record` moves `seq` forward (to at least the clock's nanoseconds), and
/// a clear keeps the file with `since: null` and the counter intact, so a
/// generation is never reused, even after the file is lost: a backfill clears
/// only the `seq` it read, and a failure recorded while it ran (or after a
/// concurrent backfill cleared) keeps the watermark for the next recovery.
/// Delivery is at-least-once, and readers de-duplicate on record identity.
#[derive(Debug, Clone)]
pub(crate) struct OutageWatermark {
    path: PathBuf,
}

/// What the state file holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stored {
    /// `ts` of the earliest record that failed to publish; `None` once cleared.
    since: Option<String>,
    /// Moved forward by every `record`; never reset by a clear.
    seq: u64,
}

/// An outstanding watermark, as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Marked {
    pub since: String,
    pub seq: u64,
}

impl OutageWatermark {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn lock_path(&self) -> PathBuf {
        let mut name = self.path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        name.push(".lock");
        self.path.with_file_name(name)
    }

    /// The state file's (mtime, length): a cheap change detector for a
    /// periodic check. `None` when the file is absent.
    pub(crate) fn signature(&self) -> Option<(std::time::SystemTime, u64)> {
        let meta = std::fs::metadata(&self.path).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    }

    fn load_stored(&self) -> Option<Stored> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// The outstanding watermark; `None` when absent, unreadable, or cleared.
    pub(crate) fn load(&self) -> Option<Marked> {
        let stored = self.load_stored()?;
        Some(Marked { since: stored.since?, seq: stored.seq })
    }

    fn store(&self, stored: &Stored) -> Result<()> {
        let mut tmp_name = self.path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        tmp_name.push(format!(".tmp-{}", std::process::id()));
        let tmp = self.path.with_file_name(tmp_name);
        // Synced before the rename: a power cut must leave the old file or the
        // whole new one, never a renamed empty file that reads as "no outage".
        let write = || -> std::io::Result<()> {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&serde_json::to_vec(stored)?)?;
            file.sync_all()
        };
        write().with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))
    }

    /// Note that the record stamped `ts` failed to publish. Keeps the earlier
    /// of `ts` and any outstanding watermark.
    pub(crate) fn record(&self, ts: &str) -> Result<()> {
        darkmux_types::flock::with_locked_file(&self.lock_path(), |_| {
            let old = self.load_stored();
            // Past the stored generation, and never below the clock: a file
            // that is missing or unreadable restarts at the clock's reading,
            // not at 1, so a backfill holding an old generation cannot match
            // a newer outage's.
            let seq = old.as_ref().map_or(0, |o| o.seq + 1).max(now_nanos());
            let since = match old.and_then(|o| o.since) {
                Some(prev) => prev.min(ts.to_string()),
                None => ts.to_string(),
            };
            self.store(&Stored { since: Some(since), seq })
        })
    }

    /// Clear the watermark if no `record` has run since `seq` was read. The
    /// counter survives, so no later generation repeats `seq`.
    pub(crate) fn clear_if(&self, seq: u64) -> Result<()> {
        darkmux_types::flock::with_locked_file(&self.lock_path(), |_| match self.load_stored() {
            Some(s) if s.seq == seq && s.since.is_some() => self.store(&Stored { since: None, seq }),
            _ => Ok(()),
        })
    }
}

/// Nanoseconds since the epoch: the floor of a new watermark generation.
fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Where the outage watermark lives: `<darkmux root>/state/hub-outage.json`.
#[cfg(not(any(test, feature = "test-support")))]
pub(crate) fn default_watermark_path() -> PathBuf {
    darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser)
        .root
        .join("state")
        .join("hub-outage.json")
}

/// Test builds never default onto the operator's real darkmux root (same
/// discipline as `audit_dir_default`).
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn default_watermark_path() -> PathBuf {
    let resolved = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser);
    let real_user_root = dirs::home_dir().map(|h| h.join(".darkmux"));
    if real_user_root.as_ref() == Some(&resolved.root) {
        return darkmux_types::paths::test_isolated_dir("state").join("hub-outage.json");
    }
    resolved.root.join("state").join("hub-outage.json")
}

/// Run `sink.tick()` every `interval` on a named thread until `stop` is set.
/// The one place the periodic check is driven, for the daemon and the tests.
pub(crate) fn spawn_tick_thread(
    sink: std::sync::Arc<dyn crate::FlowSink>,
    interval: Duration,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("darkmux-hub-catch-up".to_string())
        .spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Acquire) {
                sink.tick();
                std::thread::sleep(interval);
            }
        })
        .expect("spawning the hub catch-up thread")
}
