//! (#2902 step 5) Tests for `crate::budget`. Every clock here is injected
//! and frozen ([`FakeEnv::now`]); a "sleep" advances it. No test mixes a
//! fixed timestamp with the real clock.

use super::*;
use darkmux_types::{BudgetPolicy, EndpointSource, ModelEndpoint, UsageLimits, UsageWindow};
use darkmux_types::session_id::{RunId, SessionId};
use std::cell::{Cell, RefCell};

/// A caller in its own standalone test run: no mission to be stopped by.
pub(crate) fn solo_caller() -> BudgetCaller<'static> {
    caller_in(SessionId::adhoc(RunId::standalone("budget-test").unwrap(), "coder", "1"))
}

/// A task-seat caller in mission `mid`.
pub(crate) fn mission_caller(mid: &str) -> BudgetCaller<'static> {
    caller_in(SessionId::task(RunId::mission(mid).unwrap(), "probe"))
}

/// A caller under `session` (leaked: a test's caller lives for the test).
fn caller_in(session: SessionId) -> BudgetCaller<'static> {
    BudgetCaller { session: Box::leak(Box::new(session)), execution: Box::leak(Box::new(ExecutionId::mint())), role_id: None, model: None, phase_id: None, profiles_file: None }
}

pub(crate) const T0: i64 = 1_790_000_000; // a fixed epoch second, the frozen "now"
const DAY: i64 = 86_400;

pub(crate) fn budget(policy: BudgetPolicy, tokens: Option<u64>, calls: Option<u64>, warn_at: Option<f64>) -> EndpointBudget {
    EndpointBudget {
        endpoint_id: "azure".into(),
        policy,
        warn_at,
        window: darkmux_types::WindowBudget { period_secs: DAY as u64, tokens, calls },
        period: "1d".into(),
        max_wait: None,
    }
}

/// [`budget`] under `wait` with a `max_wait` of `secs`.
fn waiting_at_most(secs: u64) -> EndpointBudget {
    EndpointBudget { max_wait: Some((secs, format!("{}m", secs / 60))), ..budget(BudgetPolicy::Wait, Some(1_000), None, None) }
}

/// A named endpoint carrying `limits`.
fn named(limits: serde_json::Value) -> ModelEndpoint {
    let mut ep: ModelEndpoint =
        serde_json::from_value(serde_json::json!({ "url": "https://h.example/v1", "limits": limits })).unwrap();
    ep.source = EndpointSource::Named("azure".into());
    ep
}

/// The frozen-clock environment every budget test drives. `pub(crate)` so a
/// step kind's own test can stand it in for the live one
/// (`budget::with_test_env`) and see the gate fire on its path.
pub(crate) struct FakeEnv {
    now: Cell<i64>,
    records: RefCell<Vec<(i64, Spend)>>,
    slept_ms: Cell<u64>,
    paused_ms: Cell<u64>,
    /// Why the run was stopped, from this many ms slept on (a test's
    /// stand-in for an interrupt or a `mission abort`).
    stop: RefCell<Option<(u64, String)>>,
    /// Read stops from disk the way `LiveEnv` does (`run_stop_reason`).
    disk_stops: bool,
    /// What `reload` returns (`None` = keep the budget in hand).
    reload_to: RefCell<Option<Option<EndpointBudget>>>,
    /// The registry path each `reload` was asked for.
    reloaded_from: RefCell<Vec<Option<String>>>,
    emitted: RefCell<Vec<darkmux_flow::FlowRecord>>,
    said: RefCell<Vec<String>>,
    levels: RefCell<HashMap<String, Surfaced>>,
}

impl FakeEnv {
    pub(crate) fn new(records: Vec<(i64, u64)>) -> Self {
        FakeEnv {
            now: Cell::new(T0),
            records: RefCell::new(records.into_iter().map(|(t, n)| (t, Spend::full(n))).collect()),
            slept_ms: Cell::new(0),
            paused_ms: Cell::new(0),
            stop: RefCell::new(None),
            disk_stops: false,
            reload_to: RefCell::new(None),
            reloaded_from: RefCell::new(Vec::new()),
            emitted: RefCell::new(Vec::new()),
            said: RefCell::new(Vec::new()),
            levels: RefCell::new(HashMap::new()),
        }
    }
    /// Move the frozen clock to `t` (epoch seconds), so a record ages out.
    pub(crate) fn at(&self, t: i64) {
        self.now.set(t);
    }
    /// A window already over any small budget.
    pub(crate) fn full_window() -> Self {
        Self::new(vec![(T0 - 10, 1_000_000)])
    }
    /// Stopped from the start, for `reason`.
    pub(crate) fn stopped(self, reason: &str) -> Self {
        *self.stop.borrow_mut() = Some((0, reason.to_string()));
        self
    }
    /// Stopped once `ms` has been slept: the wait is announced first.
    pub(crate) fn stopped_after(self, ms: u64, reason: &str) -> Self {
        *self.stop.borrow_mut() = Some((ms, reason.to_string()));
        self
    }
    pub(crate) fn reading_disk_for_stops(mut self) -> Self {
        self.disk_stops = true;
        self
    }
    pub(crate) fn actions(&self) -> Vec<darkmux_flow::FlowAction> {
        self.emitted.borrow().iter().map(|r| r.action.clone()).collect()
    }
    pub(crate) fn payload(&self, action: darkmux_flow::FlowAction) -> serde_json::Value {
        self.emitted.borrow().iter().find(|r| r.action == action).map(|r| r.payload_json()).unwrap()
    }
    /// The first record emitted with `action`.
    pub(crate) fn record(&self, action: darkmux_flow::FlowAction) -> Option<darkmux_flow::FlowRecord> {
        self.emitted.borrow().iter().find(|r| r.action == action).cloned()
    }
}

impl BudgetEnv for FakeEnv {
    fn now(&self) -> i64 {
        self.now.get()
    }
    fn window(&self, b: &EndpointBudget, now: i64) -> WindowEntries {
        let from = now - b.window.period_secs as i64;
        self.records.borrow().iter().copied().filter(|(t, _)| *t > from && *t <= now).collect()
    }
    fn sleep(&self, d: Duration) {
        self.slept_ms.set(self.slept_ms.get() + d.as_millis() as u64);
        // The frozen clock moves only when the gate sleeps.
        self.now.set(T0 + (self.slept_ms.get() / 1000) as i64);
    }
    fn stop_reason(&self, caller: &BudgetCaller<'_>) -> Option<String> {
        if self.disk_stops {
            if let Some(r) = run_stop_reason(caller.session.mission_id(), caller.phase_id) {
                return Some(r);
            }
        }
        match &*self.stop.borrow() {
            Some((after, why)) if self.slept_ms.get() >= *after => Some(why.clone()),
            _ => None,
        }
    }
    fn reload(&self, _id: &str, profiles_file: Option<&str>) -> Option<Option<EndpointBudget>> {
        self.reloaded_from.borrow_mut().push(profiles_file.map(str::to_string));
        self.reload_to.borrow().clone()
    }
    fn emit(&self, rec: darkmux_flow::FlowRecord) {
        self.emitted.borrow_mut().push(rec);
    }
    fn say(&self, line: &str) {
        self.said.borrow_mut().push(line.to_string());
    }
    fn paused(&self, ms: u64) {
        self.paused_ms.set(self.paused_ms.get() + ms);
    }
    fn last_surfaced(&self, key: &str) -> Option<Surfaced> {
        self.levels.borrow().get(key).copied()
    }
    fn set_surfaced(&self, key: &str, level: Option<Surfaced>) {
        match level {
            Some(l) => {
                self.levels.borrow_mut().insert(key.into(), l);
            }
            None => {
                self.levels.borrow_mut().remove(key);
            }
        }
    }
}

// ── What counts: nothing unless the operator sets it ─────────────────────

