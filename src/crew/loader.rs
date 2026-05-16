//! Load crew architecture manifests (roles, crews, missions, sprints) by id.
//!
//! Search order: user dir → binary-embedded built-ins.

#![allow(dead_code)]

use crate::crew::types::*;
use crate::lab::paths::{resolve, ResolveScope};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
#[cfg(test)]
use std::env;
use std::fs;
use std::path::PathBuf;

/// Roles compiled into the binary at build time. Filename = `<id>.json`.
const BUILTIN_ROLES: &[(&str, &str)] = &[
    ("coder", include_str!("../../templates/builtin/crew/roles/coder.json")),
    ("scribe", include_str!("../../templates/builtin/crew/roles/scribe.json")),
    ("code-reviewer", include_str!("../../templates/builtin/crew/roles/code-reviewer.json")),
    ("analyst", include_str!("../../templates/builtin/crew/roles/analyst.json")),
    ("voice-editor", include_str!("../../templates/builtin/crew/roles/voice-editor.json")),
    ("design-reviewer", include_str!("../../templates/builtin/crew/roles/design-reviewer.json")),
    ("test-designer", include_str!("../../templates/builtin/crew/roles/test-designer.json")),
    ("lab-runner", include_str!("../../templates/builtin/crew/roles/lab-runner.json")),
    ("mission-compiler", include_str!("../../templates/builtin/crew/roles/mission-compiler.json")),
    // Non-SWE engagement roles (#141): trip planning, health, athletics, legal.
    // Each is bounded — research/organize/structure only; no exec, no execution
    // of bookings or commitments. Each prompt's opening lines name what the role
    // is NOT, to keep cross-bounds questions from leaking into licensed-
    // professional territory.
    ("trip-researcher", include_str!("../../templates/builtin/crew/roles/trip-researcher.json")),
    ("logistics-coordinator", include_str!("../../templates/builtin/crew/roles/logistics-coordinator.json")),
    ("health-research", include_str!("../../templates/builtin/crew/roles/health-research.json")),
    ("fitness-coach", include_str!("../../templates/builtin/crew/roles/fitness-coach.json")),
    ("legal-research", include_str!("../../templates/builtin/crew/roles/legal-research.json")),
];

/// Capabilities compiled into the binary at build time. Filename = `<id>.json`.
const BUILTIN_CAPABILITIES: &[(&str, &str)] = &[
    ("coding", include_str!("../../templates/builtin/crew/capabilities/coding.json")),
    ("documenting", include_str!("../../templates/builtin/crew/capabilities/documenting.json")),
    ("test-designing", include_str!("../../templates/builtin/crew/capabilities/test-designing.json")),
    ("code-reviewing", include_str!("../../templates/builtin/crew/capabilities/code-reviewing.json")),
    ("analyzing", include_str!("../../templates/builtin/crew/capabilities/analyzing.json")),
    ("writing", include_str!("../../templates/builtin/crew/capabilities/writing.json")),
    ("voice-editing", include_str!("../../templates/builtin/crew/capabilities/voice-editing.json")),
    ("lab-running", include_str!("../../templates/builtin/crew/capabilities/lab-running.json")),
    ("design-reviewing", include_str!("../../templates/builtin/crew/capabilities/design-reviewing.json")),
    ("mission-compiling", include_str!("../../templates/builtin/crew/capabilities/mission-compiling.json")),
];

