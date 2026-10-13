#!/usr/bin/env python3
# complexity-ratchet.py (4.0): no new function above cyclomatic complexity
# 10, and no function's complexity rises. (The limit was 15 until 5.x,
# #3136; every function then at 11-15 was added to the baseline at its
# complexity, so only new code meets the lower limit.)
#
# Measured per language (`--lang`):
#   rust  rust-code-analysis-cli (RCA_BIN, default `rust-code-analysis-cli`)
#         over the tracked .rs files under src/, crates/, runtime/, plugins/,
#         test code excluded (`rust_source`). A function's complexity is RCA's
#         cyclomatic for it, counting the decisions inside its closures but
#         not a closure's own entry, and not a nested named fn (that is its
#         own function). The same reading as the CRAP sweep.
#   ts    ESLint's core `complexity` rule over ui/src (tests, test helpers
#         and the generated bindings excluded), run from ui/ at the limit, so only
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
#       set: every function then above the limit, at its complexity then. Only
#       `--prune` rewrites it, and prune only lowers an entry or drops it.
#   scripts/complexity-allowlist.json  deliberate exceptions, by hand:
#       {"<key>": {"max": <= 25, "reason": "..."}}.
#
# It fails only when a function in neither file is above the limit, or one is above
# its baseline or allowlist max. A gain never fails: a baseline entry whose
# function fell, or is gone (split, renamed, deleted), passes with a notice
# naming `--prune`, which lowers or drops it so the gain is kept.
#
# CRAP (`--crap <crap.json>`, Rust only, #3198): the same ratchet on
#   CRAP = complexity^2 * (1 - line coverage)^3 + complexity,
# read from the `crap-report.py --json` export of CI's coverage run (the
# `crap-report` artifact of quality.yml's coverage job), never measured here.
# No new function above CRAP 10, and no function's CRAP rises above its entry
# in scripts/crap-baseline.json; `--prune` and `--init` work as above. A
# complexity-allowlist max applies to CRAP too (that function, fully tested).
# A function with no coverage data (not built on the measuring platform) is
# held at its baseline: never judged, never a gain, kept by prune. Keys carry
# the file path, so a function moved or renamed in its path would read as new:
# a key not in the baseline takes the one vanished entry with its function
# name (then also its complexity, recorded in the baseline's `rust_cc`) and is
# judged against that CRAP; still ambiguous, it is new. The output names both.
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
LIMIT = 10
CRAP_BASELINE = os.path.join(ROOT, "scripts", "crap-baseline.json")
CRAP_LIMIT = 10
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

def judge(measured, baseline, allowlist, limit=LIMIT, metric="complexity"):
    """Every way `measured` breaks the ratchet, as lines to print."""
    problems = []
    for key, spec in sorted(allowlist.items()):
        if spec.get("max", 0) > CEILING or not spec.get("reason"):
            problems.append(f"{key}: an allowlist entry needs a reason and a max of at most {CEILING}")
    for key, cc in sorted(measured.items()):
        if key in allowlist:
            cap, why = allowlist[key]["max"], "its allowlisted max"
        elif key in baseline:
            cap, why = baseline[key], "its baseline"
        else:
            cap, why = limit, "the limit for a function not in the baseline"
        if cc > cap:
            problems.append(f"{key}: {metric} {cc}, above {why} ({cap})")
    return problems


def gains(measured, baseline, limit=LIMIT):
    """Every baseline entry its function has beaten (fell, or is gone), as
    lines to print. A notice, never a failure: a PR that simplifies or splits
    a function must not go red for it."""
    notes = []
    for key, was in sorted(baseline.items()):
        now = measured.get(key, 0)
        if now < was:
            state = "is gone or at most the limit" if now <= limit else f"fell to {now}"
            notes.append(f"{key}: baseline {was}, the function {state}")
    return notes


def pruned(measured, baseline, limit=LIMIT):
    """The baseline with every entry lowered to what was measured, and the
    entries at or under the limit dropped. Never adds, never raises."""
    return {k: min(v, measured[k]) for k, v in baseline.items() if measured.get(k, 0) > limit}


