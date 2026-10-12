//! The fleet window (#3217): the hub's work and telemetry streams, held by
//! the daemon in a compact form and brought up to date incrementally.
//!
//! `/runs`, `/flow-missions`, the catalog routes and the CLI panels all read
//! the newest [`crate::MAX_FLOW_FILE_RECORDS`] entries of the hub's streams.
//! Before #3217 every read past a 2-second cache re-read and re-parsed the
//! whole window into `serde_json::Value` (five to ten times the record text:
//! ~200 MB measured for the hub's 22 MB), cloned it out of the cache for each
//! request, and a remote `/runs` wrote all of it to a snapshot file for its
//! `run-list` child to parse again.
//!
//! Now each stream is read from the last entry id seen (an entry is parsed
//! once, when it arrives), the window is shared between requests by
//! reference, and each [`FleetRecord`] holds:
//!
//! - its record cut to what the window's readers read: the top-level
//!   [`READ_FIELDS`], and the payload only for the actions whose payload a
//!   reader acts on ([`payload_is_read`]). The bulk of the text is tool
//!   results, reasoning, heartbeats, context and compaction records, which no
//!   `/runs`-shaped reader looks inside;
//! - its de-duplication identity, computed once from the whole record, so
//!   dropping the payload does not change which local records it matches;
//! - the record's text as the hub stores it, which the catalog routes parse
//!   on demand for the records they actually serve ([`FleetRecord::full`]).
//!
//! The window's contents are the same as a fresh `XREVRANGE + - COUNT cap`
//! would return: entries the hub trimmed off the head leave it, it never
//! holds more than the cap, and a stream that no longer holds the last entry
//! read (deleted, rewritten, a hub restarted empty, ids gone backwards,
//! trimmed past it) is read afresh. The hub never deletes from the middle of
//! a stream (no `XDEL` anywhere in darkmux), which is what lets the head trim
//! and the cursor check together stand in for a full re-read.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use serde_json::Value;

use crate::source_state::SourceState;

/// How many hub stream entries this process has parsed into records: the
/// instrument a test reads to prove a repeated fleet read does not re-parse
/// what it already holds.
pub(crate) static STREAM_ENTRY_PARSES: AtomicU64 = AtomicU64::new(0);

/// A hub stream entry id, `<ms>-<seq>`, as a sortable pair.
pub(crate) type StreamId = (u64, u64);

pub(crate) fn parse_stream_id(id: &str) -> Option<StreamId> {
    let (ms, seq) = id.split_once('-')?;
    Some((ms.parse().ok()?, seq.parse().ok()?))
}

/// The de-duplication identity of a flow record: a digest of
/// [`darkmux_flow::flow_record_identity`], the one definition of "the same
/// record" across the fleet stream and the local day files. Held as a digest
/// so a fleet record keeps its identity after its payload is dropped, and so
/// a reader's set of identities costs 32 bytes a record instead of a copy of
/// the record's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordIdentity([u8; 32]);

impl RecordIdentity {
    pub fn of(record: &Value) -> Self {
        Self(*blake3::hash(darkmux_flow::flow_record_identity(record).as_bytes()).as_bytes())
    }

    fn to_hex(self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }

    fn from_hex(hex: &str) -> Option<Self> {
        blake3::Hash::from_hex(hex)
            .ok()
            .map(|h| Self(*h.as_bytes()))
    }
}

/// Whether a reader of the fleet window acts on this record's payload. Every
/// reader, by the payload it reads:
///
/// - the run and mission indexes (`runs.rs`): a `mission.start`'s
///   `config_id`, the `beats` of a `dispatch.start`/`run.start`, the
///   `endpoint` of a `dispatch.*` bookend;
/// - the lifecycle fold (`run_lifecycle.rs`): a `budget.stop`'s `reason`, the
///   `stop_reason` of a `dispatch.error`/`run.error`/`step.error`, a
///   `budget.wait`'s `wait_ms`, a `step.complete`'s `later_step_planned`, a
///   `run.complete`'s status;
/// - the usage fold (`usage_sum.rs`): a usage record's counts, endpoint and
///   models;
/// - `/flow-missions` (`scan_flow_missions`) and the peer-graph owner lookup:
///   none.
///
/// The catalog routes serve whole records, which they read from the entry
/// text ([`FleetRecord::full`]), never from this form. A reader that starts
/// acting on another action's payload, or another top-level field, adds it
/// here or to [`READ_FIELDS`]; the equality test over the realistic fixture
/// (`every_reader_answers_the_same_from_the_compact_form`) is what fails when
/// one does not.
pub(crate) fn payload_is_read(record: &Value) -> bool {
    use darkmux_flow::FlowAction::*;
    crate::usage_sum::is_usage_record(record)
        || matches!(
            darkmux_flow::reader::action_of(record),
            Some(
                MissionStart
                    | RunStart
                    | RunComplete
                    | RunError
                    | DispatchStart
                    | DispatchComplete
                    | DispatchError
                    | StepComplete
                    | StepError
                    | BudgetStop
                    | BudgetWait
            )
        )
}

/// The top-level fields a reader of the fleet window reads: the run and
/// mission indexes (`ts`, `action`, `session_id`, `mission_id`,
/// `execution_id`, `machine_id`, `machine_uid`, `handle`, `model`, `hub_id`),
/// the usage fold (`category`, `source`, and the ids and machine above),
/// `/flow-missions` and the catalog filters (`mission_id`, `session_id`,
/// `machine_uid`, `ts`, `action`), and a day file's header check (`_type`).
/// The payload is kept separately, by [`payload_is_read`]. The identity
/// fields no reader looks at once the identity is computed (`level`,
/// `stage`, `tier`, `phase_id`, `reasoning`, ...) are dropped.
const READ_FIELDS: &[&str] = &[
    "ts",
    "action",
    "session_id",
    "mission_id",
    "execution_id",
    "machine_id",
    "machine_uid",
    "handle",
    "model",
    "category",
    "source",
    "hub_id",
    "_type",
];

