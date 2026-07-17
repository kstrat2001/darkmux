//! `darkmux lab doctor` — lint the lab registry for schema + path +
//! hash drift.
//!
//! Phase 4 of the lab-reproducibility cluster (#487, #491). Doctor
//! is cheap + offline: schema validation, path existence, content
//! hash recompute. No expensive operations (no `npm test`, no
//! network, no model dispatch).

use crate::lab::artifact_dirs::RUN_ARTIFACT_DIRS;
use crate::lab::fixture::FixtureManifest;
use crate::lab::paths::{self, ResolveScope};
use crate::lab::registry::{default_registry_path, LabRegistry, RegisteredFixture};
use crate::lab::sandbox_hash::hash_sandbox_dir;
use anyhow::Result;
use std::path::Path;

/// Result of doctor — operator-formatted output goes through the
/// CLI's print path. `notes` is the sequence of human-readable
/// findings; one entry per check. Doctor is fail-soft: missing
/// registry isn't an error, just a warning with next-step hints.
#[derive(Debug, Default)]
pub struct DoctorReport {
    pub registry_present: bool,
    pub fixture_count: usize,
    pub warnings: Vec<String>,
    pub passes: Vec<String>,
}

impl DoctorReport {
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }
}

pub fn lab_doctor() -> Result<DoctorReport> {
    let paths = paths::resolve(ResolveScope::Auto);
    let reg_path = default_registry_path(&paths);
    let mut report = DoctorReport::default();

    if !reg_path.exists() {
        report.warnings.push(format!(
            "no registry found at {}\n  Options:\n    (a) Bootstrap built-in synthetic fixtures:  `scripts/lab-init.sh`\n    (b) Register your own fixture:              `dm lab fixture register /path/to/your/fixture/`\n    (c) Hand-write the registry:                see docs/lab-registry.md (when published)\n  Until then, `dm lab run` can't resolve any fixture by name.",
            reg_path.display()
        ));
        return Ok(report);
    }
    report.registry_present = true;

    let registry = LabRegistry::load(&reg_path)?;
    report.fixture_count = registry.fixtures.len();

    if registry.fixtures.is_empty() {
        report.warnings.push(format!(
            "registry at {} is empty — no fixtures registered.\n  Add one: `dm lab fixture register /path/to/your/fixture/`",
            reg_path.display()
        ));
        return Ok(report);
    }

    for (name, entry) in &registry.fixtures {
        match check_one(name, entry) {
            Ok(()) => {
                report.passes.push(format!(
                    "{name} → {} (hash: {})",
                    entry.path.display(),
                    short_hash(&entry.content_hash),
                ));
            }
            Err(msg) => {
                report.warnings.push(msg);
            }
        }

        // Cleanliness walk (#487, #491): surface run-artifact dirs that
        // leaked into the fixture source. These are excluded from the
        // content hash (artifact_dirs.rs), so check_one's hash-drift
        // check is BLIND to them — a stale `.darkmux-runtime/` or
        // `coverage/` passes hash-clean. This is the observability gap.
        // Runs alongside check_one (not inside it) because it can emit
        // 0..N findings, which won't fit check_one's single-Result shape.
        for finding in check_fixture_cleanliness(name, &entry.path) {
            report.warnings.push(finding);
        }
    }

    Ok(report)
}

/// Walk a registered fixture's source dir for run-artifact directories
/// that shouldn't live in fixture content. Returns one advisory warning
/// per offending dir. ADVISORY ONLY — never deletes, never hard-fails
/// (operator-sovereignty + doctor's fail-soft posture): warn + suggest a
/// next-step, the operator decides.
///
/// A run-artifact dir (e.g. `.darkmux-runtime/`, `coverage/`) is excluded
/// from the content hash by `artifact_dirs::RUN_ARTIFACT_DIRS`, so it
/// never perturbs hash-equality — which is exactly why a stale one passes
/// `check_one`'s drift check completely clean. This walk is the only
/// doctor surface that sees it.
///
/// Shallow walk: the fixture root plus one level down. The real-world
/// contamination case (a leftover `.darkmux-runtime`/`coverage` from a
/// prior run) sits at the fixture root, and doctor's contract is
/// cheap + offline — a deep recursive walk would betray that. We do NOT
/// descend into `node_modules` (huge + legitimate, it's a
/// HASH_ONLY_EXCLUDE not a run-artifact dir) nor into the artifact dirs
/// themselves (no point — finding the dir is the whole signal).
///
/// `RUN_ARTIFACT_DIRS` is consumed directly from `artifact_dirs` rather
/// than re-listed here — re-listing is precisely the cross-copy drift
/// that #487's single-source-of-truth eliminated; this consumer inherits
/// the canonical list.
fn check_fixture_cleanliness(name: &str, root: &Path) -> Vec<String> {
    let mut findings = Vec::new();
    walk_for_artifacts(name, root, true, &mut findings);
    findings
}