/// Role system prompts (`.md`) compiled into the binary. Used as the
/// fallback source when no user-side `<crew_root>/roles/<id>.md` exists.
/// One entry per role advertised in `BUILTIN_ROLES`; the
/// `crew_role_prompt_coverage` doctor check verifies this invariant.
pub(crate) const BUILTIN_ROLE_PROMPTS: &[(&str, &str)] = &[
    ("coder", include_str!("../../templates/builtin/crew/roles/coder.md")),
    ("scribe", include_str!("../../templates/builtin/crew/roles/scribe.md")),
    ("code-reviewer", include_str!("../../templates/builtin/crew/roles/code-reviewer.md")),
    ("mission-compiler", include_str!("../../templates/builtin/crew/roles/mission-compiler.md")),
    ("analyst", include_str!("../../templates/builtin/crew/roles/analyst.md")),
    ("design-reviewer", include_str!("../../templates/builtin/crew/roles/design-reviewer.md")),
    ("lab-runner", include_str!("../../templates/builtin/crew/roles/lab-runner.md")),
    ("test-designer", include_str!("../../templates/builtin/crew/roles/test-designer.md")),
    ("voice-editor", include_str!("../../templates/builtin/crew/roles/voice-editor.md")),
    // Non-SWE engagement roles (#141). Order mirrors BUILTIN_ROLES.
    ("trip-researcher", include_str!("../../templates/builtin/crew/roles/trip-researcher.md")),
    ("logistics-coordinator", include_str!("../../templates/builtin/crew/roles/logistics-coordinator.md")),
    ("health-research", include_str!("../../templates/builtin/crew/roles/health-research.md")),
    ("fitness-coach", include_str!("../../templates/builtin/crew/roles/fitness-coach.md")),
    ("legal-research", include_str!("../../templates/builtin/crew/roles/legal-research.md")),
];

/// Expose the embedded role-id list for callers that need to verify
/// prompt coverage against `BUILTIN_ROLES` (e.g., doctor checks). Kept
/// thin so the visibility surface stays minimal.
pub(crate) fn builtin_roles_ids() -> Vec<&'static str> {
    BUILTIN_ROLES.iter().map(|(id, _)| *id).collect()
}

/// Same shape for embedded role-prompt ids. Used by doctor's
/// `crew_role_prompt_coverage` check.
pub(crate) fn builtin_role_prompt_ids() -> Vec<&'static str> {
    BUILTIN_ROLE_PROMPTS.iter().map(|(id, _)| *id).collect()
}

/// Missions compiled into the binary at build time.
const BUILTIN_MISSIONS: &[(&str, &str)] = &[];

/// Sprints compiled into the binary at build time.
const BUILTIN_SPRINTS: &[(&str, &str)] = &[];

/// The user-side crew root: `DARKMUX_CREW_DIR` if set, else `<paths.crew>`
/// from the active workspace.
pub(crate) fn crew_root() -> PathBuf {
    std::env::var("DARKMUX_CREW_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| resolve(ResolveScope::Auto).crew)
}

/// Resolve a role's system-prompt text. Search order:
///   1. User dir: `<crew_root>/roles/<role-id>.md` (operator override)
///   2. Embedded `BUILTIN_ROLE_PROMPTS` (binary-bundled defaults)
///
/// Returns the prompt content if found, `None` if neither source has a
/// prompt for this role (e.g., the JSON manifest exists but no `.md`
/// prompt has been authored yet — Pair 2 of the bake-off covers those).
pub fn load_role_prompt(role_id: &str) -> Option<String> {
    let user_path = crew_root().join("roles").join(format!("{role_id}.md"));
    if user_path.is_file() {
        if let Ok(content) = fs::read_to_string(&user_path) {
            return Some(content);
        }
    }
    for (id, content) in BUILTIN_ROLE_PROMPTS {
        if *id == role_id {
            return Some((*content).to_string());
        }
    }
    None
}

fn read_json<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Result<T> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading manifest at {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parsing JSON at {}", path.display()))
}

fn read_all_roles(dir: &std::path::Path) -> Result<Vec<(String, Role)>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut results = Vec::new();
    let entries = fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?;
    for entry in entries {
        let path = match entry {
            Ok(e) => e.path(),
            Err(_) => continue,
        };
        if !path.is_file() { continue; }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !name.ends_with(".json") { continue; }
        let id = name.trim_end_matches(".json").to_string();
        match read_json::<Role>(&path) {
            Ok(role) => results.push((id, role)),
            Err(e) => eprintln!("warning: failed to load role {path:?}: {e}"),
        }
    }
    Ok(results)
}

