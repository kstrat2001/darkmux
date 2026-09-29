//! (#2902 step 5) Budgets: the operator's own limits on what darkmux spends
//! at an endpoint, and on what one step spends, and what reaching one does.
//!
//! **Nothing here runs unless the operator sets a budget.** darkmux ships no
//! number: an endpoint with no `limits.window` budget, or with `policy:
//! "off"`, is returned from [`EndpointBudget::of`] as `None` before any file
//! is opened, so the gate costs a struct read. A `limits` that cannot be
//! used as written (unreadable, or a window period that does not parse) is
//! an ERROR, never "no budget": a typo must not silently disarm a budget
//! (preflight refuses it first, `darkmux_profiles::preflight`).
//!
//! **Once set, a breach never stops the run.** An endpoint budget's policy
//! is one of [`BudgetPolicy`]'s three values:
//!
//! - `off`: nothing is counted.
//! - `warn` (the default once a budget is set): the breach is printed on the
//!   CLI and written as a Warn-level `budget.warn` flow record, and the call
//!   goes ahead. An optional `warn_at` fraction warns once earlier.
//! - `wait`: the call waits until enough of the ROLLING window has expired
//!   to leave room, says how long (CLI line and a `budget.wait` record the
//!   run page and `darkmux mission status` read), then goes ahead and writes
//!   `budget.resume`. The wait is recorded in [`darkmux_types::run_pause`],
//!   which extends the run's wall-clock bound, the host-side twin of the
//!   thermal governor's pause.
//!
//! **Ending a wait.** Every half second the wait checks whether its run was
//! stopped: the process was interrupted (Ctrl-C, a signal), or its mission
//! is aborted or finalized, or its phase abandoned, on disk
//! ([`BudgetEnv::stop_reason`]). `darkmux mission abort` runs in another
//! process and only writes that terminal state, so the waiter reads it
//! rather than being signalled: a pid is not the operator's handle on a run
//! (a mission can outlive, and be aborted apart from, the process that
//! launched it), and the terminal state on disk is the one authority the
//! board and every other consumer already read. A stopped wait returns an
//! error and the call is never sent.
//!
//! **The per-step cap** (`remote.max_tokens_per_step`, [`admit_step`] /
//! [`settle_step`]) has only `off` and `warn` (operator, 2026-09-27): a step
//! has no rolling window, so there is nothing to wait for.
//!
//! **The window.** "The last `period` from now": a rolling window with no
//! calendar reset. Its spend is the sum of this machine's usage records
//! (`telemetry.tokens`, one per model call, `crate::usage`) that carry this
//! endpoint's id (`endpoint_id`), read with the same per-record reading the
//! token fold uses ([`crate::usage::usage_contribution`]). Both `purpose`s
//! count, work and utility: a budget is about what the ENDPOINT served, and
//! a provider bills a utility call like any other. Records written before
//! `endpoint_id` existed (flow < 1.65.0) carry no id and are not counted.
//! Another machine's spend at the same endpoint is not seen: the window
//! reads this machine's flow log.
//!
//! **"Room"** is `spent < budget`, for tokens and calls alike. The next
//! call's own cost is unknowable until it returns, so a call admitted with
//! room can overshoot by itself (a soft ceiling), and the next one then
//! waits.
//!
//! **Cost.** The window is read before every hosted call to a budgeted
//! endpoint, so it must not re-scan the flow history each time (the #2891
//! trap). [`Ledger`] opens only the day files the window can touch and
//! remembers, per file, the byte offset it has read to: a later call reads
//! only the bytes appended since, and keeps only the (endpoint id, second,
//! tokens) triples of records that carry an id. The FIRST read in a process
//! pays for every day file the window spans (measured on release builds:
//! about 5 ms for a 1d window, 45 ms for 7d, 180 ms for 30d on a busy
//! machine); every later read about 0.1 to 0.7 ms.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use darkmux_types::execution_id::ExecutionId;
use darkmux_types::session_id::SessionId;
use darkmux_types::{BudgetPolicy, ModelEndpoint, WindowBudget};

/// The flow-record telemetry `source` every budget record carries. The
/// actions are `FlowAction::Budget*`: `budget.warn` (level Warn),
/// `budget.wait` (Warn, carries when it will resume), `budget.resume` (Info,
/// carries how long it waited) and `budget.stop` (Warn).
pub const BUDGET_SOURCE: &str = "budget";

/// The longest single sleep while waiting: the wait re-reads the window
/// (and the endpoint's limits, so a raised or switched-off budget releases
/// it) at least this often, and polls the interrupt flag each slice.
pub const WAIT_POLL_MAX: Duration = Duration::from_secs(30);
/// How often an interrupt is noticed while a wait sleeps.
const WAIT_SLICE: Duration = Duration::from_millis(500);

/// Who a gated call is for, so a budget record lands on the right run, and
/// so a wait can tell that its run was stopped. The session is required: it
/// names the run, so two launches of one config never share a budget
/// record, and a wait reads only its own run's stop. The execution is
/// required too: every budget record is about the one execution whose call
/// was gated. The rest is optional (a `dispatch.map` step runs no role).
#[derive(Clone, Copy, Debug)]
pub struct BudgetCaller<'a> {
    pub session: &'a SessionId,
    pub execution: &'a ExecutionId,
    pub role_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub phase_id: Option<&'a str>,
    /// The profile registry the command resolved its endpoint from (its
    /// `--profiles-file`), re-read while a call waits. `None`: the default
    /// search.
    pub profiles_file: Option<&'a str>,
}

/// An endpoint budget ready to enforce.
#[derive(Clone, Debug, PartialEq)]
pub struct EndpointBudget {
    pub endpoint_id: String,
    pub policy: BudgetPolicy,
    pub warn_at: Option<f64>,
    pub window: WindowBudget,
    /// The period as written (`"1d"`), for messages.
    pub period: String,
}

