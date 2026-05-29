//! Fixture manifest format — `.fixture.json` lives inside every
//! registered fixture directory.
//!
//! Phase 2 of the lab-reproducibility cluster (#487, #489). A fixture
//! is a self-contained directory (definition + artifact merged). The
//! manifest declares what the fixture is, what it satisfies, how to
//! verify it, and what's load-bearing for hashing.
//!
//! ## Shape
//!
//! ```json
//! {
//!   "name": "pepper-grinder",
//!   "version": "1.0",
//!   "satisfies": "node-refresh-token-rotation@1.0",
//!   "language": "nodejs",
//!   "verify_command": "npm test -- {test_files}",
//!   "baseline": { "test_count": 77 },
//!   "required_files": [
//!     "src/services/refreshTokenService.ts"
//!   ],
//!   "hash_include": [],
//!   "hash_exclude": ["node_modules", ".git", "target", "__pycache__"]
//! }
//! ```
//!
//! ## What's optional
//!
//! Minimum viable manifest: just `name`. Everything else has sensible
//! defaults. A registered fixture with just `{"name": "demo"}` works.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Parsed `.fixture.json`. Field naming uses snake_case to match
/// existing manifest conventions in `workloads::types`. Phase 2
/// (#489) ships the type + parser; Phase 3 + 4 are the consumers
/// (resolver + doctor), so most fields/methods read as dead-code
/// until then.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureManifest {
    /// Logical name of the fixture. Used as the registry key. Operator
    /// can override at register-time via `--name <name>`.
    pub name: String,

    /// Operator-named version. Free-form string (typically semver but
    /// not enforced). Pairs with `satisfies` for version requirements.
    #[serde(default = "default_version")]
    pub version: String,

    /// What abstract fixture definition this artifact satisfies.
    /// Format: `<definition-name>@<version>`. Phase 3 uses this for
    /// the workload's `requires_fixture` resolution.
    #[serde(default)]
    pub satisfies: Option<String>,

    /// Free-form language hint (e.g. `"nodejs"`, `"python"`, `"rust"`).
    /// Tools can use this to apply language-specific defaults; not
    /// load-bearing for the runtime.
    #[serde(default)]
    pub language: Option<String>,

    /// Verify-command template. `{test_files}` and other `{fixture.*}`
    /// placeholders substituted at dispatch time by the workload's
    /// verify-command template (Phase 3).
    #[serde(default)]
    pub verify_command: Option<String>,

    /// Baseline expectations (operator-declared; `dm lab doctor`
    /// surfaces mismatches as warnings). Free-form JSON object.
    #[serde(default)]
    pub baseline: serde_json::Value,

    /// Files that MUST exist for this fixture to be valid. Doctor
    /// fails the fixture if any required_file is missing.
    #[serde(default)]
    pub required_files: Vec<String>,

    /// Extra paths to include in the content hash beyond what's
    /// auto-discovered. Useful when a fixture has out-of-tree files
    /// that should affect the hash (e.g., a shared lockfile under
    /// the fixture root).
    #[serde(default)]
    pub hash_include: Vec<String>,

    /// Paths to exclude from the content hash. Layered on top of the
    /// sandbox_hash module's default exclusion list.
    #[serde(default)]
    pub hash_exclude: Vec<String>,
}

fn default_version() -> String {
    "1.0".to_string()
}

