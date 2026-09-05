//! Unified-diff parsing (#2310 P4b move) — a straight port of the Python
//! reference's `parse_diff` (`bundler.py`, Phase A). Splits a multi-file
//! unified diff into per-file hunks, each carrying the added/removed/old/new
//! line sets a consumer needs.
//!
//! No `regex` crate (workspace dep discipline) — the two line shapes this
//! parses (`+++ [b/]<path>[\t<timestamp>]` and `@@ -a,b +c,d @@`) are
//! simple enough for hand-rolled prefix/token parsing. Every unified-diff
//! dialect a real tool emits is accepted (#2310 fix-loop B1) — see
//! [`header_path`] for the three verified header shapes and why the
//! optional `a/`/`b/` prefix is accepted rather than required.
//!
//! **Canonical home, moved from `darkmux-lab`'s `bundle::diff` (#2310 P4b).**
//! `deliver_github_review`'s "is this line inside the diff" check needs the
//! SAME parser `darkmux-lab`'s bundler already had, and darkmux-lab depends
//! on darkmux-crew (never the reverse) — so the crate boundary is a reason
//! to MOVE the parser down to where both sides can reach it, not to
//! hand-roll a second copy in darkmux-crew (the original mistake this
//! move fixes; see this crate's own `deliver_github_review.rs` module doc
//! for the incident). `darkmux_lab::lab::bundle::diff` re-exports this
//! module's `Hunk`/`parse_diff` verbatim, so every existing lab call site
//! (`bundle::source`, `bundle::mod`) is untouched.

use std::collections::BTreeSet;


/// One `@@ ... @@` hunk within a file's diff.
#[derive(Debug, Clone, Default)]
pub struct Hunk {
    /// The hunk's starting line number in the NEW file (1-indexed), from
    /// the `@@ -a,b +c,d @@` header's `+c`.
    pub new_start: u32,
    /// The hunk's starting line number in the OLD file (1-indexed), from
    /// the same header's `-a` (#2310 P4b review, M-B). Not needed by the
    /// bundler (every existing caller anchors off the NEW side only) —
    /// added for `deliver_github_review`'s suggestion-block anchoring: a
    /// mod's kit is itself a unified diff whose OLD side names the lines
    /// it replaces in the file's CURRENT state, which is the SAME
    /// coordinate space the PR diff's NEW side already occupies (the file
    /// as the PR leaves it). Additive — every existing reader of `Hunk`
    /// that never looks at this field is unaffected.
    pub old_start: u32,
    /// Every line number (1-indexed, in the NEW file) touched by this
    /// hunk — added lines AND unchanged context lines (matches the
    /// reference: context lines advance `new_ln` and land in
    /// `new_lines` too, since a changed function can be located via a
    /// context line inside it just as well as an added one). Do NOT
    /// narrow this to "actually changed" lines — locating the enclosing
    /// function is exactly the case that needs context lines included.
    /// For "was this specific line actually added" (as opposed to merely
    /// present in the hunk's span), see `added_lines` below.
    pub new_lines: BTreeSet<u32>,
    /// Subset of `new_lines`: just the line numbers that were actually
    /// ADDED (`+` prefix) — no context lines (#1605 follow-up, QA
    /// finding). `new_lines` deliberately mixes added and context so a
    /// changed function can be located via either; a caller that needs to
    /// know whether a specific line was really touched by the diff (e.g.
    /// deciding whether an unenclosed line is real changed code worth
    /// bundling, vs. unchanged context that merely fell inside a hunk's
    /// span) wants this set instead.
    pub added_line_numbers: BTreeSet<u32>,
    /// Every line of the pre-image within this hunk's span: removed
    /// lines AND unchanged context lines, in order.
    pub old_block: Vec<String>,
    /// Every line of the post-image within this hunk's span: added
    /// lines AND unchanged context lines, in order.
    pub new_block: Vec<String>,
    /// Just the added lines (`+` prefix), in order.
    pub added: Vec<String>,
    /// Just the removed lines (`-` prefix), in order.
    pub removed: Vec<String>,
}