impl EndpointBudget {
    /// The budget `ep` carries, or `None` when there is nothing to enforce:
    /// no `limits`, no window budget, policy `off`, no `endpoints` id (an
    /// inline endpoint: its usage records carry no id to sum by, which
    /// `darkmux doctor` names), or a MANAGED endpoint (darkmux budgets the
    /// calls it SENDS to an endpoint it does not manage).
    ///
    /// `Err` (review M2), never `Ok(None)`, when the limits cannot be used as
    /// written: unreadable (one mistyped field makes the whole value
    /// unreadable), an unregistered `policy`, a set window whose `period`
    /// does not parse, or `warn_at` outside (0, 1). Preflight refuses all of
    /// them first; this keeps a caller that skipped preflight from running a
    /// typo'd budget as no budget.
    pub fn of(ep: &ModelEndpoint) -> Result<Option<Self>, String> {
        let at = match ep.named_id() {
            Some(id) => format!("endpoint `{id}`"),
            None => "an inline endpoint".to_string(),
        };
        let limits = match ep.limits.as_ref() {
            None => return Ok(None),
            Some(darkmux_types::Lenient::Known(l)) => l,
            Some(darkmux_types::Lenient::Unrecognized(raw)) => {
                let why = serde_json::from_value::<darkmux_types::UsageLimits>(raw.clone())
                    .err()
                    .map(|e| format!(" ({e})"))
                    .unwrap_or_default();
                return Err(format!(
                    "darkmux: {at}'s `limits` could not be read{why}; a budget that cannot be read is refused, \
                     never run as no budget. valid: {} (#2902)",
                    darkmux_types::config_enum::LIMITS_SHAPE
                ));
            }
        };
        limits.validate().map_err(|e| format!("darkmux: {at}: {e} (#2902)"))?;
        let policy = limits.resolved_policy().map_err(|raw| {
            format!(
                "darkmux: {at}'s budget policy `{raw}` is not one of {} (#2902)",
                <BudgetPolicy as darkmux_types::config_enum::ConfigEnum>::TOKENS.join(", ")
            )
        })?;
        let Some(id) = ep.named_id() else { return Ok(None) };
        if !matches!(ep.kind(), Ok(darkmux_types::EndpointKind::Unmanaged)) {
            return Ok(None);
        }
        if !policy.counts() {
            return Ok(None);
        }
        let Some(window) = limits.window_budget() else { return Ok(None) };
        Ok(Some(EndpointBudget {
            endpoint_id: id.to_string(),
            policy,
            warn_at: limits.warn_at,
            window,
            period: limits.window.as_ref().and_then(|w| w.period.clone()).unwrap_or_default(),
        }))
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(t) = self.window.tokens {
            parts.push(format!("{t} tokens"));
        }
        if let Some(c) = self.window.calls {
            parts.push(format!("{c} calls"));
        }
        format!("{} per {}", parts.join(", "), self.period)
    }
}

/// What a breach was measured in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    Tokens,
    Calls,
}

/// How far into a budget the KNOWN spend is. Ordered: a higher level is
/// news. Whether the spend is fully known is a separate fact
/// ([`Breach::unmetered`]), never a level, so neither masks the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BreachLevel {
    /// At or past `warn_at` of the budget, still under it.
    Early,
    /// At or past the budget.
    AtLimit,
}

/// One measured breach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Breach {
    /// `None`: the known spend is under every threshold, and the breach is
    /// only that the window is not fully metered.
    pub level: Option<BreachLevel>,
    pub metric: Metric,
    /// The known spend: every call's full total where it is known, and the
    /// halves it did report where it is not. A lower bound when
    /// `unmetered > 0`.
    pub spent: u64,
    pub limit: u64,
    /// Calls in the window whose full spend is unknown (no prompt count, or
    /// no usage at all).
    pub unmetered: u64,
}

/// What a warning for one budget already said, so it is said again only
/// when it says more: a higher level, or a window newly not fully metered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Surfaced {
    pub level: Option<BreachLevel>,
    pub unmetered: bool,
}

impl Surfaced {
    pub fn of(br: &Breach) -> Self {
        Surfaced { level: br.level, unmetered: br.unmetered > 0 }
    }

    /// Whether this says something `before` did not.
    pub fn is_news_after(&self, before: Option<Surfaced>) -> bool {
        let before = before.unwrap_or_default();
        self.level > before.level || (self.unmetered && !before.unmetered)
    }
}

/// One call's spend as the window counts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spend {
    /// The call's full total when known, else the halves it reported: never
    /// more than it spent.
    pub known: u64,
    /// True when `known` is the call's full spend
    /// ([`darkmux_trajectory::UsageCounts::total_tokens`] was known).
    pub metered: bool,
}

impl Spend {
    pub fn full(tokens: u64) -> Self {
        Spend { known: tokens, metered: true }
    }

    /// A call whose full spend is unknown; `known` is what it did report.
    pub fn partial(known: u64) -> Self {
        Spend { known, metered: false }
    }
}

/// What the pure evaluation says about the next call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Under every threshold.
    Proceed,
    /// Go ahead, and surface the breach.
    Warn(Breach),
    /// `wait`: hold the call. `resume_at` is the epoch second the window
    /// next has room, `None` when it never will on its own (a budget of 0).
    Wait { breach: Breach, resume_at: Option<i64> },
}

/// Spend inside the window: `(epoch second, spend)` per usage record,
/// oldest first.
pub type WindowEntries = Vec<(i64, Spend)>;

/// THE decision, pure: `entries` are this endpoint's records, `now` the
/// injected clock. A record at second `t` is inside the window while
/// `now < t + period`, so it leaves the window at exactly `t + period`.
///
/// An unknown spend is never read as small. Its known halves count toward
/// the budget like any spend, so a window they fill still warns or waits;
/// and under a token budget, a window holding one is surfaced as not fully
/// metered (`unmetered`), on the same warning as any level it reaches. A
/// calls budget is never unmetered: a call count is exact.
///
/// The limit, stated plainly: a call whose provider reported NO usage at
/// all adds 0 to the known spend. Under a token `wait` budget such calls
/// warn "not fully metered" but never, on their own, make the window
/// wait. The conservative charge ([`conservative_hosted_spend`]) feeds
/// only a step's own per-step bucket, never this ledger.
pub fn evaluate(b: &EndpointBudget, entries: &[(i64, Spend)], now: i64) -> Verdict {
    if !b.policy.counts() {
        return Verdict::Proceed;
    }
    let period = b.window.period_secs as i64;
    let inside: Vec<(i64, Spend)> = entries.iter().copied().filter(|(t, _)| now < t + period && *t <= now).collect();
    let tokens: u64 = inside.iter().map(|(_, s)| s.known).fold(0u64, u64::saturating_add);
    let unmetered = inside.iter().filter(|(_, s)| !s.metered).count() as u64;
    let calls = inside.len() as u64;
    let measured = [(Metric::Tokens, tokens, b.window.tokens), (Metric::Calls, calls, b.window.calls)];

    let mut at_limit: Vec<Breach> = Vec::new();
    let mut early: Option<Breach> = None;
    for (metric, spent, limit) in measured {
        let Some(limit) = limit else { continue };
        // A call count is always exact: only a token metric can be unmetered.
        let unmetered = match metric {
            Metric::Tokens => unmetered,
            Metric::Calls => 0,
        };
        if spent >= limit {
            at_limit.push(Breach { level: Some(BreachLevel::AtLimit), metric, spent, limit, unmetered });
        } else if let Some(f) = b.warn_at {
            let threshold = ((limit as f64) * f).ceil() as u64;
            if spent >= threshold && early.is_none() {
                early = Some(Breach { level: Some(BreachLevel::Early), metric, spent, limit, unmetered });
            }
        }
    }
    if let Some(first) = at_limit.first().copied() {
        if b.policy == BudgetPolicy::Wait {
            // Room returns when EVERY breached metric is back under its
            // limit: the latest of their individual resume times.
            let mut resume: Option<i64> = Some(i64::MIN);
            for br in &at_limit {
                let r = resume_at_for(&inside, br, period);
                resume = match (resume, r) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    _ => None,
                };
            }
            return Verdict::Wait { breach: first, resume_at: resume };
        }
        return Verdict::Warn(first);
    }
    match (early, unmetered > 0, b.window.tokens) {
        (Some(br), _, _) => Verdict::Warn(br),
        (None, true, Some(limit)) => {
            Verdict::Warn(Breach { level: None, metric: Metric::Tokens, spent: tokens, limit, unmetered })
        }
        _ => Verdict::Proceed,
    }
}

