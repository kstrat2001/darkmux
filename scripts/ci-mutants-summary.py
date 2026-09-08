#!/usr/bin/env python3
"""Turn a `cargo mutants` run into an honest GitHub step summary. (#1716)

Lives in the repo, not inline in `quality.yml`, for the same reason
`ci-coverage-summary.py` does: a script in the tree can be exercised locally
before it is trusted in CI, and this one specifically needed a local,
repeatable proof against real/empty/missing `mutants.out` directories before
it was believed.

Run `--self-test` to execute that proof. `quality.yml` runs it BEFORE the
mutation step in both jobs, the same wiring `ci.yml` uses for
`engagement-sentinel-guard.py --self-test`. That is not ceremony: the entire
pass/fail decision for both mutation jobs now lives in this file, and the
repo's own mutation gate structurally cannot cover it (it is not Rust). A
summary script with no test is exactly the vacuity #1716 is about, relocated.

## The bug this replaces

Both mutation jobs piped `cargo mutants ... || true` into a summary step that
only ever asked "is `missed.txt` non-empty?". When cargo-mutants' OWN baseline
`cargo test` failed inside its copied build tree — a real, observed failure,
unrelated to any mutant — no mutant was ever generated or tested, `missed.txt`
was empty (or `mutants.out` never existed), and the summary printed:

    No surviving mutants ... was caught by a test.

on every single run, indistinguishable from an actually-clean sweep. A zero
count from "the tool didn't run" and a zero count from "everything was
caught" are opposite findings; conflating them is what made the check a false
green (see the issue for the two production incidents this let through).

## The fix

cargo-mutants' own exit code already carries the distinction
(https://mutants.rs, `src/exit_code.rs`):

    0  Success         - ran; all mutants caught
    2  FoundProblems    - ran; some mutants survived (this job's entire point)
    3  Timeout          - ran; a mutant's build/test exceeded the timeout
    4  BaselineFailed   - the UNMUTATED tree's own tests failed; nothing ran
    1  Usage            - bad CLI arguments
    5  FilterDiffMismatch / 6 FilterDiffInvalid - the --in-diff file didn't
       match the tree, or couldn't be parsed
    70 Software         - an internal cargo-mutants error

Exit codes 0/2/3 mean the tool actually ran mutation testing — those are the
only codes this script will report survivors/success for. Every other code
means nothing was tested, and that is reported as a hard FAILURE (exit 1),
never as "no survivors".

## Three further false-green shapes this script closes

The exit-code gate alone was not enough; each of these was reproduced by hand
against a real fixture directory before being closed here.

1. **A JSON shape we don't recognize.** `outcomes.json` is cargo-mutants'
   authoritative count. An `outcomes.json` that parses but carries none of the
   keys we read (a future field rename — this is why `quality.yml` now pins
   `cargo-mutants` to an exact `--version` at both install sites) used to yield
   an empty totals dict, which is NOT `None`, so the documented `.txt` fallback
   never ran, every count defaulted to 0, and the script printed "nothing to
   mutate" and exited 0 — at exit code 2, with real survivors sitting in
   `missed.txt`. Same lie as the original bug, from a new cause. An empty
   totals dict is now treated as absent, so the fallback runs.

2. **The exit code contradicting the counts.** Exit 2 means "mutants survived"
   and exit 3 means "a mutant timed out". If our counts say otherwise
   (`missed == 0` at exit 2, `timeout == 0` at exit 3) then one of the two is
   wrong and we do not know which — so this fails loudly rather than rendering
   a reassuring summary off numbers we have just proven we cannot trust.

3. **"Ran, but zero mutants" where zero is never legitimate.** For the nightly
   sweep, zero mutants always means a defect (a dropped flag, a stray
   `.cargo/mutants.toml`, a filter matching nothing) — there is no such thing
   as a package with nothing to mutate here. For the PR diff job, zero IS
   legitimate when the diff added no Rust lines, so the workflow passes the
   added-line count in via `--changed-lines`; zero mutants against a non-zero
   count of added Rust lines is the same exit-0/zero-mutants/green shape the
   package-scoping bug wore for its entire life.

## What is advisory and what gates

Surviving mutants stay ADVISORY — they are surfaced, counted and listed, and
the script still exits 0. Making survivors a gate before anyone has seen the
baseline would fail honest PRs for pre-existing debt, which is the reason the
job was written advisory in the first place.

What GATES is tool integrity: the tool did not run, or it ran and reported
numbers we cannot reconcile. That distinction is the whole of #1716.
"""
import json
import subprocess
import sys
import tempfile
from pathlib import Path

