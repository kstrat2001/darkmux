//! Orchestration: diff -> per-hunk `Bundle`s, in both modes the frozen
//! `--bundler` contract has to support: a real worktree checkout, or
//! diff-only (darkmux's own self-review CI — `--github`/`--head-sha`,
//! no checkout at all — is the diff-only path, so it is the one that
//! has to work well, not the degraded fallback).
//!
//! Always emits ONE bundle per hunk that touches a `.rs` file's new-side
//! lines, regardless of whether the differential fact family found
//! anything — the bundle's CODE is the primary payload a probe reviews;
//! zero facts is a legitimate, honest outcome for a clean hunk, not a
//! reason to withhold the code from review. This also keeps a normal
//! Rust PR (most hunks carry no dropped-call signal) from collapsing to
//! zero bundles -> degenerate on ordinary changes.

use crate::contract::{Bundle, BundleRef, BundleSet};
use crate::diff::{parse_diff, Hunk};
use crate::facts;
use crate::scan;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;

fn is_rust_file(path: &str) -> bool {
    path.ends_with(".rs") && !path.starts_with("target/")
}

/// The new-file line number for each position in `hunk.new_block` —
/// `new_lines` is a `BTreeSet<u32>` built in strictly increasing order
/// during parsing (see `diff.rs`), so it aligns index-for-index with
/// `new_block`.
fn new_line_numbers(hunk: &Hunk) -> Vec<u32> {
    hunk.new_lines.iter().copied().collect()
}

/// The file-wide pool of calls appearing in ANY hunk's post-image for
/// this file — "was this call re-added ANYWHERE in the diff," not just
/// within the one hunk it might have moved to.
fn added_call_pool(hunks: &[Hunk]) -> HashSet<String> {
    let mut pool = HashSet::new();
    for h in hunks {
        pool.extend(facts::extract_calls(&h.new_block.join("\n")));
    }
    pool
}

/// Diff-only bundling: the hunk's own visible window (context lines
/// included) is all the scanner ever sees. A `fn` signature within that
/// window resolves a NAMED bundle (marked `truncated` only if the window
/// never showed the closing brace); no signature in the window at all
/// falls back to an honest, unnamed hunk-level bundle rather than
/// guessing at an enclosing function it can't see.
fn bundle_file_diff_only(path: &str, hunks: &[Hunk]) -> Vec<Bundle> {
    let pool = added_call_pool(hunks);
    // id -> index into `bundles`. Two SEPARATE hunks resolving to the
    // same function name is rare in diff-only mode (git coalesces nearby
    // edits into one hunk; this needs a short function with two edits
    // each independently within the OTHER hunk's narrow context window)
    // but not impossible — dedup rather than emit two bundles the
    // downstream review pipeline would have to reconcile itself. Each hunk's own
    // window may capture a different slice, so the merge takes the
    // UNION of both spans (an honest superset, not a guess at which is
    // "more complete") and merges facts, same principle as the worktree
    // path.
    let mut seen_ids: HashMap<String, usize> = HashMap::new();
    let mut bundles: Vec<Bundle> = Vec::with_capacity(hunks.len());
    for h in hunks {
        let line_nums = new_line_numbers(h);
        if line_nums.is_empty() {
            continue; // pure-deletion hunk — nothing on the new side to bundle
        }
        let diff_facts = facts::build_differential_facts(&h.old_block.join("\n"), &pool);

        // A hunk can legitimately contain MULTIPLE complete functions —
        // most commonly a brand-new file, where unified diff has no
        // old-side content to anchor per-function boundaries against, so
        // the whole file lands in one hunk. Try that first; every
        // resolved function gets its own bundle, sharing this hunk's
        // differential facts (a v1 simplification — see the module doc).
        let all_spans = scan::find_all_fns_in_lines(&h.new_block);
        if !all_spans.is_empty() {
            for s in &all_spans {
                let start = line_nums.get(s.start0).copied().unwrap_or(h.new_start);
                let end = line_nums.get(s.end0).copied().unwrap_or(*line_nums.last().unwrap());
                let id = format!("{}@{path}", s.name);
                if let Some(&idx) = seen_ids.get(&id) {
                    let existing = &mut bundles[idx];
                    let r = &mut existing.code[0];
                    r.start = r.start.min(start);
                    r.end = r.end.max(end);
                    existing.truncated = existing.truncated || !s.closed;
                    for f in &diff_facts {
                        if !existing.facts.contains(f) {
                            existing.facts.push(f.clone());
                        }
                    }
                    continue;
                }
                seen_ids.insert(id.clone(), bundles.len());
                bundles.push(Bundle {
                    id,
                    code: vec![BundleRef { path: path.to_string(), start, end }],
                    facts: diff_facts.clone(),
                    fact_family: "differential".to_string(),
                    manifest: Vec::new(),
                    truncated: !s.closed,
                });
            }
            continue;
        }

        // No signature anywhere in the hunk's window — an edit deep
        // inside a function body whose `fn` line sits outside the diff's
        // context. Never guess; fall back to an honest, unnamed
        // hunk-level bundle spanning exactly what's visible.
        bundles.push(Bundle {
            id: format!("hunk-L{}@{path}", h.new_start),
            code: vec![BundleRef {
                path: path.to_string(),
                start: *line_nums.first().unwrap(),
                end: *line_nums.last().unwrap(),
            }],
            facts: diff_facts,
            fact_family: "differential".to_string(),
            manifest: vec![
                "enclosing function not resolvable from diff context alone (no worktree; \
                 this hunk's context window doesn't reach a `fn` line)"
                    .to_string(),
            ],
            truncated: true,
        });
    }
    bundles
}

