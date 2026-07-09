//! `FileSource` — where the bundler reads file content from. Two
//! fidelity tiers (#1222 packet 3 mandate, no Python precedent — the
//! reference only ever had a local worktree):
//!
//! - **`Worktree`**: full fidelity. A real checkout on disk; `candidate_files`
//!   walks the whole tree (port of `iter_ts_files` + `TS_EXCLUDE_DIRS`), so
//!   the repo-wide function index, caller grep, and sibling stem-overlap all
//!   see every TS/TSX file in the repo.
//! - **`GithubApi`**: bounded, no checkout. `candidate_files` starts from the
//!   diff's changed files and does ONE hop of relative-import resolution
//!   (`import ... from './x'` -> `x.ts` / `x.tsx` / `x/index.ts` /
//!   `x/index.tsx`), capped at `MAX_API_FILES` total fetches. A symbol this
//!   source can't resolve lands in the bundle's `manifest` instead of being
//!   silently dropped or wrongly assumed absent.
//!
//! The `gh api` shell-out is the one genuinely impure edge; every other
//! piece here (diff-derived candidate paths, import parsing, relative-path
//! resolution) is a pure function, unit-tested directly with no network
//! access — the "seam" the packet brief asks for.

use super::diff;
use super::scan;
use anyhow::{Context, Result};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Bound on total GitHub API content fetches `GithubApi::candidate_files`
/// will perform for one bundling run (the diff's own changed files plus
/// one hop of resolved relative imports).
pub const MAX_API_FILES: usize = 30;

pub enum FileSource {
    Worktree(PathBuf),
    GithubApi {
        repo: String,
        head_sha: String,
        /// In-memory per-path fetch cache — `Some(content)` on a hit,
        /// `None` for a confirmed-missing path (a 404 is cached too, so a
        /// re-probed import candidate doesn't re-fetch).
        cache: RefCell<HashMap<String, Option<String>>>,
    },
}

impl FileSource {
    pub fn worktree(root: impl Into<PathBuf>) -> Self {
        FileSource::Worktree(root.into())
    }

    pub fn github_api(repo: impl Into<String>, head_sha: impl Into<String>) -> Self {
        FileSource::GithubApi {
            repo: repo.into(),
            head_sha: head_sha.into(),
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// Read `path` (repo-relative, forward-slash) as UTF-8 text.
    /// `Ok(None)` means "doesn't exist / unreadable" — never an error, so
    /// callers can treat a missing file exactly like the Python
    /// reference's `except OSError: continue`.
    pub fn read_file(&self, path: &str) -> Result<Option<String>> {
        match self {
            FileSource::Worktree(root) => Ok(read_worktree_file(root, path)),
            FileSource::GithubApi { repo, head_sha, cache } => {
                if let Some(hit) = cache.borrow().get(path) {
                    return Ok(hit.clone());
                }
                let fetched = gh_api_fetch(repo, head_sha, path)?;
                cache.borrow_mut().insert(path.to_string(), fetched.clone());
                Ok(fetched)
            }
        }
    }

    /// The universe of files this source will read for repo-index
    /// building, caller-grep, and sibling search.
    pub fn candidate_files(&self, diff_text: &str) -> Result<Vec<String>> {
        match self {
            FileSource::Worktree(root) => Ok(iter_ts_files(root)),
            FileSource::GithubApi { .. } => self.github_candidate_files(diff_text),
        }
    }

    fn github_candidate_files(&self, diff_text: &str) -> Result<Vec<String>> {
        let mut result: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut fetch_budget = MAX_API_FILES;

        let changed = changed_files_from_diff(diff_text);
        let mut frontier: Vec<String> = Vec::new();
        for path in changed {
            if seen.insert(path.clone()) {
                result.push(path.clone());
                frontier.push(path);
            }
        }

        // One hop: for each changed file (that we can still afford to
        // fetch), read its content, extract relative import specifiers,
        // resolve each to candidate paths, and confirm existence with a
        // bounded probe fetch.
        for from_path in frontier {
            if fetch_budget == 0 {
                break;
            }
            let Some(content) = self.read_file(&from_path)? else {
                fetch_budget = fetch_budget.saturating_sub(1);
                continue;
            };
            fetch_budget = fetch_budget.saturating_sub(1);
            for spec in parse_relative_imports(&content) {
                if fetch_budget == 0 {
                    break;
                }
                for candidate in resolve_relative_import_candidates(&from_path, &spec) {
                    if fetch_budget == 0 {
                        break;
                    }
                    if seen.contains(&candidate) {
                        continue;
                    }
                    let found = self.read_file(&candidate)?;
                    fetch_budget = fetch_budget.saturating_sub(1);
                    if found.is_some() {
                        seen.insert(candidate.clone());
                        result.push(candidate);
                        // A relative specifier resolves to exactly one
                        // real file — stop trying the remaining
                        // extension/index variants once one hits.
                        break;
                    }
                }
            }
        }
        Ok(result)
    }
}

fn read_worktree_file(root: &Path, path: &str) -> Option<String> {
    let full = root.join(path);
    std::fs::read_to_string(full).ok()
}

/// Port of `iter_ts_files`: walk `root`, skipping `TS_EXCLUDE_DIRS` and
/// dot-directories, yielding repo-relative `.ts`/`.tsx` paths (forward-
/// slash), excluding anything under `tests/`.
pub fn iter_ts_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk_ts_files(root, root, &mut out);
    out.sort();
    out
}

fn walk_ts_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if scan::TS_EXCLUDE_DIRS.contains(&name_str.as_ref()) || name_str.starts_with('.') {
                continue;
            }
            walk_ts_files(root, &path, out);
        } else if name_str.ends_with(".ts") || name_str.ends_with(".tsx") {
            if let Ok(rel) = path.strip_prefix(root) {
                let rel_str = rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
                if rel_str.starts_with("tests/") {
                    continue;
                }
                out.push(rel_str);
            }
        }
    }
}

