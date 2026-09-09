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
   legitimate when the diff added no *mutable* Rust lines, so the workflow
   passes a count in via `--changed-lines`; zero mutants against a non-zero
   count is the same exit-0/zero-mutants/green shape the package-scoping bug
   wore for its entire life.

   That count is computed HERE (`--count-changed-lines`), not by a `grep` in
   the workflow, and it is deliberately narrower than "added lines matching
   `^+`". See `added_line_is_countable` below for what it excludes and why —
   the first version of this floor counted every added line, and PR #2514
   (13 added lines: 12 of doc comment and one `#[serial_test::serial]`) failed
   a gate it should never have been shown to.

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


# ---------------------------------------------------------------------------
# The changed-line floor's counter.
#
# A HEURISTIC, stated as one. It approximates the question "did this diff add a
# line cargo-mutants could plausibly have generated a mutant from?" by throwing
# away added lines that are unambiguously not mutable code. It does NOT parse
# Rust, and a precise answer would mean reimplementing cargo-mutants' own
# visitor — not worth it for a floor whose only job is to notice that the tool
# measured nothing.
#
# DIRECTION OF ERROR, on purpose: a line is excluded only when it can be
# classified with certainty. Anything ambiguous COUNTS AS CODE, which keeps the
# floor armed. So the heuristic can still produce a false FAIL (a shape it
# hasn't learned about), and never produces a false PASS on its own.
#
# What it will still get wrong, all in the count-it-as-code direction:
#   * a MULTI-LINE attribute — `#[serde(` / `rename_all = "x"` / `)]` — where
#     only the first line is recognized and the continuations count as code;
#   * a MULTI-LINE block comment: `/* …` alone, a ` * …` continuation, and a
#     bare `*/` all count as code. Tracking block-comment state across a diff
#     is unreliable anyway, because the opening `/*` is often a CONTEXT line
#     that never appears as an added line;
#   * a `use` statement, which cargo-mutants never mutates but which this does
#     count — a pure import-reshuffle PR can still trip the floor;
#   * a doc comment whose body contains code (```rust fences): correctly
#     excluded here, but only because the whole line starts with `///`.
#
# Why not ask cargo-mutants itself. `cargo mutants --workspace --list
# --in-diff <diff>` looked like an exact discriminator and is cheap (measured
# 1.2s, parse-only, no build) — it reports 0 for #2514's comment-only diff and
# 5 for a `crates/`-only diff that this job's root-package scope misses. It was
# REJECTED as circular: it shares cargo-mutants' diff parser, path resolution
# and source visitor with the run it would be checking, so when one of those is
# the thing that broke, both numbers go to zero together and the floor turns
# itself off silently. Measured, not assumed: a diff carrying 848 real added
# code lines whose paths were rewritten to files not in the tree — the exact
# "--in-diff paths don't resolve" shape this floor exists to catch — makes
# `--workspace --list --in-diff` print `INFO No mutants to filter` and exit 0.
# A guard has to be independent of what it guards; git plus this classifier is.
# ---------------------------------------------------------------------------

# A line made only of these is structure, never a mutation site: `}`, `});`,
# `)`, `],`, `};`.
_STRUCTURE_ONLY_CHARS = set("{}()[];,")


def added_line_is_countable(text: str) -> bool:
    """Could this added line (the diff's leading `+` already stripped)
    plausibly have produced a mutant? Ambiguous shapes answer True — see the
    module comment above this function for the full list of known misses."""
    s = text.strip()
    if not s:
        return False  # blank
    # The `s and` is redundant at runtime — the blank check above already
    # returned — and is spelled out anyway so this branch does not rest on
    # `all()` over an empty string being vacuously True. Without it, deleting
    # the blank check above leaves every test green, which is the "test that
    # cannot fail" shape this whole job exists to find.
    if s and all(ch in _STRUCTURE_ONLY_CHARS for ch in s):
        return False  # `}` / `});` / `],` / `)`
    if s.startswith("//"):
        return False  # `//`, `///`, `//!` — line comments and doc comments
    if len(s) >= 4 and s.startswith("/*") and s.endswith("*/"):
        return False  # a block comment that opens and closes on one line
    if (s.startswith("#[") or s.startswith("#![")) and s.endswith("]") and s.count("[") == s.count("]"):
        return False  # a single-line attribute; an unbalanced one counts as code
    return True


def count_added_lines(diff_text: str) -> tuple[int, int]:
    """Return (added, countable) over a unified diff.

    `added` is every added line, the number the workflow's old `grep -cE
    '^\\+([^+]|$)'` produced. `countable` is the subset that could plausibly
    have produced a mutant, and is the one the floor asserts on. `+++ b/path`
    file headers are not added lines and are excluded from both."""
    added = 0
    countable = 0
    for line in diff_text.splitlines():
        if not line.startswith("+") or line.startswith("+++"):
            continue
        added += 1
        if added_line_is_countable(line[1:]):
            countable += 1
    return added, countable


