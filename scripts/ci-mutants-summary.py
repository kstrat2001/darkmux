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
import re
import subprocess
import sys
import tempfile
from pathlib import Path

try:
    import tomllib  # Python 3.11+, standard library
except ModuleNotFoundError:  # pragma: no cover — pre-3.11 interpreter
    tomllib = None

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
# Test code (#2582). cargo-mutants never mutates `#[cfg(test)]` items, so a PR
# whose only added lines live inside `mod tests` legitimately produces zero
# mutants — and PR #2579 (two inline unit tests, nothing else) proved the
# counter did not know that and failed an honest PR. Two exclusions:
#
#   * a whole FILE under a `tests/` directory (Cargo's integration-test
#     convention — `tests/cli.rs`, `crates/foo/tests/bar.rs`) is skipped
#     entirely, no attribute needed;
#   * a line inside a `#[cfg(test)]`-attributed item (`mod`, `fn`, …) is
#     excluded via `_test_module_ranges`, which reads the file's CURRENT
#     on-disk content (the checked-out tree — always present in the real CI
#     job) and tracks brace depth from the attribute's own line to its
#     matching close. This is the PRIMARY mechanism, and it is independent of
#     the diff: it answers "is line N inside cfg(test)?" the same way
#     regardless of how much (or little) surrounding context this diff's
#     hunks happen to show.
#
# A hunk header's own trailing text (`@@ ... @@ mod tests {`) looks like a
# free, built-in version of the same signal — git already computed it — and
# IS used, but only as a FALLBACK when the file cannot be read (a self-test
# fixture with no matching file on disk, a deleted file). It is not trusted
# as the primary mechanism because it is not reliable: git's default
# heuristic picks the nearest column-0 line above the hunk, and PR #2579's
# own second hunk proved this out — its enclosing `#[cfg(test)] mod tests {`
# is thousands of lines above the hunk, but a multi-line string literal
# inside an EARLIER test (an unindented continuation line reading literally
# `line two`) sits at column 0 in between, so git's heuristic reports `line
# two` as the hunk's context instead of `mod tests {`. Reading the real file
# and tracking braces from the attribute sidesteps that entirely; the header
# fallback exists only for when there is no file to read.
#
# `_test_module_ranges` is not a Rust parser either, but its brace counting
# (`_brace_deltas_per_line`) IS string/comment-aware — a `{`/`}` inside a
# `"..."` or `r#"..."#` string, or a `//`/`/* */` comment, is not counted.
# This was not optional: this repo's own `plan.rs` test module builds a fake
# unified diff as string literals containing `"  } catch (e) {"`, and a
# naive per-line character count read those as real braces and closed the
# enclosing `#[cfg(test)] mod tests` hundreds of lines early — the first cut
# of this fix passed every self-test case and still failed PR #2579's real
# second hunk for exactly that reason. Nested `#[cfg(test)]` items are
# tracked with a stack, but only `#[cfg(test)]` itself is recognized — not
# `#[cfg(all(test, feature = "x"))]` or similar — and an attributed item whose
# opening brace lands on a LATER line than the attribute (a multi-line `fn`
# signature) is not recognized at all; both fall back to counting as code, as
# does a nested (non-doc) `/* /* */ */` block comment, which the scanner
# does not track — see `_brace_deltas_per_line`'s docstring. Kept in the same
# fail-open direction as everything else here: a misplaced boundary shrinks
# the excluded range, never enlarges it, so the floor stays armed.
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


# A hunk header: `@@ -<old_start>[,<old_count>] +<new_start>[,<new_count>] @@<hint>`.
# Only the new-side start is needed — it seeds the running new-file line
# counter — plus the trailing hint text for the header fallback below.
_HUNK_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@(.*)$")

# The header-fallback pattern (see the module comment): loose on purpose — a
# missed match here just falls back further, to "count as code".
_TEST_MOD_HINT_RE = re.compile(r"\bmod\s+\w*test\w*\s*\{")

# Per-process cache of {relative path -> ranges, or None if the file could
# not be read}. A fresh process per invocation of this script (including each
# `--self-test` case's own subprocess) means there is no cross-run staleness
# to worry about.
_TEST_RANGE_CACHE: dict[str, list[tuple[int, int]] | None] = {}


