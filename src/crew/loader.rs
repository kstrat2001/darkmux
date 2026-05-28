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
    ("coder", include_str!("../../templates/builtin/roles/coder.json")),
    ("scribe", include_str!("../../templates/builtin/roles/scribe.json")),
    ("code-reviewer", include_str!("../../templates/builtin/roles/code-reviewer.json")),
    ("analyst", include_str!("../../templates/builtin/roles/analyst.json")),
    ("voice-editor", include_str!("../../templates/builtin/roles/voice-editor.json")),
    ("design-reviewer", include_str!("../../templates/builtin/roles/design-reviewer.json")),
    ("test-designer", include_str!("../../templates/builtin/roles/test-designer.json")),
    ("lab-runner", include_str!("../../templates/builtin/roles/lab-runner.json")),
    ("mission-compiler", include_str!("../../templates/builtin/roles/mission-compiler.json")),
    // Non-SWE engagement roles (#141): trip planning, health, athletics, legal.
    // Each is bounded — research/organize/structure only; no exec, no execution
    // of bookings or commitments. Each prompt's opening lines name what the role
    // is NOT, to keep cross-bounds questions from leaking into licensed-
    // professional territory.
    ("trip-researcher", include_str!("../../templates/builtin/roles/trip-researcher.json")),
    ("logistics-coordinator", include_str!("../../templates/builtin/roles/logistics-coordinator.json")),
    ("health-research", include_str!("../../templates/builtin/roles/health-research.json")),
    ("fitness-coach", include_str!("../../templates/builtin/roles/fitness-coach.json")),
    ("legal-research", include_str!("../../templates/builtin/roles/legal-research.json")),
];

/// Skills compiled into the binary at build time. Filename = `<id>.json`.
const BUILTIN_SKILLS: &[(&str, &str)] = &[
    ("coding", include_str!("../../templates/builtin/skills/coding.json")),
    ("documenting", include_str!("../../templates/builtin/skills/documenting.json")),
    ("test-designing", include_str!("../../templates/builtin/skills/test-designing.json")),
    ("code-reviewing", include_str!("../../templates/builtin/skills/code-reviewing.json")),
    ("analyzing", include_str!("../../templates/builtin/skills/analyzing.json")),
    ("writing", include_str!("../../templates/builtin/skills/writing.json")),
    ("voice-editing", include_str!("../../templates/builtin/skills/voice-editing.json")),
    ("lab-running", include_str!("../../templates/builtin/skills/lab-running.json")),
    ("design-reviewing", include_str!("../../templates/builtin/skills/design-reviewing.json")),
    ("mission-compiling", include_str!("../../templates/builtin/skills/mission-compiling.json")),
];

/// Role system prompts (`.md`) compiled into the binary. Used as the
/// fallback source when no user-side `<crew_root>/roles/<id>.md` exists.
/// One entry per role advertised in `BUILTIN_ROLES`; the
/// `crew_role_prompt_coverage` doctor check verifies this invariant.
pub(crate) const BUILTIN_ROLE_PROMPTS: &[(&str, &str)] = &[
    ("coder", include_str!("../../templates/builtin/roles/coder.md")),
    ("scribe", include_str!("../../templates/builtin/roles/scribe.md")),
    ("code-reviewer", include_str!("../../templates/builtin/roles/code-reviewer.md")),
    ("mission-compiler", include_str!("../../templates/builtin/roles/mission-compiler.md")),
    ("analyst", include_str!("../../templates/builtin/roles/analyst.md")),
    ("design-reviewer", include_str!("../../templates/builtin/roles/design-reviewer.md")),
    ("lab-runner", include_str!("../../templates/builtin/roles/lab-runner.md")),
    ("test-designer", include_str!("../../templates/builtin/roles/test-designer.md")),
    ("voice-editor", include_str!("../../templates/builtin/roles/voice-editor.md")),
    // Non-SWE engagement roles (#141). Order mirrors BUILTIN_ROLES.
    ("trip-researcher", include_str!("../../templates/builtin/roles/trip-researcher.md")),
    ("logistics-coordinator", include_str!("../../templates/builtin/roles/logistics-coordinator.md")),
    ("health-research", include_str!("../../templates/builtin/roles/health-research.md")),
    ("fitness-coach", include_str!("../../templates/builtin/roles/fitness-coach.md")),
    ("legal-research", include_str!("../../templates/builtin/roles/legal-research.md")),
];