/// One level of the cleanliness walk. `descend` gates the single
/// allowed level of recursion (root → one level down, then stop).
fn walk_for_artifacts(name: &str, dir: &Path, descend: bool, findings: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // Unreadable dir is non-fatal — doctor is fail-soft, and an
        // I/O error here just means we can't inspect this level.
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        if RUN_ARTIFACT_DIRS.contains(&file_name) {
            findings.push(artifact_warning(name, file_name, &path));
            // Do NOT descend into the artifact dir — the dir itself is
            // the whole signal; its contents add nothing.
            continue;
        }

        // `node_modules` is a HASH_ONLY_EXCLUDE, not a run-artifact dir —
        // it's legitimate fixture content. Don't flag it, and never
        // descend into it (huge + irrelevant to this check).
        if file_name == "node_modules" {
            continue;
        }

        if descend {
            walk_for_artifacts(name, &path, false, findings);
        }
    }
}

/// Advisory warning for one run-artifact dir found in a fixture source.
/// `.git` is in `RUN_ARTIFACT_DIRS` (for hash exclusion), and a
/// version-controlled fixture legitimately has one — so the wording is
/// conditional ("if it's leftover from a prior run"), never imperative.
fn artifact_warning(name: &str, dir: &str, path: &Path) -> String {
    let p = path.display();
    format!(
        "fixture '{name}': '{dir}' in source ({p}) is a run-artifact dir, not fixture content — it's excluded from the content hash but shouldn't live in a fixture. If it's leftover from a prior run: `rm -rf {p}` then `dm lab fixture register --force <fixture-path>` to re-baseline."
    )
}

/// Run all doctor checks for one registered fixture. Returns Ok when
/// all pass; Err with an operator-formatted message on the first
/// failure.
fn check_one(name: &str, entry: &RegisteredFixture) -> std::result::Result<(), String> {
    if !entry.path.exists() {
        return Err(format!(
            "{name}: registered path no longer exists → {}\n  Either restore the dir OR `dm lab fixture unregister {name}`",
            entry.path.display()
        ));
    }
    if !entry.path.is_dir() {
        return Err(format!(
            "{name}: registered path is not a directory → {}",
            entry.path.display()
        ));
    }

    let manifest = FixtureManifest::load_from_dir(&entry.path)
        .map_err(|e| format!("{name}: .fixture.json invalid: {e}"))?;

    let missing = manifest.missing_required_files(&entry.path);
    if !missing.is_empty() {
        return Err(format!(
            "{name}: {} required file(s) missing: {}",
            missing.len(),
            missing.join(", ")
        ));
    }

    // Recompute hash + compare to recorded.
    let current_hash = hash_sandbox_dir(&entry.path)
        .map_err(|e| format!("{name}: hash recompute failed: {e}"))?;
    if current_hash != entry.content_hash {
        return Err(format!(
            "{name}: content drift — recorded {} vs current {}\n  Either restore the fixture, OR\n  `dm lab fixture register --force {}` to accept the new state",
            short_hash(&entry.content_hash),
            short_hash(&current_hash),
            entry.path.display()
        ));
    }

    // Manifest version drift (recorded vs on-disk).
    if entry.manifest_version != manifest.version {
        return Err(format!(
            "{name}: manifest version drift — recorded {} vs on-disk {}\n  `dm lab fixture register --force {}` to accept",
            entry.manifest_version,
            manifest.version,
            entry.path.display()
        ));
    }

    Ok(())
}