def _brace_deltas_per_line(text: str) -> list[tuple[int, int]]:
    """Return `(opens, closes)` for every line of `text`, counting only `{`
    and `}` that are real Rust syntax — never one sitting inside a string, a
    char literal, or a comment.

    Necessary, not decorative: this repo's own test fixtures are exactly the
    counter-example. `plan.rs`'s test module (the file behind PR #2579's
    real second hunk) builds a fake unified diff as a `&str` array containing
    the literal text `"  } catch (e) {"` — a naive `line.count("{")` reads
    that as two real braces and closes the enclosing `#[cfg(test)] mod
    tests` hundreds of lines early, silently un-excluding everything after
    it. Measured: with naive counting this function's caller reports the
    real module ending 500+ lines short of its actual close.

    A single forward scan over the whole file (not per-line — a string,
    block comment, or the file's `mod tests` itself can span lines) tracking
    which of `code` / a `"..."` string / a raw `r#"..."#` string / a `//`
    line comment / a `/* */` block comment we are inside. Only `{`/`}` seen
    while in `code` state count. `'` is deliberately NOT specially handled:
    a char literal (`'x'`) and a lifetime (`'a`) can never legitimately
    contain a brace either way, so treating `'` as an ordinary character
    never miscounts one.

    Known gaps, same fail-open direction as the rest of this module: Rust
    block comments nest (`/* /* */ */`); this scanner does not, so a nested
    block comment's inner `*/` closes the scan early and its remaining
    content is read as ordinary code — a stray brace in that content can
    still misplace a boundary, exactly the class this function exists to
    close for strings. Not observed in this repo's fixtures; if it ever
    happens, the module comment's fail-open guarantee still bounds the
    damage to "counts a test line as code", never the reverse."""
    lines = text.splitlines()
    deltas = [[0, 0] for _ in lines]
    lineno = 0
    i = 0
    n = len(text)
    state = "code"  # code | dstring | rawstring | line_comment | block_comment
    raw_hashes = 0
    while i < n:
        ch = text[i]
        if ch == "\n":
            lineno += 1
            if state == "line_comment":
                state = "code"
            i += 1
            continue
        if state == "code":
            if ch == "/" and i + 1 < n and text[i + 1] == "/":
                state = "line_comment"
                i += 2
                continue
            if ch == "/" and i + 1 < n and text[i + 1] == "*":
                state = "block_comment"
                i += 2
                continue
            if ch == "{":
                deltas[lineno][0] += 1
                i += 1
                continue
            if ch == "}":
                deltas[lineno][1] += 1
                i += 1
                continue
            if ch == '"':
                state = "dstring"
                i += 1
                continue
            if ch == "r" and i + 1 < n and text[i + 1] in ('"', "#"):
                j = i + 1
                hashes = 0
                while j < n and text[j] == "#":
                    hashes += 1
                    j += 1
                if j < n and text[j] == '"':
                    state = "rawstring"
                    raw_hashes = hashes
                    i = j + 1
                    continue
            i += 1
            continue
        if state == "dstring":
            if ch == "\\":
                # Skip the escaped character (including an escaped quote,
                # which must not end the string) WITHOUT going through the
                # newline check above — except when the escaped character
                # IS a newline (Rust's `"...\` + line-break line-continuation,
                # which strips the break from the string's value but not
                # from the file), which must still advance `lineno`, or
                # every delta for the rest of the file silently shifts by
                # one line. Proven: this is exactly what happened to
                # `run_record.rs`'s own `mod tests` before this branch
                # existed — one such continuation in a test's assertion
                # message desynced `lineno` and closed the range 296 lines
                # early.
                if i + 1 < n and text[i + 1] == "\n":
                    lineno += 1
                i += 2
                continue
            if ch == '"':
                state = "code"
            i += 1
            continue
        if state == "rawstring":
            if ch == '"':
                j = i + 1
                matched = 0
                while j < n and matched < raw_hashes and text[j] == "#":
                    matched += 1
                    j += 1
                if matched == raw_hashes:
                    state = "code"
                    i = j
                    continue
            i += 1
            continue
        if state == "block_comment":
            if ch == "*" and i + 1 < n and text[i + 1] == "/":
                state = "code"
                i += 2
                continue
            i += 1
            continue
        if state == "line_comment":
            # Every non-newline character while inside a `//` comment is
            # inert — the branch above already handles the newline that
            # ends it. Just advance; a missing branch here is an infinite
            # loop, not a miscount (proven: this was that bug).
            i += 1
            continue
    return [(o, c) for o, c in deltas]


def _test_module_ranges(path: Path) -> list[tuple[int, int]] | None:
    """Scan `path`'s CURRENT on-disk content for `#[cfg(test)]`-attributed
    items and return their [start, end] line ranges (1-indexed, inclusive),
    by tracking brace depth (via `_brace_deltas_per_line`, so strings and
    comments cannot masquerade as braces) from each attribute's own line to
    its matching close — a stack, so nested items are handled.

    Returns `None` when the file cannot be read at all (deleted, outside the
    working directory, a self-test fixture that supplies only a diff) — the
    caller then falls back to the much weaker hunk-header signal; see the
    module comment for why the header is not trusted as the primary
    mechanism. Returns `[]` (not `None`) when the file WAS read but has no
    `#[cfg(test)]` item — that is a real answer ("nothing to exclude"), not a
    missing one.

    Not a Rust parser: see the module comment for what this still gets
    wrong, all in the fail-open ("still counts as code") direction."""
    try:
        text = path.read_text(errors="replace")
    except OSError:
        return None
    line_texts = text.splitlines()
    line_deltas = _brace_deltas_per_line(text)
    ranges: list[tuple[int, int]] = []
    stack: list[tuple[int, int]] = []  # (depth before the opening brace, start line)
    depth = 0
    pending_attr = False
    for idx, raw in enumerate(line_texts):
        lineno = idx + 1
        stripped = raw.strip()
        opens, closes = line_deltas[idx]
        if not stripped or stripped.startswith("//"):
            continue
        if stripped.startswith("#[cfg(test)]"):
            pending_attr = True
            continue
        if pending_attr:
            pending_attr = False
            if opens > closes:
                stack.append((depth, lineno))
                depth += opens - closes
                continue
            # A `#[cfg(test)]` item with no net brace-open on its own line:
            # either brace-less (`#[cfg(test)] use x as y;`) or a multi-line
            # signature whose `{` lands on a later line (unrecognized — see
            # the module comment). Either way only this one line is known to
            # be spent on the attribute.
            ranges.append((lineno, lineno))
            depth += opens - closes
            continue
        depth += opens - closes
        while stack and depth <= stack[-1][0]:
            _, start_line = stack.pop()
            ranges.append((start_line, lineno))
    return ranges


# ---------------------------------------------------------------------------
# (#2544) Which paths a given `cargo mutants` invocation can actually reach.
#
# The floor above answers "did this diff add lines that could plausibly
# produce a mutant" — that is only a sound gate when paired with "...and
# could THIS invocation's scope ever see them". `quality.yml` runs two
# mutation invocations against two different manifests: the root workspace
# (`Cargo.toml`, `--workspace`) and `runtime/Cargo.toml` (its own standalone
# crate, deliberately excluded from the root workspace — see the root
# manifest's own comment). A line in `runtime/` is invisible to the FIRST
# invocation no matter how the diff pathspec is widened — the root
# manifest's `[workspace] exclude` list says so — and a line outside
# `runtime/` is equally invisible to the SECOND. Counting either against the
# wrong invocation's floor reproduces #2544: a runtime-only diff failed the
# PR gate because the workspace-scoped run reported zero mutants against a
# floor that expected it to reach code it structurally cannot.
#
# `--manifest-path <path>` (on `--count-changed-lines`) tells the counter
# which invocation's floor it is computing. The exclusion list is READ from
# that manifest's own `[workspace] exclude` array rather than hardcoded, so
# a future addition to (or removal from) that array changes the floor
# automatically instead of silently reintroducing this bug the next time
# someone excludes a fourth project and forgets the gate exists.
#
# Fails open in the SAME direction as the rest of this file when the input
# is diff/manifest CONTENT: if the manifest carries no `[workspace] exclude`
# list, nothing is excluded — every path under the manifest's own directory
# counts as reachable, keeping the floor armed (more code counted, never
# less). But an EXPLICITLY-passed `--manifest-path` that cannot be READ AT
# ALL is an operator/workflow input, not diff content — same class as the
# `--count-changed-lines <diff>` path itself, which already exits loudly
# rather than printing 0 — so `count_changed_lines_main` raises that as a
# hard failure instead of silently scoping the floor to a directory nothing
# lives under.
#
# (#2602) The array is parsed with the standard library's TOML parser
# (`tomllib`, Python 3.11+ — confirmed present on the runner) rather than a
# hand-rolled regex. The regex version handed the whole `exclude = [...]`
# array BODY to a bare quoted-string finder with no notion of a TOML
# comment: `exclude = ["runtime", # keeps "src" out? no - just prose\n
# "tools/darkmux-mock-model"]` picked up the quoted words inside the
# comment as a THIRD excluded path (`"src"`), and that is exactly the kind
# of edit this feature's own commit message invites — a future exclusion
# added or annotated without touching this script. On the real root
# manifest that dropped the floor for an ordinary `src/` diff from 2 to 0,
# with no error and no warning: the gate that exists to catch a diff
# structurally invisible to its own invocation went silently blind on the
# most ordinary kind of edit. A real parser does not have this failure
# mode — a `#` inside a TOML comment is never data, regardless of what
# looks quoted next to it. `tomllib` is used first; a regex fallback
# remains for a pre-3.11 interpreter, and is used (with a stderr warning)
# if the manifest fails to parse as TOML at all.
# ---------------------------------------------------------------------------