/// Every TS/TSX file the diff touches in its post-image (deletions —
/// `+++ /dev/null` — carry no path and are already dropped by
/// `diff::parse_diff`), filtered through `scan::ts_file`. Pure; the
/// unit-testable "candidate-file derivation" the packet brief asks for.
pub fn changed_files_from_diff(diff_text: &str) -> Vec<String> {
    diff::parse_diff(diff_text)
        .into_iter()
        .map(|(path, _)| path)
        .filter(|p| scan::ts_file(p))
        .collect()
}

/// Extract every relative (`./` or `../`) module specifier referenced by
/// an `import ... from '<spec>'` / `export ... from '<spec>'` / bare
/// `import '<spec>'` statement in `text`. Pure — no filesystem/network
/// access. Non-relative specifiers (bare package names) are skipped:
/// GithubApi mode has no node_modules to resolve against, and an
/// unresolved external symbol is exactly what the manifest is for.
pub fn parse_relative_imports(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !(trimmed.starts_with("import") || trimmed.starts_with("export")) {
            continue;
        }
        if let Some(spec) = extract_from_specifier(trimmed) {
            if spec.starts_with("./") || spec.starts_with("../") {
                out.push(spec);
            }
        }
    }
    out
}

/// Find a quoted string literal following the last `from` keyword on the
/// line, or (for a bare `import '<spec>';`) the first quoted string
/// right after `import`. Hand-rolled — no regex crate available.
fn extract_from_specifier(line: &str) -> Option<String> {
    let search_from = if let Some(idx) = line.rfind("from") {
        &line[idx + 4..]
    } else if let Some(rest) = line.strip_prefix("import") {
        rest
    } else {
        return None;
    };
    let bytes = search_from.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\'' || b == b'"' {
            let quote = b;
            let rest = &search_from[i + 1..];
            if let Some(end) = rest.as_bytes().iter().position(|&c| c == quote) {
                return Some(rest[..end].to_string());
            }
            return None;
        }
    }
    None
}

/// Binding name -> module specifier for every `import` statement in
/// `text` (default/named/namespace/combined forms). Feeds the manifest's
/// provenance (`referenced but not defined in bundle: X <- <module>`) —
/// #1222 packet 3, new logic with no Python precedent. Best-effort: a
/// binding-clause shape this doesn't recognize is simply skipped, never
/// panics.
pub fn parse_import_bindings(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("import") {
            continue;
        }
        let Some(module) = extract_from_specifier(trimmed) else {
            continue;
        };
        let Some(from_idx) = trimmed.rfind("from") else {
            continue;
        };
        let clause = trimmed["import".len()..from_idx].trim();
        for part in clause.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some(inner) = part.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                for item in inner.split(',') {
                    let item = item.trim();
                    if item.is_empty() {
                        continue;
                    }
                    let binding = match item.find(" as ") {
                        Some(idx) => item[idx + 4..].trim(),
                        None => item,
                    };
                    if !binding.is_empty() {
                        out.insert(binding.to_string(), module.clone());
                    }
                }
            } else if let Some(rest) = part.strip_prefix("* as ") {
                let binding = rest.trim();
                if !binding.is_empty() {
                    out.insert(binding.to_string(), module.clone());
                }
            } else if part.chars().next().map(|c| c.is_alphabetic() || c == '_' || c == '$').unwrap_or(false) {
                out.insert(part.to_string(), module.clone());
            }
        }
    }
    out
}