/// Worktree-available bundling: full-file context, so `find_enclosing_fn`
/// resolves reliably and rarely truncates. Multiple hunks resolving to
/// the SAME function dedup to ONE bundle (its code span was already
/// resolved from the first hunk that found it, so re-resolving costs
/// nothing new) — but each hunk's OWN differential facts are still
/// merged in, not discarded. A call dropped only in a later hunk's
/// pre-image is a real finding; silently keeping just the first hunk's
/// facts would lose it.
fn bundle_file_with_worktree(worktree: &Path, path: &str, hunks: &[Hunk]) -> Result<Vec<Bundle>> {
    let full_path = worktree.join(path);
    let content = std::fs::read_to_string(&full_path)
        .with_context(|| format!("reading {} from the worktree", full_path.display()))?;
    let file_lines: Vec<String> = content.lines().map(str::to_string).collect();
    let pool = added_call_pool(hunks);
    // id -> index into `bundles`, so a dedup hit can merge facts into the
    // EXISTING bundle rather than just discarding the new hunk's findings.
    let mut seen_ids: HashMap<String, usize> = HashMap::new();
    let mut bundles: Vec<Bundle> = Vec::with_capacity(hunks.len());
    for h in hunks {
        if h.new_lines.is_empty() {
            continue;
        }
        let touched_line = *h.new_lines.iter().next().unwrap();
        let touched_idx0 = (touched_line as usize).saturating_sub(1);
        if touched_idx0 >= file_lines.len() {
            continue; // diff/worktree mismatch (stale checkout) — skip rather than panic
        }
        let span = scan::find_enclosing_fn(&file_lines, touched_idx0);
        let diff_facts = facts::build_differential_facts(&h.old_block.join("\n"), &pool);

        let (id, start_line, end_line, truncated, manifest) = match &span {
            Some(s) => {
                let id = format!("{}@{path}", s.name);
                if let Some(&idx) = seen_ids.get(&id) {
                    let existing = &mut bundles[idx];
                    for f in diff_facts {
                        if !existing.facts.contains(&f) {
                            existing.facts.push(f);
                        }
                    }
                    continue;
                }
                seen_ids.insert(id.clone(), bundles.len());
                (id, (s.start0 + 1) as u32, (s.end0 + 1) as u32, !s.closed, Vec::new())
            }
            None => {
                let line_nums = new_line_numbers(h);
                (
                    format!("hunk-L{}@{path}", h.new_start),
                    *line_nums.first().unwrap(),
                    *line_nums.last().unwrap(),
                    true,
                    vec!["no enclosing `fn` found (module-level code, or the file didn't \
                          brace-match cleanly)"
                        .to_string()],
                )
            }
        };
        bundles.push(Bundle {
            id,
            code: vec![BundleRef { path: path.to_string(), start: start_line, end: end_line }],
            facts: diff_facts,
            fact_family: "differential".to_string(),
            manifest,
            truncated,
        });
    }
    Ok(bundles)
}

/// Build every bundle for `diff_text`, restricted to changed `.rs`
/// files. `worktree.is_some()` picks the full-file-context path; `None`
/// is the diff-only path darkmux's own self-review CI actually runs.
pub fn build_bundles(worktree: Option<&Path>, diff_text: &str) -> Result<BundleSet> {
    let files = parse_diff(diff_text);
    let mut bundles = Vec::new();
    for (path, hunks) in &files {
        if !is_rust_file(path) {
            continue;
        }
        let file_bundles = match worktree {
            Some(wt) => bundle_file_with_worktree(wt, path, hunks)?,
            None => bundle_file_diff_only(path, hunks),
        };
        bundles.extend(file_bundles);
    }
    Ok(BundleSet { bundles })
}

