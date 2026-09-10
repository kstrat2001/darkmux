#!/usr/bin/env python3
"""env-audit-report.py (#2632) — cross-reference DARKMUX_*-derived test
readers against DARKMUX_*-derived test writers and each reader's
`#[serial_test::serial]` annotation.

## The bug class this exists to catch

`serial_test::serial` only serializes tests that carry it. A test that
mutates a `DARKMUX_*` env var IS guarded against every OTHER serial test —
but not against a test that reads the same (or a derived) value without the
annotation. A guarded writer racing an unguarded reader is still a race
(#2632: `dialectic_seats_contract` observed a `TempDir` another, unrelated
test had already dropped and deleted; `liveness_dir_and_host_sampler_lock_
path_are_test_isolated` observed its own two `liveness_dir()` calls resolve
to two different roots within one test run; seven `darkmux-crew::scheduler`
tests raced `bounded_command`'s two `DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS`-
mutating tests on a worker thread three `thread::scope` hops below their own
— found only after this script stopped silently discarding unnamed-thread
reads, see "Three honesty fixes" below).

Reading test source and guessing which tests are "probably" affected misses
tests that reach the env indirectly through a call chain (a test calling
`load_roles()`, which calls `roles_dir()`, which resolves through
`paths::resolve()` — nothing at the test's own call site names an env var at
all). This tool instruments the resolution chokepoints instead, so the
READ side of the report is a real runtime registry walk, not a guess.

## Two different kinds of "mechanical" in this report — read this before
## trusting either half

**The READER half is a real registry walk (dynamic).** `darkmux_types::
env_audit::audit_env_read` is compiled into `darkmux-types` under
`#[cfg(any(test, feature = "test-support"))]` and called from every
instrumented resolution chokepoint (`config_access::env_str`, `paths::
resolve` + `paths::paths_from_root`, `dispatch_liveness::liveness_dir`,
`residency_lease::residency_dir`, and — a fifth, in `darkmux-profiles` —
`profiles::default_locations` + `profiles::load_registry`; see
`crates/darkmux-types/src/env_audit.rs` for the exact list, the reasoning
for stopping there, and the KNOWN GAPS it names honestly). Set
`DARKMUX_ENV_AUDIT_LOG=<path>` before running `cargo test -p darkmux-types
--lib` and `cargo test -p darkmux-crew --lib`; every `DARKMUX_*` read
through a chokepoint appends one `<test-thread-name>\\t<key>` line (`cargo
test`'s harness names each test's thread after its fully-qualified test
path — a WORKER thread a production code path spawns off that test thread
is unnamed by default, and `darkmux-crew::concurrent_dispatch::
spawn_scoped_named` propagates the name across those boundaries where it's
been wired in; see the KNOWN GAPS in `env_audit.rs`'s module doc for where
it hasn't). This is exhaustive for anything that actually runs through one
of those chokepoints AND whose reporting thread has a name this script can
resolve to source, and LOUD (not silently blind) about everything else — see
"Three honesty fixes" below. A NEW direct `std::env::var("DARKMUX_...")`
call added outside every chokepoint (production code, not a test's own
save/restore idiom) still won't show up here until it's wired in too — that
gap remains genuinely invisible, unlike the two this script now surfaces.

**The WRITER half (which keys are ever mutated) and the SERIAL-ANNOTATION
check are text scans — call them what they are: lints.** "Is `DARKMUX_HOME`
ever `set_var`'d in a test" and "does this test carry `#[serial]` or
`#[serial_test::serial]`" are both syntactic facts (a literal call, a literal
attribute) with no indirection to miss, so they're low-risk lints — but they
ARE lints, not a walk of anything that runs. A key mutated only through a
helper this script doesn't recognize (some indirection beyond the plain
`set_var("DARKMUX_X", ...)` / `remove_var("DARKMUX_X")` and the `let k =
"DARKMUX_X"; ...set_var(k, ...)` forms below) would be silently missed here.

## Three honesty fixes (this fix pass — an adversarial review of the first
## cut proved both silent-pass bugs below with a live log)

1. **Unnamed-thread reads are no longer discarded.** A worker thread a
   production code path spawns (not `cargo test`'s own per-test thread) has
   no name by default, and the FIRST cut of this script `continue`d straight
   past any such line — meaning every env read on a spawned thread was
   invisible to the report, with no trace it had even been dropped. That is
   exactly how the `DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS` race above hid:
   154 of 3782 log lines began `<unnamed>` on one real sweep, covering 8
   keys, 3 of them genuinely mutated in-crate. This script now buckets an
   unnamed-thread read of a mutated key as UNATTRIBUTABLE, prints it, and
   FAILS the run on it — because "I can't tell if this is guarded" is not
   the same claim as "this is guarded," and a `continue` was making the two
   indistinguishable in the exit code.
2. **A source-match miss (`NO-SOURCE-MATCH`) is no longer a silent pass
   either.** `is_serial()` returns `None` when it can't find the reading
   test's `fn` under `SRC_DIRS` — e.g. because `SRC_DIRS` doesn't cover the
   crate the test actually lives in. The first cut's `main()` only ever
   checked `ok is False`, so `ok is None` fell through to neither branch and
   the test was silently treated as clean. Proven on one real log: pointing
   `SRC_DIRS` at this script's own default (types + crew) reported 0
   unguarded and exit 0 for a set of `darkmux-flow` tests it could not even
   locate the source for; pointing `SRC_DIRS` at `darkmux-flow/src` instead
   found 34 of them genuinely unguarded, exit 1. This script now buckets a
   `None` result as UNVERIFIABLE, prints it, and fails the run — refusing to
   claim a verdict for a test it was not pointed at, rather than reporting
   "clean" by omission.
3. **`SRC_DIRS` is printed every run.** Making the swept set visible is the
   other half of fix 2 — a reader should never have to infer coverage from
   an absence of findings.

**Scope, stated plainly:** this script sweeps `darkmux-types` +
`darkmux-crew` only (`SRC_DIRS` below). `darkmux-flow`, `darkmux-lab`,
`darkmux-serve`, `darkmux-doctor`, `darkmux-fleet`, top-level `src/`, and the
`runtime/` crate (a separate Docker-image binary, not linked into this
workspace, so this instrumentation cannot reach it at all) are UNSWEPT —
narrowing `SRC_DIRS` to run this script against one of them is possible (see
fix 2's proof above) but not done as part of the default invocation, and
leaving them unswept is a scope decision, not a claim that they're clean.

## Usage

    DARKMUX_ENV_AUDIT_LOG=/tmp/env-audit.log cargo test -p darkmux-types --lib
    DARKMUX_ENV_AUDIT_LOG=/tmp/env-audit.log cargo test -p darkmux-crew --lib
    python3 scripts/env-audit-report.py /tmp/env-audit.log

Run it after adding a new test that touches `DARKMUX_*`-derived state, or
periodically as a sweep. Exits 1 if it finds an unguarded reader of a
mutated key, an UNATTRIBUTABLE (unnamed-thread) read of a mutated key, or an
UNVERIFIABLE (`NO-SOURCE-MATCH`) reader; 0 otherwise (including when the
audit log is empty — an empty run proves nothing, so pair a `0` exit with
checking the passed-in log actually has content before trusting it, and with
checking `SRC_DIRS` above actually covers what you meant to sweep).

NOT wired into CI as of #2632 — doing that well means setting
`DARKMUX_ENV_AUDIT_LOG` during CI's own `cargo test` runs (near-zero extra
cost, reusing the existing run) rather than a second dedicated run; left as
a named follow-up rather than done silently or done expensively.
"""

