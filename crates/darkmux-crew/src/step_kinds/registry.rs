//! `StepKindRegistry` — an owned, instance-scoped step-kind lookup.
//!
//! Mirrors `workloads::registry`'s mechanics (`Mutex<HashMap<String,
//! ...>>`, `register()` errors on a duplicate id, a not-found error names
//! what IS registered) but as a value the caller owns and passes by
//! reference, rather than a process-global `OnceLock` — see the module
//! doc on `step_kinds` for why.

use super::builtins::{
    DispatchInternalStepKind, DispatchMapStepKind, DispatchSingleShotStepKind,
    ProceduralNoopStepKind, ProceduralShellStepKind,
};
use super::types::StepKind;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct StepKindRegistry {
    kinds: Mutex<HashMap<String, Arc<dyn StepKind>>>,
}

impl StepKindRegistry {
    /// An empty registry — no kinds registered. Useful for tests that
    /// want a tightly-scoped set (e.g. only `procedural.noop`).
    pub fn new() -> Self {
        Self {
            kinds: Mutex::new(HashMap::new()),
        }
    }

    /// The registry `run_step_graph` uses in production: the four
    /// built-in kinds from `step_kinds::builtins`.
    pub fn with_builtins() -> Self {
        let registry = Self::new();
        registry
            .register(Arc::new(DispatchInternalStepKind))
            .expect("built-in step kind ids are unique by construction");
        registry
            .register(Arc::new(DispatchSingleShotStepKind))
            .expect("built-in step kind ids are unique by construction");
        registry
            .register(Arc::new(ProceduralShellStepKind))
            .expect("built-in step kind ids are unique by construction");
        registry
            .register(Arc::new(ProceduralNoopStepKind))
            .expect("built-in step kind ids are unique by construction");
        // (#1442) The generic map block — one single-shot per item of a
        // runtime collection. Tier 1: config-driven, no caller strategy.
        registry
            .register(Arc::new(DispatchMapStepKind))
            .expect("built-in step kind ids are unique by construction");
        registry
    }

    /// Register a step kind. Errors if a kind with the same id is
    /// already registered (calling-order programming bug — same
    /// contract as `workloads::registry::register`).
    pub fn register(&self, kind: Arc<dyn StepKind>) -> Result<()> {
        let mut map = self.kinds.lock().expect("step-kind registry poisoned");
        let id = kind.id().to_string();
        if map.contains_key(&id) {
            return Err(anyhow!("step kind already registered: {id}"));
        }
        map.insert(id, kind);
        Ok(())
    }

    /// (#1349) Register `kind` under an EXPLICIT `id`, bypassing
    /// `kind.id()` — for a legacy/retired id that must keep resolving to
    /// the SAME `StepKind` impl after a rename (a persisted `Step.kind`
    /// string from before the rename shipped, if anything ever re-reads
    /// it back through a registry, must not become "unknown step kind").
    /// Same duplicate-id guard as [`Self::register`]. `Arc::clone` is
    /// cheap — the caller registers the real instance once under its
    /// current `kind.id()`, then calls this once per legacy alias with a
    /// clone of the SAME `Arc`.
    pub fn register_alias(&self, id: &str, kind: Arc<dyn StepKind>) -> Result<()> {
        let mut map = self.kinds.lock().expect("step-kind registry poisoned");
        if map.contains_key(id) {
            return Err(anyhow!("step kind already registered: {id}"));
        }
        map.insert(id.to_string(), kind);
        Ok(())
    }

    /// Every registered step-kind id, sorted. (#1284 Packet 1) Lets a
    /// caller that only has REGISTRY ACCESS — not the registration call
    /// site — enumerate what's known, e.g. `darkmux doctor`'s mission-config
    /// check validating `Step.kind`/`StepConfig.kind` references against
    /// `StepKindRegistry::with_builtins()`'s Tier 1 ids. Deliberately does
    /// NOT see Tier 2/3 kinds registered ad hoc inside a mission builder
    /// (`build_review_graph`, `default_phase_graph`) — those register into
    /// their OWN per-call registry instance, never this shared one; a
    /// caller that only has `with_builtins()` structurally cannot know
    /// about them (see the mission-config doctor check's own doc for why
    /// an unknown Tier 3 id is a warning, not a failure).
    pub fn ids(&self) -> Vec<String> {
        let map = self.kinds.lock().expect("step-kind registry poisoned");
        let mut keys: Vec<String> = map.keys().cloned().collect();
        keys.sort();
        keys
    }

