//! Unified-diff parsing — a straight port of the Python reference's
//! `parse_diff` (`bundler.py`, Phase A). Splits a multi-file unified diff
//! into per-file hunks, each carrying the added/removed/old/new line sets
//! the scanner and fact-builders need.
//!
//! No `regex` crate (workspace dep discipline) — the two line shapes this
//! parses (`+++ b/<path>` and `@@ -a,b +c,d @@`) are simple enough for
//! hand-rolled prefix/token parsing.

use std::collections::BTreeSet;

/// One `@@ ... @@` hunk within a file's diff.
#[derive(Debug, Clone, Default)]
pub struct Hunk {
    /// The hunk's starting line number in the NEW file (1-indexed), from
    /// the `@@ -a,b +c,d @@` header's `+c`.
    pub new_start: u32,
    /// Every line number (1-indexed, in the NEW file) touched by this
    /// hunk — added lines AND unchanged context lines (matches the
    /// reference: context lines advance `new_ln` and land in
    /// `new_lines` too, since a changed function can be located via a
    /// context line inside it just as well as an added one).
    pub new_lines: BTreeSet<u32>,
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
        if let Some(rest) = ln.strip_prefix("+++ b/") {
            flush(&mut files, &path, &mut cur);
            path = if rest == "/dev/null" {
                None
            } else {
                Some(rest.to_string())
            };
            cur = None;
            continue;
        }
        if let Some(start) = parse_hunk_header(ln) {
            flush(&mut files, &path, &mut cur);
            cur = Some(Hunk {
                new_start: start,
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
        }
        // Other lines (e.g. `\ No newline at end of file`, the `---
        // a/<path>` line, `diff --git` headers) carry no line-content
        // signal — ignored, matching the reference.
    }
    flush(&mut files, &path, &mut cur);
    files
}

/// Parse `@@ -a[,b] +c[,d] @@...` and return `c` (the new-file start
/// line), or `None` if `ln` isn't a hunk header. Hand-rolled equivalent
/// of `re.match(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@", ln)`.
fn parse_hunk_header(ln: &str) -> Option<u32> {
    let rest = ln.strip_prefix("@@ -")?;
    // Skip the `-a[,b]` side entirely — we only need the `+c` number.
    let space = rest.find(' ')?;
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
    plus_digits[..end].parse::<u32>().ok()
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
        assert_eq!(h.added, vec!["new line", "added line"]);
        assert_eq!(h.removed, vec!["old line"]);
        // new_lines: line one(1), new line(2), added line(3), line four(4)
        assert_eq!(
            h.new_lines,
            [1, 2, 3, 4].into_iter().collect::<BTreeSet<u32>>()
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
}