/// (#425) Autonomous-dispatch preamble — prepended to specialist
/// role prompts at dispatch-time so the model knows upfront it
/// can't ask questions, can't pause, and should escalate
/// explicitly if blocked. Admin roles (bounded-I/O transformers
/// like mission-compiler) skip the preamble since they don't run
/// agent loops.
const AUTONOMOUS_DISPATCH_PREAMBLE: &str =
    include_str!("../../templates/builtin/AUTONOMOUS_DISPATCH_PREAMBLE.md");

/// (#425) Resolve the autonomous-dispatch preamble — operator-side
/// override at `<crew_root>/AUTONOMOUS_DISPATCH_PREAMBLE.md` wins
/// if present; otherwise the embedded default is returned.
///
/// The override path lets operators tune nudge wording per fleet /
/// per machine without recompiling. Lives in the crew root rather
/// than per-role so the preamble stays uniform across the
/// specialist set.
pub fn load_autonomous_dispatch_preamble() -> String {
    let user_path = crew_root().join("AUTONOMOUS_DISPATCH_PREAMBLE.md");
    if user_path.is_file() {
        if let Ok(content) = fs::read_to_string(&user_path) {
            return content;
        }
    }
    AUTONOMOUS_DISPATCH_PREAMBLE.to_string()
}

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
///
/// **Deprecated direction (Beat 33):** prefer the per-subdir helpers
/// `roles_dir()`, `missions_dir()`, `sprints_dir()`, `crews_dir()`,
/// `skills_dir()`, and `role_pins_path()` for new code. They
/// resolve the post-flatten `<root>/<subdir>/` layout with a backward-
/// compatibility fallback to the legacy `<root>/crew/<subdir>/` layout.
/// `crew_root()` is retained for callers that need the parent directory
/// itself (e.g., daemon banner messages).
pub fn crew_root() -> PathBuf {
    std::env::var("DARKMUX_CREW_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| resolve(ResolveScope::Auto).crew)
}

/// User-state root for the post-Beat-33 flattened layout. Returns
/// `DARKMUX_CREW_DIR` if set (operator override; unchanged semantics —
/// the env var points at the directory CONTAINING the subdirs, with no
/// `crew/` nesting), otherwise `<paths.root>` (e.g., `~/.darkmux/`).
pub(crate) fn user_state_root() -> PathBuf {
    std::env::var("DARKMUX_CREW_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| resolve(ResolveScope::Auto).root)
}

/// Resolve a user-state subdirectory with backward-compat fallback.
///
/// Resolution order:
///   1. `<root>/<subdir>/` (post-flatten canonical) — if exists
///   2. `<root>/crew/<subdir>/` (pre-flatten legacy) — if exists
///   3. `<root>/<subdir>/` (canonical, returned even when missing so a
///      fresh write creates the new layout)
///
/// Writes follow reads: if legacy exists and canonical doesn't, BOTH
/// reads AND new writes go to legacy. State never silently splits
/// across the two locations. Operators migrate explicitly via the
/// `mv` script that `darkmux doctor` emits when it detects the legacy
/// layout (PR-3b).
fn resolve_user_subdir(subdir: &str) -> PathBuf {
    let root = user_state_root();
    let canonical = root.join(subdir);
    if canonical.exists() {
        return canonical;
    }
    let legacy = root.join("crew").join(subdir);
    if legacy.exists() {
        return legacy;
    }
    canonical
}

/// User-side roles directory. Post-Beat-33: `<root>/roles/`. Falls back
/// to `<root>/crew/roles/` for operators on the legacy layout.
pub fn roles_dir() -> PathBuf {
    resolve_user_subdir("roles")
}

/// User-side missions directory. Post-Beat-33: `<root>/missions/`.
/// Falls back to `<root>/crew/missions/` for operators on the legacy
/// layout.
pub fn missions_dir() -> PathBuf {
    resolve_user_subdir("missions")
}

/// User-side sprints directory. Post-Beat-33: `<root>/sprints/`. Falls
/// back to `<root>/crew/sprints/` for operators on the legacy layout.
pub fn sprints_dir() -> PathBuf {
    resolve_user_subdir("sprints")
}

/// User-side crews directory (operator overrides). Post-Beat-33:
/// `<root>/crews/`. Falls back to `<root>/crew/crews/` for legacy.
pub fn crews_dir() -> PathBuf {
    resolve_user_subdir("crews")
}

/// User-side skills directory (operator overrides). Post-Beat-33:
/// `<root>/skills/`. Falls back to `<root>/crew/skills/`
/// for legacy.
pub fn skills_dir() -> PathBuf {
    resolve_user_subdir("skills")
}

/// User-side role-model-pins.json path. Post-Beat-33:
/// `<root>/role-model-pins.json`. Falls back to
/// `<root>/crew/role-model-pins.json` for legacy.
pub fn role_pins_path() -> PathBuf {
    let root = user_state_root();
    let canonical = root.join("role-model-pins.json");
    if canonical.is_file() {
        return canonical;
    }
    let legacy = root.join("crew").join("role-model-pins.json");
    if legacy.is_file() {
        return legacy;
    }
    canonical
}

/// Resolve a role's system-prompt text. Search order:
///   1. User dir: `<crew_root>/roles/<role-id>.md` (operator override)
///   2. Embedded `BUILTIN_ROLE_PROMPTS` (binary-bundled defaults)
///
/// Returns the prompt content if found, `None` if neither source has a
/// prompt for this role (e.g., the JSON manifest exists but no `.md`
/// prompt has been authored yet — Pair 2 of the bake-off covers those).
pub fn load_role_prompt(role_id: &str) -> Option<String> {
    let user_path = roles_dir().join(format!("{role_id}.md"));
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
    let roles_dir = roles_dir();

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
    let user_dir = crews_dir();
    Ok(read_all_json::<Crew>(&user_dir)?.into_iter().map(|(_, c)| c).collect())
}

/// Load all missions from the new per-mission nested layout.
///
/// Walks `<crew_root>/missions/` and for each **subdirectory** containing a
/// `mission.json`, deserializes it.  Flat `.json` files directly under
/// `<crew_root>/missions/` (pre-#148 legacy layout) are silently skipped —
/// the migration verb (`darkmux mission migrate`) is the bridge.
///
/// Built-in missions (currently empty) are merged last, same as other loaders.
pub fn load_missions() -> Result<Vec<Mission>> {
    use crate::crew::lifecycle;
    let missions_root = missions_dir();
    if !missions_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut map: BTreeMap<String, Mission> = BTreeMap::new();
    for entry in fs::read_dir(&missions_root)
        .with_context(|| format!("reading {}", missions_root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            // Skip legacy flat <id>.json files. Migration verb is the bridge.
            continue;
        }
        let mission_id = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let mission_file = lifecycle::mission_path(&mission_id);
        if !mission_file.is_file() {
            // Empty subdir or partial state — skip silently.
            continue;
        }
        let text = fs::read_to_string(&mission_file)
            .with_context(|| format!("reading {}", mission_file.display()))?;
        match serde_json::from_str::<Mission>(&text) {
            Ok(m) => { map.insert(m.id.clone(), m); }
            Err(e) => eprintln!("warning: failed to parse mission at {}: {e}", mission_file.display()),
        }
    }

    // Merge built-in missions (currently empty — future-proofing).
    for (id, json) in BUILTIN_MISSIONS {
        match serde_json::from_str::<Mission>(json) {
            Ok(m) => { map.entry(id.to_string()).or_insert(m); }
            Err(e) => eprintln!("warning: failed to parse builtin mission \"{id}\": {e}"),
        }
    }

    let mut out: Vec<Mission> = map.into_values().collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Load all sprints from the new per-mission nested layout.
///
/// Walks every `<crew_root>/missions/<mission-id>/sprints/*.json`.  The
/// Sprint JSON already carries `mission_id`, so no inference from the dir
/// name is needed.  Legacy flat sprint files under `<crew_root>/sprints/`
/// are silently ignored — the migration verb is the bridge.
pub fn load_sprints() -> Result<Vec<Sprint>> {
    use crate::crew::lifecycle;
    let missions_root = missions_dir();
    if !missions_root.is_dir() {
        return Ok(Vec::new());
    }
    // Key by (mission_id, sprint_id) so the same sprint id is allowed across
    // different missions — that's the composite-PK invariant #148 establishes.
    let mut map: BTreeMap<(String, String), Sprint> = BTreeMap::new();
    for entry in fs::read_dir(&missions_root)
        .with_context(|| format!("reading {}", missions_root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let mission_id = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let sprints_dir = lifecycle::sprints_dir(&mission_id);
        if !sprints_dir.is_dir() {
            continue;
        }
        for sprint_entry in fs::read_dir(&sprints_dir)
            .with_context(|| format!("reading {}", sprints_dir.display()))?
        {
            let sprint_entry = sprint_entry?;
            let sprint_path = sprint_entry.path();
            if !sprint_path.is_file() {
                continue;
            }
            if sprint_path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let text = fs::read_to_string(&sprint_path)
                .with_context(|| format!("reading {}", sprint_path.display()))?;
            match serde_json::from_str::<Sprint>(&text) {
                Ok(s) => { map.insert((s.mission_id.clone(), s.id.clone()), s); }
                Err(e) => eprintln!("warning: failed to parse sprint at {}: {e}", sprint_path.display()),
            }
        }
    }

    // Built-in sprints (currently empty — future-proofing).
    for (_id, json) in BUILTIN_SPRINTS {
        match serde_json::from_str::<Sprint>(json) {
            Ok(s) => { map.entry((s.mission_id.clone(), s.id.clone())).or_insert(s); }
            Err(e) => eprintln!("warning: failed to parse builtin sprint \"{_id}\": {e}"),
        }
    }

    let mut out: Vec<Sprint> = map.into_values().collect();
    out.sort_by(|a, b| (&a.mission_id, &a.id).cmp(&(&b.mission_id, &b.id)));
    Ok(out)
}

/// Load all skills.
pub fn load_skills() -> Result<Vec<Skill>> {
    let user_dir = skills_dir();
    let mut map: BTreeMap<String, Skill> = read_all_json::<Skill>(&user_dir)?
        .into_iter()
        .map(|(id, c)| (id.clone(), c))
        .collect();

    for (id, json) in BUILTIN_SKILLS {
        match serde_json::from_str::<Skill>(json) {
            Ok(c) => { map.entry(id.to_string()).or_insert(c); }
            Err(e) => eprintln!("warning: failed to parse builtin skill \"{id}\": {e}"),
        }
    }

    Ok(map.into_values().collect())
}

/// Resolve the prompt file path for a role at runtime. Used when an operator
/// needs to read the actual system prompt text (the loader only stores path).
///
/// TODO(Beat-33 dual-read): this function takes a `DarkmuxPaths` and joins
/// `paths.crew.join("roles")` directly, bypassing the canonical-vs-legacy
/// fallback that `roles_dir()` performs. Currently `#[allow(dead_code)]` —
/// when revived, migrate to use `roles_dir()` (and either drop the
/// `paths` arg or document that it's only consulted for `prompt_path`'s
/// absolute-path passthrough).
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
            skills: vec![String::from("coding")],
            tool_palette: ToolPalette { allow: vec!["read".to_string(), "edit".to_string()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            tier: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        };
        let json = serde_json::to_string(&role).unwrap();
        let back: Role = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "test");
        assert_eq!(back.description, "A test role");
        assert_eq!(back.skills, vec![String::from("coding")]);
    }

    #[test]
    fn skills_round_trip() {
        let cap = Skill {
            id: "coding".into(),
            description: "Writes code".into(),
            keywords: vec![KeywordWeight { keyword: "implement".into(), weight: 1.0 }],
            capabilities: Default::default(),
        };
        let json = serde_json::to_string(&cap).unwrap();
        let back: Skill = serde_json::from_str(&json).unwrap();
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
        let user_json = r#"{"id":"coder","description":"user override","skills":[],"tool_palette":{"allow":["read"],"deny":[]},"escalation_contract":"bail-with-explanation"}"#;
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
        let role_json = r#"{"id":"custom","description":"a custom role","skills":[],"tool_palette":{"allow":["read"],"deny":[]},"escalation_contract":"bail-with-explanation"}"#;
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
        let role_json = r#"{"id":"no-prompt","description":"no prompt file","skills":[],"tool_palette":{"allow":["read"],"deny":[]},"escalation_contract":"bail-with-explanation"}"#;
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
        assert_eq!(mc.skills, vec!["mission-compiling".to_string()]);
        assert_eq!(mc.tool_palette.allow, vec!["read".to_string()]);
        assert_eq!(mc.tool_palette.deny, vec!["edit", "write", "exec", "process"]);
    }
}

#[cfg(test)]
mod load_per_mission_tests {
    use super::*;
    use serial_test::serial;

    /// RAII guard: sets `DARKMUX_CREW_DIR` to a TempDir for the test's duration,
    /// then restores (or unsets) on drop.  Mirrors `CrewDirGuard` in the sibling
    /// `tests` module — kept local to this module to avoid cross-module coupling.
    struct TestCrewRoot {
        prev: Option<String>,
        _tmp: tempfile::TempDir,
    }

    impl TestCrewRoot {
        fn new() -> Self {
            let tmp = tempfile::TempDir::new().unwrap();
            let prev = std::env::var("DARKMUX_CREW_DIR").ok();
            // SAFETY: serialized via #[serial] on every caller.
            unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()); }
            Self { prev, _tmp: tmp }
        }

        fn path(&self) -> &std::path::Path {
            self._tmp.path()
        }
    }

    impl Drop for TestCrewRoot {
        fn drop(&mut self) {
            // SAFETY: serialized via #[serial] on every caller.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                    None => std::env::remove_var("DARKMUX_CREW_DIR"),
                }
            }
        }
    }

    fn seed_mission(root: &std::path::Path, id: &str) {
        let dir = root.join("missions").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let mission = serde_json::json!({
            "id": id,
            "description": format!("test mission {id}"),
            "sprint_ids": [],
            "created_ts": 1_700_000_000u64,
        });
        std::fs::write(
            dir.join("mission.json"),
            serde_json::to_string_pretty(&mission).unwrap(),
        )
        .unwrap();
    }

    fn seed_sprint(root: &std::path::Path, mission_id: &str, sprint_id: &str) {
        let sdir = root.join("missions").join(mission_id).join("sprints");
        std::fs::create_dir_all(&sdir).unwrap();
        let sprint = serde_json::json!({
            "id": sprint_id,
            "mission_id": mission_id,
            "description": format!("test sprint {sprint_id}"),
            "depends_on": [],
            "created_ts": 1_700_000_000u64,
        });
        std::fs::write(
            sdir.join(format!("{sprint_id}.json")),
            serde_json::to_string_pretty(&sprint).unwrap(),
        )
        .unwrap();
    }

    #[test]
    #[serial]
    fn load_missions_empty_root() {
        let _guard = TestCrewRoot::new();
        assert_eq!(load_missions().unwrap().len(), 0);
    }

    #[test]
    #[serial]
    fn load_sprints_empty_root() {
        let _guard = TestCrewRoot::new();
        assert_eq!(load_sprints().unwrap().len(), 0);
    }

    #[test]
    #[serial]
    fn load_missions_two_missions_one_sprint_each() {
        let guard = TestCrewRoot::new();
        let root = guard.path();

        seed_mission(root, "alpha");
        seed_mission(root, "beta");
        seed_sprint(root, "alpha", "s1");
        seed_sprint(root, "beta", "s2");

        let missions = load_missions().unwrap();
        assert_eq!(missions.len(), 2);
        // Sorted by id.
        assert_eq!(missions[0].id, "alpha");
        assert_eq!(missions[1].id, "beta");

        let sprints = load_sprints().unwrap();
        assert_eq!(sprints.len(), 2);
        let alpha_sprint = sprints.iter().find(|s| s.id == "s1").expect("sprint s1 missing");
        let beta_sprint = sprints.iter().find(|s| s.id == "s2").expect("sprint s2 missing");
        assert_eq!(alpha_sprint.mission_id, "alpha");
        assert_eq!(beta_sprint.mission_id, "beta");
    }

    #[test]
    #[serial]
    fn load_missions_ignores_legacy_flat_files() {
        let guard = TestCrewRoot::new();
        let root = guard.path();

        // New layout mission.
        seed_mission(root, "current");

        // Legacy flat mission file at <crew_root>/missions/old.json — should be IGNORED.
        let missions_root = root.join("missions");
        std::fs::write(
            missions_root.join("old.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "id": "old",
                "description": "legacy",
                "sprint_ids": [],
                "created_ts": 1u64,
            }))
            .unwrap(),
        )
        .unwrap();

        // Legacy flat sprint at <crew_root>/sprints/x.json — should be IGNORED.
        let legacy_sprints = root.join("sprints");
        std::fs::create_dir_all(&legacy_sprints).unwrap();
        std::fs::write(
            legacy_sprints.join("x.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "id": "x",
                "mission_id": "old",
                "description": "legacy",
                "depends_on": [],
                "created_ts": 1u64,
            }))
            .unwrap(),
        )
        .unwrap();

        let missions = load_missions().unwrap();
        assert_eq!(missions.len(), 1, "legacy flat file must be ignored; got {:?}", missions.iter().map(|m| &m.id).collect::<Vec<_>>());
        assert_eq!(missions[0].id, "current");

        // current has no sprints; legacy x.json under <crew_root>/sprints/ is ignored.
        let sprints = load_sprints().unwrap();
        assert_eq!(sprints.len(), 0, "legacy flat sprint must be ignored; got {:?}", sprints.iter().map(|s| &s.id).collect::<Vec<_>>());
    }

    #[test]
    #[serial]
    fn load_missions_skips_subdir_without_mission_json() {
        let guard = TestCrewRoot::new();
        let root = guard.path();

        // Create a subdir with no mission.json inside it.
        let partial = root.join("missions").join("partial");
        std::fs::create_dir_all(&partial).unwrap();

        // And a fully-formed mission alongside it.
        seed_mission(root, "complete");

        let missions = load_missions().unwrap();
        assert_eq!(missions.len(), 1);
        assert_eq!(missions[0].id, "complete");
    }

    #[test]
    #[serial]
    fn load_sprints_skips_non_json_files() {
        let guard = TestCrewRoot::new();
        let root = guard.path();

        seed_mission(root, "mymission");
        seed_sprint(root, "mymission", "s-real");

        // Drop a non-JSON file into the sprints dir — should be ignored.
        let sprints_dir = root.join("missions").join("mymission").join("sprints");
        std::fs::write(sprints_dir.join("notes.txt"), "just a note").unwrap();

        let sprints = load_sprints().unwrap();
        assert_eq!(sprints.len(), 1);
        assert_eq!(sprints[0].id, "s-real");
    }

    // ─── Beat-33 dual-read fallback tests ────────────────────────────────
    //
    // `resolve_user_subdir` prefers the post-flatten canonical path
    // (`<root>/<subdir>/`) and falls back to the legacy pre-flatten path
    // (`<root>/crew/<subdir>/`) for operators who haven't migrated. These
    // tests pin the resolution table so a regression that silently flips
    // preference (e.g., always prefer legacy) fails loudly.

    #[serial]
    #[test]
    fn resolve_user_subdir_prefers_canonical_when_only_canonical_exists() {
        let guard = TestCrewRoot::new();
        let canonical = guard.path().join("roles");
        std::fs::create_dir_all(&canonical).unwrap();
        assert_eq!(roles_dir(), canonical);
    }

    #[serial]
    #[test]
    fn resolve_user_subdir_falls_back_to_legacy_when_only_legacy_exists() {
        let guard = TestCrewRoot::new();
        let legacy = guard.path().join("crew").join("roles");
        std::fs::create_dir_all(&legacy).unwrap();
        assert_eq!(roles_dir(), legacy);
    }

    #[serial]
    #[test]
    fn resolve_user_subdir_prefers_canonical_when_both_exist() {
        // Operator partway through a migration — canonical should win so
        // future reads + writes consolidate at the new layout (legacy
        // becomes operator-visible-but-not-touched, doctor PR-3b
        // surfaces it for cleanup).
        let guard = TestCrewRoot::new();
        let canonical = guard.path().join("missions");
        let legacy = guard.path().join("crew").join("missions");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::create_dir_all(&legacy).unwrap();
        assert_eq!(missions_dir(), canonical);
    }

    #[serial]
    #[test]
    fn resolve_user_subdir_returns_canonical_when_neither_exists() {
        // Fresh install — neither layout exists. The canonical path is
        // returned so a subsequent write creates the new layout
        // (operator-sovereignty: no silent migration of legacy state).
        let guard = TestCrewRoot::new();
        let canonical = guard.path().join("sprints");
        assert!(!canonical.exists());
        assert!(!guard.path().join("crew").join("sprints").exists());
        assert_eq!(sprints_dir(), canonical);
    }

    #[serial]
    #[test]
    fn role_pins_path_resolves_canonical_first_then_legacy_then_canonical() {
        // Pins is a file (not a dir) so it has its own resolution. Mirror
        // the same four-case table.
        let guard = TestCrewRoot::new();
        let canonical = guard.path().join("role-model-pins.json");
        let legacy = guard.path().join("crew").join("role-model-pins.json");

        // Neither: canonical
        assert!(!canonical.is_file());
        assert!(!legacy.is_file());
        assert_eq!(role_pins_path(), canonical);

        // Legacy only: legacy
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "{}").unwrap();
        assert_eq!(role_pins_path(), legacy);

        // Both: canonical wins
        std::fs::write(&canonical, "{}").unwrap();
        assert_eq!(role_pins_path(), canonical);
    }

    #[serial]
    #[test]
    fn loader_finds_user_role_override_in_legacy_layout() {
        // End-to-end: an operator with the legacy `<root>/crew/roles/`
        // layout still gets their override picked up by load_roles().
        let guard = TestCrewRoot::new();
        let legacy_roles = guard.path().join("crew").join("roles");
        std::fs::create_dir_all(&legacy_roles).unwrap();
        let user_json = r#"{"id":"coder","description":"legacy-layout override","skills":[],"tool_palette":{"allow":["read"],"deny":[]},"escalation_contract":"bail-with-explanation"}"#;
        std::fs::write(legacy_roles.join("coder.json"), user_json).unwrap();

        let roles = load_roles().unwrap();
        let coder = roles.iter().find(|r| r.id == "coder").expect("coder must load");
        assert_eq!(coder.description, "legacy-layout override");
    }

    // ─── #425: autonomous-dispatch preamble ─────────────────────────────

    #[test]
    fn embedded_preamble_contains_load_bearing_directives() {
        // Smoke-check that the embedded preamble file has the
        // directives it's meant to carry. A future edit that
        // accidentally truncated the file or removed key phrasing
        // would fail this test.
        let preamble = AUTONOMOUS_DISPATCH_PREAMBLE;
        assert!(preamble.contains("autonomous dispatch mode"));
        assert!(preamble.contains("cannot ask questions"));
        assert!(preamble.contains("cannot pause"));
        assert!(preamble.contains("BLOCKED:"));
    }

    /// (#442) Budget-aware prompt-engineering section pins the four
    /// bounds, their failure modes, and the floor-not-ceiling framing.
    /// Future edits that silently revert these load-bearing phrases
    /// would dilute the budget-awareness signal and reopen the
    /// Parkinson's-law concern.
    ///
    /// Deliberately does NOT assert specific numeric values — the
    /// preamble describes the bounds conceptually; the live values
    /// surface in the dynamic Dispatch budget block at compaction
    /// time. Pinning numerics here would couple the prompt to the
    /// runtime's current tuning and silently lie after a retune.
    #[test]
    fn embedded_preamble_carries_bounded_dispatch_prompt_engineering() {
        let preamble = AUTONOMOUS_DISPATCH_PREAMBLE;
        // Four-bound enumeration — by concept, not value.
        assert!(
            preamble.contains("Turn cap"),
            "preamble must name the turn cap"
        );
        assert!(
            preamble.contains("Per-turn token cap"),
            "preamble must name the per-turn cap"
        );
        assert!(
            preamble.contains("Cumulative completion-token cap"),
            "preamble must name the cumulative-token cap"
        );
        assert!(
            preamble.contains("Wall-clock deadline"),
            "preamble must name the wall-clock deadline"
        );
        // Failure-mode names each bound emits — these are operator-
        // visible JSON envelope strings, stable contract.
        assert!(
            preamble.contains("result: \"max_turns\""),
            "preamble must name the max_turns terminal"
        );
        assert!(
            preamble.contains("escalation_intra_turn_stall_exhausted"),
            "preamble must name the intra-turn-stall escalation"
        );
        assert!(
            preamble.contains("escalation_cumulative_tokens_exceeded"),
            "preamble must name the cumulative-tokens escalation"
        );
        // Floor-not-ceiling framing — the key prompt-engineering move
        // that guards against Parkinson's-law expansion.
        assert!(
            preamble.contains("*floor*, not a *ceiling*"),
            "preamble must use floor-not-ceiling framing"
        );
        // Behavioral guidance must mention reasoning tokens count
        // (the dominant failure mode from Beat 47).
        assert!(
            preamble.contains("Reasoning tokens count"),
            "preamble must warn that reasoning tokens count toward the caps"
        );
        // Operator-tunable signal — names that values live in the
        // dynamic Dispatch budget block, not in the preamble.
        assert!(
            preamble.contains("operator-configurable"),
            "preamble must signal that cap values are operator-tunable, \
             not hardcoded into the prompt"
        );
    }

    #[serial]
    #[test]
    fn load_preamble_returns_embedded_when_no_user_override() {
        let _guard = TestCrewRoot::new();
        let preamble = load_autonomous_dispatch_preamble();
        assert!(preamble.contains("autonomous dispatch mode"));
    }

    #[serial]
    #[test]
    fn load_preamble_returns_user_override_when_present() {
        let guard = TestCrewRoot::new();
        let override_path = guard.path().join("AUTONOMOUS_DISPATCH_PREAMBLE.md");
        std::fs::write(&override_path, "# CUSTOM OPERATOR PREAMBLE\nbe brief.").unwrap();
        let preamble = load_autonomous_dispatch_preamble();
        assert!(preamble.contains("CUSTOM OPERATOR PREAMBLE"));
        assert!(!preamble.contains("autonomous dispatch mode"));
    }

    #[test]
    fn role_family_default_is_specialist() {
        // Field absent on Role manifests defaults to specialist
        // (preventive: better to prepend an unneeded preamble than to
        // miss prepending a needed one).
        let role = Role {
            id: "test-role".into(),
            description: "A test role".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            tier: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        };
        assert!(role.is_specialist());
    }

    #[test]
    fn role_family_admin_opts_out_of_specialist() {
        let role = Role {
            id: "test-role".into(),
            description: "A test role".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            tier: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: Some("admin".into()),
            feedback_templates: None,
        };
        assert!(!role.is_specialist());
    }

    #[test]
    fn role_family_explicit_specialist_matches_default() {
        let role = Role {
            id: "test-role".into(),
            description: "A test role".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            tier: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: Some("specialist".into()),
            feedback_templates: None,
        };
        assert!(role.is_specialist());
    }

    #[serial]
    #[test]
    fn mission_compiler_manifest_declares_role_family_admin() {
        // The one canonical admin role today. Pins the manifest's
        // role_family value so a future edit that accidentally
        // removed it would fail loudly + force a conscious decision.
        let _guard = TestCrewRoot::new();
        let roles = load_roles().expect("load_roles must succeed against embedded defaults");
        let mc = roles.iter()
            .find(|r| r.id == "mission-compiler")
            .expect("mission-compiler must be in the embedded role registry");
        assert_eq!(mc.role_family.as_deref(), Some("admin"),
            "mission-compiler must be tagged role_family: admin — it's the canonical bounded-I/O role; \
             retagging requires explicit operator decision since it changes preamble behavior");
        assert!(!mc.is_specialist());
    }

    #[serial]
    #[test]
    fn other_embedded_roles_default_to_specialist() {
        // Every embedded role OTHER than mission-compiler should
        // default to specialist (either via absent field or explicit
        // "specialist"). The preamble-prepend logic depends on this
        // invariant being stable.
        let _guard = TestCrewRoot::new();
        let roles = load_roles().expect("load_roles must succeed against embedded defaults");
        for r in &roles {
            if r.id == "mission-compiler" {
                continue;
            }
            assert!(r.is_specialist(),
                "role `{}` must be specialist (default or explicit) — only mission-compiler is admin today",
                r.id);
        }
    }
}
