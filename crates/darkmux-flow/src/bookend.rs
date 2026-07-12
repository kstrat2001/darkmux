//! Generic RAII liveness-bookend stack (#1230 Packet 0).
//!
//! Three structurally-identical guards grew independently before this
//! module existed: `darkmux-crew`'s `DispatchBookendGuard` (flat, one
//! `dispatch.start` → `dispatch.complete`/`dispatch.error`),
//! `darkmux-lab`'s `FunnelBookendGuard` (a stack — one `funnel.task`
//! nesting `funnel.step`s), and `src/pr_review.rs`'s
//! `FunnelDispatchBookendGuard` (flat, bridging a funnel run into the
//! `dispatch.*` vocabulary). Same shape every time: emit a `started`
//! record, arm a guard, and guarantee a matching terminal record fires no
//! matter how the covered code exits — clean return, early `?`-return, or
//! panic. A flat one-shot guard is just a stack of depth ≤ 1, so one type
//! subsumes both shapes.
//!
//! The duplication is why the concrete bug this packet fixes (a funnel run
//! with a remote-endpoint seat silently reporting 0 cloud tokens — see
//! [`stamp_remote_classification`]) went unnoticed: `pr_review.rs`'s
//! independently-reinvented terminal-record payload never got the
//! `payload.endpoint` field the other two paths already carried.
//!
//! This module intentionally knows nothing about `FlowRecord`'s domain
//! meaning beyond its existence — callers build every `started`/`finished`/
//! abort record themselves (via their own crate's record builders,
//! `darkmux-crew::dispatch::build_dispatch_record_with_payload` or
//! `darkmux-lab`'s `funnel_flow_record`) and hand the guard a fully-built
//! [`FlowRecord`]. `darkmux-flow` is a dependency LEAF w.r.t. both
//! `darkmux-crew` and `darkmux-lab` (neither of those crates' record
//! builders can be called from here without introducing a cycle), so this
//! stays true by construction, not by convention.

use crate::FlowRecord;

/// Where a [`BookendGuard`] sends the records it opens/closes/emits.
/// Blanket-implemented for any `FnMut(FlowRecord)` (typically a closure
/// wrapping `darkmux_flow::record`), so most callers never need a named
/// type — construct the guard with a closure directly. A caller that also
/// needs to hand the SAME underlying sink to code the guard doesn't own
/// (see `src/pr_review.rs`'s `with_dispatch_bookends`, which must lend its
/// sink to the wrapped funnel dispatch as a `&mut dyn FunnelEmitter`) can
/// implement this trait on its own adapter type and construct
/// `BookendGuard` generically over that concrete type instead of a trait
/// object — see [`BookendGuard::sink_mut`].
pub trait BookendSink {
    fn emit(&mut self, record: FlowRecord);
}

impl<F: FnMut(FlowRecord)> BookendSink for F {
    fn emit(&mut self, record: FlowRecord) {
        (self)(record)
    }
}

/// One open (started, not yet finished) unit on a [`BookendGuard`]'s stack.
/// `kind` is opaque to the guard — callers use it purely to decide, inside
/// their `on_abort` closure, which record shape to build for a given open
/// unit (e.g. the funnel guard's `on_abort` builds a `funnel.task`-shaped
/// abort record when `kind == "task"`, a `funnel.step`-shaped one
/// otherwise).
struct OpenUnit {
    id: String,
    kind: String,
}

/// Builds the terminal record for one still-open unit at Drop time —
/// factored into a named alias (clippy's `type_complexity`) rather than
/// spelled inline on [`BookendGuard`]'s field/constructor.
type OnAbort<'a> = Box<dyn Fn(&str, &str) -> FlowRecord + 'a>;

