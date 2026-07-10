//! Host ports — the seam between the pure core and the world.
//!
//! Packet 2 ships the real adapters (`LmsHost` over the `lms` CLI in
//! darkmux-profiles; a macOS `ResourceProbe` presenting one "unified" pool).
//! Remote endpoints NEVER get a [`ModelHost`]: they are quarantined at
//! ingest ([`crate::desired::QuarantineReason::RemoteEndpoint`]), preserving
//! the #1177 residency-free design instead of a meaningless no-op adapter.

use crate::facts::{CatalogFact, Pools, ResidentFact};
use crate::plan::OwnedTarget;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

/// Mandatory bounded wait on every mutating host call — the #1276 lesson
/// (the current `lms load` wrapper blocks indefinitely on a child process;
/// an unknown model id can hang it forever). A port implementor cannot
/// forget the bound: it is a required parameter, deliberately NOT
/// `Option<Duration>` — an unbounded-looking "host default" variant is the
/// #1276 re-introduction foot-gun. Packet 2 resolves the value from a
/// visible `runtime.model_load_timeout_seconds` config field (the
/// visible-defaults doctrine); this type carries the resolved bound, never
/// reads a clock itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline(pub Duration);

impl Deadline {
    pub fn from_secs(secs: u64) -> Self {
        Deadline(Duration::from_secs(secs))
    }
}

/// Typed, `Eq` host errors ⇒ executor failure paths are table-testable too.
/// Deliberately NOT anyhow: these are data the executor matches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    /// The #1276 hang, surfaced loud with the phase named.
    Timeout { phase: &'static str, waited: Duration },
    UnknownModel { model_key: String },
    /// The #1139 fast-fail shape (insufficient host resources), structured.
    InsufficientResources { detail: String },
    /// Unload of a non-resident identifier — the #1279 failure shape,
    /// typed instead of an opaque stderr bail.
    NotResident { identifier: String },
    CommandFailed { detail: String },
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Timeout { phase, waited } => {
                write!(f, "host call timed out (phase: {phase}, waited {waited:?})")
            }
            HostError::UnknownModel { model_key } => {
                write!(f, "host does not know model \"{model_key}\"")
            }
            HostError::InsufficientResources { detail } => {
                write!(f, "host refused: insufficient resources ({detail})")
            }
            HostError::NotResident { identifier } => {
                write!(f, "\"{identifier}\" is not resident — nothing to unload")
            }
            HostError::CommandFailed { detail } => write!(f, "host command failed: {detail}"),
        }
    }
}

impl std::error::Error for HostError {}

/// Host-reported post-action load configuration (#1257 provenance: resolved
/// ctx, quant, flash-attention, KV quant, GPU layout as the host exposes
/// them). Lenient `extras` overflow: `PartialEq` only (`serde_json::Value`
/// carries floats) — deliberately NOT part of Plan equality.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LoadReport {
    pub resolved_ctx: Option<u64>,
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// The model-lifecycle port. `unload` accepts only a claim-checked
/// [`OwnedTarget`] — a foreign identifier cannot reach the host through this
/// seam (the #1284 namespace contract, structural).
pub trait ModelHost {
    fn list_resident(&mut self) -> Result<Vec<ResidentFact>, HostError>;
    /// Catalog facts for the #1276 existence fast-fail + the estimator's
    /// base term.
    fn list_catalog(&mut self) -> Result<Vec<CatalogFact>, HostError>;
    fn load(
        &mut self,
        model_key: &str,
        identifier: &str,
        min_ctx: u32,
        deadline: Deadline,
    ) -> Result<LoadReport, HostError>;
    fn unload(&mut self, target: &OwnedTarget, deadline: Deadline) -> Result<(), HostError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    Unavailable { detail: String },
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Unavailable { detail } => write!(f, "resource probe unavailable: {detail}"),
        }
    }
}

impl std::error::Error for ProbeError {}

/// Pools-as-data probe (#1274). The core never branches on platform — only
/// on pool math.
pub trait ResourceProbe {
    fn pools(&mut self) -> Result<Pools, ProbeError>;
}