/// No limits, an inline endpoint (no id to sum by), `off`, or no window:
/// nothing to enforce, so the gate returns before reading anything.
#[test]
fn nothing_is_enforced_unless_a_window_budget_is_set_and_not_off() {
    assert_eq!(EndpointBudget::of(&ModelEndpoint::reference("x")), Ok(None), "no limits");
    let mut inline = named(serde_json::json!({"window": {"period": "1d", "tokens": 10}}));
    inline.source = EndpointSource::Inline;
    let err = EndpointBudget::of(&inline).unwrap_err();
    assert!(err.contains("inline endpoint") && err.contains("by id"), "inline: no id to sum by, so a counting window is refused: {err}");
    let mut inline_off = named(serde_json::json!({"policy": "off", "window": {"period": "1d", "tokens": 10}}));
    inline_off.source = EndpointSource::Inline;
    assert_eq!(EndpointBudget::of(&inline_off), Ok(None), "an inline window that counts nothing is inert");
    let mut inline_cap = named(serde_json::json!({"tokens_per_dispatch": 10, "concurrent_calls": 2}));
    inline_cap.source = EndpointSource::Inline;
    assert_eq!(EndpointBudget::of(&inline_cap), Ok(None), "an inline endpoint's per-dispatch cap and concurrency need no id");
    assert_eq!(crate::dispatch_budget::DispatchBudget::for_endpoint(&inline_cap).unwrap().budget(), Some(10));
    let off = named(serde_json::json!({"policy": "off", "window": {"period": "1d", "tokens": 10}}));
    assert_eq!(EndpointBudget::of(&off), Ok(None), "policy off counts nothing");
    // (5th review MF2) `off` with a 0 is still off: the gate admits, never
    // refusing every hosted call over an inert number.
    let off_zero = named(serde_json::json!({"policy": "off", "window": {"period": "1d", "tokens": 0, "calls": 0}}));
    assert_eq!(EndpointBudget::of(&off_zero), Ok(None), "policy off + 0 counts nothing and refuses nothing");
    let warn_zero = named(serde_json::json!({"policy": "warn", "window": {"period": "1d", "tokens": 0}}));
    assert!(EndpointBudget::of(&warn_zero).unwrap_err().contains("0 is not a budget"), "warn + 0 is refused");
    let shipped = named(serde_json::json!({"policy": null, "warn_at": null, "window": {"period": null, "tokens": null, "calls": null}}));
    assert_eq!(EndpointBudget::of(&shipped), Ok(None), "the shipped all-null shape is no budget");
    let mut managed = named(serde_json::json!({"window": {"period": "1d", "tokens": 10}}));
    managed.url = None;
    managed.managed = Some(darkmux_types::ManagedBackend::Lmstudio.into());
    let b = EndpointBudget::of(&managed).unwrap().expect("(#3035) a managed endpoint's window is budgeted too");
    assert_eq!((b.endpoint_id.as_str(), b.policy), ("azure", BudgetPolicy::Warn));
    let per_dispatch_only = named(serde_json::json!({"tokens_per_dispatch": 5}));
    assert_eq!(EndpointBudget::of(&per_dispatch_only), Ok(None), "the per-dispatch cap is the dispatch bucket's, not the window gate's");
}

/// (#3035) `concurrent_calls` on a managed endpoint is an error at the gate
/// and at preflight's registry pass, naming the scheduler; on an unmanaged
/// one it is fine.
#[test]
fn concurrent_calls_on_a_managed_endpoint_is_refused_at_the_gate_and_at_preflight() {
    let mut managed = named(serde_json::json!({"concurrent_calls": 2, "window": {"period": "1d", "tokens": 10}}));
    managed.url = None;
    managed.managed = Some(darkmux_types::ManagedBackend::Lmstudio.into());
    let err = EndpointBudget::of(&managed).unwrap_err();
    assert!(err.contains("concurrent_calls") && err.contains("scheduler") && err.contains("azure"), "{err}");
    assert!(crate::dispatch_budget::DispatchBudget::for_endpoint(&managed).is_err(), "the dispatch cap reads limits the same way");
    let unmanaged = named(serde_json::json!({"concurrent_calls": 2, "window": {"period": "1d", "tokens": 10}}));
    assert!(EndpointBudget::of(&unmanaged).is_ok());
    let mut reg: darkmux_types::ProfileRegistry = serde_json::from_value(serde_json::json!({
        "profiles": {"p": {"models": [{"id": "m", "n_ctx": 1, "endpoint": "lms"}]}},
        "endpoints": {"lms": {"managed": "lmstudio", "limits": {"concurrent_calls": 2}}},
    }))
    .unwrap();
    reg.materialize_endpoints();
    let invalid = darkmux_types::config_enum::invalid_endpoint_limits(&reg);
    assert_eq!(invalid.len(), 1, "{invalid:?}");
    let line = invalid[0].to_string();
    assert!(line.contains("endpoints.lms.limits") && line.contains("scheduler"), "{line}");
}

/// A budget written without a policy is `warn`; an unregistered policy is
/// an error naming the raw value, never a fallback.
#[test]
fn an_absent_policy_is_warn_and_an_unknown_one_is_refused() {
    let set = named(serde_json::json!({"window": {"period": "1d", "tokens": 10}}));
    assert_eq!(EndpointBudget::of(&set).unwrap().unwrap().policy, BudgetPolicy::Warn);
    let wait = named(serde_json::json!({"policy": "wait", "warn_at": 0.8, "window": {"period": "2h", "calls": 3}}));
    let b = EndpointBudget::of(&wait).unwrap().unwrap();
    assert_eq!((b.policy, b.warn_at, b.window.period_secs, b.window.calls), (BudgetPolicy::Wait, Some(0.8), 7_200, Some(3)));
    for raw in ["stop", "WAIT", "enforce"] {
        let bad = named(serde_json::json!({"policy": raw, "window": {"period": "1d", "tokens": 10}}));
        let err = EndpointBudget::of(&bad).unwrap_err();
        assert!(err.contains(&format!("`{raw}`")) && err.contains("off, warn, wait"), "{err}");
    }
}

/// (review M2) The reviewer's five probes, committed. Each typo used to
/// make `limits` unreadable (or leave an unparseable period), which read as
/// NO budget: it passed preflight and ran unmetered. Now each is an error
/// at the gate (and refused at preflight: `invalid_endpoint_limits`).
#[test]
fn a_typo_in_limits_is_an_error_never_no_budget() {
    let probes = [
        (serde_json::json!({"warn_at": "80%", "window": {"period": "1d", "tokens": 10}}), "80%"),
        (serde_json::json!({"window": {"period": "1d", "tokens": "2M"}}), "2M"),
        (serde_json::json!({"window": {"period": "24H", "tokens": 10}}), "24H"),
        (serde_json::json!({"window": {"period": "1w", "tokens": 10}}), "1w"),
        (serde_json::json!({"policy": "wiat", "warn_at": "80%", "window": {"period": "1d", "tokens": 10}}), "80%"),
    ];
    for (limits, needle) in probes {
        let err = EndpointBudget::of(&named(limits.clone())).expect_err(&format!("{limits} must not be Ok"));
        assert!(err.contains(needle) && err.contains("azure"), "{limits}: {err}");
        // And preflight's registry pass names it by path.
        let mut reg: darkmux_types::ProfileRegistry = serde_json::from_value(serde_json::json!({
            "profiles": {"p": {"models": [{"id": "m", "endpoint": "azure"}]}},
            "endpoints": {"azure": {"url": "https://h.example/v1", "limits": limits}},
        }))
        .unwrap();
        reg.materialize_endpoints();
        let invalid = darkmux_types::config_enum::invalid_endpoint_limits(&reg);
        assert_eq!(invalid.len(), 1, "{limits}: {invalid:?}");
        let line = invalid[0].to_string();
        assert!(line.contains("endpoints.azure.limits") && line.contains("period"), "{line}");
    }
}

/// (review MF2) The reviewer's four misspelled-KEY probes, committed. Each
/// used to disarm the budget silently (no window, the default policy, a
/// dropped token budget). Each is now an unknown key in `profiles.json`,
/// which every dispatching preflight refuses (`darkmux_types::user_files`),
/// naming the key and the nearest valid one.
#[test]
fn a_misspelled_key_in_limits_is_an_unknown_key_naming_the_nearest() {
    let probes = [
        (serde_json::json!({"windw": {"period": "1d", "tokens": 10}, "policy": "wait"}), "limits.windw", "limits.window"),
        (serde_json::json!({"polcy": "wait", "window": {"period": "1d", "tokens": 10}}), "limits.polcy", "limits.policy"),
        (serde_json::json!({"policy": "wait", "window": {"period": "1d", "tokns": 10, "calls": 400}}), "limits.window.tokns", "limits.window.tokens"),
        (serde_json::json!({"policy": "wait", "window": {"tokns": 10}}), "limits.window.tokns", "limits.window.tokens"),
    ];
    for (limits, key, nearest) in probes {
        let doc = serde_json::json!({
            "profiles": {"p": {"models": [{"id": "m", "endpoint": "azure"}]}},
            "endpoints": {"azure": {"url": "https://h.example/v1", "limits": limits}},
        });
        let keys = darkmux_types::user_files::key_issues::<darkmux_types::ProfileRegistry>(&doc, &darkmux_types::user_files::no_retired);
        let msgs: Vec<String> = keys.iter().map(ToString::to_string).collect();
        assert_eq!(keys.len(), 1, "{limits}: {msgs:?}");
        assert!(
            msgs[0].contains(&format!("unknown key `endpoints.azure.{key}`: did you mean `endpoints.azure.{nearest}`?")),
            "{limits}: {msgs:?}"
        );
    }
    // A policy that governs nothing is named too.
    let err = EndpointBudget::of(&named(serde_json::json!({"policy": "wait"}))).unwrap_err();
    assert!(err.contains("governs nothing"), "{err}");
    assert!(EndpointBudget::of(&named(serde_json::json!({"policy": "off"}))).unwrap().is_none(), "`off` with no window is fine");
}