    /// Look up a step kind by id, returning an owned `Arc` clone —
    /// `'static` and `Send`, so the caller can move it into a
    /// `run_bounded` worker closure without holding the registry's
    /// lock across the thread boundary.
    pub fn get(&self, id: &str) -> Result<Arc<dyn StepKind>> {
        let map = self.kinds.lock().expect("step-kind registry poisoned");
        map.get(id).cloned().ok_or_else(|| {
            anyhow!(
                "unknown step kind: \"{id}\". Registered: {}",
                list_inner(&map)
            )
        })
    }
}

impl Default for StepKindRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn list_inner(map: &HashMap<String, Arc<dyn StepKind>>) -> String {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    if keys.is_empty() {
        "(none)".to_string()
    } else {
        keys.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step_kinds::StepOutcome;
    use crate::types::{Step, Task};
    use std::collections::BTreeMap;

    struct StubKind(&'static str);
    impl StepKind for StubKind {
        fn id(&self) -> &'static str {
            self.0
        }
        fn run(&self, _step: &Step, _task: &Task, _input: &BTreeMap<String, String>) -> Result<StepOutcome> {
            Ok(StepOutcome {
                output: "stub".to_string(),
                flow_records: Vec::new(),
            })
        }
    }

    /// The steps every registered kind is asked about below.
    fn conformance_step(kind: &str) -> Step {
        Step {
            id: "s-conf".to_string(),
            task_id: "t-conf".to_string(),
            gate: None,
            kind: kind.to_string(),
            status: crate::types::NodeStatus::Planned,
            config: serde_json::Value::Null,
            started_ts: None,
            completed_ts: None,
            output: None,
        }
    }

    /// (#1979) The session each registered kind MUST resolve, pinned by
    /// VALUE. Not `Option::is_some` — see the test below for why that
    /// distinction is the entire point of this table.
    ///
    /// `None` means the kind declares it never dispatches. Landing on that
    /// requires a deliberate two-sided edit (the override AND this row), so
    /// it cannot absorb a mistake.
    fn expected_session(kind_id: &str, step: &Step) -> Option<Option<String>> {
        Some(match kind_id {
            // Step-scoped: a solo dispatch owns its own session.
            "dispatch.internal" => Some(darkmux_types::session_id::step(&step.id)),
            // Task-scoped: sibling seats fanned out within one task share a
            // join key so a seat's tokens tie to its endpoint.
            "dispatch.single_shot" | "dispatch.map" => {
                Some(darkmux_types::session_id::task(&step.task_id))
            }
            // Declared no-dispatch.
            "procedural.shell" | "procedural.noop" => None,
            _ => return None,
        })
    }

    #[test]
    fn every_registered_kind_resolves_the_session_it_actually_emits_under() {
        // The structural half of #1979 — and it asserts the VALUE, which an
        // earlier version of this test did not.
        //
        // That earlier version asserted only `resolved.is_some()` for every
        // dispatching kind. Because `dispatch_session_id` has a trait
        // DEFAULT, the default answered on behalf of any kind that forgot to
        // override — with a STEP-scoped id. So a kind whose real convention
        // is task-scoped could lose its override and the test stayed green:
        // deleting `DispatchMapStepKind::dispatch_session_id` left 761
        // darkmux-crew and 331 darkmux-serve tests passing. It caught
        // "returns nothing" and missed "returns the wrong thing", which is
        // the failure that actually matters — a wrong session is claimed for
        // the mission and the real one is not.
        //
        // Iterating the REGISTRY is still the point: a sixth kind fails here
        // the moment it is registered, because `expected_session` will not
        // have a row for it. That is deliberate — a new kind must state its
        // convention in BOTH places, and the mismatch is what forces the
        // author to look at what the kind really emits.
        let registry = StepKindRegistry::with_builtins();
        for id in registry.ids() {
            let kind = registry.get(&id).expect("registry.ids() only yields registered kinds");
            let step = conformance_step(&id);
            let expected = expected_session(&id, &step).unwrap_or_else(|| {
                panic!(
                    "step kind `{id}` is registered but has no row in `expected_session`. \
                     Add one naming the session this kind ACTUALLY emits under (check its \
                     `run`/`run_streaming` in step_kinds::builtins), or `None` if it never \
                     dispatches. Do not guess from the trait default — the default is \
                     step-scoped, and a fan-out kind that shares a task-scoped join key \
                     would silently disagree with its own emission.",
                )
            });
            assert_eq!(
                kind.dispatch_session_id(&step),
                expected,
                "{id} resolves a different session than the one pinned in `expected_session`. \
                 A forward prediction that disagrees with the real emission claims the wrong \
                 session for a mission and leaves the real one unclaimed.",
            );
        }
    }

    #[test]
    fn an_explicit_config_session_wins_for_every_dispatching_kind() {
        // The caller-named session (review's seats, coder-phase's
        // `mission-run-` ids) must survive every kind's own convention —
        // claiming the wrong id reintroduces the ghost from the other side.
        let registry = StepKindRegistry::with_builtins();
        for id in registry.ids() {
            // Skip the declared no-dispatch kinds — they resolve `None`
            // whatever the config says, which is their whole contract.
            if expected_session(&id, &conformance_step(&id)) == Some(None) {
                continue;
            }
            let kind = registry.get(&id).unwrap();
            let mut step = conformance_step(&id);
            step.config = serde_json::json!({ "session_id": "caller-named" });
            assert_eq!(
                kind.dispatch_session_id(&step),
                Some("caller-named".to_string()),
                "{id} ignored an explicit config session_id",
            );
        }
    }

    #[test]
    fn register_and_lookup_basic() {
        let registry = StepKindRegistry::new();
        registry.register(Arc::new(StubKind("test.stub"))).unwrap();
        let kind = registry.get("test.stub").unwrap();
        assert_eq!(kind.id(), "test.stub");
    }

    #[test]
    fn double_register_errors() {
        let registry = StepKindRegistry::new();
        registry.register(Arc::new(StubKind("dup"))).unwrap();
        let err = registry.register(Arc::new(StubKind("dup"))).unwrap_err();
        assert!(err.to_string().contains("already registered"));
    }

    #[test]
    fn register_alias_resolves_a_legacy_id_to_the_same_kind() {
        let registry = StepKindRegistry::new();
        let kind = Arc::new(StubKind("review.probe:fast"));
        registry.register(kind.clone()).unwrap();
        registry.register_alias("funnel.probe:fast", kind).unwrap();
        assert_eq!(registry.get("review.probe:fast").unwrap().id(), "review.probe:fast");
        // The legacy id resolves to the SAME impl — its `.id()` still
        // reports the CURRENT id (kind.id() is a property of the impl,
        // not of which key found it), proving both keys point at one
        // instance rather than two independently-registered stubs.
        assert_eq!(registry.get("funnel.probe:fast").unwrap().id(), "review.probe:fast");
    }

    #[test]
    fn register_alias_errors_on_a_duplicate_id() {
        let registry = StepKindRegistry::new();
        registry.register(Arc::new(StubKind("taken"))).unwrap();
        let err = registry.register_alias("taken", Arc::new(StubKind("other"))).unwrap_err();
        assert!(err.to_string().contains("already registered"));
    }

    #[test]
    fn unknown_kind_errors_with_list() {
        let registry = StepKindRegistry::new();
        registry.register(Arc::new(StubKind("known"))).unwrap();
        // `Arc<dyn StepKind>` (the `Ok` type) isn't `Debug`, so
        // `unwrap_err()` (which requires `T: Debug`) doesn't apply here —
        // match it out instead.
        let err = match registry.get("ghost") {
            Ok(_) => panic!("expected an error for an unregistered id"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("unknown step kind"));
        assert!(msg.contains("known"));
    }

    #[test]
    fn with_builtins_registers_every_tier1_kind() {
        let registry = StepKindRegistry::with_builtins();
        for id in [
            "dispatch.internal",
            "dispatch.single_shot",
            "dispatch.map",
            "procedural.shell",
            "procedural.noop",
        ] {
            assert!(registry.get(id).is_ok(), "expected `{id}` to be registered");
        }
    }

    #[test]
    fn ids_lists_every_registered_kind_sorted() {
        let registry = StepKindRegistry::new();
        registry.register(Arc::new(StubKind("zebra"))).unwrap();
        registry.register(Arc::new(StubKind("alpha"))).unwrap();
        assert_eq!(registry.ids(), vec!["alpha".to_string(), "zebra".to_string()]);
    }

    #[test]
    fn ids_is_empty_for_a_fresh_registry() {
        assert!(StepKindRegistry::new().ids().is_empty());
    }

    #[test]
    fn with_builtins_ids_matches_the_known_tier_1_kinds() {
        let registry = StepKindRegistry::with_builtins();
        assert_eq!(
            registry.ids(),
            vec![
                "dispatch.internal".to_string(),
                "dispatch.map".to_string(),
                "dispatch.single_shot".to_string(),
                "procedural.noop".to_string(),
                "procedural.shell".to_string(),
            ]
        );
    }

    #[test]
    fn registries_are_independently_scoped() {
        // Two instances don't share state — unlike a hidden global
        // registry, registering "dup" in one doesn't collide with the
        // other. This is the whole point of the instance-scoped design.
        let a = StepKindRegistry::new();
        let b = StepKindRegistry::new();
        a.register(Arc::new(StubKind("shared-id"))).unwrap();
        b.register(Arc::new(StubKind("shared-id"))).unwrap();
        assert!(a.get("shared-id").is_ok());
        assert!(b.get("shared-id").is_ok());
    }
}