def count_changed_lines_main(args: list[str]) -> int:
    """`--count-changed-lines <diff>`: print the countable total on stdout (the
    workflow captures it) and the breakdown on stderr (the job log reads it).

    Anything that goes wrong exits non-zero rather than printing a 0. A guard
    that cannot read its input must fail the step, not silently disarm itself —
    which is the whole shape of #1716."""
    i = args.index("--count-changed-lines")
    if i + 1 >= len(args):
        print("--count-changed-lines requires a path to a unified diff", file=sys.stderr)
        return 2
    path = Path(args[i + 1])
    try:
        text = path.read_text(errors="replace")
    except OSError as exc:
        print(f"--count-changed-lines could not read {path}: {exc}", file=sys.stderr)
        return 2
    added, countable = count_added_lines(text)
    print(countable)
    print(
        f"{added} added Rust line(s) in scope; {countable} could plausibly produce a "
        f"mutant ({added - countable} blank / structure-only / comment-only / "
        "attribute-only)",
        file=sys.stderr,
    )
    return 0


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
                f"{changed_lines} added Rust line(s) in the diff that could plausibly have "
                "produced a mutant (blank, structure-only, comment-only and single-line "
                "attribute lines are already excluded from that count).",
                "",
                "Causes seen in this repo, in order of likelihood: the `--in-diff` file's paths "
                "don't resolve against the checked-out tree (cargo-mutants logs `No mutants to "
                "filter` and exits 0), the diff is unparseable (`Diff file is empty`, also exit "
                "0), or the package scope excludes the crate that changed. It can still be "
                "legitimate — the count is a heuristic that cannot parse Rust, and it counts "
                "`use` statements, multi-line attributes and multi-line block-comment bodies as "
                "code (see `added_line_is_countable`). Read the raw log above before assuming "
                "which.",
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


# ---------------------------------------------------------------------------
# Self-test, part 2: the changed-line counter and the floor it feeds.
#
# These cases run END TO END — the diff goes through `--count-changed-lines`,
# and the number that comes out is handed straight to the floor at exit 0 with
# no output directory ("cargo-mutants ran and mutated nothing"). That is the
# real composition CI performs, so a row here is the actual gate verdict for
# that diff, not two half-tests that happen to agree.
#
# `expect_gate` 0 = the PR passes, 1 = the floor fires.
# ---------------------------------------------------------------------------

# PR #2514's entire src/ diff, reproduced: twelve `///` lines and one
# single-line attribute. The floor as first written counted 13 and failed it.
_DIFF_2514 = """diff --git a/src/coder_phase_tests.rs b/src/coder_phase_tests.rs
--- a/src/coder_phase_tests.rs
+++ b/src/coder_phase_tests.rs
@@ -1122,7 +1122,20 @@
         assert_eq!(branch_name("s1"), "darkmux/s1");
     }

+    /// `worktree_path` joins `worktrees_base_dir()` — which reads the shared
+    /// `DARKMUX_HOME` env var live, uncached, on every call. This test
+    /// never sets `DARKMUX_HOME` itself, but the two `worktree_path` calls
+    /// below straddle a window in which *another* test can: without
+    /// `#[serial]` this test can interleave with any of the many
+    /// `#[serial]`-marked tests elsewhere in this binary that
+    /// `set_var("DARKMUX_HOME", ...)` then restore it, so the first call can
+    /// observe the operator's real `~/.darkmux` and the second call can
+    /// observe a concurrent test's tempdir — same repo-relative path, two
+    /// different bases, spurious inequality. `#[serial]` closes the window by
+    /// excluding this test from running while any other `#[serial]` test
+    /// (the full set of `DARKMUX_HOME` mutators in this binary) is active.
     #[test]
+    #[serial_test::serial]
     fn worktree_path_is_deterministic_under_repo_name() {
"""

_DIFF_REAL_CODE = """diff --git a/src/mission_status.rs b/src/mission_status.rs
--- a/src/mission_status.rs
+++ b/src/mission_status.rs
@@ -70,6 +70,9 @@
 impl MissionView<'_> {
+    fn done(&self) -> usize {
+        self.finalized + self.aborted
+    }
 }
"""