import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# Grouped by the `cargo test -p <crate>` PROCESS each dir's tests actually
# run in (per this script's own "Usage" section: types and crew are swept
# with two SEPARATE `cargo test -p` invocations, merged into one log
# afterward). A `set_var` in one crate's own `#[cfg(test)]` module runs in
# THAT crate's test binary only — a fully separate OS process from the
# other crate's test binary, with its own copy of the environment — so it
# can never race a reader that only ever executes in the OTHER process.
# `find_mutated_keys()` and `main()` both key off this grouping (self-
# discovered while re-running the enumeration for this fix pass: an
# ungrouped merge falsely flagged 23 `darkmux-crew::scheduler` tests as
# racing `DARKMUX_DISPATCH_FREE_CONCURRENCY` against a write that exists
# ONLY in `darkmux-types::config_access`'s own test module — two different
# processes, never a real race).
CRATE_DIRS: dict[str, list[Path]] = {
    "darkmux-types": [REPO_ROOT / "crates/darkmux-types/src"],
    "darkmux-crew": [
        REPO_ROOT / "crates/darkmux-crew/src",
        REPO_ROOT / "crates/darkmux-crew/tests",
    ],
}
SRC_DIRS = [d for dirs in CRATE_DIRS.values() for d in dirs]

MUTATOR_RE = re.compile(r'(?:set_var|remove_var)\(\s*"(DARKMUX_[A-Z0-9_]+)"')
INDIRECT_KEY_RE = re.compile(r'let\s+k\s*=\s*"(DARKMUX_[A-Z0-9_]+)"')
SERIAL_RE = re.compile(r"#\[\s*(serial_test::)?serial(\(|\])")

