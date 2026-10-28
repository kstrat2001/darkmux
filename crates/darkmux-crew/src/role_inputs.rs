//! (#3363) The one role resolution every dispatch path makes, and what it
//! records.
//!
//! A dispatch's system prompt is the role's prompt, the specialist preamble,
//! and darkmux's additions (operator identity, workspace notes). For a
//! non-lab dispatch the role comes from `load_roles` (the home folder's
//! `roles/` over the compiled-in roles), the preamble may be the home
//! folder's override, and the additions apply as each path decides.
//!
//! A lab run is a measurement, so its prompt is built only from pinned
//! inputs ([`crate::dispatch::LabInputs`]), read once before the run starts
//! and carried as bytes (nothing reads them again). An eval owns its roles,
//! and the eval's fixture schema ([`RoleLayout`]) says where they live:
//!
//! - **schema 1.2 or newer**: `<eval_dir>/roles/<id>.json` and `.md`, and
//!   nothing else. A missing file refuses the run; there is no fallback.
//!   `eval_dir` never reaches the model's sandbox, so neither do they.
//! - **below 1.2, or none** (every eval published before 1.2): `roles/<id>.*`
//!   at the fixture root when the eval carries them, else darkmux's
//!   compiled-in role, else refused.
//!
//! A workload bound to no fixture takes darkmux's compiled-in role. The
//! preamble is always the compiled-in one, no addition applies, and the
//! home folder is never read, whatever the schema. On 2026-10-24 a stale
//! `~/.darkmux/roles/web-probe.md` replaced an eval's pinned prompt for 19
//! runs; this module is why that cannot happen again.
//!
//! [`PromptProvenance`] is what a dispatch used, stamped on `dispatch.start`
//! by [`stamp_prompt_inputs`]: the assembled prompt's SHA-256 on every
//! dispatch, and for a lab dispatch the SHA-256 of the role files and
//! preamble as loaded (hashed from the bytes in hand, never read again).

use anyhow::{anyhow, bail, Context, Result};
use darkmux_flow::payload::{DispatchRoleInputs, DispatchStartPayload, PromptAdditions, RoleInputSource};
use darkmux_flow::sha256_hex;
use std::path::Path;

use crate::dispatch::{DispatchOpts, LabInputs};
use crate::types::Role;

/// The role a lab dispatch loaded: the bytes of its manifest and prompt, as
/// read, and where they came from.
#[derive(Debug, Clone)]
struct LabRole {
    source: RoleInputSource,
    json: String,
    md: String,
}

/// (#3363) An eval's role, read once before the run starts: the parsed
/// manifest and the exact bytes of its `.json` and `.md`.
#[derive(Debug, Clone)]
pub struct EvalRole {
    role: Role,
    json: String,
    md: String,
    source: RoleInputSource,
}

impl PartialEq for EvalRole {
    /// The same bytes from the same place: the manifest is parsed from `json`.
    fn eq(&self, other: &Self) -> bool {
        (&self.json, &self.md, self.source) == (&other.json, &other.md, other.source)
    }
}

impl EvalRole {
    /// Where the role was read: the eval's `<eval_dir>/roles/`, or a legacy
    /// eval's root `roles/`.
    pub(crate) fn source(&self) -> RoleInputSource {
        self.source
    }
}

/// (#3363) Where a fixture-bound lab run's role lives. The eval's fixture
/// schema (`.fixture.json`'s `schema_version`) decides it, not darkmux's
/// version: an eval opts into the strict rule, and every eval published
/// before it keeps running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleLayout {
    /// Fixture schema 1.2 or newer: the eval owns its roles, at
    /// `<eval_dir>/roles/<id>.json` and `.md`. Nothing else is read: a
    /// missing file refuses the run, whatever darkmux compiles in or the
    /// fixture root holds.
    Eval { eval_dir: String },
    /// Fixture schema below 1.2 (1.0, 1.1), or none: every eval published before
    /// 1.2. `roles/<id>.json` and `.md` at the fixture root when the eval
    /// carries them, else darkmux's compiled-in role, else refused.
    /// `eval_dir` is the folder the fixture declares, if any.
    Legacy { eval_dir: Option<String> },
}

