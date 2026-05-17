//! Per-role `agent.model` pin table (#160).
//!
//! Ships a curated mapping from crew role id → LMStudio model id that
//! `darkmux crew sync` writes into the openclaw `agents.list[].model`
//! field. The pin table closes the **architectural** half of the model-
//! choice gap (companion to #159, which closes the **curation** half).
//!
//! Without a pin, openclaw routes a dispatch to whatever's ambient-
//! loaded as primary — so the bake-off's hire decision (D for routine
//! coding, B for prose, etc.) lived as doctrine only. Pinning each
//! `darkmux/<role>` agent to its hired model means a wrong-ambient-
//! load surfaces as a hard load error rather than a silent fallback
//! to the wrong model. **Loud beats quiet.**
//!
//! ## Override pattern
//!
//! Mirrors the role-prompt loader (`crew::loader::load_role_prompt`):
//! 1. User dir: `<crew_root>/role-model-pins.json` (operator override)
//! 2. Embedded `templates/builtin/crew/role-model-pins.json` (binary-bundled default)
//!
//! User pins fully override the embedded table — partial overlays are
//! not supported in v1. The operator either uses the shipped table or
//! authors their own complete replacement. (Future enhancement: per-
//! role partial overlay if it becomes ergonomic to maintain.)

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::sync::OnceLock;

const EMBEDDED_PINS_JSON: &str =
    include_str!("../../templates/builtin/crew/role-model-pins.json");

/// The deserialized pin table. `BTreeMap` for `per_role` so doctor
/// output / diagnostic dumps come out in a stable alphabetical order
/// rather than HashMap's nondeterministic iteration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PinTable {
    /// Operator-readable justification for the pins — bake-off url,
    /// doctrine reference, scope notes. Surfaced in doctor messages
    /// and `darkmux crew sync` output so operators see WHY a role
    /// gets the model it gets.
    #[serde(default)]
    pub rationale: String,
    /// Link to the bake-off / methodology doc the pins were derived
    /// from. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bake_off_url: Option<String>,
    /// Pin used for any role not explicitly listed in `per_role`. The
    /// safe default — keeps unrecognized roles routed to the proven
    /// coding model.
    pub default_pin: String,
    /// Per-role explicit overrides. Role id → model id.
    #[serde(default)]
    pub per_role: BTreeMap<String, String>,
}

impl PinTable {
    /// Resolve the pin for a role: per-role override → `default_pin`.
    pub fn pin_for(&self, role_id: &str) -> &str {
        self.per_role
            .get(role_id)
            .map(|s| s.as_str())
            .unwrap_or(self.default_pin.as_str())
    }

}

/// Resolve the active pin table. Caches the result process-wide via
/// `OnceLock` since the table doesn't change without restart. If the
/// user-dir file exists and parses, it WINS; otherwise the embedded
/// default is used. Parse errors on the user file return the parse
/// error (operator-visible) rather than silently falling through —
/// silent fallback to embedded on a malformed user file would hide
/// the operator's intended override.
pub fn load_pins() -> Result<&'static PinTable> {
    static CACHE: OnceLock<PinTable> = OnceLock::new();
    if let Some(t) = CACHE.get() {
        return Ok(t);
    }
    let table = load_pins_uncached()?;
    let _ = CACHE.set(table);
    Ok(CACHE.get().expect("OnceLock just set"))
}

/// Parse-only version that always re-reads from disk; used in tests
/// where the cache would interfere with overriding the user dir.
fn load_pins_uncached() -> Result<PinTable> {
    let user_path = crate::crew::loader::crew_root().join("role-model-pins.json");
    if user_path.is_file() {
        let raw = fs::read_to_string(&user_path)
            .with_context(|| format!("reading user pin table at {}", user_path.display()))?;
        let parsed: PinTable = serde_json::from_str(&raw).with_context(|| {
            format!("parsing user pin table at {}", user_path.display())
        })?;
        return Ok(parsed);
    }
    let parsed: PinTable = serde_json::from_str(EMBEDDED_PINS_JSON).unwrap_or_else(|e| {
        panic!("BUG: embedded role-model-pins.json failed to parse: {e}")
    });
    // Build-time recursion guard analogous to #159 — a default_pin or
    // per_role value that matches the "recommended" reserved profile
    // name would route through `darkmux swap recommended` infinitely
    // if anything ever evaluated the pin as a profile. Defensive:
    // empty pins fail loudly at startup.
    if parsed.default_pin.trim().is_empty() {
        panic!("BUG: embedded pin table has empty `default_pin` — fix template/builtin/crew/role-model-pins.json");
    }
    for (role, pin) in &parsed.per_role {
        if pin.trim().is_empty() {
            panic!(
                "BUG: embedded pin table has empty pin for role `{role}` — fix template/builtin/crew/role-model-pins.json"
            );
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_table() -> PinTable {
        let mut per_role = BTreeMap::new();
        per_role.insert("coder".to_string(), "darkmux:fast-coder".to_string());
        per_role.insert("voice-editor".to_string(), "darkmux:prose-master".to_string());
        PinTable {
            rationale: "test".to_string(),
            bake_off_url: None,
            default_pin: "darkmux:default-model".to_string(),
            per_role,
        }
    }

    #[test]
    fn pin_for_returns_per_role_when_present() {
        let t = sample_table();
        assert_eq!(t.pin_for("coder"), "darkmux:fast-coder");
        assert_eq!(t.pin_for("voice-editor"), "darkmux:prose-master");
    }

    #[test]
    fn pin_for_falls_back_to_default_when_not_in_per_role() {
        let t = sample_table();
        assert_eq!(t.pin_for("scribe"), "darkmux:default-model");
        assert_eq!(t.pin_for("analyst"), "darkmux:default-model");
        assert_eq!(t.pin_for("anything-else"), "darkmux:default-model");
    }

    #[test]
    fn embedded_table_parses_and_has_known_pins() {
        // Sanity: the shipped table parses and contains the bake-off-derived
        // entries we documented in the issue body.
        let t = load_pins_uncached().unwrap();
        assert!(!t.default_pin.is_empty());
        assert!(t.per_role.contains_key("coder"));
        assert!(t.per_role.contains_key("voice-editor"));
        assert!(t.per_role.contains_key("mission-compiler"));
        // Non-pinned role gets default
        assert_eq!(t.pin_for("scribe"), t.default_pin);
    }

    #[serial_test::serial]
    #[test]
    fn user_dir_override_takes_precedence() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()); }

        let user_table = r#"{
            "rationale": "user-authored override",
            "default_pin": "user:override-default",
            "per_role": {
                "coder": "user:override-coder"
            }
        }"#;
        std::fs::write(tmp.path().join("role-model-pins.json"), user_table).unwrap();

        let t = load_pins_uncached().unwrap();
        assert_eq!(t.default_pin, "user:override-default");
        assert_eq!(t.pin_for("coder"), "user:override-coder");
        // Roles NOT in user override fall through to user's default
        // (NOT to the embedded per_role). Full-replacement semantics.
        assert_eq!(t.pin_for("voice-editor"), "user:override-default");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn user_dir_malformed_returns_parse_error_not_silent_fallback() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()); }

        std::fs::write(tmp.path().join("role-model-pins.json"), "{not valid json").unwrap();
        let result = load_pins_uncached();
        assert!(result.is_err(), "malformed user file should error, not silently fall through to embedded");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }
}
