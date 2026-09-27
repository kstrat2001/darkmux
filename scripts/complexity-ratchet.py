#!/usr/bin/env python3
# complexity-ratchet.py (4.0): no new function above cyclomatic complexity
# 15, and no function's complexity rises.
#
# Measured per language (`--lang`):
#   rust  rust-code-analysis-cli (RCA_BIN, default `rust-code-analysis-cli`)
#         over the tracked .rs files under src/, crates/, runtime/, plugins/,
#         test code excluded (`rust_source`). A function's complexity is RCA's
#         cyclomatic for it, counting the decisions inside its closures but
#         not a closure's own entry, and not a nested named fn (that is its
#         own function). The same reading as the CRAP sweep.
#   ts    ESLint's core `complexity` rule over ui/src (tests, test helpers
#         and the generated bindings excluded), run from ui/ at 15, so only
#         the functions above it are reported.
#
# A function is keyed `<path>::<Owner>::<fn>` (Rust; Owner is the impl or
# trait) or `<path>::<name>` (TypeScript; an unnamed one `<anonymous>#<n>`,
# by order in its file). Line numbers are not in the key, so an edit above a
# function does not move it.
#
# Two files, each split by language:
#   scripts/complexity-baseline.json   the debt measured when the ratchet was
#       set: every function then above 15, at its complexity then. Only
#       `--prune` rewrites it, and prune only lowers an entry or drops it.
#   scripts/complexity-allowlist.json  deliberate exceptions, by hand:
#       {"<key>": {"max": <= 25, "reason": "..."}}.
#
# It fails when a function in neither file is above 15, when one is above its
# baseline or allowlist max, and when a baseline entry is stale (its function
# fell, or is gone): `--prune` lowers it, which is how a gain is kept.
#
# Stdlib only. `--self-test` proves each rule can fail.

import json
import os
import re
import subprocess
import sys
import tempfile

from rust_source import is_rust_test_file, test_lines

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE = os.path.join(ROOT, "scripts", "complexity-baseline.json")
ALLOWLIST = os.path.join(ROOT, "scripts", "complexity-allowlist.json")
LIMIT = 15
CEILING = 25
RUST_ROOTS = ("src/", "crates/", "runtime/", "plugins/")
TS_MESSAGE = re.compile(r"^(?:.*?'(?P<name>[^']+)'|[^']*?) has a complexity of (?P<cc>\d+)")


# --- measuring ----------------------------------------------------------------

def tracked(pattern):
    out = subprocess.run(["git", "ls-files", "-z", pattern], cwd=ROOT, capture_output=True, check=True)
    return [p for p in out.stdout.decode().split("\0") if p]


def rust_sources():
    return [p for p in tracked("*.rs") if p.startswith(RUST_ROOTS) and not is_rust_test_file(p)]


def closures(space):
    """How many closures sit under `space`, not looking inside a named fn."""
    n = 0
    for s in space["spaces"]:
        if s["kind"] == "function" and s["name"] == "<anonymous>":
            n += 1 + closures(s)
        elif s["kind"] != "function":
            n += closures(s)
    return n


def named_fns_sum(space):
    """The complexity of the named fns nested under `space`, which count as
    their own functions."""
    total = 0
    for s in space["spaces"]:
        if s["kind"] == "function" and s["name"] != "<anonymous>":
            total += s["metrics"]["cyclomatic"]["sum"]
        else:
            total += named_fns_sum(s)
    return total


def rust_functions(unit, rel, tests):
    """(key, complexity) for every named fn in one RCA unit, outside tests."""
    out = []

    def walk(space, owner):
        for s in space["spaces"]:
            if s["kind"] == "function" and s["name"] != "<anonymous>":
                if s["start_line"] - 1 not in tests:
                    cc = s["metrics"]["cyclomatic"]["sum"] - named_fns_sum(s) - closures(s)
                    out.append((f"{rel}::{owner + '::' if owner else ''}{s['name']}", int(cc)))
                walk(s, owner)
            elif s["kind"] in ("impl", "trait") and s["name"]:
                walk(s, s["name"].split("<")[0].strip())
            else:
                walk(s, owner)

    walk(unit, "")
    return out


