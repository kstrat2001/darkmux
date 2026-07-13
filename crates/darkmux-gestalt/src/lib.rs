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
//! Packet 2a adds two more pure surfaces on the same facts: the co-residency
//! wave scheduler ([`waves`], #1285 — budget-driven parallel↔sequential
//! realignment, the #1243 budget doubling as a hardware-tier emulator) and
//! the architecture-aware footprint estimator ([`estimator::ArchEstimator`],
//! #1286 — '4B doesn't mean 4GB').
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
//! of the review's validated miniature (`darkmux-lab` review.rs,
//! PR #1275); [`ownership`] duplicates `darkmux_profiles::swap`'s canonical
//! ownership helpers under golden parity tests (the root-crate
//! `tests/gestalt_parity.rs` guards the duplication window until packet 3
//! re-points swap.rs at this crate — the #1271 one-definition discipline).
//!
//! Namespace ownership is ABSOLUTE (operator decision, 2026-07-10, #1274):
//! every planned load/unload/reconcile targets only `darkmux:*` instances
//! (plus a placement's own explicit alias), measurement counts only the
//! namespaced subset, and non-namespaced residents are user state — visible
//! to the planner as pool consumption only, structurally unnameable in plan
//! actions ([`plan::OwnedTarget`] has no foreign constructor). This
//! supersedes the #408-derived preflight behavior of reusing/unloading
//! foreign residents; the named per-path behavior changes are documented in
//! the planner module docs ("Cutover behavior changes").

pub mod desired;
pub mod estimator;
pub mod facts;
pub mod mock;
pub mod ownership;
pub mod plan;
pub mod planner;
pub mod ports;
pub mod residency;
pub mod waves;

pub use desired::{ingest, DesiredEntry, Placement, Quarantined, QuarantineReason};
pub use estimator::{
    ArchEstimator, ArchFacts, FixedEstimator, FootprintEstimator, V1Estimator,
    DEFAULT_TRANSIENT_MARGIN_BYTES,
};
pub use facts::{Budget, CallerIntent, CatalogFact, Facts, PoolFact, PoolId, Pools, ResidentFact};
pub use ownership::{ctx_sufficient, is_darkmux_owned, namespaced_identifier, DARKMUX_NAMESPACE};
pub use plan::{
    Action, EvictionOrder, ExecHint, ForeignTargetError, OwnedTarget, Plan, PlannedAction,
    Precondition, Reason, Warning,
};
pub use planner::{plan_acquire, plan_release, AcquireOpts, AcquireScope};
pub use ports::{Deadline, HostError, LoadReport, ModelHost, ProbeError, ResourceProbe};
pub use residency::{decide_residency, ResidencyDecision};
pub use waves::{plan_waves, ForceParallelRefused, WaveMode, WaveRefusal, WaveSchedule};
