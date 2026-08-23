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

# ── Network identifiers ────────────────────────────────────────────────────
#
# A word scanner cannot see these: an address and a MagicDNS name spell no
# sentinel. They reached this repo exactly that way — an operator tailnet
# address in 21 places including `machine add` and `config set` help text, and
# a MagicDNS hostname in a committed doctor golden, sitting beside an IP the
# parity scrubber HAD rewritten. Neither is publicly routable and neither is a
# credential, so this is topology disclosure rather than a breach — but both
# are durable, correlatable, and permanent once history has them.
#
# The example convention this repo already uses, and which these patterns
# deliberately permit:
#   * addresses  -> 100.64.x.x  (the first /16 of the CGNAT range)
#   * MagicDNS   -> a tailnet component listed in EXAMPLE_TAILNETS
# Anything else inside CGNAT, or any other tailnet name, is presumed real.
CGNAT_RE = re.compile(
    r"(?<![\d.])100\.(?:6[5-9]|[7-9]\d|1[01]\d|12[0-7])\.\d{1,3}\.\d{1,3}(?![\d.])"
)
EXAMPLE_TAILNETS = {"tailnet", "tailnet-example", "your-tailnet", "example"}
# `sanitize.mjs` rewrites every MagicDNS name it finds into
# `host-<8 hex>.tailnet-<10 hex>.ts.net`, and the goldens that output lands in
# are tracked — so the guard scans the scrubber's own sanitized product. That
# product is not a word in EXAMPLE_TAILNETS, so without this the guard flags
# CORRECTLY sanitized content the first time a corpus re-record carries a
# MagicDNS hostname, and the obvious remedy a maintainer reaches for under a
# red build is loosening the guard — reopening the hole it exists to close.
#
# The IP half already avoids exactly this by forcing its synthetic octet above
# CGNAT (see `syntheticIpv4`). The two halves shipped asymmetric: one defended,
# one not. This is the missing half. Shape-matched rather than word-listed so
# it recognizes the scrubber's output and nothing looser.
SYNTHETIC_TAILNET_RE = re.compile(r"^tailnet-[0-9a-f]{10}$")
# The machine label is OPTIONAL, matching `sanitize.mjs`. `tailscale status
# --json` reports the tailnet as a bare `MagicDNSSuffix`
# (`<tailnet>.ts.net`), and the tailnet is the durable half — a machine can
# be renamed, a tailnet name is the same string everywhere it appears.
# Requiring two labels let exactly that form past this guard.
MAGICDNS_RE = re.compile(r"\b(?:[a-z0-9-]+\.)?([a-z0-9-]+)\.ts\.net\b", re.I)


def network_identifier_hits(line: str) -> bool:
    """True when the line carries a presumed-real tailnet address or hostname."""
    if CGNAT_RE.search(line):
        return True
    return any(not _is_permitted_tailnet(m.group(1)) for m in MAGICDNS_RE.finditer(line))


def _is_permitted_tailnet(tailnet: str) -> bool:
    """The documented example names, plus the scrubber's own synthetic form."""
    t = tailnet.lower()
    return t in EXAMPLE_TAILNETS or SYNTHETIC_TAILNET_RE.match(t) is not None


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
            if (
                word_re.search(line)
                or any(r.search(line) for r in TICKET_RES)
                or network_identifier_hits(line)
            ):
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


# Cases the network matcher must get right, as (line, should_flag, why).
#
# These exist because the two halves of this matcher shipped ASYMMETRIC: the IP
# side pushed its synthetic value out of the flagged range, the MagicDNS side
# did not, and nothing failed — the collision stayed LATENT until a corpus
# re-record happened to carry a MagicDNS hostname. A guard whose false-positive
# behavior is first discovered by a red build gets loosened, not fixed.
#
# Every "must be caught" value is INVENTED, and ASSEMBLED rather than written
# whole. Two separate reasons, both load-bearing:
#   * invented, because a fixture proving the guard catches real identifiers
#     must never contain one — this file is tracked in a PUBLIC repo.
#   * assembled, because this file is scanned by the guard itself (deliberately;
#     an allowlisted guard is a permanent blind spot, and it has already spelled
#     real tracker keys into its own comments once). A complete identifier
#     literal here would be flagged by the very matcher it is testing. Assembly
#     keeps the SOURCE line unmatchable while the value handed to the matcher is
#     shaped exactly like the real thing.
_TS_SUFFIX = "ts" + ".net"
_INVENTED_TAILNET = "tailfeed99"
_INVENTED_CGNAT = "100." + "99" + ".1.2"

SELF_TEST_CASES = [
    # The scrubber's own sanitized output must never be flagged.
    (f"url: host-a1b2c3d4.tailnet-0f1e2d3c4b.{_TS_SUFFIX}", False, "scrubber's synthetic MagicDNS"),
    ("addr: 100.201.14.7", False, "scrubber's synthetic IP — second octet above CGNAT"),
    # The documented example convention must never be flagged.
    (f"url: laptop.tailnet-example.{_TS_SUFFIX}", False, "documented example tailnet"),
    # Real-SHAPED identifiers must still be caught, both halves.
    (f"url: somebox.{_INVENTED_TAILNET}.{_TS_SUFFIX}", True, "a real-shaped tailnet"),
    (f"addr: {_INVENTED_CGNAT}", True, "an address inside CGNAT"),
    (f"suffix: {_INVENTED_TAILNET}.{_TS_SUFFIX}", True, "bare MagicDNSSuffix, no machine label"),
    # The DISCRIMINATOR is the 10-hex, not the `tailnet-` prefix. Without this
    # case, loosening SYNTHETIC_TAILNET_RE to `^tailnet` (or dropping the `$`)
    # keeps the suite green — and that is precisely the loosening a maintainer
    # reaches for under a red build, which is the failure this self-test exists
    # to prevent. Every other "must be caught" fixture uses a name that does not
    # begin with `tailnet`, so none of them can tell the two apart.
    (f"url: box.tailnet-corp.{_TS_SUFFIX}", True, "a real name may begin `tailnet-`; only the 10-hex form is the scrubber's"),
]


def self_test() -> int:
    failures = []
    for line, should_flag, why in SELF_TEST_CASES:
        got = network_identifier_hits(line)
        if got != should_flag:
            verb = "flagged" if got else "passed"
            want = "flag" if should_flag else "pass"
            failures.append(f"  {line!r}\n    {verb}, expected to {want} ({why})")
    if failures:
        print("network matcher self-test FAILED:\n" + "\n".join(failures))
        return 1
    print(f"network matcher self-test passed: {len(SELF_TEST_CASES)} cases")
    return 0


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        sys.exit(self_test())
    sys.exit(main())
