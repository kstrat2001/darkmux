//! Load mission configs by id from any of several known locations.
//! Search order: user dir → on-disk built-in template dirs → binary-embedded
//! built-ins. Closely mirrors `darkmux_lab::workloads::load`'s resolution
//! (#1284 Packet 1) — same three-tier shape, same
//! `templates_override_dirs()` override accessor, same home/system candidate
//! tree — with one deliberate omission: workloads also resolve a
//! nested `<dir>/<id>/workload.json` form (for manifests that ship sibling
//! on-disk resources like sandbox seeds); mission configs are pure data
//! with no sibling files, so only the flat `<dir>/<id>.json` form exists
//! here.
//!
//! **(#2432) The on-disk tier is NOT cwd-sensitive.** It used to
//! unconditionally search `<shell's cwd>/templates/builtin/mission-configs/`
//! ahead of the embedded tier, unscoped by anything the operator declared —
//! so `darkmux mission launch <id>` picked a different document depending
//! on which of an operator's several worktrees the shell happened to be
//! standing in, with no signal that the choice was an accident of `pwd`
//! rather than a decision. `CONTRIBUTING.md`'s documented dev loop is
//! rebuild-to-embed (`include_str!` resolves at compile time), so that cwd
//! search was never the sanctioned "edit and rerun" path anyway — the
//! sanctioned explicit path is `DARKMUX_TEMPLATES_DIR` (or
//! `config.dirs.templates`), which already sits at the TOP of
//! [`builtin_dirs`] via [`darkmux_types::config_access::templates_override_dirs`].
//! An operator who wants "run the templates in this checkout" sets that
//! var explicitly for the session; darkmux no longer infers it from `pwd`.

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
        "coder-phase",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../templates/builtin/mission-configs/coder-phase.json"
        )),
    ),
    // (#1959) Documentation-only — `crawl` is routed by literal config id
    // in `mission_launch::launch`, BEFORE this (or any) document loads;
    // its Task/Step graph is computed at run time, never declared here.
    // Embedded purely so `mission config list`/`show` can enumerate its
    // inputs — see the document's own `description` field.
    (
        "crawl",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../templates/builtin/mission-configs/crawl.json"
        )),
    ),
    // (#2310 P4d) `review` — the code review, built on the crawl's shared
    // building blocks. The bespoke funnel launcher it replaced is deleted;
    // this document runs through the generic launcher exactly like `crawl`
    // (no bespoke launcher, no literal routing).
    (
        "review",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../templates/builtin/mission-configs/review.json"
        )),
    ),
];

/// The raw embedded JSON for a built-in id. `pub(crate)` (#1284 review
/// round 1) so `mission_config`'s golden tests can parse the EMBEDDED
/// documents directly — they are goldens for the embedded constants, and
/// loading them through the full user → on-disk → embedded chain both
/// tested the wrong thing and was unisolated (it raced this module's own
/// `#[serial]` tests that write user-tier stubs, and would read a real
/// operator override at `~/.darkmux/mission-configs/<id>.json` on a dev
/// machine). The chain's embedded-tier resolution keeps its own
/// `#[serial]`-guarded test below
/// (`embedded_resolves_with_no_user_or_on_disk_copy`).
/// Every embedded built-in as `(id, raw JSON)` — test-only, for the same
/// reason [`find_embedded`] is `pub(crate)`: `mission_config`'s golden
/// tests assert document-wide invariants over the WHOLE embedded set, and
/// a test that enumerates the set itself is the only kind a newly-added
/// fifth config cannot silently escape. Production code always resolves
/// one config by id through the user → on-disk → embedded chain, never the
/// whole table, so this is `#[cfg(test)]` rather than dead weight in the
/// shipped binary.
#[cfg(test)]
pub(crate) fn embedded_all() -> &'static [(&'static str, &'static str)] {
    EMBEDDED_MISSION_CONFIGS
}

