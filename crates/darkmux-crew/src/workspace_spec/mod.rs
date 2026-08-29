//! Workspace spec (#1959) — a generic mission input: named sources (a git
//! remote or a local path, at a ref) materialized into a read-only tree,
//! filtered by `include`/`exclude` globs, with named edges between
//! sources. ANY mission can take one — this type carries nothing
//! crawl-specific. The crawl launcher consumes a materialized workspace
//! whole (workspace + rules -> work units, see
//! `darkmux_lab::crawl::plan`); the PR-review pipeline uses a workspace
//! spec's `include`/`exclude` alone as an additive filter over a diff (see
//! `SkipReason::ExcludedByWorkspaceSpec` in `darkmux_lab::lab::review`) —
//! it never materializes a tree, since review bundles from a diff, not a
//! checked-out tree.
//!
//! Promoted out of the crawl module's `CorpusManifest`
//! (`darkmux_lab::crawl::manifest`, #1959 refactor). Schema + validation
//! here follow that type's shape almost exactly (source-id charset,
//! case-insensitive uniqueness, edge-reference checks, lenient-on-read
//! `#[serde(flatten)] extras`); `materialize` in `materialize.rs` moves
//! `crawl::sources`'s git mechanics and containment guards UNCHANGED; the
//! glob matcher in `glob.rs` is `crawl::glob` moved verbatim — one filter
//! language for both `include`/`exclude` here and every rule's own
//! `applies_to`/`exclude`.
//!
//! **Descope, stated plainly:** this packet does NOT yet cut the crawl
//! planner (`darkmux_lab::crawl::plan`) over to consume `Materialized`
//! directly — `crawl::sources::resolve` (the pre-#1959-refactor mirror of
//! `materialize` here) stays the crawl pipeline's own resolution path for
//! now, unchanged, and continues to pass its own 36+ tests. Rewiring
//! `plan.rs`'s ~700 lines of unit-collection logic to read a `Materialized`
//! workspace instead of walking `ResolvedSource` trees itself is real
//! surgery to an already deeply-tested pipeline; forcing it through in the
//! same pass as this type's introduction would trade a correctness risk
//! for a completeness checkbox. `workspace_spec::materialize` is a real,
//! independently useful, fully-tested primitive as of this packet — the
//! crawl-pipeline cutover is a follow-up, not a broken promise.

pub mod glob;
mod materialize;