#[allow(dead_code)]
impl FixtureManifest {
    /// Read + parse `.fixture.json` from inside a fixture dir.
    pub fn load_from_dir(dir: &Path) -> Result<Self> {
        let manifest_path = dir.join(".fixture.json");
        if !manifest_path.exists() {
            return Err(anyhow!(
                "no .fixture.json found at {}",
                manifest_path.display()
            ));
        }
        let raw = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?;
        let manifest: FixtureManifest = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {} as fixture manifest", manifest_path.display()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Verify the manifest's invariants. Cheap; doesn't touch disk
    /// beyond the parent dir already known.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(anyhow!("fixture name cannot be empty"));
        }
        if self.name.contains('/') || self.name.contains(std::path::MAIN_SEPARATOR) {
            return Err(anyhow!(
                "fixture name `{}` contains path separators",
                self.name
            ));
        }
        if let Some(satisfies) = &self.satisfies {
            if !satisfies.contains('@') {
                return Err(anyhow!(
                    "satisfies `{}` must be `<name>@<version>` (no @ found)",
                    satisfies
                ));
            }
        }
        Ok(())
    }

    /// Returns (definition_name, version) split from `satisfies`.
    /// Returns `None` when `satisfies` is unset.
    pub fn satisfies_parts(&self) -> Option<(&str, &str)> {
        self.satisfies
            .as_deref()
            .and_then(|s| s.split_once('@'))
    }

    /// Check whether on-disk fixture dir has all `required_files`
    /// present. Returns the list of missing paths (empty = all present).
    pub fn missing_required_files(&self, fixture_dir: &Path) -> Vec<String> {
        self.required_files
            .iter()
            .filter(|rel| !fixture_dir.join(rel).exists())
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, json: &str) {
        std::fs::write(dir.join(".fixture.json"), json).unwrap();
    }

    #[test]
    fn loads_minimal_manifest() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), r#"{"name": "demo"}"#);
        let m = FixtureManifest::load_from_dir(tmp.path()).unwrap();
        assert_eq!(m.name, "demo");
        assert_eq!(m.version, "1.0"); // default
        assert!(m.satisfies.is_none());
    }

    #[test]
    fn loads_full_manifest() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{
                "name": "pepper-grinder",
                "version": "1.0",
                "satisfies": "node-refresh-token-rotation@1.0",
                "language": "nodejs",
                "verify_command": "npm test -- {test_files}",
                "baseline": {"test_count": 77},
                "required_files": ["src/services/refreshTokenService.ts"],
                "hash_exclude": ["node_modules", ".git"]
            }"#,
        );
        let m = FixtureManifest::load_from_dir(tmp.path()).unwrap();
        assert_eq!(m.name, "pepper-grinder");
        assert_eq!(m.satisfies.as_deref(), Some("node-refresh-token-rotation@1.0"));
        assert_eq!(m.language.as_deref(), Some("nodejs"));
        assert_eq!(m.required_files.len(), 1);
        assert_eq!(m.hash_exclude.len(), 2);
        assert_eq!(m.baseline["test_count"], 77);
    }

    #[test]
    fn rejects_missing_manifest() {
        let tmp = TempDir::new().unwrap();
        let err = FixtureManifest::load_from_dir(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("no .fixture.json found"), "got: {err}");
    }

    #[test]
    fn rejects_malformed_json() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), "not json at all");
        let err = FixtureManifest::load_from_dir(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("parsing"), "got: {err}");
    }

    #[test]
    fn rejects_empty_name() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), r#"{"name": ""}"#);
        let err = FixtureManifest::load_from_dir(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("name cannot be empty"), "got: {err}");
    }

    #[test]
    fn rejects_name_with_slashes() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), r#"{"name": "foo/bar"}"#);
        let err = FixtureManifest::load_from_dir(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("path separators"), "got: {err}");
    }

    #[test]
    fn rejects_satisfies_without_at_sign() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), r#"{"name": "demo", "satisfies": "missing-at-sign"}"#);
        let err = FixtureManifest::load_from_dir(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("@"), "got: {err}");
    }

    #[test]
    fn satisfies_parts_splits_correctly() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{"name": "demo", "satisfies": "tiny-python-suite@1.0"}"#,
        );
        let m = FixtureManifest::load_from_dir(tmp.path()).unwrap();
        assert_eq!(m.satisfies_parts(), Some(("tiny-python-suite", "1.0")));
    }

    #[test]
    fn missing_required_files_reports_each() {
        let tmp = TempDir::new().unwrap();
        // Create some files but not others.
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/exists.ts"), "x").unwrap();
        write_manifest(
            tmp.path(),
            r#"{
                "name": "demo",
                "required_files": [
                    "src/exists.ts",
                    "src/missing.ts",
                    "tests/missing.test.ts"
                ]
            }"#,
        );
        let m = FixtureManifest::load_from_dir(tmp.path()).unwrap();
        let missing = m.missing_required_files(tmp.path());
        assert_eq!(missing.len(), 2);
        assert!(missing.contains(&"src/missing.ts".to_string()));
        assert!(missing.contains(&"tests/missing.test.ts".to_string()));
    }

    #[test]
    fn missing_required_files_empty_when_all_present() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        write_manifest(
            tmp.path(),
            r#"{"name": "demo", "required_files": ["a.txt"]}"#,
        );
        let m = FixtureManifest::load_from_dir(tmp.path()).unwrap();
        assert!(m.missing_required_files(tmp.path()).is_empty());
    }
}
