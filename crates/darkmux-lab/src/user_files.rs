//! The lab's user files (workload documents, lab fixture manifests) through
//! the unknown-key gate (`darkmux_types::user_files`), and the lab-run
//! preflight: the crew preflight (`darkmux_crew::user_files::preflight_with`)
//! plus these kinds.

use darkmux_types::config_enum::{PreflightRefusal, Scope};
use darkmux_types::paths::{self, ResolveScope};
use darkmux_types::user_files::{check_path, FileProblem, UserFileKind};

use crate::lab::fixture::FixtureManifest;
use crate::lab::registry::{default_registry_path, LabRegistry};
use crate::workloads::types::WorkloadManifest;

/// A workload document's retired keys, named instead of guessed at.
/// Every key a past workload schema had and this one does not, from
/// `git log` (`every_historical_workload_and_fixture_key_is_named_as_retired`).
fn workload_retired(path: &str) -> Option<String> {
    let line = match path {
        "workload.expected.test_count_baseline" => {
            "removed in #2833: nothing read it; a coding workload's baseline test count is its fixture's \
             `.fixture.json` `baseline.test_count`. Delete it"
        }
        "workload.agent" => "renamed to `role` (#328): name a darkmux role manifest",
        _ => return None,
    };
    Some(line.to_string())
}

/// A fixture manifest's retired keys, named instead of guessed at.
fn fixture_retired(path: &str) -> Option<String> {
    matches!(path, "hash_exclude" | "hash_include").then(|| {
        "removed in #610: nothing read it (the hash excludes are built in). Delete it; a built-in fixture \
         registered from an older darkmux checkout still carries it"
            .to_string()
    })
}

/// Every problem in the on-disk files of `kind` this crate owns. A kind
/// another crate owns has nothing here.
pub fn problems(kind: UserFileKind) -> Vec<FileProblem> {
    match kind {
        UserFileKind::Workload => {
            let user_root = paths::resolve(ResolveScope::ForceUser).root;
            crate::workloads::load::on_disk_dirs(&user_root)
                .iter()
                .flat_map(|d| crate::workloads::load::documents_in(d))
                .filter_map(|p| check_path::<WorkloadManifest>(kind, &p, &workload_retired))
                .collect()
        }
        UserFileKind::LabFixture => fixture_manifests()
            .iter()
            .filter_map(|p| check_path::<FixtureManifest>(kind, p, &fixture_retired))
            .collect(),
        UserFileKind::Config
        | UserFileKind::Profiles
        | UserFileKind::Role
        | UserFileKind::Skill
        | UserFileKind::Crew
        | UserFileKind::MissionConfig
        | UserFileKind::Rule
        | UserFileKind::WorkspaceSpec => Vec::new(),
    }
}

/// The `.fixture.json` of every fixture in the home-tier lab registry. A
/// registry that cannot be read names no fixtures here; `darkmux lab doctor`
/// reports it.
fn fixture_manifests() -> Vec<std::path::PathBuf> {
    let reg_path = default_registry_path(&paths::resolve(ResolveScope::ForceUser));
    if darkmux_types::user_files::is_operator_state(&reg_path) {
        return Vec::new();
    }
    let Ok(registry) = LabRegistry::load(&reg_path) else { return Vec::new() };
    registry.fixtures.values().map(|f| f.path.join(".fixture.json")).collect()
}

/// The preflight of every lab verb that runs a workload: the crew preflight
/// for `scope`, plus the lab-owned files `scope` consumes.
pub fn preflight_with(scope: Scope, profiles_file: Option<&str>) -> Result<(), PreflightRefusal> {
    let mut refusal = darkmux_crew::user_files::preflight_with(scope, profiles_file)
        .err()
        .unwrap_or_else(|| PreflightRefusal::none(scope));
    refusal.files.extend(UserFileKind::consumed_by(scope).flat_map(problems));
    refusal.into_result()
}

#[cfg(test)]
#[path = "user_files_tests.rs"]
mod tests;
