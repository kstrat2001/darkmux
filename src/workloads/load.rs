//! Load workload manifests by id from any of several known locations.
//! Search order: user dir → on-disk built-in template dirs → binary-embedded built-ins.

use crate::workloads::types::{LoadedWorkload, WorkloadManifest, WorkloadSource};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Workloads compiled into the binary at build time. Each entry is `(id, json)`
/// where `json` is the verbatim manifest. This makes `cargo install --path .`
/// self-contained — users don't need a checked-out source tree to use the
/// built-in workloads.
const EMBEDDED_WORKLOADS: &[(&str, &str)] = &[
    (
        "quick-q",
        include_str!("../../templates/builtin/workloads/quick-q.json"),
    ),
    (
        "long-agentic",
        include_str!("../../templates/builtin/workloads/long-agentic.json"),
    ),
];

fn find_embedded(id: &str) -> Option<&'static str> {
    EMBEDDED_WORKLOADS
        .iter()
        .find(|(name, _)| *name == id)
        .map(|(_, json)| *json)
}

/// Built-in template directories, in priority order. Override with
/// `DARKMUX_TEMPLATES_DIR`.
fn builtin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(p) = env::var("DARKMUX_TEMPLATES_DIR") {
        dirs.push(Path::new(&p).join("workloads"));
    }
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    dirs.push(cwd.join("templates").join("builtin").join("workloads"));
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".darkmux").join("templates").join("builtin").join("workloads"));
    }
    dirs.push(PathBuf::from("/usr/local/share/darkmux/templates/builtin/workloads"));
    dirs
}

pub fn load(id: &str, user_dir: Option<&Path>) -> Result<LoadedWorkload> {
    if let Some(udir) = user_dir {
        let user_workloads = udir.join("workloads");
        if let Some(p) = find_in_dir(&user_workloads, id) {
            return parse(&p, WorkloadSource::User);
        }
    }
    for d in builtin_dirs() {
        if let Some(p) = find_in_dir(&d, id) {
            return parse(&p, WorkloadSource::Builtin);
        }
    }
    if let Some(json) = find_embedded(id) {
        return parse_str(json, id);
    }
    let avail = list_available(user_dir);
    let listed = if avail.is_empty() {
        "(none)".to_string()
    } else {
        avail.join(", ")
    };
    bail!("workload \"{id}\" not found. Available: {listed}")
}

pub fn list_available(user_dir: Option<&Path>) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    let mut all_dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = user_dir {
        all_dirs.push(d.join("workloads"));
    }
    all_dirs.extend(builtin_dirs());
    for dir in all_dirs {
        if !dir.exists() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if path.is_file() && name.ends_with(".json") {
                set.insert(name.trim_end_matches(".json").to_string());
            } else if path.is_dir() && path.join("workload.json").exists() {
                set.insert(name.to_string());
            }
        }
    }
    for (id, _) in EMBEDDED_WORKLOADS {
        set.insert((*id).to_string());
    }
    set.into_iter().collect()
}

fn find_in_dir(dir: &Path, id: &str) -> Option<PathBuf> {
    if !dir.exists() {
        return None;
    }
    let flat = dir.join(format!("{id}.json"));
    if flat.is_file() {
        return Some(flat);
    }
    let nested = dir.join(id).join("workload.json");
    if nested.is_file() {
        return Some(nested);
    }
    None
}

fn parse(path: &Path, source: WorkloadSource) -> Result<LoadedWorkload> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading workload manifest at {}", path.display()))?;
    let manifest: WorkloadManifest = serde_json::from_str(&raw)
        .with_context(|| format!("parsing workload JSON at {}", path.display()))?;
    let base_dir = path
        .parent()
        .ok_or_else(|| anyhow!("manifest has no parent dir: {}", path.display()))?
        .to_path_buf();
    Ok(LoadedWorkload {
        manifest,
        manifest_path: path.to_path_buf(),
        base_dir,
        source,
    })
}