def fn_name(key):
    """The function's own name: the last segment of its key."""
    return key.rsplit("::", 1)[-1]


def moves(measured, baseline, cc_now, cc_base):
    """new key -> vanished baseline key, for every function that moved or was
    renamed in its path. A measured key not in the baseline takes the one
    vanished entry with its function name; when several share it, the one with
    its complexity too. Anything still ambiguous (two candidates, or two new
    functions after one entry) is no move: the function is judged as new."""
    vanished = [k for k in baseline if k not in measured]
    claims = {}
    for new in (k for k in measured if k not in baseline):
        same = [v for v in vanished if fn_name(v) == fn_name(new)]
        if len(same) > 1:
            same = [v for v in same if cc_base.get(v) is not None and cc_base.get(v) == cc_now.get(new)]
        if len(same) == 1:
            claims.setdefault(same[0], []).append(new)
    return {news[0]: old for old, news in claims.items() if len(news) == 1}


def with_moves(baseline, found):
    """The baseline with each moved entry under its new key."""
    eff = {k: v for k, v in baseline.items() if k not in found.values()}
    eff.update({new: baseline[old] for new, old in found.items()})
    return eff


def move_notes(found, baseline):
    return [f"{old} -> {new}: moved, judged against its recorded CRAP ({baseline[old]})" for new, old in sorted(found.items())]


def crap_scores(rows, baseline):
    """key -> CRAP from `crap-report.py --json` rows. A row with no coverage
    data scores its baseline entry (held: not judged, no gain, kept by prune),
    or is left out when it has none."""
    scored = unique([(r["key"], r["crap"]) for r in rows if r["crap"] is not None])
    held = {r["key"]: baseline[r["key"]] for r in rows if r["crap"] is None and r["key"] in baseline}
    return {**held, **scored}


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


def moved_self_test():
    """A function moved or renamed in its path keeps its recorded CRAP: a
    measured key not in the baseline takes the one vanished entry with its
    function name (then its complexity too); still ambiguous, it is new."""
    lim = dict(limit=CRAP_LIMIT, metric="CRAP")
    base, base_cc = {"a.rs::Store::f": 40.0}, {"a.rs::Store::f": 8}
    found = moves({"b.rs::f": 40.0}, base, {"b.rs::f": 8}, base_cc)
    assert found == {"b.rs::f": "a.rs::Store::f"}, found
    eff = with_moves(base, found)
    assert not judge({"b.rs::f": 40.0}, eff, {}, **lim), "a function moved with its CRAP unchanged passes"
    assert judge({"b.rs::f": 40.1}, with_moves(base, moves({"b.rs::f": 40.1}, base, {"b.rs::f": 8}, base_cc)), {}, **lim), \
        "a function moved that also grew fails"
    assert not gains({"b.rs::f": 40.0}, eff, limit=CRAP_LIMIT), "a move is not a gain"
    assert pruned({"b.rs::f": 30.0}, eff, CRAP_LIMIT) == {"b.rs::f": 30.0}, "prune keeps a move under its new key"
    assert moves({"b.rs::f": 40.0}, base, {"b.rs::f": 8}, base_cc) and not moves({"a.rs::Store::f": 40.0}, base, {}, base_cc), \
        "a function still at its key has not moved"
    assert not moves({"b.rs::g": 40.0}, base, {"b.rs::g": 8}, base_cc), "another name is not a move"
    assert not moves({"a.rs::f": 40.0}, {"a.rs::f": 40.0, "c.rs::f": 20.0}, {"a.rs::f": 8}, {}), \
        "a function still in the baseline never claims a deleted same-named entry"
    two = {"a.rs::g": 30.0, "c.rs::g": 50.0}
    two_cc = {"a.rs::g": 6, "c.rs::g": 9}
    assert moves({"b.rs::g": 30.0, "d.rs::g": 50.0}, two, {"b.rs::g": 6, "d.rs::g": 9}, two_cc) == \
        {"b.rs::g": "a.rs::g", "d.rs::g": "c.rs::g"}, "two same-named functions resolve by complexity"
    assert moves({"b.rs::g": 30.0, "d.rs::g": 50.0}, two, {"b.rs::g": 6, "d.rs::g": 6}, {"a.rs::g": 6, "c.rs::g": 6}) == {}, \
        "two same-named functions at one complexity are ambiguous: both are new"
    assert moves({"b.rs::g": 30.0, "d.rs::g": 50.0}, {"a.rs::g": 30.0}, {"b.rs::g": 6, "d.rs::g": 6}, {"a.rs::g": 6}) == {}, \
        "one vanished entry claimed by two new functions is ambiguous: both are new"
    note = move_notes({"b.rs::f": "a.rs::Store::f"}, base)
    assert note == ["a.rs::Store::f -> b.rs::f: moved, judged against its recorded CRAP (40.0)"], note


