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
//!
//! **Physical tiering (#1352).** `builtins` (this module's `Tier 1`) is
//! generic and config-driven — every `Step` kind here reads its behavior
//! entirely from `Step.config`, with no per-mission control flow baked in.
//! `patterns` (`Tier 2`) holds genuinely new, reusable control-flow SHAPES
//! whose domain-specific algorithm plugs in as a caller-supplied strategy —
//! see that module's own doc. Tier 3 (genuinely bespoke, single-purpose
//! kinds) never lives in this crate at all: it stays physically co-located
//! with the mission module that owns it (`darkmux-lab`'s `review.rs`, the
//! `darkmux` binary's own `coder_phase.rs`) — see `step_kinds::patterns`'s
//! module doc for the full three-tier picture and `CLAUDE.md`'s "StepKind
//! tiering" section for the doctrine this physical layout enforces.

mod builtins;
pub mod patterns;
mod registry;
mod types;

pub use builtins::{
    parse_failed_verifiers, resolve_local_placement, DispatchInternalStepKind,
    DispatchMapStepKind, DispatchSingleShotStepKind, FailedVerifier, MapItemResult,
    ProceduralNoopStepKind, ProceduralShellStepKind,
};
pub use builtins::MAP_BUDGET_SKIP_ERROR;
pub use registry::StepKindRegistry;
pub use types::{
    MapDispatchOverride, MapRemoteBucket, OverrideDispatchCall, StepKind, StepOutcome, StepRunCtx,
    WaveSignal,
};

/// Re-exported so callers OUTSIDE this crate (e.g. `darkmux`'s own
/// `coder_phase` — the `run_step_graph`/`StepKind::residency` caller for
/// the mission-run migration, #1230 Packet 3) can name these types without
/// a direct `darkmux-gestalt` dependency of their own — only DIRECT
/// dependencies get an implicit extern-prelude entry, so a transitive user
/// needs a path through a crate they DO depend on. `FixedEstimator` is the
/// same "not yet meaningful" placeholder Packet 1's own production caller
/// (`review::inert_estimator`) uses — `Facts::default()` (no known
/// residents/pools) makes it structurally inert either way.
pub use darkmux_gestalt::{Facts, FixedEstimator, FootprintEstimator, Placement};
