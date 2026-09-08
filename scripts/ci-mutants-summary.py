#!/usr/bin/env python3
"""Turn a `cargo mutants` run into an honest GitHub step summary. (#1716)

Lives in the repo, not inline in `quality.yml`, for the same reason
`ci-coverage-summary.py` does: a script in the tree can be exercised locally
before it is trusted in CI, and this one specifically needed a local,
repeatable proof against real/empty/missing `mutants.out` directories before
it was believed.

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

Kept honest even when the tool DID run: this script reads cargo-mutants' own
authoritative counts out of `outcomes.json` (falling back to counting lines in
the per-category `.txt` files only if that JSON is missing or unreadable), and
still treats "ran, but generated zero mutants" (e.g. an `--in-diff` touching no
Rust code) as a legitimate, distinct zero from "didn't run" — the exit-code
gate above is what tells them apart.
"""
import json
import sys
from pathlib import Path

# The only exit codes that mean "cargo-mutants actually tested mutants".
OK_CODES = {0, 2, 3}

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
    one Baseline entry and would be off-by-one). Returns None if the file is
    missing or unreadable, so the caller falls back to counting `.txt` lines."""
    if out_dir is None:
        return None
    outcomes = out_dir / "outcomes.json"
    if not outcomes.exists():
        return None
    try:
        data = json.loads(outcomes.read_text())
        return {
            k: int(data[k])
            for k in ("total_mutants", "missed", "caught", "timeout", "unviable")
            if k in data
        }
    except (OSError, json.JSONDecodeError, TypeError, ValueError):
        return None


def main(exit_code: int, mode: str, title: str, candidates: list[str]) -> int:
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
        n_missed = totals.get("missed", 0)
        n_caught = totals.get("caught", 0)
        n_timeout = totals.get("timeout", 0)
        n_unviable = totals.get("unviable", 0)
        n_total = totals.get("total_mutants", n_missed + n_caught + n_timeout + n_unviable)
    else:
        # outcomes.json missing/corrupt but the exit code said "ran" — fall
        # back to the per-category .txt files cargo-mutants also writes.
        n_missed = line_count(missed) if missed else 0
        n_caught = line_count(caught) if caught else 0
        n_timeout = line_count(timeout) if timeout else 0
        n_unviable = line_count(unviable) if unviable else 0
        n_total = n_missed + n_caught + n_timeout + n_unviable

    # Exit said "ran" (0/2/3), but if there's no output directory at all, or
    # the output directory is empty on every axis, cargo-mutants genuinely
    # found nothing to mutate (e.g. an --in-diff with no Rust changes) —
    # that IS a legitimate zero, distinct from a swallowed tool failure,
    # because we already gated on the exit code above.
    if out_dir is None or n_total == 0:
        lines.append(
            "cargo-mutants ran (exit "
            f"{exit_code}) and reported nothing to mutate — no output to summarize."
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
        lines.append(
            "No surviving mutants on the changed lines. Every mutation this PR made testable "
            "was caught by a test."
        )
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    if len(sys.argv) < 4:
        print(
            "usage: ci-mutants-summary.py <exit_code> <diff|full> <title> [out_dir_candidate ...]",
            file=sys.stderr,
        )
        sys.exit(2)
    try:
        code = int(sys.argv[1])
    except ValueError:
        print(f"exit_code must be an integer, got {sys.argv[1]!r}", file=sys.stderr)
        sys.exit(2)
    sys.exit(main(code, sys.argv[2], sys.argv[3], sys.argv[4:]))
