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
# A function is keyed by name, never by position, so an edit above it does not
# move it and a new function cannot inherit another's baseline:
#   Rust  `<path>::<mods>::<owners>::<fn>`: the inline `mod` blocks around it,
#         then its impl (the header without `impl` and its generic
#         parameters: `Display for Token`, `From<String> for Token`) or trait,
#         and any fn it is nested in; each carries its `[cfg(...)]`, so cfg
#         twins differ.
#   TS    `<path>::<name>` from `ts-function-key.mjs`: its enclosing named
#         functions and classes, then its own name or declarator
#         (`const X = () => ...`); a callback is `<map callback>#<n>`, counted
#         within its nearest named enclosing function.
# Two functions that still share a key fail the run. Keys are never merged.
#
# Two files, each split by language:
#   scripts/complexity-baseline.json   the debt measured when the ratchet was
#       set: every function then above 15, at its complexity then. Only
#       `--prune` rewrites it, and prune only lowers an entry or drops it.
#   scripts/complexity-allowlist.json  deliberate exceptions, by hand:
#       {"<key>": {"max": <= 25, "reason": "..."}}.
#
# It fails only when a function in neither file is above 15, or one is above
# its baseline or allowlist max. A gain never fails: a baseline entry whose
# function fell, or is gone (split, renamed, deleted), passes with a notice
# naming `--prune`, which lowers or drops it so the gain is kept.
#
# Stdlib only. `--self-test` proves each rule can fail.

import json
import os
import re
import subprocess
import sys
import tempfile

from rust_source import cfgs_above, is_rust_test_file, mod_blocks, test_lines

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


def with_cfg(name, lines, idx):
    """`name[cfg(unix)]` for an item under a cfg attribute: cfg twins (one fn
    per target) are different functions and must not share a key."""
    cfgs = cfgs_above(lines, idx)
    return name + "".join(f"[cfg({c})]" for c in cfgs)


def impl_label(lines, idx):
    """An impl's header without `impl`, its generic parameters or its where
    clause: `Display for Token`, `From<Vec<u8>> for Token`, `Store<T>`. Two
    trait impls on one type then differ, as do two impls of one generic
    trait with different arguments."""
    head = " ".join(line.strip() for line in lines[idx:idx + 8]).split("{", 1)[0]
    head = re.sub(r"^.*?\bimpl\b", "", head).strip()
    if head.startswith("<"):
        depth = 0
        for i, c in enumerate(head):
            depth += {"<": 1, ">": -1}.get(c, 0)
            if depth == 0:
                head = head[i + 1:]
                break
    head = re.split(r"\bwhere\b", head)[0]
    return with_cfg(re.sub(r"\s+", " ", head).strip(), lines, idx)


def mod_path(mods, lines, idx):
    """The inline `mod` blocks around 0-based line `idx`, outermost first."""
    return [with_cfg(name, lines, first) for name, first, last in mods if first <= idx <= last]


def rust_functions(unit, rel, tests, text):
    """(key, complexity) for every named fn in one RCA unit, outside tests.
    The key is `path::mods::owners::fn`: the inline mods around it, then its
    impl or trait and any fn it is nested in, each with its cfg."""
    out = []
    lines = text.split("\n")
    mods = mod_blocks(text)

    def walk(space, owners):
        for s in space["spaces"]:
            idx = s["start_line"] - 1
            if s["kind"] == "function" and s["name"] != "<anonymous>":
                name = with_cfg(s["name"], lines, idx)
                if idx not in tests:
                    cc = s["metrics"]["cyclomatic"]["sum"] - named_fns_sum(s) - closures(s)
                    out.append(("::".join([rel, *mod_path(mods, lines, idx), *owners, name]), int(cc)))
                walk(s, [*owners, name])
            elif s["kind"] == "impl":
                walk(s, [*owners, impl_label(lines, idx)])
            elif s["kind"] == "trait" and s["name"]:
                walk(s, [*owners, with_cfg(s["name"], lines, idx)])
            else:
                walk(s, owners)

    walk(unit, [])
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
                text = open(os.path.join(ROOT, rel), encoding="utf-8").read()
                found += rust_functions(unit, rel, test_lines(text), text)
    if len(files) < 100 or len(found) < 1000:
        sys.exit(f"complexity-ratchet: the Rust measurement looks broken ({len(files)} files, {len(found)} functions)")
    return unique(found)


def is_ts_source(rel):
    return not (re.search(r"\.test\.tsx?$", rel) or "/testing/" in rel or "/types/generated/" in rel)