/// When the oldest records have left the window far enough for `br`'s
/// metric to be under its limit again. `None` for a limit of 0 (there is
/// never room).
fn resume_at_for(inside: &[(i64, Spend)], br: &Breach, period: i64) -> Option<i64> {
    if br.limit == 0 {
        return None;
    }
    let mut remaining = br.spent;
    for (t, n) in inside {
        remaining = remaining.saturating_sub(match br.metric {
            Metric::Tokens => n.known,
            Metric::Calls => 1,
        });
        if remaining < br.limit {
            return Some(t + period);
        }
    }
    None
}

/// (#1442 gate C4) What one hosted call SPENDS from a step's bucket: its
/// total ([`darkmux_trajectory::UsageCounts::total_tokens`], the amount its
/// usage record carries) when the spend is known. When it is not (the reply
/// reported no usage, or a split without its prompt half), the charge is
/// the `max_tokens` the call was granted, which bounds its completion, PLUS
/// the prompt it sent, estimated from `request` (the body actually posted)
/// by the project's one estimate ([`darkmux_trajectory::estimate_tokens`]).
/// Charging 0, the completion alone, or the cap alone would let such an
/// endpoint run off the meter; over-counting is the safe direction.
pub fn conservative_hosted_spend(total_tokens: Option<u64>, granted_max_tokens: u32, request: &serde_json::Value) -> u64 {
    total_tokens.unwrap_or_else(|| {
        let prompt = darkmux_trajectory::estimate_tokens(&request.to_string()) as u64;
        u64::from(granted_max_tokens).saturating_add(prompt)
    })
}

// ── The window ledger ───────────────────────────────────────────────────

#[derive(Default)]
struct DayFile {
    /// Bytes read so far (always at a line boundary).
    offset: u64,
    /// (review C7) What the bytes already read looked like: the file's
    /// inode, its first bytes, and the bytes just before `offset`. An append
    /// changes none of them; a file replaced (new inode) or rewritten in
    /// place (different head or tail bytes) resets the file and it is read
    /// again from the start. Not mtime: every append changes that.
    inode: u64,
    head: Vec<u8>,
    tail: Vec<u8>,
    /// `(endpoint id, epoch second, spend)` for every usage record in the
    /// bytes read that carries an `endpoint_id`.
    entries: Vec<(String, i64, Spend)>,
}

/// How many bytes of a day file's start, and of the bytes just before the
/// read offset, identify what was already read.
const FINGERPRINT_BYTES: u64 = 64;

/// An incremental reader of the flow day files (`<flows_dir>/YYYY-MM-DD.jsonl`,
/// one per UTC day) for the usage records a window budget sums. See the
/// module doc's "Cost" paragraph.
pub struct Ledger {
    dir: PathBuf,
    days: BTreeMap<String, DayFile>,
    /// Bytes read from disk over this ledger's life (the cost check's
    /// instrument).
    bytes_read: u64,
}

impl Ledger {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Ledger { dir: dir.into(), days: BTreeMap::new(), bytes_read: 0 }
    }

    /// Bytes read from disk so far.
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Every usage record for `endpoint_id` stamped in `(now - period,
    /// now]`, oldest first. Reads only the day files that range touches, and
    /// only the bytes of each appended since the last call.
    pub fn window(&mut self, endpoint_id: &str, now: i64, period_secs: u64) -> WindowEntries {
        let from = now - period_secs as i64;
        let first_day = darkmux_flow::day_utc_at(from);
        let last_day = darkmux_flow::day_utc_at(now);
        // Every day stem from `first_day` to `last_day` inclusive.
        let mut day = from.div_euclid(86_400) * 86_400;
        while darkmux_flow::day_utc_at(day) <= last_day {
            let stem = darkmux_flow::day_utc_at(day);
            self.refresh(&stem);
            day += 86_400;
        }
        // Drop day files no window has needed for a while: anything older
        // than the widest window asked of this ledger is re-read if a wider
        // one is ever asked.
        let keep_from = first_day.clone();
        let stale: Vec<String> = self.days.keys().filter(|d| **d < keep_from).cloned().collect();
        if self.days.len() > 64 {
            for d in stale {
                self.days.remove(&d);
            }
        }
        let mut out: WindowEntries = self
            .days
            .range(first_day..=last_day)
            .flat_map(|(_, f)| f.entries.iter())
            .filter(|(id, t, _)| id == endpoint_id && *t > from && *t <= now)
            .map(|(_, t, n)| (*t, *n))
            .collect();
        out.sort_by_key(|(t, _)| *t);
        out
    }

    fn refresh(&mut self, stem: &str) {
        let path = self.dir.join(format!("{stem}.jsonl"));
        let Ok(mut file) = std::fs::File::open(&path) else { return };
        let Ok(meta) = file.metadata() else { return };
        let len = meta.len();
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&meta);
        #[cfg(not(unix))]
        let inode = 0u64;
        let entry = self.days.entry(stem.to_string()).or_default();
        if entry.offset > 0 {
            let same = entry.inode == inode
                && len >= entry.offset
                && read_at(&mut file, 0, entry.head.len() as u64).as_deref() == Some(entry.head.as_slice())
                && read_at(&mut file, entry.offset - entry.tail.len() as u64, entry.tail.len() as u64).as_deref()
                    == Some(entry.tail.as_slice());
            if !same {
                // Replaced, truncated or rewritten: start the file over.
                *entry = DayFile::default();
            }
        }
        entry.inode = inode;
        if len == entry.offset {
            return;
        }
        let Some(buf) = read_at(&mut file, entry.offset, len - entry.offset) else { return };
        self.bytes_read += buf.len() as u64;
        // Only COMPLETE lines: a record still being appended is read next
        // time, from the start of its line.
        let Some(last_nl) = buf.iter().rposition(|b| *b == b'\n') else { return };
        for line in buf[..=last_nl].split(|b| *b == b'\n') {
            if let Some(e) = parse_entry(line) {
                entry.entries.push(e);
            }
        }
        entry.offset += (last_nl + 1) as u64;
        let head_len = entry.offset.min(FINGERPRINT_BYTES);
        entry.head = read_at(&mut file, 0, head_len).unwrap_or_default();
        let tail_len = entry.offset.min(FINGERPRINT_BYTES);
        entry.tail = read_at(&mut file, entry.offset - tail_len, tail_len).unwrap_or_default();
    }
}

/// `len` bytes of `file` from `at`, or `None` when they cannot be read.
fn read_at(file: &mut std::fs::File, at: u64, len: u64) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(at)).ok()?;
    let mut buf = Vec::with_capacity(len as usize);
    std::io::Read::by_ref(file).take(len).read_to_end(&mut buf).ok()?;
    (buf.len() as u64 == len).then_some(buf)
}

