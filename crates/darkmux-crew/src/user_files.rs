//! The crew library's user files (roles, skills, crews, mission configs,
//! rules) through the unknown-key gate (`darkmux_types::user_files`), and
//! [`preflight_with`], the one preflight every work-starting entry point
//! calls: `config.json` and the profile registry
//! (`darkmux_profiles::preflight_with`) plus the crew-owned files its scope
//! consumes. `darkmux-lab` adds workloads and fixtures for a lab run.

use darkmux_types::config_enum::{PreflightRefusal, Scope};
use darkmux_types::user_files::{check_dir, no_retired, FileProblem, UserFileKind};

use crate::mission_config::MissionConfig;
use crate::rules::Rule;
use crate::types::{Crew, Role, Skill};

/// Every problem in the on-disk files of `kind` this crate owns. A kind
/// another crate owns has nothing here.
pub fn problems(kind: UserFileKind) -> Vec<FileProblem> {
    match kind {
        UserFileKind::Role => check_dir::<Role>(kind, &crate::loader::roles_dir(), &no_retired),
        UserFileKind::Skill => check_dir::<Skill>(kind, &crate::loader::skills_dir(), &no_retired),
        UserFileKind::Crew => check_dir::<Crew>(kind, &crate::loader::crews_dir(), &no_retired),
        UserFileKind::MissionConfig => crate::mission_config::load::on_disk_dirs()
            .iter()
            .flat_map(|d| check_dir::<MissionConfig>(kind, d, &crate::mission_config::retired_key))
            .collect(),
        UserFileKind::Rule => check_dir::<Rule>(kind, &crate::rules::user_rules_dir(), &no_retired),
        UserFileKind::Config
        | UserFileKind::Profiles
        | UserFileKind::Workload
        | UserFileKind::LabFixture
        | UserFileKind::WorkspaceSpec => Vec::new(),
    }
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
    refusal.files.extend(UserFileKind::consumed_by(scope).flat_map(problems));
    refusal.into_result()
}

/// [`preflight_with`] against the default registry search.
pub fn preflight(scope: Scope) -> Result<(), PreflightRefusal> {
    preflight_with(scope, None)
}

#[cfg(test)]
#[path = "user_files_tests.rs"]
mod tests;