/// THE role resolution of a fixture-bound lab run, read once from the
/// fixture checkout before the run starts. See [`RoleLayout`]. Whatever the
/// layout, the home folder's `roles/` is never read.
pub(crate) fn read_fixture_role(fixture_dir: &Path, layout: &RoleLayout, role_id: &str) -> Result<LabInputs> {
    let eval = |role| Ok(LabInputs::Eval(Box::new(role)));
    match layout {
        RoleLayout::Eval { eval_dir } => {
            eval(load_role_files(fixture_dir, &[eval_dir, "roles"], role_id, RoleInputSource::Eval)?)
        }
        RoleLayout::Legacy { eval_dir } => {
            if let Some(dir) = eval_dir.as_deref().filter(|d| carries_role(fixture_dir, &[d, "roles"], role_id)) {
                bail!(
                    "{} carries {dir}/roles/{role_id}.json or .md, but its .fixture.json declares a fixture schema \
                     below 1.2, or none, so a lab run would never read them. Declare `\"schema_version\": \"1.2\"`: \
                     the eval then owns its roles in {dir}/roles/",
                    fixture_dir.display()
                );
            }
            // A linked root `roles/` is refused even when its target lacks
            // the role: what the eval runs depends only on its own checkout.
            let linked = has_entry(fixture_dir, "roles") && is_symlink(&fixture_dir.join("roles"));
            if linked || carries_role(fixture_dir, &["roles"], role_id) {
                return eval(load_role_files(fixture_dir, &["roles"], role_id, RoleInputSource::LegacyRoot)?);
            }
            builtin_role_text(role_id).map(|_| LabInputs::Builtin).ok_or_else(|| {
                anyhow!(
                    "role `{role_id}` is not in the eval (roles/{role_id}.json and roles/{role_id}.md at {}) and is \
                     not one of darkmux's built-in roles. The eval declares a fixture schema below 1.2, or none, so a \
                     lab run takes its role from the fixture root's roles/ when there, else from darkmux's compiled-in \
                     roles; never from the home folder's roles/. To measure this role, add it to the eval: \
                     {}roles/{role_id}.json and .md, with `\"schema_version\": \"1.2\"`.",
                    fixture_dir.display(),
                    eval_dir.as_deref().map(|d| format!("{d}/")).unwrap_or_else(|| "<eval_dir>/".into())
                )
            })
        }
    }
}

/// Whether `base/<dirs..>/` holds either of `role_id`'s files, by exact
/// names (a link counts, and is then refused by the read).
fn carries_role(base: &Path, dirs: &[&str], role_id: &str) -> bool {
    ["json", "md"].iter().any(|ext| exact_path(base, dirs, &format!("{role_id}.{ext}")))
}

/// Whether `base/<dirs..>/<file>` exists with every component named exactly
/// so. A case-folding filesystem (macOS) would also open `ROLES/Coder.md`
/// for `roles/coder.md`; matching directory entries by name makes an eval
/// resolve the same way on every OS.
fn exact_path(base: &Path, dirs: &[&str], file: &str) -> bool {
    let mut here = base.to_path_buf();
    dirs.iter().chain([&file]).all(|name| {
        let found = has_entry(&here, name);
        here.push(name);
        found
    })
}

/// Whether `dir` has an entry named exactly `name`.
fn has_entry(dir: &Path, name: &str) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| entries.flatten().any(|e| e.file_name() == name))
}

/// Refuse `path` when it is a symbolic link: the fixture checkout may hold
/// links, and one could deliver a role from anywhere.
fn refuse_symlink(path: &Path, what: &str) -> Result<()> {
    if is_symlink(path) {
        bail!(
            "the eval's `{}` is a symlink; a lab run takes its role from the eval itself, and a link could \
             deliver a role from anywhere. Make it a real {what}",
            path.display()
        );
    }
    Ok(())
}