/// One line to a ledger entry: a usage record that carries an
/// `endpoint_id` and a parseable `ts`. A cheap substring test first, so the
/// JSON of every OTHER record (the vast majority) is never parsed.
fn parse_entry(line: &[u8]) -> Option<(String, i64, Spend)> {
    const NEEDLE: &[u8] = b"\"endpoint_id\"";
    if line.len() < NEEDLE.len() || !line.windows(NEEDLE.len()).any(|w| w == NEEDLE) {
        return None;
    }
    let v = darkmux_flow::reader::parse_value(std::str::from_utf8(line).ok()?)?;
    let amount = crate::usage::usage_contribution(&v)?;
    let id = crate::usage::payload_of(&v).get("endpoint_id")?.as_str()?.to_string();
    let ts = crate::records_emitted::parse_ts_secs(v.get("ts")?.as_str()?)?;
    // `total` is the record's full total when known, else the halves it
    // reported; `spend` says which.
    Some((id, ts, Spend { known: amount.total, metered: amount.spend.is_some() }))
}

/// The process's ledger over the configured flows dir.
fn live_ledger() -> &'static Mutex<Ledger> {
    static LEDGER: std::sync::OnceLock<Mutex<Ledger>> = std::sync::OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(Ledger::new(darkmux_types::config_access::flows_dir())))
}

// ── The gate ────────────────────────────────────────────────────────────

