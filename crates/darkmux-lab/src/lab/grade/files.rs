//! What a result changed against the pristine fixture, and the two checks
//! that read it: `scope` (which files may change, and how) and `style`
//! (what a new file must contain). Paths are relative and `/`-separated;
//! the run-artifact and dependency directories (`crate::lab::artifact_dirs`)
//! are skipped at the top level, where the suite's copy skips them.

use crate::lab::artifact_dirs::RUN_ARTIFACT_DIRS;
use crate::workloads::grade_spec::{ScopeGrade, StyleGrade};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// One entry's content: a file's bytes, or a symlink's target.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    File(Vec<u8>),
    Symlink(String),
}

/// A modified path, with the fixture's and the result's bytes (`None` for a
/// symlink side).
pub(crate) type Modified = (String, Option<Vec<u8>>, Option<Vec<u8>>);

/// How the result differs from the fixture.
#[derive(Debug, Default)]
pub(crate) struct Changes {
    pub added: Vec<String>,
    /// The added regular files' bytes, as the walk read them (a symlink is
    /// not followed, so it has none).
    pub added_files: BTreeMap<String, Vec<u8>>,
    pub deleted: Vec<String>,
    /// Modified files, with both sides' bytes (`None` for a symlink side).
    pub modified: Vec<Modified>,
}

pub(crate) fn compare(fixture: &Path, result: &Path) -> Result<Changes> {
    let mut before = walk(fixture)?;
    // (#3361) The eval's own folder never reaches the sandbox, so its
    // absence from the result is not a deletion.
    if let Some(dir) = crate::lab::fixture::eval_dir_of(fixture) {
        let inside = format!("{dir}/");
        before.retain(|path, _| !path.starts_with(&inside));
    }
    let mut after = walk(result)?;
    let mut changes = Changes::default();
    for (path, old) in before {
        match after.remove(&path) {
            None => changes.deleted.push(path),
            Some(new) if new == old => {}
            Some(new) => changes.modified.push((path, bytes(old), bytes(new))),
        }
    }
    for (path, entry) in after {
        if let Entry::File(bytes) = entry {
            changes.added_files.insert(path.clone(), bytes);
        }
        changes.added.push(path);
    }
    Ok(changes)
}

fn bytes(e: Entry) -> Option<Vec<u8>> {
    match e {
        Entry::File(b) => Some(b),
        Entry::Symlink(_) => None,
    }
}

fn walk(root: &Path) -> Result<BTreeMap<String, Entry>> {
    let mut out = BTreeMap::new();
    walk_into(root, "", &mut out)?;
    Ok(out)
}

fn walk_into(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Entry>) -> Result<()> {
    for (name, path, kind) in entries(dir)? {
        if is_artifact(prefix, &name) {
            continue;
        }
        let rel = join_rel(prefix, &name);
        if kind.is_dir() {
            walk_into(&path, &rel, out)?;
        } else if let Some(e) = read_entry(&path, kind)? {
            out.insert(rel, e);
        }
    }
    Ok(())
}

/// Every entry of `dir`: its name, path and type (a link's own type, never
/// its target's).
fn entries(dir: &Path) -> Result<Vec<(String, std::path::PathBuf, fs::FileType)>> {
    fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|entry| {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            Ok((name, entry.path(), entry.file_type()?))
        })
        .collect()
}

fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// A run or build dropping (`__pycache__`, `.coverage`, `target`, ...), file
/// or directory, at any depth outside a dependency tree. Scope ignores them,
/// and every copy the suite runs in has them removed, so what scope does not
/// see the suite does not run.
pub(crate) fn is_artifact(prefix: &str, name: &str) -> bool {
    RUN_ARTIFACT_DIRS.contains(&name) && !prefix.split('/').any(|p| p == "node_modules")
}

/// Remove every run or build dropping under `dir` (see [`is_artifact`]).
/// Symlinks are removed as links, never followed.
pub(crate) fn prune_artifacts(dir: &Path, prefix: &str) -> Result<()> {
    for (name, path, kind) in entries(dir)? {
        if is_artifact(prefix, &name) {
            remove(&path, kind)?;
        } else if kind.is_dir() {
            prune_artifacts(&path, &join_rel(prefix, &name))?;
        }
    }
    Ok(())
}

