//! Corpus manifest (#1959 packet 1) — the crawl program's top-level input.
//!
//! Lenient on read: every field but `name`/`sources` is `Option`, plus
//! `#[serde(flatten)] extras` overflow — matching the config.json /
//! workload-manifest discipline elsewhere in this crate (`darkmux_types`'s
//! `DarkmuxConfig`, `crate::workloads::types::WorkloadManifest`). Loud
//! validation happens once, at [`CorpusManifest::load`], never on a hot path.
//!
//! Pacing is deliberately NOT a manifest concept. It went through three
//! revisions during packet-1 design (a `budget.tokens_per_night` cap, then
//! `budget.tokens_per_hour`, then a manifest-level `duty_cycle`) before
//! landing on: pacing is a GLOBAL runtime setting
//! (`runtime.turn_delay_ms` in `config.json`, a separate ticket), not
//! per-corpus. `duty_cycle` and any `budget` key are both rejected loudly
//! at load — see [`CorpusManifest::validate`].

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const MANIFEST_SCHEMA_VERSION: &str = "1.0";

/// One corpus manifest: a named set of git sources, the dependency edges
/// between them, and the crawl rules to run. No pacing/budget knob lives
/// here — see the module docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusManifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    pub name: String,
    /// Root directory this corpus's mirrors/worktrees/plan live under.
    /// `~`-expanded; defaults to `<darkmux root>/crawl/<name>` via
    /// [`CorpusManifest::resolved_root`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    pub sources: Vec<SourceSpec>,
    #[serde(default)]
    pub edges: Vec<EdgeSpec>,
    #[serde(default)]
    pub rules: Vec<String>,
    /// Forward-compat overflow — unknown top-level keys land here and
    /// re-serialize flat (a newer manifest read by an older binary). Also
    /// what makes the `duty_cycle`/`budget` rejection below possible: those
    /// keys land here rather than being silently absorbed by a typed field
    /// that no longer exists.
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// A source is exactly one of a git clone URL or a local clone path, at a
/// ref (defaulting to `main`).
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
    /// The ref to resolve — `main` when the manifest doesn't name one.
    pub fn resolved_ref(&self) -> &str {
        self.git_ref.as_deref().unwrap_or("main")
    }

    /// The clone origin — whichever of `git`/`path` is set. Validation
    /// guarantees exactly one is present by the time this is called.
    pub fn origin(&self) -> Option<&str> {
        self.git.as_deref().or(self.path.as_deref())
    }
}

/// A dependency edge the corpus manifest declares: `consumer` imports
/// `package` from `library`, both named sources in the same manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeSpec {
    pub consumer: String,
    pub library: String,
    pub package: String,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// The message every pacing-shaped key (`duty_cycle`, `budget`) is
/// rejected with — one place, one wording, for every past revision of this
/// design decision.
const PACING_REJECTION: &str = "pacing is not a corpus setting: set `runtime.turn_delay_ms` in \
     config.json (darkmux config set runtime.turn_delay_ms <ms>)";