_WORKSPACE_TABLE_RE = re.compile(r"^\[workspace\]\s*$", re.MULTILINE)
_ANY_TABLE_HEADER_RE = re.compile(r"^\[[^\]]*\]\s*$", re.MULTILINE)
_EXCLUDE_ARRAY_RE = re.compile(r"^\s*exclude\s*=\s*\[(.*?)\]", re.DOTALL | re.MULTILINE)
_QUOTED_STRING_RE = re.compile(r'"([^"]*)"')


def _normalize_exclude_entry(raw: str) -> str:
    """Normalize one `[workspace] exclude` array entry to the bare
    manifest-relative directory prefix `reachable_predicate` compares
    against. (#2602) Cargo's own exclude matching, and a plausible future
    hand-edit of this array, both write forms that name the exact same
    directory as the bare form but do not compare EQUAL to it: a trailing
    slash (`"runtime/"`), a leading `./` (`"./runtime"`), or a trailing
    glob marking "everything under this directory" (`"runtime/*"` or
    `"runtime/**"`). Compared literally, each of those matches nothing —
    `reachable_predicate` reports every path under the intended directory
    as reachable, the silent-disarm TWIN of the comment bug above: the
    array parses cleanly, the entry is real, and the exclusion still does
    nothing. This fails SAFE (more code counted, matching the rest of this
    file's fail-open posture) rather than green, but it defeats the "the
    floor is derived from the manifest, not hand-maintained" claim on the
    very next edit that writes one of these forms — so normalize instead
    of documenting the gap."""
    entry = raw.strip()
    if entry.startswith("./"):
        entry = entry[2:]
    if entry.endswith("/**"):
        entry = entry[:-3]
    elif entry.endswith("/*"):
        entry = entry[:-2]
    return entry.rstrip("/")


def _parse_workspace_exclude_regex(manifest_text: str) -> list[str]:
    """Fallback extraction used only when `tomllib` is unavailable (a
    pre-3.11 interpreter) or the manifest could not be parsed as TOML at
    all. Not a TOML parser — a small, targeted regex, good enough for this
    manifest's actual shape (a single- or multi-line quoted-string array)
    but, unlike `tomllib`, unable to tell a `#` comment from array
    content — callers that reach this path print a warning explaining
    why. Returns `[]` (fails open) for anything it does not recognize,
    including a manifest with no `[workspace]` table, or a `[workspace]`
    table with no `exclude` key."""
    m = _WORKSPACE_TABLE_RE.search(manifest_text)
    if not m:
        return []
    next_header = _ANY_TABLE_HEADER_RE.search(manifest_text, m.end())
    body = manifest_text[m.end() : next_header.start() if next_header else len(manifest_text)]
    em = _EXCLUDE_ARRAY_RE.search(body)
    if not em:
        return []
    return [_normalize_exclude_entry(s) for s in _QUOTED_STRING_RE.findall(em.group(1))]


def parse_workspace_exclude(manifest_text: str) -> list[str]:
    """Extract the `[workspace] exclude = [...]` array's string values from
    a Cargo.toml's raw text, normalized (see `_normalize_exclude_entry`).

    Parsed as real TOML via `tomllib` when available (#2602) — a `#`
    sharing a line with array content is unambiguously a comment, not
    data, the same as it is to `cargo` itself. Fails open silently (`[]`,
    no warning) for the two LEGITIMATE no-exclusion shapes: no
    `[workspace]` table at all, or a `[workspace]` table that deliberately
    carries no `exclude` key (e.g. `runtime/Cargo.toml`'s own empty
    `[workspace]`). Warns on stderr — the parser genuinely gave up,
    distinct from those two deliberate shapes — when the manifest fails to
    parse as TOML at all (falls back to the regex extractor), or when an
    `exclude` key is present but is not a list of strings; either way the
    floor still fails open (`[]` / whatever the fallback recovers) rather
    than raising, since a manifest a human wrote and CI still needs to run
    against must not hard-fail the job over its shape."""
    if tomllib is None:
        return _parse_workspace_exclude_regex(manifest_text)
    try:
        data = tomllib.loads(manifest_text)
    except tomllib.TOMLDecodeError as exc:
        print(
            f"parse_workspace_exclude: manifest did not parse as TOML ({exc}); "
            "falling back to regex extraction of [workspace] exclude",
            file=sys.stderr,
        )
        return _parse_workspace_exclude_regex(manifest_text)
    workspace = data.get("workspace")
    if not isinstance(workspace, dict):
        return []  # no [workspace] table at all — legitimately nothing to exclude
    if "exclude" not in workspace:
        return []  # a real [workspace] table that deliberately carries no exclude
    exclude = workspace["exclude"]
    if isinstance(exclude, list) and all(isinstance(x, str) for x in exclude):
        return [_normalize_exclude_entry(s) for s in exclude]
    print(
        "parse_workspace_exclude: [workspace] table found but its `exclude` "
        f"key is not a list of strings ({exclude!r}); ignoring it",
        file=sys.stderr,
    )
    return []


