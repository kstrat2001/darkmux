//! Unified-diff parsing — re-exports the canonical parser (#2310 P4b move).
//!
//! **Moved to `darkmux-crew`'s `diff` module.** `deliver_github_review`
//! (a darkmux-crew step kind) needed the SAME parser this bundler already
//! had, and darkmux-lab depends on darkmux-crew (never the reverse) — so
//! the parser lives there now and this module re-exports it, keeping every
//! existing call site in this crate (`bundle::source`, `bundle::mod`)
//! untouched. See `darkmux_crew::diff`'s own module doc for the full
//! reasoning and the incident that prompted the move.
//!
//! The tests below stay here deliberately (not deleted, not moved) — they
//! exercise THIS re-export, i.e. that `darkmux-lab`'s own import path still
//! resolves to working behavior. `darkmux_crew::diff`'s own module carries
//! its own copy of this coverage beside the real implementation.

pub use darkmux_crew::diff::{parse_diff, Hunk};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

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
}