/// Everything the gate touches outside itself, so the decision and the
/// wait are tested with a frozen clock and no real sleep.
pub trait BudgetEnv {
    /// Epoch seconds now.
    fn now(&self) -> i64;
    /// This endpoint's spend in the window ending now.
    fn window(&self, b: &EndpointBudget, now: i64) -> WindowEntries;
    /// Sleep `d` (a test advances its clock instead).
    fn sleep(&self, d: Duration);
    /// Why the caller's run was stopped, when it was: the process was
    /// interrupted, or its mission is aborted or finalized, or its phase
    /// abandoned (read from disk: `darkmux mission abort` runs in another
    /// process). `None`: keep waiting.
    fn stop_reason(&self, caller: &BudgetCaller<'_>) -> Option<String>;
    /// The endpoint's budget as the registry (`profiles_file`, else the
    /// default search) says NOW, re-read while a call waits, so raising or
    /// switching off a budget, or removing the endpoint, releases the wait
    /// (`Some(None)`). `None`: keep the one in hand (the registry could not
    /// be read).
    fn reload(&self, endpoint_id: &str, profiles_file: Option<&str>) -> Option<Option<EndpointBudget>>;
    /// Write a flow record.
    fn emit(&self, rec: darkmux_flow::FlowRecord);
    /// Print an operator line.
    fn say(&self, line: &str);
    /// Record deliberate pause time ([`darkmux_types::run_pause`]).
    fn paused(&self, ms: u64);
    /// What was already surfaced for `key` in this process.
    fn last_surfaced(&self, key: &str) -> Option<Surfaced>;
    fn set_surfaced(&self, key: &str, surfaced: Option<Surfaced>);
}

/// The production environment: the system clock, the configured flows dir,
/// the live profile registry, the mission store, the process-wide flow sink.
pub struct LiveEnv;

fn warned_levels() -> &'static Mutex<HashMap<String, Surfaced>> {
    static LEVELS: std::sync::OnceLock<Mutex<HashMap<String, Surfaced>>> = std::sync::OnceLock::new();
    LEVELS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// An endpoint's budget as a freshly loaded registry says it: `Ok(None)`
/// when the endpoint was removed, switched `off` or left without a window
/// (review C-i: the operator took the budget away, which releases a wait);
/// `Err` when its limits cannot be used as written (a 0 edited in, a typo).
pub(crate) fn reloaded_budget(
    reg: &darkmux_types::ProfileRegistry,
    endpoint_id: &str,
) -> Result<Option<EndpointBudget>, String> {
    let Some(ep) = reg.endpoints.get(endpoint_id) else { return Ok(None) };
    let mut named = ep.clone();
    named.source = darkmux_types::EndpointSource::Named(endpoint_id.to_string());
    EndpointBudget::of(&named)
}

/// The line a refused mid-wait edit prints.
pub(crate) fn refused_edit_line(endpoint_id: &str, why: &str) -> String {
    format!(
        "darkmux: ⚠ endpoint `{endpoint_id}`'s edited limits were refused ({why}); the wait keeps the budget it had"
    )
}

/// True the first time `line` is seen in this process: a refused edit is
/// re-read every poll of a wait, and said once.
pub(crate) fn first_refusal(line: &str) -> bool {
    static SEEN: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(line.to_string())
}

/// The lowercase `status` of the JSON document at `path`, when it has one.
fn status_at(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(v.get("status")?.as_str()?.to_ascii_lowercase())
}

/// (review M1) Why a run was stopped, read from what `darkmux mission
/// abort` / `finalize` write: the mission's own status, and its phase's.
pub fn run_stop_reason(mission_id: Option<&str>, phase_id: Option<&str>) -> Option<String> {
    let mid = mission_id?;
    if let Some(status) = status_at(&crate::lifecycle::mission_path(mid)) {
        if matches!(status.as_str(), "aborted" | "finalized") {
            return Some(format!("mission `{mid}` is {status}"));
        }
    }
    let pid = phase_id?;
    match status_at(&crate::lifecycle::phase_path(mid, pid)).as_deref() {
        Some("abandoned") => Some(format!("phase `{pid}` of mission `{mid}` is abandoned")),
        _ => None,
    }
}

impl BudgetEnv for LiveEnv {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
    fn window(&self, b: &EndpointBudget, now: i64) -> WindowEntries {
        let mut ledger = live_ledger().lock().unwrap_or_else(|p| p.into_inner());
        ledger.window(&b.endpoint_id, now, b.window.period_secs)
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d)
    }
    fn stop_reason(&self, caller: &BudgetCaller<'_>) -> Option<String> {
        if darkmux_types::interrupt::is_set() {
            return Some("the run was interrupted".to_string());
        }
        run_stop_reason(caller.session.mission_id(), caller.phase_id)
    }
    fn reload(&self, endpoint_id: &str, profiles_file: Option<&str>) -> Option<Option<EndpointBudget>> {
        // Quiet: a quarantine warning every 30 s of a wait is noise; the
        // command's own load already printed it once.
        let loaded = darkmux_profiles::profiles::load_registry_quiet(profiles_file).ok()?;
        match reloaded_budget(&loaded.registry, endpoint_id) {
            Ok(b) => Some(b),
            // (5th review C5) An edit the budget cannot take keeps the one in
            // hand, said once, never dropped without a word.
            Err(e) => {
                let line = refused_edit_line(endpoint_id, &e);
                if first_refusal(&line) {
                    eprintln!("{line}");
                }
                None
            }
        }
    }
    fn emit(&self, rec: darkmux_flow::FlowRecord) {
        let _ = darkmux_flow::record(rec);
    }
    fn say(&self, line: &str) {
        eprintln!("{line}");
    }
    fn paused(&self, ms: u64) {
        darkmux_types::run_pause::add(ms);
    }
    fn last_surfaced(&self, key: &str) -> Option<Surfaced> {
        warned_levels().lock().unwrap_or_else(|p| p.into_inner()).get(key).copied()
    }
    fn set_surfaced(&self, key: &str, level: Option<Surfaced>) {
        let mut m = warned_levels().lock().unwrap_or_else(|p| p.into_inner());
        match level {
            Some(l) => {
                m.insert(key.to_string(), l);
            }
            None => {
                m.remove(key);
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    /// (review C1) A test's stand-in environment for [`admit_endpoint`], so
    /// a behavioral test drives a real production path (the step kinds,
    /// `dispatch_remote`) and sees the gate fire. Thread-local: the paths
    /// under test gate on the thread that runs them.
    static TEST_ENV: std::cell::RefCell<Option<std::rc::Rc<dyn BudgetEnv>>> = const { std::cell::RefCell::new(None) };
}

/// Run `f` with `env` standing in for [`LiveEnv`] in [`admit_endpoint`] on
/// this thread.
#[cfg(test)]
pub(crate) fn with_test_env<T>(env: std::rc::Rc<dyn BudgetEnv>, f: impl FnOnce() -> T) -> T {
    TEST_ENV.with(|e| *e.borrow_mut() = Some(env));
    let out = f();
    TEST_ENV.with(|e| *e.borrow_mut() = None);
    out
}

/// Run `f` with the environment the budget code uses on this thread: the
/// live one, or a test's stand-in ([`with_test_env`]). The sampler's pacer
/// tick goes through this, so a test drives it on the real sampler.
pub fn with_env<T>(f: impl FnOnce(&dyn BudgetEnv) -> T) -> T {
    #[cfg(test)]
    if let Some(env) = TEST_ENV.with(|e| e.borrow().clone()) {
        return f(&*env);
    }
    f(&LiveEnv)
}

/// THE gate every hosted call to an endpoint passes through, before the
/// call. `Ok` means go ahead (after warning, or after waiting); `Err` for
/// limits that cannot be used as written, or a run stopped during a wait
/// (nothing was sent). An endpoint with no budget returns at once, without
/// reading anything.
pub fn admit_endpoint(ep: &ModelEndpoint, caller: &BudgetCaller<'_>) -> anyhow::Result<()> {
    let Some(budget) = EndpointBudget::of(ep).map_err(|e| anyhow::anyhow!(e))? else { return Ok(()) };
    with_env(|env| admit_with(budget, caller, env))
}

/// [`admit_endpoint`] against an explicit environment.
pub fn admit_with(mut b: EndpointBudget, caller: &BudgetCaller<'_>, env: &dyn BudgetEnv) -> anyhow::Result<()> {
    let key = format!("endpoint:{}", b.endpoint_id);
    let mut waited_ms: u64 = 0;
    let mut announced: Option<Option<i64>> = None;
    loop {
        let now = env.now();
        let entries = env.window(&b, now);
        match evaluate(&b, &entries, now) {
            Verdict::Proceed | Verdict::Warn(_) if waited_ms > 0 && env.stop_reason(caller).is_some() => {
                // (review C-b) Room returned, but the run was stopped while
                // it waited (an abort in the wait's last slice): never send.
                let why = env.stop_reason(caller).unwrap_or_default();
                return Err(stopped_wait(&b, why, waited_ms, announced.is_some(), caller, env));
            }
            Verdict::Proceed => {
                env.set_surfaced(&key, None);
                resume_if_waited(&b, caller, env, waited_ms);
                return Ok(());
            }
            Verdict::Warn(br) => {
                if Surfaced::of(&br).is_news_after(env.last_surfaced(&key)) {
                    warn(&b, &br, caller, env);
                }
                env.set_surfaced(&key, Some(Surfaced::of(&br)));
                resume_if_waited(&b, caller, env, waited_ms);
                return Ok(());
            }
            Verdict::Wait { breach, resume_at } => {
                if let Some(why) = env.stop_reason(caller) {
                    return Err(stopped_wait(&b, why, waited_ms, announced.is_some(), caller, env));
                }
                if announced != Some(resume_at) {
                    announce_wait(&b, &breach, resume_at, now, caller, env);
                    announced = Some(resume_at);
                }
                env.set_surfaced(&key, Some(Surfaced::of(&breach)));
                // Sleep to the resume second (+1: a record leaves the window
                // AT `t + period`), in bounded slices so a stopped run is
                // seen and a raised budget is noticed.
                let until = resume_at.map(|r| r + 1).unwrap_or(now + WAIT_POLL_MAX.as_secs() as i64);
                let span = Duration::from_secs((until - now).clamp(1, WAIT_POLL_MAX.as_secs() as i64) as u64);
                let slept = sleep_sliced(env, span, caller);
                waited_ms = waited_ms.saturating_add(slept);
                env.paused(slept);
                match env.reload(&b.endpoint_id, caller.profiles_file) {
                    Some(Some(fresh)) => b = fresh,
                    Some(None) => {
                        // Switched off, or the endpoint removed from the
                        // registry, while waiting: the operator took the
                        // budget away, so go ahead (unless the run was
                        // stopped meanwhile: re-checked at the top of the
                        // loop, which evaluates nothing against no budget).
                        if let Some(why) = env.stop_reason(caller) {
                            return Err(stopped_wait(&b, why, waited_ms, announced.is_some(), caller, env));
                        }
                        resume_if_waited(&b, caller, env, waited_ms);
                        return Ok(());
                    }
                    None => {}
                }
            }
        }
    }
}

/// A wait whose run was stopped: close it in the log (a `budget.stop`
/// record, when a `budget.wait` was announced, so the wait is ended rather
/// than inferred) and return the error. Nothing was sent.
fn stopped_wait(
    b: &EndpointBudget,
    why: String,
    waited_ms: u64,
    announced: bool,
    caller: &BudgetCaller<'_>,
    env: &dyn BudgetEnv,
) -> anyhow::Error {
    let message = format!(
        "darkmux: stopped waiting on endpoint `{}`'s budget ({}): {why}; nothing was sent",
        b.endpoint_id,
        b.describe()
    );
    if announced {
        stop_record(&b.endpoint_id, &why, waited_ms, &message, caller, env);
    }
    anyhow::anyhow!(message)
}

/// The `budget.stop` record: a wait ended because its run was stopped.
fn stop_record(endpoint_id: &str, reason: &str, waited_ms: u64, message: &str, caller: &BudgetCaller<'_>, env: &dyn BudgetEnv) {
    env.emit(record(
        darkmux_flow::Level::Warn,
        darkmux_flow::FlowAction::BudgetStop,
        caller,
        serde_json::json!({
            "scope": "endpoint",
            "endpoint_id": endpoint_id,
            "reason": reason,
            "waited_ms": waited_ms,
            "pid": std::process::id(),
            "message": message,
        }),
    ));
}

/// Sleep `span` in [`WAIT_SLICE`]s, stopping early once the caller's run
/// was stopped. Returns the milliseconds slept.
fn sleep_sliced(env: &dyn BudgetEnv, span: Duration, caller: &BudgetCaller<'_>) -> u64 {
    let mut left = span;
    let mut slept = 0u64;
    while !left.is_zero() {
        if env.stop_reason(caller).is_some() {
            break;
        }
        let step = left.min(WAIT_SLICE);
        env.sleep(step);
        slept += step.as_millis() as u64;
        left = left.saturating_sub(step);
    }
    slept
}

fn metric_word(m: Metric) -> &'static str {
    match m {
        Metric::Tokens => "tokens",
        Metric::Calls => "calls",
    }
}

/// `1h 4m`, `3m 20s`, `45s`.
pub fn human_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m {s}s"),
        _ => format!("{h}h {m}m"),
    }
}

