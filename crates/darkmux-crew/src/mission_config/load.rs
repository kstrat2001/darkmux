//! Load mission configs by id from any of several known locations.
//! Search order: user dir → on-disk built-in template dirs → binary-embedded
//! built-ins. Mirrors `darkmux_lab::workloads::load`'s resolution EXACTLY
//! (#1284 Packet 1) — see that module's doc for the precedent this copies:
//! same three-tier shape, same `templates_override_dirs()` override
//! accessor, same cwd/home/system candidate tree.

use super::MissionConfig;
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Mission configs compiled into the binary at build time. Each entry is
/// `(id, json)` where `json` is the verbatim document — mirrors
/// `workloads::load::EMBEDDED_WORKLOADS`. Unlike workloads (which reject
/// embedding a manifest that references `promptFile`/`sandboxSeed`, since
/// those need a resolvable on-disk `base_dir`), mission configs carry no
/// filesystem-only fields at all — every built-in embeds cleanly, no
/// restriction.
const EMBEDDED_MISSION_CONFIGS: &[(&str, &str)] = &[
    (
        "review",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../templates/builtin/mission-configs/review.json"
        )),
    ),
    (
        "coder-phase",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../templates/builtin/mission-configs/coder-phase.json"
        )),
    ),
];

fn find_embedded(id: &str) -> Option<&'static str> {
    EMBEDDED_MISSION_CONFIGS
        .iter()
        .find(|(name, _)| *name == id)
        .map(|(_, json)| *json)
}

/// Which tier a loaded mission config actually resolved from — the WINNING
/// tier of the user → on-disk → embedded search order. Three DISTINCT
/// variants (unlike `workloads::WorkloadSource`'s two, which folds on-disk
/// and embedded together under `Builtin`) because `darkmux doctor`'s
/// mission-config check surfaces the tier explicitly, per the packet spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionConfigSource {
    /// `<mission_configs_dir()>/<id>.json` — operator override.
    User,
    /// A `templates/builtin/mission-configs/<id>.json` found on disk
    /// (repo checkout, `DARKMUX_TEMPLATES_DIR` override, or the
    /// `~/.darkmux/templates/...` / `/usr/local/share/...` candidates).
    OnDisk,
    /// Compiled into the binary (`EMBEDDED_MISSION_CONFIGS`) — always
    /// resolvable even from a bare `cargo install`, no source tree needed.
    Embedded,
}

impl MissionConfigSource {
    pub fn label(self) -> &'static str {
        match self {
            MissionConfigSource::User => "user",
            MissionConfigSource::OnDisk => "on-disk",
            MissionConfigSource::Embedded => "embedded",
        }
    }
}

impl std::fmt::Display for MissionConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// A loaded mission config document, plus where it came from.
#[derive(Debug, Clone)]
pub struct LoadedMissionConfig {
    pub config: MissionConfig,
    pub manifest_path: PathBuf,
    pub source: MissionConfigSource,
}

/// On-disk built-in template dirs, in priority order. Override candidates
/// come from `env(DARKMUX_TEMPLATES_DIR)` then `config.dirs.templates`
/// (#661 Slice 3, via the SAME accessor `workloads::load::builtin_dirs`
/// uses), prepended ahead of cwd/home/system — mirrors that function
/// exactly, swapping `workloads` for `mission-configs`.
fn builtin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for base in darkmux_types::config_access::templates_override_dirs() {
        dirs.push(base.join("mission-configs"));
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    dirs.push(cwd.join("templates").join("builtin").join("mission-configs"));
    if let Some(home) = dirs::home_dir() {
        dirs.push(
            home.join(".darkmux")
                .join("templates")
                .join("builtin")
                .join("mission-configs"),
        );
    }
    dirs.push(PathBuf::from(
        "/usr/local/share/darkmux/templates/builtin/mission-configs",
    ));
    dirs
}

fn find_in_dir(dir: &Path, id: &str) -> Option<PathBuf> {
    if !dir.exists() {
        return None;
    }
    let flat = dir.join(format!("{id}.json"));
    if flat.is_file() {
        return Some(flat);
    }
    None
}

fn parse(path: &Path, source: MissionConfigSource) -> Result<LoadedMissionConfig> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading mission config at {}", path.display()))?;
    let config: MissionConfig = serde_json::from_str(&raw)
        .with_context(|| format!("parsing mission config JSON at {}", path.display()))?;
    Ok(LoadedMissionConfig {
        config,
        manifest_path: path.to_path_buf(),
        source,
    })
}

