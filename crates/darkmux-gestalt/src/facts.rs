//! Observed-world + standing-declaration inputs to the pure core.
//!
//! Facts are SNAPSHOTS (#1274): the planner reasons over them without
//! re-probing, and the packet-3 executor re-verifies each action's
//! [`crate::plan::Precondition`] against live host state immediately before
//! executing, aborting-and-replanning on drift.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One resident model instance as reported by the host (the packet-2 adapter
/// source is `lms ps --json` → `darkmux_types::LoadedModel`).
///
/// `Vec<ResidentFact>` ORDER IS DECISION-BEARING: it is the host-reported
/// order, [`crate::residency::decide_residency`] is first-match-wins, and
/// budget eviction walks it deterministically (#1243). Adapters MUST NOT
/// sort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResidentFact {
    /// Host-visible instance identifier (`darkmux:qwen…` or user state).
    pub identifier: String,
    /// Catalog model key — the weights identity residency matches on
    /// (`LoadedModel.model`, the `lms ps` modelKey fallback chain).
    pub model_key: String,
    /// Loaded context window. 0 = unknown → compares as a tiny load, which
    /// at worst reconciles (never silently reuses the wrong ctx — the
    /// #1135 direction of caution).
    pub ctx: u64,
    /// Adapter-estimated resident footprint in bytes. `None` = unknown —
    /// surfaces [`crate::plan::Warning::ResidentBytesUnknown`] under an
    /// active budget and counts as 0 against the cap (documented loud
    /// degradation, never a panic). `LoadedModel.size` is a DISPLAY string
    /// today; the packet-2 adapter reads raw `sizeBytes` where available.
    pub est_bytes: Option<u64>,
}

/// One catalog entry (packet-2 adapter source: `lms ls --json`). The
/// existence fact behind the #1276 unknown-model fast-fail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogFact {
    pub model_key: String,
    /// On-disk weights size — the [`crate::estimator::V1Estimator`]'s base
    /// term (#1274 estimation decision).
    pub size_bytes: Option<u64>,
}

/// Named memory pool (#1274 pools-as-data): "unified" on Apple Silicon;
/// "system-ram" + "gpu0-vram" on a CUDA box. `BTreeMap` keying ⇒
/// deterministic iteration ⇒ deterministic plans. The core never branches
/// on platform — only on pool math.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PoolId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolFact {
    pub capacity_bytes: u64,
    pub available_bytes: u64,
}

pub type Pools = BTreeMap<PoolId, PoolFact>;

/// Global AI RAM budget (#1243). Counts ONLY darkmux-owned residents — user
/// loads never count against the cap (cross-checking total physical pressure
/// is a doctor/observability concern layered on top, never a core decision
/// input). The config source (`runtime.max_model_ram_gb`) is resolved by the
/// caller; the core sees bytes. `None` = no budget configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Budget {
    pub max_darkmux_bytes: Option<u64>,
}

/// Who is asking (#1243): auto-triggered paths (cycler/JIT/scheduler) never
/// breach the budget — they evict, serialize, or refuse. Operator-explicit
/// commands (e.g. `darkmux swap`) warn-and-proceed: operator intent wins,
/// loudly (operator sovereignty, #44).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallerIntent {
    Auto,
    OperatorExplicit,
}

/// The complete input world for the pure core: observed host state
/// (residents, catalog, pools) plus the two standing operator declarations
/// planning needs (budget, utility binding). A SNAPSHOT — see module docs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Facts {
    pub residents: Vec<ResidentFact>,
    /// `None` = catalog unavailable (older `lms` without `ls --json`, probe
    /// failure) → the #1276 existence fast-fail is SKIPPED, not failed —
    /// the leniency contract; the bounded [`crate::ports::Deadline`] still
    /// backstops execution.
    pub catalog: Option<Vec<CatalogFact>>,
    pub pools: Pools,
    pub budget: Budget,
    /// The registry's standing utility-model binding (`internal.utility`),
    /// resolved by the caller to its namespaced identifier; `None` when
    /// unconfigured. Exclusive-scope planning uses it for the #1280 guard:
    /// a pass-1 unload that would evict this identifier additionally emits
    /// [`crate::plan::Warning::UtilityBindingEvicted`], so a swap-shaped
    /// caller that forgot to include the utility seat cannot silently evict
    /// the compactor.
    pub utility_binding: Option<String>,
}
