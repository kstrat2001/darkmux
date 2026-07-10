//! Desired-state ingestion — the lenient layer between operator config and
//! the planner (#1282 direction).
//!
//! [`ingest`] NEVER fails the batch: structurally unusable or remote entries
//! come back quarantined with a named reason; valid local entries become
//! [`Placement`]s the planner reasons about.
//!
//! Scope (#1282): the registry layer is now lenient too —
//! `ProfileModel.n_ctx` is `Option<u32>` (endpoint-bearing models declare
//! none) and a structurally-broken registry entry is quarantined per-entry
//! at parse instead of blasting the whole file. So the Option on
//! [`DesiredEntry::n_ctx`] flows straight through from `ProfileModel`: a
//! local entry that lacks `n_ctx` reaches THIS layer and [`ingest`]
//! quarantines it with a named reason ([`QuarantineReason::MissingNCtx`])
//! unless the entry is remote.

use crate::ownership::namespaced_identifier;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Lenient pre-ingestion desired-state entry. Mirrors `ProfileModel`'s
/// lenient schema (#1282 — see module docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredEntry {
    pub model_key: String,
    /// n_ctx-as-MINIMUM (#600). `None` ⇒ quarantine unless remote.
    pub n_ctx: Option<u32>,
    /// Explicit identifier alias (the namespace opt-out — see
    /// [`crate::ownership::namespaced_identifier`]).
    pub identifier: Option<String>,
    /// Endpoint-bearing ⇒ no local residency concept (#1177): remote seats
    /// consume zero local pool (#1260) and never reach a
    /// [`crate::ports::ModelHost`].
    pub remote: bool,
    /// Provenance label ("primary", "utility", "probe:security", …). Never
    /// decision-bearing (#1280: no seat is exempt from the residency path);
    /// feeds reasons + the #1279 refcount report.
    pub seat: String,
}

impl DesiredEntry {
    /// `darkmux_types::ProfileModel` → lenient entry. Since #1282 the
    /// registry schema itself is lenient — `ProfileModel.n_ctx` is
    /// `Option<u32>` (endpoint-bearing models declare none) — so the Option
    /// flows straight through; [`ingest`] quarantines a LOCAL entry that
    /// lacks one. `remote` delegates to `ProfileModel::is_remote()` (the
    /// same declared-url test the dispatch path routes on) so gestalt and
    /// dispatch can never disagree about what counts as remote.
    pub fn from_profile_model(pm: &darkmux_types::ProfileModel, seat: &str) -> Self {
        DesiredEntry {
            model_key: pm.id.clone(),
            n_ctx: pm.n_ctx,
            identifier: pm.identifier.clone(),
            remote: pm.is_remote(),
            seat: seat.to_string(),
        }
    }
}

