//! Validated role-prompt scaffolds for agents.
//!
//! Each scaffold is a `systemPromptOverride` body + recommended profile
//! pairing + recommended tool subset, derived from the empirical lab work
//! in PERFORMANCE.md and the article-2 lab notebook.
//!
//! The user owns their agent definitions. darkmux just ships *opinionated
//! starting points* via `darkmux agent template <role>`. Output is a JSON
//! snippet ready to paste into `agents.list[]` in openclaw.json (or the
//! equivalent under another runtime).
//!
//! ## Why three roles only
//!
//! `qa`, `scribe`, and `engineer` have the strongest empirical backing
//! from the lab work — qa-v3 is validated, scribe is the stable
//! single-turn writing shape used for notebook drafts, engineer is the
//! long-context-shaped role used in bigctx work. Other roles (devops,
//! code-review, planner) are deferrable to v0.3+ when they have similar
//! grounding.
//!
//! ## Adding a new role
//!
//! 1. Add a JSON file under `templates/builtin/agent-roles/<role>.json`
//!    matching the `RoleTemplate` schema below.
//! 2. Append it to `EMBEDDED_ROLES`.
//! 3. Tests + doctor check pick it up automatically.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// One role-template manifest, parsed from JSON. Runtime-tagged so the
/// same role name can have OpenClaw / Aider / Cline flavors later (only
/// OpenClaw ships in v0.x — others are stubs the contributor adds).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleTemplate {
    pub role: String,
    pub runtime: String,
    pub description: String,
    pub recommended_profile: String,
    pub recommended_tools: Vec<String>,
    pub override_text: String,
}

/// Compile-time-embedded role manifests. Same pattern as workloads + skills.
const EMBEDDED_ROLES: &[(&str, &str)] = &[
    (
        "qa",
        include_str!("../templates/builtin/agent-roles/qa.json"),
    ),
    (
        "scribe",
        include_str!("../templates/builtin/agent-roles/scribe.json"),
    ),
    (
        "engineer",
        include_str!("../templates/builtin/agent-roles/engineer.json"),
    ),
];

/// List the roles darkmux ships scaffolds for.
pub fn list_role_ids() -> Vec<&'static str> {
    EMBEDDED_ROLES.iter().map(|(id, _)| *id).collect()
}

/// Load a role template by id. Returns an error with the available roles
/// listed when the id isn't found.
pub fn load_role(id: &str) -> Result<RoleTemplate> {
    let raw = EMBEDDED_ROLES
        .iter()
        .find(|(name, _)| *name == id)
        .map(|(_, body)| *body)
        .ok_or_else(|| {
            let available = list_role_ids().join(", ");
            anyhow::anyhow!(
                "agent role '{id}' not found. Available: {available}"
            )
        })?;
    let template: RoleTemplate = serde_json::from_str(raw)
        .with_context(|| format!("parsing embedded role '{id}'"))?;
    if template.role != id {
        bail!(
            "embedded role manifest mismatch: expected role='{id}' but manifest declares role='{}'",
            template.role
        );
    }
    Ok(template)
}

/// Emit a JSON snippet for `agents.list[]` in the target runtime config.
/// User pastes this into their openclaw.json (or equivalent) themselves —
/// stdout-only, no auto-edit.
pub fn snippet_for_agents_list(template: &RoleTemplate) -> serde_json::Value {
    serde_json::json!({
        "_notes": [
            format!("Auto-drafted by `darkmux agent template {}` (runtime={}).", template.role, template.runtime),
            format!("Pair with the `{}` profile for best fit. Adjust tools/skills to taste.", template.recommended_profile),
        ],
        "id": template.role,
        "systemPromptOverride": template.override_text,
        "tools": template.recommended_tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_roles_ship_in_v0() {
        let ids = list_role_ids();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&"qa"));
        assert!(ids.contains(&"scribe"));
        assert!(ids.contains(&"engineer"));
    }

    #[test]
    fn each_role_parses_and_self_consistent() {
        for id in list_role_ids() {
            let t = load_role(id).expect(id);
            assert_eq!(t.role, id, "role field must match id for {id}");
            assert_eq!(t.runtime, "openclaw", "v0 ships OpenClaw flavor only");
            assert!(!t.description.is_empty(), "missing description for {id}");
            assert!(!t.override_text.is_empty(), "missing override_text for {id}");
            assert!(!t.recommended_profile.is_empty(), "missing profile for {id}");
            assert!(!t.recommended_tools.is_empty(), "tools list empty for {id}");
        }
    }

    #[test]
    fn unknown_role_lists_available() {
        let err = load_role("nonsense").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"));
        assert!(msg.contains("qa"));
        assert!(msg.contains("scribe"));
        assert!(msg.contains("engineer"));
    }

    #[test]
    fn snippet_includes_id_override_and_tools() {
        let t = load_role("qa").unwrap();
        let snip = snippet_for_agents_list(&t);
        let obj = snip.as_object().unwrap();
        assert_eq!(obj.get("id").and_then(|v| v.as_str()), Some("qa"));
        assert!(obj.get("systemPromptOverride").is_some());
        assert!(obj.get("tools").and_then(|v| v.as_array()).map(|a| !a.is_empty()).unwrap_or(false));
        // _notes is included for self-documentation
        assert!(obj.get("_notes").is_some());
    }

    #[test]
    fn qa_override_includes_tool_call_style_and_execution_bias() {
        let t = load_role("qa").unwrap();
        // Two markers from the validated v3 override that should not drift
        assert!(t.override_text.contains("Tool Call Style"));
        assert!(t.override_text.contains("Execution Bias"));
    }

    #[test]
    fn engineer_role_pairs_with_deep() {
        let t = load_role("engineer").unwrap();
        assert_eq!(t.recommended_profile, "deep");
    }

    #[test]
    fn scribe_pairs_with_scribe_profile() {
        let t = load_role("scribe").unwrap();
        assert_eq!(t.recommended_profile, "scribe");
    }
}