/// The shape checks `darkmux doctor` and the registry validation report.
#[test]
fn limits_validate_warn_at_and_policy() {
    let ok: UsageLimits = serde_json::from_value(serde_json::json!({"policy": "wait", "warn_at": 0.5, "window": {"period": "1d", "tokens": 1}})).unwrap();
    assert_eq!(ok.validate(), Ok(()));
    for f in [0.0, 1.0, 1.5, -0.1] {
        let l = UsageLimits { warn_at: Some(f), ..Default::default() };
        assert!(l.validate().unwrap_err().contains("warn_at"), "{f}");
    }
    let bad: UsageLimits = serde_json::from_value(serde_json::json!({"policy": "stop"})).unwrap();
    assert!(bad.validate().unwrap_err().contains("`stop`"));
    let shipped: UsageLimits = serde_json::from_value(serde_json::json!({"window": {"period": null, "tokens": null}})).unwrap();
    assert_eq!(shipped.validate(), Ok(()), "an all-null window is unset, not an error");
    let no_number = UsageLimits { window: Some(UsageWindow { period: Some("1d".into()), ..Default::default() }), ..Default::default() };
    assert!(no_number.validate().is_err(), "a period with no budget is still named");
}

// ── The pure decision ────────────────────────────────────────────────────

/// Every entry's spend known: the shape all but the unmetered tests use.
fn evaluate_known(b: &EndpointBudget, entries: &[(i64, u64)], now: i64) -> Verdict {
    let known: Vec<(i64, Spend)> = entries.iter().map(|(t, n)| (*t, Spend::full(*n))).collect();
    evaluate(b, &known, now)
}

/// A call whose full spend is unknown (no prompt count, or no usage at
/// all) is never read as small: its known halves still count toward the
/// budget (a lower bound), and the window is flagged as not fully metered.
/// The flag rides on the same warning; it never replaces the level.
#[test]
fn an_unmetered_call_counts_its_known_halves_and_is_flagged() {
    let entries = [(T0 - 10, Spend::full(100)), (T0 - 5, Spend::partial(0))];
    for policy in [BudgetPolicy::Warn, BudgetPolicy::Wait] {
        let b = budget(policy, Some(1_000), None, None);
        let Verdict::Warn(br) = evaluate(&b, &entries, T0) else { panic!("{policy:?}: not surfaced") };
        assert_eq!((br.level, br.metric, br.spent, br.limit, br.unmetered), (None, Metric::Tokens, 100, 1_000, 1));
    }
    let calls_only = budget(BudgetPolicy::Warn, None, Some(10), None);
    assert_eq!(evaluate(&calls_only, &entries, T0), Verdict::Proceed, "a calls budget counts the call either way");
}

/// (re-review N1, the probe) Five completion-only records of 400,000
/// tokens against a 100,000-token `wait` budget: the known halves alone
/// are 2,000,000, so the call WAITS. Throwing the halves away read the
/// window as 0 spent and let the call through.
#[test]
fn completion_only_spend_still_fills_a_wait_budget() {
    let entries: Vec<(i64, Spend)> = (1..=5).map(|i| (T0 - 100 + i, Spend::partial(400_000))).collect();
    let b = budget(BudgetPolicy::Wait, Some(100_000), None, None);
    let Verdict::Wait { breach, .. } = evaluate(&b, &entries, T0) else { panic!("an over-full window must wait") };
    assert_eq!((breach.level, breach.spent, breach.unmetered), (Some(BreachLevel::AtLimit), 2_000_000, 5));
}

/// The wait a partial window causes says its spend is a floor, and how many
/// calls it could not fully count.
#[test]
fn a_wait_on_a_partial_window_names_its_floor() {
    let env = FakeEnv::new(vec![]);
    for i in 1..=5 {
        env.records.borrow_mut().push((T0 - 100 + i, Spend::partial(400_000)));
    }
    admit_with(budget(BudgetPolicy::Wait, Some(100_000), None, None), &solo_caller(), &env).unwrap();
    let w = env.payload(darkmux_flow::FlowAction::BudgetWait);
    assert_eq!((w["spent"].as_u64(), w["unmetered_calls"].as_u64()), (Some(2_000_000), Some(5)), "{w}");
    assert!(w["message"].as_str().unwrap().contains("at least 2000000 of 100000"), "{w}");
}

/// A call count is always exact, so "not fully metered" applies only to a
/// token metric: a calls-only budget's early warning never says "at
/// least", carries no unmetered count, and is not repeated when an
/// unmetered call enters the window.
#[test]
fn a_calls_budget_is_never_unmetered() {
    let env = FakeEnv::new(vec![(T0 - 60, 10), (T0 - 50, 10)]);
    env.records.borrow_mut().push((T0 - 40, Spend::partial(0)));
    let b = budget(BudgetPolicy::Warn, None, Some(5), Some(0.5));
    let caller = solo_caller();
    admit_with(b.clone(), &caller, &env).unwrap();
    assert_eq!(env.actions().len(), 1, "3 of 5 calls: early");
    let p = env.payload(darkmux_flow::FlowAction::BudgetWarn);
    assert_eq!((p["metric"].as_str(), p["unmetered_calls"].as_u64()), (Some("calls"), Some(0)), "{p}");
    assert!(!p["message"].as_str().unwrap().contains("at least"), "{p}");
    env.records.borrow_mut().push((T0 - 30, Spend::partial(0)));
    admit_with(b, &caller, &env).unwrap();
    assert_eq!(env.actions().len(), 1, "another unmetered call is no news to a calls budget");
}

/// (re-review N4) The level (over the known spend) and the unmetered flag
/// are independent: an early warning that arrives after an unmetered-only
/// one is news and is said, and a newly unmetered window at the same level
/// is said too.
#[test]
fn an_early_warning_after_an_unmetered_one_is_still_said() {
    let env = FakeEnv::new(vec![]);
    env.records.borrow_mut().push((T0 - 60, Spend::partial(10)));
    let b = budget(BudgetPolicy::Warn, Some(1_000), None, Some(0.5));
    let caller = solo_caller();
    admit_with(b.clone(), &caller, &env).unwrap();
    assert_eq!(env.actions().len(), 1, "unmetered, under every threshold: said once");
    admit_with(b.clone(), &caller, &env).unwrap();
    assert_eq!(env.actions().len(), 1, "nothing new: not repeated");
    env.records.borrow_mut().push((T0 - 30, Spend::full(600)));
    admit_with(b.clone(), &caller, &env).unwrap();
    assert_eq!(env.actions().len(), 2, "crossing warn_at is news even after an unmetered warning");
    let early = env.emitted.borrow()[1].payload_json();
    assert_eq!((early["level"].as_str(), early["unmetered_calls"].as_u64()), (Some("early"), Some(1)));
    // The other direction: a metered early warning, then an unmetered call.
    let env = FakeEnv::new(vec![(T0 - 60, 600)]);
    admit_with(b.clone(), &caller, &env).unwrap();
    env.records.borrow_mut().push((T0 - 30, Spend::partial(0)));
    admit_with(b, &caller, &env).unwrap();
    assert_eq!(env.actions().len(), 2, "a window that became unmetered is news at the same level");
}

/// The window's reading of a usage record: a completion-only record's
/// spend is unknown, not its completion count.
#[test]
fn a_usage_record_without_a_prompt_count_has_an_unknown_spend() {
    let rec = |payload: serde_json::Value| {
        serde_json::json!({
            "ts": "2026-09-28T00:00:00Z", "category": "telemetry", "source": "tokens",
            "action": "telemetry.tokens", "payload": payload,
        })
        .to_string()
    };
    let partial = rec(serde_json::json!({"endpoint_id": "azure", "token_source": "provider", "completion_tokens": 12}));
    assert_eq!(parse_entry(partial.as_bytes()).map(|e| e.2), Some(Spend::partial(12)), "the known half is kept as a lower bound");
    let absent = rec(serde_json::json!({"endpoint_id": "azure", "token_source": "absent"}));
    assert_eq!(parse_entry(absent.as_bytes()).map(|e| e.2), Some(Spend::partial(0)), "no usage at all is unknown too");
    let whole = rec(serde_json::json!({"endpoint_id": "azure", "prompt_tokens": 90, "completion_tokens": 12}));
    assert_eq!(parse_entry(whole.as_bytes()).map(|e| e.2), Some(Spend::full(102)));
}

#[test]
fn under_budget_proceeds_at_budget_warns_or_waits() {
    let warn = budget(BudgetPolicy::Warn, Some(1_000), None, None);
    assert_eq!(evaluate_known(&warn, &[(T0 - 10, 999)], T0), Verdict::Proceed);
    let at = [(T0 - 10, 600), (T0 - 5, 400)];
    let Verdict::Warn(br) = evaluate_known(&warn, &at, T0) else { panic!() };
    assert_eq!((br.level, br.metric, br.spent, br.limit), (Some(BreachLevel::AtLimit), Metric::Tokens, 1_000, 1_000));
    let wait = budget(BudgetPolicy::Wait, Some(1_000), None, None);
    // Room returns when the 600 at T0-10 leaves the window: T0-10 + 1d.
    assert_eq!(
        evaluate_known(&wait, &at, T0),
        Verdict::Wait { breach: br, resume_at: Some(T0 - 10 + DAY) }
    );
}