def manifest_scope(manifest_path: Path) -> tuple[str, list[str]]:
    """Return `(manifest_dir, excluded_prefixes)` for `manifest_path`:
    `manifest_dir` is that manifest's own directory, POSIX-style and
    relative to the repo root (`""` for a root `Cargo.toml`);
    `excluded_prefixes` are the paths its own `[workspace] exclude` array
    names, relative to `manifest_dir`. Raises `OSError` if the file cannot
    be read, and `ValueError` if `manifest_path`'s directory is not
    repository-root-relative — either an absolute path or one containing a
    `..` parent segment (#2602). `reachable_predicate` below only ever
    compares `manifest_dir` as a PREFIX of a diff's own repo-root-relative
    `+++ b/<path>` header; a directory that isn't repo-root-relative can
    never be that prefix, so every line silently reads as unreachable and
    the floor drops to zero with nothing to catch it — the same class of
    failure as the unreadable-manifest case just one step further along,
    so it gets the same hard-failure treatment rather than a quiet empty
    scope. The caller decides whether either is a hard failure (an
    explicit `--manifest-path`) or should fail open (no flag given)."""
    text = manifest_path.read_text()
    manifest_dir = manifest_path.parent.as_posix()
    if manifest_dir == ".":
        manifest_dir = ""
    if manifest_path.is_absolute() or ".." in Path(manifest_dir).parts:
        raise ValueError(
            f"manifest directory {manifest_dir!r} (from {manifest_path}) is not "
            "repository-root-relative — it can never prefix a diff's own "
            "repo-root-relative path, so every line would silently read as "
            "unreachable"
        )
    return manifest_dir, parse_workspace_exclude(text)


def reachable_predicate(manifest_dir: str, excluded_prefixes: list[str]):
    """Build the `reachable(path) -> bool` predicate `count_added_lines`
    takes: True iff `path` (repo-root-relative, as it appears in a diff's
    `+++ b/<path>` header) is under `manifest_dir` and not under any of
    `excluded_prefixes` (each relative to `manifest_dir`)."""

    def reachable(path: str) -> bool:
        posix_path = Path(path).as_posix()
        if manifest_dir:
            prefix = manifest_dir + "/"
            if not posix_path.startswith(prefix):
                return False
            scoped = posix_path[len(prefix) :]
        else:
            scoped = posix_path
        return not any(
            scoped == excl or scoped.startswith(excl + "/") for excl in excluded_prefixes
        )

    return reachable


def count_added_lines(diff_text: str, reachable=None) -> tuple[int, int]:
    """Return (added, countable) over a unified diff.

    `added` is every added line, the number the workflow's old `grep -cE
    '^\\+([^+]|$)'` produced. `countable` is the subset that could plausibly
    have produced a mutant, and is the one the floor asserts on. `+++ b/path`
    file headers are not added lines and are excluded from both.

    (#2582) Walks the diff's own structure — `diff --git` / `+++ b/<path>`
    file headers and `@@ ... @@` hunk headers — to know which file and which
    new-file line number each added line belongs to, so it can also exclude
    a file under a `tests/` directory entirely, or a line inside a
    `#[cfg(test)]` item (see the module comment for the exclusion mechanism
    and its known gaps).

    `reachable`, if given, is a `Callable[[str], bool]` (see
    `reachable_predicate` above) — a file for which it returns False is
    excluded wholesale, the same way a `tests/` file already is (#2544: a
    path outside this invocation's mutation scope can never produce a
    mutant regardless of what it contains)."""
    added = 0
    countable = 0
    file_path: str | None = None
    skip_file = False
    file_ranges: list[tuple[int, int]] | None = None
    hunk_all_test = False
    new_line = 0

    for line in diff_text.splitlines():
        if line.startswith("diff --git "):
            # A new file entry starts here; `+++` (below) sets the real
            # state. Reset defensively so a file entry with no `+++` at all
            # (a binary-only diff) cannot leak the PREVIOUS file's state.
            file_path, skip_file, file_ranges, hunk_all_test = None, False, None, False
            continue
        if line.startswith("+++"):
            raw = line[4:] if line.startswith("+++ ") else ""
            if raw.startswith("b/"):
                raw = raw[2:]
            if raw in ("", "/dev/null"):
                file_path, skip_file, file_ranges = None, False, None
            else:
                file_path = raw
                skip_file = "tests" in Path(raw).parent.parts
                if not skip_file and reachable is not None and not reachable(raw):
                    skip_file = True  # (#2544) out of this invocation's mutation scope
                if skip_file:
                    file_ranges = None
                else:
                    if raw not in _TEST_RANGE_CACHE:
                        _TEST_RANGE_CACHE[raw] = _test_module_ranges(Path(raw))
                    file_ranges = _TEST_RANGE_CACHE[raw]
            continue
        if line.startswith("@@"):
            m = _HUNK_RE.match(line)
            if m:
                new_line = int(m.group(1))
                hint = m.group(2) or ""
                # Only meaningful when we have no per-line answer from the
                # file itself — see the module comment.
                hunk_all_test = file_ranges is None and bool(_TEST_MOD_HINT_RE.search(hint))
            # A malformed header leaves `new_line` (and any per-line
            # classification depending on it) stale for this hunk — better
            # than crashing on an input `git` itself produced.
            continue
        if not line.startswith("+"):
            if line.startswith(" "):
                new_line += 1  # a context line: exists in the new file too
            continue  # a removed ('-') line, or a "\ No newline" marker
        added += 1
        if skip_file:
            new_line += 1
            continue
        if file_ranges is not None:
            excluded = any(start <= new_line <= end for start, end in file_ranges)
        else:
            excluded = hunk_all_test
        if not excluded and added_line_is_countable(line[1:]):
            countable += 1
        new_line += 1
    return added, countable


