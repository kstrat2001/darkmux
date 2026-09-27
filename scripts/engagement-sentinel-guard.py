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
MAGICDNS_RE = re.compile(r"\b(?:([a-z0-9-]+)\.)?([a-z0-9-]+)\.ts\.net\b", re.I)

# Host labels that are self-evidently not an identity. `sanitize.mjs` rewrites
# the host to `host-<8 hex>`, and the docs use a small set of placeholders.
SYNTHETIC_HOST_RE = re.compile(r"^host-[0-9a-f]{8}$")
EXAMPLE_HOSTS = {
    "laptop", "somebox", "machine", "host", "example", "localhost",
    "mini", "studio", "peer", "hub", "box",
}


def network_identifier_hits(line: str) -> bool:
    """True when the line carries a presumed-real tailnet address or hostname."""
    if CGNAT_RE.search(line):
        return True
    return any(not _is_permitted_magicdns(m) for m in MAGICDNS_RE.finditer(line))


def _is_permitted_magicdns(m: "re.Match[str]") -> bool:
    """BOTH halves must be permitted, not just the tailnet.

    The host used to be a non-capturing group, so `network_identifier_hits`
    judged the tailnet alone — and a permitted tailnet made the whole match
    permitted. Proven by execution 2026-08-24:
    an identifying host label placed in front of a PERMITTED tailnet suffix
    scanned CLEAN — publishing the identifying half while the sanctioned half
    vouched for it. (Spelled out only in SELF_TEST_CASES, assembled from
    fragments: written whole here it would trip this very matcher, which
    deliberately scans its own source.)

    `sanitize.mjs` rewrites BOTH labels in one replacement; this is the guard
    catching up to what the scrubber already knew.
    """
    host, tailnet = m.group(1), m.group(2)
    if not _is_permitted_tailnet(tailnet):
        return False
    if host is None:
        return True  # bare `<tailnet>.ts.net`, nothing else to judge
    h = host.lower()
    return h in EXAMPLE_HOSTS or SYNTHETIC_HOST_RE.match(h) is not None


def _is_permitted_tailnet(tailnet: str) -> bool:
    """The documented example names, plus the scrubber's own synthetic form."""
    t = tailnet.lower()
    return t in EXAMPLE_TAILNETS or SYNTHETIC_TAILNET_RE.match(t) is not None


# ── Hardware UIDs (#2957) ──────────────────────────────────────────────────
#
# A machine's hardware UUID (IOPlatformUUID, carried as `machine_uid` in flow
# records, presence beats and the fleet roster) is a durable identifier of one
# physical machine. It reached this repo as ordinary test data: a real laptop
# uid in 7 UI test files, pasted from a live record because a realistic value
# was the easiest thing to reach for. Like a tailnet name it spells no
# sentinel word, so only a shape check can see it.
#
# Any UUID-shaped string is REFUSED unless it is a recognizable fake. The fake
# form is `00000000-0000-4000-8000-<12 hex>`: hand-written fixtures put hex
# LETTERS in the tail (`...-ABCDEF000001`, so a test of case-insensitive
# handling has a letter to fold), and the parity scrubber (`syntheticUuid` in
# tests/parity/lib/sanitize.mjs) fills it from a digest. Matched
# case-insensitively, because real uids arrive UPPERCASE from the hardware
# probe and lowercase after a normalizing hop.
#
# The allow-list below is for UUID-shaped values that are not machine
# identities at all. Keep it tiny and give every entry a reason. Before adding
# one, prefer rewriting the value into the fake form: that is almost always
# possible and it keeps this list from becoming the hole.
#
# No boundary assertions, on purpose: a uid glued to a key (`uid` ends in a hex
# digit) or run into a longer hex token is still a uid, and over-matching only
# ever errs toward refusing.
UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", re.I)
FAKE_UUID_RE = re.compile(r"^00000000-0000-4000-8000-[0-9a-f]{12}$", re.I)
ALLOWED_UUIDS = {
    "00000000-0000-0000-0000-000000000000": "the nil UUID: identifies nothing",
}


def _is_permitted_uuid(value: str) -> bool:
    v = value.lower()
    return FAKE_UUID_RE.match(v) is not None or v in ALLOWED_UUIDS