# What an unnamed-thread reader's thread field looks like in the log —
# `env_audit::audit_env_read` writes the literal `<unnamed>` when
# `std::thread::current().name()` returns `None`. An empty thread field
# (malformed line) is treated the same way: neither can be resolved to a
# test, so neither can be judged guarded.
UNATTRIBUTABLE_THREADS = ("<unnamed>", "")


def crate_for_path(path: Path) -> str | None:
    """Which `CRATE_DIRS` entry (hence which `cargo test -p` PROCESS) a
    source file belongs to, by directory prefix. `None` if it matches
    none — should not happen for anything `is_serial` finds via `SRC_DIRS`,
    but handled explicitly rather than assumed."""
    resolved = path.resolve()
    for crate, dirs in CRATE_DIRS.items():
        for d in dirs:
            if d.exists() and resolved.is_relative_to(d.resolve()):
                return crate
    return None


def find_mutated_keys() -> dict[str, set[str]]:
    """The WRITER half — a text scan (a lint; see the module doc above).

    Returns keys PER CRATE (see `CRATE_DIRS`'s doc for why the grouping
    matters): a key mutated only in one crate's own test module can never
    race a reader that only ever runs in the OTHER crate's test process."""
    by_crate: dict[str, set[str]] = {crate: set() for crate in CRATE_DIRS}
    for crate, dirs in CRATE_DIRS.items():
        for d in dirs:
            if not d.exists():
                continue
            for path in d.rglob("*.rs"):
                text = path.read_text(errors="ignore")
                by_crate[crate].update(MUTATOR_RE.findall(text))
                # `let k = "DARKMUX_X"; ...set_var(k, ...)` indirection: any
                # file that binds a DARKMUX_* literal to a local named `k`
                # and also calls `set_var`/`remove_var` at all is presumed
                # to mutate that key through the indirection — a coarse but
                # safe-in-practice over-approximation for a same-file-scoped
                # idiom.
                if re.search(r"(?:set_var|remove_var)\(k[,)]", text):
                    by_crate[crate].update(INDIRECT_KEY_RE.findall(text))
    return by_crate


