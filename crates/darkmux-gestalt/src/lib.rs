//! darkmux-gestalt — the pure model-lifecycle planning core (#1274).
//!
//! Facts in, [`Plan`] out. The GestaltManager design settled on #1274 splits
//! model-lifecycle management into a pure planning core (this crate) and a
//! thin executor over host ports (packet 3): the core takes desired state
//! (crew staffing needs), observed residency, resource facts, the darkmux
//! ownership namespace, and the global AI RAM budget (#1243), and emits a
//! plan of Load/Unload/Reuse/Reconcile/Block actions — each carrying a typed
//! [`plan::Reason`] (so "why did darkmux unload X?" always has an answer) and
//! a machine-checkable [`plan::Precondition`] the executor re-verifies
//! immediately before executing (facts are snapshots; drift aborts-and-replans).
//!
//! Crate rules (enforced by review, honored by construction):
//!
//! - The pure core — everything outside [`ports`] and [`mock`] — performs
//!   zero I/O, zero clock reads, zero env reads. `std::process`, `std::fs`,
//!   and `std::time::SystemTime` are off-limits in this crate ([`ports::Deadline`]
//!   wraps a `Duration` bound, not a clock read).
//! - All fact and plan types are deterministic-ordered (`BTreeMap`, documented
//!   `Vec` orders) and totally comparable where the executor never needs to
//!   parse them back — plans are `Eq` and `Serialize`-only (see
//!   [`plan::OwnedTarget`]), so every behavior is one `assert_eq!` table row.
//! - No plan schema-version constant ships in packet 1: like the eureka
//!   RuleDefs precedent, the plan shape is engine-internal until a
//!   cross-process consumer exists. `Serialize` derives exist for run
//!   artifacts and debugging only; `Reason` variant names are not frozen.
//!
//! Absorption lineage: [`residency::decide_residency`] is a fact-typed port
//! of the review funnel's validated miniature (`darkmux-lab` funnel.rs,
//! PR #1275); [`ownership`] duplicates `darkmux_profiles::swap`'s canonical
//! ownership helpers under golden parity tests (the root-crate
//! `tests/gestalt_parity.rs` guards the duplication window until packet 3
//! re-points swap.rs at this crate — the #1271 one-definition discipline).

pub mod desired;
pub mod estimator;
pub mod facts;
pub mod mock;
pub mod ownership;
pub mod plan;
pub mod planner;
pub mod ports;
pub mod residency;

pub use desired::{ingest, DesiredEntry, Placement, Quarantined, QuarantineReason};
pub use estimator::{FixedEstimator, FootprintEstimator, V1Estimator};
pub use facts::{Budget, CallerIntent, CatalogFact, Facts, PoolFact, PoolId, Pools, ResidentFact};
pub use ownership::{ctx_sufficient, is_darkmux_owned, namespaced_identifier, DARKMUX_NAMESPACE};
pub use plan::{
    Action, EvictionOrder, ExecHint, ForeignTargetError, OwnedTarget, Plan, PlannedAction,
    Precondition, Reason, Warning,
};
pub use planner::{plan_acquire, plan_release, AcquireOpts, AcquireScope, ForeignPolicy};
pub use ports::{Deadline, HostError, LoadReport, ModelHost, ProbeError, ResourceProbe};
pub use residency::{decide_residency, ResidencyDecision};