# The FRAGMENT form: a uid's first group standing in for the whole, as a
# half-redacted fixture (`<8 hex>-UID`) or in prose (`<8 hex>-…`). The first
# group alone is 32 bits of a real uid, and this repo carried exactly that
# form in six files. Refused unless the eight hex are all zeros, which is the
# fake form's own head.
UID_FRAGMENT_RE = re.compile(r"(?<![0-9a-f])([0-9a-f]{8})-(?:uid\b|…|\.\.\.)", re.I)
FAKE_UID_HEAD = "00000000"


def uuid_hits(line: str) -> bool:
    """True when the line carries a uid, whole or as a fragment, that is not a known fake."""
    return any(not _is_permitted_uuid(m.group(0)) for m in UUID_RE.finditer(line)) or any(
        m.group(1) != FAKE_UID_HEAD for m in UID_FRAGMENT_RE.finditer(line)
    )


def mask_refused_uuids(line: str) -> str:
    """Never echo a refused uid: the report would republish what it caught.

    The finding already names file and line, which is all a maintainer needs
    to go and fix it. Permitted fakes are left readable.
    """
    line = UUID_RE.sub(
        lambda m: m.group(0) if _is_permitted_uuid(m.group(0)) else "<refused-uuid>", line
    )
    return UID_FRAGMENT_RE.sub(
        lambda m: m.group(0)
        if m.group(1) == FAKE_UID_HEAD
        else m.group(0).replace(m.group(1), "<refused-uid-head>", 1),
        line,
    )


def _identifier_hits(line: str, word_re: "re.Pattern[str]") -> bool:
    """The vocabulary and network checks: the ones ALLOWLIST exempts."""
    return bool(
        word_re.search(line)
        or any(r.search(line) for r in TICKET_RES)
        or network_identifier_hits(line)
    )


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


def files_under(root: Path) -> list[str]:
    """Every regular file under `root`, as paths relative to it.

    The pre-commit hook materializes STAGED blobs into a temp dir and scans
    that, so the guard sees what is actually about to be committed rather than
    the working tree. `git ls-files` cannot be used there — the temp dir is not
    a repository.
    """
    return [
        str(p.relative_to(root))
        for p in sorted(root.rglob("*"))
        if p.is_file() and ".git/" not in str(p)
    ]


def main(scan_dir: Path | None = None) -> int:
    canaries, delegated = load_canaries()
    if not canaries:
        sys.exit("guard is broken, not the tree: canary list resolved to empty")
    word_re = re.compile("|".join(re.escape(s) for s in canaries), re.I)

    findings: list[tuple[str, int, str]] = []
    base = scan_dir if scan_dir is not None else ROOT
    names = files_under(scan_dir) if scan_dir is not None else tracked_files()
    for rel in names:
        # ALLOWLIST files exist to NAME the vocabulary, so the word and network
        # checks skip them. The uid check does not: no file's purpose is to
        # carry a real hardware uid.
        exempt = rel in ALLOWLIST
        path = base / rel
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, FileNotFoundError, IsADirectoryError):
            continue  # binary or gone — nothing to read
        for n, line in enumerate(text.splitlines(), 1):
            if uuid_hits(line) or (not exempt and _identifier_hits(line, word_re)):
                findings.append((rel, n, mask_refused_uuids(line.strip())[:120]))

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
            f"tracker keys, and `00000000-0000-4000-8000-<12 hex>` for a machine "
            f"uid, e.g. ...-ABCDEF000001), or — only if the file's PURPOSE is to name the "
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
    # BOTH halves are identifying. Judging the tailnet alone published the host.
    (f"url: acme-client-prod-db.tailnet-example.{_TS_SUFFIX}", True, "an identifying HOST beside a permitted tailnet"),
    (f"url: host-a1b2c3d4.tailnet-example.{_TS_SUFFIX}", False, "scrubber-shaped host beside a documented tailnet"),
]


