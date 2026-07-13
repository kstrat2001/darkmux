//! Step-kind registry (#1230 Packet 2).
//!
//! A `Step`'s `kind` field is a registered id (e.g. `"dispatch.internal"`)
//! resolved through this module at scheduling time — mirrors
//! `darkmux-lab`'s `WorkloadProvider` pattern (`workloads::types::
//! WorkloadProvider` trait + `workloads::registry`'s `Mutex<HashMap<...>>`
//! + `register_builtins()`), adapted for the scheduler's needs:
//!
//! - **Instance, not a hidden global.** `WorkloadProvider`'s registry is a
//!   process-wide `OnceLock`, which forces every test to mint a
//!   collision-free unique id (see `workloads::registry`'s test module).
//!   `StepKindRegistry` here is a plain owned value the caller constructs
//!   (`StepKindRegistry::with_builtins()` for the four built-ins, or
//!   `StepKindRegistry::new()` + `register()` for a test-scoped subset) —
//!   `run_step_graph` takes `&StepKindRegistry` as an explicit parameter,
//!   so each test gets its own isolated registry instead of sharing
//!   process-global state. The internal shape (`Mutex<HashMap<String,
//!   Arc<dyn StepKind>>>`) still mirrors the workloads registry's
//!   mechanics.
//! - **`Arc`, not `Box`.** A `Step` job dispatched through Packet 1's
//!   `run_bounded` runs inside a spawned `thread::scope` worker and must
//!   be `'static` — a `Box<dyn StepKind>` borrowed out from behind a
//!   `MutexGuard` can't outlive the lookup call. Storing `Arc<dyn
//!   StepKind>` lets the scheduler clone an owned, `Send + Sync` handle
//!   into the job closure.

mod builtins;
mod registry;
mod types;

pub use builtins::{
    parse_failed_verifiers, resolve_local_placement, DispatchInternalStepKind,
    DispatchSingleShotStepKind, FailedVerifier, ProceduralNoopStepKind, ProceduralShellStepKind,
};
pub use registry::StepKindRegistry;
pub use types::{StepKind, StepOutcome};

/// Re-exported so callers OUTSIDE this crate (e.g. `darkmux`'s own
/// `mission_run` — the `run_step_graph`/`StepKind::residency` caller for
/// the mission-run migration, #1230 Packet 3) can name these types without
/// a direct `darkmux-gestalt` dependency of their own — only DIRECT
/// dependencies get an implicit extern-prelude entry, so a transitive user
/// needs a path through a crate they DO depend on. `FixedEstimator` is the
/// same "not yet meaningful" placeholder Packet 1's own production caller
/// (`review::inert_estimator`) uses — `Facts::default()` (no known
/// residents/pools) makes it structurally inert either way.
pub use darkmux_gestalt::{Facts, FixedEstimator, FootprintEstimator, Placement};
