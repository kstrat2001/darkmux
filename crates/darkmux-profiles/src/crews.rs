//! (#1222 Phase B packet 1) Crew resolution — turns a `Crew` (saved
//! seat-staffing assignment) in the profile registry into concrete,
//! loadable models.
//!
//! Pure data-layer today: nothing in this crate or elsewhere dispatches a
//! crew yet. This module exists so the schema (`darkmux-types`), validation,
//! and resolution land ahead of the dispatch machinery a later Phase B
//! packet adds.
//!
//! `resolve_crew` is the single place a crew's validity is decided (#1269:
//! registry LOAD is deliberately lenient on crew content — one bad crew
//! must not fail the whole registry and take unrelated `--profile`
//! dispatch down with it). Every consumer that needs a specific crew calls
//! through here directly: `crew dispatch`, the funnel preflight
//! (`resolve_funnel_ctx`), `pr_review`, and `darkmux doctor`'s per-crew
//! validation check. This mirrors `get_profile`'s loud-named-error style:
//! every failure names the crew, the seat, the staffing position, and the
//! specific problem.
//!
//! **What this packet does NOT validate**: role ids (the `seats` map's
//! keys) against `crates/darkmux-crew`'s role manifest registry, or any
//! pipeline-specific seat requirement (e.g. "review-probe needs at least
//! one staffing"). Those are consumer-side checks for a later packet — this
//! schema and its resolution are generic across any future multi-seat
//! pipeline.

use anyhow::{Result, anyhow, bail};
use darkmux_types::{
    BundleSelector, ProfileModel, ProfileRegistry, QuarantinedEntryKind, SeatStaffing,
};
use std::collections::BTreeMap;

use crate::profiles::get_profile;

/// A [`SeatStaffing`] resolved to its concrete [`ProfileModel`] — what a
/// crew-aware dispatch (a later packet) will actually load and call.
#[derive(Debug, Clone)]
pub struct ResolvedSeatStaffing {
    /// The [`Profile`](darkmux_types::Profile) name this staffing dispatches
    /// through.
    pub name: String,
    pub pm: ProfileModel,
    pub k: u32,
    pub max_tokens: Option<u32>,
    pub selector: Option<BundleSelector>,
}

/// A [`Crew`] fully resolved: every seat's staffing list bound to concrete
/// [`ProfileModel`]s, keyed the same way as the source `Crew::seats`.
#[derive(Debug, Clone)]
pub struct ResolvedCrew {
    pub name: String,
    pub seats: BTreeMap<String, Vec<ResolvedSeatStaffing>>,
}

/// Resolve `name` in `reg.crews` to concrete, loadable models — validating
/// as it goes. This is the single place a crew's validity is decided:
///
/// - `seats` is non-empty
/// - every seat's staffing list is non-empty
/// - every staffing's `profile` names a real [`Profile`](darkmux_types::Profile)
/// - every explicit `model` id exists in that profile's `models[]`
/// - `k >= 1`
/// - a LOCAL staffing (no remote `endpoint`) declares `n_ctx` — a local
///   seat gets loaded at its declared context, so a missing window is a
///   resolution error here (#1282)
///
/// **Remote staffing is legal in ANY seat** (#1260, contract 1 — profile
/// uniformity): endpoint presence on the staffing's resolved model IS the
/// remoteness signal (`ProfileModel::is_remote`); no crew-side syntax, no
/// crew-side legislation of which profiles are allowed. The v1
/// "crews are local-only" rejection was a deadline fence around
/// unimplemented executor branches, not a design position — the executor
/// (the review funnel, `crew dispatch`) now routes on what the profile
/// declares: endpoint present ⇒ hosted dialect, no cycling, remote token
/// accounting. Remote models carry no `n_ctx` (#1282 — nothing is loaded
/// locally), so the `require_n_ctx` gate applies to local staffing only.
pub fn resolve_crew(reg: &ProfileRegistry, name: &str) -> Result<ResolvedCrew> {
    let crew = reg.crews.get(name).ok_or_else(|| {
        // (#1282) A quarantined name gets the entry's own parse error, not a
        // misleading "not found" — the crew IS in the file, it's broken.
        if let Some(q) = reg
            .quarantined
            .iter()
            .find(|q| q.kind == QuarantinedEntryKind::Crew && q.name == name)
        {
            return anyhow!(
                "darkmux: crew \"{}\" is quarantined — its registry entry failed to \
                 parse: {}. Fix the entry, then verify with `darkmux doctor`. (#1282)",
                name,
                q.error
            );
        }
        let available: Vec<&String> = reg.crews.keys().collect();
        let listed = available.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
        anyhow!(
            "darkmux: crew \"{}\" not found. Available: {}",
            name,
            if listed.is_empty() { "(none)" } else { &listed }
        )
    })?;

    if crew.seats.is_empty() {
        bail!("darkmux: crew \"{}\" has no seats", name);
    }

    let mut seats = BTreeMap::new();
    for (seat_id, staffings) in &crew.seats {
        if staffings.is_empty() {
            bail!(
                "darkmux: crew \"{}\" seat \"{}\" has no staffing",
                name,
                seat_id
            );
        }
        let mut resolved = Vec::with_capacity(staffings.len());
        for (i, s) in staffings.iter().enumerate() {
            resolved.push(resolve_staffing(
                reg,
                name,
                &format!("seat \"{seat_id}\" staffing[{i}]"),
                s,
            )?);
        }
        seats.insert(seat_id.clone(), resolved);
    }

    Ok(ResolvedCrew {
        name: name.to_string(),
        seats,
    })
}