#[cfg(test)]
mod tests {
    use super::*;

    // An explicit line array + join, NEVER a `\`-continued string literal
    // (see `diff.rs`'s vendored source: line continuation strips ALL
    // leading whitespace off the next line, silently eating the
    // significant leading space that marks a unified-diff context line
    // — the exact bug this helper exists to avoid re-introducing).
    fn diff(lines: &[&str]) -> String {
        let mut s = lines.join("\n");
        s.push('\n');
        s
    }

    #[test]
    fn non_rust_files_are_skipped() {
        let d = diff(&["+++ b/README.md", "@@ -1,1 +1,1 @@", "-old", "+new"]);
        let set = build_bundles(None, &d).unwrap();
        assert!(set.bundles.is_empty());
    }

    #[test]
    fn diff_only_bundles_a_resolvable_function_and_reports_a_dropped_call() {
        let d = diff(&[
            "+++ b/src/lib.rs",
            "@@ -1,5 +1,4 @@",
            " fn process(x: u32) -> u32 {",
            "-    validate(x);",
            "     let y = x + 1;",
            "     y",
            " }",
        ]);
        let set = build_bundles(None, &d).unwrap();
        assert_eq!(set.bundles.len(), 1);
        let b = &set.bundles[0];
        assert_eq!(b.id, "process@src/lib.rs");
        assert_eq!(b.fact_family, "differential");
        assert!(!b.truncated, "the closing brace is inside the hunk's own context");
        assert!(b.facts.iter().any(|f| f.contains("`validate(...)`")));
    }

    #[test]
    fn diff_only_falls_back_to_hunk_bundle_when_no_signature_in_window() {
        // A change deep inside a function body, with the `fn` line outside
        // the hunk's visible context.
        let d = diff(&[
            "+++ b/src/lib.rs",
            "@@ -50,3 +50,3 @@",
            "     let mid = 1;",
            "-    let old_call = compute();",
            "+    let new_call = compute();",
            "     mid",
        ]);
        let set = build_bundles(None, &d).unwrap();
        assert_eq!(set.bundles.len(), 1);
        let b = &set.bundles[0];
        assert!(b.id.starts_with("hunk-L50@"));
        assert!(b.truncated);
        assert!(!b.manifest.is_empty());
    }

    #[test]
    fn diff_only_mode_dedupes_two_hunks_resolving_the_same_function() {
        // Two separate hunks, each independently capturing the SAME
        // `fn shared()` signature in its own context window (hand-built —
        // real git diffs coalesce nearby edits, but this exercises the
        // dedup+union path directly rather than depending on git's own
        // hunk-splitting heuristics).
        let d = diff(&[
            "+++ b/src/lib.rs",
            "@@ -1,6 +1,6 @@",
            " fn shared() {",
            "-    let a = 1;",
            "+    let a = 10;",
            "     let b = 2;",
            "     let c = 3;",
            "     a + b + c",
            " }",
            "@@ -1,6 +1,6 @@",
            " fn shared() {",
            "     let a = 10;",
            "     let b = 2;",
            "-    let c = 3;",
            "+    let c = 30;",
            "     a + b + c",
            " }",
        ]);
        let set = build_bundles(None, &d).unwrap();
        assert_eq!(set.bundles.len(), 1, "both hunks resolve to the same function, must dedup to one bundle");
        assert_eq!(set.bundles[0].id, "shared@src/lib.rs");
    }

    #[test]
    fn worktree_mode_dedupes_multiple_hunks_in_the_same_function() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "fn process(x: u32) -> u32 {\n    let a = 1;\n    let b = 2;\n    a + b + x\n}\n",
        )
        .unwrap();
        let d = diff(&[
            "+++ b/src/lib.rs",
            "@@ -2,1 +2,1 @@",
            "-    let a = 0;",
            "+    let a = 1;",
            "@@ -3,1 +3,1 @@",
            "-    let b = 0;",
            "+    let b = 2;",
        ]);
        let set = build_bundles(Some(dir.path()), &d).unwrap();
        assert_eq!(set.bundles.len(), 1, "both hunks resolve to the same function");
        assert_eq!(set.bundles[0].id, "process@src/lib.rs");
        assert_eq!(set.bundles[0].code[0].start, 1);
        assert_eq!(set.bundles[0].code[0].end, 5);
    }
}