pub use materialize::{
    materialize, MaterializeOptions, Materialized, MaterializedSource, SkippedFile,
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const WORKSPACE_SPEC_SCHEMA_VERSION: &str = "1.0";

/// Noise directories excluded by default when a spec names no `exclude`
/// of its own — the "defaults: everything, minus the well-known noise
/// dirs" the spec calls for. A spec that sets its OWN `exclude` (even an
/// empty array) replaces this wholesale, same as every other lenient-on-
/// read array field in this codebase (`crew::rules`'s merge semantics,
/// `CorpusManifest`'s fields) — there is no implicit union.
pub const DEFAULT_EXCLUDE: &[&str] = &[
    "**/.git/**",
    "**/node_modules/**",
    "**/target/**",
    "**/dist/**",
    "**/build/**",
    "**/.venv/**",
    "**/__pycache__/**",
    "**/.next/**",
    "**/vendor/**",
];

/// A source is exactly one of a git clone URL or a local clone path, at a
/// ref (defaulting to `main`). Identical shape to
/// `crawl::manifest::SourceSpec` — moved here as the one definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSpec {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

impl SourceSpec {
    /// The ref to resolve — `main` when the spec doesn't name one.
    pub fn resolved_ref(&self) -> &str {
        self.git_ref.as_deref().unwrap_or("main")
    }

    /// The clone origin — whichever of `git`/`path` is set. Validation
    /// guarantees exactly one is present by the time this is called.
    pub fn origin(&self) -> Option<&str> {
        self.git.as_deref().or(self.path.as_deref())
    }
}

/// A dependency edge the workspace declares: `consumer` imports `package`
/// from `library`, both named sources in the same spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeSpec {
    pub consumer: String,
    pub library: String,
    pub package: String,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// A generic mission input: named sources + include/exclude filters +
/// named edges. Deliberately carries an optional `rules` array (a list of
/// rule ids) even though `WorkspaceSpec` itself never interprets it — the
/// crawl launcher reads it as a default rule binding when its own
/// `--param rules=` is absent (see `src/crawl_launch.rs`'s input docs);
/// any other mission is free to ignore the field entirely. This is the
/// one deliberate crawl-shaped field on an otherwise fully generic type,
/// and is documented here as exactly that, not hidden in `extras`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    /// Defaults to the spec file's stem (`finhero.json` -> `"finhero"`)
    /// when absent — see [`WorkspaceSpec::load`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Root directory this workspace's mirrors/worktrees live under.
    /// `~`-expanded; defaults to `<darkmux root>/workspaces/<name>` via
    /// [`WorkspaceSpec::resolved_root`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    pub sources: Vec<SourceSpec>,
    /// `None` when the spec names no `include` key at all — distinct from
    /// `Some(vec![])`, which is a deliberate "match nothing" the spec
    /// author wrote on purpose. Only `None` falls back to the default in
    /// [`WorkspaceSpec::effective_include`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    /// Same `None`-vs-`Some(vec![])` distinction as `include` — see
    /// [`WorkspaceSpec::effective_exclude`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    #[serde(default)]
    pub edges: Vec<EdgeSpec>,
    /// Optional default rule-id binding — see the struct doc. Not
    /// interpreted by this module.
    #[serde(default)]
    pub rules: Vec<String>,
    /// Forward-compat overflow — unknown top-level keys land here and
    /// re-serialize flat (a newer spec read by an older binary).
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

impl WorkspaceSpec {
    /// Load + validate a workspace spec from disk. `name` defaults to the
    /// file's stem when the spec doesn't set one. Loud validation at load
    /// time, same discipline as `CorpusManifest::load` — a malformed spec
    /// fails here, not partway through a later `materialize` call.
    pub fn load(path: &Path) -> Result<(Self, Vec<String>)> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading workspace spec {}", path.display()))?;
        let mut spec: WorkspaceSpec = serde_json::from_str(&text)
            .with_context(|| format!("parsing workspace spec {}", path.display()))?;
        if spec.name.as_deref().map(str::trim).unwrap_or("").is_empty() {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("workspace")
                .to_string();
            spec.name = Some(stem);
        }
        let warnings = spec.validate()?;
        Ok((spec, warnings))
    }

    /// The name to use — always populated after [`WorkspaceSpec::load`];
    /// a spec built directly (a test, a synthesized one-shot spec) may
    /// still have `None`, so this falls back to `"workspace"` rather than
    /// panicking.
    pub fn effective_name(&self) -> &str {
        self.name.as_deref().unwrap_or("workspace")
    }

    /// `include` when the spec names the key at all, else `["**/*"]`
    /// (everything). An explicit `"include": []` is honored as written
    /// (matches nothing) — only the ABSENT key gets the default.
    pub fn effective_include(&self) -> Vec<String> {
        match &self.include {
            Some(v) => v.clone(),
            None => vec!["**/*".to_string()],
        }
    }

    /// `exclude` when the spec names the key at all (even an explicit
    /// `"exclude": []` is a deliberate override — see [`DEFAULT_EXCLUDE`]'s
    /// doc), else the built-in noise-dir default.
    pub fn effective_exclude(&self) -> Vec<String> {
        match &self.exclude {
            Some(v) => v.clone(),
            None => DEFAULT_EXCLUDE.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Structural validation: source id shape + case-insensitive
    /// uniqueness, a source naming neither (or both) of `git`/`path`, and
    /// an edge naming an unknown source — the same checks
    /// `CorpusManifest::validate` ran, moved here as the one definition.
    /// Returns non-fatal warnings (a `schema_version` major mismatch) on
    /// success.
    pub fn validate(&self) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        let name = self.effective_name().to_string();

        if let Some(sv) = &self.schema_version {
            if let (Some(got), Some(want)) =
                (schema_major(sv), schema_major(WORKSPACE_SPEC_SCHEMA_VERSION))
            {
                if got != want {
                    warnings.push(format!(
                        "workspace spec '{name}': schema_version '{sv}' is a different major version than this binary's spec schema ('{WORKSPACE_SPEC_SCHEMA_VERSION}') — fields may not resolve as expected"
                    ));
                }
            }
        }

        let mut seen = std::collections::BTreeSet::new();
        let mut seen_lower: std::collections::BTreeMap<String, &str> = std::collections::BTreeMap::new();
        for s in &self.sources {
            if !valid_source_id(&s.id) {
                bail!(
                    "workspace spec '{name}': source id '{}' is invalid — ids must match \
                     ^[A-Za-z0-9][A-Za-z0-9._-]*$ and must not be '.' or '..'",
                    s.id
                );
            }
            if !seen.insert(s.id.as_str()) {
                bail!("workspace spec '{name}': duplicate source id '{}'", s.id);
            }
            let lower = s.id.to_lowercase();
            if let Some(prev) = seen_lower.get(lower.as_str()) {
                bail!(
                    "workspace spec '{name}': source ids '{}' and '{}' collide case-insensitively \
                     (APFS treats these as the same path)",
                    prev,
                    s.id
                );
            }
            seen_lower.insert(lower, s.id.as_str());
            match (&s.git, &s.path) {
                (None, None) => bail!(
                    "workspace spec '{name}': source '{}' names neither `git` nor `path`",
                    s.id
                ),
                (Some(_), Some(_)) => bail!(
                    "workspace spec '{name}': source '{}' names BOTH `git` and `path` — exactly one is required",
                    s.id
                ),
                _ => {}
            }
        }
        for e in &self.edges {
            if !seen.contains(e.consumer.as_str()) {
                bail!(
                    "workspace spec '{name}': edge names unknown consumer source '{}'",
                    e.consumer
                );
            }
            if !seen.contains(e.library.as_str()) {
                bail!(
                    "workspace spec '{name}': edge names unknown library source '{}'",
                    e.library
                );
            }
        }
        Ok(warnings)
    }

    /// Resolve `root`, `~`-expanding an explicit value or defaulting to
    /// `<darkmux root>/workspaces/<name>`.
    pub fn resolved_root(&self) -> PathBuf {
        match &self.root {
            Some(r) if !r.trim().is_empty() => expand_tilde(r),
            _ => darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto)
                .root
                .join("workspaces")
                .join(self.effective_name()),
        }
    }
}