COUNT_SELF_TEST_CASES = [
    {
        "name": "PR #2514: doc comments + one attribute count as zero, and the floor passes",
        "diff": _DIFF_2514,
        "expect_count": 0,
        "expect_gate": 0,
    },
    {
        # The reason the floor exists. Must stay red.
        "name": "real added code with zero mutants still fails (the under-scoping shape)",
        "diff": _DIFF_REAL_CODE,
        # 3 added lines, of which the trailing `    }` is structure-only.
        "expect_count": 2,
        "expect_gate": 1,
    },
    {
        "name": "a closing-brace-only diff counts zero",
        "diff": "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1,4 @@\n+}\n+});\n+    ],\n+)\n",
        "expect_count": 0,
        "expect_gate": 0,
    },
    {
        "name": "a blank-line-only diff counts zero",
        "diff": "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1,3 @@\n+\n+   \n+\t\n",
        "expect_count": 0,
        "expect_gate": 0,
    },
    {
        "name": "line comments and single-line block comments count zero",
        "diff": (
            "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1,4 @@\n"
            "+// plain\n+//! inner doc\n+/* one liner */\n+    /*x*/\n"
        ),
        "expect_count": 0,
        "expect_gate": 0,
    },
    {
        # BOUNDARY, and it is the fail-open direction on purpose: only shapes
        # the classifier is CERTAIN about are excluded, so a multi-line
        # attribute's continuation lines and a multi-line block comment's body
        # count as code and keep the floor armed.
        "name": "ambiguous shapes count as code — multi-line attribute and block comment",
        "diff": (
            "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1,6 @@\n"
            "+#[serde(\n+    rename_all = \"snake_case\"\n+)]\n"
            "+/* opens here\n+ * body\n+ */\n"
        ),
        # `#[serde(` (unbalanced), `rename_all = ...`, `/* opens here`,
        # `* body`, `*/`. The `)]` line is structure-only and drops out.
        "expect_count": 5,
        "expect_gate": 1,
    },
    {
        "name": "`+++ b/path` headers are never counted as added lines",
        "diff": "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1,1 @@\n+// only a comment\n",
        "expect_count": 0,
        "expect_gate": 0,
    },
    {
        "name": "a mix counts only the mutable line",
        "diff": (
            "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1,5 @@\n"
            "+\n+/// doc\n+#[test]\n+let x = a + b;\n+}\n"
        ),
        "expect_count": 1,
        "expect_gate": 1,
    },
    {
        # (#2499) The workflow's diff pathspec widened from 'src/*.rs' to
        # '*.rs' when `mutants-in-diff` started passing `--workspace`, so a
        # `crates/**` diff now reaches this counter for the first time. The
        # counter itself never looked at the path — `added_line_is_countable`
        # only reads line CONTENT — so this is the same real-code shape as
        # `_DIFF_REAL_CODE` above, just under a `crates/` path, proving that
        # widening held rather than assuming it from the src/ cases alone.
        "name": "a crates/ path (the newly in-scope tree) counts the same as src/",
        "diff": (
            "--- a/crates/darkmux-eureka/src/lib.rs\n"
            "+++ b/crates/darkmux-eureka/src/lib.rs\n"
            "@@ -380,3 +380,8 @@\n"
            "+\n+/// doc comment: zero\n"
            "+pub fn scratch_rule_count() -> usize {\n"
            "+    all_rules().len()\n+}\n"
        ),
        # blank, doc comment, and the trailing `}` are excluded; the `pub fn`
        # signature and the body line count.
        "expect_count": 2,
        "expect_gate": 1,
    },
]


def _run_self(argv: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(Path(__file__).resolve()), *argv],
        capture_output=True,
        text=True,
    )


def count_self_test() -> list[str]:
    failures = []
    for case in COUNT_SELF_TEST_CASES:
        with tempfile.TemporaryDirectory() as tmp:
            diff_path = Path(tmp) / "pr.diff"
            diff_path.write_text(case["diff"])
            problems = []

            proc = _run_self(["--count-changed-lines", str(diff_path)])
            got = proc.stdout.strip()
            if proc.returncode != 0:
                problems.append(f"counter exited {proc.returncode}: {proc.stderr.strip()}")
            elif got != str(case["expect_count"]):
                problems.append(f"counted {got!r}, expected {case['expect_count']}")

            # Hand the count straight to the floor, the way the workflow does:
            # exit 0, no output directory, i.e. "ran and mutated nothing".
            if proc.returncode == 0:
                gate = _run_self(
                    [
                        "0",
                        "diff",
                        "T",
                        "--changed-lines",
                        got,
                        str(Path(tmp) / "nope" / "mutants.out"),
                    ]
                )
                if gate.returncode != case["expect_gate"]:
                    problems.append(
                        f"gate exited {gate.returncode}, expected {case['expect_gate']}\n"
                        + "".join(
                            f"    | {ln}\n" for ln in (gate.stdout + gate.stderr).splitlines()
                        )
                    )
            if problems:
                failures.append(
                    f"  [count] {case['name']}\n" + "".join(f"    - {p}\n" for p in problems)
                )
    return failures


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
    failures += count_self_test()
    if failures:
        print("ci-mutants-summary self-test FAILED:\n" + "\n".join(failures))
        return 1
    total = len(SELF_TEST_CASES) + len(COUNT_SELF_TEST_CASES)
    print(f"ci-mutants-summary self-test passed: {total} cases")
    return 0


USAGE = (
    "usage: ci-mutants-summary.py <exit_code> <diff|full> <title> "
    "[--changed-lines N] [out_dir_candidate ...]\n"
    "       ci-mutants-summary.py --count-changed-lines <unified.diff>\n"
    "       ci-mutants-summary.py --self-test"
)


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        sys.exit(self_test())
    if "--count-changed-lines" in sys.argv:
        sys.exit(count_changed_lines_main(sys.argv[1:]))

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
