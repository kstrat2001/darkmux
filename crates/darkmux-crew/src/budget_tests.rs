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

struct FakeEnv {
    now: Cell<i64>,
    records: RefCell<Vec<(i64, u64)>>,
    slept_ms: Cell<u64>,
    paused_ms: Cell<u64>,
    interrupted: Cell<bool>,
    /// Interrupt once this much has been slept.
    interrupt_after_ms: Option<u64>,
    /// What `reload` returns (`None` = keep the budget in hand).
    reload_to: RefCell<Option<Option<EndpointBudget>>>,
    emitted: RefCell<Vec<darkmux_flow::FlowRecord>>,
    said: RefCell<Vec<String>>,
    levels: RefCell<HashMap<String, BreachLevel>>,
    /// The stage budget `stage_budget_fresh` reports.
    stage: RefCell<(Option<u64>, BudgetPolicy)>,
}

impl FakeEnv {
    fn new(records: Vec<(i64, u64)>) -> Self {
        FakeEnv {
            now: Cell::new(T0),
            records: RefCell::new(records),
            slept_ms: Cell::new(0),
            paused_ms: Cell::new(0),
            interrupted: Cell::new(false),
            interrupt_after_ms: None,
            reload_to: RefCell::new(None),
            emitted: RefCell::new(Vec::new()),
            said: RefCell::new(Vec::new()),
            levels: RefCell::new(HashMap::new()),
            stage: RefCell::new((None, BudgetPolicy::Warn)),
        }
    }
    fn actions(&self) -> Vec<String> {
        self.emitted.borrow().iter().map(|r| r.action.clone()).collect()
    }
    fn payload(&self, action: &str) -> serde_json::Value {
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
        if let Some(after) = self.interrupt_after_ms {
            if self.slept_ms.get() >= after {
                self.interrupted.set(true);
            }
        }
    }
    fn interrupted(&self) -> bool {
        self.interrupted.get()
    }
    fn reload(&self, _id: &str) -> Option<Option<EndpointBudget>> {
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
    fn stage_budget_fresh(&self) -> (Option<u64>, Result<BudgetPolicy, darkmux_types::config_enum::BadEnumValue>) {
        let (b, p) = *self.stage.borrow();
        (b, Ok(p))
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
    assert_eq!(env.actions(), vec![BUDGET_WARN_ACTION], "surfaced once");
    let p = env.payload(BUDGET_WARN_ACTION);
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
    assert_eq!(env.actions(), vec![BUDGET_WAIT_ACTION, BUDGET_RESUME_ACTION]);
    let wait = env.payload(BUDGET_WAIT_ACTION);
    assert_eq!(wait["wait_seconds"].as_i64(), Some(90));
    assert_eq!(wait["resume_at"].as_str(), Some(darkmux_flow::ts_utc_at(T0 + 90).as_str()));
    assert!(env.said.borrow()[0].contains("resuming in about 1m 30s"), "{:?}", env.said.borrow());
    assert!(env.payload(BUDGET_RESUME_ACTION)["waited_ms"].as_u64().unwrap() >= 90_000);
}

/// An abort ends a wait before anything is sent; the gate says so.
#[test]
fn an_interrupt_ends_a_wait_and_nothing_is_sent() {
    let mut env = FakeEnv::new(vec![(T0 - 10, 1_000)]);
    env.interrupt_after_ms = Some(2_000);
    let b = budget(BudgetPolicy::Wait, Some(1_000), None, None);
    let err = admit_with(b, &BudgetCaller::default(), &env).unwrap_err().to_string();
    assert!(err.contains("interrupted") && err.contains("nothing was sent"), "{err}");
    assert!(env.slept_ms.get() <= 30_000, "stopped within one poll: {}", env.slept_ms.get());
}

/// A budget switched off (or raised) while waiting releases the wait.
#[test]
fn a_budget_switched_off_while_waiting_releases_it() {
    let env = FakeEnv::new(vec![(T0 - 10, 1_000)]);
    *env.reload_to.borrow_mut() = Some(None);
    let b = budget(BudgetPolicy::Wait, Some(1_000), None, None);
    admit_with(b, &BudgetCaller::default(), &env).unwrap();
    assert!(env.slept_ms.get() <= 30_000, "released at the first re-read: {}", env.slept_ms.get());
    assert_eq!(env.actions(), vec![BUDGET_WAIT_ACTION, BUDGET_RESUME_ACTION]);
}

/// A zero budget under `wait` waits (it has no resume time) and polls.
#[test]
fn a_zero_budget_waits_until_raised() {
    let env = FakeEnv::new(vec![]);
    *env.reload_to.borrow_mut() = Some(Some(budget(BudgetPolicy::Wait, Some(5), None, None)));
    admit_with(budget(BudgetPolicy::Wait, Some(0), None, None), &BudgetCaller::default(), &env).unwrap();
    assert!(env.payload(BUDGET_WAIT_ACTION)["resume_at"].is_null());
    assert!(env.said.borrow()[0].contains("waits until it is raised"));
}

// ── The stage budget ─────────────────────────────────────────────────────

#[test]
fn a_stage_under_warn_never_holds_and_warns_once_on_crossing() {
    let env = FakeEnv::new(vec![]);
    let bucket = Mutex::new(crate::remote_budget::RemoteBudget::new(Some(1_000), BudgetPolicy::Warn));
    for _ in 0..3 {
        admit_stage(&bucket, 4_096, "probe", &BudgetCaller::default(), &env).unwrap();
        settle_stage(&bucket, 4_096, 600, 1, "probe", &BudgetCaller::default(), &env);
    }
    assert_eq!(env.slept_ms.get(), 0);
    assert_eq!(env.actions(), vec![BUDGET_WARN_ACTION], "one warning, at the crossing");
    let p = env.payload(BUDGET_WARN_ACTION);
    assert_eq!((p["scope"].as_str(), p["stage"].as_str()), (Some("stage"), Some("probe")));
}

/// A stage under `wait` holds its next call until the budget is raised
/// (read fresh from the config while it waits), then resumes.
#[test]
fn a_stage_under_wait_holds_until_the_budget_is_raised() {
    let env = FakeEnv::new(vec![]);
    let bucket = Mutex::new(crate::remote_budget::RemoteBudget::new(Some(1_000), BudgetPolicy::Wait));
    admit_stage(&bucket, 100, "s1", &BudgetCaller::default(), &env).unwrap();
    settle_stage(&bucket, 100, 1_000, 1, "s1", &BudgetCaller::default(), &env);
    *env.stage.borrow_mut() = (Some(10_000), BudgetPolicy::Wait);
    admit_stage(&bucket, 100, "s1", &BudgetCaller::default(), &env).unwrap();
    assert!(env.slept_ms.get() > 0, "it waited");
    assert_eq!(env.paused_ms.get(), env.slept_ms.get());
    assert_eq!(env.actions(), vec![BUDGET_WARN_ACTION, BUDGET_WAIT_ACTION, BUDGET_RESUME_ACTION]);
}

#[test]
fn a_waiting_stage_ends_only_on_an_interrupt() {
    let mut env = FakeEnv::new(vec![]);
    env.interrupt_after_ms = Some(45_000);
    *env.stage.borrow_mut() = (Some(0), BudgetPolicy::Wait);
    let bucket = Mutex::new(crate::remote_budget::RemoteBudget::new(Some(0), BudgetPolicy::Wait));
    let err = admit_stage(&bucket, 100, "s1", &BudgetCaller::default(), &env).unwrap_err().to_string();
    assert!(err.contains("interrupted while stage `s1` waited"), "{err}");
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
    let mut p = BudgetPacer::new(budget(BudgetPolicy::Wait, Some(1_000), None, None));
    let c = BudgetCaller::default();
    let ev = p.on_tick(0, dir.path(), false, &c, &env);
    assert!(matches!(ev, Some(PacerEvent::Paused { .. })), "{ev:?}");
    assert_eq!(pace(dir.path())["pause"], true);
    assert_eq!(pace(dir.path())["reason"], PACE_REASON);
    // Another governor pauses: the pacer writes nothing over it.
    crate::pace_file::write(dir.path(), true, "thermal", "serious");
    assert_eq!(p.on_tick(2_000, dir.path(), true, &c, &env), None);
    assert_eq!(pace(dir.path())["reason"], "thermal");
    // Thermal lets go: the pacer re-asserts its own hold.
    assert_eq!(p.on_tick(2_000, dir.path(), false, &c, &env), None);
    assert_eq!(pace(dir.path())["reason"], PACE_REASON);
    assert_eq!(env.paused_ms.get(), 4_000, "time held counts as pause");
    // The record leaves the window.
    env.now.set(T0 + 11);
    let ev = p.on_tick(2_000, dir.path(), false, &c, &env);
    assert!(matches!(ev, Some(PacerEvent::Resumed { .. })), "{ev:?}");
    assert_eq!(pace(dir.path())["pause"], false);
    assert!(!p.is_pausing());
    assert_eq!(env.actions(), vec![BUDGET_WAIT_ACTION, BUDGET_RESUME_ACTION]);
}

/// Under `warn` the pacer never touches the pace file.
#[test]
fn the_pacer_under_warn_never_pauses() {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnv::new(vec![(T0 - 10, 5_000)]);
    let mut p = BudgetPacer::new(budget(BudgetPolicy::Warn, Some(1_000), None, None));
    assert_eq!(p.on_tick(0, dir.path(), false, &BudgetCaller::default(), &env), None);
    assert!(!crate::pace_file::path(dir.path()).exists());
    assert_eq!(env.actions(), vec![BUDGET_WARN_ACTION]);
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
    let rec = |action: &str, sid: &str, pid: u64, resume: Option<i64>| {
        serde_json::json!({
            "ts": darkmux_flow::ts_utc_at(T0 - 30), "action": action, "session_id": sid, "mission_id": "m-1",
            "payload": {"scope": "endpoint", "endpoint_id": "azure", "pid": pid,
                "resume_at": resume.map(darkmux_flow::ts_utc_at), "message": "m"}
        })
        .to_string()
            + "\n"
    };
    let mut text = String::new();
    text += &rec(BUDGET_WAIT_ACTION, "open", 1, Some(T0 + 600));
    text += &rec(BUDGET_WAIT_ACTION, "resumed", 1, Some(T0 + 600));
    text += &rec(BUDGET_RESUME_ACTION, "resumed", 1, None);
    text += &rec(BUDGET_WAIT_ACTION, "dead-process", 2, Some(T0 + 600));
    text += &rec(BUDGET_WAIT_ACTION, "long-past", 1, Some(T0 - 100));
    text += &rec(BUDGET_WAIT_ACTION, "indefinite", 1, None);
    append(dir.path(), T0, &text);
    let waits = active_waits(dir.path(), T0, &|pid| pid == 1);
    let sessions: Vec<&str> = waits.iter().filter_map(|w| w.session_id.as_deref()).collect();
    assert_eq!(sessions, vec!["indefinite", "open"]);
    let open = waits.iter().find(|w| w.session_id.as_deref() == Some("open")).unwrap();
    assert_eq!(open.resumes_in_secs, Some(600));
    assert_eq!(open.mission_id.as_deref(), Some("m-1"));
}

#[test]
fn human_duration_reads_naturally() {
    assert_eq!(human_duration(45), "45s");
    assert_eq!(human_duration(200), "3m 20s");
    assert_eq!(human_duration(3_840), "1h 4m");
}