/// A source id is safe to join onto a filesystem root only if it can't
/// smuggle a path-traversal or hidden-file component through — moved
/// unchanged from `crawl::manifest::valid_source_id`.
fn valid_source_id(id: &str) -> bool {
    if id == "." || id == ".." {
        return false;
    }
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The major component of a `schema_version` string (`"1.0"` -> `"1"`).
fn schema_major(v: &str) -> Option<&str> {
    v.split('.').next().filter(|s| !s.is_empty())
}

/// Expand a leading `~` to the user's home directory — moved unchanged
/// from `crawl::manifest::expand_tilde`.
fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix('~') {
        if rest.is_empty() {
            if let Some(home) = dirs::home_dir() {
                return home;
            }
        } else if let Some(rest) = rest.strip_prefix('/') {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest);
            }
        }
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &TempDir, name: &str, contents: &str) -> PathBuf {
        let p = dir.path().join(name);
        fs::write(&p, contents).unwrap();
        p
    }

    fn minimal_spec_json() -> serde_json::Value {
        serde_json::json!({
            "schema_version": "1.0",
            "name": "example",
            "sources": [
                {"id": "lib", "git": "git@github.com:org/lib.git", "ref": "main"},
                {"id": "app", "path": "/some/local/clone", "ref": "main"}
            ],
            "edges": [{"consumer": "app", "library": "lib", "package": "@org/lib"}]
        })
    }

    #[test]
    fn loads_valid_spec() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &minimal_spec_json().to_string());
        let (s, _) = WorkspaceSpec::load(&path).unwrap();
        assert_eq!(s.name.as_deref(), Some("example"));
        assert_eq!(s.sources.len(), 2);
        assert_eq!(s.edges.len(), 1);
    }

    #[test]
    fn name_defaults_to_file_stem_when_absent() {
        let mut json = minimal_spec_json();
        json.as_object_mut().unwrap().remove("name");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "finhero.json", &json.to_string());
        let (s, _) = WorkspaceSpec::load(&path).unwrap();
        assert_eq!(s.name.as_deref(), Some("finhero"));
    }

    #[test]
    fn duplicate_source_id_rejected() {
        let mut json = minimal_spec_json();
        json["sources"][1]["id"] = serde_json::json!("lib");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let err = WorkspaceSpec::load(&path).unwrap_err();
        assert!(err.to_string().contains("duplicate source id"), "{err}");
    }

    #[test]
    fn source_with_neither_git_nor_path_rejected() {
        let mut json = minimal_spec_json();
        json["sources"][0].as_object_mut().unwrap().remove("git");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let err = WorkspaceSpec::load(&path).unwrap_err();
        assert!(err.to_string().contains("names neither"), "{err}");
    }

    #[test]
    fn source_with_both_git_and_path_rejected() {
        let mut json = minimal_spec_json();
        json["sources"][0]["path"] = serde_json::json!("/x");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let err = WorkspaceSpec::load(&path).unwrap_err();
        assert!(err.to_string().contains("names BOTH"), "{err}");
    }

    #[test]
    fn edge_with_unknown_source_rejected() {
        let mut json = minimal_spec_json();
        json["edges"][0]["consumer"] = serde_json::json!("ghost");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let err = WorkspaceSpec::load(&path).unwrap_err();
        assert!(err.to_string().contains("unknown consumer source"), "{err}");
    }

    #[test]
    fn source_id_with_path_traversal_shape_is_rejected() {
        let mut json = minimal_spec_json();
        json["sources"][0]["id"] = serde_json::json!("../../victim");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let err = WorkspaceSpec::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("../../victim"), "{msg}");
        assert!(msg.contains("invalid"), "{msg}");
    }

    #[test]
    fn source_ids_differing_only_by_case_are_rejected_together() {
        let mut json = minimal_spec_json();
        json["sources"][0]["id"] = serde_json::json!("app");
        json["sources"][1]["id"] = serde_json::json!("App");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let err = WorkspaceSpec::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("app"), "{msg}");
        assert!(msg.contains("App"), "{msg}");
        assert!(msg.contains("case-insensitively"), "{msg}");
    }

    #[test]
    fn resolved_root_expands_tilde() {
        let mut json = minimal_spec_json();
        json["root"] = serde_json::json!("~/somewhere/workspace-x");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let (s, _) = WorkspaceSpec::load(&path).unwrap();
        let root = s.resolved_root();
        assert!(!root.to_string_lossy().starts_with('~'), "{root:?}");
        assert!(root.ends_with("somewhere/workspace-x"), "{root:?}");
    }

    #[test]
    #[serial_test::serial]
    fn resolved_root_defaults_under_darkmux_root_workspaces() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe {
            std::env::set_var("DARKMUX_HOME", tmp.path());
        }
        let json = minimal_spec_json();
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let (s, _) = WorkspaceSpec::load(&path).unwrap();
        let root = s.resolved_root();
        unsafe {
            match &prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert_eq!(root, tmp.path().join("workspaces").join("example"));
    }

    #[test]
    fn extras_round_trip_forward_compat() {
        let mut json = minimal_spec_json();
        json["future_field"] = serde_json::json!("kept");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let (s, _) = WorkspaceSpec::load(&path).unwrap();
        assert_eq!(s.extras.get("future_field"), Some(&serde_json::json!("kept")));
    }

    #[test]
    fn schema_version_major_mismatch_warns_not_fails() {
        let mut json = minimal_spec_json();
        json["schema_version"] = serde_json::json!("2.0");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let (s, warnings) = WorkspaceSpec::load(&path).unwrap();
        assert_eq!(s.name.as_deref(), Some("example"));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("2.0"), "{warnings:?}");
    }

    // ── effective_include / effective_exclude defaults ──

    #[test]
    fn effective_include_defaults_to_everything_when_absent() {
        let spec: WorkspaceSpec = serde_json::from_value(minimal_spec_json()).unwrap();
        assert_eq!(spec.effective_include(), vec!["**/*".to_string()]);
    }

    #[test]
    fn effective_exclude_defaults_to_noise_dirs_when_absent() {
        let spec: WorkspaceSpec = serde_json::from_value(minimal_spec_json()).unwrap();
        let ex = spec.effective_exclude();
        assert!(ex.iter().any(|p| p.contains("node_modules")), "{ex:?}");
        assert!(ex.iter().any(|p| p.contains(".git")), "{ex:?}");
    }

    #[test]
    fn an_explicit_empty_exclude_array_overrides_the_default_wholesale() {
        let mut json = minimal_spec_json();
        json["exclude"] = serde_json::json!([]);
        let spec: WorkspaceSpec = serde_json::from_value(json).unwrap();
        assert!(spec.effective_exclude().is_empty());
    }
}