impl CorpusManifest {
    /// Load + validate a corpus manifest from disk. Validation is loud by
    /// design (#1959 packet 1 spec) — a malformed manifest fails here, not
    /// partway through a later resolve/plan pass. Non-fatal issues (a
    /// `schema_version` major bump the binary doesn't recognize yet) come
    /// back as warnings alongside the manifest, the same `(T, Vec<String>)`
    /// shape `rules::load_all`/`rules::resolve` already use — the caller
    /// (`crawl_cli::cmd_crawl_plan`) prints them with the identical
    /// `style::warn` loop it already runs for rule warnings.
    pub fn load(path: &Path) -> Result<(Self, Vec<String>)> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading corpus manifest {}", path.display()))?;
        let manifest: CorpusManifest = serde_json::from_str(&text)
            .with_context(|| format!("parsing corpus manifest {}", path.display()))?;
        let warnings = manifest.validate()?;
        Ok((manifest, warnings))
    }

    /// Structural validation: source id shape + case-insensitive
    /// uniqueness, a source naming neither (or both) of `git`/`path`, an
    /// edge naming an unknown source, and any pacing-shaped key
    /// (`duty_cycle` or `budget`) — rejected loudly rather than silently
    /// dropped, since manifests already on disk were written against
    /// earlier revisions of this design and must fail visibly, not run
    /// with a silently-ignored setting. Returns non-fatal warnings (a
    /// `schema_version` major mismatch) on success.
    ///
    /// Rule-id resolution ("a rule id that resolves nowhere") is NOT checked
    /// here — that lives in `rules::resolve`, which is the single place that
    /// knows the full rule registry (embedded + user tier). Calling it is
    /// the caller's job (`crawl_cli::plan` does, immediately after loading).
    pub fn validate(&self) -> Result<Vec<String>> {
        let mut warnings = Vec::new();

        if let Some(sv) = &self.schema_version {
            if let (Some(got), Some(want)) = (schema_major(sv), schema_major(MANIFEST_SCHEMA_VERSION)) {
                if got != want {
                    warnings.push(format!(
                        "corpus manifest '{}': schema_version '{sv}' is a different major version than this binary's manifest schema ('{MANIFEST_SCHEMA_VERSION}') — fields may not resolve as expected",
                        self.name
                    ));
                }
            }
        }

        let mut seen = std::collections::BTreeSet::new();
        // lowercase id -> the first original-case id seen for it, so an
        // APFS-colliding pair (`app`/`App`) can be named in the error even
        // though APFS itself would silently treat them as the same path.
        let mut seen_lower: std::collections::BTreeMap<String, &str> = std::collections::BTreeMap::new();
        for s in &self.sources {
            if !valid_source_id(&s.id) {
                bail!(
                    "corpus manifest '{}': source id '{}' is invalid — ids must match \
                     ^[A-Za-z0-9][A-Za-z0-9._-]*$ and must not be '.' or '..'",
                    self.name,
                    s.id
                );
            }
            if !seen.insert(s.id.as_str()) {
                bail!(
                    "corpus manifest '{}': duplicate source id '{}'",
                    self.name,
                    s.id
                );
            }
            let lower = s.id.to_lowercase();
            if let Some(prev) = seen_lower.get(lower.as_str()) {
                bail!(
                    "corpus manifest '{}': source ids '{}' and '{}' collide case-insensitively \
                     (APFS treats these as the same path)",
                    self.name,
                    prev,
                    s.id
                );
            }
            seen_lower.insert(lower, s.id.as_str());
            match (&s.git, &s.path) {
                (None, None) => bail!(
                    "corpus manifest '{}': source '{}' names neither `git` nor `path`",
                    self.name,
                    s.id
                ),
                (Some(_), Some(_)) => bail!(
                    "corpus manifest '{}': source '{}' names BOTH `git` and `path` — exactly one is required",
                    self.name,
                    s.id
                ),
                _ => {}
            }
        }
        for e in &self.edges {
            if !seen.contains(e.consumer.as_str()) {
                bail!(
                    "corpus manifest '{}': edge names unknown consumer source '{}'",
                    self.name,
                    e.consumer
                );
            }
            if !seen.contains(e.library.as_str()) {
                bail!(
                    "corpus manifest '{}': edge names unknown library source '{}'",
                    self.name,
                    e.library
                );
            }
        }
        if self.extras.contains_key("duty_cycle") || self.extras.contains_key("budget") {
            bail!("corpus manifest '{}': {PACING_REJECTION}", self.name);
        }
        Ok(warnings)
    }

    /// Resolve `root`, `~`-expanding an explicit value or defaulting to
    /// `<darkmux root>/crawl/<name>` (the same root every other darkmux
    /// subsystem resolves through — `darkmux_types::paths::resolve`).
    pub fn resolved_root(&self) -> PathBuf {
        match &self.root {
            Some(r) if !r.trim().is_empty() => expand_tilde(r),
            _ => darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto)
                .root
                .join("crawl")
                .join(&self.name),
        }
    }
}

