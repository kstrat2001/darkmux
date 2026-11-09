#!/usr/bin/env python3
"""Verify the Homebrew tap actually serves the tag that was just released.

Why this exists (#2825): merging the formula pin used to fire
`sync-homebrew-tap.yml`, which pushed a PR to the tap. On 2026-09-19 that
workflow failed with `Bad credentials` and opened nothing, so v3.8.0 was
tagged, released and had its GHCR image published while the tap kept serving **v3.7.1**. Every other signal
read healthy. A user running `brew upgrade darkmux` in that window silently
stayed on the previous version.

The failure mode is what makes it dangerous: nothing goes red. This check turns
"the tap fell behind" into a loud, answerable question, and it is the same
one-line comparison used to verify the manual fix.

The tap now pulls the formula itself (`sync-from-upstream.yml` in
kstrat2001/homebrew-tap, dispatched by the release skill and polled every 6
hours), so no credential can expire, but a pull can still fail or lag, and this
check is still the one that says whether the tap serves the tag.

Pure-logic half is `tap_pin_problems()`, exercised by `--self-test` (this
repo's convention for script-level guards, per `engagement-sentinel-guard.py`).
"""
import re
import sys

URL_RE = re.compile(r'url\s+"([^"]+)"')
SHA_RE = re.compile(r'sha256\s+"([0-9a-f]{64})"')
COMMIT_RE = re.compile(r'ENV\["DARKMUX_COMMIT"\]\s*=\s*"([0-9a-f]{40})"')


def tap_pin_problems(formula_text: str, expected_tag: str, tarball_sha256: str, tag_commit: str) -> list:
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

    # The tarball has no `.git`, so a brew build's `darkmuxCommit` comes from
    # DARKMUX_COMMIT. Bumping url and sha256 but not this line (2026-10-28,
    # v5.9.0) built a binary that stamped the previous release's commit, and
    # the evals site refuses runs whose commit is not the tag's.
    commits = COMMIT_RE.findall(formula_text)
    if not commits:
        problems.append("formula has no DARKMUX_COMMIT line")
    elif commits[0] != tag_commit:
        problems.append(
            f"DARKMUX_COMMIT mismatch: formula has {commits[0][:12]}..., "
            f"{expected_tag} is commit {tag_commit[:12]}..."
        )

    return problems


COMMIT_C = '    ENV["DARKMUX_COMMIT"] = "' + "c" * 40 + '" unless build.head?\n'

SELF_TEST_CASES = [
    # (formula_text, expected_tag, tarball_sha, tag_commit, should_flag, why)
    (
        'url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v3.8.0.tar.gz"\n'
        '  sha256 "' + "a" * 64 + '"\n'
        + COMMIT_C,
        "v3.8.0",
        "a" * 64,
        "c" * 40,
        False,
        "url and sha both match the released tag",
    ),
    (
        # THE case that actually happened on 2026-09-19.
        'url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v3.7.1.tar.gz"\n'
        '  sha256 "' + "b" * 64 + '"\n',
        "v3.8.0",
        "a" * 64,
        "c" * 40,
        True,
        "tap still serves the PREVIOUS tag after a failed sync",
    ),
    (
        'url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v3.8.0.tar.gz"\n'
        '  sha256 "' + "b" * 64 + '"\n',
        "v3.8.0",
        "a" * 64,
        "c" * 40,
        True,
        "right tag, WRONG sha256 — a hand-edit or a stale pin",
    ),
    (
        'url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v3.8.0.tar.gz"\n',
        "v3.8.0",
        "a" * 64,
        "c" * 40,
        True,
        "no sha256 at all must FLAG, never pass silently",
    ),
    (
        'sha256 "' + "a" * 64 + '"\n',
        "v3.8.0",
        "a" * 64,
        "c" * 40,
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
        '  sha256 "' + "a" * 64 + '"\n'
        + COMMIT_C,
        "v3.8.0",
        "a" * 64,
        "c" * 40,
        True,
        "url must END at the tag tarball; a trailing suffix is a different artifact",
    ),
    (
        # A tag that is a PREFIX of another must not satisfy the check.
        'url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v3.8.01.tar.gz"\n'
        '  sha256 "' + "a" * 64 + '"\n'
        + COMMIT_C,
        "v3.8.0",
        "a" * 64,
        "c" * 40,
        True,
        "v3.8.01 must not count as v3.8.0 — substring match would pass this",
    ),
    (
        # THE case of 2026-10-28: url and sha256 bumped to the new tag, but
        # DARKMUX_COMMIT left at the previous release's commit, so every brew
        # build stamped the old commit and the evals site refused its runs.
        'url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v3.8.0.tar.gz"\n'
        '  sha256 "' + "a" * 64 + '"\n'
        '    ENV["DARKMUX_COMMIT"] = "' + "d" * 40 + '" unless build.head?\n',
        "v3.8.0",
        "a" * 64,
        "c" * 40,
        True,
        "DARKMUX_COMMIT still names the previous release's commit",
    ),
    (
        'url "https://github.com/kstrat2001/darkmux/archive/refs/tags/v3.8.0.tar.gz"\n'
        '  sha256 "' + "a" * 64 + '"\n',
        "v3.8.0",
        "a" * 64,
        "c" * 40,
        True,
        "no DARKMUX_COMMIT at all must FLAG: the build would stamp no commit",
    ),
]


def self_test() -> int:
    failures = []
    for text, tag, sha, commit, should_flag, why in SELF_TEST_CASES:
        got = bool(tap_pin_problems(text, tag, sha, commit))
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
    "https://raw.githubusercontent.com/kstrat2001/homebrew-tap/main/Formula/darkmux.rb"
)
TARBALL_URL = "https://github.com/kstrat2001/darkmux/archive/refs/tags/{tag}.tar.gz"
COMMIT_URL = "https://api.github.com/repos/kstrat2001/darkmux/commits/{tag}"


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

    try:
        import json

        # The commits endpoint resolves a tag (lightweight or annotated) to its commit.
        tag_commit = json.loads(_fetch(COMMIT_URL.format(tag=tag)))["sha"]
    except Exception as e:
        print(f"could not read the commit of {tag}: {e}")
        return 2

    problems = tap_pin_problems(formula, tag, tarball_sha, tag_commit)
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
    print(f"tap serves {tag}, sha256 matches the live tarball, DARKMUX_COMMIT is the tag's commit")
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