fn read_all_json<T: serde::de::DeserializeOwned>(dir: &std::path::Path) -> Result<Vec<(String, T)>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut results = Vec::new();
    let entries = fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?;
    for entry in entries {
        let path = match entry {
            Ok(e) => e.path(),
            Err(_) => continue,
        };
        if !path.is_file() { continue; }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !name.ends_with(".json") { continue; }
        let id = name.trim_end_matches(".json").to_string();
        match read_json::<T>(&path) {
            Ok(val) => results.push((id, val)),
            Err(e) => eprintln!("warning: failed to load {path:?}: {e}"),
        }
    }
    Ok(results)
}

/// Load all roles from the user dir, falling back to built-in templates.
///
/// `DARKMUX_CREW_DIR` overrides the crew root — useful for tests and
/// non-standard layouts. (The Phase A Era env var was `DARKMUX_CREW_DIR`;
/// renamed for the Crew doctrine + to fix the typo. Anyone who set the old
/// one needs to update.)
pub fn load_roles() -> Result<Vec<Role>> {
    let crew_dir = crew_root();
    let roles_dir = crew_dir.join("roles");

    // Load user-defined roles first.
    let mut map: BTreeMap<String, Role> = BTreeMap::new();
    for (_id, role) in read_all_roles(&roles_dir)? {
        map.insert(role.id.clone(), role);
    }

    // Built-in roles fill in any ids not covered by user files.
    for (id, json) in BUILTIN_ROLES {
        match serde_json::from_str::<Role>(json) {
            Ok(role) => { map.entry(id.to_string()).or_insert(role); }
            Err(e) => eprintln!("warning: failed to parse builtin role \"{id}\": {e}"),
        }
    }

    // Resolve prompt paths by checking for sibling .md files.
    Ok(map.into_values().map(|mut role| {
        let md_path = roles_dir.join(format!("{}.md", role.id));
        if md_path.is_file() {
            role.prompt_path = Some(md_path);
        } else if let Some(_p) = &role.prompt_path {
            // User-provided prompt path — keep it as-is (absolute).
        } else {
            role.prompt_path = None;
        }
        role
    }).collect())
}

/// Load all crews.
pub fn load_crews() -> Result<Vec<Crew>> {
    let user_dir = crew_root().join("crews");
    Ok(read_all_json::<Crew>(&user_dir)?.into_iter().map(|(_, c)| c).collect())
}

/// Load all missions.
pub fn load_missions() -> Result<Vec<Mission>> {
    let user_dir = crew_root().join("missions");
    let mut map: BTreeMap<String, Mission> = read_all_json::<Mission>(&user_dir)?
        .into_iter()
        .map(|(id, m)| (id.clone(), m))
        .collect();

    for (id, json) in BUILTIN_MISSIONS {
        match serde_json::from_str::<Mission>(json) {
            Ok(m) => { map.entry(id.to_string()).or_insert(m); }
            Err(e) => eprintln!("warning: failed to parse builtin mission \"{id}\": {e}"),
        }
    }

    Ok(map.into_values().collect())
}

/// Load all sprints.
pub fn load_sprints() -> Result<Vec<Sprint>> {
    let user_dir = crew_root().join("sprints");
    let mut map: BTreeMap<String, Sprint> = read_all_json::<Sprint>(&user_dir)?
        .into_iter()
        .map(|(id, s)| (id.clone(), s))
        .collect();

    for (id, json) in BUILTIN_SPRINTS {
        match serde_json::from_str::<Sprint>(json) {
            Ok(s) => { map.entry(id.to_string()).or_insert(s); }
            Err(e) => eprintln!("warning: failed to parse builtin sprint \"{id}\": {e}"),
        }
    }

    Ok(map.into_values().collect())
}

/// Load all capabilities.
pub fn load_capabilities() -> Result<Vec<Capability>> {
    let user_dir = crew_root().join("capabilities");
    let mut map: BTreeMap<String, Capability> = read_all_json::<Capability>(&user_dir)?
        .into_iter()
        .map(|(id, c)| (id.clone(), c))
        .collect();

    for (id, json) in BUILTIN_CAPABILITIES {
        match serde_json::from_str::<Capability>(json) {
            Ok(c) => { map.entry(id.to_string()).or_insert(c); }
            Err(e) => eprintln!("warning: failed to parse builtin capability \"{id}\": {e}"),
        }
    }

    Ok(map.into_values().collect())
}

