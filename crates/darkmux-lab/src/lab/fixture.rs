//! Fixture manifest format — `.fixture.json` lives inside every
//! registered fixture directory.
//!
//! Part of the lab-reproducibility cluster (#487, #489). A fixture
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
//!   "satisfies": "node-pepper-grinder-js@2.0",
//!   "language": "nodejs",
//!   "verify_command": "npm test -- {test_files}",
//!   "baseline": { "test_count": 14 },
//!   "required_files": [
//!     "src/tokenRotation.js"
//!   ]
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
/// existing manifest conventions in `workloads::types` (#489).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct FixtureManifest {
    /// (#3035) Data-shape version of this file (`darkmux_types::data_version::LAB_FIXTURE_SCHEMA_VERSION`).
    /// Absent means written before the marker existed, which is accepted; a
    /// newer value is refused by the unknown-key gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    /// Logical name of the fixture. Used as the registry key. Operator
    /// can override at register-time via `--name <name>`.
    pub name: String,

    /// Operator-named version. Free-form string (typically semver but
    /// not enforced). Pairs with `satisfies` for version requirements.
    #[serde(default = "default_version")]
    pub version: String,

    /// What abstract fixture definition this artifact satisfies.
    /// Format: `<definition-name>@<version>`. The resolver uses this
    /// for the workload's `requires_fixture` resolution.
    #[serde(default)]
    pub satisfies: Option<String>,

    /// Free-form language hint (e.g. `"nodejs"`, `"python"`, `"rust"`).
    /// Tools can use this to apply language-specific defaults; not
    /// load-bearing for the runtime.
    #[serde(default)]
    pub language: Option<String>,

    /// Verify-command template. `{test_files}` and other `{fixture.*}`
    /// placeholders substituted at dispatch time by the workload's
    /// verify-command template.
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

    /// (#3361) The eval's own folder, relative to the fixture root: the
    /// workload documents this fixture defines and their grading data. The
    /// eval owns its workload and grader; darkmux only processes them.
    /// `lab run` loads a workload the fixture defines from here, and a run's
    /// sandbox never holds this folder, so the model never sees the grader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_dir: Option<String>,
}

fn default_version() -> String {
    "1.0".to_string()
}

impl FixtureManifest {
    /// Read + parse `.fixture.json` from inside a fixture dir.
    pub(crate) fn load_from_dir(dir: &Path) -> Result<Self> {
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
        manifest.check_eval_dir_case(dir)?;
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
        self.validate_eval_dir()
    }

    /// `eval_dir`'s invariants: one plain folder name, bound by `satisfies`,
    /// and declared by every fixture whose eval owns its roles (#3363). The
    /// schema marker must parse first, since it decides that.
    fn validate_eval_dir(&self) -> Result<()> {
        if let Some(problem) = self.schema_version.as_deref().and_then(unreadable_schema_version) {
            return Err(anyhow!("{problem}"));
        }
        if let Some(dir) = &self.eval_dir {
            if !is_plain_name(dir) {
                return Err(anyhow!(
                    "eval_dir `{dir}` must be one folder name at the top of the fixture (e.g. `eval`): \
                     letters, digits, `.`, `_` and `-`, never `.` or `..`"
                ));
            }
            if self.satisfies.is_none() {
                return Err(anyhow!(
                    "eval_dir `{dir}` needs `satisfies`: a workload the eval defines binds its fixture by it"
                ));
            }
        }
        if self.owns_its_roles() && self.eval_dir.is_none() {
            return Err(anyhow!(
                "schema_version `{}` means the eval owns its roles, at `<eval_dir>/roles/<id>.json` and `.md`, \
                 so it needs `eval_dir` (e.g. `\"eval_dir\": \"eval\"`)",
                self.schema_version.as_deref().unwrap_or_default()
            ));
        }
        Ok(())
    }