/// A source id is safe to join onto a filesystem root (`sources::resolve`
/// does exactly that, for both the mirror and the tree path) only if it
/// can't smuggle a path-traversal or hidden-file component through: ASCII
/// alphanumeric, `.`, `_`, `-`, starting with an alphanumeric, and never
/// exactly `.` or `..` (#1959 finding 1). `sources::resolve_one` also
/// asserts containment on the resolved paths directly — this is the first,
/// cheap layer; that one is the structural backstop for any caller that
/// skips `validate()`.
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
/// `None` for an empty/malformed value — that's a separate, not-yet-scoped
/// concern from the major-mismatch warning this feeds (#1959 finding 13).
fn schema_major(v: &str) -> Option<&str> {
    v.split('.').next().filter(|s| !s.is_empty())
}

/// Expand a leading `~` to the user's home directory; pass through
/// everything else unchanged. `darkmux_types::paths::expand_tilde` does the
/// same thing but is `pub(crate)` to that crate — this is the small
/// inline mirror the "don't add dependencies casually" convention calls
/// for rather than widening that crate's public surface for one caller.
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

    fn minimal_manifest_json() -> serde_json::Value {
        serde_json::json!({
            "schema_version": "1.0",
            "name": "example",
            "sources": [
                {"id": "lib", "git": "git@github.com:org/lib.git", "ref": "main"},
                {"id": "app", "path": "/some/local/clone", "ref": "main"}
            ],
            "edges": [{"consumer": "app", "library": "lib", "package": "@org/lib"}],
            "rules": ["swallowed-error"]
        })
    }

    #[test]
    fn loads_valid_manifest() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "corpus.json", &minimal_manifest_json().to_string());
        let (m, _) = CorpusManifest::load(&path).unwrap();
        assert_eq!(m.name, "example");
        assert_eq!(m.sources.len(), 2);
        assert_eq!(m.edges.len(), 1);
    }

    #[test]
    fn duplicate_source_id_rejected() {
        let mut json = minimal_manifest_json();
        json["sources"][1]["id"] = serde_json::json!("lib");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "corpus.json", &json.to_string());
        let err = CorpusManifest::load(&path).unwrap_err();
        assert!(err.to_string().contains("duplicate source id"), "{err}");
    }

    #[test]
    fn source_with_neither_git_nor_path_rejected() {
        let mut json = minimal_manifest_json();
        json["sources"][0].as_object_mut().unwrap().remove("git");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "corpus.json", &json.to_string());
        let err = CorpusManifest::load(&path).unwrap_err();
        assert!(err.to_string().contains("names neither"), "{err}");
    }

    #[test]
    fn source_with_both_git_and_path_rejected() {
        let mut json = minimal_manifest_json();
        json["sources"][0]["path"] = serde_json::json!("/x");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "corpus.json", &json.to_string());
        let err = CorpusManifest::load(&path).unwrap_err();
        assert!(err.to_string().contains("names BOTH"), "{err}");
    }

    #[test]
    fn edge_with_unknown_source_rejected() {
        let mut json = minimal_manifest_json();
        json["edges"][0]["consumer"] = serde_json::json!("ghost");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "corpus.json", &json.to_string());
        let err = CorpusManifest::load(&path).unwrap_err();
        assert!(err.to_string().contains("unknown consumer source"), "{err}");
    }

    #[test]
    fn duty_cycle_key_rejected_pointing_at_runtime_setting() {
        let mut json = minimal_manifest_json();
        json["duty_cycle"] = serde_json::json!(0.35);
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "corpus.json", &json.to_string());
        let err = CorpusManifest::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("pacing is not a corpus setting"), "{msg}");
        assert!(msg.contains("runtime.turn_delay_ms"), "{msg}");
    }

    #[test]
    fn any_budget_key_rejected_pointing_at_runtime_setting() {
        for budget_value in [
            serde_json::json!({"tokens_per_night": 20000000}),
            serde_json::json!({"tokens_per_hour": 500000}),
            serde_json::json!({}),
        ] {
            let mut json = minimal_manifest_json();
            json["budget"] = budget_value.clone();
            let dir = TempDir::new().unwrap();
            let path = write(&dir, "corpus.json", &json.to_string());
            let err = CorpusManifest::load(&path).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("pacing is not a corpus setting"), "{msg} (value={budget_value})");
            assert!(msg.contains("runtime.turn_delay_ms"), "{msg}");
        }
    }

    #[test]
    fn resolved_root_expands_tilde() {
        let mut json = minimal_manifest_json();
        json["root"] = serde_json::json!("~/somewhere/crawl-x");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "corpus.json", &json.to_string());
        let (m, _) = CorpusManifest::load(&path).unwrap();
        let root = m.resolved_root();
        assert!(!root.to_string_lossy().starts_with('~'), "{root:?}");
        assert!(root.ends_with("somewhere/crawl-x"), "{root:?}");
    }

    #[test]
    #[serial_test::serial]
    fn resolved_root_defaults_under_darkmux_root() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe {
            std::env::set_var("DARKMUX_HOME", tmp.path());
        }
        let json = minimal_manifest_json();
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "corpus.json", &json.to_string());
        let (m, _) = CorpusManifest::load(&path).unwrap();
        let root = m.resolved_root();
        unsafe {
            match &prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert_eq!(root, tmp.path().join("crawl").join("example"));
    }

    #[test]
    fn extras_round_trip_forward_compat() {
        let mut json = minimal_manifest_json();
        json["future_field"] = serde_json::json!("kept");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "corpus.json", &json.to_string());
        let (m, _) = CorpusManifest::load(&path).unwrap();
        assert_eq!(
            m.extras.get("future_field"),
            Some(&serde_json::json!("kept"))
        );
    }

    // ── #1959 finding 1: source id shape + case-insensitive uniqueness ──

    #[test]
    fn source_id_with_path_traversal_shape_is_rejected() {
        let mut json = minimal_manifest_json();
        json["sources"][0]["id"] = serde_json::json!("../../victim");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "corpus.json", &json.to_string());
        let err = CorpusManifest::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("../../victim"), "{msg}");
        assert!(msg.contains("invalid"), "{msg}");
    }

    #[test]
    fn source_ids_differing_only_by_case_are_rejected_together() {
        let mut json = minimal_manifest_json();
        json["sources"][0]["id"] = serde_json::json!("app");
        json["sources"][1]["id"] = serde_json::json!("App");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "corpus.json", &json.to_string());
        let err = CorpusManifest::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("app"), "{msg}");
        assert!(msg.contains("App"), "{msg}");
        assert!(msg.contains("case-insensitively"), "{msg}");
    }

    // ── #1959 finding 13: schema_version major mismatch warns, doesn't fail ──

    #[test]
    fn schema_version_major_mismatch_warns_not_fails() {
        let mut json = minimal_manifest_json();
        json["schema_version"] = serde_json::json!("2.0");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "corpus.json", &json.to_string());
        let (m, warnings) = CorpusManifest::load(&path).unwrap();
        assert_eq!(m.name, "example");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("2.0"), "{warnings:?}");
        assert!(warnings[0].contains(MANIFEST_SCHEMA_VERSION), "{warnings:?}");
    }

    #[test]
    fn schema_version_matching_major_is_silent() {
        let json = minimal_manifest_json(); // schema_version: "1.0"
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "corpus.json", &json.to_string());
        let (_, warnings) = CorpusManifest::load(&path).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}