/// A dispatch's role, resolved once. A lab role carries the bytes it was
/// loaded from; a non-lab role's prompt is read at assembly, as it always was.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedRole {
    pub(crate) role: Role,
    lab: Option<LabRole>,
}

/// What a dispatch's system prompt was built from, for `dispatch.start`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PromptProvenance {
    pub(crate) additions: PromptAdditions,
    /// A lab dispatch's role block; `None` on every other dispatch.
    pub(crate) role: Option<DispatchRoleInputs>,
}

/// THE role resolution of a dispatch. `Ok(None)` when a non-lab dispatch
/// names a role nobody defines (each path words that refusal itself). A lab
/// dispatch whose role cannot be loaded from its pinned inputs is refused.
pub(crate) fn resolve_role(opts: &DispatchOpts) -> Result<Option<ResolvedRole>> {
    if let Some(inputs) = &opts.lab_inputs {
        let (role, lab) = load_lab_role(inputs, &opts.role_id)?;
        return Ok(Some(ResolvedRole { role, lab: Some(lab) }));
    }
    let roles = crate::loader::load_roles().context("loading crew roles for internal dispatch")?;
    Ok(roles.into_iter().find(|r| r.id == opts.role_id).map(|role| ResolvedRole { role, lab: None }))
}

/// Refuse a lab run whose role cannot be loaded from `inputs`, with the same
/// message its dispatch would give. The lab calls it before any model loads.
pub(crate) fn check_lab_role(inputs: &LabInputs, role_id: &str) -> Result<()> {
    lab_role(inputs, role_id).map(|_| ())
}

/// The role manifest a lab dispatch with `inputs` runs, loaded the way its
/// dispatch loads it, for a lab check that reads the role before dispatching
/// (the coding-task palette check, the bench's recorded window).
pub(crate) fn lab_role(inputs: &LabInputs, role_id: &str) -> Result<Role> {
    load_lab_role(inputs, role_id).map(|(role, _)| role)
}

fn load_lab_role(inputs: &LabInputs, role_id: &str) -> Result<(Role, LabRole)> {
    match inputs {
        LabInputs::Eval(eval) => {
            if eval.role.id != role_id {
                bail!(
                    "the lab run loaded role `{}` from the eval, but this dispatch runs `{role_id}`",
                    eval.role.id
                );
            }
            let lab = LabRole { source: eval.source, json: eval.json.clone(), md: eval.md.clone() };
            Ok((eval.role.clone(), lab))
        }
        LabInputs::Builtin => load_builtin_role(role_id),
    }
}

/// `base/<dirs..>/<role_id>.json` and `.md`, through the same unknown-key
/// gate and family check every role manifest gets. Read once, before the
/// run. Missing (by exact names), a symlink (as is any folder on the way),
/// past the size cap, an unknown key or another id refuses it.
fn load_role_files(base: &Path, dirs: &[&str], role_id: &str, source: RoleInputSource) -> Result<EvalRole> {
    let mut roles_dir = base.to_path_buf();
    for dir in dirs {
        roles_dir.push(dir);
        refuse_symlink(&roles_dir, "folder")?;
    }
    let read = |file: String| {
        let path = roles_dir.join(&file);
        let found = exact_path(base, dirs, &file);
        read_eval_role_file(&path, found, role_id, source).map(|text| (path, text))
    };
    let (json_path, json) = read(format!("{role_id}.json"))?;
    let (md_path, md) = read(format!("{role_id}.md"))?;
    use darkmux_types::user_files::{check_text, UserFileKind};
    if let Some(problem) = check_text::<Role>(UserFileKind::Role, &json_path, &json, &crate::user_files::role_retired) {
        bail!("{problem}");
    }
    let mut role: Role =
        serde_json::from_str(&json).with_context(|| format!("parsing JSON at {}", json_path.display()))?;
    if role.id != role_id {
        bail!(
            "role manifest {} declares id `{}`, but the workload runs `{role_id}`: an eval's role file is named for its id",
            json_path.display(),
            role.id
        );
    }
    crate::loader::validate_role_family(&role, crate::loader::RoleSource::Eval)
        .with_context(|| format!("role manifest {}", json_path.display()))?;
    role.prompt_path = Some(md_path);
    Ok(EvalRole { role, json, md, source })
}