/// Rolling, no calendar reset: a record leaves the window at exactly
/// `t + period`, whatever day it is.
#[test]
fn the_window_rolls_over_the_last_period_from_now() {
    let b = budget(BudgetPolicy::Warn, Some(100), None, None);
    let old = [(T0 - DAY, 100)];
    assert_eq!(evaluate_known(&b, &old, T0), Verdict::Proceed, "t + period == now: out");
    let inside = [(T0 - DAY + 1, 100)];
    assert!(matches!(evaluate_known(&b, &inside, T0), Verdict::Warn(_)), "t + period > now: in");
    let future = [(T0 + 5, 100)];
    assert_eq!(evaluate_known(&b, &future, T0), Verdict::Proceed, "a record stamped ahead of now is not counted yet");
}

#[test]
fn a_calls_budget_and_the_resume_time_for_calls() {
    let b = budget(BudgetPolicy::Wait, None, Some(3), None);
    let entries = [(T0 - 300, 1), (T0 - 200, 1), (T0 - 100, 1)];
    let Verdict::Wait { breach, resume_at } = evaluate_known(&b, &entries, T0) else { panic!() };
    assert_eq!((breach.metric, breach.spent, breach.limit), (Metric::Calls, 3, 3));
    assert_eq!(resume_at, Some(T0 - 300 + DAY), "one call leaving makes room for one");
    assert_eq!(evaluate_known(&b, &entries[..2], T0), Verdict::Proceed);
}

/// Both budgets breached: room returns when BOTH are back under.
#[test]
fn two_breached_budgets_resume_at_the_later_time() {
    let b = budget(BudgetPolicy::Wait, Some(100), Some(2), None);
    let entries = [(T0 - 500, 90), (T0 - 400, 5), (T0 - 300, 5)];
    // Tokens 100 >= 100 → room when the 90 leaves (T0-500+DAY).
    // Calls 3 >= 2 → room when two calls leave (T0-400+DAY). Later wins.
    let Verdict::Wait { resume_at, .. } = evaluate_known(&b, &entries, T0) else { panic!() };
    assert_eq!(resume_at, Some(T0 - 400 + DAY));
}

/// (Defensive: `limits.validate` refuses a zero window, so only a budget
/// built directly reaches this.) No resume time exists for 0.
#[test]
fn a_zero_budget_waits_with_no_resume_time() {
    let b = budget(BudgetPolicy::Wait, Some(0), None, None);
    assert!(matches!(evaluate_known(&b, &[], T0), Verdict::Wait { resume_at: None, .. }));
}

/// `warn_at` warns once earlier; unset, only the at-limit warning fires.
#[test]
fn warn_at_warns_early_and_is_never_guessed() {
    let with = budget(BudgetPolicy::Warn, Some(1_000), None, Some(0.8));
    let Verdict::Warn(br) = evaluate_known(&with, &[(T0 - 1, 800)], T0) else { panic!() };
    assert_eq!(br.level, Some(BreachLevel::Early));
    assert_eq!(evaluate_known(&with, &[(T0 - 1, 799)], T0), Verdict::Proceed);
    let without = budget(BudgetPolicy::Warn, Some(1_000), None, None);
    assert_eq!(evaluate_known(&without, &[(T0 - 1, 999)], T0), Verdict::Proceed, "no threshold picked by darkmux");
    let wait_early = budget(BudgetPolicy::Wait, Some(1_000), None, Some(0.8));
    assert!(matches!(evaluate_known(&wait_early, &[(T0 - 1, 900)], T0), Verdict::Warn(_)), "wait warns early, waits only at the limit");
}

/// (3rd review #2) The warning names the policy it runs under: an early
/// warning under `wait` says the calls will wait at the limit, never
/// "policy warn".
#[test]
fn the_warning_names_the_policy_it_runs_under() {
    let env = FakeEnv::new(vec![(T0 - 60, 900)]);
    admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, Some(0.8)), &solo_caller(), &env).unwrap();
    let said = env.said.borrow()[0].clone();
    assert!(said.contains("policy wait: continuing; calls wait once the budget is reached"), "{said}");
    assert!(!said.contains("policy warn"), "{said}");
    assert_eq!(env.payload(darkmux_flow::FlowAction::BudgetWarn)["policy"], "wait");
    let env = FakeEnv::new(vec![(T0 - 60, 900)]);
    admit_with(budget(BudgetPolicy::Warn, Some(1_000), None, Some(0.8)), &solo_caller(), &env).unwrap();
    assert!(env.said.borrow()[0].contains("policy warn: continuing"), "{:?}", env.said.borrow());
}

#[test]
fn off_never_acts_even_if_evaluated() {
    let b = budget(BudgetPolicy::Off, Some(0), Some(0), None);
    assert_eq!(evaluate_known(&b, &[(T0, 99)], T0), Verdict::Proceed);
}

// ── The gate ─────────────────────────────────────────────────────────────

/// `warn` never stops and never sleeps; the breach is surfaced once, and
/// again only after the spend dropped back below and rose again.
#[test]
fn warn_surfaces_the_breach_once_and_never_holds_the_call() {
    let env = FakeEnv::new(vec![(T0 - 60, 2_000)]);
    let b = budget(BudgetPolicy::Warn, Some(1_000), None, None);
    let caller = mission_caller("m1");
    admit_with(b.clone(), &caller, &env).unwrap();
    admit_with(b.clone(), &caller, &env).unwrap();
    assert_eq!(env.slept_ms.get(), 0, "warn never waits");
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWarn], "surfaced once");
    let p = env.payload(darkmux_flow::FlowAction::BudgetWarn);
    assert_eq!((p["spent"].as_u64(), p["limit"].as_u64(), p["level"].as_str()), (Some(2_000), Some(1_000), Some("at_limit")));
    assert!(env.said.borrow()[0].contains("continuing"), "{:?}", env.said.borrow());
    let rec = env.emitted.borrow()[0].clone();
    assert_eq!((rec.session_id.as_deref(), rec.mission_id.as_deref()), (Some("m1.task.probe"), Some("m1")), "lands on the run");
    // Spend leaves the window, then returns: a second crossing warns again.
    env.records.borrow_mut().clear();
    admit_with(b.clone(), &caller, &env).unwrap();
    env.records.borrow_mut().push((T0, Spend::full(5_000)));
    admit_with(b, &caller, &env).unwrap();
    assert_eq!(env.actions().len(), 2);
}

/// A budget record lands on its caller's session, and so on its run: the
/// wait and the stop a run's abort ends carry that run's session and
/// mission; a caller in a standalone run carries no mission.
#[test]
fn budget_records_land_on_the_callers_run() {
    let env = FakeEnv::full_window().stopped_after(1, "mission `m-a` is aborted");
    let _ = admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, None), &mission_caller("m-a"), &env);
    let got: Vec<(Option<String>, Option<String>)> =
        env.emitted.borrow().iter().map(|r| (r.session_id.clone(), r.mission_id.clone())).collect();
    let on_a = (Some("m-a.task.probe".to_string()), Some("m-a".to_string()));
    assert_eq!(got, vec![on_a.clone(), on_a], "wait and stop, both on mission A's run");
    let env = FakeEnv::new(vec![(T0 - 60, 2_000)]);
    admit_with(budget(BudgetPolicy::Warn, Some(1_000), None, None), &solo_caller(), &env).unwrap();
    let rec = env.emitted.borrow()[0].clone();
    assert_eq!(
        (rec.session_id.as_deref(), rec.mission_id.as_deref()),
        (Some("budget-test.solo.adhoc.coder.1"), None),
        "a standalone run: no mission"
    );
}

/// Every budget record is about the ONE execution whose call was gated:
/// the wait and the stop its run's abort ends both name the caller's
/// execution, and no other.
#[test]
fn budget_records_name_the_gated_calls_execution() {
    let env = FakeEnv::full_window().stopped_after(1, "mission `m-a` is aborted");
    let caller = mission_caller("m-a");
    let _ = admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, None), &caller, &env);
    let got: Vec<Option<darkmux_types::execution_id::ExecutionId>> = env.emitted.borrow().iter().map(|r| r.execution_id.clone()).collect();
    assert_eq!(got, vec![Some(caller.execution.clone()), Some(caller.execution.clone())], "wait and stop, both of the caller's execution");
}

