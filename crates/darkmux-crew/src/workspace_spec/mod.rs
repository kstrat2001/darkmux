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
    materialize, MaterializeOptions, Materialized, MaterializedSource, SkippedFile, WorkspaceLock,
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

/// True iff `s` carries a URL scheme (`scheme://...`) that marks it as a
/// REMOTE address rather than a filesystem path. Deliberately permissive
/// about which scheme — this only needs to distinguish "this is a URL"
/// from "this is a path", not validate which schemes `git` itself accepts
/// for a given transport.
fn has_url_scheme(s: &str) -> bool {
    match s.find("://") {
        Some(idx) if idx > 0 => {
            s[..idx].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        }
        _ => false,
    }
}

/// True iff `s` is git's scp-like remote shorthand (`[user@]host:path`,
/// the same syntax `git@github.com:org/repo.git` uses) rather than a
/// local filesystem path that merely happens to contain a colon.
/// Requires an `@` or a `.` before the first colon so an ordinary
/// relative path never matches by accident — this crate's own path
/// handling is POSIX-only throughout (see this module's `Path`/`PathBuf`
/// usage), so a Windows drive-letter collision (`C:\...`) isn't a case
/// this needs to guard against.
fn has_scp_like_host(s: &str) -> bool {
    let Some(colon) = s.find(':') else { return false };
    if s[colon..].starts_with("://") {
        return false; // already a URL — `has_url_scheme` owns that case
    }
    let host = &s[..colon];
    !host.is_empty() && (host.contains('@') || host.contains('.'))
}