pub(crate) fn find_embedded(id: &str) -> Option<&'static str> {
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
    /// A `templates/builtin/mission-configs/<id>.json` found on disk — the
    /// explicit `DARKMUX_TEMPLATES_DIR`/`config.dirs.templates` override, or
    /// the `~/.darkmux/templates/...` / `/usr/local/share/...` candidates.
    /// (#2432) NOT the shell's cwd — see [`builtin_dirs`].
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
/// uses), prepended ahead of home/system.
///
/// **(#2432) Deliberately NOT cwd-sensitive.** An earlier version of this
/// function also pushed `<cwd>/templates/builtin/mission-configs`, so the
/// document that won depended on which directory the shell happened to be
/// in when `darkmux` ran — invisible to an operator who didn't already know
/// to suspect it, and dangerous specifically when the cwd's document shares
/// the binary's `schema_version` but has DIFFERENT content (a stale or
/// half-edited worktree), because that case parses and validates cleanly
/// with no error to catch it. The explicit `templates_override_dirs()` tier
/// above already gives an operator who wants "this checkout's templates"
/// exactly that, on purpose: `DARKMUX_TEMPLATES_DIR=$PWD/templates/builtin`.
fn builtin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for base in darkmux_types::config_access::templates_override_dirs() {
        dirs.push(base.join("mission-configs"));
    }
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

