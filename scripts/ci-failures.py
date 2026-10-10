#!/usr/bin/env python3
"""Print only what failed in a GitHub Actions run (#3134).

    scripts/ci-failures.py <run-id> [--repo owner/name]

Fetches the failed jobs' logs (`gh run view --log-failed`) and prints, per
job, each failing test's block: nextest's `FAIL [` line, the panic message
with its location and assertion lines, and the run's `Summary` line. A
compile error prints its `error[...]` block instead. A reader never has to
fetch a whole job log to learn which test failed and why.
"""
import argparse
import re
import subprocess
import sys

# `gh run view --log` lines are `<job>\t<step>\t<timestamp> <text>`.
LINE = re.compile(r"^(?P<job>[^\t]*)\t[^\t]*\t\S+Z (?P<text>.*)$")
START = re.compile(r"(\bFAIL \[|panicked at|^error(\[E\d+\])?:|^\s*Summary \[|^failures:$|test result: FAILED)")
ANSI = re.compile(r"(?:\x1b|\^\[)\[[0-9;]*m")
# A panic's message runs until a blank line or the next marker.
CONTEXT = 12


def failing_blocks(lines):
    """Yield (job, [text lines]) for each block worth printing, in order."""
    current_job, block, budget = None, [], 0
    for raw in lines:
        m = LINE.match(raw)
        job, text = (m.group("job"), m.group("text")) if m else (current_job, raw)
        text = ANSI.sub("", text)
        if job != current_job and block:
            yield current_job, block
            block, budget = [], 0
        current_job = job
        if START.search(text):
            block.append(text)
            budget = CONTEXT if ("panicked at" in text or text.startswith("error")) else 0
        elif budget > 0:
            if not text.strip():
                budget = 0
            else:
                block.append(text)
                budget -= 1
    if block:
        yield current_job, block


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("run_id")
    ap.add_argument("--repo", default="kstrat2001/darkmux")
    a = ap.parse_args()
    out = subprocess.run(
        ["gh", "run", "view", a.run_id, "--repo", a.repo, "--log-failed"],
        capture_output=True, text=True,
    )
    if out.returncode != 0:
        sys.exit(f"gh run view {a.run_id} --log-failed failed: {out.stderr.strip()}")
    printed = False
    for job, block in failing_blocks(out.stdout.splitlines()):
        print(f"== {job}")
        print("\n".join(block))
        print()
        printed = True
    if not printed:
        print(f"run {a.run_id}: no failing step output (it may have been cancelled, or still running)")


if __name__ == "__main__":
    main()
