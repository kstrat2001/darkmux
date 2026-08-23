#!/usr/bin/env python3
"""Block engagement-private identifiers from entering this PUBLIC repo.

darkmux is public OSS developed alongside private client work. Names that are
ordinary in a work session — an employer, a private repo, a hosted endpoint, a
tracker key, a foreign source-tree path — become a disclosure the moment they
land here, and git history makes the landing permanent. This guard turns
"remember not to paste that" into a blocked PR.

THE VOCABULARY IS NOT DEFINED HERE. It is read from
`tests/parity/lib/sanitize.mjs`, which already owns it for the parity-golden
scrubber. Specifically it reads `CANARIES`, the BROADER verification list, not
`SENTINELS`. That distinction is load-bearing and was a review finding: the
scrubber's own module doc records that its first word-scanning version missed
real leaks — a live client source-tree path among them — precisely because they
spell no sentinel word. `CANARIES` exists for that class; reading `SENTINELS`
would inherit the narrower blind spot.

WHAT THIS GUARD CANNOT DO, stated so nobody mistakes a pass for proof: it is a
word scanner, and `sanitize.mjs` is explicit that word scanning is the wrong
model for content-bearing leaks. A test fixture reproducing real client code
carries no sentinel word and will pass here. Field-policy review of new
fixtures is still a human job.
"""

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SANITIZER = ROOT / "tests/parity/lib/sanitize.mjs"

# Files whose PURPOSE is to name the vocabulary. Keep this set tiny, and note
# what is NOT here: this script. An earlier cut allowlisted itself, which made
# its own comments a permanent blind spot — and it had already spelled two real
# tracker keys into them. A guard that cannot see itself is not a guard.
ALLOWLIST = {
    "tests/parity/lib/sanitize.mjs",
    "tests/parity/README.md",
}

# Canary words that are ALSO ordinary darkmux vocabulary. Each needs a reason;
# an entry without one is a bug. Verified 2026-08-23 against every occurrence
# in the tree.
CANARY_EXCEPTIONS = {
    # darkmux's own operator-consent gating, plus DISCLAIMER.md's discussion of
    # attorney-client privilege. Nothing client-derived.
    "consent": "darkmux's own operator-consent gate + DISCLAIMER.md legal prose",
    # The retired `admin` role family (now `utility`) and its rejection tests.
    "admin_": "darkmux's own retired admin role family and its validation tests",
}

# Tracker keys are a shape, not a word. Two alternatives on purpose:
#
#   * case-INSENSITIVE with a strict lookbehind — catches the lowercase
#     underscore form, and the
#     `(?<![-_\w])` keeps it off crate paths like `dirs-sys-0.4.1`, which
#     produced 15 false positives when it was absent.
#   * case-SENSITIVE uppercase with a LOOSER lookbehind — catches the shapes
#     the strict one walks past, which are the common accidental ones: a branch
#     or worktree name pasted into a comment, where the key sits after a
#     hyphen or underscore instead of at a token boundary.
#     Restricting it to uppercase is what keeps lowercase crate suffixes out.
#
# The prefix set is a second hand-maintained list, and unlike the word
# vocabulary it has no upstream owner to read from. Add to both alternatives.
_PREFIXES = "SYS|OFAL|DEVOPS|IR"
TICKET_RES = [
    re.compile(rf"(?<![-_\w])(?:{_PREFIXES})[-_]\d+\b", re.I),
    re.compile(rf"(?<![A-Za-z0-9])(?:{_PREFIXES})[-_]\d+\b"),
]

# Bare tracker prefixes in the upstream list — delegated to TICKET_RES above,
# which is anchored on digits and so does not fire on unrelated prose. Named
# explicitly rather than inferred: an earlier cut used a case/length heuristic
# that silently discarded any short all-caps entry a maintainer might add.
DELEGATED_TO_TICKET_RE = {"SYS-", "SYS_"}


def load_canaries() -> tuple[list[str], list[str]]:
    """Parse the `CANARIES` array out of the scrubber.

    `CANARIES` begins with `...SENTINELS`, so both lists are pulled and merged.
    """
    src = SANITIZER.read_text(encoding="utf-8")

    def array(name: str) -> list[str]:
        m = re.search(rf"export const {name}\s*=\s*\[(.*?)\]", src, re.S)
        if not m:
            sys.exit(
                f"guard is broken, not the tree: could not find "
                f"`export const {name}` in {SANITIZER.relative_to(ROOT)} — "
                f"did it get renamed?"
            )
        return re.findall(r"[\"']([^\"']+)[\"']", m.group(1))

    words = array("SENTINELS") + array("CANARIES")
    seen, out, delegated = set(), [], []
    for w in words:
        k = w.lower()
        if k in seen:
            continue
        seen.add(k)
        if w in DELEGATED_TO_TICKET_RE:
            delegated.append(w)
        elif k in CANARY_EXCEPTIONS:
            continue
        else:
            out.append(w)
    return out, delegated


def tracked_files() -> list[str]:
    out = subprocess.run(
        ["git", "ls-files"], cwd=ROOT, capture_output=True, text=True, check=True
    )
    return [p for p in out.stdout.splitlines() if p]


def main() -> int:
    canaries, delegated = load_canaries()
    if not canaries:
        sys.exit("guard is broken, not the tree: canary list resolved to empty")
    word_re = re.compile("|".join(re.escape(s) for s in canaries), re.I)

    findings: list[tuple[str, int, str]] = []
    for rel in tracked_files():
        if rel in ALLOWLIST:
            continue
        path = ROOT / rel
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, FileNotFoundError, IsADirectoryError):
            continue  # binary or gone — nothing to read
        for n, line in enumerate(text.splitlines(), 1):
            if word_re.search(line) or any(r.search(line) for r in TICKET_RES):
                findings.append((rel, n, line.strip()[:120]))

    if delegated:
        print(f"note: {len(delegated)} bare tracker prefix(es) delegated to TICKET_RES: {delegated}")
    if CANARY_EXCEPTIONS:
        print(f"note: {len(CANARY_EXCEPTIONS)} canary word(s) excepted as darkmux vocabulary: {sorted(CANARY_EXCEPTIONS)}")

    if findings:
        print("\nENGAGEMENT SENTINEL FOUND — this repo is PUBLIC.\n")
        for rel, n, line in findings:
            print(f"  {rel}:{n}: {line}")
        print(
            f"\n{len(findings)} occurrence(s). Replace with a neutral placeholder "
            f"(this repo uses `example-*` for hosts and a `SAMPLE-` prefix for "
            f"tracker keys), or — only if the file's PURPOSE is to name the "
            f"vocabulary — add it to ALLOWLIST. A word that is genuine darkmux "
            f"vocabulary goes in CANARY_EXCEPTIONS with a reason.\n"
            f"Vocabulary is owned by tests/parity/lib/sanitize.mjs (CANARIES)."
        )
        return 1

    print(f"engagement sentinel guard passed: {len(canaries)} canaries, 0 occurrences")
    return 0


if __name__ == "__main__":
    sys.exit(main())