/// `wait` holds the call until the rolling window has room, says how
/// long, records the pause (the run's time limits skip it), and resumes.
#[test]
fn wait_holds_until_the_window_has_room_then_resumes() {
    // 1000 of 1000 spent; the only record leaves at T0 - 60 + DAY + 1.
    let env = FakeEnv::new(vec![(T0 - DAY + 90, 1_000)]);
    let b = budget(BudgetPolicy::Wait, Some(1_000), None, None);
    admit_with(b, &solo_caller(), &env).unwrap();
    // Room at T0 + 90 (+1 s, the leave-at-exactly-t+period rule).
    assert!(env.now.get() >= T0 + 90 && env.now.get() <= T0 + 91, "{}", env.now.get());
    assert_eq!(env.paused_ms.get(), env.slept_ms.get(), "every waited ms is recorded as pause");
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWait, darkmux_flow::FlowAction::BudgetResume]);
    let wait = env.payload(darkmux_flow::FlowAction::BudgetWait);
    assert_eq!(wait["wait_ms"].as_i64(), Some(90_000));
    assert_eq!(wait["resume_at_ms"].as_i64(), Some((T0 + 90) * 1_000));
    assert!(wait.get("wait_seconds").is_none() && wait.get("resume_at").is_none(), "no second spelling: {wait}");
    assert!(env.said.borrow()[0].contains("resuming in about 1m 30s"), "{:?}", env.said.borrow());
    assert!(env.payload(darkmux_flow::FlowAction::BudgetResume)["waited_ms"].as_u64().unwrap() >= 90_000);
}

/// A stopped run (an interrupt, or a `mission abort`) ends a wait before
/// anything is sent; the gate says why.
#[test]
fn a_stopped_run_ends_a_wait_and_nothing_is_sent() {
    let env = FakeEnv::new(vec![(T0 - 10, 1_000)]).stopped_after(2_000, "mission `m` is aborted");
    let b = budget(BudgetPolicy::Wait, Some(1_000), None, None);
    let err = admit_with(b, &solo_caller(), &env).unwrap_err().to_string();
    assert!(err.contains("mission `m` is aborted") && err.contains("nothing was sent"), "{err}");
    assert!(env.slept_ms.get() <= 2_500, "stopped within one slice: {}", env.slept_ms.get());
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWait, darkmux_flow::FlowAction::BudgetStop], "an announced wait records its stop");
    let stop = env.payload(darkmux_flow::FlowAction::BudgetStop);
    assert_eq!((stop["endpoint_id"].as_str(), stop["reason"].as_str()), (Some("azure"), Some("mission `m` is aborted")));
}

/// (review M1) The stop is read from DISK, where `darkmux mission abort`
/// (another process) writes it: an aborted or finalized mission, or an
/// abandoned phase, stops the wait; an active one does not.
#[test]
#[serial_test::serial]
fn an_aborted_mission_on_disk_stops_its_waiter_without_sending() {
    let crew = tempfile::tempdir().unwrap();
    let prev = std::env::var("DARKMUX_HOME").ok();
    unsafe { std::env::set_var("DARKMUX_HOME", crew.path()) };
    let write = |path: std::path::PathBuf, status: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!(r#"{{"status":"{status}"}}"#)).unwrap();
    };
    write(crate::lifecycle::mission_path("m1"), "active");
    write(crate::lifecycle::phase_path("m1", "p1"), "running");
    let active = run_stop_reason(Some("m1"), Some("p1"));
    write(crate::lifecycle::phase_path("m1", "p1"), "abandoned");
    let abandoned = run_stop_reason(Some("m1"), Some("p1"));
    write(crate::lifecycle::mission_path("m1"), "aborted");
    let aborted = run_stop_reason(Some("m1"), None);
    // The waiter itself, end to end on the real disk read.
    let env = FakeEnv::new(vec![(T0 - 10, 1_000)]).reading_disk_for_stops();
    let caller = mission_caller("m1");
    let res = admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, None), &caller, &env);
    let live = LiveEnv.stop_reason(&caller);
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DARKMUX_HOME", v),
            None => std::env::remove_var("DARKMUX_HOME"),
        }
    }
    assert_eq!(active, None);
    assert_eq!(abandoned.as_deref(), Some("phase `p1` of mission `m1` is abandoned"));
    assert_eq!(aborted.as_deref(), Some("mission `m1` is aborted"));
    let err = res.unwrap_err().to_string();
    assert!(err.contains("mission `m1` is aborted") && err.contains("nothing was sent"), "{err}");
    assert_eq!(env.slept_ms.get(), 0, "already aborted: it never sleeps");
    assert!(env.actions().is_empty(), "no wait was announced, so no stop is recorded: {:?}", env.actions());
    assert_eq!(live.as_deref(), Some("mission `m1` is aborted"), "LiveEnv reads the same disk");
}

/// (review C-b, the reviewer's probe) The window frees at +12 s and the run
/// is stopped at +11.8 s, in the wait's last slice: the call is NOT sent.
#[test]
fn an_abort_in_the_last_slice_of_a_wait_still_sends_nothing() {
    let env = FakeEnv::new(vec![(T0 - DAY + 12, 1_000)]).stopped_after(11_800, "mission `m` is aborted");
    let err = admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, None), &solo_caller(), &env)
        .unwrap_err()
        .to_string();
    assert!(err.contains("mission `m` is aborted") && err.contains("nothing was sent"), "{err}");
    assert!(!env.actions().contains(&darkmux_flow::FlowAction::BudgetResume), "{:?}", env.actions());
}

/// (review C-i) An endpoint removed from the registry while a call waits
/// releases the wait (the operator took the budget away).
#[test]
fn an_endpoint_removed_while_waiting_releases_it() {
    let env = FakeEnv::new(vec![(T0 - 10, 1_000)]);
    *env.reload_to.borrow_mut() = Some(None);
    admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, None), &solo_caller(), &env).unwrap();
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWait, darkmux_flow::FlowAction::BudgetResume]);
}

/// (3rd review #3) The reload release point re-checks the stop: an abort
/// and a budget removal landing in the same slice send nothing, and the
/// ended wait is recorded as `budget.stop`, never `budget.resume`.
#[test]
fn an_abort_and_a_removal_in_one_slice_still_send_nothing() {
    let env = FakeEnv::full_window().stopped_after(1, "mission `m` is aborted");
    *env.reload_to.borrow_mut() = Some(None);
    let err = admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, None), &solo_caller(), &env)
        .unwrap_err()
        .to_string();
    assert!(err.contains("mission `m` is aborted") && err.contains("nothing was sent"), "{err}");
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWait, darkmux_flow::FlowAction::BudgetStop]);
    assert_eq!(env.payload(darkmux_flow::FlowAction::BudgetStop)["reason"], "mission `m` is aborted");
}

/// (review C-i, C4) `LiveEnv::reload` reads the command's own registry: an
/// endpoint removed from it releases (`Some(None)`), a present one reloads,
/// and a registry that cannot be read keeps the budget in hand (`None`).
#[test]
fn live_reload_releases_a_removed_endpoint_and_keeps_on_an_unreadable_registry() {
    let dir = tempfile::tempdir().unwrap();
    let pf = dir.path().join("profiles.json");
    std::fs::write(
        &pf,
        r#"{"profiles":{"p":{"models":[{"id":"m","n_ctx":1}]}},
            "endpoints":{"azure":{"url":"https://h.example/v1","limits":{"policy":"wait","window":{"period":"1d","tokens":5}}}}}"#,
    )
    .unwrap();
    let path = pf.to_str().unwrap();
    let present = LiveEnv.reload("azure", Some(path));
    assert!(matches!(present, Some(Some(ref b)) if b.window.tokens == Some(5)), "{present:?}");
    assert_eq!(LiveEnv.reload("gone", Some(path)), Some(None), "removed: released");
    assert_eq!(LiveEnv.reload("azure", Some("/no/such/profiles.json")), None, "unreadable: kept");
}

/// (5th review C5) An edit the budget cannot take (a 0 written in while a
/// call waits) keeps the budget in hand, and says so ONCE, naming the
/// reason, instead of being dropped without a word.
#[test]
fn a_refused_edit_during_a_wait_keeps_the_budget_and_says_why_once() {
    let dir = tempfile::tempdir().unwrap();
    let pf = dir.path().join("profiles.json");
    std::fs::write(
        &pf,
        r#"{"profiles":{"p":{"models":[{"id":"m","n_ctx":1}]}},
            "endpoints":{"refused-edit":{"url":"https://h.example/v1","limits":{"policy":"wait","window":{"period":"1d","tokens":0}}}}}"#,
    )
    .unwrap();
    let loaded = darkmux_profiles::profiles::load_registry_quiet(Some(pf.to_str().unwrap())).unwrap();
    let err = reloaded_budget(&loaded.registry, "refused-edit").unwrap_err();
    assert!(err.contains("0 is not a budget"), "{err}");
    let line = refused_edit_line("refused-edit", &err);
    assert!(
        line.contains("endpoint `refused-edit`") && line.contains("0 is not a budget") && line.contains("keeps the budget it had"),
        "{line}"
    );
    assert!(first_refusal(&line), "said the first time");
    assert!(!first_refusal(&line), "and only once");
    assert_eq!(LiveEnv.reload("refused-edit", Some(pf.to_str().unwrap())), None, "kept, not released");
}

