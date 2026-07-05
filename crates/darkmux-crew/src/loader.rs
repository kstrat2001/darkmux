//! Load crew architecture manifests (roles, crews, missions, sprints) by id.
//!
//! Search order: user dir → binary-embedded built-ins.

#![allow(dead_code)]

use crate::types::*;
use darkmux_types::paths::{resolve, ResolveScope};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
#[cfg(test)]
use std::env;
use std::fs;
use std::path::PathBuf;

/// Roles compiled into the binary at build time. Filename = `<id>.json`.
const BUILTIN_ROLES: &[(&str, &str)] = &[
    ("coder", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/coder.json"))),
    ("scribe", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/scribe.json"))),
    ("code-reviewer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/code-reviewer.json"))),
    // Tool-less PR reviewer for CI: reads a diff, emits cite-the-line JSON a
    // workflow posts as inline PR comments. Empty tool palette by design.
    ("pr-reviewer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/pr-reviewer.json"))),
    // (#1113) Agentic PR reviewer: repo checked out, read/exec tools, FREEFORM
    // marker-block output (deliberately no output_schema — a grammar lock
    // combined with tools makes the model skip tool-calling and fabricate).
    // `darkmux pr-review render` parses the markers into inline comments.
    ("pr-reviewer-agentic", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/pr-reviewer-agentic.json"))),
    // (#1196) Tool-call bench harness role: the full runtime belt via an
    // EXPLICIT allow-list (empty palette = whole catalog, silently — the
    // #1197 bench-role rule) and NO output_schema (schema+tools makes the
    // model fabricate instead of calling tools; fabrication under the
    // freeform ANSWER:/BLOCKED: contract is what the bench measures).
    ("tool-bench", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/tool-bench.json"))),
    ("analyst", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/analyst.json"))),
    ("voice-editor", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/voice-editor.json"))),
    ("design-reviewer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/design-reviewer.json"))),
    ("test-designer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/test-designer.json"))),
    ("lab-manager", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/lab-manager.json"))),
    ("mission-compiler", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/mission-compiler.json"))),
    // Non-SWE engagement roles (#141): trip planning, health, athletics, legal.
    // Each is bounded — research/organize/structure only; no exec, no execution
    // of bookings or commitments. Each prompt's opening lines name what the role
    // is NOT, to keep cross-bounds questions from leaking into licensed-
    // professional territory.
    ("trip-researcher", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/trip-researcher.json"))),
    ("logistics-coordinator", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/logistics-coordinator.json"))),
    ("health-research", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/health-research.json"))),
    ("fitness-coach", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/fitness-coach.json"))),
    ("legal-research", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/legal-research.json"))),
];

/// Skills compiled into the binary at build time. Filename = `<id>.json`.
const BUILTIN_SKILLS: &[(&str, &str)] = &[
    ("coding", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/coding.json"))),
    ("documenting", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/documenting.json"))),
    ("test-designing", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/test-designing.json"))),
    ("code-reviewing", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/code-reviewing.json"))),
    ("analyzing", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/analyzing.json"))),
    ("writing", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/writing.json"))),
    ("voice-editing", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/voice-editing.json"))),
    ("lab-running", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/lab-running.json"))),
    ("design-reviewing", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/design-reviewing.json"))),
    ("mission-compiling", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/mission-compiling.json"))),
];

/// Role system prompts (`.md`) compiled into the binary. Used as the
/// fallback source when no user-side `<crew_root>/roles/<id>.md` exists.
/// One entry per role advertised in `BUILTIN_ROLES`; the
/// `crew_role_prompt_coverage` doctor check verifies this invariant.
pub(crate) const BUILTIN_ROLE_PROMPTS: &[(&str, &str)] = &[
    ("coder", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/coder.md"))),
    ("scribe", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/scribe.md"))),
    ("code-reviewer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/code-reviewer.md"))),
    ("pr-reviewer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/pr-reviewer.md"))),
    ("pr-reviewer-agentic", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/pr-reviewer-agentic.md"))),
    ("tool-bench", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/tool-bench.md"))),
    ("mission-compiler", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/mission-compiler.md"))),
    ("analyst", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/analyst.md"))),
    ("design-reviewer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/design-reviewer.md"))),
    ("lab-manager", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/lab-manager.md"))),
    ("test-designer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/test-designer.md"))),
    ("voice-editor", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/voice-editor.md"))),
    // Non-SWE engagement roles (#141). Order mirrors BUILTIN_ROLES.
    ("trip-researcher", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/trip-researcher.md"))),
    ("logistics-coordinator", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/logistics-coordinator.md"))),
    ("health-research", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/health-research.md"))),
    ("fitness-coach", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/fitness-coach.md"))),
    ("legal-research", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/legal-research.md"))),
];

/// (#425) Autonomous-dispatch preamble — prepended to specialist
/// role prompts at dispatch-time so the model knows upfront it
/// can't ask questions, can't pause, and should escalate
/// explicitly if blocked. Utility roles (bounded-I/O transformers
/// like mission-compiler) skip the preamble since they don't run
/// agent loops.
const AUTONOMOUS_DISPATCH_PREAMBLE: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/AUTONOMOUS_DISPATCH_PREAMBLE.md"));

/// (#425) Resolve the autonomous-dispatch preamble — operator-side
/// override at `<crew_root>/AUTONOMOUS_DISPATCH_PREAMBLE.md` wins
/// if present; otherwise the embedded default is returned.
///
/// The override path lets operators tune nudge wording per fleet /
/// per machine without recompiling. Lives in the crew root rather
/// than per-role so the preamble stays uniform across the
/// specialist set.
pub(crate) fn load_autonomous_dispatch_preamble() -> String {
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
pub fn builtin_roles_ids() -> Vec<&'static str> {
    BUILTIN_ROLES.iter().map(|(id, _)| *id).collect()
}

/// Same shape for embedded role-prompt ids. Used by doctor's
/// `crew_role_prompt_coverage` check.
pub fn builtin_role_prompt_ids() -> Vec<&'static str> {
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
pub(crate) fn crew_root() -> PathBuf {
    // env(DARKMUX_CREW_DIR) > config.dirs.crew > <root>/crew (#661 Slice 3).
    // (#1012) ForceUser, NOT Auto: crew manifests are operator/fleet-level state.
    // Auto flipped to PROJECT scope the moment a bare `<cwd>/.darkmux/` existed
    // (e.g. created by repo-tier `lessons add` or lab runs), silently shadowing
    // the operator's user-scope crew/missions. DARKMUX_HOME + the explicit
    // override above still win; only the default no longer hijacks on a stray dir.
    darkmux_types::config_access::crew_dir_override()
        .unwrap_or_else(|| resolve(ResolveScope::ForceUser).crew)
}

/// User-state root for the post-Beat-33 flattened layout. Returns
/// `DARKMUX_CREW_DIR` if set (operator override; unchanged semantics —
/// the env var points at the directory CONTAINING the subdirs, with no
/// `crew/` nesting), otherwise `<paths.root>` (e.g., `~/.darkmux/`).
pub fn user_state_root() -> PathBuf {
    // Same override as crew_root, but the no-override default is the bare root
    // (no `crew/` nesting). env(DARKMUX_CREW_DIR) > config.dirs.crew > <root>.
    // (#1012) ForceUser, NOT Auto — missions/sprints are operator-level work
    // tracking (the operator's board lives in ~/.darkmux), never a project's
    // stray `.darkmux/`. Auto made them VANISH from the CLI + viewer the moment
    // a repo got a bare `.darkmux/` (from `lessons add` / lab runs). DARKMUX_HOME
    // and the explicit override still win.
    darkmux_types::config_access::crew_dir_override()
        .unwrap_or_else(|| resolve(ResolveScope::ForceUser).root)
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
pub(crate) fn roles_dir() -> PathBuf {
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
pub(crate) fn crews_dir() -> PathBuf {
    resolve_user_subdir("crews")
}

/// User-side skills directory (operator overrides). Post-Beat-33:
/// `<root>/skills/`. Falls back to `<root>/crew/skills/`
/// for legacy.
pub(crate) fn skills_dir() -> PathBuf {
    resolve_user_subdir("skills")
}

/// User-side role-model-pins.json path. Post-Beat-33:
/// `<root>/role-model-pins.json`. Falls back to
/// `<root>/crew/role-model-pins.json` for legacy.
pub(crate) fn role_pins_path() -> PathBuf {
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
pub(crate) fn load_role_prompt(role_id: &str) -> Option<String> {
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

/// (#906) Defense-in-depth cap on a single manifest file. Role / mission /
/// sprint / crew manifests are small (a few KB); a multi-MB file is either
/// corrupt or hostile, and an unbounded `read_to_string` + `from_str` is a
/// needless memory-amplification surface. 1 MiB is far above any real
/// manifest while still bounding the blast radius.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

fn read_json<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Result<T> {
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > MAX_MANIFEST_BYTES {
            anyhow::bail!(
                "manifest at {} is {} bytes — exceeds the {MAX_MANIFEST_BYTES} byte cap; \
                 refusing to parse (corrupt or hostile manifest?)",
                path.display(),
                meta.len()
            );
        }
    }
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
        // #892: strip_suffix removes exactly one ".json" — trim_end_matches
        // would strip repeated trailing matches (`foo.json.json` -> `foo`).
        let stem = name.strip_suffix(".json").unwrap_or(name);
        match read_json::<Role>(&path) {
            Ok(role) => {
                // #892: the body `id` is authoritative; warn when the filename
                // stem disagrees so a misnamed manifest doesn't surprise the
                // operator (operator-sovereignty: surface, don't silently
                // mishandle).
                if role.id != stem {
                    eprintln!(
                        "warning: role manifest {path:?} is filed as '{stem}.json' but its id is \
                         '{}' — the id field wins; rename the file to '{}.json' to avoid confusion",
                        role.id, role.id
                    );
                }
                results.push((role.id.clone(), role));
            }
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
        // #892: strip exactly one ".json" suffix (see read_all_roles).
        let id = name.strip_suffix(".json").unwrap_or(name).to_string();
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
        validate_role_family(&role, RoleSource::User)?;
        map.insert(role.id.clone(), role);
    }

    // Built-in roles fill in any ids not covered by user files.
    for (id, json) in BUILTIN_ROLES {
        match serde_json::from_str::<Role>(json) {
            Ok(role) => {
                // Defense-in-depth: builtin manifests get the same
                // validation as user-authored ones. A future builtin
                // drift back to `admin` would bail loudly here rather
                // than silently shipping the wrong family.
                validate_role_family(&role, RoleSource::Builtin)?;
                map.entry(id.to_string()).or_insert(role);
            }
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

/// Where a Role came from. Drives the error message in
/// `validate_role_family` — user-authored manifests get an actionable
/// file path; builtin manifests can't be operator-edited and get a
/// "please file an issue" message instead.
#[derive(Debug, Clone, Copy)]
enum RoleSource {
    /// Operator-authored manifest under `roles_dir()`.
    User,
    /// Embedded manifest from `templates/builtin/roles/` — operator
    /// cannot edit it, so the actionable repair is "file an issue."
    Builtin,
}

/// Validate a role's `role_family` field. The family is a validated
/// **two-value axis** (#590): `"specialist"` (works the mission/sprints)
/// or `"utility"` (supports the runtime outside mission scope), or absent
/// (defaults to specialist). Anything else loud-fails at the loader
/// boundary:
///
/// - The legacy `"admin"` value (renamed to `"utility"` in the
///   codebase-wide nomenclature transition; no pre-1.0 compat alias) gets
///   a targeted migration message.
/// - Any other unknown value (a typo like `"utilty"`) is rejected rather
///   than silently treated as specialist — a silent misclassification is
///   exactly the bug a validated axis prevents.
///
/// Each error names the offending role, the value, and a repair path
/// appropriate to where the manifest came from.
fn validate_role_family(role: &Role, source: RoleSource) -> Result<()> {
    match role.role_family.as_deref() {
        // The two recognized families, or absent (defaults to specialist).
        None | Some("specialist") | Some("utility") => Ok(()),
        // Legacy value renamed to "utility" — keep the targeted migration message.
        Some("admin") => Err(match source {
            RoleSource::User => anyhow::anyhow!(
                "role `{}` has `role_family: \"admin\"`, which was renamed to \"utility\" \
                 in the codebase-wide nomenclature transition. Pre-1.0 there's no \
                 compat alias — update your role manifest at `{}/{}.json` to set \
                 `\"role_family\": \"utility\"` (the canonical bounded-I/O family).",
                role.id,
                roles_dir().display(),
                role.id
            ),
            RoleSource::Builtin => anyhow::anyhow!(
                "internal regression: builtin role `{}` declares the legacy \
                 `role_family: \"admin\"` value, which was renamed to \"utility\" \
                 in the codebase-wide nomenclature transition. This is not an \
                 operator-actionable error — please file an issue at \
                 https://github.com/kstrat2001/darkmux/issues with the role id \
                 (`{}`) and your darkmux version (`darkmux --version`).",
                role.id, role.id
            ),
        }),
        // Any other value is a typo / unknown family — fail loud.
        Some(other) => Err(match source {
            RoleSource::User => anyhow::anyhow!(
                "role `{}` has `role_family: \"{}\"`, which is not a recognized family. \
                 Valid values are \"specialist\" (works the mission/sprints) or \"utility\" \
                 (supports the runtime outside mission scope); omit the field to default to \
                 \"specialist\". Update your role manifest at `{}/{}.json`.",
                role.id,
                other,
                roles_dir().display(),
                role.id
            ),
            RoleSource::Builtin => anyhow::anyhow!(
                "internal regression: builtin role `{}` declares an unrecognized \
                 `role_family: \"{}\"` (valid: \"specialist\" / \"utility\"). Please file an \
                 issue at https://github.com/kstrat2001/darkmux/issues with the role id \
                 (`{}`) and your darkmux version (`darkmux --version`).",
                role.id, other, role.id
            ),
        }),
    }
}

/// Load all crews.
pub(crate) fn load_crews() -> Result<Vec<Crew>> {
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
    use crate::lifecycle;
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
    use crate::lifecycle;
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
pub(crate) fn load_skills() -> Result<Vec<Skill>> {
    let user_dir = skills_dir();
    let mut map: BTreeMap<String, Skill> = read_all_json::<Skill>(&user_dir)?
        .into_iter()
        .map(|(stem, skill)| {
            // #892: key on the authoritative body id, not the filename stem,
            // so a user skill filed under a mismatched name still overrides the
            // builtin of the same id (keying on the stem left both lingering).
            // Warn on the mismatch (operator-sovereignty: surface it).
            if skill.id != stem {
                eprintln!(
                    "warning: skill manifest '{stem}.json' has id '{}' — the id field wins; \
                     rename the file to '{}.json' to avoid confusion",
                    skill.id, skill.id
                );
            }
            (skill.id.clone(), skill)
        })
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
pub(crate) fn resolve_role_prompt_path(
    role: &Role,
    paths: &darkmux_types::paths::DarkmuxPaths,
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

    /// (#1053) The pr-reviewer prompt must carry the intent-assessment
    /// directive — the cross-tier-validated lever against "restate-the-fix"
    /// false positives (a reviewer flagging the very bug a PR fixes). The
    /// self-review workflow supplies the PR title + description; this directive
    /// is what tells the model to judge the diff against that stated intent. A
    /// future edit that dropped it would silently regress the behavior the
    /// bench A/B (8B and 122B, with-intent → clean pass) proved.
    #[test]
    fn pr_reviewer_prompt_carries_intent_directive() {
        let prompt = BUILTIN_ROLE_PROMPTS
            .iter()
            .find(|(id, _)| *id == "pr-reviewer")
            .map(|(_, c)| *c)
            .expect("pr-reviewer prompt must be embedded");
        assert!(
            prompt.contains("stated intent"),
            "pr-reviewer.md must instruct assessing against the change's stated intent (#1053)"
        );
        assert!(
            prompt.contains("achieves its stated purpose"),
            "pr-reviewer.md must frame a correct change as one that achieves its stated purpose (#1053)"
        );
    }

    /// (#1113) The agentic PR reviewer's contract, pinned:
    /// - NO `output_schema` on the manifest — a grammar-constrained output
    ///   combined with tools makes the model skip tool-calling entirely and
    ///   fabricate plausible output (verified empirically 2026-07-04). The
    ///   freeform marker contract exists BECAUSE of that; a future edit that
    ///   "helpfully" adds a schema back would break the role silently.
    /// - The prompt must carry the marker + verdict-line contract the render
    ///   verb parses, the explore-before-concluding directive (the 2-turn
    ///   near-miss vs 5-turn access-gap-catch finding), the no-cap and
    ///   no-self-downgrade language, and the intent-assessment directive
    ///   (#1053, same lever as the tool-less reviewer).
    #[test]
    fn pr_reviewer_agentic_contract() {
        let role = load_roles()
            .expect("builtin roles load")
            .into_iter()
            .find(|r| r.id == "pr-reviewer-agentic")
            .expect("pr-reviewer-agentic must be embedded");
        assert!(
            role.output_schema.is_none(),
            "pr-reviewer-agentic must NOT declare an output_schema — schema+tools \
             makes the model fabricate instead of exploring (#1113)"
        );
        assert!(
            !role.tool_palette.allow.is_empty(),
            "pr-reviewer-agentic must grant tools — exploration is its whole point"
        );
        let prompt = BUILTIN_ROLE_PROMPTS
            .iter()
            .find(|(id, _)| *id == "pr-reviewer-agentic")
            .map(|(_, c)| *c)
            .expect("pr-reviewer-agentic prompt must be embedded");
        for needle in [
            "MUST FIX",
            "CONSIDER",
            "VERDICT:",
            "stated intent",
            "no cap on findings",
            "Never talk yourself out of a finding",
            "Explore before concluding",
        ] {
            assert!(
                prompt.contains(needle),
                "pr-reviewer-agentic.md must contain {needle:?} (#1113 contract)"
            );
        }
    }

    /// (#1196) The tool-bench harness role's contract, pinned:
    /// - An EXPLICIT non-empty tool allow-list. An empty palette makes the
    ///   dispatch path expose the runtime's FULL catalog silently
    ///   (`compute_runtime_allowed_tools` returns `None`) — a bench role must
    ///   never rely on that, per the #1197 bench-role rule. The list must
    ///   cover the whole belt the bench measures (read brings search; exec
    ///   brings bash).
    /// - NO `output_schema` — schema+tools makes the model skip tool-calling
    ///   and fabricate; the ANSWER:/BLOCKED: line contract replaces it, and
    ///   fabrication under it is exactly what the bench scores.
    /// - SPECIALIST family — the autonomous-dispatch preamble carries the
    ///   `BLOCKED:` escalation convention the termination axis scores
    ///   against; a utility role would skip the preamble.
    /// - The prompt must carry the ANSWER/BLOCKED contract, the provenance
    ///   rule (only report tokens observed in tool results), and the
    ///   DMX- token definition the provider's scoring parses against.
    #[test]
    fn tool_bench_role_contract() {
        let role = load_roles()
            .expect("builtin roles load")
            .into_iter()
            .find(|r| r.id == "tool-bench")
            .expect("tool-bench must be embedded");
        assert!(
            role.output_schema.is_none(),
            "tool-bench must NOT declare an output_schema — schema+tools makes the \
             model fabricate instead of calling tools (#1196)"
        );
        for tool in ["read", "write", "edit", "exec"] {
            assert!(
                role.tool_palette.allow.iter().any(|t| t == tool),
                "tool-bench palette must EXPLICITLY allow {tool:?} — the full belt, \
                 never the empty-palette full-catalog fallback (#1197 bench-role rule)"
            );
        }
        assert!(
            role.is_specialist(),
            "tool-bench must be a specialist role so the autonomous-dispatch preamble \
             (the BLOCKED: escalation convention) is injected (#1196 termination axis)"
        );
        let prompt = BUILTIN_ROLE_PROMPTS
            .iter()
            .find(|(id, _)| *id == "tool-bench")
            .map(|(_, c)| *c)
            .expect("tool-bench prompt must be embedded");
        for needle in [
            "ANSWER:",
            "BLOCKED:",
            "DMX-",
            "actually observed in a tool result",
            "Never construct, guess",
        ] {
            assert!(
                prompt.contains(needle),
                "tool-bench.md must contain {needle:?} (#1196 contract)"
            );
        }
    }

    /// (#1053 quote-resolve) The pr-reviewer prompt must instruct the model to
    /// quote the line (`anchor`) rather than emit a line number — the workflow
    /// resolves the quote to a coordinate deterministically. The n=5 A/B showed
    /// model-emitted line numbers mis-localize (the construct it names is right,
    /// the integer is ~20 lines off); a regression back to line-numbers would
    /// silently bring that back. The output schema (`pr-reviewer.json`) must
    /// agree — it carries `anchor`, not `line`.
    #[test]
    fn pr_reviewer_prompt_and_schema_use_quote_anchor() {
        let prompt = BUILTIN_ROLE_PROMPTS
            .iter()
            .find(|(id, _)| *id == "pr-reviewer")
            .map(|(_, c)| *c)
            .expect("pr-reviewer prompt must be embedded");
        assert!(
            prompt.contains("anchor"),
            "pr-reviewer.md must describe the `anchor` (quote-the-line) field (#1053)"
        );
        assert!(
            prompt.contains("do not output a line number"),
            "pr-reviewer.md must tell the model NOT to emit a line number (#1053)"
        );
        let role_json = BUILTIN_ROLES
            .iter()
            .find(|(id, _)| *id == "pr-reviewer")
            .map(|(_, c)| *c)
            .expect("pr-reviewer manifest must be embedded");
        let role: Role = serde_json::from_str(role_json).expect("pr-reviewer manifest parses");
        let schema = role.output_schema.expect("pr-reviewer has an output_schema");
        let props = &schema["properties"]["findings"]["items"]["properties"];
        assert!(props.get("anchor").is_some(), "finding schema must carry `anchor` (#1053)");
        assert!(props.get("line").is_none(), "finding schema must NOT carry `line` (replaced by anchor, #1053)");
    }

    /// (#1038) Every builtin role's `output_schema`, if present, must be
    /// LMStudio-grammar-safe — the runtime sends it with `strict: true`, and a
    /// non-conforming schema makes LMStudio reject the request on the FIRST real
    /// dispatch (a backend error that no unit test catches because nothing else
    /// loads these manifests AND calls LMStudio — only a live dogfood does).
    /// Two rules, learned the hard way. First, the OpenAI strict contract: every
    /// object schema sets `additionalProperties: false` and lists EVERY declared
    /// property in `required`. Second, `type` is ALWAYS a single string, never an
    /// array — LMStudio's grammar compiler rejects the union form
    /// `"type": ["string","null"]` with `ValueError: 'type' must be a string`
    /// (the regression that shipped in #1039 and broke every dispatch; nullable
    /// goes through `anyOf: [{type:string},{type:null}]` instead). Walks nested
    /// objects, array items, and anyOf/oneOf/allOf branches so a future schema
    /// edit can't silently drop either invariant.
    #[test]
    fn builtin_role_output_schemas_are_strict_safe() {
        fn assert_strict_safe(schema: &serde_json::Value, role: &str, path: &str) {
            let Some(obj) = schema.as_object() else { return };
            // Rule 2: `type` must be a single string, never an array (LMStudio).
            if let Some(ty) = obj.get("type") {
                assert!(
                    ty.is_string(),
                    "role `{role}` schema at `{path}`: `type` must be a single string, not {ty} — LMStudio rejects the union form (use anyOf for nullable)",
                );
            }
            // Recurse into array items + schema-composition branches.
            if let Some(items) = obj.get("items") {
                assert_strict_safe(items, role, &format!("{path}[]"));
            }
            for kw in ["anyOf", "oneOf", "allOf"] {
                if let Some(branches) = obj.get(kw).and_then(|b| b.as_array()) {
                    for (i, branch) in branches.iter().enumerate() {
                        assert_strict_safe(branch, role, &format!("{path}.{kw}[{i}]"));
                    }
                }
            }
            // Only object-typed schemas carry the properties/required contract.
            let Some(props) = obj.get("properties").and_then(|p| p.as_object()) else {
                return;
            };
            assert_eq!(
                obj.get("additionalProperties"),
                Some(&serde_json::Value::Bool(false)),
                "role `{role}` schema at `{path}`: object must set additionalProperties:false for strict:true",
            );
            let required: Vec<&str> = obj
                .get("required")
                .and_then(|r| r.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            for key in props.keys() {
                assert!(
                    required.contains(&key.as_str()),
                    "role `{role}` schema at `{path}`: property `{key}` must be in `required` for strict:true (optionals stay required, nullable via anyOf)",
                );
                assert_strict_safe(&props[key], role, &format!("{path}.{key}"));
            }
        }

        for (id, json) in BUILTIN_ROLES {
            let role: Role = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("builtin role `{id}` must parse: {e}"));
            if let Some(schema) = &role.output_schema {
                assert_strict_safe(schema, id, "$");
            }
        }
    }

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
            output_schema: None,
            id: "test".into(),
            description: "A test role".into(),
            skills: vec![String::from("coding")],
            tool_palette: ToolPalette { allow: vec!["read".to_string(), "edit".to_string()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
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
        assert_eq!(coder.description, "Implements features described by the user. Reads existing code first; follows patterns; runs tests before reporting done.");
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
            output_schema: None,
            id: "test-role".into(),
            description: "A test role".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        };
        assert!(role.is_specialist());
    }

    #[test]
    fn role_family_utility_opts_out_of_specialist() {
        let role = Role {
            output_schema: None,
            id: "test-role".into(),
            description: "A test role".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: Some("utility".into()),
            feedback_templates: None,
        };
        assert!(!role.is_specialist());
    }

    /// Legacy `"admin"` value (renamed to `"utility"` in the
    /// admin→utility nomenclature transition) is treated as
    /// specialist by `is_specialist()` itself — silent fallthrough
    /// at the matcher layer. The loud-fail lives at the loader
    /// boundary (`validate_role_family`) so the matcher stays
    /// branch-free. See `validate_rejects_legacy_admin_role_family`.
    #[test]
    fn legacy_admin_value_is_silently_specialist_at_matcher_layer() {
        let role = Role {
            output_schema: None,
            id: "test-role".into(),
            description: "A test role".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: Some("admin".into()),
            feedback_templates: None,
        };
        // Matcher only recognizes "utility" — legacy "admin" falls
        // through to specialist (preventive default). Production paths
        // rely on the loader's validator catching this before the
        // matcher ever sees it.
        assert!(role.is_specialist());
    }

    #[test]
    fn role_family_explicit_specialist_matches_default() {
        let role = Role {
            output_schema: None,
            id: "test-role".into(),
            description: "A test role".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: Some("specialist".into()),
            feedback_templates: None,
        };
        assert!(role.is_specialist());
    }

    #[serial]
    #[test]
    fn mission_compiler_manifest_declares_role_family_utility() {
        // A canonical utility role (#590 also tags scribe, and will add
        // the compactor). Pins the manifest's role_family value so a
        // future edit that accidentally removed it would fail loudly.
        let _guard = TestCrewRoot::new();
        let roles = load_roles().expect("load_roles must succeed against embedded defaults");
        let mc = roles.iter()
            .find(|r| r.id == "mission-compiler")
            .expect("mission-compiler must be in the embedded role registry");
        assert_eq!(mc.role_family.as_deref(), Some("utility"),
            "mission-compiler must be tagged role_family: utility — it's the canonical bounded-I/O role; \
             retagging requires explicit operator decision since it changes preamble behavior");
        assert!(!mc.is_specialist());
    }

    #[serial]
    #[test]
    fn embedded_roles_declare_expected_families() {
        // Pin the specialist/utility split across the built-in roster (#590):
        // utility = supports the runtime outside mission scope; specialist =
        // works the mission/sprints. Every built-in declares the family
        // EXPLICITLY (no implicit default), and a retag must be a conscious
        // edit here. The preamble-prepend logic depends on this split.
        let _guard = TestCrewRoot::new();
        let roles = load_roles().expect("load_roles must succeed against embedded defaults");
        let utility: std::collections::BTreeSet<&str> =
            ["mission-compiler", "scribe"].into_iter().collect();
        for r in &roles {
            assert!(
                r.role_family.is_some(),
                "built-in role `{}` must declare role_family explicitly (#590)",
                r.id
            );
            if utility.contains(r.id.as_str()) {
                assert_eq!(
                    r.role_family.as_deref(),
                    Some("utility"),
                    "role `{}` must be utility (supports the runtime outside mission scope)",
                    r.id
                );
                assert!(!r.is_specialist());
            } else {
                assert_eq!(
                    r.role_family.as_deref(),
                    Some("specialist"),
                    "role `{}` must be specialist (works the mission/sprints)",
                    r.id
                );
                assert!(r.is_specialist());
            }
        }
    }

    /// Loader rejects the legacy `"admin"` role_family value from a
    /// user-authored manifest with an operator-actionable error
    /// message that names the rename, points at the offending file
    /// path, and tells the operator what to change. Pre-1.0 no-compat
    /// doctrine — no silent rewrite, no env-var alias.
    #[test]
    fn validate_rejects_legacy_admin_role_family_user_source() {
        let legacy_role = Role {
            output_schema: None,
            id: "legacy-role".into(),
            description: "A role using the pre-rename admin value".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: Some("admin".into()),
            feedback_templates: None,
        };
        let err = super::validate_role_family(&legacy_role, super::RoleSource::User).expect_err(
            "validator must reject the legacy `admin` value on user-authored roles"
        );
        let msg = format!("{err}");
        assert!(msg.contains("legacy-role"), "msg names the offending role: {msg}");
        assert!(msg.contains("\"admin\""), "msg names the legacy value: {msg}");
        assert!(msg.contains("\"utility\""), "msg names the new value: {msg}");
        // User-source message points at the editable file path.
        assert!(msg.contains("legacy-role.json"), "msg points at the file to edit: {msg}");
    }

    /// Builtin-source rejection produces a different message — operator
    /// can't edit embedded manifests, so the actionable repair is
    /// "please file an issue" rather than "edit your manifest."
    #[test]
    fn validate_rejects_legacy_admin_role_family_builtin_source() {
        let legacy_role = Role {
            output_schema: None,
            id: "broken-builtin".into(),
            description: "Simulates a builtin manifest that drifted back to admin".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: Some("admin".into()),
            feedback_templates: None,
        };
        let err = super::validate_role_family(&legacy_role, super::RoleSource::Builtin)
            .expect_err("validator must reject the legacy `admin` value on builtin roles");
        let msg = format!("{err}");
        assert!(msg.contains("broken-builtin"), "msg names the offending role: {msg}");
        assert!(msg.contains("internal regression"), "msg flags this is not operator-actionable: {msg}");
        assert!(msg.contains("file an issue"), "msg points to the right repair path: {msg}");
    }

    #[test]
    fn validate_accepts_utility_specialist_and_none() {
        let mut r = Role {
            output_schema: None,
            id: "test-role".into(),
            description: "A test role".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        };
        // Source doesn't matter for the accept path — assert both.
        assert!(super::validate_role_family(&r, super::RoleSource::User).is_ok(), "None+User must pass");
        assert!(super::validate_role_family(&r, super::RoleSource::Builtin).is_ok(), "None+Builtin must pass");
        r.role_family = Some("utility".into());
        assert!(super::validate_role_family(&r, super::RoleSource::User).is_ok(), "utility+User must pass");
        r.role_family = Some("specialist".into());
        assert!(super::validate_role_family(&r, super::RoleSource::User).is_ok(), "specialist+User must pass");
    }

    /// Unknown role_family values (a typo like `"worker"`) are now rejected
    /// rather than silently treated as specialist (#590: validated two-value
    /// axis). User source → actionable; builtin source → regression framing.
    #[test]
    fn validate_rejects_unknown_role_family() {
        let r = Role {
            output_schema: None,
            id: "test-role".into(),
            description: "A role with a typo'd family".into(),
            skills: vec![],
            tool_palette: ToolPalette { allow: vec!["read".into()], deny: vec![] },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: Some("worker".into()),
            feedback_templates: None,
        };
        let err = super::validate_role_family(&r, super::RoleSource::User)
            .expect_err("unknown role_family must be rejected on user roles");
        let msg = format!("{err}");
        assert!(msg.contains("\"worker\""), "names the bad value: {msg}");
        assert!(msg.contains("not a recognized family"), "explains the problem: {msg}");
        assert!(msg.contains("test-role.json"), "points at the file to edit: {msg}");

        let err_b = super::validate_role_family(&r, super::RoleSource::Builtin)
            .expect_err("unknown role_family must be rejected on builtin roles");
        let msg_b = format!("{err_b}");
        assert!(msg_b.contains("internal regression"), "flags not operator-actionable: {msg_b}");
        assert!(msg_b.contains("file an issue"), "points to the repair path: {msg_b}");
    }

    #[test]
    #[serial]
    fn load_skills_user_override_keys_on_body_id_not_filename() {
        // #892: load_skills keyed the dedup map on the filename stem, so a user
        // skill filed under a name != its body id failed to override the
        // builtin of the same id (both lingered). Keying on the body id fixes
        // the override. Goes red pre-fix (two 'coding' skills survive).
        let guard = TestCrewRoot::new();
        let skills_dir = guard.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        // A user override for the builtin "coding" skill, filed under a
        // mismatched filename.
        std::fs::write(
            skills_dir.join("wrong-name.json"),
            r#"{"id":"coding","description":"USER OVERRIDE","keywords":[]}"#,
        )
        .unwrap();

        let coding: Vec<_> = load_skills()
            .unwrap()
            .into_iter()
            .filter(|s| s.id == "coding")
            .collect();
        assert_eq!(
            coding.len(),
            1,
            "exactly one 'coding' skill — the user file must override the builtin by body id"
        );
        assert_eq!(
            coding[0].description, "USER OVERRIDE",
            "the user manifest must win the override"
        );
    }
}