/// Split a unified multi-file diff into per-file hunks. Returns
/// `(path, hunks)` pairs in first-appearance order (mirrors Python's
/// `dict.setdefault` insertion-order semantics, which the reference
/// relies on for deterministic bundle ordering).
pub fn parse_diff(diff_text: &str) -> Vec<(String, Vec<Hunk>)> {
    let mut files: Vec<(String, Vec<Hunk>)> = Vec::new();
    let mut path: Option<String> = None;
    let mut cur: Option<Hunk> = None;
    let mut new_ln: u32 = 0;

    fn flush(files: &mut Vec<(String, Vec<Hunk>)>, path: &Option<String>, cur: &mut Option<Hunk>) {
        if let (Some(h), Some(p)) = (cur.take(), path) {
            match files.iter_mut().find(|(fp, _)| fp == p) {
                Some(entry) => entry.1.push(h),
                None => files.push((p.clone(), vec![h])),
            }
        }
    }

    for ln in diff_text.lines() {
        if let Some(rest) = ln.strip_prefix("+++ ") {
            flush(&mut files, &path, &mut cur);
            path = header_path(rest);
            cur = None;
            continue;
        }
        if let Some((old_start, start)) = parse_hunk_header(ln) {
            flush(&mut files, &path, &mut cur);
            cur = Some(Hunk {
                new_start: start,
                old_start,
                ..Default::default()
            });
            new_ln = start;
            continue;
        }
        if cur.is_none() || path.is_none() {
            continue;
        }
        if ln.starts_with('+') && !ln.starts_with("+++") {
            let content = &ln[1..];
            let h = cur.as_mut().unwrap();
            h.added.push(content.to_string());
            h.new_block.push(content.to_string());
            h.new_lines.insert(new_ln);
            h.added_line_numbers.insert(new_ln);
            new_ln += 1;
        } else if ln.starts_with('-') && !ln.starts_with("---") {
            let content = &ln[1..];
            let h = cur.as_mut().unwrap();
            h.removed.push(content.to_string());
            h.old_block.push(content.to_string());
        } else if let Some(content) = ln.strip_prefix(' ') {
            let h = cur.as_mut().unwrap();
            h.old_block.push(content.to_string());
            h.new_block.push(content.to_string());
            h.new_lines.insert(new_ln);
            new_ln += 1;
        } else if ln.is_empty() || ln == "\r" {
            // (#2310 fix-loop B2, S3-3) A TRULY empty line inside a hunk
            // body is a context line whose content is empty. git emits it
            // as a lone space, but everything that touches a patch on the
            // way here — editors, copy/paste, trailing-whitespace
            // strippers, the GitHub API — routinely drops that space.
            // Skipping such a line left `new_ln` un-advanced, which
            // shifted the line number of EVERY later line in the hunk and
            // desynced `old_block`/`new_block` from the file, so a finding
            // on a real changed line read as outside the diff. Treated as
            // context with empty content, exactly like ` ` would be.
            let h = cur.as_mut().unwrap();
            h.old_block.push(String::new());
            h.new_block.push(String::new());
            h.new_lines.insert(new_ln);
            new_ln += 1;
        }
        // Other lines (e.g. `\ No newline at end of file`, the `---
        // a/<path>` line, `diff --git` headers) carry no line-content
        // signal — ignored, matching the reference.
    }
    flush(&mut files, &path, &mut cur);
    files
}

/// The path a `+++ <rest>` (or `--- <rest>`) header names, or `None` when
/// it names `/dev/null` (a deleted file) or nothing at all.
///
/// (#2310 fix-loop B1, S3-1 — PROVEN live) This used to be a literal
/// `strip_prefix("+++ b/")`, which recognized ONLY `git diff`'s default
/// dialect. Three shapes verified against real tool output on 2026-09-05:
///
/// ```text
/// +++ b/src/x.ts                          git diff (default)
/// +++ src/x.ts                            git diff --no-prefix
/// +++ src/x.ts\t2026-09-05 08:45:23       diff -u / diff -Naur
/// ```
///
/// A patch in either prefixless dialect parsed to ZERO files, so a
/// review-v2 run over it completed green with a noop payload — the
/// operator read "the PR is clean" off a diff that was never parsed. The
/// `-Naur` shape parsed the tab and timestamp INTO the path, which then
/// matched no file in the tree (`hunks_total:5, covered:0` → noop).
///
/// So: cut at the first tab (the timestamp field), drop a trailing `\r`
/// (a lone CR — `str::lines` already eats the one in a CRLF pair), then
/// strip an OPTIONAL `b/`/`a/` prefix.
///
/// **The optional prefix is genuinely ambiguous and cannot be resolved
/// from one line.** A `--no-prefix` diff of a repo whose top-level
/// directory is literally named `b` yields `+++ b/foo.ts`, which is
/// indistinguishable from the default dialect's rendering of `foo.ts`.
/// Accepting the prefix is right for the overwhelmingly common case (git's
/// default) and for the operator's real one (`--no-prefix`); the `b/`-
/// directory repo is the sacrificed corner. Preferring the strict
/// dialect — the previous behavior — sacrifices EVERY prefixless patch
/// instead, silently, which is the bug being fixed.
fn header_path(rest: &str) -> Option<String> {
    let p = rest.split('\t').next().unwrap_or(rest);
    let p = p.strip_suffix('\r').unwrap_or(p);
    let p = p.strip_prefix("b/").or_else(|| p.strip_prefix("a/")).unwrap_or(p);
    if p.is_empty() || p == "/dev/null" {
        None
    } else {
        Some(p.to_string())
    }
}