fn record(
    level: darkmux_flow::Level,
    action: darkmux_flow::FlowAction,
    caller: &BudgetCaller<'_>,
    payload: serde_json::Value,
) -> darkmux_flow::FlowRecord {
    crate::dispatch::build_telemetry_record(
        level,
        action,
        BUDGET_SOURCE,
        caller.role_id.unwrap_or("budget"),
        caller.session,
        caller.execution,
        caller.model,
        caller.phase_id,
        payload,
    )
}

fn warn(b: &EndpointBudget, br: &Breach, caller: &BudgetCaller<'_>, env: &dyn BudgetEnv) {
    let what = match br.level {
        Some(BreachLevel::AtLimit) => "has reached its budget".to_string(),
        Some(BreachLevel::Early) => format!("is at {}% of its budget", (br.spent.saturating_mul(100)) / br.limit.max(1)),
        None => "is not fully metered".to_string(),
    };
    // (N4) Not fully metered is its own fact, said beside any level: the
    // spend shown is a floor.
    let (floor, unmetered) = match br.unmetered {
        0 => ("", String::new()),
        n => (
            "at least ",
            format!("; {n} call(s) reported no complete token count (no prompt count, or no usage)"),
        ),
    };
    let message = format!(
        "darkmux: ⚠ endpoint `{}` {what}: {floor}{} of {} {} in the last {}{unmetered} ({})",
        b.endpoint_id,
        br.spent,
        br.limit,
        metric_word(br.metric),
        b.period,
        match b.policy {
            // (review #2) An early warning under `wait` continues too: calls
            // wait only once the budget is reached.
            BudgetPolicy::Wait => "policy wait: continuing; calls wait once the budget is reached",
            _ => "policy warn: continuing",
        }
    );
    env.say(&message);
    env.emit(record(
        darkmux_flow::Level::Warn,
        darkmux_flow::FlowAction::BudgetWarn,
        caller,
        serde_json::json!({
            "scope": "endpoint",
            "endpoint_id": b.endpoint_id,
            "policy": darkmux_types::config_enum::ConfigEnum::token(b.policy),
            "level": br.level,
            "metric": br.metric,
            "spent": br.spent,
            "limit": br.limit,
            "unmetered_calls": br.unmetered,
            "period": b.period,
            "warn_at": b.warn_at,
            "message": message,
        }),
    ));
}

fn announce_wait(
    b: &EndpointBudget,
    br: &Breach,
    resume_at: Option<i64>,
    now: i64,
    caller: &BudgetCaller<'_>,
    env: &dyn BudgetEnv,
) {
    let when = match resume_at {
        Some(r) => format!(
            "resuming in about {} (at {})",
            human_duration((r - now).max(0) as u64),
            darkmux_flow::ts_utc_at(r)
        ),
        None => "until its window has room".to_string(),
    };
    let how_to_stop = match caller.session.mission_id() {
        Some(mid) => format!("`darkmux mission abort {mid}` ends the wait without sending"),
        None => "Ctrl-C ends the wait without sending".to_string(),
    };
    let floor = if br.unmetered > 0 { "at least " } else { "" };
    let message = format!(
        "darkmux: endpoint `{}` has reached its budget ({floor}{} of {} {} in the last {}); calls to it are \
         waiting, {when}. {how_to_stop}.",
        b.endpoint_id,
        br.spent,
        br.limit,
        metric_word(br.metric),
        b.period
    );
    env.say(&message);
    env.emit(record(
        darkmux_flow::Level::Warn,
        darkmux_flow::FlowAction::BudgetWait,
        caller,
        serde_json::json!({
            "scope": "endpoint",
            "endpoint_id": b.endpoint_id,
            "policy": "wait",
            "metric": br.metric,
            "spent": br.spent,
            "limit": br.limit,
            "unmetered_calls": br.unmetered,
            "period": b.period,
            "resume_at": resume_at.map(darkmux_flow::ts_utc_at),
            "wait_seconds": resume_at.map(|r| (r - now).max(0)),
            "pid": std::process::id(),
            "message": message,
        }),
    ));
}

fn resume_if_waited(b: &EndpointBudget, caller: &BudgetCaller<'_>, env: &dyn BudgetEnv, waited_ms: u64) {
    if waited_ms == 0 {
        return;
    }
    let message = format!(
        "darkmux: endpoint `{}` has room again after waiting {}; resuming",
        b.endpoint_id,
        human_duration(waited_ms / 1000)
    );
    env.say(&message);
    env.emit(record(
        darkmux_flow::Level::Info,
        darkmux_flow::FlowAction::BudgetResume,
        caller,
        serde_json::json!({
            "scope": "endpoint",
            "endpoint_id": b.endpoint_id,
            "waited_ms": waited_ms,
            "pid": std::process::id(),
            "message": message,
        }),
    ));
}

// ── The in-run pacer (agentic-remote container dispatches) ──────────────

/// The pace-file `reason` a budget wait writes: the runtime echoes it into
/// its rest events, and the host turns those into `dispatch.rest` records
/// with this reason, so a budget rest reads as its own state, not thermal.
pub const PACE_REASON: &str = "budget";

/// How often the pacer re-reads the window while NOT waiting. While it
/// waits it re-reads on every sampler tick (~2 s).
pub const PACER_CHECK_MS: u64 = 5_000;
/// How often a waiting pacer re-reads the endpoint's limits from the
/// registry (a raised or switched-off budget releases the wait).
pub const PACER_RELOAD_MS: u64 = 30_000;

/// What a pacer tick changed, for the sampler to report as `dispatch.rest`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PacerEvent {
    Paused { state: String },
    Resumed { state: String },
    /// (review MF1) The run was stopped (Ctrl-C, `mission abort`, an
    /// abandoned phase) while the pacer held it: the pacer never releases a
    /// stopped run, and the sampler ends it the way an interrupt does, so
    /// nothing more is sent. Reported once.
    Stopped { reason: String },
}

