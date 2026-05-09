//! Provider registry. Providers register themselves at startup via a
//! single explicit call from `main`. Storing `Box<dyn WorkloadProvider>`
//! behind a `OnceLock<Mutex<...>>` keeps it simple and test-friendly.

use crate::workloads::types::WorkloadProvider;
use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

type Registry = Mutex<HashMap<String, Box<dyn WorkloadProvider>>>;

fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a provider. Errors if a provider with the same id is already
/// registered (calling-order programming bug).
pub fn register(provider: Box<dyn WorkloadProvider>) -> Result<()> {
    let mut map = registry().lock().expect("registry poisoned");
    let id = provider.id().to_string();
    if map.contains_key(&id) {
        return Err(anyhow!("provider already registered: {id}"));
    }
    map.insert(id, provider);
    Ok(())
}

/// Look up a provider by id. Returns a clone of the trait-object's id+description
/// plus a closure-style invoker that holds the registry lock long enough to call
/// the provider's methods. For darkmux's single-threaded CLI use, just return
/// the box's reference under the lock.
pub fn with_provider<R>(id: &str, f: impl FnOnce(&dyn WorkloadProvider) -> R) -> Result<R> {
    let map = registry().lock().expect("registry poisoned");
    let p = map
        .get(id)
        .ok_or_else(|| anyhow!("unknown workload provider: \"{id}\". Registered: {}", list_inner(&map)))?;
    Ok(f(p.as_ref()))
}

/// Snapshot of registered provider ids and descriptions, for `darkmux lab providers`.
pub fn list() -> Vec<(String, String)> {
    let map = registry().lock().expect("registry poisoned");
    map.iter()
        .map(|(k, v)| (k.clone(), v.description().to_string()))
        .collect()
}

fn list_inner(map: &HashMap<String, Box<dyn WorkloadProvider>>) -> String {
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
    use crate::types::Profile;
    use crate::workloads::types::{
        InspectionReport, LoadedWorkload, RunResult, VerifyOutcome,
    };
    use std::path::Path;

    /// A minimal stub provider for tests. We use a unique id per test so
    /// the global registry doesn't collide across tests.
    struct StubProvider(&'static str);
    impl WorkloadProvider for StubProvider {
        fn id(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            "stub for tests"
        }
        fn setup(&self, _: &LoadedWorkload, _: &Path, _: &Path) -> Result<()> {
            Ok(())
        }
        fn run(
            &self,
            _: &LoadedWorkload,
            _: &Path,
            _: &Path,
            _: &Profile,
            _: &str,
        ) -> Result<RunResult> {
            Ok(RunResult {
                ok: true,
                duration_ms: 1,
                payload_text: Some("stub".into()),
                trajectory_path: None,
                verify: Some(VerifyOutcome {
                    passed: true,
                    details: "stub".into(),
                }),
                error: None,
            })
        }
        fn inspect(&self, _: &LoadedWorkload, _: &Path) -> Result<InspectionReport> {
            Ok(InspectionReport::default())
        }
    }

    #[test]
    fn register_and_lookup_basic() {
        let id = "test-stub-basic";
        register(Box::new(StubProvider(id))).unwrap();
        let desc = with_provider(id, |p| p.description().to_string()).unwrap();
        assert_eq!(desc, "stub for tests");
    }

    #[test]
    fn double_register_errors() {
        let id = "test-stub-dup";
        register(Box::new(StubProvider(id))).unwrap();
        let err = register(Box::new(StubProvider(id))).unwrap_err();
        assert!(err.to_string().contains("already registered"));
    }

    #[test]
    fn unknown_provider_errors_with_list() {
        let _ = register(Box::new(StubProvider("test-stub-list"))); // ok if dup
        let err = with_provider("definitely-not-real", |_| ()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown workload provider"));
        assert!(msg.contains("Registered:"));
    }

    #[test]
    fn list_returns_registered() {
        let id = "test-stub-list-fn";
        let _ = register(Box::new(StubProvider(id)));
        let listed = list();
        assert!(listed.iter().any(|(k, _)| k == id));
    }
}