/// `record` as the window holds it: the [`READ_FIELDS`], and the payload
/// when [`payload_is_read`], in their order, every object and string
/// allocated to its exact size (a parsed object grows by doubling, so a
/// thirteen-field record reserves sixteen).
fn compact(record: Value) -> Value {
    let keep_payload = payload_is_read(&record);
    let Value::Object(whole) = record else {
        return record;
    };
    let kept = |k: &str| READ_FIELDS.contains(&k) || (keep_payload && k == "payload");
    let mut slim = serde_json::Map::with_capacity(whole.keys().filter(|k| kept(k)).count());
    for (k, v) in whole {
        if kept(&k) {
            slim.insert(k, tight(v));
        }
    }
    Value::Object(slim)
}

/// `value` rebuilt with every object, array and string at its exact size.
fn tight(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (mut k, v) in map {
                k.shrink_to_fit();
                out.insert(k, tight(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(tight)
                .collect::<Vec<_>>()
                .into_boxed_slice()
                .into_vec(),
        ),
        Value::String(mut s) => {
            s.shrink_to_fit();
            Value::String(s)
        }
        other => other,
    }
}

/// One record of the fleet window. See the module doc for what it holds.
#[derive(Debug)]
pub struct FleetRecord {
    value: Value,
    identity: RecordIdentity,
    /// The record's text as the hub stores it; `None` for a record that did
    /// not come off the hub in this process (a caller's own values, which
    /// are held whole, or a snapshot file's compact records).
    text: Option<Box<str>>,
    hub_id: Option<StreamId>,
}

impl FleetRecord {
    /// A hub stream entry, parsed once: `None` when its text is not a
    /// record, the same entries a full read skips.
    pub(crate) fn from_entry(id: &str, text: &str) -> Option<Self> {
        STREAM_ENTRY_PARSES.fetch_add(1, Ordering::Relaxed);
        let mut whole = darkmux_flow::reader::parse_value(text)?;
        crate::stamp_hub_id(&mut whole, id);
        let identity = RecordIdentity::of(&whole);
        Some(Self {
            value: compact(whole),
            identity,
            text: Some(text.into()),
            hub_id: parse_stream_id(id),
        })
    }

    /// A record the caller already holds whole, kept whole.
    pub(crate) fn whole(value: Value) -> Self {
        let identity = RecordIdentity::of(&value);
        let hub_id = hub_id_of(&value);
        Self {
            value,
            identity,
            text: None,
            hub_id,
        }
    }

    /// The record as a fleet reader reads it: whole, or with an unread
    /// payload dropped.
    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn identity(&self) -> RecordIdentity {
        self.identity
    }

    /// The whole record, for a route that serves records rather than
    /// reading them: parsed from the hub's text and stamped with its hub id,
    /// exactly as a full read of the stream returns it.
    pub fn full(&self) -> Value {
        let parsed = self
            .text
            .as_deref()
            .and_then(darkmux_flow::reader::parse_value);
        match parsed {
            Some(mut whole) => {
                if let Some(id) = self.value.get("hub_id").and_then(Value::as_str) {
                    crate::stamp_hub_id(&mut whole, id);
                }
                whole
            }
            None => self.value.clone(),
        }
    }
}

fn hub_id_of(record: &Value) -> Option<StreamId> {
    record
        .get("hub_id")
        .and_then(Value::as_str)
        .and_then(parse_stream_id)
}

/// A fleet read's records, in hub order, shared by reference: a request
/// holds the window it was served without copying it.
#[derive(Debug, Clone, Default)]
pub struct FleetRecords(Arc<Vec<Arc<FleetRecord>>>);

impl FleetRecords {
    pub fn iter(&self) -> impl Iterator<Item = &FleetRecord> {
        self.0.iter().map(|r| r.as_ref())
    }

    /// Each record as a fleet reader reads it ([`FleetRecord::value`]).
    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.iter().map(FleetRecord::value)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn shared(records: &Arc<Vec<Arc<FleetRecord>>>) -> Self {
        Self(Arc::clone(records))
    }
}

/// Two reads hold the same records when each record reads the same and has
/// the same identity, whatever form it came in (off the hub, a snapshot
/// file, a caller's values).
impl PartialEq for FleetRecords {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self
                .iter()
                .zip(other.iter())
                .all(|(a, b)| a.value == b.value && a.identity == b.identity)
    }
}

/// Values a caller holds become whole records (a test fixture, or any
/// caller that read records some other way): nothing is dropped.
impl From<&[Value]> for FleetRecords {
    fn from(values: &[Value]) -> Self {
        Self(Arc::new(
            values
                .iter()
                .cloned()
                .map(|v| Arc::new(FleetRecord::whole(v)))
                .collect(),
        ))
    }
}

impl<const N: usize> From<&[Value; N]> for FleetRecords {
    fn from(values: &[Value; N]) -> Self {
        Self::from(&values[..])
    }
}

impl From<&Vec<Value>> for FleetRecords {
    fn from(values: &Vec<Value>) -> Self {
        Self::from(&values[..])
    }
}

impl From<Vec<Value>> for FleetRecords {
    fn from(values: Vec<Value>) -> Self {
        Self(Arc::new(
            values
                .into_iter()
                .map(|v| Arc::new(FleetRecord::whole(v)))
                .collect(),
        ))
    }
}

impl From<&FleetRecords> for FleetRecords {
    fn from(records: &FleetRecords) -> Self {
        records.clone()
    }
}

/// One record of a snapshot file (#1914): the compact record and its
/// identity, which the record alone can no longer reproduce.
#[derive(serde::Serialize)]
pub(crate) struct SnapshotRecordOut<'a> {
    identity: String,
    record: &'a Value,
}

#[derive(serde::Deserialize)]
pub(crate) struct SnapshotRecordIn {
    identity: String,
    record: Value,
}

