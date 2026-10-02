//! Load crew architecture manifests (roles, crews, missions, phases) by id.
//!
//! Search order: user dir → binary-embedded built-ins.

use crate::retired_state::{self, parse_state, StateKind};
use crate::types::*;
use darkmux_types::paths::{resolve, ResolveScope};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
#[cfg(test)]
use std::env;
use std::fs;
use std::path::PathBuf;

/// Roles compiled into the binary at build time. Filename = `<id>.json`.
/// The radio interpreter's ROUTING role id (#1698). One constant, so the
/// role table, the radio dispatch (`src/radio.rs`) and the utility-job
/// definition ([`crate::usage::call_purpose`], #2914) name the same role.
pub const RADIO_ROUTER_ROLE_ID: &str = "radio-router";

/// The radio answering seat's role id: the one role whose persona
/// ([`crate::radio_persona`]) a fleet job may ask a peer to build for a
/// tool-less single exchange.
pub const RADIO_HOST_ROLE_ID: &str = "radio-host";

pub(crate) const BUILTIN_ROLES: &[(&str, &str)] = &[
    ("coder", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/coder.json"))),
    ("code-reviewer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/code-reviewer.json"))),
    ("crawler", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/crawler.json"))),
    // (#2310 P4c) review.json's unit-<rule> dispatch role — crawler.json's
    // sibling, `dispatch.unit` reused with `role_id: "reviewer"`.
    ("reviewer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/reviewer.json"))),
    // Tool-less PR reviewer for CI: reads a diff, emits cite-the-line JSON a
    // workflow posts as inline PR comments. Empty tool palette by design.
    ("pr-reviewer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/pr-reviewer.json"))),
    // (#1196) Tool-call bench harness role: the full runtime belt via an
    // EXPLICIT allow-list (empty palette = whole catalog, silently — the
    // #1197 bench-role rule) and NO output_schema (schema+tools makes the
    // model fabricate instead of calling tools; fabrication under the
    // freeform ANSWER:/BLOCKED: contract is what the bench measures).
    ("tool-bench", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/tool-bench.json"))),
    // (#1222 Phase B packet 4 - #2418) Review seats (probe: k-draw,
    // per-bundle defect-finding; judge: double-confirm ruling on each
    // surviving flag) were removed here — they were the review funnel's
    // own seats, and the funnel itself (`build_review_graph`) was deleted
    // in #2310 P4d. The shipped `review` config stages its work through
    // `reviewer`/`coder` instead.
    ("analyst", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/analyst.json"))),
    ("design-reviewer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/design-reviewer.json"))),
    ("test-designer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/test-designer.json"))),
    // (#1698 Packet A) The radio interpreter's ROUTING seat — bounded
    // classification over the currently advertised command catalog. See
    // `src/radio.rs`'s module doc for the two-seat receiver architecture.
    (RADIO_ROUTER_ROLE_ID, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/radio-router.json"))),
    // (#1698 Packet B2) The radio interpreter's ANSWERING seat — dispatched
    // only when radio-router refuses. See `src/radio_answer.rs`'s module
    // doc.
    ("radio-host", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/radio-host.json"))),
];

/// Skills compiled into the binary at build time. Filename = `<id>.json`.
pub(crate) const BUILTIN_SKILLS: &[(&str, &str)] = &[
    ("coding", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/coding.json"))),
    ("test-designing", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/test-designing.json"))),
    ("code-reviewing", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/code-reviewing.json"))),
    ("analyzing", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/analyzing.json"))),
    ("lab-running", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/lab-running.json"))),
    ("design-reviewing", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/skills/design-reviewing.json"))),
];

/// Role system prompts (`.md`) compiled into the binary. Used as the
/// fallback source when no user-side `<root>/roles/<id>.md` exists.
/// One entry per role advertised in `BUILTIN_ROLES`; the
/// `crew_role_prompt_coverage` doctor check verifies this invariant.
pub(crate) const BUILTIN_ROLE_PROMPTS: &[(&str, &str)] = &[
    ("coder", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/coder.md"))),
    ("code-reviewer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/code-reviewer.md"))),
    ("crawler", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/crawler.md"))),
    ("reviewer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/reviewer.md"))),
    ("pr-reviewer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/pr-reviewer.md"))),
    ("tool-bench", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/tool-bench.md"))),
    // (#1222 Phase B packet 4 - #2418) The review-probe/review-probe-{high,
    // mid,low}/review-judge/review-verify seat prompts were removed here
    // along with their role manifests above — the review funnel that
    // staffed them (`build_review_graph`) was deleted in #2310 P4d.
    // (#1698 Packet A) Frozen model-facing text (contract 6) — byte-locked
    // by `radio::tests::radio_router_role_prompt_matches_frozen_golden`.
    (RADIO_ROUTER_ROLE_ID, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/radio-router.md"))),
    // (#1698 Packet B2) Frozen model-facing PERSONA template (contract 6) —
    // carries a `{{humor}}` placeholder substituted at assembly time
    // (`src/radio_answer.rs`), never resolved by the loader itself. Byte-
    // locked by `radio_answer::tests::radio_host_role_prompt_matches_frozen_golden`.
    ("radio-host", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/radio-host.md"))),
    ("analyst", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/analyst.md"))),
    ("design-reviewer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/design-reviewer.md"))),
    ("test-designer", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/roles/test-designer.md"))),
];

/// (#425) Autonomous-dispatch preamble — prepended to specialist
/// role prompts at dispatch-time so the model knows upfront it
/// can't ask questions, can't pause, and should escalate
/// explicitly if blocked. Utility roles (bounded-I/O transformers
/// like radio-router) skip the preamble since they don't run
/// agent loops.
const AUTONOMOUS_DISPATCH_PREAMBLE: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/AUTONOMOUS_DISPATCH_PREAMBLE.md"));

/// The operator's override of the autonomous-dispatch preamble, a file directly
/// under the darkmux root.
pub const PREAMBLE_OVERRIDE_FILE: &str = "AUTONOMOUS_DISPATCH_PREAMBLE.md";

/// (#425) Resolve the autonomous-dispatch preamble — operator-side
/// override at `<root>/AUTONOMOUS_DISPATCH_PREAMBLE.md` wins
/// if present; otherwise the embedded default is returned.
///
/// The override path lets operators tune nudge wording per fleet /
/// per machine without recompiling. Lives in the root rather
/// than per-role so the preamble stays uniform across the
/// specialist set.
pub(crate) fn load_autonomous_dispatch_preamble() -> String {
    let user_path = user_state_root().join(PREAMBLE_OVERRIDE_FILE);
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

/// The user-state root: the darkmux root itself (`DARKMUX_HOME` when set,
/// else `~/.darkmux`). Roles, missions, phases, crews and skills live
/// directly under it.
pub fn user_state_root() -> PathBuf {
    resolve(ResolveScope::ForceUser).root
}

/// A user-state subdirectory: `<root>/<subdir>/`. The pre-Beat-33
/// `<root>/crew/<subdir>/` layout is not read; `darkmux doctor` fails on it
/// and prints the move script.
fn user_subdir(subdir: &str) -> PathBuf {
    user_state_root().join(subdir)
}

/// User-side roles directory: `<root>/roles/`.
pub(crate) fn roles_dir() -> PathBuf {
    user_subdir("roles")
}

/// Public read of the user-tier roles directory, for `darkmux doctor`'s
/// leftover-role checks (#2912/#2913 review). Same resolution as
/// [`roles_dir`].
pub fn user_roles_dir() -> PathBuf {
    roles_dir()
}

/// (#2912 review M1) The user-tier manifest that declares `role_id`, or
/// `None` when the role is builtin-only. The manifest's `id` field is
/// authoritative (#892), so a misnamed file is still found; the
/// `<role_id>.json` filename is checked first because it is the common
/// case. Unreadable or unparseable files are skipped silently — the loader
/// already warns about those on every load.
pub fn user_role_manifest_path(role_id: &str) -> Option<PathBuf> {
    #[derive(serde::Deserialize)]
    struct IdOnly {
        id: String,
    }
    let dir = roles_dir();
    let id_of = |p: &std::path::Path| read_json::<IdOnly>(p).ok().map(|r| r.id);
    let direct = dir.join(format!("{role_id}.json"));
    if direct.is_file() && id_of(&direct).as_deref() == Some(role_id) {
        return Some(direct);
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
        .collect();
    entries.sort();
    entries.into_iter().find(|p| id_of(p).as_deref() == Some(role_id))
}

/// User-side missions directory: `<root>/missions/`.
pub fn missions_dir() -> PathBuf {
    user_subdir("missions")
}

/// User-side phases directory: `<root>/phases/`.
pub fn phases_dir() -> PathBuf {
    user_subdir("phases")
}

/// User-side crews directory (operator overrides): `<root>/crews/`.
pub(crate) fn crews_dir() -> PathBuf {
    user_subdir("crews")
}

/// User-side skills directory (operator overrides): `<root>/skills/`.
pub(crate) fn skills_dir() -> PathBuf {
    user_subdir("skills")
}

/// User-side mission-configs directory (#1284 Packet 1) — the top tier of
/// `mission_config::load`'s user → on-disk → embedded resolution, mirroring
/// `workloads::load`'s search order. `<root>/mission-configs/`, honoring the
/// SAME root every other user-state dir does (`user_state_root`). No legacy-layout fallback
/// — mission configs postdate the Beat-33 flatten, so there's no
/// `<root>/crew/mission-configs/` to have ever existed (per the "no compat
/// baggage pre-1.0" doctrine — nothing to be compatible WITH here).
pub fn mission_configs_dir() -> PathBuf {
    user_state_root().join("mission-configs")
}

/// Resolve a role's system-prompt text. Search order:
///   1. User dir: `<root>/roles/<role-id>.md` (operator override)
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

/// (#1550 cluster item 3) Resolve a role's system-prompt text, honoring an
/// explicit `Role.prompt_path` FIRST — before falling to the conventional
/// search [`load_role_prompt`] performs (sibling `<root>/roles/<id>.md`,
/// then the embedded table).
///
/// Before this, `prompt_path` was preserved from a user manifest (see
/// `load_roles`'s sibling-file resolution above), persisted to the role
/// index, and displayed verbatim by `darkmux role show` — but nothing that
/// actually DISPATCHED ever read it: `load_role_prompt` only ever checked
/// the conventional sibling-file location. A role authored with
/// `"prompt_path": "/Users/me/prompts/my-role.md"` (a location OTHER than
/// the conventional one) showed that exact path in `role show`, then failed
/// to dispatch with "role has no .md system prompt" — a loud error that
/// directly contradicted what the tool had just displayed.
///
/// When `prompt_path` is set but unreadable (moved, permissions, a bad
/// manual edit since load), this falls through to the conventional search
/// rather than erroring here — the caller's own error message names
/// `prompt_path` explicitly when set, so the operator isn't left
/// wondering which of the two lookups failed.
pub(crate) fn load_role_prompt_for(role: &Role) -> Option<String> {
    if let Some(p) = &role.prompt_path {
        match fs::read_to_string(p) {
            Ok(content) => return Some(content),
            // (#1550 QA finding) SAY SO. Falling through silently recreates,
            // in this function's own failure arm, the exact trap the rest of
            // this change removes: a role whose id shadows a builtin gets
            // dispatched with the EMBEDDED prompt while `role show` keeps
            // displaying the operator's `prompt_path` — declared, displayed,
            // and not honored. The operator then debugs a prompt the model
            // never saw. Loud beats quiet; the dispatch still proceeds on the
            // conventional prompt, because refusing to run over a bad path
            // would be worse than running with a named substitution.
            Err(e) => eprintln!(
                "darkmux: role `{}` declares prompt_path `{}` but it could not be read \
                 ({e}); falling back to the conventional prompt for `{}`",
                role.id,
                p.display(),
                role.id
            ),
        }
    }
    load_role_prompt(&role.id)
}

/// Public accessor for [`load_role_prompt`] (#1222 Phase B packet 5
/// reconciliation). `darkmux mission launch review` used to dispatch
/// `review-probe`/`review-judge` through this via the (now-deleted)
/// dedicated review launcher's own `darkmux_crew::single_shot::
/// single_shot_chat` call, not `darkmux_crew::dispatch`/`dispatch_internal`
/// — so it needs the raw
/// system-prompt text itself rather than a full role dispatch, and
/// `load_role_prompt` is `pub(crate)`, invisible outside this crate.
/// Same search order: user override (`<root>/roles/<id>.md`), then
/// the embedded `BUILTIN_ROLE_PROMPTS`.
pub fn role_prompt(role_id: &str) -> Option<String> {
    load_role_prompt(role_id)
}

/// (#906) Defense-in-depth cap on a single manifest file. Role / mission /
/// phase / crew manifests are small (a few KB); a multi-MB file is either
/// corrupt or hostile, and an unbounded `read_to_string` + `from_str` is a
/// needless memory-amplification surface. The same cap the unknown-key gate
/// reads user files under.
const MAX_MANIFEST_BYTES: u64 = darkmux_types::user_files::MAX_USER_FILE_BYTES;

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

/// (#2268) ONE builtin role as embedded in the binary — no user tier, no
/// env, no `~/.darkmux`, no `.md` attached (the palette lives in the JSON).
/// For tests that make a claim about the SHIPPED manifest: `load_roles()`
/// merges the user tier first, so a test through it can be flipped by an
/// operator's local `crawler.json` override in either direction (proven in
/// review). `Ok(None)` when `id` is not a builtin; `Err` carries the serde
/// message when its JSON does not parse, so a broken embedded manifest names
/// itself instead of reading as "not a builtin".
#[cfg(test)]
pub(crate) fn builtin_role(id: &str) -> Result<Option<Role>> {
    BUILTIN_ROLES
        .iter()
        .find(|(bid, _)| *bid == id)
        .map(|(_, json)| {
            serde_json::from_str::<Role>(json)
                .with_context(|| format!("builtin role `{id}` failed to parse"))
        })
        .transpose()
}

/// Load all roles from the user dir, falling back to built-in templates.
///
/// `DARKMUX_HOME` relocates the root — useful for tests and non-standard
/// layouts.
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

/// Load all missions from the per-mission nested layout.
///
/// Walks `<root>/missions/` and for each **subdirectory** containing a
/// `mission.json`, deserializes it. A plain file directly under
/// `<root>/missions/` is not a mission and is skipped here;
/// `darkmux doctor` fails on a pre-#148 flat `<id>.json` so the skip is
/// never silent.
///
/// Built-in missions (currently empty) are merged last, same as other loaders.
pub fn load_missions() -> Result<Vec<Mission>> {
    let mut warnings = Vec::new();
    let missions = read_missions(&mut warnings);
    say_once(&warnings);
    missions
}

/// [`load_missions`] without the saying: every mission it refused is a line in
/// `warnings`.
fn read_missions(warnings: &mut Vec<String>) -> Result<Vec<Mission>> {
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
            // A plain file is not a mission directory.
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
        match parse_state::<Mission>(StateKind::Mission, &mission_file, &text) {
            Ok(m) => { map.insert(m.id.clone(), m); }
            Err(e) => warnings.push(format!("warning: failed to read mission: {e:#}")),
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

/// Say each of `warnings` on stderr unless this process already said it: the
/// serve daemon loads missions and phases on every poll, and the same refused
/// file would otherwise repeat on each one. Returns how many it said.
fn say_once(warnings: &[String]) -> usize {
    warnings
        .iter()
        .filter(|line| crate::budget::first_refusal(line))
        .inspect(|line| eprintln!("{line}"))
        .count()
}

/// Load all phases from the new per-mission nested layout.
///
/// Walks every `<root>/missions/<mission-id>/phases/*.json`.  The
/// Phase JSON already carries `mission_id`, so no inference from the dir
/// name is needed.  Legacy flat phase files under `<root>/phases/`
/// are silently ignored — the migration verb is the bridge.
pub fn load_phases() -> Result<Vec<Phase>> {
    let mut warnings = Vec::new();
    let phases = read_phases(&mut warnings);
    say_once(&warnings);
    phases
}

/// Read every `*.json` phase file under one mission's phases directory into
/// `map`, keyed by `(mission_id, phase_id)`; a file that will not parse is a
/// line in `warnings`.
fn read_mission_phases(
    mission_id: &str,
    map: &mut BTreeMap<(String, String), Phase>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let phases_dir = crate::lifecycle::phases_dir(mission_id);
    if !phases_dir.is_dir() {
        return Ok(());
    }
    for phase_entry in fs::read_dir(&phases_dir)
        .with_context(|| format!("reading {}", phases_dir.display()))?
    {
        let phase_path = phase_entry?.path();
        if !phase_path.is_file() || phase_path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let text = fs::read_to_string(&phase_path)
            .with_context(|| format!("reading {}", phase_path.display()))?;
        match parse_state::<Phase>(StateKind::Phase, &phase_path, &text) {
            Ok(s) => { map.insert((s.mission_id.clone(), s.id.clone()), s); }
            Err(e) => warnings.push(format!("warning: failed to read phase: {e:#}")),
        }
    }
    Ok(())
}

/// [`load_phases`] without the saying: a leftover `sprints/` directory and
/// every phase it refused is a line in `warnings`.
fn read_phases(warnings: &mut Vec<String>) -> Result<Vec<Phase>> {
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
        if let Some(retired) = retired_state::retired_phases_dir(&path) {
            warnings.push(format!("warning: {}: {}", retired.path.display(), retired.fix));
        }
        read_mission_phases(&mission_id, &mut map, warnings)?;
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

// (#1550 cluster item 3) `resolve_role_prompt_path` — a stale,
// never-revived `#[allow(dead_code)]` helper with its own TODO (it bypassed
// `roles_dir()`'s canonical-vs-legacy fallback) — was removed here. Its
// intended purpose (honor an explicit `Role.prompt_path`) is now live via
// `load_role_prompt_for` above, which the two real dispatch call sites use.

#[cfg(test)]
#[path = "loader_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "loader_load_per_mission_tests.rs"]
mod load_per_mission_tests;
