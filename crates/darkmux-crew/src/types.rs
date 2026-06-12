//! Schema types for the darkmux crew architecture.
//!
//! These are scaffolding types — consumed by downstream phases (crew orchestration,
//! mission management) but not yet wired into any CLI command.
//!
//! User files at `~/.darkmux/<entity-type>/` take precedence over bundled templates.
//! Bundled templates are starting points, never the source-of-truth.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// `Capability` + `CapabilityProfile` moved to the foundation crate
// (darkmux-types) so the profile/model layer can carry them too — a role
// requests capabilities, a model offers them, both speak one vocabulary
// (E14 / #450). Re-exported here so existing `crate::types::Capability`
// references resolve unchanged.
pub use darkmux_types::{Capability, CapabilityProfile};

/// A named skill with keyword-based relevance scoring + an intrinsic
/// capability profile. A role declares which skills it has; the
/// allocator matches mission keywords against role skills to route
/// work (the keyword routing); the dispatch's model selection composes
/// per-skill capability profiles via union-via-max into a per-role
/// capability vector (the capability routing). Renamed from
/// `Capability` to free the word for the industry-conventional
/// model-capability meaning (E14).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub keywords: Vec<KeywordWeight>,
    /// Intrinsic capability profile — which AI capabilities this skill
    /// inherently demands at what relative weights. Phase-1 schema
    /// addition (E14). Absent in older manifests; defaults to empty
    /// map (sparse-as-zero — the skill demands nothing). Phase-1
    /// dispatch routing falls back to `default` regardless, so absence
    /// is harmless until phase 2 activates scoring.
    #[serde(default)]
    pub capabilities: CapabilityProfile,
}

/// A keyword paired with a relevance weight (0.0–1.0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeywordWeight {
    pub keyword: String,
    pub weight: f32,
}

/// Role positioning within a crew.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    Lead,
    Support,
}

/// Escalation contract — what a role does when it can't solve an issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EscalationContract {
    BailWithExplanation,
    RetryWithHint,
    HandOffTo(String), // role id to hand off to
}

/// A single role definition: skills, tool palette, escalation behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub skills: Vec<String>, // skill ids
    pub tool_palette: ToolPalette,
    pub escalation_contract: EscalationContract,
    /// Path to the sibling `<role-id>.md` prompt file if present. The loader
    /// resolves this from the same directory as the role's JSON manifest; it
    /// does NOT read the prompt content — just stores the resolvable path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_path: Option<PathBuf>,
    /// (#377) Per-role escalation bound — overrides
    /// `profile.runtime.compaction.reserve.bail_after_compactions`
    /// when set. Sprint-shaped roles (coder, reviewer) typically pin
    /// a low value (2-3) for tight bound; long-arc roles (researcher,
    /// mission-compiler) may pin higher (5-7) for more local-tier
    /// runway. Absent ⇒ profile default ⇒ runtime default (unbounded).
    /// Schema-compatible: older role manifests without the field
    /// continue to work unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bail_after_compactions: Option<u32>,
    /// (#377) What to do when an escalation bound fires. `"auto"`
    /// (default) emits the `EscalationTriggered` terminal and exits
    /// the dispatch — frontier-tier picks up via the
    /// `darkmux-escalation-handler` skill. `"pause"` is the operator
    /// opt-in for roles where work should NOT auto-escalate (e.g. a
    /// human-supervised long-arc role). The runtime treats both the
    /// same today; the field is plumbed for the host/skill layer to
    /// branch on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation_posture: Option<String>,
    /// (#425) Role family — a **scope** distinction (#590): `"specialist"`
    /// roles work the mission/sprints (the deliverable); `"utility"` roles
    /// support the runtime outside mission scope (mission-compiler, scribe,
    /// and — once #590 lands it — the compactor). The split also drives
    /// dispatch shape: specialists run the multi-turn agent loop and get the
    /// autonomous-dispatch preamble prepended; utility roles are bounded-I/O
    /// transformers with no agent loop, no asking-mode failure shape, and no
    /// preamble. Absent ⇒ treat as `"specialist"` (preventive safety: better
    /// an unneeded preamble than a missing one). Built-in roles all declare
    /// it explicitly. `validate_role_family` in `loader.rs` enforces the
    /// two-value axis — the legacy `"admin"` value and any unknown value are
    /// rejected with an operator-actionable message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_family: Option<String>,
    /// (#457 Step 2) Per-role overrides for the runtime's feedback-
    /// injection templates. Keys are signal-kind names matching
    /// `FeedbackInjector::queue_*` methods today:
    ///
    /// - `cycle_suspected` — fired when the cycle detector observes
    ///   the same tool+args called 3+ times in 10 turns
    /// - `tool_failure_cascade` — fired when a tool fails 3+ times
    ///   in a row
    ///
    /// Values are template strings with placeholder substitution.
    /// Supported placeholders depend on the signal kind:
    ///
    /// - `cycle_suspected`: `{tool_name}`, `{count}`, `{window_size}`
    /// - `tool_failure_cascade`: `{tool_name}`, `{count}`
    ///
    /// Missing key ⇒ runtime falls back to the hardcoded default
    /// (the [darkmux-runtime]-prefixed directive shipped in PR #455).
    /// Absent field entirely ⇒ all defaults apply.
    ///
    /// **Per-role nuance the operator named** (Beat 53 roadmap):
    /// a coder hitting a cycle gets the imperative *"regroup and
    /// change the strategy"*; a code-reviewer (read-only palette)
    /// might benefit from *"narrow the scope of your review;
    /// commit to a verdict"* instead — same signal, different
    /// actionable next move per the role's tool surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_templates: Option<std::collections::BTreeMap<String, String>>,
}