# The only exit codes that mean "cargo-mutants actually tested mutants".
OK_CODES = {0, 2, 3}

# The keys we read out of `outcomes.json`. Named once so the self-test's
# "unrecognized JSON shape" fixture cannot accidentally agree with it.
TOTALS_KEYS = ("total_mutants", "missed", "caught", "timeout", "unviable")

EXIT_MEANING = {
    0: "Success — ran, all mutants caught",
    1: "Usage — bad CLI arguments",
    2: "FoundProblems — ran, some mutants survived",
    3: "Timeout — ran, a mutant's build/test exceeded the timeout",
    4: "BaselineFailed — the UNMUTATED tree's own tests failed; nothing was tested",
    5: "FilterDiffMismatch — the --in-diff file didn't match the source tree",
    6: "FilterDiffInvalid — the --in-diff file could not be parsed",
    70: "Software — an internal cargo-mutants error",
}


def find_out_dir(candidates: list[str]) -> Path | None:
    for c in candidates:
        p = Path(c)
        if p.is_dir():
            return p
    return None


def line_count(path: Path) -> int:
    if not path.exists():
        return 0
    return sum(1 for line in path.read_text().splitlines() if line.strip())


def load_outcomes_totals(out_dir: Path | None) -> dict[str, int] | None:
    """Read cargo-mutants' own authoritative counts straight from its JSON
    (`outcomes.json` carries top-level `total_mutants`/`missed`/`caught`/
    `timeout`/`unviable` integers — NOT `len(outcomes)`, which also counts the
    one Baseline entry and would be off-by-one).

    Returns None if the file is missing, unreadable, OR carries none of the
    keys we know how to read, so the caller falls back to counting `.txt`
    lines. That last clause is load-bearing: an empty dict is falsy but is not
    `None`, and returning one made every count default to 0 while suppressing
    the fallback — a silent "no survivors" over a directory full of them."""
    if out_dir is None:
        return None
    outcomes = out_dir / "outcomes.json"
    if not outcomes.exists():
        return None
    try:
        data = json.loads(outcomes.read_text())
        totals = {k: int(data[k]) for k in TOTALS_KEYS if k in data}
    except (OSError, json.JSONDecodeError, TypeError, ValueError):
        return None
    # No recognized keys at all => we do not understand this file. Treat it as
    # absent rather than as a run of all zeroes.
    return totals or None