/// What the other governors hold on the pace file this tick, so the pacer
/// neither overwrites nor drops them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OtherPacing {
    /// Thermal or battery holds a genuine pause: the pacer writes nothing.
    pub pausing: bool,
    /// Thermal is duty-cycling (`pause: false` with a turn delay):
    /// `(turn_delay_ms, state)`. A pacer releasing its own pause writes this
    /// instruction back rather than a bare `pause: false` (review C6).
    pub duty_cycle: Option<(u64, String)>,
}

/// The duty-cycle `reason` the thermal governor writes, restored verbatim
/// when the pacer lets go of the file during a duty-cycle episode.
const THERMAL_DUTY_REASON: &str = "thermal-duty-cycle";

/// The budget gate for a dispatch whose calls are made INSIDE the runtime
/// container (an agentic-remote brain, #1187): the host cannot stand in
/// front of each call, so it rides the dispatch's host sampler (a ~2 s
/// tick) and, when the window is full under `wait`, pauses the runtime
/// between turns through the pace file, exactly as the thermal governor
/// does. The runtime rests in bounded increments that count as proof of
/// work, so neither its inactivity clock nor the host watchdog fires while
/// it waits; the wait is also recorded in [`darkmux_types::run_pause`] for
/// the run's wall-clock bound. `warn` surfaces the breach and never pauses.
///
/// While NOT waiting it re-reads the window every [`PACER_CHECK_MS`] (5 s),
/// so the runtime can start up to that many seconds of turns after a window
/// fills and before the pacer notices; while waiting, every tick.
///
/// Sharing the pace file: when another governor (thermal, battery) is
/// pausing, the pacer writes nothing (theirs is the stronger reason to
/// hold) and re-asserts its own pause on the next tick they are not. When
/// it releases during a thermal duty cycle, it writes the duty cycle's
/// instruction back.
pub struct BudgetPacer {
    budget: EndpointBudget,
    profiles_file: Option<String>,
    since_check_ms: u64,
    since_reload_ms: u64,
    checked_once: bool,
    pausing: bool,
    waited_ms: u64,
    announced: Option<Option<i64>>,
    /// The run was stopped and `Stopped` was reported: hold, never release.
    stopped: bool,
}

impl BudgetPacer {
    pub fn new(budget: EndpointBudget, profiles_file: Option<String>) -> Self {
        BudgetPacer {
            budget,
            profiles_file,
            since_check_ms: 0,
            since_reload_ms: 0,
            checked_once: false,
            pausing: false,
            waited_ms: 0,
            announced: None,
            stopped: false,
        }
    }

    /// True while this pacer holds the runtime.
    pub fn is_pausing(&self) -> bool {
        self.pausing
    }

    /// The pace-file `state`: the endpoint id. When the wait resumes lives
    /// on the `budget.wait` record; the rest reason reads "budget · <id>".
    fn state(&self) -> String {
        self.budget.endpoint_id.clone()
    }

    /// One sampler tick. `elapsed_ms` is the real gap since the last tick,
    /// `others` what thermal and battery hold on the pace file now.
    pub fn on_tick(
        &mut self,
        elapsed_ms: u64,
        host_out: &Path,
        others: &OtherPacing,
        caller: &BudgetCaller<'_>,
        env: &dyn BudgetEnv,
    ) -> Option<PacerEvent> {
        self.since_check_ms = self.since_check_ms.saturating_add(elapsed_ms);
        if self.stopped {
            // Reported already; keep holding (the sampler is ending the run).
            if self.pausing && !others.pausing {
                crate::pace_file::write(host_out, true, PACE_REASON, &self.state());
            }
            return None;
        }
        if self.pausing {
            // (review MF1) Every tick while holding: a stopped run is never
            // released, and ends here.
            if let Some(reason) = env.stop_reason(caller) {
                return Some(self.stop(reason, caller, env));
            }
            self.waited_ms = self.waited_ms.saturating_add(elapsed_ms);
            env.paused(elapsed_ms);
            self.since_reload_ms = self.since_reload_ms.saturating_add(elapsed_ms);
            if self.since_reload_ms >= PACER_RELOAD_MS {
                self.since_reload_ms = 0;
                match env.reload(&self.budget.endpoint_id, self.profiles_file.as_deref()) {
                    Some(Some(fresh)) => self.budget = fresh,
                    Some(None) => return self.release(host_out, others, caller, env),
                    None => {}
                }
            }
        }
        if !self.pausing && self.checked_once && self.since_check_ms < PACER_CHECK_MS {
            return None;
        }
        self.checked_once = true;
        self.since_check_ms = 0;
        let now = env.now();
        let entries = env.window(&self.budget, now);
        let key = format!("endpoint:{}", self.budget.endpoint_id);
        match evaluate(&self.budget, &entries, now) {
            Verdict::Wait { breach, resume_at } => {
                if let Some(reason) = env.stop_reason(caller) {
                    // A stopped run whose window is full: hold it (never let
                    // another turn go out) and end it.
                    if !others.pausing {
                        crate::pace_file::write(host_out, true, PACE_REASON, &self.state());
                    }
                    self.pausing = true;
                    return Some(self.stop(reason, caller, env));
                }
                if self.announced != Some(resume_at) {
                    announce_wait(&self.budget, &breach, resume_at, now, caller, env);
                    self.announced = Some(resume_at);
                }
                env.set_surfaced(&key, Some(Surfaced::of(&breach)));
                let state = self.state();
                if !others.pausing {
                    crate::pace_file::write(host_out, true, PACE_REASON, &state);
                }
                if !self.pausing {
                    self.pausing = true;
                    return Some(PacerEvent::Paused { state });
                }
                None
            }
            Verdict::Warn(br) => {
                if Surfaced::of(&br).is_news_after(env.last_surfaced(&key)) {
                    warn(&self.budget, &br, caller, env);
                }
                env.set_surfaced(&key, Some(Surfaced::of(&br)));
                self.release(host_out, others, caller, env)
            }
            Verdict::Proceed => {
                env.set_surfaced(&key, None);
                self.release(host_out, others, caller, env)
            }
        }
    }

    fn release(
        &mut self,
        host_out: &Path,
        others: &OtherPacing,
        caller: &BudgetCaller<'_>,
        env: &dyn BudgetEnv,
    ) -> Option<PacerEvent> {
        if !self.pausing {
            return None;
        }
        // (review MF1) A stopped run never reaches here while held: every
        // tick of a held run checks the stop first (`on_tick`), before the
        // window, a reload or a release.
        self.pausing = false;
        self.announced = None;
        let state = self.state();
        if !others.pausing {
            match &others.duty_cycle {
                Some((delay, thermal_state)) => crate::pace_file::write_with_turn_delay(
                    host_out,
                    false,
                    THERMAL_DUTY_REASON,
                    thermal_state,
                    Some(*delay),
                ),
                None => crate::pace_file::write(host_out, false, PACE_REASON, &state),
            }
        }
        resume_if_waited(&self.budget, caller, env, self.waited_ms.max(1));
        self.waited_ms = 0;
        Some(PacerEvent::Resumed { state })
    }
}