/// True iff `origin` (a source's `git` OR `path` field value) is a
/// filesystem path that would resolve against darkmux's own AMBIENT
/// working directory if handed to `git`/a shell verbatim — i.e. it names
/// neither a URL nor git's scp-like remote shorthand, and is not already
/// an absolute path.
///
/// (#2612 review MUST-FIX 1) The check this predicate backs used to live
/// on the `path` FIELD alone — but the value actually handed to `git
/// clone` is [`SourceSpec::origin`], which PREFERS `git` over `path`, and
/// a `git:` value is only a remote URL by convention, never by type: the
/// same field also accepts a bare local filesystem path (`git clone
/// ../sibling-repo` works from the right directory, which is exactly how
/// an operator ends up writing one). Refusing/absolutizing only `path`
/// left a relative `git:` origin reaching the same unguarded `git clone`
/// call with no directory set — the identical #2577 hazard, one field
/// over. See `materialize::resolve_one`'s doc for the refusal and
/// [`WorkspaceSpec::load`]'s doc for the absolutization, both of which
/// now apply this predicate to EITHER field rather than special-casing
/// `path`.
pub(crate) fn origin_is_relative_local(origin: &str) -> bool {
    !has_url_scheme(origin) && !has_scp_like_host(origin) && !Path::new(origin).is_absolute()
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
/// `--param rules=` is absent (see `crawl.json`'s own `rules` input);
/// any other mission is free to ignore the field entirely. This is the
/// one deliberate crawl-shaped field on an otherwise fully generic type,
/// and is documented here as exactly that, not hidden in `extras`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    /// Defaults to the spec file's stem (`acme.json` -> `"acme"`)
    /// when absent — see [`WorkspaceSpec::load`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Root directory this workspace's mirrors/worktrees live under.
    /// `~`-expanded; defaults to `<darkmux root>/workspaces/<name>` via
    /// [`WorkspaceSpec::resolved_root`] when absent. **An explicit `root:`
    /// here is an unvalidated operator override** — expanded verbatim,
    /// with no containment check — while the DEFAULT arm's `<name>` is
    /// guarded twice: by the character class in
    /// [`WorkspaceSpec::validate`] at load time, and structurally at the
    /// join itself in [`WorkspaceSpec::resolved_root`] (#2455). The
    /// asymmetry is deliberate: `name` is a spec-internal identifier the
    /// operator rarely thinks about as a path at all, so it gets the same
    /// structural guard as a source `id`; `root:` is the operator
    /// explicitly naming a filesystem location, and second-guessing an
    /// explicit path an operator wrote on purpose is not this type's job.
    ///
    /// The bound on that override, stated so it is not mistaken for an
    /// oversight: `root:` is only ever read from a spec FILE the operator
    /// wrote (or from a caller in this repo that passes a path it chose
    /// itself). It is not derived from any remote-controlled value — the
    /// one production caller that builds a spec from external input,
    /// `darkmux_lab::crawl::plan_sites_step::derive_workspace_spec`,
    /// leaves it `None` and always takes the guarded default arm.
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
        // (#2577 review) A source's clone origin is used VERBATIM by
        // `materialize` (`workspace_spec::materialize::resolve_one`,
        // which now REFUSES a relative local one outright — see that
        // function's own doc for why). Absolutize a relative local
        // origin here, against the spec FILE's own directory — a named,
        // stable base computed once, now, from where this file actually
        // lives, never from the process's ambient working directory —
        // so an operator writing `"path": "../sibling-repo"` in a spec
        // gets "relative to this spec file" rather than a load-time
        // refusal. `path.canonicalize()` resolves this file's own
        // location (and any symlinks in it) before taking its parent, so
        // the base is itself independent of whatever the process cwd
        // happens to be when `load()` runs; only the fallback
        // (canonicalize failing, e.g. the spec file vanished between the
        // read above and here) leaves the source value exactly as its
        // author wrote it, for `resolve_one`'s guard to refuse.
        //
        // (#2612 review MUST-FIX 1) Applied to BOTH `path` and `git` —
        // `origin_is_relative_local` treats them identically, since a
        // relative `git:` value is exactly as local-filesystem-shaped as
        // a relative `path:` value (see that predicate's own doc); it
        // just never used to be absolutized here, which left it reaching
        // `resolve_one`'s clone call with no directory set.
        //
        // (#2612 review Also-fix 2) A leading `~` is deliberately
        // EXCLUDED from this join even though `Path::new("~/foo").
        // is_absolute()` is false: joining a spec directory onto a
        // shell-only home-directory shorthand darkmux never expands
        // would produce an absolute-LOOKING but nonsense path
        // (`<spec_dir>/~/foo`) that silently PASSES `resolve_one`'s
        // absoluteness check instead of being refused with the source
        // named — trading a clear refusal for a confusing "not found"
        // from `git` itself. This is a regression fix, not a feature:
        // before this absolutization existed at all, a `~`-prefixed
        // origin was never absolute either, so it always reached
        // `resolve_one`'s refusal directly. Leaving it untouched here
        // restores that path — it still satisfies
        // `origin_is_relative_local` (no scheme, no scp-like host, not
        // absolute) and reaches the same named refusal.
        let spec_dir = path.canonicalize().ok().and_then(|p| p.parent().map(Path::to_path_buf));
        if let Some(spec_dir) = spec_dir {
            for source in &mut spec.sources {
                if let Some(p) = &source.path {
                    if origin_is_relative_local(p) && !p.starts_with('~') {
                        source.path = Some(spec_dir.join(Path::new(p)).to_string_lossy().into_owned());
                    }
                }
                if let Some(g) = &source.git {
                    if origin_is_relative_local(g) && !g.starts_with('~') {
                        source.git = Some(spec_dir.join(Path::new(g)).to_string_lossy().into_owned());
                    }
                }
            }
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

        // (#2455) `resolved_root()` joins `effective_name()` onto
        // `<darkmux root>/workspaces/` — the identical shape as a source
        // `id` joined onto that source's own materialized root just below,
        // and just as unsafe to leave unvalidated: `Path::join` REPLACES
        // the accumulated path outright when the joined component is
        // absolute, and never strips a `..` segment. Reuses
        // `valid_source_id` rather than a second predicate (see that
        // function's own doc) — checked here, at the one place an
        // OPERATOR-AUTHORED spec is read, so an invalid name is refused
        // with an error naming their config rather than a structural
        // refusal from three layers down.
        //
        // This check is the ERGONOMIC half of the pair, not the load-
        // bearing one. `validate()` runs only from `load()`, and a
        // `WorkspaceSpec` built as a struct literal never reaches it
        // (`darkmux_lab::crawl::plan_sites_step::derive_workspace_spec`
        // does exactly that, then materializes). The containment that
        // cannot be skipped lives at the join, in `resolved_root()` —
        // see its doc for why the two guards use different classes.
        // Neither covers `root:`: an explicit override takes a different,
        // unvalidated arm of `resolved_root()` entirely.
        if !valid_source_id(&name) {
            // The remedy names BOTH origins on purpose (#2455 review).
            // `load()` substitutes the spec FILE's stem when the spec sets
            // no `name` of its own, so the offending value is very often
            // one the operator never typed into the file at all — a spec
            // at `q1 corpus.json` produces exactly this error, and
            // "rename it in the spec" alone would send them looking for a
            // `name` key that isn't there.
            bail!(
                "workspace spec '{name}': the workspace name is invalid — it becomes a single \
                 path component under `<darkmux root>/workspaces/`, so it must match \
                 ^[A-Za-z0-9][A-Za-z0-9._-]*$ (start with a letter or digit; then letters, \
                 digits, `.`, `_` or `-` only — no `/`, no spaces, no leading dot). Fix it in \
                 one of three ways: set a valid `\"name\"` in the spec; or, if the spec sets no \
                 `name` at all, rename the spec FILE (the name defaults to its stem); or set an \
                 explicit `root:` to place this workspace at a path of your choosing"
            );
        }

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
    ///
    /// **The default arm CONTAINS `<name>` here, at the join itself
    /// (#2455 review)** — not only in [`WorkspaceSpec::validate`]. The
    /// two guards are deliberate defense in depth, and this is the one
    /// that cannot be skipped:
    ///
    /// - `validate()`'s check is the operator-facing one. It runs inside
    ///   [`WorkspaceSpec::load`], applies the full [`valid_source_id`]
    ///   character class, and refuses a bad spec at the moment the
    ///   operator's file is read, naming the config.
    /// - This check is STRUCTURAL. `WorkspaceSpec` is a plain `pub` struct
    ///   with `pub` fields, and real production code builds one as a
    ///   literal without ever calling `load()` (see
    ///   `darkmux_lab::crawl::plan_sites_step::derive_workspace_spec`,
    ///   which hands its spec straight to
    ///   [`materialize`](super::materialize)). A guard that only lives in
    ///   `validate()` is therefore worth exactly as much as every caller
    ///   remembering to call it — the arrangement `darkmux_types::paths`
    ///   already rejected in writing when it made the lab-root bypass
    ///   unrepresentable rather than merely discouraged (#1882). So the
    ///   containment lives where the dangerous `join` lives, and every
    ///   caller of this function gets it whether it went through `load()`
    ///   or not.
    ///
    /// The two checks are deliberately DIFFERENT classes, and the
    /// difference is load-bearing. This one asks only "is it a single,
    /// non-escaping path component" — the exact question `Path::join`
    /// makes dangerous, and the same predicate
    /// `materialize`'s own `contained_child` already applies to a source
    /// `id`. It does NOT apply the stricter charset, because a
    /// STRUCTURALLY safe name that the charset would reject is genuinely
    /// reachable: `derive_workspace_spec` names the workspace after the
    /// GitHub repo, and `owner/.github` is a real and common repository —
    /// `.github` is a perfectly safe single component that
    /// [`valid_source_id`] rejects for its leading dot. Applying the
    /// charset here would refuse to review that repo at all. The charset
    /// stays where it belongs: on operator-authored specs, at load time.
    ///
    /// The explicit-`root:` arm is an unvalidated operator override and is
    /// NOT contained — see that field's own doc for why.
    pub fn resolved_root(&self) -> Result<PathBuf> {
        if let Some(r) = &self.root {
            if !r.trim().is_empty() {
                return Ok(expand_tilde(r));
            }
        }
        let name = self.effective_name();
        if !single_path_component(name) {
            bail!(
                "workspace '{name}': the workspace name must be a single non-escaping path \
                 component — it is joined onto `<darkmux root>/workspaces/`, and `Path::join` \
                 REPLACES the whole accumulated path when the joined value is absolute (and \
                 never strips a `..` segment) — rename the workspace, or set an explicit \
                 `root:` if you meant to place it somewhere specific"
            );
        }
        Ok(darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto)
            .root
            .join("workspaces")
            .join(name))
    }
}

