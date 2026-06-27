#!/usr/bin/env python3
"""Unit tests for pr-review-post.py's anchor resolver (#1053 quote-resolve).

The reviewer quotes the line it's commenting on (`anchor`); the harness resolves
that quote to a new-side line deterministically. These pin that resolution +
its edge cases. Run: `python3 .github/scripts/test_pr_review_post.py`.
(Not in the Rust CI matrix; the resolver is also exercised end-to-end by the
self-review dogfood. Kept self-contained — stdlib unittest only.)
"""
import importlib.util
import os
import tempfile
import unittest

# pr-review-post.py reads RUNNER_TEMP at import time — set it before loading.
os.environ.setdefault("RUNNER_TEMP", tempfile.gettempdir())
_spec = importlib.util.spec_from_file_location(
    "pr_review_post", os.path.join(os.path.dirname(__file__), "pr-review-post.py")
)
m = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(m)

DIFF = """\
diff --git a/src/x.ts b/src/x.ts
--- a/src/x.ts
+++ b/src/x.ts
@@ -1,3 +1,4 @@
 const a = 1;
+const b = 2;
 const c = 3;
-const d = 4;
+const d = 5;
"""


class NewSideIndex(unittest.TestCase):
    def setUp(self):
        self.idx = m.new_side_index(DIFF)

    def test_indexes_added_and_context_lines(self):
        self.assertEqual(self.idx["src/x.ts"]["const b = 2;"], [2])  # added
        self.assertEqual(self.idx["src/x.ts"]["const a = 1;"], [1])  # context
        self.assertEqual(self.idx["src/x.ts"]["const d = 5;"], [4])  # added (post -)

    def test_excludes_removed_lines(self):
        self.assertNotIn("const d = 4;", self.idx["src/x.ts"])  # the removed - line


class ResolveAnchor(unittest.TestCase):
    def setUp(self):
        self.idx = m.new_side_index(DIFF)

    def r(self, path, anchor):
        return m.resolve_anchor(path, anchor, self.idx)

    def test_exact_match(self):
        self.assertEqual(self.r("src/x.ts", "const b = 2;"), 2)

    def test_path_prefix_normalized(self):
        self.assertEqual(self.r("b/src/x.ts", "const b = 2;"), 2)

    def test_strips_leading_diff_marker(self):
        self.assertEqual(self.r("src/x.ts", "+const b = 2;"), 2)

    def test_whitespace_tolerant(self):
        self.assertEqual(self.r("src/x.ts", "   const b = 2;   "), 2)

    def test_multiline_anchor_uses_first_line(self):
        self.assertEqual(self.r("src/x.ts", "const b = 2;\nconst c = 3;"), 2)

    def test_null_anchor_is_file_level(self):
        self.assertIsNone(self.r("src/x.ts", None))

    def test_empty_anchor(self):
        self.assertIsNone(self.r("src/x.ts", "   "))

    def test_no_match(self):
        self.assertIsNone(self.r("src/x.ts", "const z = 9;"))

    def test_removed_line_does_not_resolve(self):
        self.assertIsNone(self.r("src/x.ts", "const d = 4;"))

    def test_ambiguous_duplicate_text(self):
        dup = (
            "diff --git a/y.ts b/y.ts\n--- a/y.ts\n+++ b/y.ts\n"
            "@@ -1,0 +1,2 @@\n+  return;\n+  return;\n"
        )
        idx = m.new_side_index(dup)
        self.assertEqual(idx["y.ts"]["return;"], [1, 2])
        self.assertIsNone(m.resolve_anchor("y.ts", "return;", idx))  # 2 hits → general


class MarkerContentLines(unittest.TestCase):
    """A line whose CONTENT begins with -/+ (a markdown bullet, a diff snippet
    in docs — common in darkmux's own docs) must resolve as-is, not be
    double-stripped. new_side_index already removed the diff marker, so the
    stored key keeps the content's leading -/+; resolve tries the quote as-is
    before stripping (#1053 QA CONSIDER-1)."""

    DIFF = (
        "diff --git a/doc.md b/doc.md\n--- a/doc.md\n+++ b/doc.md\n"
        "@@ -0,0 +1,2 @@\n+- a bullet item\n++count\n"
    )

    def setUp(self):
        self.idx = m.new_side_index(self.DIFF)

    def test_content_starting_with_dash_resolves_as_is(self):
        self.assertEqual(m.resolve_anchor("doc.md", "- a bullet item", self.idx), 1)

    def test_content_starting_with_plus_resolves_as_is(self):
        self.assertEqual(m.resolve_anchor("doc.md", "+count", self.idx), 2)

    def test_marker_left_on_still_falls_back_to_strip(self):
        idx = m.new_side_index("diff --git a/z b/z\n+++ b/z\n@@ -0,0 +1 @@\n+const x = 1;\n")
        # model left the diff marker on a normal line → strip fallback resolves it
        self.assertEqual(m.resolve_anchor("z", "+const x = 1;", idx), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