def count_changed_lines_main(args: list[str]) -> int:
    """`--count-changed-lines <diff> [--manifest-path <path>]`: print the
    countable total on stdout (the workflow captures it) and the breakdown
    on stderr (the job log reads it).

    Anything that goes wrong exits non-zero rather than printing a 0. A guard
    that cannot read its input must fail the step, not silently disarm itself —
    which is the whole shape of #1716.

    `--manifest-path`, when given, scopes the count to what THAT manifest's
    `cargo mutants` invocation can reach (#2544) — see the module comment
    above `parse_workspace_exclude`. Omitted entirely, behavior is unchanged
    from before #2544: every added line counts, regardless of path. An
    UNREADABLE `--manifest-path`, or one that resolves to a directory that
    is not repository-root-relative (#2602 — an absolute path or a `..`
    parent segment; see `manifest_scope`), is a hard failure (exit 2), the
    same as an unreadable diff — a typo'd or malformed flag must not
    silently narrow the floor to a directory nothing lives under."""
    args = list(args)
    reachable = None
    if "--manifest-path" in args:
        mi = args.index("--manifest-path")
        if mi + 1 >= len(args):
            print("--manifest-path requires a path to a Cargo.toml", file=sys.stderr)
            return 2
        manifest_arg = args[mi + 1]
        del args[mi : mi + 2]
        try:
            manifest_dir, excluded_prefixes = manifest_scope(Path(manifest_arg))
        except OSError as exc:
            print(f"--manifest-path could not read {manifest_arg}: {exc}", file=sys.stderr)
            return 2
        except ValueError as exc:
            print(f"--manifest-path {manifest_arg} is invalid: {exc}", file=sys.stderr)
            return 2
        reachable = reachable_predicate(manifest_dir, excluded_prefixes)

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
    added, countable = count_added_lines(text, reachable=reachable)
    print(countable)
    print(
        f"{added} added Rust line(s) in scope; {countable} could plausibly produce a "
        f"mutant ({added - countable} blank / structure-only / comment-only / "
        "attribute-only / outside this invocation's mutation scope)",
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
        # `crates/**` diff now reaches this counter for the first time. This
        # is the same real-code shape as `_DIFF_REAL_CODE` above, just under
        # a `crates/` path, proving that widening held rather than assuming
        # it from the src/ cases alone. (#2582) The counter now DOES look at
        # the path — for the `tests/` directory and `#[cfg(test)]`
        # exclusions — but self-test runs isolated (`cwd` is an empty
        # tempdir, no `source_files` supplied for this case), so there is no
        # file to read at this path, the module exclusion cannot fire, and
        # the outcome is unchanged.
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


# ---------------------------------------------------------------------------
# (#2582) The bug this was filed for: PR #2579 added two inline unit tests
# and nothing else, and the floor read the legitimate zero as the
# package-scoping defect it exists to catch. `_MOD_TESTS_SOURCE` below is a
# minimal, faithful reproduction of that PR's SECOND hunk specifically — not
# the easy one. `source_files` writes real content to disk so these cases
# exercise the PRIMARY mechanism (`_test_module_ranges` reading the file),
# not the hunk-header fallback.
# ---------------------------------------------------------------------------

# The new-file content a `#[cfg(test)] mod tests` grows a second test inside.
# Line 5-7 is a plain (non-raw) string literal spanning three physical lines
# — `line two` at column 0 is deliberate: it is what makes git's own
# hunk-header heuristic (nearest column-0 line above the hunk) name `line
# two` as context instead of `mod tests {`, exactly as it did on PR #2579's
# real second hunk. If this fix relied on the header, this case would fail.
_MOD_TESTS_SOURCE = (
    "#[cfg(test)]\n"
    "mod tests {\n"
    "    #[test]\n"
    "    fn existing_test() {\n"
    "        let multi = \"line one\n"
    "line two\n"
    "line three\";\n"
    "        assert_eq!(multi.len(), 5);\n"
    "    }\n"
    "\n"
    "    #[test]\n"
    "    fn new_test_still_zero() {\n"
    "        assert_eq!(2 + 2, 4);\n"
    "    }\n"
    "}\n"
)

# The diff that grew `_MOD_TESTS_SOURCE` from its 10-line predecessor. Old
# start=7 (context begins mid-string, at "line three";") is what makes git
# resolve the header context from `line two` at old line 6 rather than the
# real enclosing `mod tests {` at old line 2.
_DIFF_MOD_TESTS = (
    "--- a/crates/fake/src/example.rs\n"
    "+++ b/crates/fake/src/example.rs\n"
    "@@ -7,4 +7,9 @@ line two\n"
    " line three\";\n"
    "         assert_eq!(multi.len(), 5);\n"
    "     }\n"
    "+\n"
    "+    #[test]\n"
    "+    fn new_test_still_zero() {\n"
    "+        assert_eq!(2 + 2, 4);\n"
    "+    }\n"
    " }\n"
)

_PROD_ONLY_SOURCE = "pub fn helper() -> usize {\n    1 + 1\n}\n"

_DIFF_PROD_ONLY = (
    "--- a/crates/fake/src/prod_only.rs\n"
    "+++ b/crates/fake/src/prod_only.rs\n"
    "@@ -0,0 +1,3 @@\n"
    "+pub fn helper() -> usize {\n"
    "+    1 + 1\n"
    "+}\n"
)

TEST_MODULE_SELF_TEST_CASES = [
    {
        # This is the reproduction of #2582 / PR #2579 itself: a diff whose
        # ONLY added lines are inside `#[cfg(test)] mod tests`, in a hunk
        # whose own header context is misleading. Before the fix this
        # counted 2 (the new fn's signature + its assert line) and failed
        # the floor exactly like the real PR did.
        "name": "#2582: a test-only diff (misleading hunk header) counts zero and the floor passes",
        "diff": _DIFF_MOD_TESTS,
        "source_files": {"crates/fake/src/example.rs": _MOD_TESTS_SOURCE},
        "expect_count": 0,
        "expect_gate": 0,
    },
    {
        # The other half of the same fix: a production-only diff must count
        # exactly as it did before #2582 — the module exclusion must not
        # over-fire on ordinary code just because the counter now reads the
        # file. Same file is on disk (proving the counter DID look) and
        # carries no `#[cfg(test)]` at all.
        "name": "#2582: a production-only diff counts unchanged and the floor still fails",
        "diff": _DIFF_PROD_ONLY,
        "source_files": {"crates/fake/src/prod_only.rs": _PROD_ONLY_SOURCE},
        "expect_count": 2,
        "expect_gate": 1,
    },
    {
        # The case that MUST stay red: a diff mixing a test-only hunk (zero
        # contribution) with a production hunk that added real mutable code
        # cargo-mutants found no mutants for. Proves the fix excludes the
        # TEST half only — it must never zero out an honest under-scoping
        # defect just because part of the same diff happens to be tests.
        "name": "#2582: a mixed diff counts only the production half, and the floor still fails",
        "diff": _DIFF_MOD_TESTS + _DIFF_PROD_ONLY,
        "source_files": {
            "crates/fake/src/example.rs": _MOD_TESTS_SOURCE,
            "crates/fake/src/prod_only.rs": _PROD_ONLY_SOURCE,
        },
        "expect_count": 2,
        "expect_gate": 1,
    },
    {
        # The other exclusion #2582 asked for: a whole file under a `tests/`
        # directory (Cargo's integration-test convention) is skipped by PATH
        # alone — no `source_files` here, proving this does not depend on
        # reading the file at all. Without the fix this would count 2 (the
        # `fn` signature and the `assert_eq!` line; the `#[test]` attribute
        # and the closing `}` already excluded on their own).
        "name": "#2582: a file under tests/ is excluded wholesale, by path alone",
        "diff": (
            "--- a/tests/golden_check.rs\n"
            "+++ b/tests/golden_check.rs\n"
            "@@ -0,0 +1,4 @@\n"
            "+#[test]\n"
            "+fn golden_matches() {\n"
            "+    assert_eq!(2 + 2, 4);\n"
            "+}\n"
        ),
        "expect_count": 0,
        "expect_gate": 0,
    },
]

COUNT_SELF_TEST_CASES += TEST_MODULE_SELF_TEST_CASES


# ---------------------------------------------------------------------------
# (#2544) The manifest-scoping half: `--manifest-path` and the exclusion list
# it derives from a `[workspace] exclude` array, reproducing the exact
# failure #2544 proved on #2599 (a diff entirely under `runtime/`, floor
# counted it as reachable by the root `--workspace` invocation, that
# invocation reported zero mutants — legitimately, since it structurally
# cannot see `runtime/` — and the gate failed a diff nothing was wrong
# with).
# ---------------------------------------------------------------------------

# The real repo's root manifest shape, reproduced (not read from disk — see
# the "run isolated" comment on `count_self_test`).
_ROOT_MANIFEST = (
    "[workspace]\n"
    'members = [".", "crates/darkmux-types"]\n'
    "# comment between members and exclude, like the real file\n"
    'exclude = ["runtime", "plugins/darkmux-bundler-rust", "tools/darkmux-mock-model"]\n'
    "\n"
    "[package]\n"
    'name = "darkmux"\n'
)

# `runtime/Cargo.toml`'s real shape: its own empty `[workspace]` table (makes
# it a standalone crate root), no `exclude` — see the file's own comment for
# why (needs to resolve independently of the parent workspace).
_RUNTIME_MANIFEST = "[workspace]\n\n[package]\nname = \"darkmux-runtime\"\n"

# A manifest with no `[workspace]` table at all — the fail-open case: nothing
# can be excluded when there is nothing to read it from.
_NO_WORKSPACE_MANIFEST = '[package]\nname = "standalone"\n'

_DIFF_RUNTIME_ONLY = (
    "--- a/runtime/src/loop_runner.rs\n"
    "+++ b/runtime/src/loop_runner.rs\n"
    "@@ -10,3 +10,6 @@\n"
    "+\n"
    "+pub fn helper() -> usize {\n"
    "+    1 + 1\n+}\n"
)

_DIFF_ROOT_SRC_ONLY = (
    "--- a/src/mission_status.rs\n"
    "+++ b/src/mission_status.rs\n"
    "@@ -10,3 +10,6 @@\n"
    "+\n"
    "+pub fn helper() -> usize {\n"
    "+    1 + 1\n+}\n"
)

MANIFEST_SCOPE_SELF_TEST_CASES = [
    {
        # THE #2544 REPRODUCTION. Without the fix, this diff counts 2 (real
        # code) against the root manifest and fails the gate — exactly the
        # false failure proven on #2599. With the fix, `runtime/` is outside
        # the root manifest's own `[workspace] exclude`, so the floor reads
        # this as legitimately unreachable and the gate PASSES.
        "name": "#2544: a runtime-only diff is invisible to the root-manifest floor, and the gate passes",
        "diff": _DIFF_RUNTIME_ONLY,
        "manifest_path": "Cargo.toml",
        "manifest_content": _ROOT_MANIFEST,
        "manifest_arg": "Cargo.toml",
        "expect_count": 0,
        "expect_gate": 0,
    },
    {
        # The other half: an ordinary src/ diff is completely unaffected by
        # the new exclusion mechanism — it is not under any excluded prefix.
        "name": "#2544: a normal src/ diff still counts fully under the root-manifest floor",
        "diff": _DIFF_ROOT_SRC_ONLY,
        "manifest_path": "Cargo.toml",
        "manifest_content": _ROOT_MANIFEST,
        "manifest_arg": "Cargo.toml",
        "expect_count": 2,
        "expect_gate": 1,
    },
    {
        # THE SECOND INVOCATION'S floor: the SAME runtime-only diff, scoped
        # to `runtime/Cargo.toml` instead. This manifest's own `[workspace]`
        # is empty (no exclude), and its directory IS `runtime`, so the
        # lines are now reachable and the floor is armed — proving the
        # runtime invocation's own floor actually counts what it should,
        # not just that the root floor correctly ignores it.
        "name": "#2544: the same runtime-only diff counts fully under runtime/Cargo.toml's own floor",
        "diff": _DIFF_RUNTIME_ONLY,
        "manifest_path": "runtime/Cargo.toml",
        "manifest_content": _RUNTIME_MANIFEST,
        "manifest_arg": "runtime/Cargo.toml",
        "expect_count": 2,
        "expect_gate": 1,
    },
    {
        # A file OUTSIDE runtime/ is equally invisible to the runtime
        # manifest's own invocation — the exclusion is symmetric, not just
        # "runtime is special".
        "name": "#2544: a root src/ diff is invisible to the runtime-manifest floor",
        "diff": _DIFF_ROOT_SRC_ONLY,
        "manifest_path": "runtime/Cargo.toml",
        "manifest_content": _RUNTIME_MANIFEST,
        "manifest_arg": "runtime/Cargo.toml",
        "expect_count": 0,
        "expect_gate": 0,
    },
    {
        # Fail-open: a manifest with no `[workspace]` table at all excludes
        # nothing, so the runtime-only diff counts as if unscoped — the
        # heuristic never invents an exclusion it cannot read.
        "name": "#2544: a manifest with no [workspace] table excludes nothing (fails open)",
        "diff": _DIFF_RUNTIME_ONLY,
        "manifest_path": "Cargo.toml",
        "manifest_content": _NO_WORKSPACE_MANIFEST,
        "manifest_arg": "Cargo.toml",
        "expect_count": 2,
        "expect_gate": 1,
    },
    {
        # An explicit `--manifest-path` that cannot be read at all is a hard
        # failure, not a silent empty-exclude-list — a typo'd flag must not
        # quietly shrink the floor's reachable scope to a directory nothing
        # lives under and pass every PR that touches it.
        "name": "#2544: an unreadable --manifest-path fails loudly, not open",
        "diff": _DIFF_RUNTIME_ONLY,
        "manifest_arg": "does-not-exist/Cargo.toml",
        "expect_exit": 2,
    },
]

COUNT_SELF_TEST_CASES += MANIFEST_SCOPE_SELF_TEST_CASES


# ---------------------------------------------------------------------------
# (#2602) The frontier-review follow-up half: the exact reproduction of the
# comment-inside-the-exclude-array bug (the one that turns a real red into a
# false green), the manifest-directory validation, and the exclude-entry
# normalization forms.
# ---------------------------------------------------------------------------

# The reviewer's exact reproduction: a `#` comment sharing a line with an
# exclude array's own quoted content. The old regex-based extraction handed
# this whole bracketed body to a bare quoted-string finder with no notion of
# a TOML comment, so `"src"` inside the prose was picked up as a THIRD
# excluded path — dropping the root floor for an ordinary `src/` diff from 2
# to 0, silently.
_ROOT_MANIFEST_WITH_ARRAY_COMMENT = (
    "[workspace]\n"
    'members = [".", "crates/darkmux-types"]\n'
    "exclude = [\n"
    '  "runtime",           # keeps "src" out of the parent build? no - just prose\n'
    '  "tools/darkmux-mock-model",\n'
    "]\n"
    "\n"
    "[package]\n"
    'name = "darkmux"\n'
)

# The three normalization forms named by the review, all on one manifest:
# a trailing slash, a leading `./`, and a trailing glob.
_NORMALIZE_MANIFEST = (
    "[workspace]\n"
    'members = ["."]\n'
    'exclude = ["runtime/", "./plugins/bundler", "tools/mock/*"]\n'
    "\n"
    "[package]\n"
    'name = "darkmux"\n'
)

_DIFF_PLUGINS_BUNDLER_ONLY = (
    "--- a/plugins/bundler/src/lib.rs\n"
    "+++ b/plugins/bundler/src/lib.rs\n"
    "@@ -10,3 +10,6 @@\n"
    "+\n"
    "+pub fn helper() -> usize {\n"
    "+    1 + 1\n+}\n"
)

_DIFF_TOOLS_MOCK_ONLY = (
    "--- a/tools/mock/src/lib.rs\n"
    "+++ b/tools/mock/src/lib.rs\n"
    "@@ -10,3 +10,6 @@\n"
    "+\n"
    "+pub fn helper() -> usize {\n"
    "+    1 + 1\n+}\n"
)

# Invalid TOML (a duplicate `exclude` key in the same table) that still
# leaves the `[workspace]` header and the FIRST `exclude = [...]` intact, so
# the regex fallback can still recover something — proving the fallback path
# actually engages (and warns) rather than only existing in theory.
_MALFORMED_MANIFEST_DUPLICATE_KEY = (
    "[workspace]\n"
    'exclude = ["runtime"]\n'
    'exclude = ["oops-this-is-invalid-toml"]\n'
    "\n"
    "[package]\n"
    'name = "darkmux"\n'
)

# Valid TOML, but `exclude` is not an array of strings — a shape `tomllib`
# parses cleanly but this script cannot use.
_BAD_EXCLUDE_TYPE_MANIFEST = (
    "[workspace]\n"
    "exclude = true\n"
    "\n"
    "[package]\n"
    'name = "darkmux"\n'
)

MANIFEST_PARSE_SELF_TEST_CASES = [
    {
        # THE MUST-FIX REPRODUCTION. Without the fix, `"src"` inside the
        # comment is parsed as a real exclusion and this diff (entirely
        # under `src/`, nowhere near the comment) reads as 0 instead of 2 —
        # the root floor going silently blind on an ordinary source diff, in
        # the exact repository that comments this array heavily.
        "name": "#2602: a comment inside the exclude array's own text is not parsed as an exclusion",
        "diff": _DIFF_ROOT_SRC_ONLY,
        "manifest_path": "Cargo.toml",
        "manifest_content": _ROOT_MANIFEST_WITH_ARRAY_COMMENT,
        "manifest_arg": "Cargo.toml",
        "expect_count": 2,
        "expect_gate": 1,
    },
    {
        # The other half of the same manifest: the two REAL exclusions
        # (`runtime`, `tools/darkmux-mock-model`) still work — the fix isn't
        # just "ignore the array", it is "parse it correctly".
        "name": "#2602: the same comment-bearing manifest still excludes its real entries",
        "diff": _DIFF_RUNTIME_ONLY,
        "manifest_path": "Cargo.toml",
        "manifest_content": _ROOT_MANIFEST_WITH_ARRAY_COMMENT,
        "manifest_arg": "Cargo.toml",
        "expect_count": 0,
        "expect_gate": 0,
    },
    {
        "name": "#2602: a trailing slash on an exclude entry still excludes it",
        "diff": _DIFF_RUNTIME_ONLY,
        "manifest_path": "Cargo.toml",
        "manifest_content": _NORMALIZE_MANIFEST,
        "manifest_arg": "Cargo.toml",
        "expect_count": 0,
        "expect_gate": 0,
    },
    {
        "name": "#2602: a leading ./ on an exclude entry still excludes it",
        "diff": _DIFF_PLUGINS_BUNDLER_ONLY,
        "manifest_path": "Cargo.toml",
        "manifest_content": _NORMALIZE_MANIFEST,
        "manifest_arg": "Cargo.toml",
        "expect_count": 0,
        "expect_gate": 0,
    },
    {
        "name": "#2602: a trailing glob on an exclude entry still excludes it",
        "diff": _DIFF_TOOLS_MOCK_ONLY,
        "manifest_path": "Cargo.toml",
        "manifest_content": _NORMALIZE_MANIFEST,
        "manifest_arg": "Cargo.toml",
        "expect_count": 0,
        "expect_gate": 0,
    },
    {
        # An absolute --manifest-path is rejected before it can silently
        # zero the floor: its directory can never prefix a diff's own
        # repo-root-relative path, so every line would read as unreachable.
        "name": "#2602: an absolute --manifest-path is rejected, not silently scoped to nothing",
        "diff": _DIFF_ROOT_SRC_ONLY,
        "manifest_path": "sub/Cargo.toml",
        "manifest_content": _ROOT_MANIFEST,
        "manifest_arg_absolute": "sub/Cargo.toml",
        "expect_exit": 2,
        "expect_stderr_contains": ["is invalid", "repository-root-relative"],
    },
    {
        # A --manifest-path containing a `..` parent segment is rejected the
        # same way, even when the file it names is perfectly readable.
        "name": "#2602: a --manifest-path with a parent (..) segment is rejected",
        "diff": _DIFF_ROOT_SRC_ONLY,
        "manifest_path": "Cargo.toml",
        "manifest_content": _ROOT_MANIFEST,
        "manifest_arg": "sub/../Cargo.toml",
        "source_files": {"sub/.keep": ""},
        "expect_exit": 2,
        "expect_stderr_contains": ["is invalid", "repository-root-relative"],
    },
    {
        # A manifest that fails to parse as TOML at all warns on stderr and
        # falls back to the regex extractor, rather than silently returning
        # an empty exclusion list with no hint the parser gave up.
        "name": "#2602: a manifest that fails to parse as TOML warns and falls back",
        "diff": _DIFF_RUNTIME_ONLY,
        "manifest_path": "Cargo.toml",
        "manifest_content": _MALFORMED_MANIFEST_DUPLICATE_KEY,
        "manifest_arg": "Cargo.toml",
        "expect_count": 0,
        "expect_gate": 0,
        "expect_stderr_contains": ["did not parse as TOML", "falling back to regex"],
    },
    {
        # A `[workspace]` table found with an `exclude` key that parses as
        # TOML but is not a list of strings warns and fails open (nothing
        # excluded), rather than crashing or silently doing nothing.
        "name": "#2602: an exclude key that isn't a list of strings warns and fails open",
        "diff": _DIFF_RUNTIME_ONLY,
        "manifest_path": "Cargo.toml",
        "manifest_content": _BAD_EXCLUDE_TYPE_MANIFEST,
        "manifest_arg": "Cargo.toml",
        "expect_count": 2,
        "expect_gate": 1,
        "expect_stderr_contains": ["is not a list of strings"],
    },
]

COUNT_SELF_TEST_CASES += MANIFEST_PARSE_SELF_TEST_CASES


def _run_self(argv: list[str], cwd: Path | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(Path(__file__).resolve()), *argv],
        capture_output=True,
        text=True,
        cwd=cwd,
    )


