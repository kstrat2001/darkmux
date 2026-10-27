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
//! inputs ([`crate::dispatch::LabInputs`]): an eval owns its roles, and a
//! workload bound to a fixture takes `roles/<id>.json` and `roles/<id>.md`
//! from the run's copy of that fixture, refusing when either is missing; a
//! workload bound to none takes darkmux's compiled-in role. The preamble is
//! always the compiled-in one, and no addition applies. On 2026-10-24 a
//! stale `~/.darkmux/roles/web-probe.md` replaced an eval's pinned prompt for
//! 19 runs; this module is why that cannot happen again.
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
    load_lab_role(inputs, role_id).map(|_| ())
}

fn load_lab_role(inputs: &LabInputs, role_id: &str) -> Result<(Role, LabRole)> {
    match inputs {
        LabInputs::Eval { dir } => load_eval_role(dir, role_id),
        LabInputs::Builtin => load_builtin_role(role_id),
    }
}

/// `<dir>/roles/<role_id>.json` and `.md`, through the same unknown-key gate
/// and family check every role manifest gets.
fn load_eval_role(dir: &Path, role_id: &str) -> Result<(Role, LabRole)> {
    let json_path = dir.join("roles").join(format!("{role_id}.json"));
    let md_path = dir.join("roles").join(format!("{role_id}.md"));
    let json = read_eval_role_file(&json_path, role_id)?;
    let md = read_eval_role_file(&md_path, role_id)?;
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
    Ok((role, LabRole { source: RoleInputSource::Eval, json, md }))
}

/// One of an eval's role files, bounded like every role manifest. A missing
/// file is the refusal this module exists for: there is no fallback.
fn read_eval_role_file(path: &Path, role_id: &str) -> Result<String> {
    let cap = darkmux_types::user_files::MAX_USER_FILE_BYTES;
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "{} is missing: a lab run of a workload bound to a fixture takes its role from the eval, so the \
             eval must carry its roles (roles/{role_id}.json and roles/{role_id}.md, listed in .fixture.json's \
             required_files). darkmux's built-in role and the home folder's roles/ are never used instead.",
            path.display()
        ),
        Ok(meta) if meta.len() > cap => {
            bail!("{} is {} bytes, past the {cap}-byte cap for a role file", path.display(), meta.len())
        }
        _ => std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display())),
    }
}

/// darkmux's compiled-in role and prompt, for a workload bound to no fixture.
fn load_builtin_role(role_id: &str) -> Result<(Role, LabRole)> {
    let builtin = |table: &[(&str, &'static str)]| table.iter().find(|(id, _)| *id == role_id).map(|(_, text)| *text);
    let (Some(json), Some(md)) =
        (builtin(crate::loader::BUILTIN_ROLES), builtin(crate::loader::BUILTIN_ROLE_PROMPTS))
    else {
        bail!(
            "role `{role_id}` is not one of darkmux's built-in roles. A lab workload bound to no fixture runs on \
             darkmux's compiled-in roles only, never the home folder's roles/; to measure another role, put \
             roles/{role_id}.json and roles/{role_id}.md in an eval and bind the workload to it (requires_fixture)."
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