/// Given the importing file's own repo-relative path and a relative
/// specifier (`./x`, `../y/z`), produce the candidate resolved paths in
/// Node/TS resolution order: `<resolved>.ts`, `<resolved>.tsx`,
/// `<resolved>/index.ts`, `<resolved>/index.tsx`. Pure — callers confirm
/// existence via `FileSource::read_file`.
pub fn resolve_relative_import_candidates(from_path: &str, spec: &str) -> Vec<String> {
    let from_dir = match from_path.rfind('/') {
        Some(idx) => &from_path[..idx],
        None => "",
    };
    let resolved = normalize_path(from_dir, spec);
    vec![
        format!("{resolved}.ts"),
        format!("{resolved}.tsx"),
        format!("{resolved}/index.ts"),
        format!("{resolved}/index.tsx"),
    ]
}

/// Join `base_dir` + `spec` (a `./`/`../`-relative specifier) and
/// collapse `.`/`..` segments, forward-slash throughout (git diff paths
/// are always POSIX-style regardless of host OS).
fn normalize_path(base_dir: &str, spec: &str) -> String {
    let mut segments: Vec<&str> = if base_dir.is_empty() {
        Vec::new()
    } else {
        base_dir.split('/').collect()
    };
    for part in spec.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// The one impure edge: `gh api "repos/{repo}/contents/{path}?ref={sha}"
/// -H "Accept: application/vnd.github.raw"`. Never called from a unit
/// test — the pure functions above (`changed_files_from_diff`,
/// `parse_relative_imports`, `resolve_relative_import_candidates`) are
/// the tested seam boundary.
fn gh_api_fetch(repo: &str, head_sha: &str, path: &str) -> Result<Option<String>> {
    let endpoint = format!("repos/{repo}/contents/{path}?ref={head_sha}");
    let output = Command::new("gh")
        .args(["api", &endpoint, "-H", "Accept: application/vnd.github.raw"])
        .output()
        .with_context(|| format!("running `gh api {endpoint}`"))?;
    if !output.status.success() {
        // A 404 (file doesn't exist at this ref) is the common case for a
        // speculative import-resolution probe — treat any failure as
        // "not found" rather than a hard error, matching the reference's
        // tolerant `except OSError: continue`.
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_files_from_diff_filters_ts_and_drops_deletions() {
        let d = "+++ b/src/a.ts\n@@ -1,1 +1,1 @@\n+x\n\
+++ b/src/b.json\n@@ -1,1 +1,1 @@\n+y\n\
+++ /dev/null\n@@ -1,1 +0,0 @@\n-z\n";
        let files = changed_files_from_diff(d);
        assert_eq!(files, vec!["src/a.ts".to_string()]);
    }

    #[test]
    fn parse_relative_imports_finds_relative_specs_only() {
        let src = "import { foo } from './helpers';\n\
                    import bar from '../lib/bar';\n\
                    import 'reflect-metadata';\n\
                    import { z } from 'zod';\n\
                    export { thing } from './thing';\n";
        let specs = parse_relative_imports(src);
        assert_eq!(specs, vec!["./helpers", "../lib/bar", "./thing"]);
    }

    #[test]
    fn resolve_relative_import_candidates_builds_four_variants() {
        let cands = resolve_relative_import_candidates("src/services/order.ts", "./helpers");
        assert_eq!(
            cands,
            vec![
                "src/services/helpers.ts".to_string(),
                "src/services/helpers.tsx".to_string(),
                "src/services/helpers/index.ts".to_string(),
                "src/services/helpers/index.tsx".to_string(),
            ]
        );
    }

    #[test]
    fn resolve_relative_import_candidates_handles_parent_dir() {
        let cands = resolve_relative_import_candidates("src/services/order.ts", "../lib/bar");
        assert_eq!(cands[0], "src/lib/bar.ts");
    }
}
