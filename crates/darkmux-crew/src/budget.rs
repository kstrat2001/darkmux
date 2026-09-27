//! (#2902 step 5) Budgets: the operator's own limits on what darkmux spends
//! at an endpoint, and what reaching one does.
//!
//! **Nothing here runs unless the operator sets a budget.** darkmux ships no
//! number: an endpoint with no `limits.window` budget, or with `policy:
//! "off"`, is returned from [`EndpointBudget::of`] as `None` before any file
//! is opened, so the gate costs a struct read.
//!
//! **Once set, a breach never stops the run.** The policy is one of
//! [`BudgetPolicy`]'s three values:
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
//!   thermal governor's pause. A hard stop is the operator's own
//!   `darkmux mission abort` (the wait polls the interrupt flag).
//!
//! **The window.** "The last `period` from now": a rolling window with no
//! calendar reset. Its spend is the sum of this machine's usage records
//! (`telemetry.tokens`, one per model call, `crate::usage`) that carry this
//! endpoint's id (`endpoint_id`), read with the same per-record reading the
//! token fold uses ([`crate::usage::usage_contribution`]). Both `purpose`s
//! count, work and utility: a budget is about what the ENDPOINT served, and
//! a provider bills a utility call like any other. Records written before
//! `endpoint_id` existed (flow < 1.64.0) carry no id and are not counted.
//! Another machine's spend at the same endpoint is not seen: the window
//! reads this machine's flow log.
//!
//! **"Room"** is `spent < budget`, for tokens and calls alike. The next
//! call's own cost is unknowable until it returns, so a call admitted with
//! room can overshoot by itself (a soft ceiling, the same reading the stage
//! bucket has always had), and the next one then waits.
//!
//! **Cost.** The window is read before every hosted call to a budgeted
//! endpoint, so it must not re-scan the flow history each time (the #2891
//! trap). [`Ledger`] opens only the day files the window can touch and
//! remembers, per file, the byte offset it has read to: a later call reads
//! only the bytes appended since, and keeps only the (endpoint id, second,
//! tokens) triples of records that carry an id. Measured in
//! `ledger_cost_is_incremental_after_the_first_read` below.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use darkmux_types::{BudgetPolicy, ModelEndpoint, WindowBudget};

/// The flow-record telemetry `source` every budget record carries.
pub const BUDGET_SOURCE: &str = "budget";
/// A budget was reached (or its `warn_at` fraction was) under `warn`, or a
/// stage budget was crossed. Level Warn. The call went ahead.
pub const BUDGET_WARN_ACTION: &str = "budget.warn";
/// A call is waiting on a budget (`wait`). Level Warn. Carries when it will
/// resume, when that is known.
pub const BUDGET_WAIT_ACTION: &str = "budget.wait";
/// A waiting call went ahead. Level Info. Carries how long it waited.
pub const BUDGET_RESUME_ACTION: &str = "budget.resume";

/// The longest single sleep while waiting: the wait re-reads the window
/// (and the endpoint's limits, so a raised or switched-off budget releases
/// it) at least this often, and polls the interrupt flag each slice.
pub const WAIT_POLL_MAX: Duration = Duration::from_secs(30);
/// How often an interrupt is noticed while a wait sleeps.
const WAIT_SLICE: Duration = Duration::from_millis(500);

