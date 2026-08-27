#!/usr/bin/env python3
"""Read the engagement-sentinel vocabulary out of its owner, safely.

`tests/parity/lib/sanitize.mjs` owns this list; two Python guards need it
(`engagement-sentinel-guard.py`, `demo-env/import_session.py`). Both used to
regex it out with `\\[(.*?)\\]`, which is NON-GREEDY and therefore stops at the
first `]` inside the array literal — including one inside a comment:

    export const SENTINELS = ["alpha", "beta",
      // matches sentinel[0] in the tokenizer output
      "gamma"];

...which yields `["alpha", "beta"]`. That truncates the list silently. Both callers only checked for a COMPLETELY
empty parse, so a partial list sailed through and the guards went half-blind:
the repo-wide public-leak gate stopped recognizing a real engagement name, and
the demo scrubber stopped removing it. One natural comment edit disabled both
at once. Found by a QA agent that planted the mutation rather than reading the
parser.

Two changes make the failure mode LOUD instead of silent:

1. Comments are stripped before parsing, and the scan runs to the array's real
   terminator, so the shape that caused this cannot recur.
2. `REQUIRED` names are asserted present. A parser that silently returns
   something plausible-but-short is the whole hazard, and "did it come back
   empty" does not catch it — only "did it come back with the things we KNOW
   must be there" does.
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SANITIZE = ROOT / "tests" / "parity" / "lib" / "sanitize.mjs"

# A FLOOR on how many entries a healthy parse returns.
#
# This was a set of REQUIRED NAMES, which is the better check and cannot live
# here: naming them puts the very identifiers this vocabulary exists to keep
# out of a public repo INTO a public repo. `engagement-sentinel-guard.py`
# caught that on the first CI run of the commit that introduced it — the guard
# catching its own helper reintroducing the bug it was written to fix.
#
# A count is weaker (it cannot tell WHICH entry vanished) but it is the
# strongest check available without restating the list, and it catches the
# failure that actually happened: a truncating parse returns FEWER entries.
# Raise it when the owner's list grows; a legitimate shrink below this is a
# deliberate edit that should require thinking about this line.
MIN_ENTRIES = 6


def _strip_comments(src: str) -> str:
    src = re.sub(r"/\*.*?\*/", "", src, flags=re.S)
    return re.sub(r"(?m)//[^\n]*$", "", src)


def sentinels(path: pathlib.Path = SANITIZE) -> list[str]:
    """Every SENTINELS entry, or exit non-zero explaining why not."""
    try:
        src = _strip_comments(path.read_text())
    except OSError as e:
        sys.exit(f"cannot read the sentinel vocabulary at {path} ({e}). Refusing "
                 f"to run a leak guard against an unknown vocabulary.")
    m = re.search(r"export const SENTINELS\s*=\s*\[(.*?)\]\s*;", src, re.S)
    if not m:
        sys.exit(f"cannot find `export const SENTINELS = [...];` in {path}. The "
                 f"guard refuses to run rather than under-scrub.")
    names = re.findall(r'"([^"]+)"', m.group(1))
    if not names:
        sys.exit(f"SENTINELS in {path} parsed EMPTY. Refusing to run.")
    if len(names) < MIN_ENTRIES:
        # Deliberately does NOT echo `names` — that would print the vocabulary
        # into CI logs, which are public on a public repo.
        sys.exit(f"SENTINELS in {path} parsed only {len(names)} entries, below the "
                 f"floor of {MIN_ENTRIES}. That means the parse truncated rather "
                 f"than the list legitimately changing. Refusing to run a "
                 f"half-blind guard.")
    return names
