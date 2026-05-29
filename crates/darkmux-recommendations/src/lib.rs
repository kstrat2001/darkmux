//! Model recommendation registry per hardware tier (#159).
//!
//! Ships a per-tier curated recommendation alongside the darkmux binary,
//! resolved at runtime from the operator's detected hardware. The
//! recommendation is the maintainer's *"I bake-off'd this and these
//! models are what the data prefers."* It composes with the per-role
//! pinning (#160) to close the model-selection gap: registry says WHAT
//! to load; pinning enforces the role→model binding.
//!
//! **Operator sovereignty preserved.** Per the architectural principle:
//! - The recommendation is a SUGGESTION WITH PROVENANCE. `validated_against`
//!   names what it IS validated for; `not_validated_against` names what
//!   it ISN'T.
//! - The hard error happens only at the explicit `darkmux swap recommended`
//!   verb. Operators using other swap verbs keep existing behavior.
//! - Doctor warns on drift; doesn't block dispatches.
//! - Operators authoring custom profiles in `~/.darkmux/profiles.json`
//!   retain their override.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Recommendation status per tier:
/// - `validated` — a bake-off has been run on this tier and the listed
///   profile won. Operators get a real recommendation.
/// - `pending-bake-off` — the tier is known but no bake-off has been run.
///   `darkmux swap recommended` errors with the explanation; operators
///   select a profile manually.
/// - `no-recommendation` — the tier (e.g. `generic`) is too broad to
///   recommend against. Same UX as `pending-bake-off` but with a
///   different framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecommendationStatus {
    Validated,
    PendingBakeOff,
    NoRecommendation,
}

/// One model row within a recommendation. Mirrors the shape of
/// `ProfileModel` but stays a separate type so a future schema bump on
/// either side doesn't force the other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecommendedModel {
    pub model_id: String,
    pub role: String,
    pub n_ctx: u32,
}

/// A per-tier recommendation. JSON files at
/// `templates/builtin/recommendations/<tier>.json` deserialize into this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recommendation {
    /// The hardware tier id this recommendation is for. Matches
    /// `heuristics::HeuristicsProvider::id()` — `m-series-128`,
    /// `m-series-64`, `m-series-32`, `generic`.
    pub tier: String,
    pub status: RecommendationStatus,
    /// Operator-readable explanation: why this pick, what was tested,
    /// what wasn't. The transparency layer that keeps the recommendation
    /// a suggestion rather than an oracle.
    pub rationale: String,
    /// Link to the bake-off issue / PR / methodology doc where the
    /// hire decision was made. `None` for `pending-bake-off` and
    /// `no-recommendation` tiers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bake_off_url: Option<String>,
    /// Profile name the operator should swap to. Maps to
    /// `~/.darkmux/profiles.json::profiles[<name>]`. `None` when status
    /// is `pending-bake-off` or `no-recommendation`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
    /// Workload classes the bake-off validated against. Empty for
    /// non-validated tiers.
    #[serde(default)]
    pub validated_against: Vec<String>,
    /// Workload classes explicitly NOT validated — operator-facing
    /// transparency. The recommendation may still work for these, but
    /// the maintainer hasn't tested it.
    #[serde(default)]
    pub not_validated_against: Vec<String>,
    /// Primary model the recommendation expects to be loaded. `None`
    /// for non-validated tiers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<RecommendedModel>,
    /// Compactor model. Optional even for validated tiers (a future
    /// recommendation may not include a compactor).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compactor: Option<RecommendedModel>,
}

impl Recommendation {
    /// Flat list of all model_ids this recommendation expects to be loaded.
    /// Used by the "model pull-recommended" verb + the missing-models
    /// error path of "swap recommended".
    pub fn required_model_ids(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(p) = &self.primary {
            out.push(p.model_id.clone());
        }
        if let Some(c) = &self.compactor {
            out.push(c.model_id.clone());
        }
        out
    }
}

// ─── Embedded registry (per-tier files baked into the binary) ─────────
//
// `cargo install --path .` produces a self-contained binary; the
// recommendations have to travel with it. `include_str!` reads the JSON
// at compile time, the runtime parser turns them into Recommendation
// structs lazily on first lookup.

const EMBEDDED_M_SERIES_128: &str =
    include_str!("../../../templates/builtin/recommendations/m-series-128.json");
const EMBEDDED_M_SERIES_64: &str =
    include_str!("../../../templates/builtin/recommendations/m-series-64.json");
const EMBEDDED_M_SERIES_32: &str =
    include_str!("../../../templates/builtin/recommendations/m-series-32.json");
const EMBEDDED_GENERIC: &str =
    include_str!("../../../templates/builtin/recommendations/generic.json");

/// (tier_id, json_str) pairs. The tier_id duplication with the JSON's
/// `tier` field is deliberate — the registry constructor validates that
/// the file's `tier` matches the registered key, catching typos at
/// startup rather than at lookup.
const EMBEDDED: &[(&str, &str)] = &[
    ("m-series-128", EMBEDDED_M_SERIES_128),
    ("m-series-64", EMBEDDED_M_SERIES_64),
    ("m-series-32", EMBEDDED_M_SERIES_32),
    ("generic", EMBEDDED_GENERIC),
];