def main(
    exit_code: int,
    mode: str,
    title: str,
    candidates: list[str],
    changed_lines: int = 0,
) -> int:
    out_dir = find_out_dir(candidates)
    meaning = EXIT_MEANING.get(exit_code, f"unrecognized exit code {exit_code}")
    lines = [f"## {title}", ""]

    if exit_code not in OK_CODES:
        lines += [
            "**Mutation testing DID NOT RUN — this is not a pass.**",
            "",
            f"`cargo mutants` exited **{exit_code}** ({meaning}).",
        ]
        if exit_code == 4:
            lines += [
                "",
                "The baseline (unmutated) test suite failed inside cargo-mutants' own copied",
                "build tree — see the raw job log above for which test(s) failed and why.",
            ]
        lines += [
            "",
            "Reporting this as a failure instead of a false \"no survivors\" pass — see #1716.",
        ]
        print("\n".join(lines))
        return 1

    missed = out_dir / "missed.txt" if out_dir else None
    caught = out_dir / "caught.txt" if out_dir else None
    timeout = out_dir / "timeout.txt" if out_dir else None
    unviable = out_dir / "unviable.txt" if out_dir else None

    totals = load_outcomes_totals(out_dir)
    if totals is not None:
        source = "outcomes.json"
        n_missed = totals.get("missed", 0)
        n_caught = totals.get("caught", 0)
        n_timeout = totals.get("timeout", 0)
        n_unviable = totals.get("unviable", 0)
        n_total = totals.get("total_mutants", n_missed + n_caught + n_timeout + n_unviable)
    else:
        # outcomes.json missing/corrupt/unrecognized but the exit code said
        # "ran" — fall back to the per-category .txt files cargo-mutants also
        # writes.
        source = "the per-category .txt files (outcomes.json missing or unrecognized)"
        n_missed = line_count(missed) if missed else 0
        n_caught = line_count(caught) if caught else 0
        n_timeout = line_count(timeout) if timeout else 0
        n_unviable = line_count(unviable) if unviable else 0
        n_total = n_missed + n_caught + n_timeout + n_unviable

    # The exit code and the counts have to agree. Where they don't, one of the
    # two is lying and we cannot tell which — so say so instead of rendering a
    # reassuring summary over numbers we have just proven untrustworthy.
    contradictions = []
    if exit_code == 2 and n_missed == 0:
        contradictions.append(
            "exit 2 means **FoundProblems** (mutants survived), but the counts read "
            f"`missed: 0` from {source}."
        )
    if exit_code == 3 and n_timeout == 0:
        contradictions.append(
            "exit 3 means **Timeout** (a mutant's build/test exceeded the timeout), but "
            f"the counts read `timeout: 0` from {source}."
        )
    if contradictions:
        lines += [
            "**Mutation results are INCONSISTENT — this is not a pass.**",
            "",
        ]
        lines += [f"- {c}" for c in contradictions]
        lines += [
            "",
            "cargo-mutants' exit code and its own output disagree. Most likely the output "
            "shape changed under us (see the pinned `--version` in `quality.yml`) or the "
            "output directory is not the one this run wrote. Either way the numbers below "
            "would be fiction, so this fails instead of reporting them — see #1716.",
        ]
        print("\n".join(lines))
        return 1

    if out_dir is None or n_total == 0:
        # "Ran, and zero mutants." Legitimate in exactly one place: a PR diff
        # that added no Rust lines. Everywhere else it is the same exit-0,
        # zero-mutants, green shape the package-scoping bug wore for its whole
        # life, so it fails.
        if mode == "full":
            lines += [
                "**The sweep produced ZERO mutants — this is not a pass.**",
                "",
                f"`cargo mutants` exited {exit_code} but nothing was mutated. A sweep of this "
                "package never legitimately yields zero: check for a dropped package flag, a "
                "stray `.cargo/mutants.toml`, or an exclude filter matching everything.",
            ]
            print("\n".join(lines))
            return 1
        if changed_lines > 0:
            lines += [
                "**This PR changed Rust code but produced ZERO mutants — this is not a pass.**",
                "",
                f"`cargo mutants` exited {exit_code} and mutated nothing, against "
                f"{changed_lines} added Rust line(s) in the diff.",
                "",
                "Causes seen in this repo, in order of likelihood: the `--in-diff` file's paths "
                "don't resolve against the checked-out tree (cargo-mutants logs `No mutants to "
                "filter` and exits 0), the diff is unparseable (`Diff file is empty`, also exit "
                "0), or the package scope excludes the crate that changed. It can also be "
                "legitimate — a diff that only adds comments, blank lines or `use` statements "
                "has no mutable code in it. Read the raw log above before assuming which.",
            ]
            print("\n".join(lines))
            return 1
        lines.append(
            "cargo-mutants ran (exit "
            f"{exit_code}) and reported nothing to mutate — no added Rust lines in the diff."
        )
        print("\n".join(lines))
        return 0

    if mode == "full":
        for name, n in (("caught", n_caught), ("missed", n_missed), ("timeout", n_timeout), ("unviable", n_unviable)):
            lines.append(f"- **{name}**: {n}")
        lines += [
            "",
            'A surviving ("missed") mutant is a line no test constrains. This is the',
            "backlog number — not something to fix in one pass, but the direction of travel",
            "should be down.",
        ]
        print("\n".join(lines))
        return 0

    # mode == "diff"
    lines.append(f"cargo-mutants ran (exit {exit_code}); {n_total} mutant(s) evaluated from the diff.")
    lines.append("")
    lines.append(
        f"- **caught**: {n_caught} · **missed**: {n_missed} · "
        f"**timeout**: {n_timeout} · **unviable**: {n_unviable}"
    )
    lines.append("")
    if n_missed:
        lines += [
            f"**{n_missed} surviving mutant(s).** Each is a change to your code that NO test noticed.",
            "",
            "Advisory, not a gate — some survivors are genuinely untestable (a log line, a",
            "display string). But a survivor on a branch that decides a STATUS or a NUMBER is",
            'the "test that cannot fail" shape, and worth one minute before merging.',
            "",
            "```",
            missed.read_text().rstrip("\n") if missed else "",
            "```",
        ]
    else:
        lines.append("No surviving mutants on the changed lines.")
    if n_timeout or n_unviable:
        # Never claim "every mutation was caught" while some went unadjudicated.
        # A timed-out mutant was never ruled on: nobody knows whether a test
        # constrains that line. An unviable one never compiled.
        lines += [
            "",
            f"**{n_timeout} timed out, {n_unviable} unviable — those mutants were never "
            "adjudicated.** A timeout is not a catch: no test was ever proven to constrain "
            "those lines. Raise `--timeout`, or read them in the artifact, before treating "
            "this diff as covered.",
        ]
    elif not n_missed:
        lines.append("Every mutation this PR made testable was caught by a test.")
    print("\n".join(lines))
    return 0


