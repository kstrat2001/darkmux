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

    /// Every registered step-kind id, sorted. (#1284 Packet 1) Lets a
    /// caller that only has REGISTRY ACCESS — not the registration call
    /// site — enumerate what's known, e.g. `darkmux doctor`'s mission-config
    /// check validating `Step.kind`/`StepConfig.kind` references against
    /// `StepKindRegistry::with_builtins()`'s Tier 1 ids. Deliberately does
    /// NOT see Tier 2/3 kinds registered ad hoc inside a mission builder
    /// (`src/mission_launch.rs`'s `register_coder_phase_kinds` is the live
    /// example today; `build_review_graph` and `default_phase_graph` did
    /// the same before both were deleted, #2310 P4d) — those register into
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

    /// Every registered kind's id and DATA ports, for
    /// `MissionConfig::validate_with` (#2312). Artifact ports are run-scoped
    /// shared state, not hand-offs between tasks, so they are left out.
    pub fn catalog(&self) -> crate::mission_config::KindCatalog {
        let map = self.kinds.lock().expect("step-kind registry poisoned");
        let data = |ports: &[super::Port]| -> Vec<String> {
            ports.iter().filter(|p| matches!(p.kind, super::PortKind::Data)).map(|p| p.name.to_string()).collect()
        };
        let mut catalog = crate::mission_config::KindCatalog::default();
        for (id, kind) in map.iter() {
            catalog.insert(id, crate::mission_config::KindPorts { requires: data(kind.requires()), provides: data(kind.provides()) });
        }
        catalog
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
    use darkmux_types::session_id::SessionScope;
    use std::collections::BTreeMap;

    use super::super::types::{SeatClaim, StepRunCtx};

    struct StubKind(&'static str);
    impl StepKind for StubKind {
        /// (#2394) [`SeatClaim::NoModel`] — this kind is a test stub; it
    /// dispatches nothing. Bounded by `runtime.dispatch_free_concurrency`
    /// and, per command, by `runtime.step_command_timeout_seconds` — never
    /// by the hosted-endpoint cap.
    fn seat(
        &self,
        _step: &Step,
        _task: &Task,
        _input: &std::collections::BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> SeatClaim {
        SeatClaim::NoModel
    }

    fn id(&self) -> &'static str {
            self.0
        }

        fn run(&self, _step: &Step, _task: &Task, _input: &BTreeMap<String, String>, _ctx: &StepRunCtx) -> Result<StepOutcome> {
            Ok(StepOutcome {
                output: "stub".to_string(),
                flow_records: Vec::new(),
                degraded: None,
            })
        }
    }

    /// (#1979) The session scope each registered kind MUST declare, pinned
    /// by VALUE. `None` in the outer `Option` means the kind has no row.
    fn expected_scope(kind_id: &str) -> Option<SessionScope> {
        Some(match kind_id {
            // Step-scoped: a solo dispatch owns its own session.
            "dispatch.internal" => SessionScope::Step,
            // Task-scoped: sibling seats fanned out within one task share a
            // join key so a seat's tokens tie to its endpoint.
            "dispatch.single_shot" | "dispatch.map" => SessionScope::Task,
            // Declared no-dispatch.
            "procedural.shell" | "procedural.noop" => SessionScope::None,
            _ => return None,
        })
    }

    /// The scheduling class a kind's seat claim puts it in, for pinning by
    /// value. Matched exhaustively: a new `SeatClaim` variant is a compile
    /// error here, not a silently unclassified seat.
    #[derive(Debug, PartialEq, Eq)]
    enum SeatClass {
        Local,
        Remote,
        NoModel,
        Unresolved,
    }

    impl SeatClass {
        fn of(claim: &SeatClaim) -> Self {
            match claim {
                SeatClaim::LocalModel(_) => SeatClass::Local,
                SeatClaim::RemoteEndpoint => SeatClass::Remote,
                SeatClaim::NoModel => SeatClass::NoModel,
                SeatClaim::LocalModelUnresolved { .. } => SeatClass::Unresolved,
            }
        }
    }

    /// The seat class each crew-crate step kind MUST claim, with a
    /// representative step config where the answer depends on it. The kinds
    /// that run no model (`procedural.*`, `mods.gate`, `records.gather`,
    /// `deliver.github_review`) must be `NoModel`: claiming `Remote` puts them
    /// behind the hosted-endpoint cap, which a mission launch sets to 1, so
    /// independent shell and gate steps run one at a time (#2394).
    fn expected_seat(kind_id: &str) -> Option<(SeatClass, serde_json::Value)> {
        use serde_json::json;
        let endpoint = json!({ "url": "https://h.example/v1" });
        Some(match kind_id {
            "procedural.shell" | "procedural.noop" | "mods.gate" | "records.gather" | "deliver.github_review" => {
                (SeatClass::NoModel, json!({}))
            }
            "dispatch.single_shot" => (SeatClass::Remote, json!({ "model": "m", "user": "hi", "endpoint": endpoint })),
            "dispatch.map" => (
                SeatClass::Remote,
                json!({ "model": "m", "user_template": "check {item}", "collection": ["a"], "endpoint": endpoint }),
            ),
            // No role on the task or in config: nothing to resolve.
            "dispatch.internal" => (SeatClass::Unresolved, json!({})),
            _ => return None,
        })
    }

    #[test]
    fn every_crew_step_kind_claims_the_seat_class_pinned_for_it() {
        let registry = StepKindRegistry::with_builtins();
        for extra in [
            Arc::new(super::super::mods_gate::ModsGateStepKind) as Arc<dyn StepKind>,
            Arc::new(super::super::records_gather::RecordsGatherStepKind),
            Arc::new(super::super::deliver_github_review::DeliverGithubReviewStepKind),
        ] {
            registry.register(extra).expect("distinct ids");
        }
        assert_eq!(registry.ids().len(), 8, "five builtins plus the three dispatch-free crew kinds: {:?}", registry.ids());
        for id in registry.ids() {
            let (expected, config) = expected_seat(&id).unwrap_or_else(|| {
                panic!("step kind `{id}` is registered but has no row in `expected_seat`; state whether it claims a model")
            });
            let kind = registry.get(&id).unwrap();
            let step = Step {
                id: "s1".to_string(),
                task_id: "t1".to_string(),
                gate: None,
                kind: id.clone(),
                status: crate::types::NodeStatus::Planned,
                config,
                started_ts: None,
                completed_ts: None,
                output: None,
            };
            let task = Task {
                run_on: crate::types::default_run_on(),
                id: "t1".to_string(),
                phase_id: "p1".to_string(),
                description: "t".to_string(),
                display_name: None,
                step_ids: vec!["s1".to_string()],
                depends_on: Vec::new(),
                reads: Vec::new(),
                role_id: None,
                profile_name: None,
                workdir: None,
                image: None,
            };
            let claim = kind.seat(&step, &task, &BTreeMap::new(), &StepRunCtx::for_test());
            assert_eq!(SeatClass::of(&claim), expected, "`{id}` claims a different seat class than the one pinned");
        }
    }

    #[test]
    fn every_registered_kind_declares_the_session_scope_it_emits_under() {
        // Asserts the VALUE: `session_scope` has a trait DEFAULT (step), so
        // asserting only that a kind answers would let a task-scoped kind
        // lose its override and stay green. Iterating the REGISTRY makes a
        // new kind fail here until it has a row: a new kind states its
        // convention in both places, and the mismatch is what makes its
        // author look at what the kind really emits. The kinds build their
        // sessions from this same declaration (`StepRunCtx::session`), so
        // a pinned declaration is a pinned emission.
        let registry = StepKindRegistry::with_builtins();
        for id in registry.ids() {
            let kind = registry.get(&id).expect("registry.ids() only yields registered kinds");
            let expected = expected_scope(&id).unwrap_or_else(|| {
                panic!(
                    "step kind `{id}` is registered but has no row in `expected_scope`. Add one \
                     naming the session this kind ACTUALLY emits under (check its `run` in \
                     step_kinds::builtins), or `SessionScope::None` if it never dispatches.",
                )
            });
            assert_eq!(kind.session_scope(), expected, "{id} declares a different scope than the one pinned");
        }
    }

    /// (#2577) The `cwd_policy()` every registered Tier 1 kind MUST
    /// report, pinned by VALUE — mirrors `expected_scope`'s own
    /// discipline immediately above (and its own doc's warning: because
    /// `cwd_policy` has a trait DEFAULT, asserting only "is a value
    /// present" would prove nothing — every kind already has one, for
    /// free, whether or not anyone thought about it).
    ///
    /// `None` here would mean "a kind is registered with no row" — landing
    /// there requires forgetting to add a row for a brand new kind, which
    /// is caught by the panic in the test below rather than silently
    /// defaulting to "safe".
    fn expected_cwd_policy(kind_id: &str) -> Option<super::super::types::CwdPolicy> {
        use super::super::types::CwdPolicy;
        Some(match kind_id {
            // The one kind allowed to fall back to the process's own
            // ambient working directory — see `CwdPolicy::
            // AmbientWithRefusal`'s own doc for why it is safe here (a
            // documented resolve+refuse chain) and nowhere else.
            "procedural.shell" => CwdPolicy::AmbientWithRefusal,
            // Every other Tier 1 kind either dispatches to a model (never
            // touching the local filesystem's ambient directory) or, for
            // `procedural.noop`, spawns no subprocess at all.
            "dispatch.internal" | "dispatch.single_shot" | "dispatch.map" | "procedural.noop" => {
                CwdPolicy::NoAmbientDependency
            }
            _ => return None,
        })
    }

    #[test]
    fn with_builtins_has_exactly_one_kind_with_ambient_cwd_fallback() {
        // Iterating the REGISTRY is the point, same as the session test
        // above: a sixth Tier 1 kind fails HERE, the moment it is
        // registered, because `expected_cwd_policy` will not have a row
        // for it — forcing whoever adds it to state, in this table,
        // whether it is safe to depend on the ambient directory.
        //
        // This test does NOT see Tier 2/3 kinds — those are registered by
        // their own missions in other crates, with no single shared
        // registry across all of them. An exhaustive sweep of every
        // non-test `impl StepKind for` in the workspace (a review finding:
        // the original #2577 sweep undercounted at 19; the reproducible
        // count is FIFTEEN) finds ten such kinds: `mods.gate`, the two
        // crawl planners (`crawl.plan`, `plan.sites`), the two crawl unit
        // kinds (`dispatch.unit`, `dispatch.summary`), the three `mission.*`
        // kinds (`mission.worktree`, `mission.coder`, `mission.verify`),
        // `deliver.github_review`, and `records.gather`. Every one of
        // them now carries its OWN explicit `cwd_policy()` override (a
        // review finding: three of them — `mission.coder`,
        // `deliver.github_review`, `records.gather` — previously carried
        // none at all, silently inheriting the trait default with no row
        // recording that as a checked audit; `dispatch.unit`/`dispatch.summary`
        // were absent from this comment's roster entirely). See each
        // kind's own `cwd_policy` doc for its audit.
        //
        // `mods.gate` requires and validates an explicit `config.workdir`
        // (never falls through to ambient); the crawl planners' only
        // ambient-adjacent call (`workspace_spec::materialize`'s
        // first-clone `git clone --bare` with no `.current_dir()` set) was
        // probed live from a deleted process cwd and reads clean at exit
        // 0 — but NOT, as first recorded, because "`git` invoked directly
        // never consults the ambient directory": a repeat probe (a bare
        // `Command::new("git")`, no shell, matching `run_git` exactly)
        // shows `git` spawns its own internal shell regardless, and that
        // shell's `shell-init: error retrieving current directory`
        // reaches stderr even on a clean exit. The clone is actually safe
        // because `resolve_one` now REFUSES a relative `path`-origin
        // unconditionally (see that function's own doc) — structural, not
        // resting on the shell claim. The `mission.*` kinds in darkmux's
        // own `coder_phase` module always pass an explicitly-resolved
        // worktree path (`mission.coder` via `crew::dispatch`'s own
        // `workdir`); `deliver.github_review` and `records.gather` spawn
        // no subprocess at all.
        let registry = StepKindRegistry::with_builtins();
        for id in registry.ids() {
            let kind = registry.get(&id).expect("registry.ids() only yields registered kinds");
            let expected = expected_cwd_policy(&id).unwrap_or_else(|| {
                panic!(
                    "step kind `{id}` is registered but has no row in `expected_cwd_policy`. \
                     Add one naming whether this kind can ever spawn a subprocess in the \
                     process's own ambient working directory — check its `run`/`run_streaming` \
                     for a `Command`/subprocess spawn with no explicitly-resolved directory. \
                     Do not guess from the trait default (`NoAmbientDependency`) without \
                     checking: the default exists so kinds that never touch a filesystem don't \
                     have to say so, not to let a genuine subprocess spawn go unexamined.",
                )
            });
            let actual = kind.cwd_policy();
            assert_eq!(
                actual, expected,
                "{id} reports cwd_policy() == {actual:?}, but `expected_cwd_policy` pins \
                 {expected:?}. If you just gave a SECOND kind `AmbientWithRefusal`, give it \
                 `procedural.shell`'s own resolve+refuse chain (`builtins::resolve_shell_cwd`) \
                 rather than a fresh copy, and add it to this table deliberately -- don't let \
                 the mismatch alone be what tells you.",
            );
        }
        let ambient: Vec<String> = registry
            .ids()
            .into_iter()
            .filter(|id| registry.get(id).unwrap().cwd_policy() == super::super::types::CwdPolicy::AmbientWithRefusal)
            .collect();
        assert_eq!(
            ambient,
            vec!["procedural.shell".to_string()],
            "exactly one built-in kind may depend on the process's own ambient working \
             directory; found: {ambient:?}",
        );
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
