//! What a result changed against the pristine fixture, and the two checks
//! that read it: `scope` (which files may change, and how) and `style`
//! (what a new file must contain). Paths are relative and `/`-separated;
//! the run-artifact and dependency directories (`crate::lab::artifact_dirs`)
//! are skipped at the top level, where the suite's copy skips them.

use crate::lab::artifact_dirs::{HASH_ONLY_EXCLUDES, RUN_ARTIFACT_DIRS};
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
    pub deleted: Vec<String>,
    /// Modified files, with both sides' bytes (`None` for a symlink side).
    pub modified: Vec<Modified>,
}

pub(crate) fn compare(fixture: &Path, result: &Path) -> Result<Changes> {
    let before = walk(fixture)?;
    let mut after = walk(result)?;
    let mut changes = Changes::default();
    for (path, old) in before {
        match after.remove(&path) {
            None => changes.deleted.push(path),
            Some(new) if new == old => {}
            Some(new) => changes.modified.push((path, bytes(old), bytes(new))),
        }
    }
    changes.added = after.into_keys().collect();
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
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            out.insert(
                rel,
                Entry::Symlink(fs::read_link(entry.path())?.to_string_lossy().into_owned()),
            );
        } else if kind.is_dir() {
            // Skipped at the top only, as the suite's copy skips them: a
            // nested one is in what the suite runs, so scope sees it.
            let skipped = prefix.is_empty()
                && (RUN_ARTIFACT_DIRS.contains(&name.as_str())
                    || HASH_ONLY_EXCLUDES.contains(&name.as_str()));
            if !skipped {
                walk_into(&entry.path(), &rel, out)?;
            }
        } else if kind.is_file() {
            out.insert(
                rel,
                Entry::File(
                    fs::read(entry.path())
                        .with_context(|| format!("reading {}", entry.path().display()))?,
                ),
            );
        }
    }
    Ok(())
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
        let kept = match (old, new) {
            (Some(old), Some(new)) => lines_kept(
                &String::from_utf8_lossy(old),
                &String::from_utf8_lossy(new),
                &spec.comment_prefixes,
            ),
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
pub(crate) fn style_violations(
    changes: &Changes,
    result: &Path,
    spec: &StyleGrade,
) -> Result<Vec<String>> {
    let mut problems = vec![];
    for path in changes.added.iter().filter(|p| glob_match(&spec.files, p)) {
        let text = fs::read_to_string(result.join(path)).unwrap_or_default();
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