/// Process-wide registry. Initialized lazily on first lookup; parse
/// errors panic (a malformed embedded JSON is a build-time bug, not a
/// runtime condition the operator should ever see).
fn registry() -> &'static HashMap<String, Recommendation> {
    static REG: OnceLock<HashMap<String, Recommendation>> = OnceLock::new();
    REG.get_or_init(build_registry)
}

fn build_registry() -> HashMap<String, Recommendation> {
    let mut out = HashMap::with_capacity(EMBEDDED.len());
    for (tier_id, json) in EMBEDDED {
        let rec: Recommendation = serde_json::from_str(json).unwrap_or_else(|e| {
            panic!(
                "BUG: embedded recommendation for `{tier_id}` failed to parse: {e}"
            )
        });
        if rec.tier != *tier_id {
            panic!(
                "BUG: recommendation for `{tier_id}` has mismatched `tier` field `{}`",
                rec.tier
            );
        }
        // `recommended` is the reserved profile name short-circuited by
        // `cmd_swap`. A recommendation pointing back to "recommended"
        // would infinite-loop through cmd_swap → cmd_swap_recommended →
        // cmd_swap. Reject at build time.
        if rec.profile_name.as_deref() == Some("recommended") {
            panic!(
                "BUG: recommendation for tier `{tier_id}` has `profile_name=\"recommended\"` — that name is reserved by the `darkmux swap recommended` short-circuit and would infinite-loop"
            );
        }
        out.insert(tier_id.to_string(), rec);
    }
    out
}

/// Look up the recommendation for a tier id. Returns `Err` if the tier
/// isn't in the registry (a tier the operator's hardware matches but
/// no recommendation file ships — surfaces as a registry gap to fix).
pub fn for_tier(tier_id: &str) -> Result<&'static Recommendation> {
    registry()
        .get(tier_id)
        .ok_or_else(|| anyhow!("no recommendation registered for tier `{tier_id}` — file `templates/builtin/recommendations/{tier_id}.json` to fix"))
}

/// Look up the recommendation for the actively-detected hardware tier.
/// Convenience wrapper for the common case.
pub fn for_active_hardware() -> Result<&'static Recommendation> {
    let hw = darkmux_hardware::detect();
    let tier_id = darkmux_heuristics::active_provider(&hw).id();
    for_tier(tier_id)
        .with_context(|| format!("looking up recommendation for active tier `{tier_id}`"))
}

/// Returns true if the operator's profile registry contains a profile
/// literally named `recommended`. That name is reserved by the
/// `darkmux swap recommended` short-circuit; an operator-defined profile
/// with the same name is shadowed (the literal profile is unreachable
/// via `darkmux swap`). Doctor surfaces this once. (#159)
pub fn operator_has_shadowed_recommended_profile(
    registry: &darkmux_types::ProfileRegistry,
) -> bool {
    registry.profiles.contains_key("recommended")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_loads_all_embedded_tiers() {
        // All four shipped JSON files must parse + register. If any fails,
        // build_registry panics with a clear message naming the file.
        let reg = registry();
        assert!(reg.contains_key("m-series-128"));
        assert!(reg.contains_key("m-series-64"));
        assert!(reg.contains_key("m-series-32"));
        assert!(reg.contains_key("generic"));
        assert_eq!(reg.len(), 4);
    }

    #[test]
    fn m_series_128_is_validated_with_balanced_profile() {
        let rec = for_tier("m-series-128").unwrap();
        assert_eq!(rec.status, RecommendationStatus::Validated);
        assert_eq!(rec.profile_name.as_deref(), Some("balanced"));
        assert!(rec.bake_off_url.is_some());
        assert!(rec.primary.is_some());
        assert!(rec.compactor.is_some());
        // Spot-check the hire decision is the bake-off winner.
        assert_eq!(
            rec.primary.as_ref().unwrap().model_id,
            "qwen3.6-35b-a3b-turboquant-mlx"
        );
    }

    #[test]
    fn m_series_64_is_pending_with_no_profile() {
        let rec = for_tier("m-series-64").unwrap();
        assert_eq!(rec.status, RecommendationStatus::PendingBakeOff);
        assert!(rec.profile_name.is_none());
        assert!(rec.primary.is_none());
        assert!(rec.compactor.is_none());
    }

    #[test]
    fn m_series_32_is_pending_with_no_profile() {
        let rec = for_tier("m-series-32").unwrap();
        assert_eq!(rec.status, RecommendationStatus::PendingBakeOff);
        assert!(rec.profile_name.is_none());
    }

    #[test]
    fn generic_is_no_recommendation() {
        let rec = for_tier("generic").unwrap();
        assert_eq!(rec.status, RecommendationStatus::NoRecommendation);
        assert!(rec.profile_name.is_none());
    }

    #[test]
    fn unknown_tier_errors_clearly() {
        let err = for_tier("mips-cluster-2000").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("no recommendation registered"));
        assert!(msg.contains("mips-cluster-2000"));
    }

    #[test]
    fn required_model_ids_flattens_primary_and_compactor() {
        let rec = for_tier("m-series-128").unwrap();
        let ids = rec.required_model_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"qwen3.6-35b-a3b-turboquant-mlx".to_string()));
        assert!(ids.contains(&"qwen3-4b-instruct-2507".to_string()));
    }

    #[test]
    fn required_model_ids_empty_for_pending_tier() {
        let rec = for_tier("m-series-64").unwrap();
        assert!(rec.required_model_ids().is_empty());
    }
}