# ---------------------------------------------------------------------------
# Self-test
#
# Each case builds a REAL fixture directory and invokes this script as a real
# subprocess, so argv parsing and the exit status are exercised the way CI
# exercises them, not simulated. `files: None` means "no output directory at
# all" (cargo-mutants never wrote one).
#
# The rows that matter most are the INVERTED ones — the cases that must FAIL.
# Nothing in this repo asserted that the mutation gate ever fails, and a gate
# that has never been observed red is not evidence that anything works. Every
# `expect_exit: 1` row below was reproduced by hand against the pre-fix script,
# where it produced a green, reassuring summary.
# ---------------------------------------------------------------------------

_SURVIVORS = (
    "crates/darkmux-crew/src/dispatch.rs:120:9: replace foo -> bool with true\n"
    "crates/darkmux-crew/src/dispatch.rs:144:5: delete ! in bar\n"
    "src/main.rs:88:13: replace baz -> usize with 0\n"
)


def _outcomes(**kw: int) -> str:
    return json.dumps(kw)


SELF_TEST_CASES = [
    {
        "name": "exit 4 (BaselineFailed) fails — nothing was ever tested",
        "argv": ["4", "diff", "T"],
        "files": None,
        "expect_exit": 1,
        "must_contain": ["DID NOT RUN", "BaselineFailed"],
        "must_not_contain": ["No surviving mutants", "nothing to mutate"],
    },
    {
        "name": "exit 5 (FilterDiffMismatch) fails — nothing was ever tested",
        "argv": ["5", "diff", "T"],
        "files": None,
        "expect_exit": 1,
        "must_contain": ["DID NOT RUN", "FilterDiffMismatch"],
        "must_not_contain": ["No surviving mutants", "nothing to mutate"],
    },
    {
        "name": "an empty exit_code (the step output never got set) fails",
        "argv": ["", "diff", "T"],
        "files": None,
        "expect_exit": 2,
        "must_contain": ["exit_code must be an integer"],
        "must_not_contain": ["No surviving mutants", "nothing to mutate"],
    },
    {
        "name": "exit 2 with real survivors lists them (advisory pass)",
        "argv": ["2", "diff", "T"],
        "files": {
            "outcomes.json": _outcomes(total_mutants=8, missed=3, caught=5, timeout=0, unviable=0),
            "missed.txt": _SURVIVORS,
        },
        "expect_exit": 0,
        "must_contain": ["3 surviving mutant(s)", "dispatch.rs:120", "8 mutant(s) evaluated"],
        "must_not_contain": ["No surviving mutants", "nothing to mutate", "Every mutation"],
    },
    {
        "name": "exit 0 with no output directory is a legitimate empty diff",
        "argv": ["0", "diff", "T"],
        "files": None,
        "expect_exit": 0,
        "must_contain": ["nothing to mutate"],
        "must_not_contain": ["DID NOT RUN", "INCONSISTENT"],
    },
    {
        # BLOCKER 3. A valid JSON object carrying none of the keys we read —
        # exactly what a future cargo-mutants field rename produces. Before the
        # fix this printed "reported nothing to mutate" and exited 0 while three
        # survivors sat in missed.txt at exit code 2.
        "name": "an unrecognized outcomes.json shape falls back to the .txt files",
        "argv": ["2", "diff", "T"],
        "files": {
            "outcomes.json": "{}",
            "missed.txt": _SURVIVORS,
        },
        "expect_exit": 0,
        "must_contain": ["3 surviving mutant(s)", "dispatch.rs:120"],
        "must_not_contain": ["No surviving mutants", "nothing to mutate", "Every mutation"],
    },
    {
        "name": "exit 3 reports the timeouts and never claims everything was caught",
        "argv": ["3", "diff", "T"],
        "files": {
            "outcomes.json": _outcomes(total_mutants=10, missed=0, caught=5, timeout=5, unviable=0),
        },
        "expect_exit": 0,
        "must_contain": ["5 timed out", "never adjudicated"],
        "must_not_contain": ["Every mutation", "DID NOT RUN"],
    },
    {
        # The exit code says survivors exist; the counts say none do.
        "name": "exit 2 with missed: 0 is a contradiction and fails",
        "argv": ["2", "diff", "T"],
        "files": {
            "outcomes.json": _outcomes(total_mutants=8, missed=0, caught=8, timeout=0, unviable=0),
        },
        "expect_exit": 1,
        "must_contain": ["INCONSISTENT", "missed: 0"],
        "must_not_contain": ["No surviving mutants", "Every mutation"],
    },
    {
        "name": "exit 3 with timeout: 0 is a contradiction and fails",
        "argv": ["3", "diff", "T"],
        "files": {
            "outcomes.json": _outcomes(total_mutants=8, missed=0, caught=8, timeout=0, unviable=0),
        },
        "expect_exit": 1,
        "must_contain": ["INCONSISTENT", "timeout: 0"],
        "must_not_contain": ["No surviving mutants", "Every mutation"],
    },
    {
        # MEDIUM 5, full mode. Zero is never legitimate for a whole-package sweep.
        "name": "full mode with zero mutants fails",
        "argv": ["0", "full", "T"],
        "files": None,
        "expect_exit": 1,
        "must_contain": ["ZERO mutants"],
        "must_not_contain": ["nothing to mutate", "direction of travel"],
    },
    {
        # MEDIUM 5, diff mode. Both PROVEN green shapes ("No mutants to filter"
        # from a diff whose paths don't resolve, and "Diff file is empty" from
        # an unparseable diff) exit 0 with zero mutants — the added-line count
        # is what tells them apart from an honestly empty diff.
        "name": "diff mode with zero mutants but added Rust lines fails",
        "argv": ["0", "diff", "T", "--changed-lines", "42"],
        "files": None,
        "expect_exit": 1,
        "must_contain": ["ZERO mutants", "42 added Rust line(s)"],
        "must_not_contain": ["nothing to mutate"],
    },
    {
        "name": "diff mode with zero mutants and zero added Rust lines passes",
        "argv": ["0", "diff", "T", "--changed-lines", "0"],
        "files": None,
        "expect_exit": 0,
        "must_contain": ["nothing to mutate"],
        "must_not_contain": ["ZERO mutants", "DID NOT RUN"],
    },
]