/// Parse `@@ -a[,b] +c[,d] @@...` and return `(a, c)` — the OLD-file and
/// NEW-file start lines — or `None` if `ln` isn't a hunk header.
/// Hand-rolled equivalent of `re.match(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@", ln)`.
///
/// (#2310 P4b review, M-B) Returns BOTH sides now (used to return only
/// `c`) — `Hunk::old_start` needs `a` too. Every existing call site reads
/// `.1` for what used to be the whole return value, so nothing downstream
/// of the new-side number changed.
fn parse_hunk_header(ln: &str) -> Option<(u32, u32)> {
    let rest = ln.strip_prefix("@@ -")?;
    let space = rest.find(' ')?;
    let minus_digits = rest[..space].split(',').next()?;
    let old_start: u32 = minus_digits.parse().ok()?;
    let after_minus = &rest[space + 1..];
    let plus_digits = after_minus.strip_prefix('+')?;
    let end = plus_digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(plus_digits.len());
    if end == 0 {
        return None;
    }
    // The char right after the digit run must be `,` (a `+c,d` count) or
    // ` ` (bare `+c`) followed eventually by ` @@` — anything else means
    // this wasn't really a hunk header (defensive; real diffs won't hit
    // this branch).
    let tail = &plus_digits[end..];
    if !(tail.starts_with(',') || tail.starts_with(' ')) {
        return None;
    }
    let new_start: u32 = plus_digits[..end].parse().ok()?;
    Some((old_start, new_start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_file_single_hunk() {
        // Built via an explicit line array (not a backslash-continued
        // string literal) — a `\` line continuation strips ALL leading
        // whitespace off the next line, which would silently eat the
        // significant leading space on unified-diff context lines below.
        let diff = [
            "diff --git a/x.ts b/x.ts",
            "--- a/x.ts",
            "+++ b/x.ts",
            "@@ -1,3 +1,4 @@",
            " line one",
            "-old line",
            "+new line",
            "+added line",
            " line four",
            "",
        ]
        .join("\n");
        let files = parse_diff(&diff);
        assert_eq!(files.len(), 1);
        let (path, hunks) = &files[0];
        assert_eq!(path, "x.ts");
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.new_start, 1);
        assert_eq!(h.old_start, 1, "(#2310 P4b) the OLD-side start line, from the header's `-a`");
        assert_eq!(h.added, vec!["new line", "added line"]);
        assert_eq!(h.removed, vec!["old line"]);
        // new_lines: line one(1), new line(2), added line(3), line four(4)
        assert_eq!(
            h.new_lines,
            [1, 2, 3, 4].into_iter().collect::<BTreeSet<u32>>()
        );
        // (#1605 follow-up) `added_line_numbers` is the STRICT subset of
        // `new_lines` that was actually added — lines 1 and 4 are context
        // (unchanged), only 2 and 3 ("new line"/"added line") carry a `+`.
        assert_eq!(
            h.added_line_numbers,
            [2, 3].into_iter().collect::<BTreeSet<u32>>()
        );
    }

    #[test]
    fn dev_null_target_drops_file() {
        let diff = "+++ /dev/null\n@@ -1,2 +0,0 @@\n-gone\n-gone2\n";
        let files = parse_diff(diff);
        assert!(files.is_empty());
    }

    #[test]
    fn multiple_hunks_same_file_collect_under_one_entry() {
        let diff = "+++ b/a.ts\n\
@@ -1,1 +1,1 @@\n\
+x\n\
+++ b/a.ts\n\
@@ -10,1 +10,1 @@\n\
+y\n";
        let files = parse_diff(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1.len(), 2);
    }

    /// (#2310 fix-loop B1, S3-1 — PROVEN live) The parser used to bind a
    /// file ONLY via a literal `+++ b/` prefix, so a `git diff
    /// --no-prefix` or a plain `diff -u`/`diff -Naur` patch parsed to ZERO
    /// files — a review-v2 run over such a diff completed green with a
    /// noop payload and the operator read that as "the PR is clean". Every
    /// dialect below is the SAME logical change and must parse to the same
    /// path, hunk count and line sets.
    ///
    /// The four header shapes are transcribed from real tool output
    /// (`git diff --cached -M`, the same with `--no-prefix`, and BSD
    /// `diff -Naur`, run 2026-09-05) — not guessed.
    #[test]
    fn every_unified_diff_dialect_parses_to_the_same_file_and_lines() {
        let body = ["@@ -1,3 +1,4 @@", " a", "-b", "+B", "+B2", " c"];
        let dialects: Vec<(&str, Vec<String>)> = vec![
            (
                "git default (a/ b/ prefixes)",
                vec!["diff --git a/src/x.ts b/src/x.ts".into(), "--- a/src/x.ts".into(), "+++ b/src/x.ts".into()],
            ),
            (
                "git --no-prefix",
                vec!["diff --git src/x.ts src/x.ts".into(), "--- src/x.ts".into(), "+++ src/x.ts".into()],
            ),
            (
                "diff -Naur (no prefix, tab + timestamp)",
                vec![
                    "diff -Naur old/src/x.ts new/src/x.ts".into(),
                    "--- old/src/x.ts\t2026-09-05 08:45:23".into(),
                    "+++ src/x.ts\t2026-09-05 08:45:23".into(),
                ],
            ),
            (
                "git default + tab-and-timestamp suffix",
                vec!["--- a/src/x.ts\t2026-09-05 08:45:23".into(), "+++ b/src/x.ts\t2026-09-05 08:45:23".into()],
            ),
        ];
        for (label, header) in dialects {
            let mut lines = header;
            lines.extend(body.iter().map(|s| s.to_string()));
            for (sep, sep_label) in [("\n", "LF"), ("\r\n", "CRLF")] {
                let text = lines.join(sep) + sep;
                let files = parse_diff(&text);
                assert_eq!(files.len(), 1, "{label} / {sep_label}: parsed {files:?}");
                assert_eq!(files[0].0, "src/x.ts", "{label} / {sep_label}");
                assert_eq!(files[0].1.len(), 1, "{label} / {sep_label}");
                let h = &files[0].1[0];
                assert_eq!(h.added, vec!["B", "B2"], "{label} / {sep_label}");
                assert_eq!(h.removed, vec!["b"], "{label} / {sep_label}");
                assert_eq!(h.new_start, 1, "{label} / {sep_label}");
                assert_eq!(h.old_start, 1, "{label} / {sep_label}");
                assert_eq!(h.new_lines, [1, 2, 3, 4].into_iter().collect::<BTreeSet<u32>>(), "{label} / {sep_label}");
                assert_eq!(h.added_line_numbers, [2, 3].into_iter().collect::<BTreeSet<u32>>(), "{label} / {sep_label}");
            }
        }
    }

    /// (#2310 fix-loop B1) `/dev/null` must still drop the file in the
    /// prefixless dialects too — the guard is on the PATH, not on the
    /// `b/` prefix that used to gate the whole branch.
    #[test]
    fn dev_null_target_drops_file_in_every_dialect() {
        for header in ["+++ /dev/null", "+++ b//dev/null", "+++ /dev/null\t2026-09-05 08:45:23"] {
            let diff = format!("{header}\n@@ -1,2 +0,0 @@\n-gone\n-gone2\n");
            assert!(parse_diff(&diff).is_empty(), "{header}");
        }
    }

    /// (#2310 fix-loop B2, S3-3) A truly empty line inside a hunk body is
    /// a CONTEXT line whose content happens to be empty (tools and copy
    /// paths routinely strip the significant trailing space git emits).
    /// Dropping it shifted `new_ln` for every later line, which moved the
    /// old/new blocks and mis-classified findings as outside the diff.
    #[test]
    fn a_bare_blank_line_inside_a_hunk_is_context_like_a_space_prefixed_one() {
        let with_space = ["+++ b/x.ts", "@@ -1,4 +1,5 @@", " a", " ", " c", "+d", " e", ""].join("\n");
        let bare = ["+++ b/x.ts", "@@ -1,4 +1,5 @@", " a", "", " c", "+d", " e", ""].join("\n");
        let a = parse_diff(&with_space);
        let b = parse_diff(&bare);
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        let (ha, hb) = (&a[0].1[0], &b[0].1[0]);
        assert_eq!(hb.new_lines, ha.new_lines, "same line set as the space-prefixed form");
        assert_eq!(hb.new_lines, [1, 2, 3, 4, 5].into_iter().collect::<BTreeSet<u32>>());
        assert_eq!(hb.added_line_numbers, [4].into_iter().collect::<BTreeSet<u32>>(), "`+d` is line 4, not line 3");
        assert_eq!(hb.new_block, ha.new_block);
        assert_eq!(hb.old_block, ha.old_block);
        assert_eq!(hb.new_block, vec!["a", "", "c", "d", "e"]);
    }
}