/// Parse a workload manifest from an in-memory JSON string. Used for embedded
/// (binary-baked) workloads. The synthesized manifest_path / base_dir use an
/// `<embedded>` sentinel that does NOT correspond to a real filesystem path:
///
/// - Workloads that reference `promptFile` cannot be embedded (no resolvable
///   base_dir to read the file from).
/// - Coding-task workloads cannot be embedded either — they expect a sandbox
///   seed directory to copy from, which the sentinel can't provide. Embedding
///   one would silently run with an empty sandbox.
///
/// Anything that needs on-disk resources should be shipped as a directory
/// under `templates/builtin/workloads/` or installed to `~/.darkmux/...`.
fn parse_str(raw: &str, id: &str) -> Result<LoadedWorkload> {
    let manifest: WorkloadManifest = serde_json::from_str(raw)
        .with_context(|| format!("parsing embedded workload \"{id}\""))?;
    if manifest.workload.prompt_file.is_some() {
        bail!(
            "embedded workload \"{id}\" uses promptFile, which has no resolvable base_dir. \
             Inline the prompt or ship the workload on-disk instead."
        );
    }
    // Coding-task workloads can be embedded only if they don't explicitly
    // declare a sandbox seed — i.e., they use an external on-disk sandbox
    // (typically via the `DARKMUX_SANDBOX_<workload>` env var). A workload
    // that depends on a seed directory must ship as on-disk because
    // `<embedded>/<id>/sandbox` won't resolve on the user's filesystem.
    if manifest.workload.provider == "coding-task"
        && manifest.workload.sandbox_seed.is_some()
    {
        bail!(
            "embedded workload \"{id}\" uses provider=coding-task with an explicit sandboxSeed, \
             which has no resolvable base_dir. Drop the sandboxSeed field (so the workload uses \
             external sandbox via DARKMUX_SANDBOX_<workload>), or ship it as a directory under \
             templates/builtin/workloads/<id>/."
        );
    }
    let base = PathBuf::from(format!("<embedded>/{id}"));
    Ok(LoadedWorkload {
        manifest,
        manifest_path: base.join("workload.json"),
        base_dir: base,
        source: WorkloadSource::Builtin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn manifest_json(id: &str) -> String {
        format!(
            r#"{{"workload":{{"id":"{id}","provider":"prompt","prompt":"hello world"}}}}"#
        )
    }

    #[test]
    fn loads_flat_json_from_user_dir() {
        let tmp = TempDir::new().unwrap();
        let user = tmp.path();
        write(&user.join("workloads/my-task.json"), &manifest_json("my-task"));
        let loaded = load("my-task", Some(user)).unwrap();
        assert_eq!(loaded.manifest.workload.id, "my-task");
        assert_eq!(loaded.source, WorkloadSource::User);
        assert_eq!(loaded.base_dir, user.join("workloads"));
    }

    #[test]
    fn loads_nested_workload_json() {
        let tmp = TempDir::new().unwrap();
        let user = tmp.path();
        write(&user.join("workloads/big-task/workload.json"), &manifest_json("big-task"));
        let loaded = load("big-task", Some(user)).unwrap();
        assert_eq!(loaded.manifest.workload.id, "big-task");
        assert_eq!(loaded.base_dir, user.join("workloads/big-task"));
    }

    #[serial_test::serial]
    #[test]
    fn list_available_returns_sorted_unique() {
        let tmp = TempDir::new().unwrap();
        let user = tmp.path();
        write(&user.join("workloads/zebra.json"), &manifest_json("zebra"));
        write(&user.join("workloads/alpha.json"), &manifest_json("alpha"));
        write(&user.join("workloads/beta/workload.json"), &manifest_json("beta"));
        // Point templates dir at an empty path so builtins don't pollute the test.
        let empty = tmp.path().join("no-builtins-here");
        unsafe { env::set_var("DARKMUX_TEMPLATES_DIR", empty.to_str().unwrap()) };
        // Also chdir to a dir without templates/ so the cwd lookup misses.
        let prev = env::current_dir().unwrap();
        env::set_current_dir(tmp.path()).unwrap();
        let avail = list_available(Some(user));
        env::set_current_dir(prev).unwrap();
        unsafe { env::remove_var("DARKMUX_TEMPLATES_DIR") };
        // Embedded workloads (compiled into the binary) are always present in
        // addition to whatever's on disk. Filter them out for this test, which
        // asserts on-disk discovery + sorting only.
        let on_disk_only: Vec<String> = avail
            .into_iter()
            .filter(|id| !EMBEDDED_WORKLOADS.iter().any(|(eid, _)| eid == id))
            .collect();
        assert_eq!(on_disk_only, vec!["alpha", "beta", "zebra"]);
    }

    #[test]
    fn missing_workload_errors_with_listing() {
        let tmp = TempDir::new().unwrap();
        let user = tmp.path();
        write(&user.join("workloads/known.json"), &manifest_json("known"));
        let err = load("not-here", Some(user)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"));
        assert!(msg.contains("known"));
    }

    #[serial_test::serial]
    #[test]
    fn missing_workload_with_no_workloads_says_none() {
        let tmp = TempDir::new().unwrap();
        let prev = env::current_dir().unwrap();
        env::set_current_dir(tmp.path()).unwrap();
        unsafe { env::set_var("DARKMUX_TEMPLATES_DIR", tmp.path().join("nope")) };

        let err = load("anything", None).unwrap_err();

        unsafe { env::remove_var("DARKMUX_TEMPLATES_DIR") };
        env::set_current_dir(prev).unwrap();

        // Embedded workloads now mean Available is never literally "(none)";
        // the listing should still surface them so users can see what's offered.
        let msg = err.to_string();
        assert!(msg.contains("not found"));
        for (id, _) in EMBEDDED_WORKLOADS {
            assert!(
                msg.contains(id),
                "expected embedded workload \"{id}\" in error listing: {msg}"
            );
        }
    }

    #[test]
    fn malformed_json_errors_with_path() {
        let tmp = TempDir::new().unwrap();
        let user = tmp.path();
        write(&user.join("workloads/broken.json"), "{not: valid json");
        let err = load("broken", Some(user)).unwrap_err();
        assert!(err.to_string().contains("parsing workload JSON"));
    }

    #[test]
    fn missing_required_field_errors() {
        let tmp = TempDir::new().unwrap();
        let user = tmp.path();
        write(&user.join("workloads/no-id.json"), r#"{"workload":{"provider":"prompt"}}"#);
        let err = load("no-id", Some(user)).unwrap_err();
        assert!(err.to_string().contains("parsing workload JSON"));
    }

    #[test]
    fn user_dir_takes_priority_over_builtin() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("workloads/quick-q.json"),
            &manifest_json("quick-q"),
        );
        let loaded = load("quick-q", Some(tmp.path())).unwrap();
        assert_eq!(loaded.source, WorkloadSource::User);
    }

    #[test]
    fn builtin_dirs_include_env_override() {
        unsafe { env::set_var("DARKMUX_TEMPLATES_DIR", "/tmp/test-templates") };
        let dirs = builtin_dirs();
        unsafe { env::remove_var("DARKMUX_TEMPLATES_DIR") };
        assert_eq!(
            dirs.first().unwrap(),
            &PathBuf::from("/tmp/test-templates/workloads")
        );
    }

    // ─── parse_str (embedded-workload parser) ────────────────────────

    #[test]
    fn parse_str_loads_valid_inline_prompt() {
        let raw = r#"{"workload":{"id":"hello","provider":"prompt","prompt":"hi"}}"#;
        let loaded = parse_str(raw, "hello").unwrap();
        assert_eq!(loaded.manifest.workload.id, "hello");
        assert_eq!(loaded.manifest.workload.provider, "prompt");
        assert_eq!(loaded.manifest.workload.prompt.as_deref(), Some("hi"));
        // Source is Builtin and the sentinel base_dir is the literal `<embedded>/<id>` form
        assert_eq!(loaded.source, WorkloadSource::Builtin);
        assert_eq!(loaded.base_dir, PathBuf::from("<embedded>/hello"));
    }

    #[test]
    fn parse_str_rejects_prompt_file() {
        let raw =
            r#"{"workload":{"id":"x","provider":"prompt","promptFile":"prompt.txt"}}"#;
        let err = parse_str(raw, "x").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("promptFile"), "got: {msg}");
        assert!(msg.contains("\"x\""), "got: {msg}");
    }

    /// Coding-task with no explicit sandbox seed embeds OK (uses external
    /// sandbox via env var).
    #[test]
    fn parse_str_allows_coding_task_without_seed() {
        let raw = r#"{"workload":{"id":"y","provider":"coding-task","prompt":"do it"}}"#;
        let loaded = parse_str(raw, "y").unwrap();
        assert_eq!(loaded.manifest.workload.provider, "coding-task");
    }

    /// Coding-task that ALSO declares sandboxSeed cannot be embedded.
    #[test]
    fn parse_str_rejects_coding_task_with_sandbox_seed() {
        let raw = r#"{"workload":{"id":"y","provider":"coding-task","prompt":"do it","sandboxSeed":"sandbox"}}"#;
        let err = parse_str(raw, "y").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("coding-task"), "got: {msg}");
        assert!(msg.contains("sandboxSeed"), "got: {msg}");
    }

    #[test]
    fn parse_str_errors_on_malformed_json() {
        let err = parse_str("{not: valid", "broken").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("parsing embedded workload"), "got: {msg}");
        assert!(msg.contains("\"broken\""), "got: {msg}");
    }

    /// Forward-compat: asking for an embedded workload by id resolves it from
    /// the binary even when there's no on-disk template anywhere.
    #[serial_test::serial]
    #[test]
    fn embedded_workload_resolves_with_no_on_disk_templates() {
        let tmp = TempDir::new().unwrap();
        let prev = env::current_dir().unwrap();
        env::set_current_dir(tmp.path()).unwrap();
        unsafe { env::set_var("DARKMUX_TEMPLATES_DIR", tmp.path().join("nope")) };
        let result = load("quick-q", None);
        unsafe { env::remove_var("DARKMUX_TEMPLATES_DIR") };
        env::set_current_dir(prev).unwrap();
        let loaded = result.expect("quick-q should load from the embedded const");
        assert_eq!(loaded.manifest.workload.id, "quick-q");
        assert_eq!(loaded.source, WorkloadSource::Builtin);
    }
}