/// Remove a directory tree, or a file or link.
fn remove(path: &Path, kind: fs::FileType) -> Result<()> {
    if kind.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// A file's bytes or a symlink's target; `None` for anything else.
fn read_entry(path: &Path, kind: fs::FileType) -> Result<Option<Entry>> {
    if kind.is_symlink() {
        return Ok(Some(Entry::Symlink(
            fs::read_link(path)?.to_string_lossy().into_owned(),
        )));
    }
    if kind.is_file() {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        return Ok(Some(Entry::File(bytes)));
    }
    Ok(None)
}

/// `scope`'s problems: a delete, a new file outside `may_add`, an existing
/// file outside `append_only` changed, or an `append_only` file whose
/// original lines did not all survive in order.
pub(crate) fn scope_violations(changes: &Changes, spec: &ScopeGrade) -> Vec<String> {
    let mut problems: Vec<String> = changes
        .deleted
        .iter()
        .map(|p| format!("deleted {p}"))
        .collect();
    problems.extend(
        changes
            .added
            .iter()
            .filter(|p| !spec.may_add.iter().any(|g| glob_match(g, p)))
            .map(|p| format!("added {p}, outside the files it may add")),
    );
    for (path, old, new) in &changes.modified {
        if !spec.append_only.iter().any(|g| glob_match(g, path)) {
            problems.push(format!("changed {path}"));
            continue;
        }
        // Not UTF-8 on either side: no line comparison holds, so it fails.
        let kept = match (old, new) {
            (Some(old), Some(new)) => match (std::str::from_utf8(old), std::str::from_utf8(new)) {
                (Ok(old), Ok(new)) => lines_kept(old, new, &spec.comment_prefixes),
                _ => false,
            },
            _ => false,
        };
        if !kept {
            problems.push(format!(
                "changed existing lines in {path} (it may only gain lines)"
            ));
        }
    }
    problems
}

/// Whether every line of `old` appears in `new`, in order, except blank
/// lines and comment lines (starting with one of `comment_prefixes` after
/// indentation) when any prefix is named.
pub(crate) fn lines_kept(old: &str, new: &str, comment_prefixes: &[String]) -> bool {
    let may_go = |line: &str| {
        let t = line.trim_start();
        !comment_prefixes.is_empty()
            && (t.is_empty() || comment_prefixes.iter().any(|p| t.starts_with(p.as_str())))
    };
    let mut new_lines = new.lines();
    old.lines()
        .filter(|l| !may_go(l))
        .all(|line| new_lines.any(|n| n == line))
}

/// `style`'s problems: a new file matching `files` that lacks a required string.
pub(crate) fn style_violations(changes: &Changes, spec: &StyleGrade) -> Result<Vec<String>> {
    let mut problems = vec![];
    for path in changes.added.iter().filter(|p| glob_match(&spec.files, p)) {
        let text = changes
            .added_files
            .get(path)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        let missing: Vec<&str> = spec
            .must_contain
            .iter()
            .filter(|s| !text.contains(s.as_str()))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            problems.push(format!(
                "{path} lacks {}",
                missing
                    .iter()
                    .map(|m| format!("`{m}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    Ok(problems)
}

/// A path glob: `**` matches any number of whole segments, `*` and `?`
/// match within one segment.
pub(crate) fn glob_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let segs: Vec<&str> = path.split('/').collect();
    segments_match(&pat, &segs)
}

fn segments_match(pat: &[&str], segs: &[&str]) -> bool {
    match pat.first() {
        None => segs.is_empty(),
        Some(&"**") => (0..=segs.len()).any(|i| segments_match(&pat[1..], &segs[i..])),
        Some(p) => {
            !segs.is_empty()
                && segment_match(p.as_bytes(), segs[0].as_bytes())
                && segments_match(&pat[1..], &segs[1..])
        }
    }
}

fn segment_match(p: &[u8], s: &[u8]) -> bool {
    match (p.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            segment_match(&p[1..], s) || (!s.is_empty() && segment_match(p, &s[1..]))
        }
        (Some(b'?'), Some(_)) => segment_match(&p[1..], &s[1..]),
        (Some(a), Some(b)) if a == b => segment_match(&p[1..], &s[1..]),
        _ => false,
    }
}