fn resolve_staffing(
    reg: &ProfileRegistry,
    crew_name: &str,
    label: &str,
    s: &SeatStaffing,
) -> Result<ResolvedSeatStaffing> {
    if s.k < 1 {
        bail!(
            "darkmux: crew \"{}\" {}: k must be >= 1 (got {})",
            crew_name,
            label,
            s.k
        );
    }
    let pm = resolve_model(reg, crew_name, label, &s.profile, s.model.as_deref())?;
    Ok(ResolvedSeatStaffing {
        name: s.profile.clone(),
        pm,
        k: s.k,
        max_tokens: s.max_tokens,
        selector: s.bundle_selector.clone(),
    })
}

fn resolve_model(
    reg: &ProfileRegistry,
    crew_name: &str,
    label: &str,
    profile_name: &str,
    model_id: Option<&str>,
) -> Result<ProfileModel> {
    let profile =
        get_profile(reg, profile_name).map_err(|e| anyhow!("crew \"{}\" {}: {}", crew_name, label, e))?;

    let pm = match model_id {
        Some(id) => profile.models.iter().find(|m| m.id == id).ok_or_else(|| {
            anyhow!(
                "darkmux: crew \"{}\" {}: model \"{}\" not found in profile \"{}\"",
                crew_name,
                label,
                id,
                profile_name
            )
        })?,
        None => {
            let default_id = profile.default_model_id().ok_or_else(|| {
                anyhow!(
                    "darkmux: crew \"{}\" {}: profile \"{}\" has no default model \
                     (empty models[])",
                    crew_name,
                    label,
                    profile_name
                )
            })?;
            profile.models.iter().find(|m| m.id == default_id).ok_or_else(|| {
                anyhow!(
                    "darkmux: crew \"{}\" {}: profile \"{}\"'s default model \"{}\" is not \
                     one of its models[]",
                    crew_name,
                    label,
                    profile_name,
                    default_id
                )
            })?
        }
    };

    // (#1260, contract 1) A remote (endpoint-bearing) staffing is legal in
    // any seat — the executor routes on the profile's own declaration
    // (hosted dialect, no cycling, remote token accounting). Only a LOCAL
    // seat gets LOADED at its declared context, so the missing-`n_ctx`
    // resolution error (#1282) applies to local staffing only; remote
    // models have no local context to declare.
    if !pm.is_remote() {
        if let Err(e) = pm.require_n_ctx() {
            bail!("darkmux: crew \"{}\" {}: {}", crew_name, label, e);
        }
    }

    Ok(pm.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_types::{Crew, ModelEndpoint, Profile, ProfileModel};
    use std::collections::BTreeMap as StdBTreeMap;

    fn profile(models: Vec<ProfileModel>) -> Profile {
        Profile {
            models,
            ..Default::default()
        }
    }

    fn model(id: &str, n_ctx: u32) -> ProfileModel {
        ProfileModel {
            id: id.to_string(),
            n_ctx: Some(n_ctx),
            ..Default::default()
        }
    }

    fn remote_model(id: &str) -> ProfileModel {
        // No `n_ctx` — the #1282 schema rule: endpoint-bearing models have
        // no local context to declare.
        ProfileModel {
            id: id.to_string(),
            endpoint: Some(ModelEndpoint {
                url: Some("https://example.azure.com/openai".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn registry(profiles: Vec<(&str, Profile)>, crews: Vec<(&str, Crew)>) -> ProfileRegistry {
        let mut p = StdBTreeMap::new();
        for (name, prof) in profiles {
            p.insert(name.to_string(), prof);
        }
        let mut c = StdBTreeMap::new();
        for (name, crew) in crews {
            c.insert(name.to_string(), crew);
        }
        ProfileRegistry {
            profiles: p,
            crews: c,
            ..Default::default()
        }
    }

    fn staffing(profile: &str) -> SeatStaffing {
        SeatStaffing {
            profile: profile.to_string(),
            k: 3,
            ..Default::default()
        }
    }

    fn seats(pairs: Vec<(&str, Vec<SeatStaffing>)>) -> StdBTreeMap<String, Vec<SeatStaffing>> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    // ── happy path ──────────────────────────────────────────────

    #[test]
    fn resolve_crew_happy_path() {
        let reg = registry(
            vec![
                ("fast", profile(vec![model("a", 32000)])),
                ("deep", profile(vec![model("b", 200000)])),
            ],
            vec![(
                "review-deep",
                Crew {
                    seats: seats(vec![
                        ("review-probe", vec![staffing("fast"), staffing("deep")]),
                        ("review-judge", vec![staffing("fast")]),
                    ]),
                    ..Default::default()
                },
            )],
        );
        let resolved = resolve_crew(&reg, "review-deep").unwrap();
        assert_eq!(resolved.name, "review-deep");
        assert_eq!(resolved.seats.len(), 2);
        let probe = resolved.seats.get("review-probe").unwrap();
        assert_eq!(probe.len(), 2);
        assert_eq!(probe[0].pm.id, "a");
        assert_eq!(probe[1].pm.id, "b");
        let judge = resolved.seats.get("review-judge").unwrap();
        assert_eq!(judge.len(), 1);
        assert_eq!(judge[0].pm.id, "a");
    }

    #[test]
    fn resolve_crew_default_model_fallback() {
        let reg = registry(
            vec![(
                "balanced",
                Profile {
                    models: vec![model("primary", 60000), model("secondary", 60000)],
                    default_model: Some("secondary".to_string()),
                    ..Default::default()
                },
            )],
            vec![(
                "solo",
                Crew {
                    seats: seats(vec![("only-seat", vec![staffing("balanced")])]),
                    ..Default::default()
                },
            )],
        );
        let resolved = resolve_crew(&reg, "solo").unwrap();
        assert_eq!(resolved.seats.get("only-seat").unwrap()[0].pm.id, "secondary");
    }

    #[test]
    fn resolve_crew_explicit_model_id_wins_over_default() {
        let reg = registry(
            vec![("balanced", profile(vec![model("primary", 60000), model("secondary", 60000)]))],
            vec![(
                "solo",
                Crew {
                    seats: seats(vec![(
                        "only-seat",
                        vec![SeatStaffing {
                            profile: "balanced".to_string(),
                            model: Some("primary".to_string()),
                            k: 3,
                            ..Default::default()
                        }],
                    )]),
                    ..Default::default()
                },
            )],
        );
        let resolved = resolve_crew(&reg, "solo").unwrap();
        assert_eq!(resolved.seats.get("only-seat").unwrap()[0].pm.id, "primary");
    }

    #[test]
    fn resolve_crew_bundle_selector_passes_through_on_any_seat() {
        // No draw-shape enum gates bundle_selector — it's valid on any
        // staffing; the consumer decides meaning.
        let reg = registry(
            vec![("fast", profile(vec![model("a", 1000)]))],
            vec![(
                "c",
                Crew {
                    seats: seats(vec![(
                        "any-seat",
                        vec![SeatStaffing {
                            profile: "fast".to_string(),
                            k: 1,
                            bundle_selector: Some(BundleSelector {
                                fact_families: vec!["auth".to_string()],
                                max_bundles: Some(2),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }],
                    )]),
                    ..Default::default()
                },
            )],
        );
        let resolved = resolve_crew(&reg, "c").unwrap();
        let staffed = &resolved.seats.get("any-seat").unwrap()[0];
        assert!(staffed.selector.is_some());
    }

    // ── error paths ─────────────────────────────────────────────

    #[test]
    fn resolve_crew_missing_crew_names_available() {
        let reg = registry(vec![], vec![("review-deep", Crew::default())]);
        let err = resolve_crew(&reg, "ghost").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"));
        assert!(msg.contains("review-deep"));
    }

    #[test]
    fn resolve_crew_empty_seats_rejected() {
        let reg = registry(
            vec![("fast", profile(vec![model("a", 1000)]))],
            vec![(
                "empty",
                Crew {
                    seats: StdBTreeMap::new(),
                    ..Default::default()
                },
            )],
        );
        let err = resolve_crew(&reg, "empty").unwrap_err();
        assert!(err.to_string().contains("no seats"));
    }

    #[test]
    fn resolve_crew_empty_staffing_list_rejected() {
        let reg = registry(
            vec![("fast", profile(vec![model("a", 1000)]))],
            vec![(
                "bad",
                Crew {
                    seats: seats(vec![("review-probe", vec![])]),
                    ..Default::default()
                },
            )],
        );
        let err = resolve_crew(&reg, "bad").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no staffing"));
        assert!(msg.contains("review-probe"));
    }

    #[test]
    fn resolve_crew_missing_profile_ref_rejected() {
        let reg = registry(
            vec![("fast", profile(vec![model("a", 1000)]))],
            vec![(
                "bad",
                Crew {
                    seats: seats(vec![("review-probe", vec![staffing("ghost-profile")])]),
                    ..Default::default()
                },
            )],
        );
        let err = resolve_crew(&reg, "bad").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("review-probe"));
        assert!(msg.contains("not found") || msg.contains("ghost-profile"));
    }

    #[test]
    fn resolve_crew_k_zero_rejected() {
        let reg = registry(
            vec![("fast", profile(vec![model("a", 1000)]))],
            vec![(
                "bad",
                Crew {
                    seats: seats(vec![(
                        "review-probe",
                        vec![SeatStaffing {
                            profile: "fast".to_string(),
                            k: 0,
                            ..Default::default()
                        }],
                    )]),
                    ..Default::default()
                },
            )],
        );
        let err = resolve_crew(&reg, "bad").unwrap_err();
        assert!(err.to_string().contains("k must be >= 1"));
    }

    #[test]
    fn resolve_crew_bad_model_id_rejected() {
        let reg = registry(
            vec![("fast", profile(vec![model("a", 1000)]))],
            vec![(
                "bad",
                Crew {
                    seats: seats(vec![(
                        "review-probe",
                        vec![SeatStaffing {
                            profile: "fast".to_string(),
                            model: Some("nonexistent".to_string()),
                            k: 3,
                            ..Default::default()
                        }],
                    )]),
                    ..Default::default()
                },
            )],
        );
        let err = resolve_crew(&reg, "bad").unwrap_err();
        assert!(err.to_string().contains("not found in profile"));
    }

    /// (#1282) A LOCAL staffing whose model omits `n_ctx` parses fine (the
    /// field is optional at the schema layer) but fails HERE, at resolution,
    /// with the crew/seat and the model named — a local seat gets loaded at
    /// its declared context, so there must be one.
    #[test]
    fn resolve_crew_local_staffing_without_n_ctx_fails_at_resolution() {
        let reg = registry(
            vec![(
                "ctxless",
                profile(vec![ProfileModel {
                    id: "local-a".to_string(),
                    ..Default::default()
                }]),
            )],
            vec![(
                "bad",
                Crew {
                    seats: seats(vec![("review-probe", vec![staffing("ctxless")])]),
                    ..Default::default()
                },
            )],
        );
        let err = resolve_crew(&reg, "bad").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("local-a"), "names the model: {msg}");
        assert!(msg.contains("n_ctx"), "names the field: {msg}");
        assert!(msg.contains("review-probe"), "names the seat: {msg}");
    }

    /// (#1260, contract 1 — profile uniformity) An endpoint-bearing profile
    /// resolves in ANY seat, with no crew-side syntax: endpoint presence on
    /// the resolved model is the remoteness signal, and a remote model needs
    /// no `n_ctx` (#1282 — nothing is loaded locally). This replaces the v1
    /// "crews are local-only" rejection that #1269 surfaced as a contract
    /// violation.
    #[test]
    fn resolve_crew_remote_staffing_accepted_in_any_seat() {
        let reg = registry(
            vec![
                ("cloud", profile(vec![remote_model("gpt-remote")])),
                ("fast", profile(vec![model("local-a", 32000)])),
            ],
            vec![(
                "mixed",
                Crew {
                    seats: seats(vec![
                        ("review-probe", vec![staffing("fast"), staffing("cloud")]),
                        ("review-judge", vec![staffing("cloud")]),
                        ("review-verify", vec![staffing("cloud")]),
                    ]),
                    ..Default::default()
                },
            )],
        );
        let resolved = resolve_crew(&reg, "mixed").expect("remote staffing resolves in any seat");
        let probe = resolved.seats.get("review-probe").unwrap();
        assert!(!probe[0].pm.is_remote(), "local staffing stays local");
        assert!(probe[1].pm.is_remote(), "remote staffing resolves in the probe seat");
        assert!(probe[1].pm.n_ctx.is_none(), "remote staffing needs no n_ctx (#1282)");
        assert!(resolved.seats.get("review-judge").unwrap()[0].pm.is_remote());
        assert!(resolved.seats.get("review-verify").unwrap()[0].pm.is_remote());
    }

    /// (#1260) Lifting the local-only fence must NOT lift the genuinely-
    /// broken-staffing checks: a remote staffing whose profile ref dangles
    /// still fails with the crew/seat named — doctor's crew-validation
    /// check (which delegates here) keeps warning on real breakage.
    #[test]
    fn resolve_crew_remote_staffing_with_bad_model_id_still_rejected() {
        let reg = registry(
            vec![("cloud", profile(vec![remote_model("gpt-remote")]))],
            vec![(
                "bad",
                Crew {
                    seats: seats(vec![(
                        "review-judge",
                        vec![SeatStaffing {
                            profile: "cloud".to_string(),
                            model: Some("ghost".to_string()),
                            k: 1,
                            ..Default::default()
                        }],
                    )]),
                    ..Default::default()
                },
            )],
        );
        let err = resolve_crew(&reg, "bad").unwrap_err();
        assert!(err.to_string().contains("not found in profile"));
    }

    #[test]
    fn resolve_crew_dangling_default_model_errors_no_panic() {
        // Panic-safety for the defensive branch in `resolve_model`: a
        // hand-built registry (bypassing `validate_profile`, which catches
        // this at load time) whose profile's `default_model` names a model
        // NOT in its `models[]`. `resolve_crew` must return a named error —
        // never panic — since nothing guarantees every registry it sees
        // came through `load_registry`.
        let reg = registry(
            vec![(
                "broken",
                Profile {
                    models: vec![model("real", 1000)],
                    default_model: Some("ghost-default".to_string()),
                    ..Default::default()
                },
            )],
            vec![(
                "c",
                Crew {
                    seats: seats(vec![("review-probe", vec![staffing("broken")])]),
                    ..Default::default()
                },
            )],
        );
        let err = resolve_crew(&reg, "c").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ghost-default"), "error names the dangling id: {msg}");
        assert!(
            msg.contains("not one of its models"),
            "error explains the mismatch: {msg}"
        );
    }

    // ── coverage additions (#1222 Phase B — packet-1 gap sweep) ────

    /// A staffing whose `profile` ref resolves fine but whose `models[]` is
    /// completely EMPTY (not just a dangling `default_model`, which
    /// `resolve_crew_dangling_default_model_errors_no_panic` already
    /// covers) hits the *other* `None` arm of `default_model_id()` — no
    /// `default_model` set AND no first model to fall back to. Normally
    /// unreachable via `load_registry` (`validate_profile` rejects empty
    /// `models[]` first), but `resolve_crew` is called directly here the
    /// same way that sibling test does, so this defensive branch needs its
    /// own coverage too.
    #[test]
    fn resolve_crew_empty_models_profile_rejected() {
        let reg = registry(
            vec![(
                "empty-profile",
                Profile {
                    models: vec![],
                    ..Default::default()
                },
            )],
            vec![(
                "c",
                Crew {
                    seats: seats(vec![("review-probe", vec![staffing("empty-profile")])]),
                    ..Default::default()
                },
            )],
        );
        let err = resolve_crew(&reg, "c").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no default model"), "got: {msg}");
        assert!(msg.contains("empty models"), "got: {msg}");
    }

    /// `k`'s lower boundary, explicit: `k == 1` is the smallest ACCEPTED
    /// value (mirrors `resolve_crew_k_zero_rejected`'s rejected boundary at
    /// `k == 0`, one below).
    #[test]
    fn resolve_crew_k_one_accepted() {
        let reg = registry(
            vec![("fast", profile(vec![model("a", 1000)]))],
            vec![(
                "c",
                Crew {
                    seats: seats(vec![(
                        "review-probe",
                        vec![SeatStaffing {
                            profile: "fast".to_string(),
                            k: 1,
                            ..Default::default()
                        }],
                    )]),
                    ..Default::default()
                },
            )],
        );
        let resolved = resolve_crew(&reg, "c").unwrap();
        assert_eq!(resolved.seats.get("review-probe").unwrap()[0].k, 1);
    }

    /// `max_tokens` has no validation in `resolve_staffing` — only `k` is
    /// checked. Both extremes (0 and `u32::MAX`) pass straight through to
    /// the resolved staffing unchanged.
    #[test]
    fn resolve_crew_max_tokens_extremes_pass_through_unvalidated() {
        let reg = registry(
            vec![("fast", profile(vec![model("a", 1000)]))],
            vec![(
                "c",
                Crew {
                    seats: seats(vec![(
                        "s1",
                        vec![
                            SeatStaffing {
                                profile: "fast".to_string(),
                                k: 1,
                                max_tokens: Some(0),
                                ..Default::default()
                            },
                            SeatStaffing {
                                profile: "fast".to_string(),
                                k: 1,
                                max_tokens: Some(u32::MAX),
                                ..Default::default()
                            },
                        ],
                    )]),
                    ..Default::default()
                },
            )],
        );
        let resolved = resolve_crew(&reg, "c").unwrap();
        let staffs = resolved.seats.get("s1").unwrap();
        assert_eq!(staffs[0].max_tokens, Some(0));
        assert_eq!(staffs[1].max_tokens, Some(u32::MAX));
    }

    /// Crew names and seat ids aren't restricted to ASCII — a name with
    /// non-Latin script + emoji resolves and error-messages the same as any
    /// other string.
    #[test]
    fn resolve_crew_unicode_crew_and_seat_names() {
        let reg = registry(
            vec![("fast", profile(vec![model("a", 1000)]))],
            vec![(
                "レビュー-深い",
                Crew {
                    seats: seats(vec![("審査-probe 🔎", vec![staffing("fast")])]),
                    ..Default::default()
                },
            )],
        );
        let resolved = resolve_crew(&reg, "レビュー-深い").unwrap();
        assert_eq!(resolved.name, "レビュー-深い");
        assert!(resolved.seats.contains_key("審査-probe 🔎"));

        // Missing-crew error names the unicode crew id verbatim, same as
        // `resolve_crew_missing_crew_names_available` does for ASCII names.
        let err = resolve_crew(&reg, "ghost-名前").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ghost-名前"));
        assert!(msg.contains("レビュー-深い"), "lists the available crew: {msg}");
    }
}
