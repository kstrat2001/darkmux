//! Load team architecture manifests (roles, crews, missions, sprints) by id.
//!
//! Search order: user dir → binary-embedded built-ins.

#![allow(dead_code)]

use crate::lab::paths::{resolve, ResolveScope};
use crate::team_types::*;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
#[cfg(test)]
use std::env;
use std::fs;
use std::path::PathBuf;

/// Roles compiled into the binary at build time.
const BUILTIN_ROLES: &[(&str, &str)] = &[
    ("coder", include_str!("../templates/builtin/team/roles/coder.json")),
    ("scribe", include_str!("../templates/builtin/team/roles/scribe.json")),
];

/// Capabilities compiled into the binary at build time.
const BUILTIN_CAPABILITIES: &[(&str, &str)] = &[
    ("coding", include_str!("../templates/builtin/team/capabilities/coding.json")),
];

/// Missions compiled into the binary at build time.
const BUILTIN_MISSIONS: &[(&str, &str)] = &[];

/// Sprints compiled into the binary at build time.
const BUILTIN_SPRINTS: &[(&str, &str)] = &[];

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
/// `DARKUX_TEAMS_DIR` overrides the root — useful for tests and custom setups.
pub fn load_roles() -> Result<Vec<Role>> {
    let teams_dir = std::env::var("DARKUX_TEAMS_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| resolve(ResolveScope::Auto).teams);
    let roles_dir = teams_dir.join("roles");

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
    let paths = resolve(ResolveScope::Auto);
    let user_dir = paths.teams.join("crews");
    Ok(read_all_json::<Crew>(&user_dir)?.into_iter().map(|(_, c)| c).collect())
}

/// Load all missions.
pub fn load_missions() -> Result<Vec<Mission>> {
    let paths = resolve(ResolveScope::Auto);
    let user_dir = paths.teams.join("missions");
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
    let paths = resolve(ResolveScope::Auto);
    let user_dir = paths.teams.join("sprints");
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
    let paths = resolve(ResolveScope::Auto);
    let user_dir = paths.teams.join("capabilities");
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
    let roles_dir = paths.teams.join("roles");
    let md_path = roles_dir.join(format!("{}.md", role.id));
    if md_path.is_file() { Some(md_path) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// RAII guard that points `DARKUX_TEAMS_DIR` at a TempDir for the test's
    /// duration, then restores the previous value (or unsets it) on drop.
    /// Uses the existing env-var hook in `load_roles` rather than mutating
    /// the process cwd — cwd mutation crashes follow-up tests when the
    /// guarded TempDir is dropped while the cwd still points inside it.
    struct TeamsDirGuard {
        prev: Option<String>,
        _tmp: TempDir,
    }

    impl TeamsDirGuard {
        fn new(tmp: TempDir) -> Self {
            let prev = env::var("DARKUX_TEAMS_DIR").ok();
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe { env::set_var("DARKUX_TEAMS_DIR", tmp.path()); }
            Self { prev, _tmp: tmp }
        }

        fn path(&self) -> &std::path::Path {
            self._tmp.path()
        }
    }

    impl Drop for TeamsDirGuard {
        fn drop(&mut self) {
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                match &self.prev {
                    Some(v) => env::set_var("DARKUX_TEAMS_DIR", v),
                    None => env::remove_var("DARKUX_TEAMS_DIR"),
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
        let guard = TeamsDirGuard::new(TempDir::new().unwrap());
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
        let _guard = TeamsDirGuard::new(TempDir::new().unwrap());
        // No user files written — loader should fall through to builtin coder.
        let roles = load_roles().unwrap();
        let coder = roles.iter().find(|r| r.id == "coder").expect("builtin coder should load");
        assert_eq!(coder.description, "Implements features described by the operator. Reads existing code first; follows patterns; runs tests before reporting done.");
    }

    #[serial_test::serial]
    #[test]
    fn prompt_path_resolved_when_md_present() {
        let guard = TeamsDirGuard::new(TempDir::new().unwrap());
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
        let guard = TeamsDirGuard::new(TempDir::new().unwrap());
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
}