/// Resolve the prompt file path for a role at runtime. Used when an operator
/// needs to read the actual system prompt text (the loader only stores path).
#[allow(dead_code)]
pub fn resolve_role_prompt_path(
    role: &Role,
    paths: &crate::lab::paths::DarkmuxPaths,
) -> Option<PathBuf> {
    if let Some(p) = &role.prompt_path {
        return Some(p.clone());
    }
    let roles_dir = paths.crew.join("roles");
    let md_path = roles_dir.join(format!("{}.md", role.id));
    if md_path.is_file() { Some(md_path) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// RAII guard that points `DARKMUX_CREW_DIR` at a TempDir for the test's
    /// duration, then restores the previous value (or unsets it) on drop.
    /// Uses the existing env-var hook in `load_roles` rather than mutating
    /// the process cwd — cwd mutation crashes follow-up tests when the
    /// guarded TempDir is dropped while the cwd still points inside it.
    struct CrewDirGuard {
        prev: Option<String>,
        _tmp: TempDir,
    }

    impl CrewDirGuard {
        fn new(tmp: TempDir) -> Self {
            let prev = env::var("DARKMUX_CREW_DIR").ok();
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe { env::set_var("DARKMUX_CREW_DIR", tmp.path()); }
            Self { prev, _tmp: tmp }
        }

        fn path(&self) -> &std::path::Path {
            self._tmp.path()
        }
    }

    impl Drop for CrewDirGuard {
        fn drop(&mut self) {
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                match &self.prev {
                    Some(v) => env::set_var("DARKMUX_CREW_DIR", v),
                    None => env::remove_var("DARKMUX_CREW_DIR"),
                }
            }
        }
    }

    #[test]
    fn roles_round_trip() {
        let role = Role {
            id: "test".into(),
            description: "A test role".into(),
            capabilities: vec![String::from("coding")],
            tool_palette: ToolPalette { allow: vec!["read".to_string(), "edit".to_string()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
        };
        let json = serde_json::to_string(&role).unwrap();
        let back: Role = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "test");
        assert_eq!(back.description, "A test role");
        assert_eq!(back.capabilities, vec![String::from("coding")]);
    }

    #[test]
    fn capabilities_round_trip() {
        let cap = Capability {
            id: "coding".into(),
            description: "Writes code".into(),
            keywords: vec![KeywordWeight { keyword: "implement".into(), weight: 1.0 }],
        };
        let json = serde_json::to_string(&cap).unwrap();
        let back: Capability = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "coding");
        assert_eq!(back.keywords[0].keyword, "implement");
    }

    #[test]
    fn escalation_contract_serializes() {
        let bail = EscalationContract::BailWithExplanation;
        assert_eq!(serde_json::to_string(&bail).unwrap(), "\"bail-with-explanation\"");

        let retry = EscalationContract::RetryWithHint;
        assert_eq!(serde_json::to_string(&retry).unwrap(), "\"retry-with-hint\"");

        let handoff = EscalationContract::HandOffTo("operator".into());
        assert_eq!(serde_json::to_string(&handoff).unwrap(), "{\"hand-off-to\":\"operator\"}");
    }

    #[test]
    fn position_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Position::Lead).unwrap(), "\"lead\"");
        assert_eq!(serde_json::to_string(&Position::Support).unwrap(), "\"support\"");
    }

    #[serial_test::serial]
    #[test]
    fn loader_picks_user_file_over_builtin() {
        let guard = CrewDirGuard::new(TempDir::new().unwrap());
        let roles_dir = guard.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();
        let user_json = r#"{"id":"coder","description":"user override","capabilities":[],"tool_palette":{"allow":["read"],"deny":[]},"escalation_contract":"bail-with-explanation"}"#;
        fs::write(roles_dir.join("coder.json"), user_json).unwrap();

        let roles = load_roles().unwrap();
        let coder = roles.iter().find(|r| r.id == "coder").expect("coder should be loaded");
        assert_eq!(coder.description, "user override");
    }

    #[serial_test::serial]
    #[test]
    fn loader_falls_through_to_builtin() {
        let _guard = CrewDirGuard::new(TempDir::new().unwrap());
        // No user files written — loader should fall through to builtin coder.
        let roles = load_roles().unwrap();
        let coder = roles.iter().find(|r| r.id == "coder").expect("builtin coder should load");
        assert_eq!(coder.description, "Implements features described by the operator. Reads existing code first; follows patterns; runs tests before reporting done.");
    }

    #[serial_test::serial]
    #[test]
    fn coder_role_can_create_new_files() {
        // Regression guard for #124. Implementing features routinely requires
        // creating new files (new modules, new templates, new fixtures).
        // `edit` only modifies existing files; `write` is what the coder uses
        // to create them. A dispatch with the previous palette
        // (`read`/`edit`/`exec`/`process` — no `write`) dead-ends mid-task on
        // any new-file step. Verified empirically on Sprint 2 of #113 (PR #123).
        let _guard = CrewDirGuard::new(TempDir::new().unwrap());
        let roles = load_roles().unwrap();
        let coder = roles.iter().find(|r| r.id == "coder").expect("builtin coder should load");
        assert!(
            coder.tool_palette.allow.iter().any(|t| t == "write"),
            "coder.tool_palette.allow must include 'write' (regression of #124); got {:?}",
            coder.tool_palette.allow
        );
    }

    #[serial_test::serial]
    #[test]
    fn prompt_path_resolved_when_md_present() {
        let guard = CrewDirGuard::new(TempDir::new().unwrap());
        let roles_dir = guard.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();
        let role_json = r#"{"id":"custom","description":"a custom role","capabilities":[],"tool_palette":{"allow":["read"],"deny":[]},"escalation_contract":"bail-with-explanation"}"#;
        fs::write(roles_dir.join("custom.json"), role_json).unwrap();
        fs::write(roles_dir.join("custom.md"), "# Custom Role\nDo stuff.").unwrap();

        let roles = load_roles().unwrap();
        let custom = roles.iter().find(|r| r.id == "custom").expect("custom should load");
        assert!(
            custom.prompt_path.is_some(),
            "prompt_path should be Some when .md exists"
        );
    }

    #[serial_test::serial]
    #[test]
    fn prompt_path_none_when_md_absent() {
        let guard = CrewDirGuard::new(TempDir::new().unwrap());
        let roles_dir = guard.path().join("roles");
        fs::create_dir_all(&roles_dir).unwrap();
        let role_json = r#"{"id":"no-prompt","description":"no prompt file","capabilities":[],"tool_palette":{"allow":["read"],"deny":[]},"escalation_contract":"bail-with-explanation"}"#;
        fs::write(roles_dir.join("no-prompt.json"), role_json).unwrap();

        let roles = load_roles().unwrap();
        let np = roles.iter().find(|r| r.id == "no-prompt").expect("no-prompt should load");
        assert!(
            np.prompt_path.is_none(),
            "prompt_path should be None when .md doesn't exist"
        );
    }

    #[serial_test::serial]
    #[test]
    fn mission_compiler_role_loads_correctly() {
        let _guard = CrewDirGuard::new(TempDir::new().unwrap());
        // No user files written — loader should fall through to builtin mission-compiler.
        let roles = load_roles().unwrap();
        let mc = roles.iter().find(|r| r.id == "mission-compiler").expect("builtin mission-compiler should load");
        assert_eq!(mc.id, "mission-compiler");
        assert_eq!(mc.capabilities, vec!["mission-compiling".to_string()]);
        assert_eq!(mc.tool_palette.allow, vec!["read".to_string()]);
        assert_eq!(mc.tool_palette.deny, vec!["edit", "write", "exec", "process"]);
    }
}
