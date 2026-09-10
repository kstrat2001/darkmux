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
to two different roots within one test run).

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
resolve`, `dispatch_liveness::liveness_dir`, `residency_lease::
residency_dir` — see `crates/darkmux-types/src/env_audit.rs` for the exact
list and the reasoning for stopping there). Set `DARKMUX_ENV_AUDIT_LOG=<path>`
before running `cargo test -p darkmux-types --lib` and `cargo test -p
darkmux-crew --lib`; every `DARKMUX_*` read through a chokepoint appends one
`<test-thread-name>\\t<key>` line (`cargo test`'s harness names each test's
thread after its fully-qualified test path). This is exhaustive for
anything that actually runs through one of those four chokepoints, and
blind to anything that doesn't — a NEW direct `std::env::var("DARKMUX_...")`
call added outside them (production code, not a test's own save/restore
idiom) won't show up here until it's wired in too.

**The WRITER half (which keys are ever mutated) and the SERIAL-ANNOTATION
check are text scans — call them what they are: lints.** "Is `DARKMUX_HOME`
ever `set_var`'d in a test" and "does this test carry `#[serial]` or
`#[serial_test::serial]`" are both syntactic facts (a literal call, a literal
attribute) with no indirection to miss, so they're low-risk lints — but they
ARE lints, not a walk of anything that runs. A key mutated only through a
helper this script doesn't recognize (some indirection beyond the plain
`set_var("DARKMUX_X", ...)` / `remove_var("DARKMUX_X")` and the `let k =
"DARKMUX_X"; ...set_var(k, ...)` forms below) would be silently missed here.

## Usage

    DARKMUX_ENV_AUDIT_LOG=/tmp/env-audit.log cargo test -p darkmux-types --lib
    DARKMUX_ENV_AUDIT_LOG=/tmp/env-audit.log cargo test -p darkmux-crew --lib
    python3 scripts/env-audit-report.py /tmp/env-audit.log

Run it after adding a new test that touches `DARKMUX_*`-derived state, or
periodically as a sweep — the way it was used to find and fix #2632's 49
remaining instances (52 total, minus 3 fixed structurally by no longer
reading the env at all). Exits 1 if it finds an unguarded reader of a
mutated key; 0 otherwise (including when the audit log is empty — an empty
run proves nothing, so pair a `0` exit with checking the passed-in log
actually has content before trusting it).

NOT wired into CI as of #2632 — doing that well means setting
`DARKMUX_ENV_AUDIT_LOG` during CI's own `cargo test` runs (near-zero extra
cost, reusing the existing run) rather than a second dedicated run; left as
a named follow-up rather than done silently or done expensively.
"""

import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

SRC_DIRS = [
    REPO_ROOT / "crates/darkmux-types/src",
    REPO_ROOT / "crates/darkmux-crew/src",
    REPO_ROOT / "crates/darkmux-crew/tests",
]

MUTATOR_RE = re.compile(r'(?:set_var|remove_var)\(\s*"(DARKMUX_[A-Z0-9_]+)"')
INDIRECT_KEY_RE = re.compile(r'let\s+k\s*=\s*"(DARKMUX_[A-Z0-9_]+)"')
SERIAL_RE = re.compile(r"#\[\s*(serial_test::)?serial(\(|\])")


def find_mutated_keys() -> set[str]:
    """The WRITER half — a text scan (a lint; see the module doc above)."""
    keys: set[str] = set()
    for d in SRC_DIRS:
        if not d.exists():
            continue
        for path in d.rglob("*.rs"):
            text = path.read_text(errors="ignore")
            keys.update(MUTATOR_RE.findall(text))
            # `let k = "DARKMUX_X"; ...set_var(k, ...)` indirection: any file
            # that binds a DARKMUX_* literal to a local named `k` and also
            # calls `set_var`/`remove_var` at all is presumed to mutate that
            # key through the indirection — a coarse but safe-in-practice
            # over-approximation for a same-file-scoped idiom.
            if re.search(r"(?:set_var|remove_var)\(k[,)]", text):
                keys.update(INDIRECT_KEY_RE.findall(text))
    return keys


def is_serial(short_name: str) -> tuple[bool | None, str]:
    """(all_serial_or_None_if_no_match, location_string) — the SERIAL-
    ANNOTATION half, also a text scan (see module doc)."""
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
        return (None, "NO-SOURCE-MATCH")
    results = []
    for hit in hits:
        file_part, rest = hit.split(":", 1)
        lineno = int(rest.split(":", 1)[0])
        lines = Path(file_part).read_text().splitlines()
        window = lines[max(0, lineno - 8) : lineno]
        results.append((file_part, lineno, any(SERIAL_RE.search(ln) for ln in window)))
    return (all(r[2] for r in results), "; ".join(f"{f}:{ln}" for f, ln, _ in results))


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <audit-log-path>", file=sys.stderr)
        return 2
    log_path = Path(sys.argv[1])
    if not log_path.exists():
        print(f"no such audit log: {log_path}", file=sys.stderr)
        return 2

    mutated_keys = find_mutated_keys()
    pairs: set[tuple[str, str]] = set()
    for line in log_path.read_text().splitlines():
        if "\t" not in line:
            continue
        thread, key = line.split("\t", 1)
        if thread in ("<unnamed>", ""):
            continue
        if key in mutated_keys:
            pairs.add((thread, key))

    tests = sorted({t for t, _ in pairs})
    unguarded: list[tuple[str, list[str], str]] = []
    for t in tests:
        short = t.rsplit("::", 1)[-1]
        keys = sorted({k for tt, k in pairs if tt == t})
        ok, loc = is_serial(short)
        if ok is False:
            unguarded.append((t, keys, loc))

    print(f"# {len(tests)} distinct test threads read a mutated DARKMUX_* key")
    print(f"# {len(mutated_keys)} mutated keys found: {sorted(mutated_keys)}")
    print(f"\n## UNGUARDED ({len(unguarded)})\n")
    for t, keys, loc in unguarded:
        print(f"- {t}  keys={keys}\n    {loc}")

    return 1 if unguarded else 0


if __name__ == "__main__":
    sys.exit(main())