def crap_self_test():
    """The CRAP ratchet: the same rules as complexity, judged at CRAP_LIMIT on
    the scores `crap-report.py --json` wrote."""
    row = lambda key, crap: {"key": key, "cc": 1, "cov": None if crap is None else 1.0, "crap": crap}
    base = {"a::slow": 40.0, "a::dark": 30.0}
    rows = [row("a::slow", 40.0), row("a::new", 4.0), row("a::dark", None), row("a::unbuilt", None)]
    scores = crap_scores(rows, base)
    assert scores == {"a::slow": 40.0, "a::new": 4.0, "a::dark": 30.0}, \
        "a function with no coverage data is held at its baseline, and is not judged when it has none"
    lim = dict(limit=CRAP_LIMIT, metric="CRAP")
    assert not judge(scores, base, {}, **lim), "every function at its baseline or under the limit passes"
    assert judge({"a::slow": 40.1}, base, {}, **lim), "a rise above the recorded CRAP fails"
    assert judge({"a::new": CRAP_LIMIT + 0.1}, base, {}, **lim), "a new function above the CRAP limit fails"
    assert not judge({"a::new": CRAP_LIMIT}, base, {}, **lim), "a new function at the CRAP limit passes"
    assert "CRAP 40.1, above its baseline (40.0)" in judge({"a::slow": 40.1}, base, {}, **lim)[0]
    assert not judge({"a::h": 22.0}, {}, {"a::h": {"max": 22, "reason": "x"}}, **lim), \
        "a complexity-allowlisted function may reach its max in CRAP too (fully tested at that complexity)"
    assert judge({"a::h": 22.5}, {}, {"a::h": {"max": 22, "reason": "x"}}, **lim)
    assert gains({"a::slow": 12.0}, base, limit=CRAP_LIMIT) and not gains(scores, base, limit=CRAP_LIMIT), \
        "a fall is noted; a function with no data is no gain"
    assert pruned({"a::slow": 12.0, "a::dark": 30.0}, base, limit=CRAP_LIMIT) == {"a::slow": 12.0, "a::dark": 30.0}
    assert pruned({"a::slow": 9.5, "a::dark": 30.0}, base, limit=CRAP_LIMIT) == {"a::dark": 30.0}, \
        "a function tested down to the limit leaves the baseline"
    assert pruned(crap_scores(rows, base), base, limit=CRAP_LIMIT) == base, "prune keeps a no-data entry"
    moved_self_test()
    try:
        crap_scores([row("k", 3.0), row("k", 20.0)], {})
        raise AssertionError("a shared key must fail")
    except SystemExit as e:
        assert "k" in str(e)