impl Role {
    /// (#425) True when this role drives a multi-turn agent loop and
    /// therefore needs the autonomous-dispatch preamble prepended to
    /// its system prompt. Default (field absent) = specialist =
    /// needs preamble. Explicit `"utility"` opts out. The legacy
    /// `"admin"` value was renamed to `"utility"` to match the
    /// codebase's broader utility/specialist nomenclature; pre-1.0,
    /// no compat alias — `validate_role_family` in `loader.rs` rejects
    /// the legacy value with a clear migration message.
    pub fn is_specialist(&self) -> bool {
        !matches!(self.role_family.as_deref(), Some("utility"))
    }

    /// (#450, E14) Derive this role's effective capability profile from
    /// the union-via-max composition of its declared skills' profiles.
    ///
    /// Composition rule: for each capability dimension, take the
    /// maximum weight from any of the role's skills. A role with
    /// `skills: ["coding", "code-reviewing"]` ends up needing whichever
    /// skill demands MORE on each dimension — the model has to satisfy
    /// the strongest demand from ANY of the role's skills.
    ///
    /// Sparse-as-zero: a skill that doesn't declare a capability is
    /// treated as weight=0 for that dimension. A capability that no
    /// skill declares stays absent in the output (also weight=0 for
    /// the role).
    ///
    /// `skill_lookup` returns the `Skill` for a given id, or `None` if
    /// the id is unknown. Unknown skills are silently skipped (with
    /// the assumption that the index-rebuild phase already validated
    /// skill ids exist; future hardening could log a warning).
    pub fn capabilities<'a, F>(&self, skill_lookup: F) -> CapabilityProfile
    where
        F: Fn(&str) -> Option<&'a Skill>,
    {
        let mut profile: CapabilityProfile = CapabilityProfile::new();
        for skill_id in &self.skills {
            let Some(skill) = skill_lookup(skill_id) else {
                continue;
            };
            for (cap, weight) in &skill.capabilities {
                // Defensive guard: skip non-finite (NaN/Infinity) and
                // negative weights. The docstring promises "non-
                // negative weights"; this enforces it at composition
                // time so phase-2 scoring never divides into garbage
                // values seeded from an operator-edited manifest with
                // a typo (e.g., `"reasoning": NaN` or `"code": -0.5`).
                // `NaN > x` is always false in IEEE-754, which would
                // otherwise silently no-op and seed 0.0; explicit
                // guard makes the intent visible.
                if !weight.is_finite() || *weight < 0.0 {
                    continue;
                }
                let entry = profile.entry(*cap).or_insert(0.0);
                if *weight > *entry {
                    *entry = *weight;
                }
            }
        }
        profile
    }
}

/// Which tool operations a role is allowed or denied.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolPalette {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

/// A single crew member: which role they play and their position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrewMember {
    pub role_id: String,
    pub position: Position,
}

/// A crew — a named collection of role assignments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Crew {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub members: Vec<CrewMember>,
}

/// Status of a mission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MissionStatus {
    #[default]
    Active,
    Closed,
    Paused,
}