/// Parse a mission config from an in-memory JSON string (the embedded
/// path). Unlike `workloads::load::parse_str`, there's no filesystem-only
/// field to reject — a mission config document is pure data end to end.
fn parse_str(raw: &str, id: &str) -> Result<LoadedMissionConfig> {
    let config: MissionConfig = serde_json::from_str(raw)
        .with_context(|| format!("parsing embedded mission config \"{id}\""))?;
    Ok(LoadedMissionConfig {
        config,
        manifest_path: PathBuf::from(format!("<embedded>/{id}.json")),
        source: MissionConfigSource::Embedded,
    })
}

/// Load a mission config by id. Search order: user dir
/// (`crate::loader::mission_configs_dir()`) → on-disk built-in template
/// dirs (`builtin_dirs()`) → binary-embedded built-ins
/// (`EMBEDDED_MISSION_CONFIGS`) — mirrors `workloads::load::load`'s
/// resolution exactly. Never calls `MissionConfig::validate` — parsing
/// (lenient-on-read) and semantic validation stay separate (contract 7).
pub fn load(id: &str) -> Result<LoadedMissionConfig> {
    let user_dir = crate::loader::mission_configs_dir();
    if let Some(p) = find_in_dir(&user_dir, id) {
        return parse(&p, MissionConfigSource::User);
    }
    for d in builtin_dirs() {
        if let Some(p) = find_in_dir(&d, id) {
            return parse(&p, MissionConfigSource::OnDisk);
        }
    }
    if let Some(json) = find_embedded(id) {
        return parse_str(json, id);
    }
    let avail = list_ids();
    let listed = if avail.is_empty() {
        "(none)".to_string()
    } else {
        avail.join(", ")
    };
    bail!("mission config \"{id}\" not found. Available: {listed}")
}