def self_test():
    assert not judge({"a::f": 3, "a::g": 20}, {"a::g": 20}, {}), "a baseline function at its baseline passes"
    assert judge({"a::f": LIMIT + 1}, {}, {}), "a new function above the limit fails"
    assert not judge({"a::f": LIMIT}, {}, {}), "a new function at the limit passes"
    assert judge({"a::g": 21}, {"a::g": 20}, {}), "a rise above the baseline fails"
    assert not judge({"a::g": 19}, {"a::g": 20}, {}), "a fall passes"
    assert not judge({}, {"a::g": 20}, {}), "a gone function passes"
    assert gains({"a::g": 19}, {"a::g": 20}) and gains({}, {"a::g": 20}), "a fall or a gone function is noted"
    assert not gains({"a::g": 20}, {"a::g": 20}), "no gain, no note"
    assert not judge({"a::h": 22}, {}, {"a::h": {"max": 22, "reason": "a dispatch table"}}), "an allowlisted max passes"
    assert judge({"a::h": 23}, {}, {"a::h": {"max": 22, "reason": "x"}}), "above an allowlisted max fails"
    assert judge({}, {}, {"a::h": {"max": 30, "reason": "x"}}), "an allowlist max above the ceiling fails"
    assert judge({}, {}, {"a::h": {"max": 20}}), "an allowlist entry without a reason fails"
    assert pruned({"a::g": 18, "a::k": LIMIT}, {"a::g": 20, "a::k": 30, "a::x": 40}) == {"a::g": 18}
    assert pruned({"a::g": 25}, {"a::g": 20}) == {"a::g": 20}, "prune never raises"

    rust_self_test()
    crap_self_test()

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


def crap_store(baseline, cc):
    """The baseline, and beside it each entry's complexity (`rust_cc`), which
    tells two same-named functions apart when both move at once."""
    store(CRAP_BASELINE, "rust", baseline)
    store(CRAP_BASELINE, "rust_cc", {k: cc[k] for k in baseline if k in cc})


def crap_main(args):
    path = args[args.index("--crap") + 1]
    if not os.path.isfile(path):
        sys.exit(f"crap-ratchet: no CRAP export at {path}; the CRAP report step did not write one, so nothing was judged")
    rows = json.load(open(path))
    if len(rows) < 1000 or not any(r["crap"] is not None for r in rows):
        sys.exit(f"crap-ratchet: the CRAP export looks broken ({len(rows)} functions); "
                 "is it `crap-report.py --json` from this checkout's coverage run?")
    baseline, base_cc = load(CRAP_BASELINE, "rust"), load(CRAP_BASELINE, "rust_cc")
    measured = crap_scores(rows, baseline)
    cc_now = {r["key"]: r["cc"] for r in rows}
    if "--init" in args:
        if baseline:
            sys.exit("crap-ratchet: a baseline exists; --init only sets the first one")
        crap_store({k: v for k, v in measured.items() if v > CRAP_LIMIT}, cc_now)
        return
    found = moves(measured, baseline, cc_now, base_cc)
    if found:
        print(f"crap-ratchet: {len(found)} function(s) moved or renamed; each keeps its recorded CRAP:")
        for n in move_notes(found, baseline):
            print(f"  {n}")
    baseline = with_moves(baseline, found)
    if "--prune" in args:
        crap_store(pruned(measured, baseline, CRAP_LIMIT), {**base_cc, **cc_now})
        baseline = load(CRAP_BASELINE, "rust")
    notes = gains(measured, baseline, CRAP_LIMIT)
    if notes:
        print(f"crap-ratchet: {len(notes)} baseline entr{'y' if len(notes) == 1 else 'ies'} beaten; keep the gain with "
              "`python3 scripts/complexity-ratchet.py --crap crap.json --prune` (crap.json from the coverage job's "
              "`crap-report` artifact) and commit the baseline:")
        for n in notes:
            print(f"  {n}")
    problems = judge(measured, baseline, load(ALLOWLIST, "rust"), CRAP_LIMIT, "CRAP")
    if problems:
        print(f"crap-ratchet: {len(problems)} problem(s). Test a function's untested branches, or split it by "
              "responsibility, rather than raise a limit.")
        for p in problems:
            print(f"  {p}")
        sys.exit(1)
    print(f"crap-ratchet passed: {len(measured)} functions scored, {len(baseline)} in the baseline")


def main():
    args = sys.argv[1:]
    if "--self-test" in args:
        self_test()
        return
    if "--crap" in args:
        crap_main(args)
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
