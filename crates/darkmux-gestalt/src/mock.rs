//! Always-compiled synthetic [`ModelHost`] / [`ResourceProbe`] doubles.
//!
//! Deliberately NOT gated behind a `test-support` feature (the darkmux-types
//! #811 pattern gates config-tier isolation, a different concern): beyond
//! the packet-3 executor tests — which assert `mock.ops == vec![…]` as one
//! table row each — this mock is the synthetic host a future
//! `darkmux gestalt plan --dry-run` verb runs against, an operator-facing
//! surface rather than test plumbing. Keeping it in the shipped module tree
//! is that decision made explicitly, not by drift.

use crate::facts::{Budget, CatalogFact, Facts, Pools, ResidentFact};
use crate::plan::OwnedTarget;
use crate::ports::{Deadline, HostError, LoadReport, ModelHost, ProbeError, ResourceProbe};

/// Recorded host operation — `Eq`, so executor tests assert the full op
/// sequence with one `assert_eq!`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostOp {
    ListResident,
    ListCatalog,
    Load { model_key: String, identifier: String, min_ctx: u32 },
    Unload { identifier: String },
}

/// Scriptable in-memory host. `load` INSERTS a resident and `unload` removes
/// one, so multi-step executor tests see state evolve; unloading a
/// non-resident identifier returns [`HostError::NotResident`] — the mock
/// ENFORCES the #1279 invariant, so a plan that double-unloads fails a test,
/// not a live run.
#[derive(Debug, Default)]
pub struct MockHost {
    pub residents: Vec<ResidentFact>,
    pub catalog: Vec<CatalogFact>,
    /// Every call, in order.
    pub ops: Vec<HostOp>,
    /// Script the next load/unload to fail (drained on use) — the #1279
    /// second-unload shape and #1139 refusals in one line. The op is still
    /// recorded (the attempt happened).
    pub fail_next_load: Option<HostError>,
    pub fail_next_unload: Option<HostError>,
}

impl MockHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: seed one resident (host-reported order = call order).
    pub fn resident(
        mut self,
        identifier: &str,
        model_key: &str,
        ctx: u64,
        est_bytes: Option<u64>,
    ) -> Self {
        self.residents.push(ResidentFact {
            identifier: identifier.to_string(),
            model_key: model_key.to_string(),
            ctx,
            est_bytes,
        });
        self
    }

    /// Builder: seed one catalog entry.
    pub fn cataloged(mut self, model_key: &str, size_bytes: u64) -> Self {
        self.catalog.push(CatalogFact {
            model_key: model_key.to_string(),
            size_bytes: Some(size_bytes),
        });
        self
    }

    /// Facts snapshot straight off mock state — one line from mock to
    /// planner input. The catalog is always `Some` here (this host can
    /// enumerate it); build [`Facts`] by hand to model a catalog-less host.
    pub fn facts(&self, pools: Pools, budget: Budget) -> Facts {
        Facts {
            residents: self.residents.clone(),
            catalog: Some(self.catalog.clone()),
            pools,
            budget,
            utility_binding: None,
        }
    }
}

impl ModelHost for MockHost {
    fn list_resident(&mut self) -> Result<Vec<ResidentFact>, HostError> {
        self.ops.push(HostOp::ListResident);
        Ok(self.residents.clone())
    }

    fn list_catalog(&mut self) -> Result<Vec<CatalogFact>, HostError> {
        self.ops.push(HostOp::ListCatalog);
        Ok(self.catalog.clone())
    }

    fn load(
        &mut self,
        model_key: &str,
        identifier: &str,
        min_ctx: u32,
        _deadline: Deadline,
    ) -> Result<LoadReport, HostError> {
        self.ops.push(HostOp::Load {
            model_key: model_key.to_string(),
            identifier: identifier.to_string(),
            min_ctx,
        });
        if let Some(err) = self.fail_next_load.take() {
            return Err(err);
        }
        self.residents.push(ResidentFact {
            identifier: identifier.to_string(),
            model_key: model_key.to_string(),
            ctx: u64::from(min_ctx),
            est_bytes: None,
        });
        Ok(LoadReport { resolved_ctx: Some(u64::from(min_ctx)), ..Default::default() })
    }