# UUID cases, same discipline as above: every "must be caught" value is
# INVENTED and ASSEMBLED, so no line of this file is a UUID-shaped literal the
# guard would refuse in its own scan.
_U = "-".join
_INVENTED_UID = _U(["C0FFEE12", "3456", "789A", "BCDE", "F0123456789A"])

UUID_SELF_TEST_CASES = [
    (f'machine_uid: "{_INVENTED_UID}"', True, "a real-shaped UPPERCASE hardware uid"),
    (f'machine_uid: "{_INVENTED_UID.lower()}"', True, "the same uid lowercased: matching is case-insensitive"),
    (f"presence:{_INVENTED_UID}", True, "a uid embedded after a key prefix"),
    (f"machineuid{_INVENTED_UID}", True, "a uid glued to a key ending in a hex digit"),
    ('machine_uid: "' + _U(["00000000", "0000", "4000", "8000", "000000000001"]) + '"', False, "a hand-numbered fake"),
    ('machine_uid: "' + _U(["00000000", "0000", "4000", "8000", "A1B2C3D4E5F6"]) + '"', False, "the parity scrubber's synthetic form"),
    ('machine_uid: "' + _U(["00000000", "0000", "4000", "8000", "a1b2c3d4e5f6"]) + '"', False, "the fake form lowercased"),
    ('nil: "' + _U(["00000000", "0000", "0000", "0000", "000000000000"]) + '"', False, "the allow-listed nil UUID"),
    # The DISCRIMINATOR is the whole `00000000-0000-4000-8000-` prefix, not its
    # leading zeros. A loosening to `startswith("00000000")` must go red here.
    ('machine_uid: "' + _U(["00000000", "3456", "789A", "BCDE", "F0123456789A"]) + '"', True, "leading zeros alone are not the fake form"),
    ('machine_uid: "' + _U(["00000000", "0000", "4000", "8001", "000000000001"]) + '"', True, "one nibble off the fake prefix"),
    ("sha: " + "C0FFEE12" * 5, False, "a dashless hex run is not UUID-shaped"),
    # Fragments: the first group standing in for the whole uid.
    ('machine_uid: "C0FFEE12' + '-UID"', True, "a half-redacted uid fixture"),
    ("reads `c0ffee12" + "-…` where", True, "a uid head in prose, lowercase"),
    ("reads `C0FFEE12" + "-...` where", True, "a uid head in prose, ASCII ellipsis"),
    ('machine_uid: "00000000' + '-UID"', False, "the fake form's all-zero head"),
    ("reads `<uid head>-…` where", False, "prose naming the head without a value"),
]


def self_test() -> int:
    failures = []
    for line, should_flag, why in UUID_SELF_TEST_CASES:
        got = uuid_hits(line)
        if got != should_flag:
            verb = "flagged" if got else "passed"
            want = "flag" if should_flag else "pass"
            failures.append(f"  {why}\n    {verb}, expected to {want}")
    if "<refused-uuid>" not in mask_refused_uuids(UUID_SELF_TEST_CASES[0][0]):
        failures.append("  a refused uid must be masked in the finding printout")
    if "C0FFEE12" in mask_refused_uuids('machine_uid: "C0FFEE12' + '-UID"'):
        failures.append("  a refused uid head must be masked in the finding printout")
    for line, should_flag, why in SELF_TEST_CASES:
        got = network_identifier_hits(line)
        if got != should_flag:
            verb = "flagged" if got else "passed"
            want = "flag" if should_flag else "pass"
            failures.append(f"  {line!r}\n    {verb}, expected to {want} ({why})")
    if failures:
        print("matcher self-test FAILED:\n" + "\n".join(failures))
        return 1
    print(
        f"network matcher self-test passed: {len(SELF_TEST_CASES)} cases; "
        f"uuid matcher: {len(UUID_SELF_TEST_CASES)} cases"
    )
    return 0


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        sys.exit(self_test())
    scan = None
    if "--scan-dir" in sys.argv:
        i = sys.argv.index("--scan-dir")
        if i + 1 >= len(sys.argv):
            sys.exit("--scan-dir requires a directory")
        scan = Path(sys.argv[i + 1])
        if not scan.is_dir():
            sys.exit(f"--scan-dir: not a directory: {scan}")
    sys.exit(main(scan))
