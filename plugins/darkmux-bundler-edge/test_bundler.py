#!/usr/bin/env python3
"""Zero-dependency contract tests for darkmux-bundler-edge.

Invokes the SCRIPT itself as a subprocess — the actual `--bundler`
contract surface a caller (darkmux's review pipeline, or any other
`--bundler`-invoking tool) uses — rather than importing its functions.
Stdlib `unittest` only, no third-party test runner.

Run directly:

    python3 plugins/darkmux-bundler-edge/test_bundler.py
"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "darkmux-bundler-edge"

# The final (post-diff) content of the fixture template. Small enough
# (<=400 lines) that the bundler emits a whole-file span rather than
# hunk windows.
WIDGET_EDGE_CONTENT = """<div>
  {{ props.title }}
  {{ props.subtitle }}
  {{-- a comment --}}
  @!component('components/panel')
  {{-- Usage: @include('widget') --}}
</div>
"""

# A diff adding: a genuine interpolation (props.subtitle), an Edge
# comment (must NOT count as an interpolation), a component reference
# to an external template (components/panel), and a self-reference
# inside a comment (must be excluded from the manifest since it names
# the template's own stem, "widget").
WIDGET_DIFF = """diff --git a/templates/widget.edge b/templates/widget.edge
index 0000000..1111111 100644
--- a/templates/widget.edge
+++ b/templates/widget.edge
@@ -1,2 +1,7 @@
 <div>
   {{ props.title }}
+  {{ props.subtitle }}
+  {{-- a comment --}}
+  @!component('components/panel')
+  {{-- Usage: @include('widget') --}}
 </div>
"""

# A diff that touches only a non-Edge file — must trigger loud failure.
TS_ONLY_DIFF = """diff --git a/src/foo.ts b/src/foo.ts
index 0000000..1111111 100644
--- a/src/foo.ts
+++ b/src/foo.ts
@@ -1,1 +1,1 @@
-const x = 1;
+const x = 2;
"""


def run_bundler(diff_text, worktree=None, tmpdir=None):
    """Write diff_text to a temp file and invoke the script as a
    subprocess, exactly the contract surface a real caller uses."""
    diff_path = Path(tmpdir) / "input.diff"
    diff_path.write_text(diff_text, encoding="utf-8")
    argv = [sys.executable, str(SCRIPT), "--diff", str(diff_path)]
    if worktree is not None:
        argv += ["--worktree", str(worktree)]
    return subprocess.run(argv, capture_output=True, text=True)


class DarkmuxBundlerEdgeTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(SCRIPT.is_file(), f"script not found at {SCRIPT}")
        self._tmp = tempfile.TemporaryDirectory()
        self.tmpdir = Path(self._tmp.name)
        self.worktree = self.tmpdir / "worktree"
        (self.worktree / "templates").mkdir(parents=True)
        (self.worktree / "templates" / "widget.edge").write_text(
            WIDGET_EDGE_CONTENT, encoding="utf-8"
        )

    def tearDown(self):
        self._tmp.cleanup()

    def _run_success(self):
        result = run_bundler(WIDGET_DIFF, worktree=self.worktree, tmpdir=self.tmpdir)
        self.assertEqual(
            result.returncode, 0, f"expected success, got stderr: {result.stderr!r}"
        )
        return result

    # -- bundle shape / id / whole-file span --------------------------

    def test_bundle_emitted_with_correct_id_and_whole_file_span(self):
        result = self._run_success()
        payload = json.loads(result.stdout)
        self.assertEqual(len(payload["bundles"]), 1)
        bundle = payload["bundles"][0]

        self.assertEqual(bundle["id"], "widget@templates/widget.edge")
        self.assertEqual(bundle["fact_family"], "differential")

        expected_lines = len(WIDGET_EDGE_CONTENT.splitlines())
        self.assertEqual(
            bundle["code"],
            [{"path": "templates/widget.edge", "start": 1, "end": expected_lines}],
        )
        # Whole-file span used (template is well under 400 lines) — the
        # contract's `truncated` field must not appear at all.
        self.assertNotIn("truncated", bundle)

    # -- facts correctness ---------------------------------------------

    def test_added_interpolation_is_reported(self):
        result = self._run_success()
        payload = json.loads(result.stdout)
        facts = payload["bundles"][0]["facts"]

        interp_facts = [f for f in facts if f.startswith("interpolation(s) added")]
        self.assertEqual(len(interp_facts), 1, f"facts were: {facts}")
        self.assertIn("{{ props.subtitle }}", interp_facts[0])

    def test_added_comment_is_not_counted_as_interpolation(self):
        result = self._run_success()
        payload = json.loads(result.stdout)
        facts = payload["bundles"][0]["facts"]

        for fact in facts:
            if fact.startswith("interpolation"):
                self.assertNotIn("--", fact, f"comment leaked into interpolation fact: {fact}")
                self.assertNotIn("a comment", fact)

    # -- manifest --------------------------------------------------------

    def test_manifest_includes_external_component_reference(self):
        result = self._run_success()
        payload = json.loads(result.stdout)
        manifest = payload["bundles"][0].get("manifest", [])
        self.assertIn("components/panel", manifest)

    def test_manifest_excludes_self_reference(self):
        result = self._run_success()
        payload = json.loads(result.stdout)
        manifest = payload["bundles"][0].get("manifest", [])
        self.assertNotIn("widget", manifest)

    # -- loud failure ------------------------------------------------------

    def test_non_edge_diff_fails_loudly(self):
        result = run_bundler(TS_ONLY_DIFF, worktree=None, tmpdir=self.tmpdir)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertTrue(result.stderr.strip(), "expected a clear stderr message")
        self.assertIn("edge", result.stderr.lower())

    # -- stdout hygiene ----------------------------------------------------

    def test_stdout_is_valid_json_and_only_json(self):
        result = self._run_success()
        # Exactly one line: the json.dumps(...) payload plus its trailing
        # newline from print(). No banner, no debug prints, nothing else.
        self.assertEqual(result.stdout.count("\n"), 1)
        payload = json.loads(result.stdout)
        self.assertIn("bundles", payload)


if __name__ == "__main__":
    unittest.main()