/// A budget switched off (or raised) while waiting releases the wait.
#[test]
fn a_budget_switched_off_while_waiting_releases_it() {
    let env = FakeEnv::new(vec![(T0 - 10, 1_000)]);
    *env.reload_to.borrow_mut() = Some(None);
    let b = budget(BudgetPolicy::Wait, Some(1_000), None, None);
    admit_with(b, &solo_caller(), &env).unwrap();
    assert!(env.slept_ms.get() <= 30_000, "released at the first re-read: {}", env.slept_ms.get());
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWait, darkmux_flow::FlowAction::BudgetResume]);
}

/// A zero budget cannot come from a registry (`limits.validate` refuses
/// it: 0 is not a budget); built directly, the gate still stays sound: it
/// waits with no resume time and polls until the budget is raised.
#[test]
fn a_zero_budget_built_directly_waits_until_raised() {
    let env = FakeEnv::new(vec![]);
    *env.reload_to.borrow_mut() = Some(Some(budget(BudgetPolicy::Wait, Some(5), None, None)));
    admit_with(budget(BudgetPolicy::Wait, Some(0), None, None), &solo_caller(), &env).unwrap();
    assert!(env.payload(darkmux_flow::FlowAction::BudgetWait)["resume_at_ms"].is_null());
    let said = env.said.borrow()[0].clone();
    assert!(said.contains("until its window has room") && !said.contains("budget is 0"), "{said}");
}

// ── The per-dispatch cap ─────────────────────────────────────────────────

/// (#3035) Under `warn` the cap never holds a call and warns once on the
/// crossing; the record's `step` field names the dispatch.
#[test]
fn a_dispatch_under_warn_never_holds_and_warns_once_on_crossing() {
    let env = FakeEnv::new(vec![]);
    let bucket = Mutex::new(crate::dispatch_budget::DispatchBudget::new(Some(1_000), BudgetPolicy::Warn));
    for _ in 0..3 {
        settle_dispatch(&bucket, 600, "probe", &solo_caller(), &env);
    }
    assert_eq!(env.slept_ms.get(), 0);
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWarn], "one warning, at the crossing");
    let p = env.payload(darkmux_flow::FlowAction::BudgetWarn);
    assert_eq!((p["scope"].as_str(), p["dispatch"].as_str()), (Some("dispatch"), Some("probe")));
    let said = env.said.borrow()[0].clone();
    assert!(said.contains("limits.tokens_per_dispatch") && said.contains("1200 of 1000"), "{said}");
}

/// (#3035) A `wait` policy applies to the window, so the per-dispatch cap
/// beside it warns and never holds.
#[test]
fn a_dispatch_cap_under_wait_warns_and_never_holds() {
    let env = FakeEnv::new(vec![]);
    let bucket = Mutex::new(crate::dispatch_budget::DispatchBudget::new(Some(100), BudgetPolicy::Wait));
    settle_dispatch(&bucket, 700, "d", &solo_caller(), &env);
    assert_eq!(env.slept_ms.get(), 0, "nothing waited");
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWarn]);
}

// ── The in-run pacer ─────────────────────────────────────────────────────

fn pace(dir: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(crate::pace_file::path(dir)).unwrap()).unwrap()
}

/// A full window pauses the runtime through the pace file (reason
/// `budget`), the pause is recorded, and it releases once the window has
/// room; another governor's pause is never overwritten.
#[test]
fn the_pacer_pauses_through_the_pace_file_and_releases() {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(vec![(T0 - DAY + 10, 1_000)]);
    let mut p = BudgetPacer::new(budget(BudgetPolicy::Wait, Some(1_000), None, None), None);
    let c = solo_caller();
    let free = OtherPacing::default();
    let held = OtherPacing { pausing: true, duty_cycle: None };
    let ev = p.on_tick(0, dir.path(), &free, &c, &env);
    assert!(matches!(ev, Some(PacerEvent::Paused { .. })), "{ev:?}");
    assert_eq!(pace(dir.path())["pause"], true);
    assert_eq!(pace(dir.path())["reason"], PACE_REASON);
    // Another governor pauses: the pacer writes nothing over it.
    crate::pace_file::write(dir.path(), true, "thermal", "serious");
    assert_eq!(p.on_tick(2_000, dir.path(), &held, &c, &env), None);
    assert_eq!(pace(dir.path())["reason"], "thermal");
    // Thermal lets go: the pacer re-asserts its own hold.
    assert_eq!(p.on_tick(2_000, dir.path(), &free, &c, &env), None);
    assert_eq!(pace(dir.path())["reason"], PACE_REASON);
    assert_eq!(env.paused_ms.get(), 4_000, "time held counts as pause");
    // The record leaves the window.
    env.now.set(T0 + 11);
    let ev = p.on_tick(2_000, dir.path(), &free, &c, &env);
    assert!(matches!(ev, Some(PacerEvent::Resumed { .. })), "{ev:?}");
    assert_eq!(pace(dir.path())["pause"], false);
    assert!(!p.is_pausing());
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWait, darkmux_flow::FlowAction::BudgetResume]);
}

/// (review C6) Releasing during a thermal duty cycle writes the duty
/// cycle's instruction back (its `turn_delay_ms`), never a bare
/// `pause: false` that would drop it.
#[test]
fn the_pacer_release_keeps_a_thermal_duty_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(vec![(T0 - DAY + 10, 1_000)]);
    let mut p = BudgetPacer::new(budget(BudgetPolicy::Wait, Some(1_000), None, None), None);
    let c = solo_caller();
    let duty = OtherPacing { pausing: false, duty_cycle: Some((15_000, "fair".into())) };
    assert!(matches!(p.on_tick(0, dir.path(), &duty, &c, &env), Some(PacerEvent::Paused { .. })));
    env.now.set(T0 + 11);
    assert!(matches!(p.on_tick(2_000, dir.path(), &duty, &c, &env), Some(PacerEvent::Resumed { .. })));
    let v = pace(dir.path());
    assert_eq!((v["pause"].as_bool(), v["turn_delay_ms"].as_u64()), (Some(false), Some(15_000)), "{v}");
    assert_eq!(v["reason"], "thermal-duty-cycle");
}

/// (review MF1, the reviewer's probe) A run stopped while the pacer holds
/// it is never released: no `pause: false`, no `budget.resume`; one
/// `Stopped` and a `budget.stop` record, and it keeps holding after.
#[test]
fn the_pacer_never_releases_a_stopped_run() {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(vec![(T0 - DAY + 10, 1_000)]);
    let mut p = BudgetPacer::new(budget(BudgetPolicy::Wait, Some(1_000), None, None), None);
    let c = mission_caller("m1");
    let free = OtherPacing::default();
    assert!(matches!(p.on_tick(0, dir.path(), &free, &c, &env), Some(PacerEvent::Paused { .. })));
    *env.stop.borrow_mut() = Some((0, "mission `m1` is aborted".into()));
    // The window frees: an unstopped pacer would release here.
    env.now.set(T0 + 11);
    let ev = p.on_tick(2_000, dir.path(), &free, &c, &env);
    assert_eq!(ev, Some(PacerEvent::Stopped { reason: "mission `m1` is aborted".into() }));
    assert_eq!(pace(dir.path())["pause"], true, "still held");
    assert_eq!(p.on_tick(2_000, dir.path(), &free, &c, &env), None, "reported once");
    assert_eq!(pace(dir.path())["pause"], true, "never released");
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWait, darkmux_flow::FlowAction::BudgetStop]);
}

/// (review MF1) A stopped run whose window is full is never paused into a
/// wait that could later release: it is held and stopped at once.
#[test]
fn a_stopped_run_with_a_full_window_is_held_and_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(vec![(T0 - 10, 1_000)]).stopped("the run was interrupted");
    let mut p = BudgetPacer::new(budget(BudgetPolicy::Wait, Some(1_000), None, None), None);
    let ev = p.on_tick(0, dir.path(), &OtherPacing::default(), &solo_caller(), &env);
    assert!(matches!(ev, Some(PacerEvent::Stopped { .. })), "{ev:?}");
    assert_eq!(pace(dir.path())["pause"], true);
    // (5th review C6) No wait was announced, so no `budget.stop` either
    // (the gate's rule): a stop record always follows its wait. The CLI
    // still says why the run ended.
    assert!(env.actions().is_empty(), "{:?}", env.actions());
    assert!(env.said.borrow().iter().any(|l| l.contains("the run was interrupted")), "{:?}", env.said.borrow());
}

/// Under `warn` the pacer never touches the pace file.
#[test]
fn the_pacer_under_warn_never_pauses() {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(vec![(T0 - 10, 5_000)]);
    let mut p = BudgetPacer::new(budget(BudgetPolicy::Warn, Some(1_000), None, None), None);
    assert_eq!(p.on_tick(0, dir.path(), &OtherPacing::default(), &solo_caller(), &env), None);
    assert!(!crate::pace_file::path(dir.path()).exists());
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWarn]);
}