/// A validated local placement — what the planner actually reasons about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub model_key: String,
    /// Pre-resolved once, at ingest, via
    /// [`crate::ownership::namespaced_identifier`] (explicit alias
    /// passthrough or `darkmux:<model_key>`).
    pub identifier: String,
    pub min_ctx: u32,
    pub seat: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quarantined {
    pub entry: DesiredEntry,
    pub reason: QuarantineReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuarantineReason {
    /// Structurally unusable locally (#1282 direction) — named, batch
    /// survives.
    MissingNCtx,
    /// Deliberate: remote placements never reach a ModelHost (#1177/#1260);
    /// quarantine here beats a meaningless no-op adapter.
    RemoteEndpoint,
}

/// Lenient ingestion: NEVER fails the batch. Remote / structurally unusable
/// entries come back quarantined with a named reason; valid local entries
/// become [`Placement`]s, identifiers resolved here, once.
///
/// Collision rule (swap's utility-append dedup, GENERALIZED to all
/// entries): entries are deduped by resolved identifier and the FIRST entry
/// wins the slot, keeping its model key and ctx. swap's `desired_loads`
/// only ever dedups the appended standing utility model against the
/// declared list; this dedup also collapses two DECLARED entries resolving
/// to the same identifier — a deliberate divergence (swap would emit both
/// as competing loads of one identifier), not a byte-semantic port. Callers
/// list declared profile models before appended standing seats (e.g. the
/// utility model) so a duplicate utility never overrides a declared model's
/// context, and a colliding explicit alias never produces two competing
/// loads of the same identifier.
pub fn ingest(entries: &[DesiredEntry]) -> (Vec<Placement>, Vec<Quarantined>) {
    let mut placements: Vec<Placement> = Vec::new();
    let mut quarantined: Vec<Quarantined> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for e in entries {
        if e.remote {
            quarantined.push(Quarantined {
                entry: e.clone(),
                reason: QuarantineReason::RemoteEndpoint,
            });
            continue;
        }
        let Some(n_ctx) = e.n_ctx else {
            quarantined.push(Quarantined {
                entry: e.clone(),
                reason: QuarantineReason::MissingNCtx,
            });
            continue;
        };
        let identifier = namespaced_identifier(&e.model_key, e.identifier.as_deref());
        if !seen.insert(identifier.clone()) {
            continue; // first (declared) entry already won this identifier slot
        }
        placements.push(Placement {
            model_key: e.model_key.clone(),
            identifier,
            min_ctx: n_ctx,
            seat: e.seat.clone(),
        });
    }
    (placements, quarantined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(model_key: &str, n_ctx: Option<u32>, identifier: Option<&str>, seat: &str) -> DesiredEntry {
        DesiredEntry {
            model_key: model_key.to_string(),
            n_ctx,
            identifier: identifier.map(str::to_string),
            remote: false,
            seat: seat.to_string(),
        }
    }

    #[test]
    fn ingest_dedup_declared_wins() {
        // Declared m@32k first, utility m@68k second — one Placement at the
        // declared 32k (the swap desired_loads tie rule).
        let entries = vec![
            entry("m", Some(32_000), None, "declared"),
            entry("m", Some(68_000), None, "utility"),
        ];
        let (placements, quarantined) = ingest(&entries);
        assert!(quarantined.is_empty());
        assert_eq!(
            placements,
            vec![Placement {
                model_key: "m".into(),
                identifier: "darkmux:m".into(),
                min_ctx: 32_000,
                seat: "declared".into(),
            }]
        );
    }

    #[test]
    fn ingest_dedup_identifier_collision_model_entry_wins() {
        // The swap.rs collision fixture: a declared model whose explicit
        // `identifier` equals the utility model's namespaced identifier
        // dedups to ONE placement (by identifier), and the declared entry
        // wins — the `darkmux:util-4b` slot carries the declared model's
        // key, never two competing loads of the same identifier.
        let entries = vec![
            entry("worker-35b", Some(100_000), Some("darkmux:util-4b"), "declared"),
            entry("util-4b", Some(68_000), None, "utility"),
        ];
        let (placements, quarantined) = ingest(&entries);
        assert!(quarantined.is_empty());
        assert_eq!(placements.len(), 1, "collision must dedup to a single placement");
        assert_eq!(placements[0].identifier, "darkmux:util-4b");
        assert_eq!(placements[0].model_key, "worker-35b", "the declared entry wins the identifier slot");
        assert_eq!(placements[0].min_ctx, 100_000);
    }

    #[test]
    fn ingest_quarantines_missing_n_ctx() {
        // (#1282 direction) The batch survives; the bad entry is named.
        let entries = vec![
            entry("m1", Some(8_000), None, "a"),
            entry("m2", None, None, "b"),
            entry("m3", Some(8_000), None, "c"),
        ];
        let (placements, quarantined) = ingest(&entries);
        assert_eq!(placements.len(), 2);
        assert_eq!(
            quarantined,
            vec![Quarantined {
                entry: entry("m2", None, None, "b"),
                reason: QuarantineReason::MissingNCtx,
            }]
        );
    }

    #[test]
    fn ingest_quarantines_remote_endpoint() {
        // (#1177/#1260) Endpoint-bearing entries never become placements —
        // zero local pool consumption, and plan_acquire never sees them.
        // Remote takes precedence over the missing-n_ctx check: a remote
        // seat legitimately has no local load context.
        let mut e = entry("gpt-4o", None, None, "judge");
        e.remote = true;
        let (placements, quarantined) = ingest(&[e.clone()]);
        assert!(placements.is_empty());
        assert_eq!(
            quarantined,
            vec![Quarantined { entry: e, reason: QuarantineReason::RemoteEndpoint }]
        );
    }

    #[test]
    fn from_profile_model_maps_fields() {
        let pm = darkmux_types::ProfileModel {
            id: "qwen3.6-35b-a3b".into(),
            n_ctx: Some(100_000),
            identifier: Some("my-alias".into()),
            capabilities: Default::default(),
            endpoint: None,
            extras: Default::default(),
        };
        let e = DesiredEntry::from_profile_model(&pm, "primary");
        assert_eq!(e.model_key, "qwen3.6-35b-a3b");
        assert_eq!(e.n_ctx, Some(100_000));
        assert_eq!(e.identifier.as_deref(), Some("my-alias"));
        assert!(!e.remote);
        assert_eq!(e.seat, "primary");
    }

    /// (#1282) `ProfileModel.n_ctx` is `Option<u32>` at the schema layer now;
    /// a local model missing it flows through as `None` and lands in the
    /// MissingNCtx quarantine at ingest — the batch survives.
    #[test]
    fn from_profile_model_passes_missing_n_ctx_through_to_ingest_quarantine() {
        let pm = darkmux_types::ProfileModel {
            id: "ctxless-local".into(),
            n_ctx: None,
            identifier: None,
            capabilities: Default::default(),
            endpoint: None,
            extras: Default::default(),
        };
        let e = DesiredEntry::from_profile_model(&pm, "primary");
        assert_eq!(e.n_ctx, None);
        assert!(!e.remote);
        let (placements, quarantined) = ingest(&[e]);
        assert!(placements.is_empty());
        assert_eq!(quarantined.len(), 1);
        assert_eq!(quarantined[0].reason, QuarantineReason::MissingNCtx);
    }

    #[test]
    fn from_profile_model_detects_remote_endpoint() {
        let pm = darkmux_types::ProfileModel {
            id: "gpt-4o".into(),
            n_ctx: Some(128_000),
            identifier: None,
            capabilities: Default::default(),
            endpoint: Some(darkmux_types::ModelEndpoint {
                url: Some("https://example.azure.com/openai".into()),
                ..Default::default()
            }),
            extras: Default::default(),
        };
        assert!(DesiredEntry::from_profile_model(&pm, "judge").remote);
        // An endpoint block WITHOUT a url is the LMStudio-local default —
        // not remote.
        let pm_local = darkmux_types::ProfileModel {
            endpoint: Some(darkmux_types::ModelEndpoint::default()),
            ..pm
        };
        assert!(!DesiredEntry::from_profile_model(&pm_local, "judge").remote);
    }
}