impl FleetRecords {
    pub(crate) fn to_snapshot(&self) -> Vec<SnapshotRecordOut<'_>> {
        self.iter()
            .map(|r| SnapshotRecordOut {
                identity: r.identity.to_hex(),
                record: &r.value,
            })
            .collect()
    }

    /// `None` when any identity is malformed: the caller falls back to a
    /// live read rather than de-duplicate against a wrong identity.
    pub(crate) fn from_snapshot(records: Vec<SnapshotRecordIn>) -> Option<Self> {
        let records = records
            .into_iter()
            .map(|r| {
                let identity = RecordIdentity::from_hex(&r.identity)?;
                let hub_id = hub_id_of(&r.record);
                Some(Arc::new(FleetRecord {
                    value: r.record,
                    identity,
                    text: None,
                    hub_id,
                }))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self(Arc::new(records)))
    }
}

/// What an incremental read of one stream found.
#[derive(Debug)]
pub(crate) enum Delta {
    /// The stream still holds the last entry read: `newer` (oldest first)
    /// follow it, `oldest` is the first entry the stream still holds, and
    /// `newest` the id of its newest entry, parsed or not (`None` when there
    /// is nothing newer).
    Continues {
        newer: Vec<Arc<FleetRecord>>,
        oldest: StreamId,
        newest: Option<StreamId>,
    },
    /// It does not (or nothing was read yet): the window is these records
    /// alone, oldest first, and `newest` the stream's newest entry id.
    Replaces {
        records: Vec<Arc<FleetRecord>>,
        newest: Option<StreamId>,
    },
}

/// The window after `delta`: what a fresh read of the newest `cap` entries
/// would hold.
pub(crate) fn advance(
    window: &[Arc<FleetRecord>],
    delta: Delta,
    cap: usize,
) -> Vec<Arc<FleetRecord>> {
    let mut out = match delta {
        Delta::Replaces { records, .. } => records,
        Delta::Continues { newer, oldest, .. } => {
            // The hub trims from the head: what it no longer holds leaves.
            let mut kept: Vec<Arc<FleetRecord>> = window
                .iter()
                .filter(|r| r.hub_id.is_some_and(|id| id >= oldest))
                .cloned()
                .collect();
            kept.extend(newer);
            kept
        }
    };
    if out.len() > cap {
        out.drain(..out.len() - cap);
    }
    out
}

/// One stream's held window. Cloning it copies a pointer to the records.
#[derive(Default, Clone)]
struct StreamWindow {
    /// When it was last brought up to date; `None` before the first read.
    at: Option<SystemTime>,
    /// Which hub and stream it was read from (a digest: the URL can carry a
    /// password). A different source starts over.
    source: Option<[u8; 32]>,
    /// The newest entry id read, parsed or not.
    cursor: Option<StreamId>,
    records: Arc<Vec<Arc<FleetRecord>>>,
}

/// The fleet read's cache: the work stream's window and the telemetry
/// stream's, shared by every reader in the process, and the condition a
/// request waiting on another's refresh sleeps on.
#[derive(Default)]
pub(crate) struct FleetCaches {
    windows: Mutex<FleetWindows>,
    settled: std::sync::Condvar,
}

#[derive(Default)]
struct FleetWindows {
    work: StreamWindow,
    telemetry: StreamWindow,
    /// A request is reading the hub right now (outside the lock).
    refreshing: bool,
    /// When the last refresh failed; cleared by a successful one. Inside the
    /// TTL of a failure nothing asks the hub again.
    failed_at: Option<SystemTime>,
}

/// The longest a request waits on another request's refresh: one round
/// trip's bound, the connect plus one reply. A refresh still going past it
/// (a slow hub, a cold read of the whole window, or a silent one: the
/// incremental read pipelines two commands and redis-rs waits out each
/// reply's bound, so the refresher itself gives up after the connect plus
/// two) does not hold the waiter, which is served the last window instead.
fn refresh_wait() -> std::time::Duration {
    darkmux_flow::REDIS_CONNECT_TIMEOUT + darkmux_flow::REDIS_RESPONSE_TIMEOUT
}

/// What a read of the hub's streams returns. Machine samples are half the
/// entries a full read parses (measured: 300 ms for 10k work entries, 610 ms
/// with 10k samples beside them), and only a session's host charts use them,
/// so the run and mission listings read the work stream alone. (#2101)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FleetScope {
    /// Work records only.
    Work,
    /// Work records and the machine-telemetry stream.
    WithSamples,
}

/// (#1705) How long a fleet window is served without asking the hub for
/// anything newer. The read is incremental now (#3217), so the TTL bounds
/// the round trips a polled lens makes, not a re-parse. A NAMED CONSTANT,
/// not an adaptive guess (the observability doctrine's "cadence is a
/// recorded knob"): 2s is well under any human-perceptible staleness in a
/// lens that renders whole missions, and well over the poll interval it is
/// protecting.
const FLEET_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

