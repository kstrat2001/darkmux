//! `StepKindRegistry` — an owned, instance-scoped step-kind lookup.
//!
//! Mirrors `workloads::registry`'s mechanics (`Mutex<HashMap<String,
//! ...>>`, `register()` errors on a duplicate id, a not-found error names
//! what IS registered) but as a value the caller owns and passes by
//! reference, rather than a process-global `OnceLock` — see the module
//! doc on `step_kinds` for why.

use super::builtins::{
    DispatchInternalStepKind, DispatchSingleShotStepKind, ProceduralNoopStepKind,
    ProceduralShellStepKind,
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
    fn with_builtins_registers_all_four() {
        let registry = StepKindRegistry::with_builtins();
        for id in [
            "dispatch.internal",
            "dispatch.single_shot",
            "procedural.shell",
            "procedural.noop",
        ] {
            assert!(registry.get(id).is_ok(), "expected `{id}` to be registered");
        }
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
