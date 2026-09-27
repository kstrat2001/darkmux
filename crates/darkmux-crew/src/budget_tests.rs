//! (#2902 step 5) Tests for `crate::budget`. Every clock here is injected
//! and frozen ([`FakeEnv::now`]); a "sleep" advances it. No test mixes a
//! fixed timestamp with the real clock.

use super::*;
use darkmux_types::{BudgetPolicy, EndpointSource, ModelEndpoint, UsageLimits, UsageWindow};
use std::cell::{Cell, RefCell};

const T0: i64 = 1_790_000_000; // a fixed epoch second, the frozen "now"
const DAY: i64 = 86_400;

fn budget(policy: BudgetPolicy, tokens: Option<u64>, calls: Option<u64>, warn_at: Option<f64>) -> EndpointBudget {
    EndpointBudget {
        endpoint_id: "azure".into(),
        policy,
        warn_at,
        window: darkmux_types::WindowBudget { period_secs: DAY as u64, tokens, calls },
        period: "1d".into(),
    }
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
    records: RefCell<Vec<(i64, u64)>>,
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
    levels: RefCell<HashMap<String, BreachLevel>>,
}

impl FakeEnv {
    pub(crate) fn new(records: Vec<(i64, u64)>) -> Self {
        FakeEnv {
            now: Cell::new(T0),
            records: RefCell::new(records),
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
        self.emitted.borrow().iter().find(|r| r.action == action).and_then(|r| r.payload.clone()).unwrap()
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
            if let Some(r) = run_stop_reason(caller.mission_id, caller.phase_id) {
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
    fn last_level(&self, key: &str) -> Option<BreachLevel> {
        self.levels.borrow().get(key).copied()
    }
    fn set_last_level(&self, key: &str, level: Option<BreachLevel>) {
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
    assert_eq!(EndpointBudget::of(&inline), Ok(None), "inline: no id to sum by");
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
    assert_eq!(EndpointBudget::of(&managed), Ok(None), "a managed endpoint's local calls are not budgeted");
    let per_dispatch_only = named(serde_json::json!({"tokens_per_dispatch": 5}));
    assert_eq!(EndpointBudget::of(&per_dispatch_only), Ok(None), "tokens_per_dispatch is not enforced");
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
/// dropped token budget); each is now an error naming the key and the
/// nearest valid one, at the gate and in preflight's registry pass.
#[test]
fn a_misspelled_key_in_limits_is_an_error_naming_it() {
    let probes = [
        (serde_json::json!({"windw": {"period": "1d", "tokens": 10}, "policy": "wait"}), "windw", "window"),
        (serde_json::json!({"polcy": "wait", "window": {"period": "1d", "tokens": 10}}), "polcy", "policy"),
        (serde_json::json!({"policy": "wait", "window": {"period": "1d", "tokns": 10, "calls": 400}}), "tokns", "tokens"),
        (serde_json::json!({"policy": "wait", "window": {"tokns": 10}}), "tokns", "tokens"),
    ];
    for (limits, key, nearest) in probes {
        let err = EndpointBudget::of(&named(limits.clone())).expect_err(&format!("{limits} must not be Ok"));
        assert!(err.contains(&format!("unknown key `{key}`")) && err.contains(&format!("did you mean `{nearest}`")), "{limits}: {err}");
        let mut reg: darkmux_types::ProfileRegistry = serde_json::from_value(serde_json::json!({
            "profiles": {"p": {"models": [{"id": "m", "endpoint": "azure"}]}},
            "endpoints": {"azure": {"url": "https://h.example/v1", "limits": limits}},
        }))
        .unwrap();
        reg.materialize_endpoints();
        let invalid = darkmux_types::config_enum::invalid_endpoint_limits(&reg);
        assert_eq!(invalid.len(), 1, "{limits}: {invalid:?}");
        assert!(invalid[0].problem.contains(key), "{:?}", invalid[0]);
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

#[test]
fn under_budget_proceeds_at_budget_warns_or_waits() {
    let warn = budget(BudgetPolicy::Warn, Some(1_000), None, None);
    assert_eq!(evaluate(&warn, &[(T0 - 10, 999)], T0), Verdict::Proceed);
    let at = [(T0 - 10, 600), (T0 - 5, 400)];
    let Verdict::Warn(br) = evaluate(&warn, &at, T0) else { panic!() };
    assert_eq!((br.level, br.metric, br.spent, br.limit), (BreachLevel::AtLimit, Metric::Tokens, 1_000, 1_000));
    let wait = budget(BudgetPolicy::Wait, Some(1_000), None, None);
    // Room returns when the 600 at T0-10 leaves the window: T0-10 + 1d.
    assert_eq!(
        evaluate(&wait, &at, T0),
        Verdict::Wait { breach: br, resume_at: Some(T0 - 10 + DAY) }
    );
}

/// Rolling, no calendar reset: a record leaves the window at exactly
/// `t + period`, whatever day it is.
#[test]
fn the_window_rolls_over_the_last_period_from_now() {
    let b = budget(BudgetPolicy::Warn, Some(100), None, None);
    let old = [(T0 - DAY, 100)];
    assert_eq!(evaluate(&b, &old, T0), Verdict::Proceed, "t + period == now: out");
    let inside = [(T0 - DAY + 1, 100)];
    assert!(matches!(evaluate(&b, &inside, T0), Verdict::Warn(_)), "t + period > now: in");
    let future = [(T0 + 5, 100)];
    assert_eq!(evaluate(&b, &future, T0), Verdict::Proceed, "a record stamped ahead of now is not counted yet");
}

#[test]
fn a_calls_budget_and_the_resume_time_for_calls() {
    let b = budget(BudgetPolicy::Wait, None, Some(3), None);
    let entries = [(T0 - 300, 1), (T0 - 200, 1), (T0 - 100, 1)];
    let Verdict::Wait { breach, resume_at } = evaluate(&b, &entries, T0) else { panic!() };
    assert_eq!((breach.metric, breach.spent, breach.limit), (Metric::Calls, 3, 3));
    assert_eq!(resume_at, Some(T0 - 300 + DAY), "one call leaving makes room for one");
    assert_eq!(evaluate(&b, &entries[..2], T0), Verdict::Proceed);
}

/// Both budgets breached: room returns when BOTH are back under.
#[test]
fn two_breached_budgets_resume_at_the_later_time() {
    let b = budget(BudgetPolicy::Wait, Some(100), Some(2), None);
    let entries = [(T0 - 500, 90), (T0 - 400, 5), (T0 - 300, 5)];
    // Tokens 100 >= 100 → room when the 90 leaves (T0-500+DAY).
    // Calls 3 >= 2 → room when two calls leave (T0-400+DAY). Later wins.
    let Verdict::Wait { resume_at, .. } = evaluate(&b, &entries, T0) else { panic!() };
    assert_eq!(resume_at, Some(T0 - 400 + DAY));
}

/// (Defensive: `limits.validate` refuses a zero window, so only a budget
/// built directly reaches this.) No resume time exists for 0.
#[test]
fn a_zero_budget_waits_with_no_resume_time() {
    let b = budget(BudgetPolicy::Wait, Some(0), None, None);
    assert!(matches!(evaluate(&b, &[], T0), Verdict::Wait { resume_at: None, .. }));
}

/// `warn_at` warns once earlier; unset, only the at-limit warning fires.
#[test]
fn warn_at_warns_early_and_is_never_guessed() {
    let with = budget(BudgetPolicy::Warn, Some(1_000), None, Some(0.8));
    let Verdict::Warn(br) = evaluate(&with, &[(T0 - 1, 800)], T0) else { panic!() };
    assert_eq!(br.level, BreachLevel::Early);
    assert_eq!(evaluate(&with, &[(T0 - 1, 799)], T0), Verdict::Proceed);
    let without = budget(BudgetPolicy::Warn, Some(1_000), None, None);
    assert_eq!(evaluate(&without, &[(T0 - 1, 999)], T0), Verdict::Proceed, "no threshold picked by darkmux");
    let wait_early = budget(BudgetPolicy::Wait, Some(1_000), None, Some(0.8));
    assert!(matches!(evaluate(&wait_early, &[(T0 - 1, 900)], T0), Verdict::Warn(_)), "wait warns early, waits only at the limit");
}

/// (3rd review #2) The warning names the policy it runs under: an early
/// warning under `wait` says the calls will wait at the limit, never
/// "policy warn".
#[test]
fn the_warning_names_the_policy_it_runs_under() {
    let env = FakeEnv::new(vec![(T0 - 60, 900)]);
    admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, Some(0.8)), &BudgetCaller::default(), &env).unwrap();
    let said = env.said.borrow()[0].clone();
    assert!(said.contains("policy wait: continuing; calls wait once the budget is reached"), "{said}");
    assert!(!said.contains("policy warn"), "{said}");
    assert_eq!(env.payload(darkmux_flow::FlowAction::BudgetWarn)["policy"], "wait");
    let env = FakeEnv::new(vec![(T0 - 60, 900)]);
    admit_with(budget(BudgetPolicy::Warn, Some(1_000), None, Some(0.8)), &BudgetCaller::default(), &env).unwrap();
    assert!(env.said.borrow()[0].contains("policy warn: continuing"), "{:?}", env.said.borrow());
}

#[test]
fn off_never_acts_even_if_evaluated() {
    let b = budget(BudgetPolicy::Off, Some(0), Some(0), None);
    assert_eq!(evaluate(&b, &[(T0, 99)], T0), Verdict::Proceed);
}

// ── The gate ─────────────────────────────────────────────────────────────

/// `warn` never stops and never sleeps; the breach is surfaced once, and
/// again only after the spend dropped back below and rose again.
#[test]
fn warn_surfaces_the_breach_once_and_never_holds_the_call() {
    let env = FakeEnv::new(vec![(T0 - 60, 2_000)]);
    let b = budget(BudgetPolicy::Warn, Some(1_000), None, None);
    let caller = BudgetCaller { session_id: Some("s1"), mission_id: Some("m1"), ..Default::default() };
    admit_with(b.clone(), &caller, &env).unwrap();
    admit_with(b.clone(), &caller, &env).unwrap();
    assert_eq!(env.slept_ms.get(), 0, "warn never waits");
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWarn], "surfaced once");
    let p = env.payload(darkmux_flow::FlowAction::BudgetWarn);
    assert_eq!((p["spent"].as_u64(), p["limit"].as_u64(), p["level"].as_str()), (Some(2_000), Some(1_000), Some("at_limit")));
    assert!(env.said.borrow()[0].contains("continuing"), "{:?}", env.said.borrow());
    let rec = env.emitted.borrow()[0].clone();
    assert_eq!((rec.session_id.as_deref(), rec.mission_id.as_deref()), (Some("s1"), Some("m1")), "lands on the run");
    // Spend leaves the window, then returns: a second crossing warns again.
    env.records.borrow_mut().clear();
    admit_with(b.clone(), &caller, &env).unwrap();
    env.records.borrow_mut().push((T0, 5_000));
    admit_with(b, &caller, &env).unwrap();
    assert_eq!(env.actions().len(), 2);
}

/// (6th review MF) A budget record lands on its RUN's session: a hosted
/// step's caller carries the bare `task-<id>` session every mission from
/// one config shares, and a record that bypasses `mission_launch`'s
/// `scope_to_run` would let mission A's `budget.stop` close mission B's
/// wait. Scoped here, once, for every budget record with a mission; a
/// caller with no mission (a standalone dispatch) keeps its own id.
#[test]
fn budget_records_are_scoped_to_their_run() {
    let env = FakeEnv::full_window().stopped_after(1, "mission `m-a` is aborted");
    let caller = BudgetCaller { session_id: Some("task-probe"), mission_id: Some("m-a"), ..Default::default() };
    let _ = admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, None), &caller, &env);
    let sids: Vec<Option<String>> = env.emitted.borrow().iter().map(|r| r.session_id.clone()).collect();
    assert_eq!(sids, vec![Some("task-probe-m-a".to_string()); 2], "wait and stop, both on mission A's run");
    let env = FakeEnv::new(vec![(T0 - 60, 2_000)]);
    let solo = BudgetCaller { session_id: Some("dispatch-coder-1"), ..Default::default() };
    admit_with(budget(BudgetPolicy::Warn, Some(1_000), None, None), &solo, &env).unwrap();
    assert_eq!(env.emitted.borrow()[0].session_id.as_deref(), Some("dispatch-coder-1"), "no mission: unchanged");
}

/// `wait` holds the call until the rolling window has room, says how
/// long, records the pause (the run's time limits skip it), and resumes.
#[test]
fn wait_holds_until_the_window_has_room_then_resumes() {
    // 1000 of 1000 spent; the only record leaves at T0 - 60 + DAY + 1.
    let env = FakeEnv::new(vec![(T0 - DAY + 90, 1_000)]);
    let b = budget(BudgetPolicy::Wait, Some(1_000), None, None);
    admit_with(b, &BudgetCaller::default(), &env).unwrap();
    // Room at T0 + 90 (+1 s, the leave-at-exactly-t+period rule).
    assert!(env.now.get() >= T0 + 90 && env.now.get() <= T0 + 91, "{}", env.now.get());
    assert_eq!(env.paused_ms.get(), env.slept_ms.get(), "every waited ms is recorded as pause");
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWait, darkmux_flow::FlowAction::BudgetResume]);
    let wait = env.payload(darkmux_flow::FlowAction::BudgetWait);
    assert_eq!(wait["wait_seconds"].as_i64(), Some(90));
    assert_eq!(wait["resume_at"].as_str(), Some(darkmux_flow::ts_utc_at(T0 + 90).as_str()));
    assert!(env.said.borrow()[0].contains("resuming in about 1m 30s"), "{:?}", env.said.borrow());
    assert!(env.payload(darkmux_flow::FlowAction::BudgetResume)["waited_ms"].as_u64().unwrap() >= 90_000);
}