def ts_reports(results):
    """(path, line, column, complexity) for every function ESLint's
    `complexity` rule reported, outside tests."""
    found = []
    for result in results:
        rel = os.path.relpath(result["filePath"], ROOT)
        if not is_ts_source(rel):
            continue
        for m in result["messages"]:
            hit = TS_MESSAGE.match(m["message"]) if m.get("ruleId") == "complexity" else None
            if hit is not None:
                found.append((rel, m["line"], m["column"], int(hit.group("cc"))))
    return found


def name_ts_functions(reports):
    """The stable name of each reported function (`ts-function-key.mjs`):
    its declarator or its enclosing function, never its position."""
    wanted = [{"file": os.path.join(ROOT, rel), "line": line, "column": col} for rel, line, col, _ in reports]
    proc = subprocess.run(["node", os.path.join(ROOT, "scripts", "ts-function-key.mjs")],
                          input=json.dumps(wanted), capture_output=True, text=True)
    if proc.returncode != 0:
        sys.exit(f"complexity-ratchet: naming the TypeScript functions failed:\n{proc.stderr[-2000:]}")
    return json.loads(proc.stdout)


def ts_functions(results, namer=name_ts_functions):
    """(key, complexity) for every function ESLint's `complexity` rule named."""
    reports = ts_reports(results)
    names = namer(reports)
    return [(f"{rel}::{name}", cc) for (rel, _l, _c, cc), name in zip(reports, names)]


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
    return unique(ts_functions(results))


def collisions(pairs):
    """The keys two measured functions share."""
    seen, twice = set(), set()
    for key, _cc in pairs:
        (twice if key in seen else seen).add(key)
    return sorted(twice)


def unique(pairs):
    """`pairs` as a dict, or a failure naming every shared key. A shared key
    is never merged: taking the larger would let the smaller twin grow into
    the larger one's baseline unseen."""
    shared = collisions(pairs)
    if shared:
        lines = "\n".join(f"  {k}" for k in shared)
        sys.exit(f"complexity-ratchet: {len(shared)} key(s) name more than one function; "
                 f"the key scheme needs to tell them apart (see the header):\n{lines}")
    return dict(pairs)


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
    return problems


def gains(measured, baseline):
    """Every baseline entry its function has beaten (fell, or is gone), as
    lines to print. A notice, never a failure: a PR that simplifies or splits
    a function must not go red for it."""
    notes = []
    for key, was in sorted(baseline.items()):
        now = measured.get(key, 0)
        if now < was:
            state = "is gone or at most the limit" if now <= LIMIT else f"fell to {now}"
            notes.append(f"{key}: baseline {was}, the function {state}")
    return notes


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

RUST_SAMPLE = """\
impl<T> Foo<T> {
    fn run(&self) {
        let f = || 1;
        fn inner() {}
    }
}
impl fmt::Display for Token {
    fn fmt(&self) {}
}
impl fmt::Debug for Token {
    fn fmt(&self) {}
}
impl From<Vec<u8>> for Token {
    fn from(v: Vec<u8>) -> Self {}
}
impl From<String> for Token {
    fn from(v: String) -> Self {}
}
#[cfg(unix)]
fn alive() {}
/// The fallback.
#[cfg(not(unix))]
fn alive() {}
mod a {
    fn helper() {}
}
mod b {
    fn helper() {}
}
fn outer_one() {
    fn visit() {}
}
fn outer_two() {
    fn visit() {}
}
fn in_tests() {}
"""


def fn_space(name, line, cc=1.0, spaces=()):
    return {"kind": "function", "name": name, "start_line": line, "metrics": {"cyclomatic": {"sum": cc}}, "spaces": list(spaces)}


def rust_self_test():
    """Every kind of twin the key scheme must tell apart, on a hand-built RCA
    unit whose lines match RUST_SAMPLE."""
    impl = lambda name, line, *fns: {"kind": "impl", "name": name, "start_line": line, "spaces": list(fns)}
    unit = {"kind": "unit", "name": "x.rs", "spaces": [
        impl("Foo", 1, fn_space("run", 2, 9.0, [fn_space("<anonymous>", 3, 3.0), fn_space("inner", 4, 4.0)])),
        impl("Token", 7, fn_space("fmt", 8)),
        impl("Token", 10, fn_space("fmt", 11)),
        impl("Token", 13, fn_space("from", 14)),
        impl("Token", 16, fn_space("from", 17)),
        fn_space("alive", 20), fn_space("alive", 23),
        fn_space("helper", 25), fn_space("helper", 28),
        fn_space("outer_one", 30, 2.0, [fn_space("visit", 31)]),
        fn_space("outer_two", 33, 2.0, [fn_space("visit", 34)]),
        fn_space("in_tests", 36, 30.0),
    ]}
    got = rust_functions(unit, "x.rs", {35}, RUST_SAMPLE)
    assert not collisions(got), collisions(got)
    keys = dict(got)
    assert keys["x.rs::Foo<T>::run"] == 4 and keys["x.rs::Foo<T>::run::inner"] == 4, keys
    for k in ["x.rs::fmt::Display for Token::fmt", "x.rs::fmt::Debug for Token::fmt",
              "x.rs::From<Vec<u8>> for Token::from", "x.rs::From<String> for Token::from",
              "x.rs::alive[cfg(unix)]", "x.rs::alive[cfg(not(unix))]", "x.rs::a::helper", "x.rs::b::helper",
              "x.rs::outer_one::visit", "x.rs::outer_two::visit"]:
        assert k in keys, (k, sorted(keys))
    assert not any("in_tests" in k for k in keys), "test code is not measured"