/// Whether `id` has a NON-user tier to fall back to — an on-disk built-in
/// template ([`builtin_dirs`]) or a binary-embedded built-in
/// ([`EMBEDDED_MISSION_CONFIGS`]) — distinct from merely having a user-tier
/// copy. [`load`] never reports this on its own: it returns whichever tier
/// WON the search, so a caller that needs "would deleting the user-tier copy
/// still resolve to something" has to ask separately rather than infer it
/// from a successful `load`. Built for `darkmux doctor`'s mission-config
/// drift check (#1917), which was advising "delete it to fall back to the
/// embedded tier" for documents that have no embedded or on-disk tier at
/// all — this is the query that check needed and didn't have.
pub fn has_non_user_fallback(id: &str) -> bool {
    for d in builtin_dirs() {
        if find_in_dir(&d, id).is_some() {
            return true;
        }
    }
    find_embedded(id).is_some()
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

    /// #1917 — the function `darkmux doctor`'s mission-config drift remedy
    /// needed and didn't have. "review" resolves to the embedded built-in
    /// (with no user or on-disk copy present), so a fallback genuinely
    /// exists.
    #[test]
    #[serial_test::serial]
    fn has_non_user_fallback_true_for_an_embedded_builtin() {
        let user_tmp = TempDir::new().unwrap();
        let _crew_guard = CrewDirGuard::new(user_tmp);
        let empty = TempDir::new().unwrap();
        let _templates_guard = NoBuiltinTemplatesGuard::new(empty.path());
        assert!(has_non_user_fallback("review"), "review is embedded — a fallback exists");
    }

    /// #1917 — an id with no on-disk or embedded counterpart (the shape of
    /// every `pr-*` GitHub-verb config on the reporting operator's machine)
    /// reports no fallback. A synthetic id, not a real verb name, so this
    /// can't collide with a real `~/.darkmux/templates/...` tree on whatever
    /// machine runs the test.
    #[test]
    #[serial_test::serial]
    fn has_non_user_fallback_false_for_a_user_only_id() {
        let empty = TempDir::new().unwrap();
        let _templates_guard = NoBuiltinTemplatesGuard::new(empty.path());
        assert!(
            !has_non_user_fallback("definitely-fake-id-1917"),
            "a made-up id has no built-in counterpart — nothing to fall back to"
        );
    }

    /// RAII guard that changes the process cwd for the test's duration and
    /// restores it on drop. Every caller MUST be `#[serial_test::serial]` —
    /// cwd is a process-global resource, and `serial_test` only coordinates
    /// among ANNOTATED tests, not any unannotated test elsewhere in this
    /// crate that happens to read/write cwd too.
    struct CwdGuard {
        prev: PathBuf,
    }

    impl CwdGuard {
        fn new(dir: &Path) -> Self {
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir).unwrap();
            Self { prev }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.prev);
        }
    }

    /// #2432 — the on-disk tier must NOT be cwd-sensitive. A valid "review"
    /// document with DIFFERENT content sits at `<cwd>/templates/builtin/
    /// mission-configs/review.json` (the exact shape of a stale worktree's
    /// checked-out templates), with no user-tier copy and no
    /// `DARKMUX_TEMPLATES_DIR` override. `load("review")` must resolve the
    /// EMBEDDED built-in, not the cwd document — this is the precedence
    /// claim itself, not just "something loaded": before the #2432 fix this
    /// test fails, resolving `MissionConfigSource::OnDisk` with the cwd
    /// document's name instead of the embedded one's.
    #[test]
    #[serial_test::serial]
    fn on_disk_tier_ignores_cwd_templates() {
        let user_tmp = TempDir::new().unwrap();
        let _crew_guard = CrewDirGuard::new(user_tmp);
        let no_override = TempDir::new().unwrap();
        let _templates_guard = NoBuiltinTemplatesGuard::new(no_override.path());

        let cwd_tmp = TempDir::new().unwrap();
        write(
            &cwd_tmp.path().join("templates/builtin/mission-configs/review.json"),
            r#"{"id":"review","name":"cwd should never win","schema_version":"1.0"}"#,
        );
        let _cwd_guard = CwdGuard::new(cwd_tmp.path());

        let loaded = load("review").expect("review must still resolve via the embedded tier");
        assert_eq!(
            loaded.source,
            MissionConfigSource::Embedded,
            "a document sitting in cwd's templates/ must not outrank the embedded built-in"
        );
        assert_ne!(
            loaded.config.name, "cwd should never win",
            "the cwd-local document's content must not win"
        );
    }

    /// A synthetic id with NO user, embedded, or `DARKMUX_TEMPLATES_DIR`
    /// counterpart, present ONLY under `<cwd>/templates/builtin/
    /// mission-configs/` — the shape of a document that resolves purely
    /// because the shell happens to be standing inside some worktree.
    /// `load()` must report it not found, not silently resolve it from cwd.
    #[test]
    #[serial_test::serial]
    fn on_disk_tier_does_not_resolve_a_cwd_only_id() {
        let user_tmp = TempDir::new().unwrap();
        let _crew_guard = CrewDirGuard::new(user_tmp);
        let no_override = TempDir::new().unwrap();
        let _templates_guard = NoBuiltinTemplatesGuard::new(no_override.path());

        let cwd_tmp = TempDir::new().unwrap();
        write(
            &cwd_tmp
                .path()
                .join("templates/builtin/mission-configs/cwd-only-2432.json"),
            &doc_json("cwd-only-2432"),
        );
        let _cwd_guard = CwdGuard::new(cwd_tmp.path());

        let err = load("cwd-only-2432").unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    /// #2432 — the removal's own justification ("the explicit override
    /// already sits at the top of `builtin_dirs`") has no coverage without
    /// this test. `DARKMUX_TEMPLATES_DIR` holds a "review" document with
    /// DIFFERENT content than the embedded built-in, no user-tier copy
    /// exists, and no cwd document is involved at all — the override must
    /// still beat the embedded tier. Red-proved by hoisting the
    /// `find_embedded` check above the `builtin_dirs()` loop in [`load`]:
    /// that mutation disables the override outright (the embedded document
    /// wins over any `DARKMUX_TEMPLATES_DIR` the operator sets) and this is
    /// the only test in the module that goes red for it —
    /// `resolves_on_disk_when_present_and_no_user_override` can't catch it
    /// because its synthetic id has no embedded counterpart to be
    /// shadowed by.
    #[test]
    #[serial_test::serial]
    fn on_disk_override_beats_embedded_for_a_known_id() {
        let user_tmp = TempDir::new().unwrap();
        let _crew_guard = CrewDirGuard::new(user_tmp); // empty user dir
        let templates_tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_TEMPLATES_DIR").ok();
        // SAFETY: serialized via #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_TEMPLATES_DIR", templates_tmp.path()) };
        write(
            &templates_tmp.path().join("mission-configs/review.json"),
            r#"{"id":"review","name":"on-disk override should win"}"#,
        );
        let loaded = load("review");
        // SAFETY: serialized via #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_TEMPLATES_DIR", v),
                None => std::env::remove_var("DARKMUX_TEMPLATES_DIR"),
            }
        }
        let loaded = loaded.expect("review must resolve via the on-disk override");
        assert_eq!(loaded.source, MissionConfigSource::OnDisk);
        assert_eq!(loaded.config.name, "on-disk override should win");
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