/// (review C4) A waiting pacer re-reads the endpoint from the command's own
/// registry file, not the default search.
#[test]
fn the_pacer_reloads_from_the_commands_registry() {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(vec![(T0 - DAY + 100, 1_000)]);
    let mut p = BudgetPacer::new(budget(BudgetPolicy::Wait, Some(1_000), None, None), Some("/x/profiles.json".into()));
    let c = solo_caller();
    p.on_tick(0, dir.path(), &OtherPacing::default(), &c, &env);
    p.on_tick(PACER_RELOAD_MS, dir.path(), &OtherPacing::default(), &c, &env);
    assert_eq!(*env.reloaded_from.borrow(), vec![Some("/x/profiles.json".to_string())]);
}

// ── The ledger ───────────────────────────────────────────────────────────

fn usage_line(ts: i64, endpoint_id: Option<&str>, total: u64) -> String {
    let mut payload = serde_json::json!({ "call_kind": "single_shot", "purpose": "work", "total_tokens": total });
    if let Some(id) = endpoint_id {
        payload["endpoint_id"] = serde_json::json!(id);
    }
    serde_json::json!({
        "ts": darkmux_flow::ts_utc_at(ts), "category": "telemetry", "source": "tokens",
        "action": "telemetry.tokens", "payload": payload,
    })
    .to_string()
        + "\n"
}

fn append(dir: &Path, day_of: i64, text: &str) {
    use std::io::Write;
    let path = dir.join(format!("{}.jsonl", darkmux_flow::day_utc_at(day_of)));
    std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
}

/// The window sums only this endpoint's usage records inside the rolling
/// window, across the day files it touches, both purposes included, and
/// never a record with no `endpoint_id`.
#[test]
fn the_ledger_sums_this_endpoints_records_in_the_window() {
    let dir = tempfile::tempdir().unwrap();
    append(dir.path(), T0 - DAY, &usage_line(T0 - DAY + 100, Some("azure"), 10));
    append(dir.path(), T0 - DAY, &usage_line(T0 - DAY - 5, Some("azure"), 999)); // outside
    append(dir.path(), T0, &usage_line(T0 - 50, Some("azure"), 20));
    append(dir.path(), T0, &usage_line(T0 - 40, Some("other"), 500));
    append(dir.path(), T0, &usage_line(T0 - 30, None, 700));
    let utility = r#"{"ts":"X","category":"telemetry","source":"tokens","action":"telemetry.tokens","payload":{"purpose":"utility","endpoint_id":"azure","total_tokens":3}}"#
        .replace("X", &darkmux_flow::ts_utc_at(T0 - 20));
    append(dir.path(), T0, &(utility + "\n"));
    append(dir.path(), T0, "{\"not json\n");
    let mut l = Ledger::new(dir.path());
    let w = l.window("azure", T0, DAY as u64);
    assert_eq!(w, vec![(T0 - DAY + 100, Spend::full(10)), (T0 - 50, Spend::full(20)), (T0 - 20, Spend::full(3))]);
}

/// A half-written last line is read on a later call, from its start.
#[test]
fn a_partial_last_line_is_read_once_complete() {
    let dir = tempfile::tempdir().unwrap();
    let line = usage_line(T0 - 5, Some("azure"), 42);
    let (a, b) = line.split_at(line.len() / 2);
    append(dir.path(), T0, a);
    let mut l = Ledger::new(dir.path());
    assert!(l.window("azure", T0, DAY as u64).is_empty());
    append(dir.path(), T0, b);
    assert_eq!(l.window("azure", T0, DAY as u64), vec![(T0 - 5, Spend::full(42))]);
}

/// (review C7) A day file rewritten in place, larger than before, is read
/// again from the start: the old entries go and every new one is counted.
/// (The probe: `[1000]` then five `1`s read back as `[1000, 1, 1, 1]`.)
#[test]
fn a_day_file_rewritten_larger_is_read_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("{}.jsonl", darkmux_flow::day_utc_at(T0)));
    std::fs::write(&path, usage_line(T0 - 50, Some("azure"), 1_000)).unwrap();
    let mut l = Ledger::new(dir.path());
    assert_eq!(l.window("azure", T0, DAY as u64), vec![(T0 - 50, Spend::full(1_000))]);
    let rewritten: String = (1..=5).map(|i| usage_line(T0 - 50 + i, Some("azure"), 1)).collect();
    std::fs::write(&path, rewritten).unwrap();
    let w = l.window("azure", T0, DAY as u64);
    assert_eq!(w.iter().map(|(_, n)| *n).collect::<Vec<_>>(), vec![Spend::full(1); 5], "{w:?}");
    // An ordinary append still reads only the new bytes.
    let before = l.bytes_read();
    let one = usage_line(T0 - 1, Some("azure"), 7);
    append(dir.path(), T0, &one);
    assert_eq!(l.window("azure", T0, DAY as u64).len(), 6);
    assert_eq!(l.bytes_read() - before, one.len() as u64);
}

/// (review C8) No operator-facing budget message carries a run of spaces
/// (a lost line continuation in the source).
#[test]
fn budget_messages_have_no_double_spaces() {
    let env = FakeEnv::new(vec![(T0 - DAY + 90, 1_000)]);
    let caller = mission_caller("m1");
    admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, None), &caller, &env).unwrap();
    admit_with(budget(BudgetPolicy::Warn, Some(10), None, None), &caller, &env).unwrap();
    admit_with(budget(BudgetPolicy::Wait, Some(0), None, None), &solo_caller(), &FakeEnv::new(vec![]).stopped("x"))
        .unwrap_err();
    let bucket = Mutex::new(crate::dispatch_budget::DispatchBudget::new(Some(1), BudgetPolicy::Warn));
    settle_dispatch(&bucket, 5, "s1", &caller, &env);
    let dir = tempfile::tempdir().unwrap();
    let stop_env = FakeEnv::new(vec![(T0 - 10, 1_000)]).stopped("x");
    let mut pacer = BudgetPacer::new(budget(BudgetPolicy::Wait, Some(1_000), None, None), None);
    pacer.on_tick(0, dir.path(), &OtherPacing::default(), &caller, &stop_env);
    env.said.borrow_mut().extend(stop_env.said.borrow().iter().cloned());
    let limits = serde_json::json!({"window": {"period": "1d", "tokens": "2M"}});
    env.said.borrow_mut().push(EndpointBudget::of(&named(limits)).unwrap_err());
    // A misspelled `limits` key is the unknown-key gate's message.
    let typo = serde_json::json!({"endpoints": {"a": {"url": "https://h.example/v1", "limits": {"windw": {}}}}});
    let keys = darkmux_types::user_files::key_issues::<darkmux_types::ProfileRegistry>(&typo, &darkmux_types::user_files::no_retired);
    env.said.borrow_mut().extend(keys.iter().map(ToString::to_string));
    let mut messages: Vec<String> = env.said.borrow().clone();
    messages.extend(env.emitted.borrow().iter().filter_map(|r| r.payload_json().get("message")?.as_str().map(str::to_string)));
    let err = EndpointBudget::of(&named(serde_json::json!({"window": {"period": "1d", "tokens": "2M"}}))).unwrap_err();
    messages.push(err);
    assert!(messages.len() >= 5, "{messages:?}");
    for m in &messages {
        assert!(!m.contains("  "), "a message carries a run of spaces: {m:?}");
    }
}

/// (Cost check) The first read of a window pays for the day files it
/// touches; every later read pays only for the bytes appended since. The
/// timings printed here are the "before/after" numbers in the #2902 step 5
/// report (`cargo nextest run -p darkmux-crew ledger_cost --no-capture`).
#[test]
fn ledger_cost_is_incremental_after_the_first_read() {
    let dir = tempfile::tempdir().unwrap();
    // A realistic day: mostly non-usage records, some usage.
    let other = serde_json::json!({"ts": darkmux_flow::ts_utc_at(T0 - 10), "category": "work", "action": "dispatch.tool",
        "payload": {"tool_name": "read", "args": "{\"path\":\"src/lib.rs\"}", "result_chars": 1234}})
    .to_string()
        + "\n";
    let mut text = String::new();
    for i in 0..20_000 {
        if i % 10 == 0 {
            text.push_str(&usage_line(T0 - 3_600 + (i % 3_000) as i64, Some("azure"), 100));
        } else {
            text.push_str(&other);
        }
    }
    append(dir.path(), T0, &text);
    append(dir.path(), T0 - DAY, &text);
    let mut l = Ledger::new(dir.path());
    let t = std::time::Instant::now();
    let first = l.window("azure", T0, DAY as u64);
    let first_us = t.elapsed().as_micros();
    let after_first = l.bytes_read();
    let one = usage_line(T0 - 1, Some("azure"), 7);
    append(dir.path(), T0, &one);
    let t = std::time::Instant::now();
    let second = l.window("azure", T0, DAY as u64);
    let second_us = t.elapsed().as_micros();
    assert_eq!(l.bytes_read() - after_first, one.len() as u64, "only the appended bytes are read");
    assert_eq!(second.len(), first.len() + 1);
    eprintln!(
        "ledger cost: first window read {} bytes in {first_us} µs; next read {} bytes in {second_us} µs",
        after_first,
        one.len()
    );
}