/// Generic RAII liveness-bookend stack. `open()` arms the guard, pushes an
/// [`OpenUnit`], and emits the caller-built `started` record; `close()`
/// emits the caller-built `finished` record and pops the matching unit,
/// disarming once the stack empties. A guard dropped while still armed
/// (an early `?`-return or a panic between `open()` and its matching
/// `close()`) pops every still-open unit **innermost-first** (LIFO — the
/// same order a nested funnel task/step pipeline needs: the deepest open
/// step closes before its enclosing phase, which closes before the task),
/// emitting `on_abort(id, kind)` for each — so every `started` gets a
/// matching terminal event on every exit path.
///
/// Generic over the sink type `S` (rather than hardcoding `dyn
/// BookendSink`) so a caller that needs to re-view the same underlying
/// sink through a DIFFERENT trait mid-guard-lifetime (lending it to code
/// the guard doesn't own — see [`Self::sink_mut`]) can do so without an
/// unsafe downcast: give `S` a concrete adapter type that implements both
/// `BookendSink` and whatever trait the borrowed-out code needs, and
/// `sink_mut()` hands back a `&mut S` the caller can coerce.
pub struct BookendGuard<'a, S: BookendSink + ?Sized> {
    armed: bool,
    open: Vec<OpenUnit>,
    sink: &'a mut S,
    on_abort: OnAbort<'a>,
}

impl<'a, S: BookendSink + ?Sized> BookendGuard<'a, S> {
    /// `on_abort(id, kind)` builds the terminal record for one still-open
    /// unit at Drop time. Called once per open unit, innermost-first.
    pub fn new(sink: &'a mut S, on_abort: impl Fn(&str, &str) -> FlowRecord + 'a) -> Self {
        Self { armed: false, open: Vec::new(), sink, on_abort: Box::new(on_abort) }
    }

    /// Arm the guard, push `(id, kind)` onto the open stack, and emit
    /// `started`. From here, an early return or panic before the matching
    /// `close(id, ...)` fires this unit's abort record on Drop.
    pub fn open(&mut self, id: &str, kind: &str, started: FlowRecord) {
        self.armed = true;
        self.open.push(OpenUnit { id: id.to_string(), kind: kind.to_string() });
        self.sink.emit(started);
    }

    /// Emit `finished` and pop the unit matching `id` off the open stack
    /// (a no-op on the stack if `id` isn't open — mirrors the one-shot
    /// procedural-step case, which calls `close` with no prior `open`).
    /// Disarms once the stack is empty — the guard fires no abort record
    /// on Drop once every unit it opened has a matching terminal.
    pub fn close(&mut self, id: &str, finished: FlowRecord) {
        self.open.retain(|u| u.id != id);
        self.sink.emit(finished);
        if self.open.is_empty() {
            self.armed = false;
        }
    }

    /// Disarm without closing any unit — for a caller that already emitted
    /// its own terminal record through a different path and just needs to
    /// silence the Drop backstop.
    pub fn disarm(&mut self) {
        self.armed = false;
    }

    /// Emit `record` through the guard's sink without touching the open
    /// stack — for ticker/telemetry records that carry no open/close
    /// bookend of their own (a per-ruling ticker, a drained telemetry
    /// sample).
    pub fn emit_now(&mut self, record: FlowRecord) {
        self.sink.emit(record);
    }

    /// Re-borrow the underlying sink. Exists so a caller whose `S` is a
    /// concrete adapter type (not a type-erased `dyn BookendSink`) can hand
    /// the SAME sink to code the guard doesn't own, viewed through whatever
    /// other trait that adapter also implements — see this module's doc
    /// for why the guard can't do that lending itself. The reborrow must
    /// end before the next `open`/`close`/`disarm` call (ordinary borrow-
    /// checker discipline); the guard's own Drop still fires correctly on
    /// a panic that occurs while the reborrow is in use, since Rust drops
    /// the guard (a local in the enclosing frame) during unwind once the
    /// reborrow's shorter lifetime has already ended.
    pub fn sink_mut(&mut self) -> &mut S {
        self.sink
    }
}

impl<S: BookendSink + ?Sized> Drop for BookendGuard<'_, S> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        while let Some(unit) = self.open.pop() {
            let record = (self.on_abort)(&unit.id, &unit.kind);
            self.sink.emit(record);
        }
    }
}

/// Type-erased convenience alias for the common case: a sink that's either
/// a closure or an existing `dyn BookendSink` trait object, with no need to
/// re-view it through a different trait mid-lifetime. Callers that DO need
/// that (`src/pr_review.rs`'s `with_dispatch_bookends`) use the fully
/// generic [`BookendGuard`] with a concrete sink type instead.
pub type DynBookendGuard<'a> = BookendGuard<'a, dyn BookendSink + 'a>;