/// A mission — a named objective tying sprints together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mission {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub status: MissionStatus,
    #[serde(default)]
    pub sprint_ids: Vec<String>,
    pub created_ts: u64,
    /// When the mission first transitioned to `Active`. None until
    /// `darkmux mission start` runs. Used by the wall-clock UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ts: Option<u64>,
    /// When the mission transitioned to `Closed`. Closed is terminal —
    /// once set, lifecycle verbs can't move the mission elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_ts: Option<u64>,
    /// When the mission most recently transitioned to `Paused`. Resume
    /// flips status back to `Active` but does NOT clear this field —
    /// the operator may want to see when the most recent pause occurred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_ts: Option<u64>,
    /// (#815) The operator's VERBATIM `mission propose` input — the
    /// unabridged prose the mission-compiler summarized into the
    /// description + sprint descriptions. `mission run` dispatches this
    /// alongside each sprint's compiled description so exact strings and
    /// constraints survive the compiler's compression (the lost-in-
    /// translation seam from the 2026-06-12 dogfood). None on
    /// hand-authored or pre-#815 missions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_input: Option<String>,
}

/// Status of a sprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SprintStatus {
    #[default]
    Planned,
    Running,
    Complete,
    Abandoned,
}

/// A sprint — a time-boxed work unit within a mission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sprint {
    pub id: String,
    pub mission_id: String,
    pub description: String,
    #[serde(default)]
    pub status: SprintStatus,
    /// IDs of other sprints this depends on (must complete before running).
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub created_ts: u64,
    /// When the sprint first transitioned to `Running` (or last transitioned
    /// to `Running` after being `Abandoned` and restarted). None until
    /// `darkmux sprint start` runs. Wall-clock UI shows live elapsed when
    /// `status == Running` (now - started_ts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ts: Option<u64>,
    /// When the sprint transitioned to `Complete`. Complete is terminal —
    /// once set, lifecycle verbs can't move the sprint elsewhere. Wall-clock
    /// duration = completed_ts - started_ts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_ts: Option<u64>,
    /// When the sprint transitioned to `Abandoned`. Cleared when the
    /// operator changes their mind and runs `sprint start` again — the
    /// state machine treats `Abandoned → Running` as a legal restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandoned_ts: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_with(id: &str, caps: &[(Capability, f32)]) -> Skill {
        Skill {
            id: id.into(),
            description: format!("test skill {id}"),
            keywords: vec![],
            capabilities: caps.iter().copied().collect(),
        }
    }

    fn make_role(id: &str, skill_ids: &[&str]) -> Role {
        Role {
            id: id.into(),
            description: format!("test role {id}"),
            skills: skill_ids.iter().map(|s| s.to_string()).collect(),
            tool_palette: ToolPalette::default(),
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        }
    }

    /// (#450) Capability serde — snake_case round-trip. Verifies the
    /// AI-industry-conventional naming convention is the wire format.
    #[test]
    fn capability_snake_case_round_trip() {
        let cases = [
            (Capability::Code, "\"code\""),
            (Capability::Reasoning, "\"reasoning\""),
            (Capability::InstructionFollowing, "\"instruction_following\""),
            (Capability::AgenticToolUse, "\"agentic_tool_use\""),
        ];
        for (variant, expected_json) in cases {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, expected_json, "serialize {:?}", variant);
            let back: Capability = serde_json::from_str(&s).unwrap();
            assert_eq!(back, variant, "round-trip {:?}", variant);
        }
    }

    /// (#450) Parse-time validation: unknown capability names fail to
    /// deserialize. This is what guards against silent zero-weight
    /// bugs from typos in operator-edited skill manifests.
    #[test]
    fn capability_unknown_variant_fails_to_deserialize() {
        let bad_json = "\"coding\""; // not a Capability (it's a Skill id)
        let result: Result<Capability, _> = serde_json::from_str(bad_json);
        assert!(
            result.is_err(),
            "unknown capability name `coding` must fail to deserialize, got {:?}",
            result
        );

        let typo = "\"reason\""; // typo of `reasoning`
        let result: Result<Capability, _> = serde_json::from_str(typo);
        assert!(
            result.is_err(),
            "typo capability name `reason` must fail to deserialize"
        );
    }

    /// (#450) Skill capabilities field — JSON round-trip with the new
    /// schema. Verifies sparse-as-zero by absence (the JSON doesn't
    /// have to list every capability).
    #[test]
    fn skill_capabilities_field_round_trips() {
        let json = r#"{
            "id": "coding",
            "description": "test",
            "keywords": [],
            "capabilities": { "code": 0.9, "reasoning": 0.5 }
        }"#;
        let s: Skill = serde_json::from_str(json).unwrap();
        assert_eq!(s.id, "coding");
        assert_eq!(s.capabilities.len(), 2);
        assert_eq!(s.capabilities.get(&Capability::Code).copied(), Some(0.9));
        assert_eq!(s.capabilities.get(&Capability::Reasoning).copied(), Some(0.5));
        // Sparse-as-zero: absent dimensions are absent in the map.
        assert!(!s.capabilities.contains_key(&Capability::InstructionFollowing));
        assert!(!s.capabilities.contains_key(&Capability::AgenticToolUse));
    }

    /// (#450) Older skill manifests (refactor-0 era) without the
    /// `capabilities` field continue to parse — the field is
    /// `#[serde(default)]`, so absence yields empty map (no demands).
    /// Backward-compat is required for pre-refactor-1 user-edited
    /// skill manifests under `~/.darkmux/skills/`.
    #[test]
    fn skill_without_capabilities_field_parses_with_empty_profile() {
        let legacy_json = r#"{
            "id": "old-skill",
            "description": "from refactor-0 era",
            "keywords": []
        }"#;
        let s: Skill = serde_json::from_str(legacy_json).unwrap();
        assert!(s.capabilities.is_empty());
    }

    /// (#450, E14) Role capability derivation — union-via-max across
    /// the role's declared skills. The composed profile reflects the
    /// strongest demand from ANY of the role's skills on each
    /// dimension. This is the model-selection input.
    #[test]
    fn role_capabilities_union_via_max_composition() {
        let coding = skill_with(
            "coding",
            &[
                (Capability::Code, 0.9),
                (Capability::Reasoning, 0.5),
                (Capability::AgenticToolUse, 0.7),
            ],
        );
        let reviewing = skill_with(
            "code-reviewing",
            &[
                (Capability::Code, 0.7),
                (Capability::Reasoning, 0.85),
                (Capability::InstructionFollowing, 0.7),
            ],
        );
        let skills = [coding.clone(), reviewing.clone()];
        let lookup = |id: &str| skills.iter().find(|s| s.id == id);

        let role = make_role("multi-skill", &["coding", "code-reviewing"]);
        let profile = role.capabilities(lookup);

        // max(0.9, 0.7) = 0.9 — coding demands more code than reviewing
        assert_eq!(profile.get(&Capability::Code).copied(), Some(0.9));
        // max(0.5, 0.85) = 0.85 — reviewing demands more reasoning
        assert_eq!(profile.get(&Capability::Reasoning).copied(), Some(0.85));
        // Only reviewing declares it; sparse-as-zero treats coding as 0
        assert_eq!(profile.get(&Capability::InstructionFollowing).copied(), Some(0.7));
        // Only coding declares it
        assert_eq!(profile.get(&Capability::AgenticToolUse).copied(), Some(0.7));
    }

    /// (#450, E14) Sparse-as-zero semantics — a capability dimension
    /// that NO skill declares is absent from the role's profile (not
    /// present-with-zero). Distinguishes "unspecified" from "demanded
    /// at zero" — same effective behavior under union-via-max but
    /// cleaner in iteration (no zero-padding entries).
    #[test]
    fn role_capabilities_absent_dimensions_stay_absent() {
        let analyzing = skill_with(
            "analyzing",
            &[(Capability::Reasoning, 0.9), (Capability::InstructionFollowing, 0.6)],
        );
        let skills = [analyzing.clone()];
        let lookup = |id: &str| skills.iter().find(|s| s.id == id);

        let role = make_role("analyst", &["analyzing"]);
        let profile = role.capabilities(lookup);

        assert_eq!(profile.len(), 2);
        assert!(profile.contains_key(&Capability::Reasoning));
        assert!(profile.contains_key(&Capability::InstructionFollowing));
        // Code + AgenticToolUse aren't declared by analyzing → absent.
        assert!(!profile.contains_key(&Capability::Code));
        assert!(!profile.contains_key(&Capability::AgenticToolUse));
    }

    /// (#450, E14) Empty-skills role → empty profile. Edge case worth
    /// pinning since some roles (or in-flight role manifests) may
    /// declare zero skills.
    #[test]
    fn role_capabilities_empty_skills_yields_empty_profile() {
        let role = make_role("bare", &[]);
        let profile = role.capabilities(|_: &str| None);
        assert!(profile.is_empty());
    }

    /// (#450, E14) Unknown skill ids are silently skipped — the
    /// derivation doesn't fail when a role references a skill that
    /// doesn't exist in the lookup. Index-rebuild phase is expected
    /// to validate skill-id references; this layer is defensive only.
    #[test]
    fn role_capabilities_skips_unknown_skill_ids() {
        let coding = skill_with("coding", &[(Capability::Code, 0.9)]);
        let skills = [coding.clone()];
        let lookup = |id: &str| skills.iter().find(|s| s.id == id);

        // Role declares two skills; one doesn't exist in the lookup.
        let role = make_role("partial", &["coding", "ghost-skill"]);
        let profile = role.capabilities(lookup);

        // Only the resolvable skill contributes.
        assert_eq!(profile.len(), 1);
        assert_eq!(profile.get(&Capability::Code).copied(), Some(0.9));
    }

    /// (#450, E14) Same-skill-twice idempotency. A role with
    /// `skills: ["coding", "coding"]` should produce the same
    /// effective profile as a role with `skills: ["coding"]`.
    /// Union-via-max is naturally idempotent (max(x, x) = x), but
    /// pinning this prevents a future refactor from accidentally
    /// breaking the invariant via e.g. sum-composition.
    #[test]
    fn role_capabilities_is_idempotent_under_duplicate_skill_ids() {
        let coding = skill_with(
            "coding",
            &[(Capability::Code, 0.9), (Capability::Reasoning, 0.5)],
        );
        let skills = [coding.clone()];
        let lookup = |id: &str| skills.iter().find(|s| s.id == id);

        let single = make_role("single", &["coding"]).capabilities(lookup);
        let duplicate = make_role("duplicate", &["coding", "coding"]).capabilities(lookup);

        assert_eq!(single, duplicate, "same-skill-twice must equal single declaration");
        assert_eq!(duplicate.get(&Capability::Code).copied(), Some(0.9));
    }

    /// (#450, E14) Defensive guard against NaN / Infinity / negative
    /// weights in skill manifests. The docstring promises non-
    /// negative finite weights; the impl enforces it at composition.
    /// `NaN > x` is always false in IEEE-754, which would otherwise
    /// silently no-op and seed 0.0 — phase-2 scoring would then
    /// divide into garbage. Explicit skip makes the intent visible.
    #[test]
    fn role_capabilities_skips_nan_infinity_and_negative_weights() {
        let bad_skill = Skill {
            id: "bad".into(),
            description: "test".into(),
            keywords: vec![],
            capabilities: [
                (Capability::Code, f32::NAN),
                (Capability::Reasoning, f32::INFINITY),
                (Capability::InstructionFollowing, -0.5),
                (Capability::AgenticToolUse, 0.7), // the only valid one
            ]
            .into_iter()
            .collect(),
        };
        let skills = [bad_skill.clone()];
        let lookup = |id: &str| skills.iter().find(|s| s.id == id);

        let role = make_role("defensive", &["bad"]);
        let profile = role.capabilities(lookup);

        // Only the finite non-negative weight survives.
        assert_eq!(profile.len(), 1);
        assert_eq!(profile.get(&Capability::AgenticToolUse).copied(), Some(0.7));
        assert!(!profile.contains_key(&Capability::Code), "NaN must be skipped");
        assert!(!profile.contains_key(&Capability::Reasoning), "Infinity must be skipped");
        assert!(!profile.contains_key(&Capability::InstructionFollowing), "negative must be skipped");
    }

    /// (#450 follow-up to review note) `BTreeMap` iteration is
    /// deterministic — verifies the `CapabilityProfile` ordering
    /// guarantee phase-1b dispatch + `darkmux doctor` display will
    /// rely on for stable, reproducible-by-flow-record output.
    #[test]
    fn capability_profile_iteration_is_deterministic() {
        let mut profile = CapabilityProfile::new();
        // Insert in non-enum-declaration order
        profile.insert(Capability::AgenticToolUse, 0.7);
        profile.insert(Capability::Code, 0.9);
        profile.insert(Capability::Reasoning, 0.8);
        profile.insert(Capability::InstructionFollowing, 0.5);

        // BTreeMap orders by key (Capability's derived Ord — enum
        // declaration order: Code, Reasoning, InstructionFollowing,
        // AgenticToolUse).
        let keys: Vec<Capability> = profile.keys().copied().collect();
        assert_eq!(
            keys,
            vec![
                Capability::Code,
                Capability::Reasoning,
                Capability::InstructionFollowing,
                Capability::AgenticToolUse,
            ],
        );
    }
}