/// The fleet records `scope` covers, from `caches`, brought up to date when
/// older than [`FLEET_CACHE_TTL`] at `now`. Explicit cache, clock and URL so
/// a test drives the window without sleeping or touching the environment.
///
/// Single-flight, and never holding the lock across I/O: the first request
/// past the TTL reads the hub with the lock released, on its own copy of the
/// windows (a pointer copy), and installs the result. A request arriving
/// meanwhile waits for that one refresh, at most [`refresh_wait`], and is
/// served what it produced, or the last window when it failed or is still
/// going. A failed refresh is recorded, and inside the TTL of a failure no
/// request asks the hub again: a hub that accepts and never answers costs
/// one bounded attempt per TTL, not one per request, in turn. Each new entry
/// is parsed once.
///
/// Degradation is unchanged from the full-read cache it replaces: no URL is
/// `Off` (a standalone install, not a fault); a failed read serves the
/// last-known-good window as `Stale` with its age, or nothing as
/// `Unavailable` when there is none (#1707: an empty answer during a blip
/// would make a live remote mission vanish from the lenses, the "empty lens
/// indistinguishable from an idle fleet" silence #1705 exists to kill). A
/// failed refresh leaves the window's timestamp untouched, so a window inside
/// the TTL is always a successful read, never a fallback.
pub(crate) fn read_fleet_cached(
    caches: &FleetCaches,
    scope: FleetScope,
    now: SystemTime,
    redis_url: impl FnOnce() -> Option<darkmux_flow::RawRedisUrl>,
) -> crate::FleetRead {
    let within_ttl = |at: Option<SystemTime>| {
        at.is_some_and(|at| crate::wall_clock_cache_is_fresh(at, now, FLEET_CACHE_TTL))
    };
    let fresh = |w: &FleetWindows| {
        within_ttl(w.work.at) && (scope == FleetScope::Work || within_ttl(w.telemetry.at))
    };
    let mut windows = caches
        .windows
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if fresh(&windows) {
        return crate::FleetRead {
            records: windows.assemble(scope),
            state: SourceState::Ok,
        };
    }
    if within_ttl(windows.failed_at) {
        return windows.degraded(scope, now);
    }
    if windows.refreshing {
        windows = match wait_for_refresh(caches, windows, scope, now, fresh, |w| {
            within_ttl(w.failed_at)
        }) {
            Ok(windows) => windows,
            Err(read) => return read,
        };
    }
    let Some(url) = redis_url() else {
        // Not a degradation: a single-machine install has no fleet substrate
        // by design, and warning about it would be the bug.
        return crate::FleetRead {
            records: FleetRecords::default(),
            state: SourceState::Off,
        };
    };
    windows.refreshing = true;
    let (mut work, mut telemetry) = (windows.work.clone(), windows.telemetry.clone());
    drop(windows);
    let settle = Settle(caches);
    let result = refresh(
        url.expose_for_probe(),
        scope,
        now,
        &mut work,
        &mut telemetry,
    );
    let mut windows = caches
        .windows
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    // A stream read successfully keeps its new window even when the other
    // failed; one that failed is unchanged.
    windows.work = work;
    windows.telemetry = telemetry;
    windows.refreshing = false;
    let read = match result {
        Ok(()) => {
            windows.failed_at = None;
            crate::FleetRead {
                records: windows.assemble(scope),
                state: SourceState::Ok,
            }
        }
        Err(e) => {
            windows.failed_at = Some(now);
            // Never interpolate the url (#661 Slice 5): it can carry an
            // inline password. The full error goes to stderr; the wire gets
            // the classification only.
            eprintln!(
                "darkmux serve: fleet-wide flow aggregation could not read Redis ({e}); \
                 serving the last-known-good fleet snapshot, if any, plus this machine's own"
            );
            windows.degraded(scope, now)
        }
    };
    drop(windows);
    drop(settle);
    read
}

/// Wait, at most [`refresh_wait`], for the refresh in flight. `Err` is the
/// answer: the refreshed window, or the last one when it failed or is still
/// going. `Ok` hands the lock back to read the hub: the refresh succeeded but
/// covered the other scope's streams.
fn wait_for_refresh<'a>(
    caches: &'a FleetCaches,
    windows: std::sync::MutexGuard<'a, FleetWindows>,
    scope: FleetScope,
    now: SystemTime,
    fresh: impl Fn(&FleetWindows) -> bool,
    failed_recently: impl Fn(&FleetWindows) -> bool,
) -> Result<std::sync::MutexGuard<'a, FleetWindows>, crate::FleetRead> {
    let windows = caches
        .settled
        .wait_timeout_while(windows, refresh_wait(), |w| w.refreshing)
        .unwrap_or_else(PoisonError::into_inner)
        .0;
    if fresh(&windows) {
        return Err(crate::FleetRead {
            records: windows.assemble(scope),
            state: SourceState::Ok,
        });
    }
    // Never wait twice.
    if windows.refreshing || failed_recently(&windows) {
        return Err(windows.degraded(scope, now));
    }
    Ok(windows)
}

/// Wakes the requests waiting on a refresh when it ends, and clears the
/// in-flight mark even if the refresh panicked, so no request waits on a
/// refresh that will never finish.
struct Settle<'a>(&'a FleetCaches);

impl Drop for Settle<'_> {
    fn drop(&mut self) {
        self.0
            .windows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .refreshing = false;
        self.0.settled.notify_all();
    }
}

/// Bring the streams `scope` covers up to date, on the caller's copies.
fn refresh(
    url: &str,
    scope: FleetScope,
    now: SystemTime,
    work: &mut StreamWindow,
    telemetry: &mut StreamWindow,
) -> anyhow::Result<()> {
    let mut conn = crate::bounded_redis_connection(url, "the fleet window")?;
    let work_stream = darkmux_types::config_access::redis_stream(); // (#875)
    refresh_stream(&mut conn, work, url, &work_stream, now)?;
    if scope == FleetScope::WithSamples {
        let telemetry_stream = darkmux_types::config_access::redis_telemetry_stream(); // (#2101)
        refresh_stream(&mut conn, telemetry, url, &telemetry_stream, now)?;
    }
    Ok(())
}

impl FleetWindows {
    /// The records `scope` covers: the work window itself, or the two
    /// windows merged in hub order (the hub assigns ids, so they order
    /// records across the two streams even where the writers' clocks
    /// disagree; stable, work first on equal ids).
    fn assemble(&self, scope: FleetScope) -> FleetRecords {
        if scope == FleetScope::Work {
            return FleetRecords::shared(&self.work.records);
        }
        let (a, b) = (&self.work.records, &self.telemetry.records);
        let mut out = Vec::with_capacity(a.len() + b.len());
        let (mut a, mut b) = (a.iter().peekable(), b.iter().peekable());
        while let (Some(x), Some(y)) = (a.peek(), b.peek()) {
            let take_a = x.hub_id <= y.hub_id;
            out.extend(if take_a { a.next() } else { b.next() }.cloned());
        }
        out.extend(a.cloned());
        out.extend(b.cloned());
        FleetRecords(Arc::new(out))
    }