def count_self_test() -> list[str]:
    failures = []
    for case in COUNT_SELF_TEST_CASES:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            # (#2582) Run every case with `cwd` pinned to this EMPTY tempdir,
            # never the real repo. `_test_module_ranges` reads files by the
            # relative path named in the diff's `+++ b/<path>` header, and
            # several of these fixture diffs deliberately reuse real-looking
            # repo paths (`src/mission_status.rs`,
            # `crates/darkmux-eureka/src/lib.rs`) as realistic examples. If
            # this ran from the repo root, the counter would read the ACTUAL
            # files at those paths instead of the fixture's synthetic diff —
            # `darkmux-eureka/src/lib.rs` genuinely has a `#[cfg(test)]`
            # module, so a case with no relation to it would silently start
            # asserting on real repo content instead of the diff under test.
            # A case that wants the file-read mechanism exercised supplies
            # `source_files` to write real content into this same tempdir.
            diff_path = tmp_path / "pr.diff"
            diff_path.write_text(case["diff"])
            for rel, content in case.get("source_files", {}).items():
                dest = tmp_path / rel
                dest.parent.mkdir(parents=True, exist_ok=True)
                dest.write_text(content)
            # (#2544) Optional manifest fixture — write it at `manifest_path`
            # (relative to this same isolated tempdir) when supplied, and
            # pass `--manifest-path <manifest_arg>` on the counter's argv.
            # `manifest_arg` may legitimately name a path with NOTHING
            # written there (the unreadable-manifest case), which is the
            # point of that case.
            if "manifest_content" in case:
                mdest = tmp_path / case["manifest_path"]
                mdest.parent.mkdir(parents=True, exist_ok=True)
                mdest.write_text(case["manifest_content"])
            count_argv = ["--count-changed-lines", str(diff_path)]
            if "manifest_arg" in case:
                count_argv += ["--manifest-path", case["manifest_arg"]]
            elif "manifest_arg_absolute" in case:
                # (#2602) An ABSOLUTE path, computed at run time — the
                # rejection under test depends on the argument genuinely
                # being absolute, which a hardcoded case dict cannot express
                # since the tempdir's own path isn't known until now.
                count_argv += [
                    "--manifest-path",
                    str(tmp_path / case["manifest_arg_absolute"]),
                ]
            expect_exit = case.get("expect_exit", 0)
            problems = []

            proc = _run_self(count_argv, cwd=tmp_path)
            got = proc.stdout.strip()
            if proc.returncode != expect_exit:
                problems.append(
                    f"counter exited {proc.returncode}, expected {expect_exit}: "
                    f"{proc.stderr.strip()}"
                )
            elif expect_exit != 0:
                pass  # a deliberate hard failure — no count/gate to check
            elif got != str(case["expect_count"]):
                problems.append(f"counted {got!r}, expected {case['expect_count']}")

            # (#2602) Optional stderr-substring assertions — for the
            # warn-on-stderr cases (a manifest that fails to parse as TOML,
            # an `exclude` key of the wrong shape, an invalid --manifest-path)
            # a passing exit code / count alone would not prove the operator
            # is actually shown a hint the parser gave up.
            for needle in case.get("expect_stderr_contains", []):
                if needle not in proc.stderr:
                    problems.append(f"stderr is missing {needle!r}: {proc.stderr.strip()}")

            # Hand the count straight to the floor, the way the workflow does:
            # exit 0, no output directory, i.e. "ran and mutated nothing".
            if expect_exit == 0 and proc.returncode == 0:
                gate = _run_self(
                    [
                        "0",
                        "diff",
                        "T",
                        "--changed-lines",
                        got,
                        str(tmp_path / "nope" / "mutants.out"),
                    ],
                    cwd=tmp_path,
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