/// Is `name` a single path component that cannot escape the root it is
/// joined onto? The same predicate `materialize`'s `contained_child`
/// applies to a source `id` (minus the `canonicalize`, which needs the
/// root to already exist on disk — `<darkmux root>/workspaces/` may not).
///
/// Rejects, by construction rather than by blacklist: an absolute path
/// (leading `Component::RootDir`), any `..` (`Component::ParentDir`), a
/// bare or embedded `.` (`Component::CurDir`), an embedded or trailing
/// separator (more than one component), and the empty string (no
/// components at all).
fn single_path_component(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(std::path::Component::Normal(_))) && components.next().is_none()
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
        let path = write(&dir, "acme.json", &json.to_string());
        let (s, _) = WorkspaceSpec::load(&path).unwrap();
        assert_eq!(s.name.as_deref(), Some("acme"));
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

    /// (#2577 review) `load()` absolutizes a relative `path:` against the
    /// spec FILE's own directory — proven independent of the process's
    /// ambient working directory by loading the SAME spec from an
    /// unrelated ambient cwd that does not itself contain the source
    /// directory at all. Before this fix, `origin()`'s value would have
    /// stayed the literal relative string, and `resolve_one` (now refusing
    /// it outright — see that function's own doc) would have resolved it
    /// against whatever the ambient directory happened to be.
    #[test]
    #[serial_test::serial]
    fn load_absolutizes_a_relative_path_source_against_the_spec_files_own_directory() {
        let spec_dir = TempDir::new().unwrap();
        std::fs::create_dir(spec_dir.path().join("sibling-repo")).unwrap();
        let mut json = minimal_spec_json();
        json["sources"][1]["path"] = serde_json::json!("sibling-repo");
        let spec_path = write(&spec_dir, "workspace.json", &json.to_string());

        // An ambient cwd elsewhere entirely, which does NOT contain
        // "sibling-repo" — if resolution ever fell back to ambient cwd,
        // the join below would be wrong.
        let elsewhere = TempDir::new().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(elsewhere.path()).unwrap();
        let result = WorkspaceSpec::load(&spec_path);
        std::env::set_current_dir(&prev).unwrap();

        let (spec, _) = result.unwrap();
        let app = spec.sources.iter().find(|s| s.id == "app").unwrap();
        let resolved = PathBuf::from(app.path.as_deref().unwrap());
        assert!(resolved.is_absolute(), "{resolved:?}");
        assert_eq!(
            resolved.canonicalize().unwrap(),
            spec_dir.path().join("sibling-repo").canonicalize().unwrap(),
            "a relative `path:` must resolve against the SPEC FILE's own directory, not the \
             ambient cwd it happened to load from"
        );
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
        let root = s.resolved_root().unwrap();
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
        let root = s.resolved_root().unwrap();
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

    /// (#1959) Moved from the retired `crawl::manifest`'s own test of the
    /// same name — no twin existed here. The other half of the mismatch
    /// test above: a MATCHING major produces no warning at all.
    #[test]
    fn schema_version_matching_major_is_silent() {
        let json = minimal_spec_json(); // schema_version: "1.0"
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let (_, warnings) = WorkspaceSpec::load(&path).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
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

    // ── name validation (#2455) ──
    //
    // `effective_name()` used to be free-form: `validate()` never checked
    // `name`, so `resolved_root()`'s `<darkmux root>/workspaces/<name>`
    // join could be handed an absolute path (which REPLACES the
    // accumulated path per `Path::join`'s documented behavior) or a `..`
    // segment (which is never rejected by `join`). Confirmed end-to-end
    // with a scratch probe before this fix landed: a spec named an
    // absolute path under a second tempdir caused `materialize()` to
    // `git clone --bare` and check out a worktree there, while the
    // intended `<DARKMUX_HOME>/workspaces/` directory was never created
    // at all. These tests pin the fix at its source, `validate()`.

    #[test]
    fn absolute_name_is_rejected() {
        let mut json = minimal_spec_json();
        json["name"] = serde_json::json!("/tmp/anywhere");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let err = WorkspaceSpec::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("/tmp/anywhere"), "{msg}");
        assert!(msg.contains("name"), "{msg}");
    }

    #[test]
    fn dotdot_name_is_rejected() {
        let mut json = minimal_spec_json();
        json["name"] = serde_json::json!("..");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let err = WorkspaceSpec::load(&path).unwrap_err();
        assert!(err.to_string().contains("name"), "{err}");
    }

    #[test]
    fn nested_dotdot_name_that_only_escapes_after_joining_is_rejected() {
        let mut json = minimal_spec_json();
        json["name"] = serde_json::json!("../../../../etc/passwd");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let err = WorkspaceSpec::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("../../../../etc/passwd"), "{msg}");
        assert!(msg.contains("name"), "{msg}");
    }

    #[test]
    fn bare_separator_name_is_rejected() {
        let mut json = minimal_spec_json();
        json["name"] = serde_json::json!("a/b");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let err = WorkspaceSpec::load(&path).unwrap_err();
        assert!(err.to_string().contains("name"), "{err}");
    }

    #[test]
    #[serial_test::serial]
    fn ordinary_name_still_resolves_exactly_where_it_did_before() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe {
            std::env::set_var("DARKMUX_HOME", tmp.path());
        }
        let mut json = minimal_spec_json();
        json["name"] = serde_json::json!("perfectly-ordinary_name.v2");
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "workspace.json", &json.to_string());
        let loaded = WorkspaceSpec::load(&path);
        let root = loaded.as_ref().ok().map(|(s, _)| s.resolved_root().unwrap());
        unsafe {
            match &prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        loaded.unwrap();
        assert_eq!(
            root.unwrap(),
            tmp.path().join("workspaces").join("perfectly-ordinary_name.v2")
        );
    }
    // ── #2455 review: the two guards, each pinned as its own class ──
    //
    // The load-time check (`validate`) and the join-time check
    // (`resolved_root`) are DIFFERENT predicates on purpose, so each gets
    // its own table. Pinning only the four cases the fix was written for
    // would leave the class itself free to drift — the exact gap #2157's
    // review found on this codebase's other copy of this guard (there,
    // since closed: see `thermal_governor`'s own corpus).

    /// The CHARSET class. Driven through `validate()` DIRECTLY rather
    /// than through `load()`, because `load()` substitutes the spec
    /// file's stem for an absent/blank `name` — so `""` and `"   "` can
    /// never reach `validate` via that path, and testing the class
    /// through `load` would silently drop them (see
    /// `a_blank_name_becomes_the_file_stem_before_validate_sees_it`).
    #[test]
    fn validate_refuses_the_whole_bad_name_charset() {
        let cases: &[(&str, &str)] = &[
            ("", "empty"),
            ("   ", "whitespace only"),
            (".", "bare current-dir"),
            ("..", "bare parent-dir"),
            ("...", "leading dot"),
            (".hidden", "leading dot (hidden file)"),
            ("-flag", "leading hyphen (reads as a CLI flag downstream)"),
            ("_scratch", "leading underscore"),
            ("a/b", "embedded separator"),
            ("a\\b", "embedded backslash"),
            ("/abs", "absolute"),
            ("../escape", "traversal"),
            ("a/../../b", "traversal that only escapes after joining"),
            ("q1 corpus", "embedded space"),
            ("caf\u{e9}", "non-ASCII"),
            ("nam\u{435}", "Cyrillic homoglyph that renders as ASCII"),
            ("a\u{2044}b", "FRACTION SLASH — renders like a separator"),
            ("a\nb", "embedded newline"),
            ("a\0b", "embedded NUL"),
        ];
        for (name, why) in cases {
            let mut json = minimal_spec_json();
            json["name"] = serde_json::json!(name);
            let spec: WorkspaceSpec = serde_json::from_value(json).unwrap();
            let err = spec
                .validate()
                .err()
                .unwrap_or_else(|| panic!("name {name:?} ({why}) must be refused"));
            assert!(
                err.to_string().contains("the workspace name is invalid"),
                "name {name:?} ({why}) was refused for the WRONG reason: {err}"
            );
        }
    }

    /// The substitution the table above works around, pinned in its own
    /// right: a spec with no usable `name` takes the FILE's stem, and
    /// that stem is then subject to the same charset. A spec at
    /// `q1 corpus.json` is refused even though its author never typed a
    /// `name` — which is exactly why the error text names the file as one
    /// of the three remedies.
    #[test]
    fn a_blank_name_becomes_the_file_stem_before_validate_sees_it() {
        let mut json = minimal_spec_json();
        json["name"] = serde_json::json!("   ");
        let dir = TempDir::new().unwrap();

        let ok = write(&dir, "perfectly-fine.json", &json.to_string());
        let (spec, _) = WorkspaceSpec::load(&ok).unwrap();
        assert_eq!(spec.effective_name(), "perfectly-fine");

        let bad = write(&dir, "q1 corpus.json", &json.to_string());
        let err = WorkspaceSpec::load(&bad).unwrap_err().to_string();
        assert!(err.contains("q1 corpus"), "{err}");
        assert!(
            err.contains("rename the spec FILE"),
            "the remedy must cover the file-stem origin, since no `name` was written: {err}"
        );
    }

    /// The direction an over-eager charset would break: names an operator
    /// realistically writes must still load.
    #[test]
    fn validate_still_accepts_realistic_names() {
        for name in ["a", "9", "acme", "q1-corpus", "crawl_v2.1-final", "darkmux-self", "Repo.Name"] {
            let mut json = minimal_spec_json();
            json["name"] = serde_json::json!(name);
            let dir = TempDir::new().unwrap();
            let path = write(&dir, "workspace.json", &json.to_string());
            WorkspaceSpec::load(&path).unwrap_or_else(|e| panic!("name {name:?} must load: {e}"));
        }
    }

    /// The CONTAINMENT class, at the join — reached WITHOUT `load()`, the
    /// way `crawl::plan_sites_step::derive_workspace_spec` reaches it.
    /// Every value here must make `resolved_root()` refuse rather than
    /// return a path outside `<darkmux root>/workspaces/`.
    #[test]
    fn resolved_root_refuses_every_name_that_could_escape_the_join() {
        for name in [
            "",
            "   /..",
            ".",
            "..",
            "a/b",
            "a//b",
            "/abs",
            "/",
            "../escape",
            "a/../../b",
            "./a",
        ] {
            let spec = WorkspaceSpec {
                schema_version: None,
                name: Some(name.to_string()),
                root: None,
                sources: Vec::new(),
                include: None,
                exclude: None,
                edges: Vec::new(),
                rules: Vec::new(),
                extras: BTreeMap::new(),
            };
            let err = spec
                .resolved_root()
                .err()
                .unwrap_or_else(|| panic!("name {name:?} must be refused at the join"));
            assert!(
                err.to_string().contains("single non-escaping path component"),
                "name {name:?} refused for the wrong reason: {err}"
            );
        }
    }

    /// The near-miss worth stating so nobody "fixes" it: a TRAILING
    /// separator is not an escape. `Path::components()` normalizes it
    /// away, so `"a/"` is still exactly one component and still lands
    /// inside `workspaces/`. The load-time charset rejects it anyway (it
    /// contains a `/`), which is the right place for a tidiness rule —
    /// the join-time guard only owns containment.
    #[test]
    #[serial_test::serial]
    fn a_trailing_separator_is_normalized_away_not_an_escape() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe {
            std::env::set_var("DARKMUX_HOME", tmp.path());
        }
        let spec = WorkspaceSpec {
            schema_version: None,
            name: Some("a/".to_string()),
            root: None,
            sources: Vec::new(),
            include: None,
            exclude: None,
            edges: Vec::new(),
            rules: Vec::new(),
            extras: BTreeMap::new(),
        };
        let root = spec.resolved_root();
        unsafe {
            match &prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert_eq!(root.unwrap(), tmp.path().join("workspaces").join("a"));
        assert!(!valid_source_id("a/"), "the load-time charset still refuses it");
    }

    /// The join-time guard is deliberately the CONTAINMENT class, not the
    /// charset — and this is the case that proves the difference matters
    /// rather than being an accident of implementation.
    /// `derive_workspace_spec` names a workspace after the GitHub repo it
    /// is reviewing, and `owner/.github` is a real, common repository. A
    /// leading dot is structurally harmless (still one component, still
    /// contained); tightening this guard to the charset would refuse to
    /// review that repo at all.
    #[test]
    #[serial_test::serial]
    fn resolved_root_accepts_a_structurally_safe_name_the_charset_would_reject() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe {
            std::env::set_var("DARKMUX_HOME", tmp.path());
        }
        let spec = WorkspaceSpec {
            schema_version: None,
            name: Some(".github".to_string()),
            root: None,
            sources: Vec::new(),
            include: None,
            exclude: None,
            edges: Vec::new(),
            rules: Vec::new(),
            extras: BTreeMap::new(),
        };
        let root = spec.resolved_root();
        unsafe {
            match &prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert_eq!(root.unwrap(), tmp.path().join("workspaces").join(".github"));
        // ...and the charset guard genuinely WOULD have rejected it, so
        // this test fails loudly if someone later "unifies" the two.
        assert!(!valid_source_id(".github"));
    }

    /// An explicit `root:` stays the unvalidated operator override the
    /// field's doc promises — the join-time guard must not silently start
    /// policing a path the operator wrote on purpose.
    #[test]
    fn resolved_root_leaves_an_explicit_root_override_alone() {
        let spec = WorkspaceSpec {
            schema_version: None,
            name: Some("..".to_string()),
            root: Some("/some/operator/chosen/place".to_string()),
            sources: Vec::new(),
            include: None,
            exclude: None,
            edges: Vec::new(),
            rules: Vec::new(),
            extras: BTreeMap::new(),
        };
        assert_eq!(spec.resolved_root().unwrap(), PathBuf::from("/some/operator/chosen/place"));
    }

}
