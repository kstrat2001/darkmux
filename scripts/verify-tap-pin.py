#!/usr/bin/env python3
"""Verify the Homebrew tap actually serves the tag that was just released.

Why this exists (#2825): merging the formula pin fires `sync-homebrew-tap.yml`,
which opens a PR on the tap. On 2026-09-19 that workflow failed with `Bad
credentials` and opened nothing, so v3.8.0 was tagged, released and had its
GHCR image published while the tap kept serving **v3.7.1**. Every other signal
read healthy. A user running `brew upgrade darkmux` in that window silently
stayed on the previous version.

The failure mode is what makes it dangerous: nothing goes red. This check turns
"the tap fell behind" into a loud, answerable question, and it is the same
one-line comparison used to verify the manual fix.

Pure-logic half is `tap_pin_problems()`, exercised by `--self-test` (this
repo's convention for script-level guards, per `engagement-sentinel-guard.py`).
"""
import re
import sys

URL_RE = re.compile(r'url\s+"([^"]+)"')
SHA_RE = re.compile(r'sha256\s+"([0-9a-f]{64})"')


def tap_pin_problems(formula_text: str, expected_tag: str, tarball_sha256: str) -> list:
    """Return a list of problems with the tap's pin. Empty list means correct.

    Absence is a PROBLEM, never a pass: a formula with no `url` or no `sha256`
    is malformed, and treating it as "nothing to complain about" is exactly the
    silent-success shape this check exists to remove.
    """
    problems = []
    urls = URL_RE.findall(formula_text)
    shas = SHA_RE.findall(formula_text)

    if not urls:
        problems.append("formula has no url line")
    else:
        # Anchored on the FULL trailing segment, not a substring: a substring
        # test would let `v3.8.01.tar.gz` satisfy a check for `v3.8.0`.
        want = f"/tags/{expected_tag}.tar.gz"
        if not urls[0].endswith(want):
            problems.append(f"url does not point at {expected_tag}: {urls[0]}")

    if not shas:
        problems.append("formula has no sha256 line")
    elif shas[0] != tarball_sha256:
        problems.append(
            f"sha256 mismatch: formula has {shas[0][:12]}..., "
            f"the {expected_tag} tarball is {tarball_sha256[:12]}..."
        )

    return problems


SELF_TEST_CASES = [
    # (formula_text, expected_tag, tarball_sha, should_flag, why)
    (
        'url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v3.8.0.tar.gz"\n'
        '  sha256 "' + "a" * 64 + '"\n',
        "v3.8.0",
        "a" * 64,
        False,
        "url and sha both match the released tag",
    ),
    (
        # THE case that actually happened on 2026-09-19.
        'url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v3.7.1.tar.gz"\n'
        '  sha256 "' + "b" * 64 + '"\n',
        "v3.8.0",
        "a" * 64,
        True,
        "tap still serves the PREVIOUS tag after a failed sync",
    ),
    (
        'url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v3.8.0.tar.gz"\n'
        '  sha256 "' + "b" * 64 + '"\n',
        "v3.8.0",
        "a" * 64,
        True,
        "right tag, WRONG sha256 — a hand-edit or a stale pin",
    ),
    (
        'url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v3.8.0.tar.gz"\n',
        "v3.8.0",
        "a" * 64,
        True,
        "no sha256 at all must FLAG, never pass silently",
    ),
    (
        'sha256 "' + "a" * 64 + '"\n',
        "v3.8.0",
        "a" * 64,
        True,
        "no url at all must FLAG — a matching sha alone proves nothing",
    ),
    (
        # DISCRIMINATING case for `endswith` vs a substring test. The first
        # draft of this suite used only the `v3.8.01` case below, and a
        # mutation to `if want not in urls[0]` SURVIVED it -- for that url the
        # wanted string is not a substring either, so both implementations
        # flag it and the suite could not tell them apart. A trailing suffix
        # is what separates them: the tag appears in full, but not at the end.
        'url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v3.8.0.tar.gz.sig"\n'
        '  sha256 "' + "a" * 64 + '"\n',
        "v3.8.0",
        "a" * 64,
        True,
        "url must END at the tag tarball; a trailing suffix is a different artifact",
    ),
    (
        # A tag that is a PREFIX of another must not satisfy the check.
        'url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v3.8.01.tar.gz"\n'
        '  sha256 "' + "a" * 64 + '"\n',
        "v3.8.0",
        "a" * 64,
        True,
        "v3.8.01 must not count as v3.8.0 — substring match would pass this",
    ),
]


def self_test() -> int:
    failures = []
    for text, tag, sha, should_flag, why in SELF_TEST_CASES:
        got = bool(tap_pin_problems(text, tag, sha))
        if got != should_flag:
            verb = "flagged" if got else "passed"
            want = "flag" if should_flag else "pass"
            failures.append(f"  expected to {want}, {verb}: {why}")
    if failures:
        print("tap-pin checker self-test FAILED:\n" + "\n".join(failures))
        return 1
    print(f"tap-pin checker self-test passed: {len(SELF_TEST_CASES)} cases")
    return 0


TAP_FORMULA_URL = (
    "https://raw.githubusercontent.com/kstrat2001/homebrew-darkmux/main/Formula/darkmux.rb"
)
TARBALL_URL = "https://github.com/kstrat2001/darkmux/archive/refs/tags/{tag}.tar.gz"


def _fetch(url: str) -> bytes:
    import urllib.request

    with urllib.request.urlopen(url, timeout=60) as r:  # noqa: S310 - fixed hosts
        return r.read()


def live_check(tag: str) -> int:
    """Compare the tap's published formula against the tag's real tarball.

    Deliberately fetches BOTH sides fresh rather than trusting anything local:
    the question is what a `brew upgrade` user actually receives right now.
    """
    import hashlib

    try:
        formula = _fetch(TAP_FORMULA_URL).decode("utf-8", "replace")
    except Exception as e:
        print(f"could not read the tap formula: {e}")
        return 2
    try:
        tarball_sha = hashlib.sha256(_fetch(TARBALL_URL.format(tag=tag))).hexdigest()
    except Exception as e:
        print(f"could not read the {tag} tarball: {e}")
        return 2

    problems = tap_pin_problems(formula, tag, tarball_sha)
    if problems:
        print(f"TAP IS NOT SERVING {tag}:")
        for p in problems:
            print(f"  - {p}")
        print(
            "\nThe tap falling behind is SILENT: the tag, the GitHub release and the\n"
            "GHCR image all look healthy while `brew upgrade darkmux` keeps handing\n"
            "users the previous version. See #2825."
        )
        return 1
    print(f"tap serves {tag}, sha256 matches the live tarball")
    return 0


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        sys.exit(self_test())
    if "--tag" in sys.argv:
        i = sys.argv.index("--tag")
        if i + 1 >= len(sys.argv):
            sys.exit("--tag requires a value, e.g. --tag v3.8.0")
        sys.exit(live_check(sys.argv[i + 1]))
    sys.exit("usage: verify-tap-pin.py [--self-test | --tag vX.Y.Z]")