/// Whether `path` itself is a symbolic link (its target is not followed).
fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// One of an eval's role files, bounded like every role manifest. A missing
/// file (`found` false: no entry by that exact name) is the refusal this
/// module exists for: there is no fallback. A
/// symlink is refused too: the fixture checkout may hold links, and one pointing at
/// the home folder would deliver its bytes (the link itself is never followed).
fn read_eval_role_file(path: &Path, found: bool, role_id: &str, source: RoleInputSource) -> Result<String> {
    let cap = darkmux_types::user_files::MAX_USER_FILE_BYTES;
    if !found {
        return Err(missing_role_file(path, role_id, source));
    }
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(missing_role_file(path, role_id, source)),
        Ok(meta) if meta.file_type().is_symlink() => bail!(
            "the eval's `{}` is a symlink; a lab run takes its role from the eval itself, and a link could \
             deliver a role from anywhere. Make it a real file",
            path.display()
        ),
        Ok(meta) if meta.len() > cap => {
            bail!("{} is {} bytes, past the {cap}-byte cap for a role file", path.display(), meta.len())
        }
        _ => std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display())),
    }
}

/// The refusal for a role file the eval does not carry: there is no
/// fallback, whichever layout the eval declares.
fn missing_role_file(path: &Path, role_id: &str, source: RoleInputSource) -> anyhow::Error {
    if source == RoleInputSource::Eval {
        return anyhow!(
            "{} is missing: the eval declares fixture schema 1.2 or newer, under which the eval must carry \
             its roles (<eval_dir>/roles/{role_id}.json and .md). darkmux's built-in role, a roles/ folder at the \
             fixture root and the home folder's roles/ are never used instead.",
            path.display()
        );
    }
    anyhow!(
        "{} is missing: the eval carries roles/{role_id}.* at its root, so both roles/{role_id}.json and \
         roles/{role_id}.md must be there. darkmux's built-in role and the home folder's roles/ are never \
         used in place of one.",
        path.display()
    )
}