// ── Reading waits back ───────────────────────────────────────────────────

#[test]
fn active_waits_lists_open_waits_from_live_processes_only() {
    let dir = tempfile::tempdir().unwrap();
    let rec = |action: darkmux_flow::FlowAction, sid: &str, pid: u64, resume: Option<i64>| {
        serde_json::json!({
            "ts": darkmux_flow::ts_utc_at(T0 - 30), "action": action, "session_id": sid, "mission_id": "m-1",
            "payload": {"scope": "endpoint", "endpoint_id": "azure", "pid": pid,
                "resume_at_ms": resume.map(|r| r * 1_000), "message": "m"}
        })
        .to_string()
            + "\n"
    };
    let mut text = String::new();
    text += &rec(darkmux_flow::FlowAction::BudgetWait, "open", 1, Some(T0 + 600));
    text += &rec(darkmux_flow::FlowAction::BudgetWait, "resumed", 1, Some(T0 + 600));
    text += &rec(darkmux_flow::FlowAction::BudgetResume, "resumed", 1, None);
    text += &rec(darkmux_flow::FlowAction::BudgetWait, "dead-process", 2, Some(T0 + 600));
    text += &rec(darkmux_flow::FlowAction::BudgetWait, "long-past", 1, Some(T0 - 100));
    text += &rec(darkmux_flow::FlowAction::BudgetWait, "indefinite", 1, None);
    append(dir.path(), T0, &text);
    let waits = active_waits(dir.path(), T0, 86_400, &|pid| pid == 1);
    let sessions: Vec<&str> = waits.iter().filter_map(|w| w.execution_id.as_deref()).collect();
    assert_eq!(sessions, vec!["indefinite", "open"]);
    let open = waits.iter().find(|w| w.execution_id.as_deref() == Some("open")).unwrap();
    assert_eq!(open.resumes_in_secs, Some(600));
    assert_eq!(open.mission_id.as_deref(), Some("m-1"));
}

/// (3rd review #3) A wait whose run was stopped (its mission aborted on
/// disk) is not listed, even while its process lives; an active mission's
/// wait is.
#[test]
#[serial_test::serial]
fn active_waits_skip_a_stopped_runs_wait() {
    let crew = tempfile::tempdir().unwrap();
    let prev = std::env::var("DARKMUX_HOME").ok();
    unsafe { std::env::set_var("DARKMUX_HOME", crew.path()) };
    for (m, status) in [("m-live", "active"), ("m-gone", "aborted")] {
        let path = crate::lifecycle::mission_path(m);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!(r#"{{"status":"{status}"}}"#)).unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let rec = |sid: &str, mission: &str| {
        serde_json::json!({
            "ts": darkmux_flow::ts_utc_at(T0 - 30), "action": darkmux_flow::FlowAction::BudgetWait, "session_id": sid, "mission_id": mission,
            "payload": {"scope": "endpoint", "endpoint_id": "azure", "pid": 1,
                "resume_at_ms": (T0 + 600) * 1_000, "message": "m"}
        })
        .to_string()
            + "\n"
    };
    append(dir.path(), T0, &(rec("s-live", "m-live") + &rec("s-gone", "m-gone")));
    let waits = active_waits(dir.path(), T0, 86_400, &|_| true);
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DARKMUX_HOME", v),
            None => std::env::remove_var("DARKMUX_HOME"),
        }
    }
    let sessions: Vec<&str> = waits.iter().filter_map(|w| w.execution_id.as_deref()).collect();
    assert_eq!(sessions, vec!["s-live"]);
}

/// (review C2) A long window's wait was announced up to a window ago: it
/// stays listed while the lookback reaches its announcement, and the
/// lookback is the widest configured period (never under a day).
#[test]
fn active_waits_reach_back_to_the_widest_window() {
    let dir = tempfile::tempdir().unwrap();
    let wait = serde_json::json!({
        "ts": darkmux_flow::ts_utc_at(T0 - 3 * DAY), "action": darkmux_flow::FlowAction::BudgetWait, "session_id": "s",
        "payload": {"scope": "endpoint", "endpoint_id": "azure", "pid": 1,
            "resume_at_ms": (T0 + 3_600) * 1_000, "message": "m"}
    })
    .to_string()
        + "\n";
    append(dir.path(), T0 - 3 * DAY, &wait);
    assert!(active_waits(dir.path(), T0, waits_lookback_secs(None), &|_| true).is_empty(), "a day back misses it");
    let week = waits_lookback_secs(Some(7 * DAY as u64));
    assert_eq!(week, 7 * DAY as u64);
    assert_eq!(waits_lookback_secs(Some(60)), DAY as u64, "never under a day");
    let waits = active_waits(dir.path(), T0, week, &|_| true);
    assert_eq!(waits.len(), 1, "{waits:?}");
    assert_eq!(waits[0].resumes_in_secs, Some(3_600));
}

#[test]
fn human_duration_reads_naturally() {
    assert_eq!(human_duration(45), "45s");
    assert_eq!(human_duration(200), "3m 20s");
    assert_eq!(human_duration(3_840), "1h 4m");
}

/// A loopback server that answers with a fixed HTTP status and error body.
pub(crate) fn status_http_mock(status_line: &'static str, body: &'static str) -> String {
    use std::io::{Read, Write as IoWrite};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut chunk = [0u8; 8192];
        let _ = stream.read(&mut chunk);
        let resp = format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
    });
    format!("http://127.0.0.1:{port}/v1/chat/completions")
}

/// (#3067) A provider total of 0 is unreported. Beside both halves a step budget
/// settles their sum (what the display shows); beside one half there is no
/// total, so it is charged conservatively like a missing one, while the display
/// still floors on the half.
#[test]
fn a_step_budget_settles_a_zero_total_like_a_missing_one() {
    let both = darkmux_trajectory::UsageCounts { prompt: Some(900), completion: Some(40), total: Some(0), ..Default::default() };
    assert_eq!(super::conservative_spend(both.total_tokens(), 4096, "p"), 940);
    assert_eq!(both.floor_tokens(), 940);
    let one = darkmux_trajectory::UsageCounts { prompt: Some(900), total: Some(0), ..Default::default() };
    let missing = darkmux_trajectory::UsageCounts::default();
    assert_eq!(super::conservative_spend(one.total_tokens(), 4096, "p"), super::conservative_spend(missing.total_tokens(), 4096, "p"));
    assert!(super::conservative_spend(one.total_tokens(), 4096, "p") >= 4096);
    assert_eq!(one.floor_tokens(), 900);
}


/// (#3160) A wait that cannot end within the endpoint's `max_wait` fails at
/// once, sends nothing, and says when the window would have room. The
/// failure this pins: a CI review waited on a 30-day window that would free
/// in 714 hours, inside a 90-minute job, until the job was cancelled.
#[test]
fn a_wait_longer_than_max_wait_fails_at_once_without_sleeping() {
    let env = FakeEnv::new(vec![(T0 - 10, 1_000)]);
    let err = admit_with(waiting_at_most(1_800), &solo_caller(), &env).unwrap_err().to_string();
    assert!(err.contains("max_wait 30m") && err.contains("nothing was sent"), "{err}");
    assert!(err.contains("23h"), "names how long the wait would have been: {err}");
    assert_eq!(env.slept_ms.get(), 0, "it never sleeps");
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetStop], "the refusal is on the record");
    let stop = env.payload(darkmux_flow::FlowAction::BudgetStop);
    assert_eq!(stop["endpoint_id"].as_str(), Some("azure"));
    assert!(stop["reason"].as_str().unwrap().contains("max_wait"), "{stop}");
    assert_eq!(stop["resume_at_ms"].as_i64(), Some((T0 - 10 + DAY) * 1_000), "{stop}");
}

/// A wait that fits inside `max_wait` waits exactly as before.
#[test]
fn a_wait_within_max_wait_still_waits_and_resumes() {
    let env = FakeEnv::new(vec![(T0 - DAY + 90, 1_000)]);
    admit_with(waiting_at_most(1_800), &solo_caller(), &env).unwrap();
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWait, darkmux_flow::FlowAction::BudgetResume]);
}

/// A wait with no known resume time (a zero budget) is held at most
/// `max_wait`, then fails.
#[test]
fn a_wait_with_no_resume_time_gives_up_at_max_wait() {
    let env = FakeEnv::new(vec![]);
    let b = EndpointBudget { max_wait: Some((60, "1m".into())), ..budget(BudgetPolicy::Wait, Some(0), None, None) };
    let err = admit_with(b, &solo_caller(), &env).unwrap_err().to_string();
    assert!(err.contains("max_wait 1m"), "{err}");
    let slept = env.slept_ms.get();
    assert!((60_000..=90_000).contains(&slept), "held about max_wait, then stopped: {slept}");
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWait, darkmux_flow::FlowAction::BudgetStop]);
}
