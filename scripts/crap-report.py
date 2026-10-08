#!/usr/bin/env python3
# crap-report.py: every Rust function whose CRAP score is above a threshold
# (default 10), worst first. It reports; it never fails a build on a score.
#
#   CRAP = complexity^2 * (1 - line coverage)^3 + complexity
#
# CRAP is never below a function's complexity, so a function over the
# threshold either needs splitting (its complexity alone is over) or needs
# tests (it is simple, but too little of it runs under test).
#
# Complexity is the complexity ratchet's own reading (`complexity-ratchet.py`:
# rust-code-analysis cyclomatic, the decisions inside closures counted, nested
# named fns and test code excluded), taken from that script so the two never
# disagree. Line coverage per function comes from `cargo llvm-cov report
# --json` exports: each line takes the count of the innermost code region that
# covers it (the largest count across a generic function's instantiations),
# and a function's coverage is its covered lines over its instrumented lines,
# a nested named fn's lines excluded. A function with no instrumented line
# (compiled only on another platform, or never built by the measured run) is
# listed as having no coverage data, never as 0%.
#
#   python3 scripts/crap-report.py --coverage cov-full.json \
#       [--coverage cov-runtime-full.json] [--threshold 10] \
#       [--json crap.json] [--markdown crap.md] [--top 50]
#   python3 scripts/crap-report.py --self-test
#
# Stdlib only, plus rust-code-analysis-cli (RCA_BIN), like the ratchet.
import collections
import importlib.util
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def ratchet():
    sys.path.insert(0, HERE)
    spec = importlib.util.spec_from_file_location("complexity_ratchet", os.path.join(HERE, "complexity-ratchet.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def crap(cc, cov):
    return cc ** 2 * (1 - cov) ** 3 + cc


def functions(r):
    """Every non-test named fn: key, file, line span, nested fn spans, complexity."""
    found = []

    def unit_fns(unit, rel, tests, text):
        lines = text.split("\n")
        mods = r.mod_blocks(text)

        def walk(space, owners):
            for s in space["spaces"]:
                idx = s["start_line"] - 1
                if s["kind"] == "function" and s["name"] != "<anonymous>":
                    name = r.with_cfg(s["name"], lines, idx)
                    if idx not in tests:
                        cc = s["metrics"]["cyclomatic"]["sum"] - r.named_fns_sum(s) - r.closures(s)
                        nested = [(c["start_line"], c["end_line"]) for c in s["spaces"]
                                  if c["kind"] == "function" and c["name"] != "<anonymous>"]
                        found.append({"key": "::".join([rel, *r.mod_path(mods, lines, idx), *owners, name]),
                                      "file": rel, "start": s["start_line"], "end": s["end_line"],
                                      "nested": nested, "cc": int(cc)})
                    walk(s, [*owners, name])
                elif s["kind"] == "impl":
                    walk(s, [*owners, r.impl_label(lines, idx)])
                elif s["kind"] == "trait" and s["name"]:
                    walk(s, [*owners, r.with_cfg(s["name"], lines, idx)])
                else:
                    walk(s, owners)

        walk(unit, [])

    _units(r, unit_fns)
    return found


def _units(r, visit):
    import subprocess
    import tempfile
    rca = os.environ.get("RCA_BIN", "rust-code-analysis-cli")
    files = r.rust_sources()
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
                visit(unit, rel, r.test_lines(text), text)


def repo_path(name, cache={}):
    """The repo-relative path of a coverage filename, whatever directory the
    measured checkout lived in: the longest suffix that exists here."""
    if name not in cache:
        parts = name.split("/")
        cache[name] = next(("/".join(parts[i:]) for i in range(1, len(parts))
                            if os.path.isfile(os.path.join(ROOT, *parts[i:]))), None)
    return cache[name]


def paint(exports):
    """rel -> line -> count, from the innermost code region on each line."""
    lines = collections.defaultdict(dict)
    for data in exports:
        for f in data["data"][0]["functions"]:
            rel = repo_path(f["filenames"][0])
            if rel is None:
                continue
            for ls, cs, le, ce, count, file_id, _expanded, kind in f["regions"]:
                if file_id != 0 or kind != 0:
                    continue
                span = (le - ls, ce - cs)
                for ln in range(ls, le + 1):
                    cur = lines[rel].get(ln)
                    if cur is None or span < cur[0] or (span == cur[0] and count > cur[1]):
                        lines[rel][ln] = (span, count)
    return {rel: {ln: c for ln, (_s, c) in by_line.items()} for rel, by_line in lines.items()}


def score(fns, counts):
    rows = []
    for f in fns:
        by_line = counts.get(f["file"], {})
        own = [ln for ln in range(f["start"], f["end"] + 1) if not any(a <= ln <= b for a, b in f["nested"])]
        inst = [ln for ln in own if ln in by_line]
        row = {k: v for k, v in f.items() if k != "nested"}
        if inst:
            cov = sum(1 for ln in inst if by_line[ln] > 0) / len(inst)
            row.update(cov=round(cov, 4), crap=round(crap(f["cc"], cov), 1))
        else:
            row.update(cov=None, crap=None)
        rows.append(row)
    return rows


def markdown(rows, threshold, top):
    have = [x for x in rows if x["crap"] is not None]
    over = sorted((x for x in have if x["crap"] > threshold), key=lambda x: (-x["crap"], x["key"]))
    split = sum(1 for x in over if x["cc"] > threshold)
    out = [f"## CRAP over {threshold}: {len(over)} of {len(rows)} functions", "",
           f"{split} are over on complexity alone (split them); {len(over) - split} are simple enough "
           f"but too little of them runs under test (test them). {len(rows) - len(have)} have no coverage data.", "",
           "| CRAP over | Functions |", "|---|---:|"]
    out += [f"| {t} | {sum(1 for x in have if x['crap'] > t)} |" for t in sorted({threshold, 15, 30, 100})]
    shown = over if top is None else over[:top]
    out += ["", "| CRAP | Complexity | Coverage | Function |", "|---:|---:|---:|---|"]
    out += [f"| {x['crap']:.1f} | {x['cc']} | {x['cov'] * 100:.1f}% | `{x['key']}` (line {x['start']}) |" for x in shown]
    if len(shown) < len(over):
        out += ["", f"{len(over) - len(shown)} more in the full report."]
    return "\n".join(out) + "\n"


def self_test():
    assert crap(10, 1.0) == 10 and crap(10, 0.0) == 110 and abs(crap(4, 0.5) - 6) < 1e-9
    export = {"data": [{"functions": [
        {"filenames": [os.path.join("/elsewhere/checkout", "scripts", "crap-report.py")],
         "regions": [[1, 1, 10, 2, 5, 0, 0, 0],    # outer region, run
                     [4, 1, 5, 9, 0, 0, 0, 0],     # inner region, never run: wins on lines 4-5
                     [7, 1, 7, 9, 0, 1, 0, 0],     # another file's region (a macro expansion): ignored
                     [8, 1, 8, 9, 0, 0, 0, 2]]},   # not a code region: ignored
        {"filenames": [os.path.join("/elsewhere/checkout", "scripts", "crap-report.py")],
         "regions": [[4, 1, 5, 9, 3, 0, 0, 0]]},   # another instantiation ran lines 4-5
        {"filenames": ["/nowhere/at/all.rs"], "regions": [[1, 1, 2, 1, 1, 0, 0, 0]]}]}]}
    counts = paint([export])
    rel = "scripts/crap-report.py"
    assert set(counts) == {rel}, counts.keys()
    assert counts[rel][4] == 3 and counts[rel][5] == 3, "the largest count across instantiations wins on a line"
    assert counts[rel][1] == 5 and counts[rel][7] == 5 and counts[rel][8] == 5
    lone = paint([{"data": [{"functions": [export["data"][0]["functions"][0]]}]}])
    assert lone[rel][4] == 0, "the innermost region decides a line"
    fns = [{"key": "a", "file": rel, "start": 1, "end": 10, "nested": [(9, 10)], "cc": 4},
           {"key": "b", "file": "src/missing.rs", "start": 1, "end": 3, "nested": [], "cc": 2}]
    a, b = score(fns, lone)
    assert a["cov"] == 0.75, a        # lines 1-8 instrumented, 4 and 5 unrun; 9-10 belong to the nested fn
    assert abs(a["crap"] - round(crap(4, 0.75), 1)) < 1e-9
    assert b["cov"] is None and b["crap"] is None, "no instrumented line is no data, never 0%"
    md = markdown([a, b], 4, None)
    assert "CRAP over 4: 1 of 2" in md and "1 have no coverage data" in md, md
    print("crap-report self-test passed")


def main():
    args = sys.argv[1:]
    if "--self-test" in args:
        self_test()
        return
    value = lambda flag, default=None: args[args.index(flag) + 1] if flag in args else default
    covs = [args[i + 1] for i, a in enumerate(args) if a == "--coverage"]
    if not covs:
        sys.exit("crap-report: give at least one --coverage <llvm-cov export JSON>")
    threshold = float(value("--threshold", "10"))
    threshold = int(threshold) if threshold == int(threshold) else threshold
    top = value("--top")
    rows = score(functions(ratchet()), paint(json.load(open(c)) for c in covs))
    if len(rows) < 1000:
        sys.exit(f"crap-report: the measurement looks broken ({len(rows)} functions)")
    if not any(x["crap"] is not None for x in rows):
        sys.exit("crap-report: no function matched the coverage files; were they measured from this checkout?")
    if value("--json"):
        json.dump(sorted(rows, key=lambda x: x["key"]), open(value("--json"), "w"), indent=1)
    report = markdown(rows, threshold, int(top) if top else None)
    if value("--markdown"):
        open(value("--markdown"), "w").write(report)
    print(report.split("\n\n| CRAP |")[0])


if __name__ == "__main__":
    main()