/// Who a gated call is for, so a budget record lands on the right run. Every
/// field is optional: a `dispatch.map` step runs no role.
#[derive(Clone, Copy, Debug, Default)]
pub struct BudgetCaller<'a> {
    pub role_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub mission_id: Option<&'a str>,
    pub phase_id: Option<&'a str>,
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
    /// no `endpoints` id (an inline endpoint: its usage records carry no id
    /// to sum by, which `darkmux doctor` names), a MANAGED endpoint (darkmux
    /// budgets the calls it SENDS to an endpoint it does not manage; local
    /// calls carry no `endpoint_id`), no window budget, or policy `off`. `Err` names an unregistered policy (preflight refuses it first;
    /// this keeps a caller that skipped preflight from guessing one).
    pub fn of(ep: &ModelEndpoint) -> Result<Option<Self>, String> {
        let Some(limits) = ep.known_limits() else { return Ok(None) };
        let policy = limits.resolved_policy().map_err(|raw| {
            format!(
                "darkmux: endpoint budget policy `{raw}` is not one of {} (#2902)",
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

/// How far into a budget the spend is.
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
    pub level: BreachLevel,
    pub metric: Metric,
    pub spent: u64,
    pub limit: u64,
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

/// Spend inside the window: `(epoch second, tokens)` per usage record,
/// oldest first.
pub type WindowEntries = Vec<(i64, u64)>;

/// THE decision, pure: `entries` are this endpoint's records, `now` the
/// injected clock. A record at second `t` is inside the window while
/// `now < t + period`, so it leaves the window at exactly `t + period`.
pub fn evaluate(b: &EndpointBudget, entries: &[(i64, u64)], now: i64) -> Verdict {
    if !b.policy.counts() {
        return Verdict::Proceed;
    }
    let period = b.window.period_secs as i64;
    let inside: Vec<(i64, u64)> = entries.iter().copied().filter(|(t, _)| now < t + period && *t <= now).collect();
    let tokens: u64 = inside.iter().map(|(_, n)| *n).fold(0u64, u64::saturating_add);
    let calls = inside.len() as u64;
    let measured = [(Metric::Tokens, tokens, b.window.tokens), (Metric::Calls, calls, b.window.calls)];

    let mut at_limit: Vec<Breach> = Vec::new();
    let mut early: Option<Breach> = None;
    for (metric, spent, limit) in measured {
        let Some(limit) = limit else { continue };
        if spent >= limit {
            at_limit.push(Breach { level: BreachLevel::AtLimit, metric, spent, limit });
        } else if let Some(f) = b.warn_at {
            let threshold = ((limit as f64) * f).ceil() as u64;
            if spent >= threshold && early.is_none() {
                early = Some(Breach { level: BreachLevel::Early, metric, spent, limit });
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
    match early {
        Some(br) => Verdict::Warn(br),
        None => Verdict::Proceed,
    }
}

/// When the oldest records have left the window far enough for `br`'s
/// metric to be under its limit again. `None` for a limit of 0 (there is
/// never room).
fn resume_at_for(inside: &[(i64, u64)], br: &Breach, period: i64) -> Option<i64> {
    if br.limit == 0 {
        return None;
    }
    let mut remaining = br.spent;
    for (t, n) in inside {
        remaining = remaining.saturating_sub(match br.metric {
            Metric::Tokens => *n,
            Metric::Calls => 1,
        });
        if remaining < br.limit {
            return Some(t + period);
        }
    }
    None
}

// ── The window ledger ───────────────────────────────────────────────────

#[derive(Default)]
struct DayFile {
    /// Bytes read so far (always at a line boundary).
    offset: u64,
    /// `(endpoint id, epoch second, tokens)` for every usage record in the
    /// bytes read that carries an `endpoint_id`.
    entries: Vec<(String, i64, u64)>,
}

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
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let entry = self.days.entry(stem.to_string()).or_default();
        if len < entry.offset {
            // Truncated or replaced: start the file over.
            *entry = DayFile::default();
        }
        if len == entry.offset {
            return;
        }
        if file.seek(SeekFrom::Start(entry.offset)).is_err() {
            return;
        }
        let mut buf = Vec::with_capacity((len - entry.offset) as usize);
        if file.take(len - entry.offset).read_to_end(&mut buf).is_err() {
            return;
        }
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
    }
}

/// One line to a ledger entry: a usage record that carries an
/// `endpoint_id` and a parseable `ts`. A cheap substring test first, so the
/// JSON of every OTHER record (the vast majority) is never parsed.
fn parse_entry(line: &[u8]) -> Option<(String, i64, u64)> {
    const NEEDLE: &[u8] = b"\"endpoint_id\"";
    if line.len() < NEEDLE.len() || !line.windows(NEEDLE.len()).any(|w| w == NEEDLE) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    let amount = crate::usage::usage_contribution(&v)?;
    let id = crate::usage::payload_of(&v).get("endpoint_id")?.as_str()?.to_string();
    let ts = crate::records_emitted::parse_ts_secs(v.get("ts")?.as_str()?)?;
    Some((id, ts, amount.total))
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
    /// True once the process was asked to stop (`mission abort`, Ctrl-C).
    fn interrupted(&self) -> bool;
    /// The endpoint's budget as the registry says NOW, re-read while a call
    /// waits, so raising or switching off a budget releases the wait.
    /// `None`: keep the one in hand.
    fn reload(&self, endpoint_id: &str) -> Option<Option<EndpointBudget>>;
    /// Write a flow record.
    fn emit(&self, rec: darkmux_flow::FlowRecord);
    /// Print an operator line.
    fn say(&self, line: &str);
    /// Record deliberate pause time ([`darkmux_types::run_pause`]).
    fn paused(&self, ms: u64);
    /// The highest breach level already surfaced for `key` in this process.
    fn last_level(&self, key: &str) -> Option<BreachLevel>;
    fn set_last_level(&self, key: &str, level: Option<BreachLevel>);
    /// The stage budget and policy as the config says NOW (re-read from
    /// disk while a stage waits, so `darkmux config set` releases it).
    fn stage_budget_fresh(
        &self,
    ) -> (Option<u64>, Result<BudgetPolicy, darkmux_types::config_enum::BadEnumValue>);
}

/// The production environment: the system clock, the configured flows dir,
/// the live profile registry, the process-wide flow sink.
pub struct LiveEnv;

fn warned_levels() -> &'static Mutex<HashMap<String, BreachLevel>> {
    static LEVELS: std::sync::OnceLock<Mutex<HashMap<String, BreachLevel>>> = std::sync::OnceLock::new();
    LEVELS.get_or_init(|| Mutex::new(HashMap::new()))
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
    fn interrupted(&self) -> bool {
        darkmux_types::interrupt::is_set()
    }
    fn reload(&self, endpoint_id: &str) -> Option<Option<EndpointBudget>> {
        let loaded = darkmux_profiles::profiles::load_registry(None).ok()?;
        let ep = loaded.registry.endpoints.get(endpoint_id)?;
        let mut named = ep.clone();
        named.source = darkmux_types::EndpointSource::Named(endpoint_id.to_string());
        EndpointBudget::of(&named).ok()
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
    fn last_level(&self, key: &str) -> Option<BreachLevel> {
        warned_levels().lock().unwrap_or_else(|p| p.into_inner()).get(key).copied()
    }
    fn stage_budget_fresh(
        &self,
    ) -> (Option<u64>, Result<BudgetPolicy, darkmux_types::config_enum::BadEnumValue>) {
        darkmux_types::config_access::remote_stage_budget_fresh()
    }
    fn set_last_level(&self, key: &str, level: Option<BreachLevel>) {
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

/// THE gate every hosted call to an endpoint passes through, before the
/// call. `Ok` means go ahead (after warning, or after waiting); `Err` only
/// for an unregistered policy or an interrupt during a wait. An endpoint
/// with no budget returns at once, without reading anything.
pub fn admit_endpoint(ep: &ModelEndpoint, caller: &BudgetCaller<'_>) -> anyhow::Result<()> {
    let Some(budget) = EndpointBudget::of(ep).map_err(|e| anyhow::anyhow!(e))? else { return Ok(()) };
    admit_with(budget, caller, &LiveEnv)
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
            Verdict::Proceed => {
                env.set_last_level(&key, None);
                resume_if_waited(&b, caller, env, waited_ms);
                return Ok(());
            }
            Verdict::Warn(br) => {
                if env.last_level(&key).map_or(true, |l| l < br.level) {
                    warn(&b, &br, caller, env);
                }
                env.set_last_level(&key, Some(br.level));
                resume_if_waited(&b, caller, env, waited_ms);
                return Ok(());
            }
            Verdict::Wait { breach, resume_at } => {
                if env.interrupted() {
                    anyhow::bail!(
                        "darkmux: interrupted while waiting on endpoint `{}`'s budget ({}); nothing was sent",
                        b.endpoint_id,
                        b.describe()
                    );
                }
                if announced != Some(resume_at) {
                    announce_wait(&b, &breach, resume_at, now, caller, env);
                    announced = Some(resume_at);
                }
                env.set_last_level(&key, Some(BreachLevel::AtLimit));
                // Sleep to the resume second (+1: a record leaves the window
                // AT `t + period`), in bounded slices so an interrupt is seen
                // and a raised budget is noticed.
                let until = resume_at.map(|r| r + 1).unwrap_or(now + WAIT_POLL_MAX.as_secs() as i64);
                let span = Duration::from_secs((until - now).clamp(1, WAIT_POLL_MAX.as_secs() as i64) as u64);
                let slept = sleep_sliced(env, span);
                waited_ms = waited_ms.saturating_add(slept);
                env.paused(slept);
                match env.reload(&b.endpoint_id) {
                    Some(Some(fresh)) => b = fresh,
                    Some(None) => {
                        // Switched off (or removed) while waiting: go ahead.
                        resume_if_waited(&b, caller, env, waited_ms);
                        return Ok(());
                    }
                    None => {}
                }
            }
        }
    }
}

/// Sleep `span` in [`WAIT_SLICE`]s, stopping early on an interrupt.
/// Returns the milliseconds slept.
fn sleep_sliced(env: &dyn BudgetEnv, span: Duration) -> u64 {
    let mut left = span;
    let mut slept = 0u64;
    while !left.is_zero() {
        if env.interrupted() {
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
    action: &str,
    caller: &BudgetCaller<'_>,
    payload: serde_json::Value,
) -> darkmux_flow::FlowRecord {
    let mut rec = crate::dispatch::build_telemetry_record(
        level,
        action,
        BUDGET_SOURCE,
        caller.role_id.unwrap_or("budget"),
        caller.session_id.unwrap_or(""),
        caller.model,
        caller.mission_id,
        caller.phase_id,
        payload,
    );
    if caller.session_id.is_none() {
        rec.session_id = None;
    }
    rec
}

fn warn(b: &EndpointBudget, br: &Breach, caller: &BudgetCaller<'_>, env: &dyn BudgetEnv) {
    let what = match br.level {
        BreachLevel::AtLimit => "has reached its budget".to_string(),
        BreachLevel::Early => format!("is at {}% of its budget", (br.spent.saturating_mul(100)) / br.limit.max(1)),
    };
    let message = format!(
        "darkmux: ⚠ endpoint `{}` {what}: {} of {} {} in the last {} (policy warn: continuing)",
        b.endpoint_id,
        br.spent,
        br.limit,
        metric_word(br.metric),
        b.period
    );
    env.say(&message);
    env.emit(record(
        darkmux_flow::Level::Warn,
        BUDGET_WARN_ACTION,
        caller,
        serde_json::json!({
            "scope": "endpoint",
            "endpoint_id": b.endpoint_id,
            "policy": darkmux_types::config_enum::ConfigEnum::token(b.policy),
            "level": br.level,
            "metric": br.metric,
            "spent": br.spent,
            "limit": br.limit,
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
        None => "the budget is 0, so it waits until it is raised in profiles.json".to_string(),
    };
    let message = format!(
        "darkmux: endpoint `{}` has reached its budget ({} of {} {} in the last {}); \
         calls to it are waiting, {when}. `darkmux mission abort <id>` stops the run.",
        b.endpoint_id,
        br.spent,
        br.limit,
        metric_word(br.metric),
        b.period
    );
    env.say(&message);
    env.emit(record(
        darkmux_flow::Level::Warn,
        BUDGET_WAIT_ACTION,
        caller,
        serde_json::json!({
            "scope": "endpoint",
            "endpoint_id": b.endpoint_id,
            "policy": "wait",
            "metric": br.metric,
            "spent": br.spent,
            "limit": br.limit,
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
        BUDGET_RESUME_ACTION,
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

/// How often the pacer re-reads the window while NOT waiting.
pub const PACER_CHECK_MS: u64 = 5_000;
/// How often a waiting pacer re-reads the endpoint's limits from the
/// registry (a raised or switched-off budget releases the wait).
pub const PACER_RELOAD_MS: u64 = 30_000;

/// What a pacer tick changed, for the sampler to report as `dispatch.rest`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PacerEvent {
    Paused { state: String },
    Resumed { state: String },
}

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
/// Sharing the pace file: when another governor (thermal, battery) is
/// pausing, the pacer writes nothing (theirs is the stronger reason to
/// hold) and re-asserts its own pause on the next tick they are not. A tick
/// of latency is the bound: the runtime can start one more turn in the
/// ~2 s between a window filling and the pacer noticing.
pub struct BudgetPacer {
    budget: EndpointBudget,
    since_check_ms: u64,
    since_reload_ms: u64,
    checked_once: bool,
    pausing: bool,
    waited_ms: u64,
    announced: Option<Option<i64>>,
}

impl BudgetPacer {
    pub fn new(budget: EndpointBudget) -> Self {
        BudgetPacer {
            budget,
            since_check_ms: 0,
            since_reload_ms: 0,
            checked_once: false,
            pausing: false,
            waited_ms: 0,
            announced: None,
        }
    }

    /// True while this pacer holds the runtime.
    pub fn is_pausing(&self) -> bool {
        self.pausing
    }

    fn state(&self, resume_at: Option<i64>) -> String {
        match resume_at {
            Some(r) => format!("{} until {}", self.budget.endpoint_id, darkmux_flow::ts_utc_at(r)),
            None => format!("{} (budget 0)", self.budget.endpoint_id),
        }
    }

    /// One sampler tick. `elapsed_ms` is the real gap since the last tick,
    /// `others_pausing` whether thermal or battery holds the pace file now.
    pub fn on_tick(
        &mut self,
        elapsed_ms: u64,
        host_out: &Path,
        others_pausing: bool,
        caller: &BudgetCaller<'_>,
        env: &dyn BudgetEnv,
    ) -> Option<PacerEvent> {
        self.since_check_ms = self.since_check_ms.saturating_add(elapsed_ms);
        if self.pausing {
            self.waited_ms = self.waited_ms.saturating_add(elapsed_ms);
            env.paused(elapsed_ms);
            self.since_reload_ms = self.since_reload_ms.saturating_add(elapsed_ms);
            if self.since_reload_ms >= PACER_RELOAD_MS {
                self.since_reload_ms = 0;
                match env.reload(&self.budget.endpoint_id) {
                    Some(Some(fresh)) => self.budget = fresh,
                    Some(None) => return self.release(host_out, others_pausing, caller, env),
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
                if self.announced != Some(resume_at) {
                    announce_wait(&self.budget, &breach, resume_at, now, caller, env);
                    self.announced = Some(resume_at);
                }
                env.set_last_level(&key, Some(BreachLevel::AtLimit));
                let state = self.state(resume_at);
                if !others_pausing {
                    crate::pace_file::write(host_out, true, PACE_REASON, &state);
                }
                if !self.pausing {
                    self.pausing = true;
                    return Some(PacerEvent::Paused { state });
                }
                None
            }
            Verdict::Warn(br) => {
                if env.last_level(&key).map_or(true, |l| l < br.level) {
                    warn(&self.budget, &br, caller, env);
                }
                env.set_last_level(&key, Some(br.level));
                self.release(host_out, others_pausing, caller, env)
            }
            Verdict::Proceed => {
                env.set_last_level(&key, None);
                self.release(host_out, others_pausing, caller, env)
            }
        }
    }

    fn release(
        &mut self,
        host_out: &Path,
        others_pausing: bool,
        caller: &BudgetCaller<'_>,
        env: &dyn BudgetEnv,
    ) -> Option<PacerEvent> {
        if !self.pausing {
            return None;
        }
        self.pausing = false;
        self.announced = None;
        let state = self.budget.endpoint_id.clone();
        if !others_pausing {
            crate::pace_file::write(host_out, false, PACE_REASON, &state);
        }
        resume_if_waited(&self.budget, caller, env, self.waited_ms.max(1));
        self.waited_ms = 0;
        Some(PacerEvent::Resumed { state })
    }
}

// ── The stage budget ────────────────────────────────────────────────────

/// Admit one hosted call against a STAGE bucket (`remote.max_tokens_per_
/// execution`), reserving `requested`. Under `warn` (or no budget, or `off`)
/// this never holds. Under `wait` with the budget spent it waits, re-reading
/// the budget and policy from disk every [`WAIT_POLL_MAX`], until the
/// operator raises the budget or changes the policy, and reports the wait
/// like an endpoint's. `Err` only on an interrupt during the wait, or a
/// policy that became unregistered while waiting.
pub fn admit_stage(
    bucket: &Mutex<crate::remote_budget::RemoteBudget>,
    requested: u32,
    stage: &str,
    caller: &BudgetCaller<'_>,
    env: &dyn BudgetEnv,
) -> anyhow::Result<()> {
    use crate::remote_budget::StageAdmit;
    let mut waited_ms = 0u64;
    let mut announced = false;
    loop {
        let admit = bucket.lock().unwrap_or_else(|p| p.into_inner()).admit_reserve(requested);
        match admit {
            StageAdmit::Proceed => {
                if waited_ms > 0 {
                    let message = format!(
                        "darkmux: stage `{stage}` has room again after waiting {}; resuming",
                        human_duration(waited_ms / 1000)
                    );
                    env.say(&message);
                    env.emit(record(
                        darkmux_flow::Level::Info,
                        BUDGET_RESUME_ACTION,
                        caller,
                        serde_json::json!({
                            "scope": "stage", "stage": stage, "waited_ms": waited_ms,
                            "pid": std::process::id(), "message": message,
                        }),
                    ));
                }
                return Ok(());
            }
            StageAdmit::Wait(br) => {
                if env.interrupted() {
                    anyhow::bail!(
                        "darkmux: interrupted while stage `{stage}` waited on its budget ({} of {} tokens); nothing was sent",
                        br.used,
                        br.budget
                    );
                }
                if !announced {
                    announced = true;
                    let message = format!(
                        "darkmux: stage `{stage}` has spent its stage budget ({} of {} tokens,                          remote.max_tokens_per_execution); its hosted calls are waiting until the budget                          is raised (`darkmux config set remote.max_tokens_per_execution <n>`) or                          remote.stage_budget_policy is changed. `darkmux mission abort <id>` stops the run.",
                        br.used, br.budget
                    );
                    env.say(&message);
                    env.emit(record(
                        darkmux_flow::Level::Warn,
                        BUDGET_WAIT_ACTION,
                        caller,
                        serde_json::json!({
                            "scope": "stage", "stage": stage, "policy": "wait",
                            "metric": Metric::Tokens, "spent": br.used, "limit": br.budget,
                            "resume_at": null, "wait_seconds": null,
                            "pid": std::process::id(), "message": message,
                        }),
                    ));
                }
                let slept = sleep_sliced(env, WAIT_POLL_MAX);
                waited_ms = waited_ms.saturating_add(slept);
                env.paused(slept);
                let (budget, policy) = env.stage_budget_fresh();
                let policy = policy.map_err(|e| anyhow::anyhow!(e.to_string()))?;
                bucket.lock().unwrap_or_else(|p| p.into_inner()).reconfigure(budget, policy);
            }
        }
    }
}

/// Settle one call against a STAGE bucket and surface the breach, once, if
/// this call's spend reached the budget (`warn`, or `wait` whose NEXT call
/// will hold).
pub fn settle_stage(
    bucket: &Mutex<crate::remote_budget::RemoteBudget>,
    reserved: u32,
    actual: u64,
    calls: u32,
    stage: &str,
    caller: &BudgetCaller<'_>,
    env: &dyn BudgetEnv,
) {
    let (breach, policy) = {
        let mut b = bucket.lock().unwrap_or_else(|p| p.into_inner());
        b.settle(reserved, actual, calls);
        (b.take_breach(), b.policy())
    };
    let Some(br) = breach else { return };
    let then = match policy {
        BudgetPolicy::Wait => "policy wait: its next hosted call waits",
        _ => "policy warn: continuing",
    };
    let message = format!(
        "darkmux: ⚠ stage `{stage}` has reached its stage budget: {} of {} tokens          (remote.max_tokens_per_execution); {then}",
        br.used, br.budget
    );
    env.say(&message);
    env.emit(record(
        darkmux_flow::Level::Warn,
        BUDGET_WARN_ACTION,
        caller,
        serde_json::json!({
            "scope": "stage", "stage": stage,
            "policy": darkmux_types::config_enum::ConfigEnum::token(policy),
            "level": BreachLevel::AtLimit, "metric": Metric::Tokens,
            "spent": br.used, "limit": br.budget, "message": message,
        }),
    ));
}

// ── Reading waits back (mission status) ─────────────────────────────────

/// A wait still in progress, read back from the flow log.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ActiveWait {
    /// `endpoint` or `stage`.
    pub scope: String,
    /// The endpoint id (endpoint scope) or the stage label (stage scope).
    pub subject: String,
    pub mission_id: Option<String>,
    pub session_id: Option<String>,
    /// When it resumes, when known (`YYYY-MM-DDTHH:MM:SSZ`).
    pub resume_at: Option<String>,
    /// Seconds from `now` to `resume_at`.
    pub resumes_in_secs: Option<u64>,
    pub message: String,
}

/// Every budget wait still open in `dir`'s last two day files at `now`: the
/// newest `budget.wait` per (scope, subject, session, pid) with no later
/// `budget.resume`, whose process is alive and whose resume time has not
/// passed. For `darkmux mission status`.
pub fn active_waits(dir: &Path, now: i64, alive: &dyn Fn(u32) -> bool) -> Vec<ActiveWait> {
    let mut latest: BTreeMap<(String, String, String, u64), (bool, serde_json::Value)> = BTreeMap::new();
    for day in [darkmux_flow::day_utc_at(now - 86_400), darkmux_flow::day_utc_at(now)] {
        let Ok(text) = std::fs::read_to_string(dir.join(format!("{day}.jsonl"))) else { continue };
        for line in text.lines() {
            if !line.contains("\"budget.") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let action = v.get("action").and_then(|a| a.as_str()).unwrap_or("");
            let waiting = match action {
                BUDGET_WAIT_ACTION => true,
                BUDGET_RESUME_ACTION => false,
                _ => continue,
            };
            let p = crate::usage::payload_of(&v);
            let scope = p.get("scope").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let subject = p
                .get("endpoint_id")
                .or_else(|| p.get("stage"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
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
            let p = crate::usage::payload_of(&v);
            let resume_at = p.get("resume_at").and_then(|x| x.as_str()).map(str::to_string);
            let resume_secs = resume_at.as_deref().and_then(crate::records_emitted::parse_ts_secs);
            if resume_secs.is_some_and(|r| r + 1 < now) {
                return None;
            }
            Some(ActiveWait {
                scope,
                subject,
                mission_id: v.get("mission_id").and_then(|x| x.as_str()).map(str::to_string),
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
mod tests;