impl BudgetPacer {
    /// Report the stop once: a CLI line, and a `budget.stop` record when a
    /// wait was announced (the gate's rule: a stop record always follows its
    /// `budget.wait`). The caller (the sampler) ends the run.
    fn stop(&mut self, reason: String, caller: &BudgetCaller<'_>, env: &dyn BudgetEnv) -> PacerEvent {
        self.stopped = true;
        let message = format!(
            "darkmux: stopped waiting on endpoint `{}`'s budget: {reason}; ending the run, nothing more is sent",
            self.budget.endpoint_id
        );
        env.say(&message);
        if self.announced.is_some() {
            stop_record(&self.budget.endpoint_id, &reason, self.waited_ms, &message, caller, env);
        }
        PacerEvent::Stopped { reason }
    }
}

// ── The per-step cap ────────────────────────────────────────────────────

/// Admit one hosted call against a STEP bucket (`remote.max_tokens_per_step`),
/// reserving `requested` (the call's completion cap) so concurrent siblings
/// of one `bucket_group` see it in flight. Never holds, never refuses: the
/// per-step cap has only `off` and `warn`.
pub fn admit_step(bucket: &Mutex<crate::remote_budget::RemoteBudget>, requested: u32) {
    bucket.lock().unwrap_or_else(|p| p.into_inner()).admit_reserve(requested);
}

/// [`settle_step`] against the environment in effect on this thread (the
/// live one, or a test's stand-in, [`with_env`]): what the production call
/// sites use, so a behavioral test sees the settle they perform.
pub fn settle_step_live(
    bucket: &Mutex<crate::remote_budget::RemoteBudget>,
    reserved: u32,
    actual: u64,
    calls: u32,
    step: &str,
    caller: &BudgetCaller<'_>,
) {
    with_env(|env| settle_step(bucket, reserved, actual, calls, step, caller, env))
}

/// Settle one call against a STEP bucket and surface the breach, once, if
/// this call's spend reached the cap (`warn`).
pub fn settle_step(
    bucket: &Mutex<crate::remote_budget::RemoteBudget>,
    reserved: u32,
    actual: u64,
    calls: u32,
    step: &str,
    caller: &BudgetCaller<'_>,
    env: &dyn BudgetEnv,
) {
    let breach = {
        let mut b = bucket.lock().unwrap_or_else(|p| p.into_inner());
        b.settle(reserved, actual, calls);
        b.take_breach()
    };
    let Some(br) = breach else { return };
    let message = format!(
        "darkmux: ⚠ step `{step}` has reached its per-step cap: {} of {} hosted tokens \
         (remote.max_tokens_per_step); policy warn: continuing",
        br.used, br.budget
    );
    env.say(&message);
    env.emit(record(
        darkmux_flow::Level::Warn,
        darkmux_flow::FlowAction::BudgetWarn,
        caller,
        serde_json::json!({
            "scope": "step", "step": step, "policy": "warn",
            "level": BreachLevel::AtLimit, "metric": Metric::Tokens,
            "spent": br.used, "limit": br.budget, "message": message,
        }),
    ));
}

// ── Reading waits back (mission status) ─────────────────────────────────

/// A wait still in progress, read back from the flow log.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ActiveWait {
    /// `endpoint` (the one budget that waits).
    pub scope: String,
    /// The endpoint id.
    pub subject: String,
    pub mission_id: Option<String>,
    pub session_id: Option<String>,
    /// When it resumes, when known (`YYYY-MM-DDTHH:MM:SSZ`).
    pub resume_at: Option<String>,
    /// Seconds from `now` to `resume_at`.
    pub resumes_in_secs: Option<u64>,
    pub message: String,
}

/// The oldest a wait's announcement can be: a wait is announced once, when
/// it starts, and lasts at most the widest window it waits on. Callers pass
/// the widest `period` configured (`widest_window_secs`); never less than a
/// day.
pub fn waits_lookback_secs(widest_window_secs: Option<u64>) -> u64 {
    widest_window_secs.unwrap_or(0).max(86_400)
}

/// The widest window period among a registry's endpoint budgets.
pub fn widest_window_secs(reg: &darkmux_types::ProfileRegistry) -> Option<u64> {
    reg.endpoints
        .values()
        .filter_map(|ep| ep.known_limits()?.window_budget().map(|w| w.period_secs))
        .max()
}

/// Every budget wait still open at `now`, from the day files covering the
/// last `lookback_secs` (review C2: a long window's wait was announced up to
/// a whole window ago): the newest `budget.wait` per (scope, subject,
/// session, pid) with no later `budget.resume`, whose process is alive,
/// whose run was not stopped, and whose resume time has not passed. For
/// `darkmux mission status`.
pub fn active_waits(dir: &Path, now: i64, lookback_secs: u64, alive: &dyn Fn(u32) -> bool) -> Vec<ActiveWait> {
    let mut latest: BTreeMap<(String, String, String, u64), (bool, serde_json::Value)> = BTreeMap::new();
    let mut day = (now - lookback_secs as i64).div_euclid(86_400) * 86_400;
    let last = darkmux_flow::day_utc_at(now);
    while darkmux_flow::day_utc_at(day) <= last {
        let stem = darkmux_flow::day_utc_at(day);
        day += 86_400;
        let Ok(text) = std::fs::read_to_string(dir.join(format!("{stem}.jsonl"))) else { continue };
        // Cheap pre-filter: only a budget record can be one of the three.
        for v in text.lines().filter(|l| l.contains("\"budget.")).filter_map(darkmux_flow::reader::parse_value) {
            let waiting = match darkmux_flow::reader::action_of(&v) {
                Some(darkmux_flow::FlowAction::BudgetWait) => true,
                Some(darkmux_flow::FlowAction::BudgetResume | darkmux_flow::FlowAction::BudgetStop) => false,
                _ => continue,
            };
            let p = crate::usage::payload_of(&v);
            let scope = p.get("scope").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let subject = p.get("endpoint_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let session = v.get("session_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let pid = p.get("pid").and_then(|x| x.as_u64()).unwrap_or(0);
            latest.insert((scope, subject, session, pid), (waiting, v));
        }
    }
    latest
        .into_iter()
        .filter_map(|((scope, subject, session, pid), (waiting, v))| {
            if !waiting || !alive(pid as u32) {
                return None;
            }
            let s = |k: &str| v.get(k).and_then(|x| x.as_str());
            if run_stop_reason(s("mission_id"), s("phase_id")).is_some() {
                return None;
            }
            let p = crate::usage::payload_of(&v);
            let resume_at = p.get("resume_at").and_then(|x| x.as_str()).map(str::to_string);
            let resume_secs = resume_at.as_deref().and_then(crate::records_emitted::parse_ts_secs);
            if resume_secs.is_some_and(|r| r + 1 < now) {
                return None;
            }
            Some(ActiveWait {
                scope,
                subject,
                mission_id: s("mission_id").map(str::to_string),
                session_id: (!session.is_empty()).then_some(session),
                resume_at,
                resumes_in_secs: resume_secs.map(|r| (r - now).max(0) as u64),
                message: p.get("message").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "budget_tests.rs"]
pub(crate) mod tests;