/// A stopped run (an interrupt, or a `mission abort`) ends a wait before
/// anything is sent; the gate says why.
#[test]
fn a_stopped_run_ends_a_wait_and_nothing_is_sent() {
    let env = FakeEnv::new(vec![(T0 - 10, 1_000)]).stopped_after(2_000, "mission `m` is aborted");
    let b = budget(BudgetPolicy::Wait, Some(1_000), None, None);
    let err = admit_with(b, &BudgetCaller::default(), &env).unwrap_err().to_string();
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
    let prev = std::env::var("DARKMUX_CREW_DIR").ok();
    unsafe { std::env::set_var("DARKMUX_CREW_DIR", crew.path()) };
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
    let caller = BudgetCaller { mission_id: Some("m1"), ..Default::default() };
    let res = admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, None), &caller, &env);
    let live = LiveEnv.stop_reason(&caller);
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
            None => std::env::remove_var("DARKMUX_CREW_DIR"),
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
    let err = admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, None), &BudgetCaller::default(), &env)
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
    admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, None), &BudgetCaller::default(), &env).unwrap();
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWait, darkmux_flow::FlowAction::BudgetResume]);
}

/// (3rd review #3) The reload release point re-checks the stop: an abort
/// and a budget removal landing in the same slice send nothing, and the
/// ended wait is recorded as `budget.stop`, never `budget.resume`.
#[test]
fn an_abort_and_a_removal_in_one_slice_still_send_nothing() {
    let env = FakeEnv::full_window().stopped_after(1, "mission `m` is aborted");
    *env.reload_to.borrow_mut() = Some(None);
    let err = admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, None), &BudgetCaller::default(), &env)
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
    admit_with(b, &BudgetCaller::default(), &env).unwrap();
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
    admit_with(budget(BudgetPolicy::Wait, Some(0), None, None), &BudgetCaller::default(), &env).unwrap();
    assert!(env.payload(darkmux_flow::FlowAction::BudgetWait)["resume_at"].is_null());
    let said = env.said.borrow()[0].clone();
    assert!(said.contains("until its window has room") && !said.contains("budget is 0"), "{said}");
}

