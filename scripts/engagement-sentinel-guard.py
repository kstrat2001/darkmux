#!/usr/bin/env python3
"""Block engagement-private identifiers from entering this PUBLIC repo.

darkmux is public OSS developed alongside private client work. Names that are
ordinary in a work session — an employer, a private repo, a hosted endpoint, a
tracker key — become a disclosure the moment they land here, and git history
makes the landing permanent. This guard turns "remember not to paste that" into
a blocked PR.

THE SENTINEL LIST IS NOT DEFINED HERE. It is read from
`tests/parity/lib/sanitize.mjs`, which already owns it for the parity-golden
scrubber. Two lists that must agree by hand is how the first leak happened; one
list, read twice, cannot drift.

Allowlisted files legitimately CONTAIN sentinels because their job is to name
what gets stripped. Everything else is a finding.
"""

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SANITIZER = ROOT / "tests/parity/lib/sanitize.mjs"

# Files whose PURPOSE is to name the sentinels. Keep this set tiny.
ALLOWLIST = {
    "tests/parity/lib/sanitize.mjs",
    "tests/parity/README.md",
    "scripts/engagement-sentinel-guard.py",
}

# Tracker keys are a shape, not a word, so they are matched separately. The
# neutral form used throughout this repo is `SAMPLE-1234`.
#
# CASE-INSENSITIVE deliberately: the scrubber's own comment records that both
# `SYS-2590` and `sys_2609` occur in harvested text. A case-SENSITIVE version of
# this pattern passed a planted `sys_2609` during red-proving, which is the whole
# reason the lowercase half is spelled out here rather than assumed.
# The lookbehind is load-bearing: `\b` alone matches between the hyphen and the
# `s` of a crate path like `dirs-sys-0.4.1`, which red-proving surfaced as 15
# false positives. A tracker key is a STANDALONE token, never a suffix.
TICKET_RE = re.compile(r"(?<![-_\w])(?:SYS|OFAL|DEVOPS|IR)[-_]\d+\b", re.I)


def load_sentinels() -> list[str]:
    """Parse the `SENTINELS` array out of the scrubber."""
    src = SANITIZER.read_text(encoding="utf-8")
    m = re.search(r"export const SENTINELS\s*=\s*\[(.*?)\]", src, re.S)
    if not m:
        sys.exit(
            f"guard is broken, not the tree: could not find `export const SENTINELS` "
            f"in {SANITIZER.relative_to(ROOT)} — did it get renamed?"
        )
    words = re.findall(r"[\"']([^\"']+)[\"']", m.group(1))
    # Bare tracker prefixes ("SYS-", "SYS_") are covered by TICKET_RE, which is
    # anchored on digits and so does not fire on unrelated prose.
    return [w for w in words if not w.rstrip("-_").isupper() or len(w.rstrip("-_")) > 4]


def tracked_files() -> list[str]:
    out = subprocess.run(
        ["git", "ls-files"], cwd=ROOT, capture_output=True, text=True, check=True
    )
    return [p for p in out.stdout.splitlines() if p]


def main() -> int:
    sentinels = load_sentinels()
    word_re = re.compile("|".join(re.escape(s) for s in sentinels), re.I)

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
            if word_re.search(line) or TICKET_RE.search(line):
                findings.append((rel, n, line.strip()[:120]))

    if findings:
        print("ENGAGEMENT SENTINEL FOUND — this repo is PUBLIC.\n")
        for rel, n, line in findings:
            print(f"  {rel}:{n}: {line}")
        print(
            f"\n{len(findings)} occurrence(s). Replace with a neutral placeholder "
            f"(this repo uses `example-*` for hosts and `SAMPLE-1234` for tracker "
            f"keys), or — only if the file's PURPOSE is to name sentinels — add it "
            f"to ALLOWLIST in scripts/engagement-sentinel-guard.py.\n"
            f"Sentinel list is owned by tests/parity/lib/sanitize.mjs."
        )
        return 1

    print(f"engagement sentinel guard passed: {len(sentinels)} sentinels, 0 occurrences")
    return 0


if __name__ == "__main__":
    sys.exit(main())