def self_test() -> int:
    failures = []
    for case in SELF_TEST_CASES:
        with tempfile.TemporaryDirectory() as tmp:
            argv = list(case["argv"])
            if case["files"] is not None:
                out = Path(tmp) / "mutants.out"
                out.mkdir()
                for name, body in case["files"].items():
                    (out / name).write_text(body)
                argv.append(str(out))
            else:
                # A candidate path that deliberately does not exist.
                argv.append(str(Path(tmp) / "nope" / "mutants.out"))
            proc = subprocess.run(
                [sys.executable, str(Path(__file__).resolve()), *argv],
                capture_output=True,
                text=True,
            )
            blob = proc.stdout + proc.stderr
            problems = []
            if proc.returncode != case["expect_exit"]:
                problems.append(f"exited {proc.returncode}, expected {case['expect_exit']}")
            for needle in case["must_contain"]:
                if needle not in blob:
                    problems.append(f"output is missing {needle!r}")
            for needle in case["must_not_contain"]:
                if needle in blob:
                    problems.append(f"output wrongly contains {needle!r}")
            if problems:
                failures.append(
                    f"  {case['name']}\n"
                    + "".join(f"    - {p}\n" for p in problems)
                    + "    --- output ---\n"
                    + "".join(f"    | {ln}\n" for ln in blob.splitlines())
                )
    if failures:
        print("ci-mutants-summary self-test FAILED:\n" + "\n".join(failures))
        return 1
    print(f"ci-mutants-summary self-test passed: {len(SELF_TEST_CASES)} cases")
    return 0


USAGE = (
    "usage: ci-mutants-summary.py <exit_code> <diff|full> <title> "
    "[--changed-lines N] [out_dir_candidate ...]\n"
    "       ci-mutants-summary.py --self-test"
)


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        sys.exit(self_test())

    args = sys.argv[1:]
    changed_lines = 0
    if "--changed-lines" in args:
        i = args.index("--changed-lines")
        if i + 1 >= len(args):
            print("--changed-lines requires a number", file=sys.stderr)
            sys.exit(2)
        raw = args[i + 1]
        try:
            changed_lines = int(raw)
        except ValueError:
            print(f"--changed-lines must be an integer, got {raw!r}", file=sys.stderr)
            sys.exit(2)
        del args[i : i + 2]

    if len(args) < 3:
        print(USAGE, file=sys.stderr)
        sys.exit(2)
    try:
        code = int(args[0])
    except ValueError:
        print(f"exit_code must be an integer, got {args[0]!r}", file=sys.stderr)
        sys.exit(2)
    sys.exit(main(code, args[1], args[2], args[3:], changed_lines))