/// Truncate a `blake3:<hex>` to a short readable form for output.
fn short_hash(h: &str) -> String {
    if let Some(rest) = h.strip_prefix("blake3:") {
        if rest.len() > 8 {
            return format!("blake3:{}…", &rest[..8]);
        }
    }
    h.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_fixture(dir: &std::path::Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(".fixture.json"),
            format!(r#"{{"name": "{name}"}}"#),
        )
        .unwrap();
        std::fs::write(dir.join("source.txt"), "baseline content").unwrap();
    }

    #[test]
    fn check_one_passes_for_unchanged_fixture() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("fx");
        write_fixture(&dir, "demo");
        let mut reg = LabRegistry::default();
        reg.register(&dir, None, false).unwrap();
        let entry = reg.get("demo").unwrap();
        assert!(check_one("demo", entry).is_ok());
    }

    #[test]
    fn check_one_warns_on_missing_path() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("fx");
        write_fixture(&dir, "demo");
        let mut reg = LabRegistry::default();
        reg.register(&dir, None, false).unwrap();
        // Now delete the dir.
        std::fs::remove_dir_all(&dir).unwrap();
        let entry = reg.get("demo").unwrap();
        let err = check_one("demo", entry).unwrap_err();
        assert!(err.contains("no longer exists"), "got: {err}");
    }

    #[test]
    fn check_one_detects_hash_drift() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("fx");
        write_fixture(&dir, "demo");
        let mut reg = LabRegistry::default();
        reg.register(&dir, None, false).unwrap();
        // Mutate a file → hash changes.
        std::fs::write(dir.join("source.txt"), "MUTATED").unwrap();
        let entry = reg.get("demo").unwrap();
        let err = check_one("demo", entry).unwrap_err();
        assert!(err.contains("content drift"), "got: {err}");
        assert!(err.contains("dm lab fixture register --force"), "got: {err}");
    }

    #[test]
    fn check_one_detects_missing_required_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("fx");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".fixture.json"),
            r#"{"name": "demo", "required_files": ["src/important.ts"]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("placeholder.txt"), "x").unwrap();
        let mut reg = LabRegistry::default();
        reg.register(&dir, None, false).unwrap();
        let entry = reg.get("demo").unwrap();
        let err = check_one("demo", entry).unwrap_err();
        assert!(err.contains("required file(s) missing"), "got: {err}");
        assert!(err.contains("src/important.ts"), "got: {err}");
    }

    #[test]
    fn cleanliness_check_warns_on_stale_artifact_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("fx");
        write_fixture(&dir, "demo");
        // Leak a stale run-artifact dir (with content) into the source.
        let stale = dir.join(".darkmux-runtime");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("trajectory.jsonl"), "{}").unwrap();
        let mut reg = LabRegistry::default();
        reg.register(&dir, None, false).unwrap();
        let entry = reg.get("demo").unwrap();

        // Hash-drift check passes clean (the artifact dir is excluded
        // from the hash) — proving the cleanliness walk is the only
        // surface that catches this.
        assert!(check_one("demo", entry).is_ok());

        let findings = check_fixture_cleanliness("demo", &entry.path);
        assert_eq!(findings.len(), 1, "exactly one finding; got: {findings:?}");
        assert!(
            findings[0].contains(".darkmux-runtime"),
            "got: {}",
            findings[0]
        );
        assert!(
            findings[0].contains("run-artifact dir"),
            "got: {}",
            findings[0]
        );
    }

    #[test]
    fn cleanliness_check_silent_on_clean_fixture() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("fx");
        write_fixture(&dir, "demo");
        // Real content only: a top-level file plus a src/ dir with a file.
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs").as_path(), "pub fn x() {}").unwrap();
        let mut reg = LabRegistry::default();
        reg.register(&dir, None, false).unwrap();
        let entry = reg.get("demo").unwrap();

        let findings = check_fixture_cleanliness("demo", &entry.path);
        assert!(
            findings.is_empty(),
            "expected zero findings; got: {findings:?}"
        );
    }

    /// `node_modules` is a HASH_ONLY_EXCLUDE, not a run-artifact dir —
    /// it's legitimate fixture content and must NOT trigger a warning.
    #[test]
    fn cleanliness_check_ignores_node_modules() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("fx");
        write_fixture(&dir, "demo");
        let nm = dir.join("node_modules").join("left-pad");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join("index.js"), "module.exports = {};").unwrap();
        let mut reg = LabRegistry::default();
        reg.register(&dir, None, false).unwrap();
        let entry = reg.get("demo").unwrap();

        let findings = check_fixture_cleanliness("demo", &entry.path);
        assert!(
            findings.is_empty(),
            "node_modules must not trigger a cleanliness warning; got: {findings:?}"
        );
    }

    #[test]
    fn short_hash_truncates_blake3() {
        assert_eq!(
            short_hash("blake3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
            "blake3:01234567…"
        );
        // Non-prefixed pass-through.
        assert_eq!(short_hash("xyz"), "xyz");
    }
}