/// darkmux's compiled-in manifest and prompt for `role_id`, if it compiles
/// one in.
fn builtin_role_text(role_id: &str) -> Option<(&'static str, &'static str)> {
    let builtin = |table: &[(&str, &'static str)]| table.iter().find(|(id, _)| *id == role_id).map(|(_, text)| *text);
    Some((builtin(crate::loader::BUILTIN_ROLES)?, builtin(crate::loader::BUILTIN_ROLE_PROMPTS)?))
}

/// darkmux's compiled-in role and prompt: for a workload bound to no
/// fixture, and for a legacy eval that carries no role of its own.
fn load_builtin_role(role_id: &str) -> Result<(Role, LabRole)> {
    let Some((json, md)) = builtin_role_text(role_id) else {
        bail!(
            "role `{role_id}` is not one of darkmux's built-in roles. A lab workload bound to no fixture runs on \
             darkmux's compiled-in roles only, never the home folder's roles/; to measure another role, put \
             <eval_dir>/roles/{role_id}.json and .md in an eval at fixture schema 1.2 and bind the workload to it \
             (requires_fixture)."
        );
    };
    let role: Role = serde_json::from_str(json).with_context(|| format!("builtin role `{role_id}` failed to parse"))?;
    Ok((role, LabRole { source: RoleInputSource::Builtin, json: json.to_string(), md: md.to_string() }))
}

impl ResolvedRole {
    fn is_lab(&self) -> bool {
        self.lab.is_some()
    }

    /// The role prompt joined with the specialist preamble, as every path
    /// joins them, and the provenance of both. `requires` ends the
    /// missing-prompt refusal in each path's own words.
    pub(crate) fn role_and_preamble(&self, requires: &str) -> Result<(String, PromptProvenance)> {
        let role = &self.role;
        let role_prompt = match &self.lab {
            Some(lab) => Some(lab.md.clone()),
            None => crate::loader::load_role_prompt_for(role),
        }
        .ok_or_else(|| {
            anyhow!(
                "role '{}' has no readable .md system prompt (checked prompt_path={:?}, the \
                 conventional roles dir, and the embedded table){requires}",
                role.id,
                role.prompt_path
            )
        })?;
        let mut prov = self.provenance();
        if !role.is_specialist() {
            return Ok((role_prompt, prov));
        }
        let (preamble, overridden) = match self.is_lab() {
            true => (crate::loader::AUTONOMOUS_DISPATCH_PREAMBLE.to_string(), false),
            false => crate::loader::resolve_autonomous_dispatch_preamble(),
        };
        prov.additions.preamble_override = overridden;
        if let Some(r) = prov.role.as_mut() {
            r.preamble_sha256 = Some(sha256_hex(preamble.as_bytes()));
        }
        // Trim + always insert a `\n\n` separator so an override file that
        // forgot a trailing newline still joins cleanly.
        Ok((format!("{}\n\n{}", preamble.trim_end(), role_prompt), prov))
    }

    /// The provenance of the role alone: the role block for a lab dispatch,
    /// whether the home folder supplied it for any other.
    fn provenance(&self) -> PromptProvenance {
        match &self.lab {
            Some(lab) => PromptProvenance {
                additions: PromptAdditions::default(),
                role: Some(DispatchRoleInputs {
                    id: self.role.id.clone(),
                    source: lab.source,
                    json_sha256: sha256_hex(lab.json.as_bytes()),
                    md_sha256: sha256_hex(lab.md.as_bytes()),
                    preamble_sha256: None,
                }),
            },
            None => PromptProvenance {
                additions: PromptAdditions {
                    role_override: self.role.prompt_path.is_some()
                        || crate::loader::user_role_manifest_path(&self.role.id).is_some(),
                    ..PromptAdditions::default()
                },
                role: None,
            },
        }
    }

    /// The operator identity, appended when `allowed` and never for a lab
    /// dispatch.
    pub(crate) fn with_identity(&self, prompt: String, allowed: bool, prov: &mut PromptProvenance) -> String {
        if self.is_lab() || !allowed {
            return prompt;
        }
        let out = crate::dispatch::augment_prompt_with_identity(&prompt);
        prov.additions.identity = out.len() != prompt.len();
        out
    }

    /// The workspace notes, never for a lab dispatch.
    pub(crate) fn with_workspace_notes(
        &self,
        prompt: String,
        workdir: Option<&Path>,
        found: &[darkmux_types::workdir::SplitGitdir],
        prov: &mut PromptProvenance,
    ) -> String {
        if self.is_lab() {
            return prompt;
        }
        let before = prompt.len();
        let out = crate::dispatch_internal::with_workspace_notes(prompt, workdir, found);
        prov.additions.workspace_notes = out.len() != before;
        out
    }
}

/// Stamp what the system prompt was built from on a `dispatch.start`
/// payload: its SHA-256 (taken where its length is), the additions, and a
/// lab dispatch's role block.
pub(crate) fn stamp_prompt_inputs(payload: &mut DispatchStartPayload, system_prompt: &str, prov: &PromptProvenance) {
    payload.system_sha256 = Some(sha256_hex(system_prompt.as_bytes()));
    payload.prompt_additions = Some(prov.additions);
    payload.role = prov.role.clone();
}