    /// The last window `scope` covers, marked `Stale` with its age, or
    /// nothing, `Unavailable`, when it was never read.
    fn degraded(&self, scope: FleetScope, now: SystemTime) -> crate::FleetRead {
        let read_at = match scope {
            FleetScope::Work => self.work.at,
            FleetScope::WithSamples => self.work.at.zip(self.telemetry.at).map(|(a, b)| a.min(b)),
        };
        match read_at {
            Some(at) => crate::FleetRead {
                records: self.assemble(scope),
                state: SourceState::Stale {
                    age_ms: crate::wall_clock_age_ms_at(at, now),
                    detail: crate::FLEET_READ_ERROR_DETAIL,
                },
            },
            None => crate::FleetRead {
                records: FleetRecords::default(),
                state: SourceState::Unavailable {
                    detail: crate::FLEET_READ_ERROR_DETAIL,
                },
            },
        }
    }
}

/// Which hub and stream a window was read from, as a digest (the URL can
/// carry a password).
fn source_key(url: &str, stream: &str) -> [u8; 32] {
    *blake3::hash(format!("{url}\u{1f}{stream}").as_bytes()).as_bytes()
}

/// Bring one stream's window up to date.
fn refresh_stream(
    conn: &mut redis::Connection,
    window: &mut StreamWindow,
    url: &str,
    stream: &str,
    now: SystemTime,
) -> anyhow::Result<()> {
    let source = source_key(url, stream);
    if window.source != Some(source) {
        *window = StreamWindow {
            source: Some(source),
            ..StreamWindow::default()
        };
    }
    let delta = read_delta(conn, stream, window.cursor, crate::MAX_FLOW_FILE_RECORDS)?;
    let newest = match &delta {
        Delta::Continues { newest, .. } => newest.or(window.cursor),
        Delta::Replaces { newest, .. } => *newest,
    };
    window.records = Arc::new(advance(
        &window.records,
        delta,
        crate::MAX_FLOW_FILE_RECORDS,
    ));
    window.cursor = newest;
    window.at = Some(now);
    Ok(())
}

/// Read what `stream` holds past `cursor`, or the newest `cap` entries when
/// there is no cursor or the stream no longer holds it. One round trip for
/// the incremental case (the entries from the cursor up, newest first,
/// beside the stream's oldest id), a second only when it must start over.
fn read_delta(
    conn: &mut redis::Connection,
    stream: &str,
    cursor: Option<StreamId>,
    cap: usize,
) -> anyhow::Result<Delta> {
    use anyhow::Context;
    if let Some((ms, seq)) = cursor {
        let id = format!("{ms}-{seq}");
        let (newest_first, oldest): (redis::Value, redis::Value) = redis::pipe()
            .cmd("XREVRANGE")
            .arg(stream)
            .arg("+")
            .arg(&id)
            .arg("COUNT")
            .arg(cap + 1)
            .cmd("XRANGE")
            .arg(stream)
            .arg("-")
            .arg("+")
            .arg("COUNT")
            .arg(1)
            .query(conn)
            .with_context(|| format!("incremental XREVRANGE on {stream}"))?;
        let entries = entries_of(newest_first)?;
        let oldest = entries_of(oldest)?.first().and_then(entry_id);
        // The range is inclusive: the cursor's own entry comes back last
        // exactly when the stream still holds it and no more than `cap`
        // entries followed it.
        if let (Some(oldest), Some(last)) = (oldest, entries.last().and_then(entry_id)) {
            if last == (ms, seq) {
                let newest = entries
                    .first()
                    .and_then(entry_id)
                    .filter(|n| *n != (ms, seq));
                let newer = records_of(&entries[..entries.len() - 1]);
                return Ok(Delta::Continues {
                    newer,
                    oldest,
                    newest,
                });
            }
        }
    }
    let newest_first: redis::Value = redis::cmd("XREVRANGE")
        .arg(stream)
        .arg("+")
        .arg("-")
        .arg("COUNT")
        .arg(cap)
        .query(conn)
        .with_context(|| format!("XREVRANGE on {stream}"))?;
    let entries = entries_of(newest_first)?;
    let newest = entries.first().and_then(entry_id);
    Ok(Delta::Replaces {
        records: records_of(&entries),
        newest,
    })
}

fn entries_of(reply: redis::Value) -> anyhow::Result<Vec<redis::Value>> {
    match reply {
        redis::Value::Array(v) => Ok(v),
        other => Err(anyhow::anyhow!(
            "unexpected XRANGE response shape: {other:?}"
        )),
    }
}

fn entry_id(entry: &redis::Value) -> Option<StreamId> {
    let redis::Value::Array(pair) = entry else {
        return None;
    };
    parse_stream_id(crate::redis_value_as_str(pair.first()?)?)
}

