//! The crew library's user files (roles, skills, crews, mission configs,
//! rules) through the unknown-key gate (`darkmux_types::user_files`), and
//! [`preflight_with`], the one preflight every work-starting entry point
//! calls: `config.json` and the profile registry
//! (`darkmux_profiles::preflight_with`) plus the crew-owned files its scope
//! consumes. `darkmux-lab` adds workloads and fixtures for a lab run.

use darkmux_types::config_enum::{PreflightRefusal, Scope};
use darkmux_types::user_files::{check_dir, check_tiered, no_retired, FileProblem, Reach, UserFileKind};

use crate::mission_config::MissionConfig;
use crate::rules::Rule;
use crate::types::{Crew, Role, Skill};

/// Every problem in the on-disk files of `kind` this crate owns, at `reach`
/// (mission configs are tiered: a shadowed copy is only in
/// [`Reach::Every`]). A kind another crate owns has nothing here.
pub fn problems(kind: UserFileKind, reach: Reach) -> Vec<FileProblem> {
    match kind {
        UserFileKind::Role => check_dir::<Role>(kind, &crate::loader::roles_dir(), &role_retired),
        UserFileKind::Skill => check_dir::<Skill>(kind, &crate::loader::skills_dir(), &no_retired),
        UserFileKind::Crew => check_dir::<Crew>(kind, &crate::loader::crews_dir(), &no_retired),
        UserFileKind::MissionConfig => {
            let docs: Vec<(String, std::path::PathBuf)> =
                crate::mission_config::load::on_disk_dirs().iter().flat_map(|d| json_docs_by_stem(d)).collect();
            check_tiered::<MissionConfig>(
                kind,
                &docs,
                &crate::mission_config::retired_key,
                &crate::step_config::gate::step_config_issues,
                reach,
            )
        }
        UserFileKind::Rule => check_dir::<Rule>(kind, &crate::rules::user_rules_dir(), &no_retired)
            .into_iter()
            .filter_map(without_missing_keys)
            .collect(),
        UserFileKind::Config
        | UserFileKind::Profiles
        | UserFileKind::Workload
        | UserFileKind::LabFixture
        | UserFileKind::WorkspaceSpec => Vec::new(),
    }
}

/// `(id, path)` of every `<id>.json` directly in `dir`, in name order.
fn json_docs_by_stem(dir: &std::path::Path) -> Vec<(String, std::path::PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut docs: Vec<(String, std::path::PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| Some((p.file_stem()?.to_str()?.to_string(), p)))
        .collect();
    docs.sort();
    docs
}

/// A role manifest's retired keys: every key a past `Role` had and this one
/// does not, from `git log` (`every_historical_role_key_is_named_as_retired`).
fn role_retired(path: &str) -> Option<String> {
    let line = match path {
        "capabilities" => "renamed to `skills` (#449): the skill ids this role draws on",
        "tier" => "removed in #605: nothing reads it (a model is chosen by capability, not tier). Delete it",
        "escalation_posture" => "removed in 4.0, it had no effect: the runtime treated `auto` and `pause` the same. Delete it",
        _ => return None,
    };
    Some(line.to_string())
}

/// A user rule file is an OVERRIDE merged over the embedded rule of the same
/// id (`rules::load_all`), so it names only the keys it changes: a required
/// key it leaves out is not a problem.
fn without_missing_keys(mut found: FileProblem) -> Option<FileProblem> {
    use darkmux_types::user_files::{Issue, Problem};
    if let Problem::Keys(keys) = &mut found.problem {
        keys.retain(|k| !matches!(k.issue, Issue::Missing { .. }));
        if keys.is_empty() {
            return None;
        }
    }
    Some(found)
}

/// THE preflight for an entry point that starts work: every bad
/// `config.json` / registry value and unknown key its scope consumes
/// (`darkmux_profiles::preflight_with`), plus every crew-owned user file of
/// a kind its scope consumes (`UserFileKind::scopes`). Call it before
/// minting anything. `profiles_file` is the command's `--profiles-file`,
/// `None` for the default search.
pub fn preflight_with(scope: Scope, profiles_file: Option<&str>) -> Result<(), PreflightRefusal> {
    let mut refusal = darkmux_profiles::preflight_with(scope, profiles_file)
        .err()
        .unwrap_or_else(|| PreflightRefusal::none(scope));
    refusal.files.extend(UserFileKind::consumed_by(scope).flat_map(|k| problems(k, Reach::Effective)));
    refusal.into_result()
}

/// [`preflight_with`] against the default registry search.
pub fn preflight(scope: Scope) -> Result<(), PreflightRefusal> {
    preflight_with(scope, None)
}

#[cfg(test)]
#[path = "user_files_tests.rs"]
mod tests;
