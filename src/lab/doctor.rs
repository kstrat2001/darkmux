//! `darkmux lab doctor` — lint the lab registry for schema + path +
//! hash drift.
//!
//! Phase 4 of the lab-reproducibility cluster (#487, #491). Doctor
//! is cheap + offline: schema validation, path existence, content
//! hash recompute. No expensive operations (no `npm test`, no
//! network, no model dispatch).

use crate::lab::fixture::FixtureManifest;
use crate::lab::paths::{self, ResolveScope};
use crate::lab::registry::{default_registry_path, LabRegistry, RegisteredFixture};
use crate::lab::sandbox_hash::hash_sandbox_dir;
use anyhow::Result;

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
            "no registry found at {}\n  Options:\n    (a) Run the built-in example installer: `scripts/lab-init.sh`\n    (b) Register your own fixture:           `dm lab register /path/to/your/fixture/`\n    (c) Hand-write the registry:             see docs/lab-registry.md\n  Until then, `dm lab run` can't resolve any fixture by name.",
            reg_path.display()
        ));
        return Ok(report);
    }
    report.registry_present = true;

    let registry = LabRegistry::load(&reg_path)?;
    report.fixture_count = registry.fixtures.len();

    if registry.fixtures.is_empty() {
        report.warnings.push(format!(
            "registry at {} is empty — no fixtures registered.\n  Add one: `dm lab register /path/to/your/fixture/`",
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
    }

    Ok(report)
}

/// Run all doctor checks for one registered fixture. Returns Ok when
/// all pass; Err with an operator-formatted message on the first
/// failure.
fn check_one(name: &str, entry: &RegisteredFixture) -> std::result::Result<(), String> {
    if !entry.path.exists() {
        return Err(format!(
            "{name}: registered path no longer exists → {}\n  Either restore the dir OR `dm lab unregister {name}`",
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
            "{name}: content drift — recorded {} vs current {}\n  Either restore the fixture, OR\n  `dm lab register --force {}` to accept the new state",
            short_hash(&entry.content_hash),
            short_hash(&current_hash),
            entry.path.display()
        ));
    }

    // Manifest version drift (recorded vs on-disk).
    if entry.manifest_version != manifest.version {
        return Err(format!(
            "{name}: manifest version drift — recorded {} vs on-disk {}\n  `dm lab register --force {}` to accept",
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
        assert!(err.contains("dm lab register --force"), "got: {err}");
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
    fn short_hash_truncates_blake3() {
        assert_eq!(
            short_hash("blake3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
            "blake3:01234567…"
        );
        // Non-prefixed pass-through.
        assert_eq!(short_hash("xyz"), "xyz");
    }
}
