//! The lab's user files (workload documents, lab fixture manifests) through
//! the unknown-key gate (`darkmux_types::user_files`), and the lab-run
//! preflight: the crew preflight (`darkmux_crew::user_files::preflight_with`)
//! plus these kinds.

use darkmux_types::config_enum::{PreflightRefusal, Scope};
use darkmux_types::paths::{self, ResolveScope};
use darkmux_types::user_files::{check_path, check_tiered, FileProblem, Reach, UserFileKind};

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

/// Every problem in the on-disk files of `kind` this crate owns, at `reach`.
/// Workloads are tiered: a shadowed copy is only in [`Reach::Every`]. A
/// fixture is read only by a run that binds it, so [`Reach::Effective`] has
/// no fixture here ([`preflight_with`] checks the bound one); at
/// [`Reach::Every`] each registered fixture is checked, noted as such. A
/// kind another crate owns has nothing here.
pub fn problems(kind: UserFileKind, reach: Reach) -> Vec<FileProblem> {
    match (kind, reach) {
        (UserFileKind::Workload, _) => {
            check_tiered::<WorkloadManifest>(kind, &workload_docs(), &workload_retired, reach)
        }
        (UserFileKind::LabFixture, Reach::Effective) => Vec::new(),
        (UserFileKind::LabFixture, Reach::Every) => registered_fixtures()
            .into_iter()
            .filter_map(|(satisfies, manifest)| {
                let mut found = check_path::<FixtureManifest>(kind, &manifest, &fixture_retired)?;
                found.note = Some(match satisfies {
                    Some(s) => format!("only a run that binds `{}` refuses over it", darkmux_types::user_files::escape_text(&s)),
                    None => "it satisfies no requirement, so no run binds it and only a run that binds it would refuse".to_string(),
                });
                Some(found)
            })
            .collect(),
        (
            UserFileKind::Config
            | UserFileKind::Profiles
            | UserFileKind::Role
            | UserFileKind::Skill
            | UserFileKind::Crew
            | UserFileKind::MissionConfig
            | UserFileKind::Rule
            | UserFileKind::WorkspaceSpec,
            _,
        ) => Vec::new(),
    }
}

/// Every workload document on disk as `(id, path)`, in the order
/// `workloads::load` resolves them: user tier first, then each on-disk
/// template dir; within a dir `<id>.json` before `<id>/workload.json`.
fn workload_docs() -> Vec<(String, std::path::PathBuf)> {
    let user_root = paths::resolve(ResolveScope::ForceUser).root;
    let mut out = Vec::new();
    for dir in crate::workloads::load::on_disk_dirs(&user_root) {
        let (mut flat, mut nested): (Vec<_>, Vec<_>) = crate::workloads::load::documents_in(&dir)
            .into_iter()
            .partition(|p| p.file_name().is_some_and(|n| n != "workload.json"));
        let id_of = |p: &std::path::PathBuf, nested: bool| {
            let named = if nested { p.parent().and_then(|d| d.file_name()) } else { p.file_stem() };
            named.and_then(|n| n.to_str()).map(str::to_string)
        };
        flat.sort();
        nested.sort();
        out.extend(flat.into_iter().filter_map(|p| Some((id_of(&p, false)?, p))));
        out.extend(nested.into_iter().filter_map(|p| Some((id_of(&p, true)?, p))));
    }
    out
}

/// `(satisfies, .fixture.json path)` of every fixture in the home-tier lab
/// registry. A registry that cannot be read names no fixtures here;
/// `darkmux lab doctor` reports it.
fn registered_fixtures() -> Vec<(Option<String>, std::path::PathBuf)> {
    let reg_path = default_registry_path(&paths::resolve(ResolveScope::ForceUser));
    if darkmux_types::user_files::is_operator_state(&reg_path) {
        return Vec::new();
    }
    let Ok(registry) = LabRegistry::load(&reg_path) else { return Vec::new() };
    registry.fixtures.values().map(|f| (f.satisfies.clone(), f.path.join(".fixture.json"))).collect()
}

/// The preflight of every lab verb: the crew preflight for `scope`, the
/// effective copy of every workload, and the manifest of the one fixture
/// this run binds (`fixture`: the workload's `requires_fixture`), never a
/// fixture this run does not read.
pub fn preflight_with(scope: Scope, profiles_file: Option<&str>, fixture: Option<&str>) -> Result<(), PreflightRefusal> {
    let mut refusal = darkmux_crew::user_files::preflight_with(scope, profiles_file)
        .err()
        .unwrap_or_else(|| PreflightRefusal::none(scope));
    refusal.files.extend(UserFileKind::consumed_by(scope).flat_map(|k| problems(k, Reach::Effective)));
    let bound = registered_fixtures().into_iter().find(|(s, _)| s.as_deref().is_some_and(|s| Some(s) == fixture));
    if let Some((_, manifest)) = bound {
        refusal.files.extend(check_path::<FixtureManifest>(UserFileKind::LabFixture, &manifest, &fixture_retired));
    }
    refusal.into_result()
}

#[cfg(test)]
#[path = "user_files_tests.rs"]
mod tests;
