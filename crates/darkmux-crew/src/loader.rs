//! Load crew architecture manifests (roles, crews, missions, phases) by id.
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
    // The marker parser survives in the lab's review-bench scoring
    // (darkmux-lab review_bench.rs); the `pr-review render` CLI path that
    // once turned markers into inline PR comments retired in #1426.
    ("pr-reviewer-agentic", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/pr-reviewer-agentic.json"))),
    // Sibling of pr-reviewer with the JSON grammar contract (output_schema)
    // dropped — free-form prose review, MUST FIX:/CONSIDER: marked. Exists to
    // measure whether the JSON contract itself suppresses recall vs framing
    // (#1119 review-bench free-form mode); not wired into the CI workflow.
    ("pr-reviewer-freeform", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/pr-reviewer-freeform.json"))),
    // (#1196) Tool-call bench harness role: the full runtime belt via an
    // EXPLICIT allow-list (empty palette = whole catalog, silently — the
    // #1197 bench-role rule) and NO output_schema (schema+tools makes the
    // model fabricate instead of calling tools; fabrication under the
    // freeform ANSWER:/BLOCKED: contract is what the bench measures).
    ("tool-bench", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/tool-bench.json"))),
    // (#1222) Dialectic (adversarial) PR-review seats: prosecution builds the
    // evidenced case against the change, defense answers each charge, judge
    // rules on the presented record. Advocates are agentic (read/exec) with
    // freeform marker contracts (no output_schema — grammar + tools makes the
    // model fabricate); the judge is deliberately tool-less (rules on the
    // record) with a reason-then-fenced-JSON contract.
    ("dialectic-prosecutor", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/dialectic-prosecutor.json"))),
    ("dialectic-defender", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/dialectic-defender.json"))),
    ("dialectic-judge", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/dialectic-judge.json"))),
    // (#1222 Phase B packet 4) Review seats: probe (k-draw, per-bundle
    // defect-finding) and judge (double-confirm ruling on each surviving
    // flag). Both mirror dialectic-judge's tool-less, no-output_schema,
    // bail-with-explanation shape — the review dispatches both through the
    // container-free single-shot chat primitive, never the agent loop.
    ("review-probe", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/review-probe.json"))),
    // (#1475 packet 2) The three DISTINCT probe roles the role->profile flip
    // staffs — each shares the FROZEN probe persona (#1256) byte-for-byte with
    // review-probe (their .md is a verbatim copy); only the role_profiles
    // binding (and thus model) differs. The crew's recall diversity IS these
    // three distinct role->profile->model bindings.
    ("review-probe-high", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/review-probe-high.json"))),
    ("review-probe-mid", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/review-probe-mid.json"))),
    ("review-probe-low", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/review-probe-low.json"))),
    ("review-judge", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/review-judge.json"))),
    // (#1260/#1177) Optional fourth review seat: one adjudication call per
    // double-confirmed finding (in practice staffed by a frontier endpoint).
    // Same tool-less, no-output_schema, bail-with-explanation shape as
    // review-judge — dispatched through the single-shot chat primitive.
    ("review-verify", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/review-verify.json"))),
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
    ("pr-reviewer-freeform", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/pr-reviewer-freeform.md"))),
    ("tool-bench", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/tool-bench.md"))),
    // (#1222) Dialectic PR-review seat prompts. Order mirrors BUILTIN_ROLES.
    ("dialectic-prosecutor", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/dialectic-prosecutor.md"))),
    ("dialectic-defender", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/dialectic-defender.md"))),
    ("dialectic-judge", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/dialectic-judge.md"))),
    // (#1222 Phase B packet 4) Review seat prompts — frozen texts
    // (see the role JSON descriptions above for provenance).
    ("review-probe", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/review-probe.md"))),
    // (#1475 packet 2) The three probe roles' prompts are byte-identical copies
    // of review-probe.md (the FROZEN #1256 persona) — probe recall diversity
    // lives in the role->profile MODEL binding, never in the persona text.
    ("review-probe-high", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/review-probe-high.md"))),
    ("review-probe-mid", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/review-probe-mid.md"))),
    ("review-probe-low", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/review-probe-low.md"))),
    ("review-judge", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/review-judge.md"))),
    // (#1260) The verify seat's persona — frozen text (contract 6): the
    // byte-lock golden lives beside the other review-seat goldens in
    // `darkmux-lab`'s review tests (`verify_prompt_matches_frozen_golden`).
    ("review-verify", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/review-verify.md"))),
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

/// Phases compiled into the binary at build time.
const BUILTIN_PHASES: &[(&str, &str)] = &[];

/// The user-side crew root: `DARKMUX_CREW_DIR` if set, else `<paths.crew>`
/// from the active workspace.
///
/// **Deprecated direction (Beat 33):** prefer the per-subdir helpers
/// `roles_dir()`, `missions_dir()`, `phases_dir()`, `crews_dir()`,
/// and `skills_dir()` for new code. They resolve the post-flatten
/// `<root>/<subdir>/` layout with a backward-
/// compatibility fallback to the legacy `<root>/crew/<subdir>/` layout.
/// `crew_root()` is retained for callers that need the parent directory
/// itself (e.g., daemon banner messages).
pub(crate) fn crew_root() -> PathBuf {
    // env(DARKMUX_CREW_DIR) > config.dirs.crew > <root>/crew (#661 Slice 3).
    // (#1012) ForceUser, NOT Auto: crew manifests are operator/fleet-level state.
    // Auto flipped to PROJECT scope the moment a bare `<cwd>/.darkmux/` existed
    // (e.g. created by repo-tier `memory lesson add` or lab runs), silently shadowing
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
    // (#1012) ForceUser, NOT Auto — missions/phases are operator-level work
    // tracking (the operator's board lives in ~/.darkmux), never a project's
    // stray `.darkmux/`. Auto made them VANISH from the CLI + viewer the moment
    // a repo got a bare `.darkmux/` (from `memory lesson add` / lab runs). DARKMUX_HOME
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

/// User-side phases directory. Post-Beat-33: `<root>/phases/`. Falls
/// back to `<root>/crew/phases/` for operators on the legacy layout.
pub fn phases_dir() -> PathBuf {
    resolve_user_subdir("phases")
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

/// User-side mission-configs directory (#1284 Packet 1) — the top tier of
/// `mission_config::load`'s user → on-disk → embedded resolution, mirroring
/// `workloads::load`'s search order. `<root>/mission-configs/`, honoring the
/// SAME `env(DARKMUX_CREW_DIR) > config.dirs.crew` override every other
/// crew-state dir does (`user_state_root`'s doc). No legacy-layout fallback
/// — mission configs postdate the Beat-33 flatten, so there's no
/// `<root>/crew/mission-configs/` to have ever existed (per the "no compat
/// baggage pre-1.0" doctrine — nothing to be compatible WITH here).
pub fn mission_configs_dir() -> PathBuf {
    user_state_root().join("mission-configs")
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

/// Public accessor for [`load_role_prompt`] (#1222 Phase B packet 5
/// reconciliation). `darkmux mission launch review` (the `darkmux` binary
/// crate, `src/mission_launch_review.rs`) dispatches `review-probe`/`review-judge` through
/// `darkmux_crew::single_shot::single_shot_chat`, not
/// `darkmux_crew::dispatch`/`dispatch_internal` — so it needs the raw
/// system-prompt text itself rather than a full role dispatch, and
/// `load_role_prompt` is `pub(crate)`, invisible outside this crate.
/// Same search order: user override (`<crew_root>/roles/<id>.md`), then
/// the embedded `BUILTIN_ROLE_PROMPTS`.
pub fn role_prompt(role_id: &str) -> Option<String> {
    load_role_prompt(role_id)
}

/// (#906) Defense-in-depth cap on a single manifest file. Role / mission /
/// phase / crew manifests are small (a few KB); a multi-MB file is either
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
/// **two-value axis** (#590): `"specialist"` (works the mission/phases)
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
                 Valid values are \"specialist\" (works the mission/phases) or \"utility\" \
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

/// Load all phases from the new per-mission nested layout.
///
/// Walks every `<crew_root>/missions/<mission-id>/phases/*.json`.  The
/// Phase JSON already carries `mission_id`, so no inference from the dir
/// name is needed.  Legacy flat phase files under `<crew_root>/phases/`
/// are silently ignored — the migration verb is the bridge.
pub fn load_phases() -> Result<Vec<Phase>> {
    use crate::lifecycle;
    let missions_root = missions_dir();
    if !missions_root.is_dir() {
        return Ok(Vec::new());
    }
    // Key by (mission_id, phase_id) so the same phase id is allowed across
    // different missions — that's the composite-PK invariant #148 establishes.
    let mut map: BTreeMap<(String, String), Phase> = BTreeMap::new();
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
        let phases_dir = lifecycle::phases_dir(&mission_id);
        if !phases_dir.is_dir() {
            continue;
        }
        for phase_entry in fs::read_dir(&phases_dir)
            .with_context(|| format!("reading {}", phases_dir.display()))?
        {
            let phase_entry = phase_entry?;
            let phase_path = phase_entry.path();
            if !phase_path.is_file() {
                continue;
            }
            if phase_path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let text = fs::read_to_string(&phase_path)
                .with_context(|| format!("reading {}", phase_path.display()))?;
            match serde_json::from_str::<Phase>(&text) {
                Ok(s) => { map.insert((s.mission_id.clone(), s.id.clone()), s); }
                Err(e) => eprintln!("warning: failed to parse phase at {}: {e}", phase_path.display()),
            }
        }
    }

    // Built-in phases (currently empty — future-proofing).
    for (_id, json) in BUILTIN_PHASES {
        match serde_json::from_str::<Phase>(json) {
            Ok(s) => { map.entry((s.mission_id.clone(), s.id.clone())).or_insert(s); }
            Err(e) => eprintln!("warning: failed to parse builtin phase \"{_id}\": {e}"),
        }
    }

    let mut out: Vec<Phase> = map.into_values().collect();
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
#[path = "loader_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "loader_load_per_mission_tests.rs"]
mod load_per_mission_tests;