    /// (#3396 re-review) Refuse an `eval_dir` the checkout holds only under
    /// another case (`Eval/` for `eval`): a case-folding filesystem would open
    /// it while every exact-name lookup misses it, and a case-sensitive one
    /// would clone it into the model's sandbox. A declared folder that is
    /// absent altogether is not this check's to refuse.
    fn check_eval_dir_case(&self, dir: &Path) -> Result<()> {
        let Some(eval_dir) = &self.eval_dir else { return Ok(()) };
        let names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        match names.iter().find(|n| n.eq_ignore_ascii_case(eval_dir)) {
            Some(found) if found != eval_dir && !names.contains(eval_dir) => Err(anyhow!(
                "eval_dir `{eval_dir}` names no folder by that exact name; the checkout has `{found}`. \
                 Rename one so they match: names are matched exactly, on every OS"
            )),
            _ => Ok(()),
        }
    }

    /// (#3363) Whether this eval owns its roles: its `schema_version` is
    /// 1.2 or newer. Absent, older or unreadable is the legacy rule.
    pub(crate) fn owns_its_roles(&self) -> bool {
        use darkmux_types::data_version::{at_least, LAB_FIXTURE_EVAL_ROLES_SINCE};
        self.schema_version
            .as_ref()
            .is_some_and(|v| at_least(&serde_json::Value::String(v.clone()), LAB_FIXTURE_EVAL_ROLES_SINCE))
    }

    /// Check whether on-disk fixture dir has all `required_files`
    /// present. Returns the list of missing paths (empty = all present).
    pub(crate) fn missing_required_files(&self, fixture_dir: &Path) -> Vec<String> {
        self.required_files
            .iter()
            .filter(|rel| !fixture_dir.join(rel).exists())
            .cloned()
            .collect()
    }
}

/// (#3396 review) Why a fixture's `schema_version` cannot be read, or
/// `None` when it can. The marker decides where an eval's role comes from
/// (1.2 or newer owns its roles), so a value darkmux cannot compare is
/// refused rather than read as the legacy rule. `data_version::newer` reads
/// such a value as "not newer" for every kind (5.8 did too); this refusal is
/// the fixture's own, on top of that.
pub(crate) fn unreadable_schema_version(version: &str) -> Option<String> {
    let parsed = darkmux_types::data_version::parse(&serde_json::Value::String(version.to_string()));
    parsed.is_none().then(|| {
        format!(
            "schema_version `{}` is not a version darkmux can read: write digits and dots, such as `1.2`. \
             A fixture's schema decides where its eval's role comes from, so darkmux does not guess",
            darkmux_types::user_files::escape_text(version)
        )
    })
}

/// [`unreadable_schema_version`] as a gate issue, for the unknown-key gate's
/// `extra` (the lab-run preflight and `darkmux doctor`).
pub(crate) fn schema_version_issues(doc: &serde_json::Value) -> Vec<darkmux_types::user_files::KeyIssue> {
    use darkmux_types::user_files::{Issue, KeyIssue};
    let Some(version) = doc.get("schema_version").and_then(serde_json::Value::as_str) else { return Vec::new() };
    unreadable_schema_version(version)
        .map(|line| KeyIssue { path: "schema_version".into(), issue: Issue::Rule(line) })
        .into_iter()
        .collect()
}