// ── The per-step cap ─────────────────────────────────────────────────────

#[test]
fn a_step_under_warn_never_holds_and_warns_once_on_crossing() {
    use darkmux_types::config::StepBudgetPolicy;
    let env = FakeEnv::new(vec![]);
    let bucket = Mutex::new(crate::remote_budget::RemoteBudget::new(Some(1_000), StepBudgetPolicy::Warn));
    for _ in 0..3 {
        admit_step(&bucket, 4_096);
        settle_step(&bucket, 4_096, 600, 1, "probe", &BudgetCaller::default(), &env);
    }
    assert_eq!(env.slept_ms.get(), 0);
    assert_eq!(env.actions(), vec![darkmux_flow::FlowAction::BudgetWarn], "one warning, at the crossing");
    let p = env.payload(darkmux_flow::FlowAction::BudgetWarn);
    assert_eq!((p["scope"].as_str(), p["step"].as_str()), (Some("step"), Some("probe")));
}

/// (operator, 2026-09-27) The per-step cap has `off` and `warn` only:
/// `wait` is refused by the registry (preflight, doctor, `config set`,
/// help all read it).
#[test]
fn the_step_policy_refuses_wait() {
    use darkmux_types::config_enum::{ConfigEnum, ENUM_SETTINGS};
    assert_eq!(darkmux_types::config::StepBudgetPolicy::TOKENS, &["off", "warn"]);
    let s = ENUM_SETTINGS.iter().find(|s| s.key == "remote.step_budget_policy").unwrap();
    assert_eq!(s.canonical("wait"), None);
    assert_eq!(s.canonical("warn"), Some("warn"));
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
    let c = BudgetCaller::default();
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
    let c = BudgetCaller::default();
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
    let c = BudgetCaller { mission_id: Some("m1"), ..Default::default() };
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
    let ev = p.on_tick(0, dir.path(), &OtherPacing::default(), &BudgetCaller::default(), &env);
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
    assert_eq!(p.on_tick(0, dir.path(), &OtherPacing::default(), &BudgetCaller::default(), &env), None);
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
    let c = BudgetCaller::default();
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
    assert_eq!(w, vec![(T0 - DAY + 100, 10), (T0 - 50, 20), (T0 - 20, 3)]);
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
    assert_eq!(l.window("azure", T0, DAY as u64), vec![(T0 - 5, 42)]);
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
    assert_eq!(l.window("azure", T0, DAY as u64), vec![(T0 - 50, 1_000)]);
    let rewritten: String = (1..=5).map(|i| usage_line(T0 - 50 + i, Some("azure"), 1)).collect();
    std::fs::write(&path, rewritten).unwrap();
    let w = l.window("azure", T0, DAY as u64);
    assert_eq!(w.iter().map(|(_, n)| *n).collect::<Vec<_>>(), vec![1, 1, 1, 1, 1], "{w:?}");
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
    use darkmux_types::config::StepBudgetPolicy;
    let env = FakeEnv::new(vec![(T0 - DAY + 90, 1_000)]);
    let caller = BudgetCaller { mission_id: Some("m1"), ..Default::default() };
    admit_with(budget(BudgetPolicy::Wait, Some(1_000), None, None), &caller, &env).unwrap();
    admit_with(budget(BudgetPolicy::Warn, Some(10), None, None), &caller, &env).unwrap();
    admit_with(budget(BudgetPolicy::Wait, Some(0), None, None), &BudgetCaller::default(), &FakeEnv::new(vec![]).stopped("x"))
        .unwrap_err();
    let bucket = Mutex::new(crate::remote_budget::RemoteBudget::new(Some(1), StepBudgetPolicy::Warn));
    admit_step(&bucket, 1);
    settle_step(&bucket, 1, 5, 1, "s1", &caller, &env);
    let dir = tempfile::tempdir().unwrap();
    let stop_env = FakeEnv::new(vec![(T0 - 10, 1_000)]).stopped("x");
    let mut pacer = BudgetPacer::new(budget(BudgetPolicy::Wait, Some(1_000), None, None), None);
    pacer.on_tick(0, dir.path(), &OtherPacing::default(), &caller, &stop_env);
    env.said.borrow_mut().extend(stop_env.said.borrow().iter().cloned());
    for e in ["2M", "windw"] {
        let limits = if e == "2M" { serde_json::json!({"window": {"period": "1d", "tokens": "2M"}}) } else { serde_json::json!({"windw": {}}) };
        env.said.borrow_mut().push(EndpointBudget::of(&named(limits)).unwrap_err());
    }
    let mut messages: Vec<String> = env.said.borrow().clone();
    messages.extend(env.emitted.borrow().iter().filter_map(|r| r.payload.as_ref()?.get("message")?.as_str().map(str::to_string)));
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
                "resume_at": resume.map(darkmux_flow::ts_utc_at), "message": "m"}
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
    let sessions: Vec<&str> = waits.iter().filter_map(|w| w.session_id.as_deref()).collect();
    assert_eq!(sessions, vec!["indefinite", "open"]);
    let open = waits.iter().find(|w| w.session_id.as_deref() == Some("open")).unwrap();
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
    let prev = std::env::var("DARKMUX_CREW_DIR").ok();
    unsafe { std::env::set_var("DARKMUX_CREW_DIR", crew.path()) };
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
                "resume_at": darkmux_flow::ts_utc_at(T0 + 600), "message": "m"}
        })
        .to_string()
            + "\n"
    };
    append(dir.path(), T0, &(rec("s-live", "m-live") + &rec("s-gone", "m-gone")));
    let waits = active_waits(dir.path(), T0, 86_400, &|_| true);
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
            None => std::env::remove_var("DARKMUX_CREW_DIR"),
        }
    }
    let sessions: Vec<&str> = waits.iter().filter_map(|w| w.session_id.as_deref()).collect();
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
            "resume_at": darkmux_flow::ts_utc_at(T0 + 3_600), "message": "m"}
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