    fn unload(&mut self, target: &OwnedTarget, _deadline: Deadline) -> Result<(), HostError> {
        self.ops.push(HostOp::Unload { identifier: target.identifier().to_string() });
        if let Some(err) = self.fail_next_unload.take() {
            return Err(err);
        }
        let before = self.residents.len();
        self.residents.retain(|r| r.identifier != target.identifier());
        if self.residents.len() == before {
            return Err(HostError::NotResident { identifier: target.identifier().to_string() });
        }
        Ok(())
    }
}

/// Trivial probe: fixed pools.
#[derive(Debug, Clone, Default)]
pub struct MockProbe(pub Pools);

impl ResourceProbe for MockProbe {
    fn pools(&mut self) -> Result<Pools, ProbeError> {
        Ok(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{PoolFact, PoolId};
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn deadline() -> Deadline {
        Deadline(Duration::from_secs(60))
    }

    #[test]
    fn mockhost_load_mutates_state() {
        let mut host = MockHost::new().cataloged("m", 5_000_000_000);
        host.load("m", "darkmux:m", 8_000, deadline()).expect("load succeeds");
        assert_eq!(
            host.ops,
            vec![HostOp::Load {
                model_key: "m".into(),
                identifier: "darkmux:m".into(),
                min_ctx: 8_000,
            }]
        );
        let residents = host.list_resident().expect("list succeeds");
        assert_eq!(
            residents,
            vec![ResidentFact {
                identifier: "darkmux:m".into(),
                model_key: "m".into(),
                ctx: 8_000,
                est_bytes: None,
            }],
            "a follow-up list_resident sees the new resident — multi-step executor tests see state evolve"
        );
        assert_eq!(host.ops.len(), 2, "the list call is recorded too");
    }

    #[test]
    fn mockhost_double_unload_is_typed_error() {
        // The mock enforces the #1279 invariant: a plan that unloads a
        // non-resident identifier fails a test, not a live run.
        let mut host = MockHost::new();
        let target = OwnedTarget::claim("darkmux:m", None).unwrap();
        let err = host.unload(&target, deadline()).unwrap_err();
        assert_eq!(err, HostError::NotResident { identifier: "darkmux:m".into() });
        assert_eq!(host.ops, vec![HostOp::Unload { identifier: "darkmux:m".into() }]);
    }

    #[test]
    fn mockhost_scripted_failures_drain() {
        let mut host = MockHost::new().resident("darkmux:m", "m", 8_000, None);
        host.fail_next_load = Some(HostError::Timeout {
            phase: "load",
            waited: Duration::from_secs(60),
        });
        let err = host.load("m2", "darkmux:m2", 8_000, deadline()).unwrap_err();
        assert!(matches!(err, HostError::Timeout { phase: "load", .. }));
        // Drained: the next load succeeds.
        host.load("m2", "darkmux:m2", 8_000, deadline()).expect("scripted failure drained");
        // Unload failure scripting mirrors it.
        host.fail_next_unload = Some(HostError::CommandFailed { detail: "boom".into() });
        let target = OwnedTarget::claim("darkmux:m", None).unwrap();
        assert!(host.unload(&target, deadline()).is_err());
        host.unload(&target, deadline()).expect("drained — the real resident unloads");
    }

    #[test]
    fn mock_probe_returns_fixed_pools_and_facts_snapshot() {
        let pools: Pools = BTreeMap::from([(
            PoolId("unified".into()),
            PoolFact { capacity_bytes: 32, available_bytes: 10 },
        )]);
        let mut probe = MockProbe(pools.clone());
        assert_eq!(probe.pools().unwrap(), pools);

        let host = MockHost::new()
            .resident("darkmux:m", "m", 8_000, Some(42))
            .cataloged("m", 42);
        let facts = host.facts(pools.clone(), Budget::default());
        assert_eq!(facts.residents, host.residents);
        assert_eq!(facts.catalog.as_deref(), Some(host.catalog.as_slice()));
        assert_eq!(facts.pools, pools);
    }
}