def is_serial(short_name: str) -> tuple[bool | None, str, set[str]]:
    """(all_serial_or_None_if_no_match, location_string, crates_matched) —
    the SERIAL-ANNOTATION half, also a text scan (see module doc).

    Returns `(None, "NO-SOURCE-MATCH", set())` when `short_name` cannot be
    found as a `fn` under any `SRC_DIRS` entry — e.g. the reading test
    actually lives in a crate this script isn't pointed at. Callers MUST
    treat `None` as "cannot verify," never as "guarded" (fix 2 in the
    module doc above). `crates_matched` is which `CRATE_DIRS` entry/entries
    the hit(s) fell under — the caller uses it to check a reader only
    against mutations from its OWN crate's test process (see `CRATE_DIRS`'s
    doc)."""
    pattern = rf"fn {re.escape(short_name)}\b"
    hits: list[str] = []
    for d in SRC_DIRS:
        if not d.exists():
            continue
        out = subprocess.run(
            ["grep", "-rn", pattern, str(d)], capture_output=True, text=True
        ).stdout.strip()
        if out:
            hits.extend(out.splitlines())
    if not hits:
        return (None, "NO-SOURCE-MATCH", set())
    results = []
    crates: set[str] = set()
    for hit in hits:
        file_part, rest = hit.split(":", 1)
        lineno = int(rest.split(":", 1)[0])
        lines = Path(file_part).read_text().splitlines()
        window = lines[max(0, lineno - 8) : lineno]
        results.append((file_part, lineno, any(SERIAL_RE.search(ln) for ln in window)))
        c = crate_for_path(Path(file_part))
        if c:
            crates.add(c)
    return (
        all(r[2] for r in results),
        "; ".join(f"{f}:{ln}" for f, ln, _ in results),
        crates,
    )


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <audit-log-path>", file=sys.stderr)
        return 2
    log_path = Path(sys.argv[1])
    if not log_path.exists():
        print(f"no such audit log: {log_path}", file=sys.stderr)
        return 2

    mutated_by_crate = find_mutated_keys()
    all_mutated_keys = set().union(*mutated_by_crate.values()) if mutated_by_crate else set()
    raw_lines = log_path.read_text().splitlines()

    pairs: set[tuple[str, str]] = set()
    # (fix 1) Unattributable reads — a mutated key read on a thread this
    # script cannot resolve to a test — are counted, not discarded.
    unattributable_keys: Counter[str] = Counter()
    malformed = 0
    for line in raw_lines:
        if "\t" not in line:
            malformed += 1
            continue
        thread, key = line.split("\t", 1)
        # Coarse pre-filter: a key mutated in NEITHER crate can't race
        # anything regardless of process. The per-crate check below is
        # what actually decides guardedness.
        if key not in all_mutated_keys:
            continue
        if thread in UNATTRIBUTABLE_THREADS:
            unattributable_keys[key] += 1
            continue
        pairs.add((thread, key))

    tests = sorted({t for t, _ in pairs})
    unguarded: list[tuple[str, list[str], str]] = []
    # (fix 2) A named reader whose source this script can't locate is its
    # own bucket — NOT folded into "guarded" by falling through unchecked.
    unverifiable: list[tuple[str, list[str]]] = []
    # Cross-crate matches this script DROPPED because the only mutator of
    # that key lives in a crate that never shares a process with the
    # reader — reported for transparency, never treated as a finding.
    cross_crate_dropped: list[tuple[str, list[str]]] = []
    for t in tests:
        short = t.rsplit("::", 1)[-1]
        keys_seen = sorted({k for tt, k in pairs if tt == t})
        ok, loc, crates = is_serial(short)
        if ok is None:
            unverifiable.append((t, keys_seen))
            continue
        # A key only races this reader if SOME crate that reader's own `fn`
        # was found in also mutates it — see `CRATE_DIRS`'s module doc.
        racing_keys = sorted(
            {k for k in keys_seen if any(k in mutated_by_crate.get(c, set()) for c in crates)}
        )
        dropped = sorted(set(keys_seen) - set(racing_keys))
        if dropped:
            cross_crate_dropped.append((t, dropped))
        if not racing_keys:
            continue
        if ok is False:
            unguarded.append((t, racing_keys, loc))

    # (fix 3) Print the swept set every run — coverage should never have to
    # be inferred from an absence of findings.
    print(f"# swept source dirs ({len(SRC_DIRS)}), grouped by test PROCESS:")
    for crate, dirs in CRATE_DIRS.items():
        for d in dirs:
            marker = "" if d.exists() else "  (missing)"
            print(f"#   [{crate}] {d}{marker}")
    print(f"# {len(raw_lines)} total audit-log lines ({malformed} malformed, no tab)")
    print(f"# {len(tests)} distinct NAMED test threads read a mutated DARKMUX_* key")
    for crate, keys in mutated_by_crate.items():
        print(f"# {len(keys)} mutated keys found in {crate}'s own tests: {sorted(keys)}")

    total_unattributable = sum(unattributable_keys.values())
    print(f"\n## UNATTRIBUTABLE — unnamed-thread reads of a mutated key ({total_unattributable})\n")
    if unattributable_keys:
        print("# cannot be judged guarded or clean; see env_audit.rs's \"Known gaps\"")
        for key, count in sorted(unattributable_keys.items()):
            print(f"- {key}: {count} unnamed-thread read(s)")
    else:
        print("# none")

    print(f"\n## UNVERIFIABLE (NO-SOURCE-MATCH) ({len(unverifiable)})\n")
    if unverifiable:
        print("# reading test's fn could not be located under the swept source dirs above")
        for t, keys in unverifiable:
            print(f"- {t}  keys={keys}")
    else:
        print("# none")

    print(f"\n## UNGUARDED ({len(unguarded)})\n")
    for t, keys, loc in unguarded:
        print(f"- {t}  keys={keys}\n    {loc}")
    if not unguarded:
        print("# none")

    if cross_crate_dropped:
        print(f"\n## CROSS-CRATE MATCHES DROPPED, not races ({len(cross_crate_dropped)})\n")
        print(
            "# these keys are mutated ONLY in a crate that never shares a\n"
            "# process with this reader (see CRATE_DIRS's module doc) — not\n"
            "# a finding, listed so a dropped match is visible rather than\n"
            "# silently absent"
        )
        for t, keys in cross_crate_dropped:
            print(f"- {t}  non-racing keys={keys}")

    return 1 if (unguarded or unverifiable or unattributable_keys) else 0


if __name__ == "__main__":
    sys.exit(main())
