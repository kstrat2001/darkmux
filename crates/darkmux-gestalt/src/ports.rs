//! Host ports — the seam between the pure core and the world.
//!
//! Packet 2 ships the real adapters (`LmsHost` over the `lms` CLI in
//! darkmux-profiles; a macOS `ResourceProbe` presenting one "unified" pool).
//! Remote endpoints NEVER get a [`ModelHost`]: they are quarantined at
//! ingest ([`crate::desired::QuarantineReason::RemoteEndpoint`]), preserving
//! the #1177 residency-free design instead of a meaningless no-op adapter.

use crate::estimator::ArchFacts;
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
    ///
    /// **Orphan-load contract:** expiry kills the CLIENT process only (the
    /// spawned CLI call) — the host server may still complete the action
    /// after the kill and leave a `darkmux:*` resident behind. That orphan
    /// is reconciled, not leaked: the executor re-verifies preconditions
    /// from fresh facts at the next plan, observes the resident, and —
    /// absolute ownership over the darkmux namespace (#1274 contract) — it
    /// is darkmux's to unload or reuse like any other owned instance.
    Timeout { phase: &'static str, waited: Duration },
    UnknownModel { model_key: String },
    /// The #1139 fast-fail shape (insufficient host resources), structured.
    InsufficientResources { detail: String },
    /// Unload of a non-resident identifier — the #1279 failure shape,
    /// typed instead of an opaque stderr bail.
    NotResident { identifier: String },
    CommandFailed { detail: String },
    /// Refused before loading (#1135 class): the model's own maximum
    /// context, as the host's catalog states it, is below what was asked.
    /// Nothing was unloaded or loaded.
    ContextBeyondModel { model_key: String, asked: u32, limit: u64 },
    /// The load finished, but the host holds the instance at less context
    /// than was asked (#1135 class): a dispatch on it would run truncated.
    /// `limit` is the model's maximum when the catalog states one. The
    /// instance is left loaded.
    ContextShort { model_key: String, identifier: String, asked: u32, got: u64, limit: Option<u64> },
}

impl HostError {
    /// How a [`HostError::Timeout`] reads, so a caller that kept only the
    /// text can still tell a call that ran out of time from one that failed.
    pub const TIMED_OUT: &'static str = "host call timed out";
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Timeout { phase, waited } => {
                write!(f, "{} (phase: {phase}, waited {waited:?})", Self::TIMED_OUT)
            }
            HostError::UnknownModel { model_key } => {
                write!(f, "host does not know model \"{model_key}\"")
            }
            HostError::InsufficientResources { detail } => {
                write!(f, "host refused: insufficient resources ({detail})")
            }
            HostError::NotResident { identifier } => {
                write!(f, "\"{identifier}\" is not resident: nothing to unload")
            }
            HostError::CommandFailed { detail } => write!(f, "host command failed: {detail}"),
            HostError::ContextBeyondModel { model_key, asked, limit } => write!(
                f,
                "`{model_key}` takes at most {limit} tokens of context, as the engine's catalog states, \
                 and n_ctx={asked} was asked, so it was not loaded: set the profile's `n_ctx` to {limit} or less"
            ),
            HostError::ContextShort { model_key, identifier, asked, got, limit } => write!(
                f,
                "`{identifier}` loaded at context {got}, below the n_ctx={asked} asked{}. It is left loaded \
                 (`darkmux machine eject` unloads it); set the profile's `n_ctx` to {got} or less to run on it",
                short_cause(model_key, *asked, *limit)
            ),
        }
    }
}

/// Why a load lands below the context asked, as far as the catalog says.
fn short_cause(model_key: &str, asked: u32, limit: Option<u64>) -> String {
    match limit {
        Some(limit) if limit < u64::from(asked) => format!(": `{model_key}` takes at most {limit}"),
        Some(limit) => format!(
            ", though `{model_key}` states a maximum of {limit}: the engine capped the load (check its settings for this model)"
        ),
        None => format!(
            ": the engine caps a load at the model's own maximum, and `{model_key}` may take less than was asked"
        ),
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
    /// The largest context `model_key` takes, as the host's catalog states
    /// it, or `None` when it states none (#1135 class). Read before a load
    /// is decided, so a profile asking more is refused before anything is
    /// unloaded or loaded ([`crate::refuse_beyond_limit`]).
    fn context_limit(&mut self, model_key: &str) -> Option<u64> {
        let _ = model_key;
        None
    }
    /// (#3508) Architecture facts for each of `model_keys` the host can
    /// read (layers, KV heads, head size), the KV-cache term of a load's
    /// estimate, and which of them pay that cache at load. A host that
    /// cannot read them answers none, and the estimate falls back to the
    /// catalog size.
    fn arch_facts(&mut self, model_keys: &[&str]) -> ModelArch {
        let _ = model_keys;
        ModelArch::default()
    }
    /// (#3514) Whether the models this host loads take this machine's
    /// memory, so a load is planned against it. A double answers `false`:
    /// it is planned with no machine figures, never against the memory of
    /// the machine its test runs on.
    fn on_this_machine(&self) -> bool {
        false
    }
}

/// (#3508) What a host could read of its models' architecture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelArch {
    pub facts: BTreeMap<String, ArchFacts>,
    /// The keys whose facts came from a GGUF file's header. llama.cpp
    /// allocates a GGUF model's KV cache at load, for the whole context; an
    /// MLX model (facts from its `config.json`) grows its cache as the
    /// context fills, so only its weights are committed at load.
    pub kv_at_load: std::collections::BTreeSet<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_probe_errors_render_their_fields() {
        let cases = [
            (
                HostError::Timeout { phase: "load", waited: Duration::from_secs(3) },
                "host call timed out (phase: load, waited 3s)",
            ),
            (HostError::UnknownModel { model_key: "m".into() }, "host does not know model \"m\""),
            (
                HostError::InsufficientResources { detail: "no RAM".into() },
                "host refused: insufficient resources (no RAM)",
            ),
            (
                HostError::NotResident { identifier: "darkmux:m".into() },
                "\"darkmux:m\" is not resident: nothing to unload",
            ),
            (HostError::CommandFailed { detail: "exit 1".into() }, "host command failed: exit 1"),
        ];
        for (err, want) in cases {
            assert_eq!(err.to_string(), want);
        }
        assert_eq!(
            ProbeError::Unavailable { detail: "no vm_stat".into() }.to_string(),
            "resource probe unavailable: no vm_stat"
        );
        assert_eq!(Deadline::from_secs(7), Deadline(Duration::from_secs(7)));
    }
}