/// Every discoverable mission-config id, unioned across all three tiers,
/// sorted + deduplicated. Mirrors `workloads::load::list_available`. Does
/// NOT report which tier resolves each id — a caller that needs that
/// (`darkmux doctor`) calls [`load`] per id, which is the actual
/// resolution `list_ids` can't duplicate without re-implementing it.
pub fn list_ids() -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    let mut all_dirs: Vec<PathBuf> = vec![crate::loader::mission_configs_dir()];
    all_dirs.extend(builtin_dirs());
    for dir in all_dirs {
        if !dir.exists() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if path.is_file() && name.ends_with(".json") {
                set.insert(name.trim_end_matches(".json").to_string());
            }
        }
    }
    for (id, _) in EMBEDDED_MISSION_CONFIGS {
        set.insert((*id).to_string());
    }
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn doc_json(id: &str) -> String {
        format!(r#"{{"id":"{id}","name":"Test {id}"}}"#)
    }

    /// RAII guard pinning `DARKMUX_CREW_DIR` (which `mission_configs_dir()`
    /// resolves under) at a TempDir for the test's duration, restoring the
    /// previous value on drop. Mirrors `loader::tests::CrewDirGuard`
    /// exactly (that guard is private to `loader`'s own test module, so
    /// this one is a sibling copy, not a reuse).
    struct CrewDirGuard {
        prev: Option<String>,
        _tmp: TempDir,
    }

    impl CrewDirGuard {
        fn new(tmp: TempDir) -> Self {
            let prev = std::env::var("DARKMUX_CREW_DIR").ok();
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()) };
            Self { prev, _tmp: tmp }
        }

        fn path(&self) -> &Path {
            self._tmp.path()
        }
    }

    impl Drop for CrewDirGuard {
        fn drop(&mut self) {
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                    None => std::env::remove_var("DARKMUX_CREW_DIR"),
                }
            }
        }
    }

    /// RAII guard forcing the on-disk builtin tier's `templates` candidate
    /// away from anywhere real, so a test can assert embedded-only
    /// resolution deterministically. Mirrors
    /// `workloads::load::tests::embedded_workload_resolves_with_no_on_disk_templates`.
    struct NoBuiltinTemplatesGuard {
        prev: Option<String>,
    }

    impl NoBuiltinTemplatesGuard {
        fn new(empty_dir: &Path) -> Self {
            let prev = std::env::var("DARKMUX_TEMPLATES_DIR").ok();
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe { std::env::set_var("DARKMUX_TEMPLATES_DIR", empty_dir) };
            Self { prev }
        }
    }

    impl Drop for NoBuiltinTemplatesGuard {
        fn drop(&mut self) {
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_TEMPLATES_DIR", v),
                    None => std::env::remove_var("DARKMUX_TEMPLATES_DIR"),
                }
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn loads_from_user_dir() {
        let tmp = TempDir::new().unwrap();
        let guard = CrewDirGuard::new(tmp);
        write(
            &guard.path().join("mission-configs/my-mission.json"),
            &doc_json("my-mission"),
        );
        let loaded = load("my-mission").unwrap();
        assert_eq!(loaded.config.id, "my-mission");
        assert_eq!(loaded.source, MissionConfigSource::User);
        assert_eq!(
            loaded.manifest_path,
            guard.path().join("mission-configs/my-mission.json")
        );
    }

    #[test]
    #[serial_test::serial]
    fn user_dir_takes_priority_over_on_disk_and_embedded() {
        let tmp = TempDir::new().unwrap();
        let guard = CrewDirGuard::new(tmp);
        // "review" is also embedded — a user-dir copy must win.
        write(&guard.path().join("mission-configs/review.json"), &doc_json("review"));
        let loaded = load("review").unwrap();
        assert_eq!(loaded.source, MissionConfigSource::User);
    }

    #[test]
    #[serial_test::serial]
    fn resolves_on_disk_when_present_and_no_user_override() {
        let user_tmp = TempDir::new().unwrap();
        let _crew_guard = CrewDirGuard::new(user_tmp); // empty user dir
        let templates_tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_TEMPLATES_DIR").ok();
        // SAFETY: serialized via #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_TEMPLATES_DIR", templates_tmp.path()) };
        write(
            &templates_tmp.path().join("mission-configs/on-disk-only.json"),
            &doc_json("on-disk-only"),
        );
        let loaded = load("on-disk-only").unwrap();
        // SAFETY: serialized via #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_TEMPLATES_DIR", v),
                None => std::env::remove_var("DARKMUX_TEMPLATES_DIR"),
            }
        }
        assert_eq!(loaded.source, MissionConfigSource::OnDisk);
        assert_eq!(loaded.config.id, "on-disk-only");
    }

    #[test]
    #[serial_test::serial]
    fn embedded_resolves_with_no_user_or_on_disk_copy() {
        let user_tmp = TempDir::new().unwrap();
        let _crew_guard = CrewDirGuard::new(user_tmp);
        let empty = TempDir::new().unwrap();
        let _templates_guard = NoBuiltinTemplatesGuard::new(empty.path());
        let loaded = load("review").expect("review should resolve from the embedded const");
        assert_eq!(loaded.source, MissionConfigSource::Embedded);
        assert_eq!(loaded.config.id, "review");
    }

    #[test]
    #[serial_test::serial]
    fn missing_mission_config_errors_with_listing() {
        let tmp = TempDir::new().unwrap();
        let _guard = CrewDirGuard::new(tmp);
        let err = load("does-not-exist").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"));
        assert!(msg.contains("review"), "embedded ids should be listed: {msg}");
    }

    #[test]
    #[serial_test::serial]
    fn malformed_json_errors_with_path() {
        let tmp = TempDir::new().unwrap();
        let guard = CrewDirGuard::new(tmp);
        write(&guard.path().join("mission-configs/broken.json"), "{not: valid json");
        let err = load("broken").unwrap_err();
        assert!(err.to_string().contains("parsing mission config JSON"));
    }

    #[test]
    #[serial_test::serial]
    fn missing_required_id_field_errors_at_load() {
        let tmp = TempDir::new().unwrap();
        let guard = CrewDirGuard::new(tmp);
        write(&guard.path().join("mission-configs/no-id.json"), r#"{"name":"No Id"}"#);
        let err = load("no-id").unwrap_err();
        assert!(err.to_string().contains("parsing mission config JSON"));
    }

    #[test]
    #[serial_test::serial]
    fn list_ids_includes_embedded_builtins() {
        let tmp = TempDir::new().unwrap();
        let _guard = CrewDirGuard::new(tmp);
        let ids = list_ids();
        assert!(ids.contains(&"review".to_string()));
        assert!(ids.contains(&"coder-phase".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn list_ids_unions_user_dir_with_embedded() {
        let tmp = TempDir::new().unwrap();
        let guard = CrewDirGuard::new(tmp);
        write(&guard.path().join("mission-configs/custom.json"), &doc_json("custom"));
        let ids = list_ids();
        assert!(ids.contains(&"custom".to_string()));
        assert!(ids.contains(&"review".to_string()));
    }

    #[test]
    fn embedded_configs_parse_as_valid_json_strings() {
        for (id, _) in EMBEDDED_MISSION_CONFIGS {
            let loaded = parse_str(find_embedded(id).unwrap(), id)
                .unwrap_or_else(|e| panic!("embedded mission config \"{id}\" must parse: {e}"));
            assert_eq!(loaded.config.id, *id);
            assert_eq!(loaded.source, MissionConfigSource::Embedded);
        }
    }

    #[test]
    fn source_label_and_display_agree() {
        for src in [
            MissionConfigSource::User,
            MissionConfigSource::OnDisk,
            MissionConfigSource::Embedded,
        ] {
            assert_eq!(src.label(), src.to_string());
        }
    }
}