def self_test():
    assert not judge({"a::f": 3, "a::g": 20}, {"a::g": 20}, {}), "a baseline function at its baseline passes"
    assert judge({"a::f": 16}, {}, {}), "a new function above the limit fails"
    assert not judge({"a::f": 15}, {}, {}), "a new function at the limit passes"
    assert judge({"a::g": 21}, {"a::g": 20}, {}), "a rise above the baseline fails"
    assert not judge({"a::g": 19}, {"a::g": 20}, {}), "a fall passes"
    assert not judge({}, {"a::g": 20}, {}), "a gone function passes"
    assert gains({"a::g": 19}, {"a::g": 20}) and gains({}, {"a::g": 20}), "a fall or a gone function is noted"
    assert not gains({"a::g": 20}, {"a::g": 20}), "no gain, no note"
    assert not judge({"a::h": 22}, {}, {"a::h": {"max": 22, "reason": "a dispatch table"}}), "an allowlisted max passes"
    assert judge({"a::h": 23}, {}, {"a::h": {"max": 22, "reason": "x"}}), "above an allowlisted max fails"
    assert judge({}, {}, {"a::h": {"max": 30, "reason": "x"}}), "an allowlist max above the ceiling fails"
    assert judge({}, {}, {"a::h": {"max": 20}}), "an allowlist entry without a reason fails"
    assert pruned({"a::g": 18, "a::k": 12}, {"a::g": 20, "a::k": 30, "a::x": 40}) == {"a::g": 18}
    assert pruned({"a::g": 25}, {"a::g": 20}) == {"a::g": 20}, "prune never raises"

    rust_self_test()

    results = [
        {"filePath": os.path.join(ROOT, "ui/src/lib/a.ts"), "messages": [
            {"ruleId": "complexity", "line": 3, "column": 1, "message": "Function 'parse' has a complexity of 18. Maximum allowed is 15."},
            {"ruleId": "complexity", "line": 9, "column": 5, "message": "Arrow function has a complexity of 16. Maximum allowed is 15."},
            {"ruleId": "no-explicit-any", "line": 1, "column": 1, "message": "Unexpected any."},
        ]},
        {"filePath": os.path.join(ROOT, "ui/src/lib/a.test.ts"), "messages": [
            {"ruleId": "complexity", "line": 1, "column": 1, "message": "Function 'big' has a complexity of 40. Maximum allowed is 15."},
        ]},
    ]
    assert ts_reports(results) == [("ui/src/lib/a.ts", 3, 1, 18), ("ui/src/lib/a.ts", 9, 5, 16)], ts_reports(results)
    named = ts_functions(results, namer=lambda reports: ["parse", "List.<map callback>#1"])
    assert named == [("ui/src/lib/a.ts::parse", 18), ("ui/src/lib/a.ts::List.<map callback>#1", 16)], named
    assert collisions([("k", 3), ("j", 1), ("k", 20)]) == ["k"], "a shared key is found, never merged"
    try:
        unique([("k", 3), ("k", 20)])
        raise AssertionError("a shared key must fail")
    except SystemExit as e:
        assert "k" in str(e)
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
    notes = gains(measured, baseline)
    if notes:
        print(f"complexity-ratchet ({lang}): {len(notes)} baseline entr{'y' if len(notes) == 1 else 'ies'} beaten; "
              f"keep the gain with `python3 scripts/complexity-ratchet.py --lang {lang} --prune` and commit the baseline:")
        for n in notes:
            print(f"  {n}")
    problems = judge(measured, baseline, allowlist)
    if problems:
        print(f"complexity-ratchet ({lang}): {len(problems)} problem(s). Split a function by responsibility rather than raise a limit.")
        for p in problems:
            print(f"  {p}")
        sys.exit(1)
    print(f"complexity-ratchet ({lang}) passed: {len(measured)} functions measured, {len(baseline)} in the baseline")


if __name__ == "__main__":
    main()