/// A short human label for a remote endpoint, for a dispatch record's
/// `payload.endpoint` field (e.g.
/// `azure:myorg.cognitiveservices.azure.com/gpt-4o`). `host` is the
/// endpoint's hostname only (never the scheme, path, or auth) — the SAME
/// string `MemberRecord.endpoint` and `dispatch_internal`'s own
/// `remote_endpoint_label` already carry. Matches
/// `dispatch_internal::remote_endpoint_label`'s exact shape byte-for-byte
/// (that function now delegates its formatting here) because the viewer's
/// session-detail route renderer parses this string by splitting on the
/// first `:` — a format drift here would silently break that view.
///
/// `kind` is `"azure"` when `host` contains `"azure"`, else `"openai"` —
/// the same heuristic `remote_endpoint_label` used against the full URL;
/// checking the host substring instead of the full URL is equivalent for
/// every endpoint host darkmux has seen in practice (an Azure deployment's
/// hostname always contains `azure`, e.g. `*.cognitiveservices.azure.com`
/// or `*.openai.azure.com`), and host is all this function is given.
pub fn remote_route_label(host: &str, model_id: &str) -> String {
    let kind = if host.contains("azure") { "azure" } else { "openai" };
    format!("{kind}:{host}/{model_id}")
}

/// Stamp `payload.endpoint` / `payload.remote_tokens` on a dispatch-record
/// payload — the canonical, single-source-of-truth shape every
/// remote-seat-aware dispatch bookend uses (`dispatch_internal.rs`'s
/// `dispatch_remote`/container path, and — the actual bug fix this
/// function exists for — `pr_review.rs`'s `with_dispatch_bookends`, which
/// previously stamped `remote_tokens` alone). `payload.endpoint` is the
/// ONLY field `crates/darkmux-serve/assets/viewer.html`'s `tokensOffMeter()`
/// reads to classify a session as cloud vs. local; a payload carrying
/// `remote_tokens` without it renders as 100% local savings even though
/// real cloud tokens were spent.
///
/// No-op per field when its argument is `None` — a fully-local dispatch
/// (no remote seat) calls this with `(None, None)` and the payload is left
/// byte-identical to not calling it at all, so this is safe to call
/// unconditionally.
pub fn stamp_remote_classification(
    payload: &mut serde_json::Value,
    endpoint_label: Option<&str>,
    remote_tokens: Option<u64>,
) {
    if let Some(ep) = endpoint_label {
        payload["endpoint"] = serde_json::json!(ep);
    }
    if let Some(tokens) = remote_tokens {
        payload["remote_tokens"] = serde_json::json!(tokens);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(action: &str, level: crate::Level) -> FlowRecord {
        FlowRecord {
            ts: crate::ts_utc_now(),
            level,
            category: crate::Category::Work,
            tier: crate::Tier::Local,
            stage: crate::Stage::Dispatch,
            action: action.to_string(),
            handle: "test".to_string(),
            sprint_id: None,
            session_id: Some("sess".to_string()),
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            orchestrator: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        }
    }

    #[derive(Default)]
    struct Recorder {
        actions: Vec<String>,
    }
    impl BookendSink for Recorder {
        fn emit(&mut self, record: FlowRecord) {
            self.actions.push(record.action);
        }
    }

    fn on_abort_for_test(id: &str, kind: &str) -> FlowRecord {
        rec(&format!("abort:{kind}:{id}"), crate::Level::Error)
    }

    #[test]
    fn open_then_close_disarms_and_drop_emits_nothing() {
        let mut sink = Recorder::default();
        {
            let mut guard = BookendGuard::new(&mut sink, on_abort_for_test);
            guard.open("dispatch", "dispatch", rec("start", crate::Level::Info));
            guard.close("dispatch", rec("complete", crate::Level::Info));
        }
        assert_eq!(sink.actions, vec!["start", "complete"]);
    }

    #[test]
    fn armed_guard_dropped_without_close_emits_abort_for_the_open_unit() {
        let mut sink = Recorder::default();
        {
            let mut guard = BookendGuard::new(&mut sink, on_abort_for_test);
            guard.open("dispatch", "dispatch", rec("start", crate::Level::Info));
            // dropped here without close()
        }
        assert_eq!(sink.actions, vec!["start", "abort:dispatch:dispatch"]);
    }

    #[test]
    fn nested_open_units_abort_innermost_first() {
        let mut sink = Recorder::default();
        {
            let mut guard = BookendGuard::new(&mut sink, on_abort_for_test);
            guard.open("task", "task", rec("task-start", crate::Level::Info));
            guard.open("probe", "dispatch", rec("probe-start", crate::Level::Info));
            guard.open("probe:fast", "dispatch", rec("probe:fast-start", crate::Level::Info));
            // dropped still armed — pops probe:fast, then probe, then task
        }
        assert_eq!(
            sink.actions,
            vec![
                "task-start",
                "probe-start",
                "probe:fast-start",
                "abort:dispatch:probe:fast",
                "abort:dispatch:probe",
                "abort:task:task",
            ]
        );
    }

    #[test]
    fn closing_all_units_leaves_the_guard_disarmed_even_with_extra_open_calls_interleaved() {
        let mut sink = Recorder::default();
        {
            let mut guard = BookendGuard::new(&mut sink, on_abort_for_test);
            guard.open("task", "task", rec("task-start", crate::Level::Info));
            guard.open("step", "dispatch", rec("step-start", crate::Level::Info));
            guard.close("step", rec("step-finish", crate::Level::Info));
            guard.close("task", rec("task-finish", crate::Level::Info));
        }
        assert_eq!(sink.actions, vec!["task-start", "step-start", "step-finish", "task-finish"]);
    }

    #[test]
    fn disarm_silences_drop_without_emitting_a_close_record() {
        let mut sink = Recorder::default();
        {
            let mut guard = BookendGuard::new(&mut sink, on_abort_for_test);
            guard.open("dispatch", "dispatch", rec("start", crate::Level::Info));
            guard.disarm();
        }
        assert_eq!(sink.actions, vec!["start"]);
    }

    #[test]
    fn panic_while_armed_still_fires_the_abort_record() {
        let mut sink = Recorder::default();
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = BookendGuard::new(&mut sink, on_abort_for_test);
            guard.open("dispatch", "dispatch", rec("start", crate::Level::Info));
            panic!("simulated mid-dispatch panic");
        }));
        std::panic::set_hook(prev_hook);
        assert!(result.is_err());
        assert_eq!(sink.actions, vec!["start", "abort:dispatch:dispatch"]);
    }

    #[test]
    fn sink_mut_allows_a_reborrow_that_survives_into_the_next_guard_call() {
        let mut sink = Recorder::default();
        let mut guard = BookendGuard::new(&mut sink, on_abort_for_test);
        guard.open("dispatch", "dispatch", rec("start", crate::Level::Info));
        {
            let reborrowed: &mut Recorder = guard.sink_mut();
            reborrowed.emit(rec("lent-out", crate::Level::Info));
        }
        guard.close("dispatch", rec("complete", crate::Level::Info));
        drop(guard);
        assert_eq!(sink.actions, vec!["start", "lent-out", "complete"]);
    }

    #[test]
    fn remote_route_label_matches_expected_shape() {
        assert_eq!(
            remote_route_label("myorg.cognitiveservices.azure.com", "gpt-4o"),
            "azure:myorg.cognitiveservices.azure.com/gpt-4o"
        );
        assert_eq!(remote_route_label("api.openai.com", "gpt-4o"), "openai:api.openai.com/gpt-4o");
    }

    #[test]
    fn stamp_remote_classification_sets_both_fields_when_present() {
        let mut payload = serde_json::json!({ "runtime": "funnel", "result_class": "ok" });
        stamp_remote_classification(&mut payload, Some("azure:host/model"), Some(42));
        assert_eq!(payload["endpoint"], "azure:host/model");
        assert_eq!(payload["remote_tokens"], 42);
    }

    #[test]
    fn stamp_remote_classification_no_op_when_both_none() {
        let mut payload = serde_json::json!({ "runtime": "funnel", "result_class": "ok" });
        let before = payload.clone();
        stamp_remote_classification(&mut payload, None, None);
        assert_eq!(payload, before);
    }
}