/// The records newest-first `entries` hold, oldest first. An entry with no
/// `record` field, or one that is not a record, is skipped, as a full read
/// skips it.
fn records_of(entries: &[redis::Value]) -> Vec<Arc<FleetRecord>> {
    let mut out: Vec<Arc<FleetRecord>> = entries
        .iter()
        .filter_map(|entry| {
            let redis::Value::Array(pair) = entry else {
                return None;
            };
            let id = crate::redis_value_as_str(pair.first()?)?;
            let redis::Value::Array(fields) = pair.get(1)? else {
                return None;
            };
            FleetRecord::from_entry(id, crate::extract_record_field(fields)?).map(Arc::new)
        })
        .collect();
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;

    fn rec(id: &str, action: &str) -> Arc<FleetRecord> {
        let text = json!({ "ts": "2026-10-12T00:00:00Z", "action": action, "session_id": id, "payload": { "result": "x".repeat(64), "endpoint": "e" } }).to_string();
        Arc::new(FleetRecord::from_entry(id, &text).expect("a record"))
    }

    fn ids(records: &[Arc<FleetRecord>]) -> Vec<Option<StreamId>> {
        records.iter().map(|r| r.hub_id).collect()
    }

    /// A hub that accepts connections and never answers anything: the
    /// worst a peer that is half up (a wedged Redis, a stalled tailnet
    /// relay) can do. Counts the connections it accepted.
    struct SilentHub {
        url: String,
        accepts: Arc<AtomicU64>,
    }

    /// `answers_setup`: the hub completes redis-rs's connection setup (the
    /// two `CLIENT SETINFO` replies) before going silent, so the connect
    /// succeeds and the read is what stalls.
    fn silent_hub(userinfo: &str, answers_setup: bool) -> SilentHub {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "redis://{userinfo}127.0.0.1:{}",
            listener.local_addr().unwrap().port()
        );
        let accepts = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&accepts);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                counted.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    let mut stream = stream;
                    if answers_setup {
                        let _ = stream.write_all(b"+OK\r\n+OK\r\n");
                    }
                    let mut buf = [0u8; 4096];
                    while matches!(stream.read(&mut buf), Ok(n) if n > 0) {}
                });
            }
        });
        SilentHub { url, accepts }
    }

    /// Run `f` on a thread and wait at most `limit` for it: a test of a
    /// bound that is missing must fail in seconds, not hang the suite.
    fn within<T: Send + 'static>(
        limit: std::time::Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(limit).ok()
    }

    /// One Redis round trip's worth of bound: the connect, then one reply.
    fn one_bound() -> std::time::Duration {
        darkmux_flow::REDIS_CONNECT_TIMEOUT + darkmux_flow::REDIS_RESPONSE_TIMEOUT
    }

    /// The connect itself is bounded against a hub that accepts and never
    /// answers: redis-rs reads its connection-setup replies (`CLIENT
    /// SETINFO`, `AUTH`, `SELECT`) before any read timeout is set, so it
    /// waited on them forever. Plain, with a password, with a user, and with
    /// a database.
    #[test]
    fn a_connect_to_a_hub_that_never_answers_fails_within_its_bound() {
        for userinfo in ["", ":s3cret@", "user:s3cret@"] {
            let hub = silent_hub(userinfo, false);
            let url = hub.url.clone();
            let url_db = format!("{url}/3");
            for url in [url, url_db] {
                let started = std::time::Instant::now();
                // Connect, then one command: whichever phase the silence
                // stops, the whole costs one round trip's bound.
                let result = within(3 * one_bound(), move || {
                    let mut conn = crate::bounded_redis_connection(&url, "a probe")?;
                    redis::cmd("PING").query::<String>(&mut conn)?;
                    anyhow::Ok(())
                });
                let elapsed = started.elapsed();
                let result = result.unwrap_or_else(|| {
                    panic!("the connect ({userinfo}) waited past {:?}", 3 * one_bound())
                });
                assert!(result.is_err(), "a hub that never answers cannot be read");
                assert!(
                    elapsed < one_bound() + std::time::Duration::from_millis(300),
                    "the connect ({userinfo}) took {elapsed:?}"
                );
            }
        }
    }

    /// A window read long enough ago to be past the TTL, from `url`.
    fn seeded_past_ttl(url: &str, read_at: SystemTime) -> FleetCaches {
        let caches = FleetCaches::default();
        {
            let mut windows = caches.windows.lock().unwrap();
            windows.work = StreamWindow {
                at: Some(read_at),
                source: Some(source_key(
                    url,
                    &darkmux_types::config_access::redis_stream(),
                )),
                cursor: Some((1, 0)),
                records: Arc::new(vec![rec("1-0", "dispatch.tool")]),
            };
        }
        caches
    }

    /// Eight requests past the TTL against a hub that answers the connect and
    /// never a read (the slowest failure: the connect, then both pipelined
    /// replies' bounds): one of them asks the hub, the others wait at most for that one attempt, and
    /// none waits behind another's I/O in turn. Every one is served the last
    /// window, marked stale, and the hub saw one connection. A read just
    /// after, inside the TTL of the failure, does not ask again.
    #[test]
    fn concurrent_reads_never_queue_behind_a_hub_that_never_answers() {
        let hub = silent_hub("", true);
        let t0 = SystemTime::now();
        let caches = Arc::new(seeded_past_ttl(&hub.url, t0));
        let now = t0 + std::time::Duration::from_secs(10);
        let (tx, rx) = std::sync::mpsc::channel();
        let start = Arc::new(std::sync::Barrier::new(8));
        for _ in 0..8 {
            let (caches, url, tx, start) = (
                Arc::clone(&caches),
                hub.url.clone(),
                tx.clone(),
                Arc::clone(&start),
            );
            std::thread::spawn(move || {
                start.wait();
                let began = std::time::Instant::now();
                let read = read_fleet_cached(&caches, FleetScope::Work, now, || {
                    Some(darkmux_flow::RawRedisUrl::new(url))
                });
                let _ = tx.send((began.elapsed(), read.state, read.records.len()));
            });
        }
        let deadline = std::time::Instant::now() + 4 * one_bound();
        let mut results = Vec::new();
        while results.len() < 8 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(left) {
                Ok(r) => results.push(r),
                Err(_) => panic!(
                    "{} of 8 reads returned within {:?}: {results:?}",
                    results.len(),
                    4 * one_bound()
                ),
            }
        }
        let mut took: Vec<std::time::Duration> = results.iter().map(|r| r.0).collect();
        took.sort();
        eprintln!("8 concurrent reads against a silent hub: {took:?}");
        let margin = std::time::Duration::from_millis(300);
        // The one that asked: the connect, then its two pipelined replies'
        // bounds. The seven that waited: at most one round trip's bound.
        assert!(
            took[7]
                < darkmux_flow::REDIS_CONNECT_TIMEOUT
                    + 2 * darkmux_flow::REDIS_RESPONSE_TIMEOUT
                    + margin,
            "the read that asked the hub took {:?}",
            took[7]
        );
        assert!(
            took[6] < refresh_wait() + margin,
            "a read waited {:?} on another's refresh: {took:?}",
            took[6]
        );
        for (_, state, len) in &results {
            assert!(
                matches!(state, SourceState::Stale { age_ms: 10_000, .. }),
                "{state:?}"
            );
            assert_eq!(*len, 1, "the last window is served");
        }
        assert_eq!(
            hub.accepts.load(Ordering::SeqCst),
            1,
            "one attempt reached the hub, not one per request"
        );

        // Inside the TTL of that failure, nothing asks the hub again.
        let url = hub.url.clone();
        let again = within(one_bound(), move || {
            let began = std::time::Instant::now();
            let read = read_fleet_cached(
                &caches,
                FleetScope::Work,
                now + std::time::Duration::from_millis(500),
                || Some(darkmux_flow::RawRedisUrl::new(url)),
            );
            (began.elapsed(), read.state)
        })
        .expect("a read inside the failure's TTL returns at once");
        assert!(
            again.0 < std::time::Duration::from_millis(100),
            "{:?}",
            again.0
        );
        assert!(
            matches!(again.1, SourceState::Stale { .. }),
            "{:?}",
            again.1
        );
        assert_eq!(
            hub.accepts.load(Ordering::SeqCst),
            1,
            "a read inside the failure's TTL asked the hub again"
        );
    }

    #[test]
    fn a_continuing_stream_appends_and_drops_what_the_hub_trimmed() {
        let window = vec![
            rec("1-0", "dispatch.tool"),
            rec("2-0", "dispatch.tool"),
            rec("3-0", "dispatch.tool"),
        ];
        let delta = Delta::Continues {
            newer: vec![rec("4-0", "dispatch.tool")],
            oldest: (2, 0),
            newest: Some((4, 0)),
        };
        assert_eq!(
            ids(&advance(&window, delta, 10)),
            vec![Some((2, 0)), Some((3, 0)), Some((4, 0))]
        );
    }

    #[test]
    fn the_window_keeps_only_the_newest_cap_entries() {
        let window = vec![rec("1-0", "dispatch.tool"), rec("2-0", "dispatch.tool")];
        let delta = Delta::Continues {
            newer: vec![rec("3-0", "dispatch.tool"), rec("4-0", "dispatch.tool")],
            oldest: (1, 0),
            newest: Some((4, 0)),
        };
        assert_eq!(
            ids(&advance(&window, delta, 3)),
            vec![Some((2, 0)), Some((3, 0)), Some((4, 0))]
        );
    }

    #[test]
    fn a_replacing_read_discards_the_old_window() {
        let window = vec![rec("5-0", "dispatch.tool"), rec("6-0", "dispatch.tool")];
        let delta = Delta::Replaces {
            records: vec![rec("1-1", "dispatch.tool")],
            newest: Some((1, 1)),
        };
        assert_eq!(ids(&advance(&window, delta, 10)), vec![Some((1, 1))]);
    }

    #[test]
    fn an_unread_payload_is_dropped_and_a_read_one_kept() {
        let tool = rec("1-0", "dispatch.tool");
        assert!(tool.value().get("payload").is_none(), "{:?}", tool.value());
        assert_eq!(
            tool.full()["payload"]["result"],
            json!("x".repeat(64)),
            "the whole record is still served"
        );
        assert_eq!(tool.full()["hub_id"], json!("1-0"));
        for action in [
            "mission.start",
            "run.start",
            "run.complete",
            "run.error",
            "dispatch.start",
            "dispatch.complete",
            "dispatch.error",
            "step.complete",
            "step.error",
            "budget.stop",
            "budget.wait",
            "telemetry.tokens",
        ] {
            assert!(
                rec("1-0", action).value().get("payload").is_some(),
                "{action} keeps its payload"
            );
        }
    }

    #[test]
    fn a_compact_record_keeps_the_identity_of_the_whole_one() {
        let whole = json!({ "ts": "2026-10-12T00:00:00Z", "action": "dispatch.tool", "session_id": "s", "payload": { "result": "r" } });
        let compact = FleetRecord::from_entry("1-0", &whole.to_string()).unwrap();
        assert!(compact.value().get("payload").is_none());
        assert_eq!(compact.identity(), RecordIdentity::of(&whole));
        let other = json!({ "ts": "2026-10-12T00:00:00Z", "action": "dispatch.tool", "session_id": "s", "payload": { "result": "other" } });
        assert_ne!(
            compact.identity(),
            RecordIdentity::of(&other),
            "the payload is part of the identity"
        );
    }

    /// The realistic fixture as the hub would hand it over: stream entries
    /// with ids, read into the window's compact form.
    fn compact_fixture(n: usize) -> (Vec<Value>, FleetRecords) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut whole = crate::fleet_fixture::realistic_records(n, now);
        let compact = whole
            .iter_mut()
            .enumerate()
            .map(|(i, r)| {
                let id = format!("{}-{i}", 1_791_000_000_000u64 + i as u64);
                let entry = Arc::new(FleetRecord::from_entry(&id, &r.to_string()).unwrap());
                crate::stamp_hub_id(r, &id);
                entry
            })
            .collect();
        (whole, FleetRecords(Arc::new(compact)))
    }

    /// (d) without a hub, so it runs wherever the suite runs: every reader
    /// of the window answers the same from the compact form as from the
    /// whole records, on the realistic fixture.
    #[test]
    #[serial_test::serial]
    fn every_reader_answers_the_same_from_the_compact_form() {
        let _isolated = darkmux_types::test_isolation::IsolatedState::new();
        let (whole, compact) = compact_fixture(1_500);
        // This machine's own records land in both sinks: the day files hold
        // a third of the fleet's records too (without a hub id), so every
        // reader's de-duplication against the local walk is exercised.
        let flows = tempfile::tempdir().unwrap();
        let mut days: std::collections::BTreeMap<String, String> = Default::default();
        for r in whole.iter().step_by(3) {
            let mut local = r.clone();
            local.as_object_mut().unwrap().shift_remove("hub_id");
            let day = local["ts"].as_str().unwrap()[..10].to_string();
            days.entry(day).or_default().push_str(&format!("{local}\n"));
        }
        for (day, lines) in days {
            std::fs::write(flows.path().join(format!("{day}.jsonl")), lines).unwrap();
        }
        let sorted = |runs: &[crate::runs::Run]| {
            let mut rows: Vec<Value> = runs
                .iter()
                .map(|r| serde_json::to_value(r).unwrap())
                .collect();
            rows.sort_by_key(|r| r["id"].to_string());
            rows
        };
        let a = crate::runs::build_runs_with_usage(flows.path(), None, &compact, None);
        let b = crate::runs::build_runs_with_usage(flows.path(), None, &whole, None);
        assert!(
            a.runs.len() >= 12,
            "the fixture builds real rows: {}",
            a.runs.len()
        );
        assert_eq!(sorted(&a.runs), sorted(&b.runs));
        assert_eq!(
            serde_json::to_value(&a.usage).unwrap(),
            serde_json::to_value(&b.usage).unwrap()
        );
        assert_eq!(
            serde_json::to_value(crate::scan_flow_missions(flows.path(), &compact)).unwrap(),
            serde_json::to_value(crate::scan_flow_missions(flows.path(), &whole)).unwrap()
        );
        let known = HashSet::new();
        assert_eq!(
            sorted(&crate::runs::peer_mission_runs(
                flows.path(),
                &compact,
                &known
            )),
            sorted(&crate::runs::peer_mission_runs(
                flows.path(),
                &whole,
                &known
            ))
        );
        let missions = darkmux_crew::loader::load_missions().unwrap_or_default();
        assert_eq!(
            format!(
                "{:?}",
                crate::runs::local_dispatch_status(&missions, flows.path(), &compact)
            ),
            format!(
                "{:?}",
                crate::runs::local_dispatch_status(&missions, flows.path(), &whole)
            )
        );
        for mission in [
            "mis-0002-review-the-fleet-window-change",
            "mis-0029-review-the-fleet-window-change",
        ] {
            assert_eq!(
                crate::runs::mission_owner_machine(flows.path(), &compact, mission),
                crate::runs::mission_owner_machine(flows.path(), &whole, mission)
            );
            // The catalog serves whole records, byte for byte.
            assert_eq!(
                crate::collect_records_by_field(flows.path(), &compact, "mission_id", mission),
                crate::collect_records_by_field(flows.path(), &whole, "mission_id", mission)
            );
        }
        let full: Vec<Value> = compact.iter().map(FleetRecord::full).collect();
        assert_eq!(full, whole, "every record is still served whole");

        // A dispatch replay joins the machine's samples in its window, served whole.
        let session = "task-0001-1-implement-the-incremental-reader-and-tests";
        let (records, _, _) =
            crate::collect_records_by_field(flows.path(), &whole, "session_id", session);
        let start = records
            .iter()
            .find_map(|r| r["ts"].as_str())
            .unwrap()
            .to_string();
        let uid = records[0]["machine_uid"].as_str().unwrap().to_string();
        let samples: Vec<Value> = (0..5)
            .map(|i| json!({ "ts": start, "action": "machine.telemetry", "machine_uid": uid, "category": "telemetry", "payload": { "cpu_pct": i, "gpu_pct": 2 * i } }))
            .collect();
        let mut whole_with = whole.clone();
        let mut compact_with: Vec<Arc<FleetRecord>> = compact.0.iter().cloned().collect();
        for (i, sample) in samples.iter().enumerate() {
            let id = format!("1792000000000-{i}");
            compact_with.push(Arc::new(
                FleetRecord::from_entry(&id, &sample.to_string()).unwrap(),
            ));
            let mut stamped = sample.clone();
            crate::stamp_hub_id(&mut stamped, &id);
            whole_with.push(stamped);
        }
        let now_ms = crate::current_millis();
        let (mut a, mut b) = (records.clone(), records);
        crate::join_host_samples_into_session_records(
            flows.path(),
            FleetRecords(Arc::new(compact_with)),
            &mut a,
            now_ms,
        );
        crate::join_host_samples_into_session_records(flows.path(), &whole_with, &mut b, now_ms);
        assert!(
            a.iter().any(|r| r["payload"]["gpu_pct"] == json!(8)),
            "the samples joined, whole"
        );
        assert_eq!(a, b);
    }

    /// The snapshot file a remote `run-list` reads is the compact form: under
    /// half the whole records' JSON on the realistic fixture.
    #[test]
    fn the_snapshot_file_is_the_compact_form() {
        let (whole, compact) = compact_fixture(3_000);
        let whole_bytes = serde_json::to_vec(&whole).unwrap().len();
        let snapshot_bytes = serde_json::to_vec(&compact.to_snapshot()).unwrap().len();
        eprintln!("snapshot {snapshot_bytes} bytes, whole records {whole_bytes} bytes");
        assert!(
            snapshot_bytes * 2 < whole_bytes,
            "snapshot {snapshot_bytes} bytes against {whole_bytes}"
        );
    }

    #[test]
    fn a_snapshot_round_trips_the_compact_records_and_their_identities() {
        let records = FleetRecords(Arc::new(vec![
            rec("1-0", "dispatch.tool"),
            rec("2-0", "dispatch.start"),
        ]));
        let json = serde_json::to_string(&records.to_snapshot()).unwrap();
        let back = FleetRecords::from_snapshot(serde_json::from_str(&json).unwrap()).unwrap();
        let pairs = |r: &FleetRecords| {
            r.iter()
                .map(|r| (r.value().clone(), r.identity(), r.hub_id))
                .collect::<Vec<_>>()
        };
        assert_eq!(pairs(&back), pairs(&records));
        assert!(FleetRecords::from_snapshot(vec![SnapshotRecordIn {
            identity: "not hex".into(),
            record: json!({})
        }])
        .is_none());
    }
}
