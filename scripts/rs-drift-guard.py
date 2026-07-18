#!/usr/bin/env python3
# rs-drift-guard.py (#1469) — the .rs string-literal half of the retired-terms
# drift guard.
#
# The docs-drift guard in .github/workflows/ci.yml catches `darkmux <retired-verb>`
# spellings in DOCS. It anchors on `darkmux <verb>` and scans doc surfaces only, so
# it structurally MISSES a retired verb baked into a Rust user-facing string —
# help text, an error/hint message, a `println!`/`format!` banner. That blind spot
# recurred ~5 times across the 2.0 arc (e.g. #1468 shipped a stale
# `mission ship --merge` banner). This script closes it: it scans `.rs` files for
# the curated retired-verb list appearing INSIDE string literals.
#
# Match discipline (mirrors the docs guard's "anchor, don't flag incidental words"):
#   * single-word retired verbs (swap, crew, fleet, ...) are matched ONLY in the
#     `darkmux <verb>` invocation form — never bare, so the English words "swap" /
#     "ship" / "phase" never trip it;
#   * genuinely command-shaped multi-word forms (`mission ship`, `crew sync`,
#     `ship --merge`, ...) are matched anywhere inside a string literal, since they
#     read as a command a user would be told to run.
# Only text INSIDE a double-quoted literal counts; pure `//` comment lines are
# skipped (a comment explaining a retirement is legitimate).
#
# False positives (a test asserting the retirement, a noun use like the
# "darkmux skills freshness" doctor-check name) are allowlisted with an inline
# marker on the offending line OR the line immediately above it:
#   // drift-guard:allow <verb> — <reason>
#
# Dependency-light: Python 3 stdlib only. Run locally with `python3 scripts/rs-drift-guard.py`.

import os
import re
import sys

# ---- The retired-verb list — ONE place the guard reads (#1469) ----
# Command-shaped multi-word forms: matched anywhere inside a string literal
# because the whole phrase reads as a command. Retired across the 2.0 arc
# (#1405/#1426/#1463/#1465 and the review restructure).
RETIRED_COMMAND_PHRASES = [
    "crew sync",         # openclaw shell-out reconcile — removed (#1405)
    "crew dispatch",     # -> `darkmux dispatch <role>` (#1426)
    "mission run",       # -> `mission launch coder-phase` (#1426 ship-4)
    "mission ship",      # retired; the frontier does git/gh, then `mission finalize` (#1463)
    "ship --merge",      # the stale-banner shape #1468 shipped
    "optimize scaffold", # retired optimize/scaffold verb
    "doctor --fix",      # doctor is read-only; no --fix
    "lab review-bench",  # -> `lab eval <role>` (#1465)
    "pr-review run",     # -> `mission launch review` (#1426)
]

# Single-word retired verbs: matched ONLY in the `darkmux <verb>` form so common
# English words never trip. (`pr-review\b` deliberately excludes the live
# `pr-reviewer` role id — no word boundary after `pr-review` inside `pr-reviewer`.)
RETIRED_SINGLE_VERBS = [
    "swap",            # residency is internal to gestalt now (#1426 phase 3)
    "crew",            # the crew family dissolved entirely (#1426 ship-2)
    "fleet",           # -> `machine {list,add,remove}` (#1426)
    "recommendations", # retired
    "skills",          # -> `darkmux init` (#1426 phase 2)
    "phase",           # the phase family retired entirely (#1463)
    "sprint",          # ephemeral sprint vocab retired
    "pr-review",       # -> `mission launch review` (#1426)
]

ALLOW_MARKER = "drift-guard:allow"

ROOTS = ["src", "runtime/src"]  # plus crates/*/src, discovered below

_command_res = [re.compile(r"\b" + re.escape(p) + r"\b") for p in RETIRED_COMMAND_PHRASES]
_single_re = re.compile(r"darkmux (" + "|".join(RETIRED_SINGLE_VERBS) + r")\b")
_string_re = re.compile(r'"([^"\\]|\\.)*"')


def crate_src_roots():
    roots = list(ROOTS)
    crates = "crates"
    if os.path.isdir(crates):
        for d in sorted(os.listdir(crates)):
            s = os.path.join(crates, d, "src")
            if os.path.isdir(s):
                roots.append(s)
    return roots


def quoted_spans(line):
    return [(m.start(), m.end()) for m in _string_re.finditer(line)]


def find_hit(line):
    """Return (matched_text, col) for the first retired verb inside a string
    literal on this line, or None."""
    stripped = line.lstrip()
    if stripped.startswith("//"):
        return None
    spans = quoted_spans(line)
    if not spans:
        return None
    candidates = []
    for cre in _command_res:
        m = cre.search(line)
        if m:
            candidates.append((m.start(), m.group(0)))
    m = _single_re.search(line)
    if m:
        candidates.append((m.start(), m.group(0)))
    for start, text in sorted(candidates):
        if any(a <= start < b for a, b in spans):
            return (text, start)
    return None


def scan():
    findings = []
    for root in crate_src_roots():
        for dirpath, _, filenames in os.walk(root):
            for fn in filenames:
                if not fn.endswith(".rs"):
                    continue
                path = os.path.join(dirpath, fn)
                prev = ""
                with open(path, encoding="utf-8", errors="replace") as fh:
                    for lineno, line in enumerate(fh, 1):
                        hit = find_hit(line)
                        if hit is None:
                            prev = line
                            continue
                        text, _ = hit
                        # Allowlisted on this line or the line immediately above.
                        if ALLOW_MARKER in line or ALLOW_MARKER in prev:
                            prev = line
                            continue
                        findings.append((path, lineno, text, line.rstrip()))
                        prev = line
    return findings


def main():
    findings = scan()
    if findings:
        print(
            "::error::Retired verb in a .rs string literal (#1469) — a help/error/"
            "hint/banner string still names a verb retired on the 2.0 arc. Fix the "
            "string to the current verb, or, if this is a legitimate reference (a "
            "test asserting the retirement, a noun use), add an inline "
            f"`// {ALLOW_MARKER} <verb> — <reason>` marker on the line or the line above."
        )
        for path, lineno, text, line in findings:
            print(f"{path}:{lineno}: [{text}] {line.strip()}")
        return 1
    print("rs drift guard passed: no retired verbs in .rs string literals")
    return 0


if __name__ == "__main__":
    sys.exit(main())