/// One plain name: letters, digits, `.`, `_` and `-`, and not `.` or `..`.
/// The grammar the site reads a workload path in, so a path built of these
/// names is one it accepts; a name is also never a path that could leave the
/// directory it is joined to.
pub(crate) fn is_plain_name(name: &str) -> bool {
    !matches!(name, "" | "." | "..") && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// (#3361) The eval folder a fixture checkout declares, a name at its top: `None` when it has no `.fixture.json`, the manifest
/// cannot be read, or it declares none. The sandbox clone prunes it and the
/// grader's scope check skips it.
pub(crate) fn eval_dir_of(fixture_dir: &Path) -> Option<String> {
    if !fixture_dir.join(".fixture.json").exists() {
        return None;
    }
    FixtureManifest::load_from_dir(fixture_dir).ok()?.eval_dir
}

impl FixtureManifest {
    /// (#3363) Where a lab run of a workload bound to this fixture takes its
    /// role, which the fixture's schema decides. `validate` already refused
    /// a 1.2 manifest without `eval_dir` and a marker that does not parse.
    pub(crate) fn role_layout(&self) -> darkmux_crew::dispatch::RoleLayout {
        use darkmux_crew::dispatch::RoleLayout;
        match &self.eval_dir {
            Some(eval_dir) if self.owns_its_roles() => RoleLayout::Eval { eval_dir: eval_dir.clone() },
            eval_dir => RoleLayout::Legacy { eval_dir: eval_dir.clone() },
        }
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
                "satisfies": "node-pepper-grinder-js@2.0",
                "language": "nodejs",
                "verify_command": "npm test -- {test_files}",
                "baseline": {"test_count": 14},
                "required_files": ["src/tokenRotation.js"]
            }"#,
        );
        let m = FixtureManifest::load_from_dir(tmp.path()).unwrap();
        assert_eq!(m.name, "pepper-grinder");
        assert_eq!(m.satisfies.as_deref(), Some("node-pepper-grinder-js@2.0"));
        assert_eq!(m.language.as_deref(), Some("nodejs"));
        assert_eq!(m.required_files.len(), 1);
        assert_eq!(m.baseline["test_count"], 14);
    }

    #[test]
    fn eval_dir_is_read_and_named_by_eval_dir_of() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), r#"{"name": "pg", "satisfies": "pg@2.1", "eval_dir": "eval"}"#);
        assert_eq!(FixtureManifest::load_from_dir(tmp.path()).unwrap().eval_dir.as_deref(), Some("eval"));
        assert_eq!(eval_dir_of(tmp.path()).as_deref(), Some("eval"));
        let none = TempDir::new().unwrap();
        assert_eq!(eval_dir_of(none.path()), None);
    }

    /// (#3396 re-review) An `eval_dir` the checkout holds only under another
    /// case (`Eval/` for `eval`) is refused, on every OS: macOS would open it
    /// for the role lookup's purposes while the exact-name lookup and the
    /// sandbox pruning disagree, and Linux would clone it into the sandbox.
    /// A declared `eval_dir` with no folder at all is left to the run.
    #[test]
    fn an_eval_dir_cased_unlike_its_folder_is_refused() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), r#"{"name": "pg", "satisfies": "pg@2.1", "eval_dir": "eval"}"#);
        std::fs::create_dir_all(tmp.path().join("Eval/roles")).unwrap();
        let err = format!("{:#}", FixtureManifest::load_from_dir(tmp.path()).unwrap_err());
        assert!(err.contains("eval_dir `eval`") && err.contains("`Eval`"), "{err}");
        let exact = TempDir::new().unwrap();
        write_manifest(exact.path(), r#"{"name": "pg", "satisfies": "pg@2.1", "eval_dir": "eval"}"#);
        FixtureManifest::load_from_dir(exact.path()).expect("no folder at all is not this refusal");
        std::fs::create_dir_all(exact.path().join("eval")).unwrap();
        FixtureManifest::load_from_dir(exact.path()).expect("the exact folder loads");
    }

    #[test]
    fn eval_dir_must_stay_inside_the_fixture() {
        for bad in ["", ".", "..", "../grader", "/abs", "./eval", "eval/../..", "a\\b", "grading/v1", "my eval"] {
            let m: FixtureManifest =
                serde_json::from_value(serde_json::json!({"name": "pg", "satisfies": "pg@2.1", "eval_dir": bad})).unwrap();
            let err = m.validate().unwrap_err().to_string();
            assert!(err.contains("eval_dir"), "{bad}: {err}");
        }
        let plain: FixtureManifest =
            serde_json::from_value(serde_json::json!({"name": "pg", "satisfies": "pg@2.1", "eval_dir": "eval-v1.2"})).unwrap();
        plain.validate().unwrap();
    }

    /// (#3363) Fixture schema 1.2 means the eval owns its roles, at
    /// `<eval_dir>/roles/`, so a 1.2 manifest must declare `eval_dir`.
    #[test]
    fn a_schema_1_2_fixture_must_declare_eval_dir() {
        let m: FixtureManifest =
            serde_json::from_value(serde_json::json!({"schema_version": "1.2", "name": "pg", "satisfies": "pg@2.1"})).unwrap();
        let err = m.validate().unwrap_err().to_string();
        assert!(err.contains("schema_version") && err.contains("eval_dir"), "{err}");
        for legacy in [serde_json::json!("1.1"), serde_json::json!(null)] {
            let m: FixtureManifest =
                serde_json::from_value(serde_json::json!({"schema_version": legacy, "name": "pg", "satisfies": "pg@2.1"})).unwrap();
            m.validate().unwrap();
        }
    }

    /// (#3396 review) A `schema_version` darkmux cannot parse is refused,
    /// naming the value, rather than read as the legacy rule: the marker
    /// decides where the eval's role comes from.
    #[test]
    fn an_unparseable_schema_version_is_refused() {
        for bad in [" 1.2", "1.2 ", "v1.2", "1.2-rc1", ""] {
            let doc = serde_json::json!({"schema_version": bad, "name": "pg", "satisfies": "pg@2.1", "eval_dir": "eval"});
            let m: FixtureManifest = serde_json::from_value(doc.clone()).unwrap();
            let err = m.validate().unwrap_err().to_string();
            assert!(err.contains("schema_version") && err.contains(&format!("`{bad}`")), "{bad:?}: {err}");
            let tmp = TempDir::new().unwrap();
            write_manifest(tmp.path(), &doc.to_string());
            assert!(FixtureManifest::load_from_dir(tmp.path()).is_err(), "{bad:?}");
        }
    }

    /// (#3363) The role layout is keyed on the fixture's declared schema:
    /// 1.2 or newer owns its roles in `eval_dir`; below 1.2 or none is legacy.
    #[test]
    fn the_role_layout_follows_the_fixtures_schema() {
        use darkmux_crew::dispatch::RoleLayout;
        let layout = |json: &str| serde_json::from_str::<FixtureManifest>(json).unwrap().role_layout();
        let eval = || RoleLayout::Eval { eval_dir: "eval".into() };
        let legacy = |dir: Option<&str>| RoleLayout::Legacy { eval_dir: dir.map(String::from) };
        assert_eq!(layout(r#"{"schema_version": "1.2", "name": "pg", "satisfies": "pg@2.1", "eval_dir": "eval"}"#), eval());
        assert_eq!(layout(r#"{"schema_version": "1.10", "name": "pg", "satisfies": "pg@2.1", "eval_dir": "eval"}"#), eval());
        assert_eq!(layout(r#"{"schema_version": "2", "name": "pg", "satisfies": "pg@2.1", "eval_dir": "eval"}"#), eval());
        assert_eq!(layout(r#"{"schema_version": "1.1", "name": "pg", "satisfies": "pg@2.1", "eval_dir": "eval"}"#), legacy(Some("eval")));
        assert_eq!(layout(r#"{"schema_version": "1.0", "name": "pg", "satisfies": "pg@2.1", "eval_dir": "eval"}"#), legacy(Some("eval")));
        assert_eq!(layout(r#"{"name": "pg", "satisfies": "pg@2.1", "eval_dir": "eval"}"#), legacy(Some("eval")));
        assert_eq!(layout(r#"{"name": "pg"}"#), legacy(None));
    }

    #[test]
    fn eval_dir_needs_satisfies() {
        let m: FixtureManifest = serde_json::from_value(serde_json::json!({"name": "pg", "eval_dir": "eval"})).unwrap();
        assert!(m.validate().unwrap_err().to_string().contains("needs `satisfies`"));
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