def measure_rust():
    rca = os.environ.get("RCA_BIN", "rust-code-analysis-cli")
    files = rust_sources()
    found = []
    with tempfile.TemporaryDirectory() as out:
        args = [rca, "-m", "-O", "json", "-j", "4", "-o", out]
        for f in files:
            args += ["-p", f]
        subprocess.run(args, cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
        for dirpath, _dirs, names in os.walk(out):
            for name in names:
                unit = json.load(open(os.path.join(dirpath, name)))
                rel = os.path.relpath(os.path.join(ROOT, unit["name"]), ROOT)
                tests = test_lines(open(os.path.join(ROOT, rel), encoding="utf-8").read())
                found += rust_functions(unit, rel, tests)
    if len(files) < 100 or len(found) < 1000:
        sys.exit(f"complexity-ratchet: the Rust measurement looks broken ({len(files)} files, {len(found)} functions)")
    return keep_max(found)


def is_ts_source(rel):
    return not (re.search(r"\.test\.tsx?$", rel) or "/testing/" in rel or "/types/generated/" in rel)


def ts_functions(results):
    """(key, complexity) for every function ESLint's `complexity` rule named."""
    found = []
    for result in results:
        rel = os.path.relpath(result["filePath"], ROOT)
        if not is_ts_source(rel):
            continue
        anonymous = 0
        for m in result["messages"]:
            hit = TS_MESSAGE.match(m["message"]) if m.get("ruleId") == "complexity" else None
            if hit is None:
                continue
            name = hit.group("name")
            if name is None:
                anonymous += 1
                name = f"<anonymous>#{anonymous}"
            found.append((f"{rel}::{name}", int(hit.group("cc"))))
    return found


def measure_ts():
    ui = os.path.join(ROOT, "ui")
    cmd = ["bunx", "eslint", "--no-inline-config", "--rule", json.dumps({"complexity": ["error", LIMIT]}), "-f", "json", "src"]
    proc = subprocess.run(cmd, cwd=ui, capture_output=True, text=True)
    try:
        results = json.loads(proc.stdout)
    except json.JSONDecodeError:
        sys.exit(f"complexity-ratchet: ESLint did not report JSON (exit {proc.returncode}):\n{proc.stderr[-2000:]}")
    if len(results) < 100:
        sys.exit(f"complexity-ratchet: the TypeScript measurement looks broken ({len(results)} files)")
    return keep_max(ts_functions(results))


def keep_max(pairs):
    """Two functions can share a key (two impls of one trait in a file); the
    key then carries the larger complexity."""
    out = {}
    for key, cc in pairs:
        out[key] = max(cc, out.get(key, 0))
    return out


# --- judging ------------------------------------------------------------------

def judge(measured, baseline, allowlist):
    """Every way `measured` breaks the ratchet, as lines to print."""
    problems = []
    for key, spec in sorted(allowlist.items()):
        if spec.get("max", 0) > CEILING or not spec.get("reason"):
            problems.append(f"{key}: an allowlist entry needs a reason and a max of at most {CEILING}")
    for key, cc in sorted(measured.items()):
        if key in allowlist:
            limit, why = allowlist[key]["max"], "its allowlisted max"
        elif key in baseline:
            limit, why = baseline[key], "its baseline"
        else:
            limit, why = LIMIT, "the limit for a function not in the baseline"
        if cc > limit:
            problems.append(f"{key}: complexity {cc}, above {why} ({limit})")
    for key, was in sorted(baseline.items()):
        now = measured.get(key, 0)
        if now < was:
            state = "is gone or at most the limit" if now <= LIMIT else f"fell to {now}"
            problems.append(f"{key}: baseline {was} is stale, the function {state} (run --prune to keep the gain)")
    return problems


def pruned(measured, baseline):
    """The baseline with every entry lowered to what was measured, and the
    entries at or under the limit dropped. Never adds, never raises."""
    return {k: min(v, measured[k]) for k, v in baseline.items() if measured.get(k, 0) > LIMIT}


# --- files --------------------------------------------------------------------

def load(path, lang):
    try:
        return json.load(open(path)).get(lang, {})
    except FileNotFoundError:
        return {}


def store(path, lang, section):
    try:
        data = json.load(open(path))
    except FileNotFoundError:
        data = {}
    data[lang] = dict(sorted(section.items()))
    with open(path, "w") as f:
        json.dump(data, f, indent=1, sort_keys=True)
        f.write("\n")


# --- self-test ----------------------------------------------------------------

def self_test():
    assert not judge({"a::f": 3, "a::g": 20}, {"a::g": 20}, {}), "a baseline function at its baseline passes"
    assert judge({"a::f": 16}, {}, {}), "a new function above the limit fails"
    assert not judge({"a::f": 15}, {}, {}), "a new function at the limit passes"
    assert judge({"a::g": 21}, {"a::g": 20}, {}), "a rise above the baseline fails"
    assert judge({"a::g": 19}, {"a::g": 20}, {}), "a fall leaves a stale baseline"
    assert judge({}, {"a::g": 20}, {}), "a gone function leaves a stale baseline"
    assert not judge({"a::h": 22}, {}, {"a::h": {"max": 22, "reason": "a dispatch table"}}), "an allowlisted max passes"
    assert judge({"a::h": 23}, {}, {"a::h": {"max": 22, "reason": "x"}}), "above an allowlisted max fails"
    assert judge({}, {}, {"a::h": {"max": 30, "reason": "x"}}), "an allowlist max above the ceiling fails"
    assert judge({}, {}, {"a::h": {"max": 20}}), "an allowlist entry without a reason fails"
    assert pruned({"a::g": 18, "a::k": 12}, {"a::g": 20, "a::k": 30, "a::x": 40}) == {"a::g": 18}
    assert pruned({"a::g": 25}, {"a::g": 20}) == {"a::g": 20}, "prune never raises"

    unit = {"kind": "unit", "name": "x.rs", "spaces": [
        {"kind": "impl", "name": "Foo<T>", "start_line": 1, "spaces": [
            {"kind": "function", "name": "run", "start_line": 2, "metrics": {"cyclomatic": {"sum": 9.0}}, "spaces": [
                {"kind": "function", "name": "<anonymous>", "start_line": 3, "metrics": {"cyclomatic": {"sum": 3.0}}, "spaces": []},
                {"kind": "function", "name": "inner", "start_line": 4, "metrics": {"cyclomatic": {"sum": 4.0}}, "spaces": []},
            ]},
        ]},
        {"kind": "function", "name": "in_tests", "start_line": 9, "metrics": {"cyclomatic": {"sum": 30.0}}, "spaces": []},
    ]}
    got = dict(rust_functions(unit, "x.rs", tests={8}))
    assert got == {"x.rs::Foo::run": 4, "x.rs::Foo::inner": 4}, got

    results = [
        {"filePath": os.path.join(ROOT, "ui/src/lib/a.ts"), "messages": [
            {"ruleId": "complexity", "message": "Function 'parse' has a complexity of 18. Maximum allowed is 15."},
            {"ruleId": "complexity", "message": "Arrow function has a complexity of 16. Maximum allowed is 15."},
            {"ruleId": "complexity", "message": "Method 'ingest' has a complexity of 27. Maximum allowed is 15."},
            {"ruleId": "no-explicit-any", "message": "Unexpected any."},
        ]},
        {"filePath": os.path.join(ROOT, "ui/src/lib/a.test.ts"), "messages": [
            {"ruleId": "complexity", "message": "Function 'big' has a complexity of 40. Maximum allowed is 15."},
        ]},
    ]
    assert dict(ts_functions(results)) == {
        "ui/src/lib/a.ts::parse": 18, "ui/src/lib/a.ts::<anonymous>#1": 16, "ui/src/lib/a.ts::ingest": 27,
    }, ts_functions(results)
    print("complexity-ratchet self-test passed")


def main():
    args = sys.argv[1:]
    if "--self-test" in args:
        self_test()
        return
    lang = args[args.index("--lang") + 1] if "--lang" in args else sys.exit("complexity-ratchet: --lang rust|ts")
    measured = {"rust": measure_rust, "ts": measure_ts}[lang]()
    baseline, allowlist = load(BASELINE, lang), load(ALLOWLIST, lang)
    if "--init" in args:
        if baseline:
            sys.exit("complexity-ratchet: a baseline exists; --init only sets the first one")
        store(BASELINE, lang, {k: v for k, v in measured.items() if v > LIMIT and k not in allowlist})
        return
    if "--prune" in args:
        store(BASELINE, lang, pruned(measured, baseline))
        baseline = load(BASELINE, lang)
    problems = judge(measured, baseline, allowlist)
    if problems:
        print(f"complexity-ratchet ({lang}): {len(problems)} problem(s). Split a function by responsibility rather than raise a limit.")
        for p in problems:
            print(f"  {p}")
        sys.exit(1)
    print(f"complexity-ratchet ({lang}) passed: {len(measured)} functions measured, {len(baseline)} in the baseline")


if __name__ == "__main__":
    main()
