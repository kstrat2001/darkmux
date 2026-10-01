#!/usr/bin/env python3
# flow-reader-guard.py (5.0, #3035): production code reads a flow record
# through `darkmux_flow::reader`, never straight from serde.
#
# `darkmux_flow::reader` is the one lenient reader (contract 5): it upgrades a
# retired action, `source`, `tier` or payload key to its current spelling and
# keeps an action this binary does not know as `FlowAction::Other`. A consumer
# that calls `serde_json::from_str::<FlowRecord>` (or `from_value`, `from_slice`,
# `from_reader`) skips all of that, so the same archive reads one way there and
# another through the daemon.
#
# Flagged, in production Rust (every crate, `src/`, `runtime/`, `plugins/`;
# test code excluded: `tests/` directories, `*_tests.rs`, `tests.rs`, and each
# `#[cfg(test)] mod x { ... }` block):
#   * a `serde_json::from_*` call whose turbofish names `FlowRecord`
#     (`from_str::<FlowRecord>`, `from_value::<Vec<FlowRecord>>`);
#   * a binding annotated with a type naming `FlowRecord` and initialized from
#     one of those calls (`let r: FlowRecord = serde_json::from_str(..)`).
# A call whose target type is only inferred (a function returning `FlowRecord`)
# is not seen: this guard bounds the spelling, it does not prove the reader is
# the only door.
#
# Not scanned: comments, and the reader itself (`crates/darkmux-flow/src/reader.rs`).
# A deliberate exception takes a marker on its line or the line above; one
# marker covers ONE hit:
#   // flow-reader-guard:allow — <reason>
#
# Stdlib only. `--self-test` proves each shape can fail before trusting a pass.

import os
import re
import subprocess
import sys

from rust_source import is_rust_test_file, test_lines

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
READER_RS = "crates/darkmux-flow/src/reader.rs"
RUST_ROOTS = ("crates/", "src/", "runtime/", "plugins/")
ALLOW = "flow-reader-guard:allow"
CALL = r"serde_json\s*::\s*from_(?:str|value|slice|reader)"
TURBOFISH = re.compile(CALL + r"\s*::\s*<[^;{}]*?\bFlowRecord\b")
ANNOTATED = re.compile(r"\bFlowRecord\b[^=;{}]*=\s*" + CALL + r"\b")


def mask_comments(text):
    """`text` with every `//` and `/* */` comment blanked (newlines kept), so
    a comment naming the call is not a hit and line numbers still match."""
    def blank(m):
        return re.sub(r"[^\n]", " ", m.group(0))
    return re.sub(r"//[^\n]*|/\*.*?\*/", blank, text, flags=re.S)


def scan_text(text, rel):
    """The (line, why) hits in one file's text; `rel` decides what is exempt."""
    if rel == READER_RS or is_rust_test_file(rel):
        return []
    lines = text.split("\n")
    in_tests = test_lines(text)
    hits = []
    for pattern, why in ((TURBOFISH, "a turbofish reads FlowRecord straight from serde"),
                         (ANNOTATED, "a FlowRecord binding is read straight from serde")):
        for m in pattern.finditer(mask_comments(text)):
            line = text.count("\n", 0, m.start())
            if line in in_tests:
                continue
            nearby = lines[line] + (lines[line - 1] if line else "")
            if ALLOW in nearby:
                continue
            hits.append((line + 1, why))
    return sorted(set(hits))


def tracked_rust():
    out = subprocess.run(["git", "ls-files", "*.rs"], cwd=ROOT, capture_output=True, text=True, check=True).stdout
    return [f for f in out.split("\n") if f.startswith(RUST_ROOTS)]


def scan():
    failures = []
    for rel in tracked_rust():
        path = os.path.join(ROOT, rel)
        if not os.path.exists(path):
            continue
        with open(path, encoding="utf-8", errors="replace") as fh:
            for line, why in scan_text(fh.read(), rel):
                failures.append(f"{rel}:{line}: {why}; read it through darkmux_flow::reader (parse_record / parse_value)")
    return failures


def self_test():
    prod = "crates/darkmux-serve/src/x.rs"
    bad = {
        "turbofish str": 'fn f(l: &str) { let _ = serde_json::from_str::<FlowRecord>(l); }',
        "turbofish value": 'fn f(v: Value) { serde_json::from_value::<darkmux_flow::FlowRecord>(v).ok(); }',
        "turbofish slice": 'fn f(b: &[u8]) { serde_json::from_slice::<FlowRecord>(b); }',
        "turbofish reader": 'fn f(r: File) { serde_json::from_reader::<_, FlowRecord>(r); }',
        "turbofish vec": 'fn f(l: &str) { serde_json::from_str::<Vec<FlowRecord>>(l); }',
        "annotated let": 'fn f(l: &str) { let r: FlowRecord = serde_json::from_str(l).unwrap(); }',
        "annotated option": 'fn f(l: &str) { let r: Option<FlowRecord> = serde_json::from_str(l).ok(); }',
        "multi-line turbofish": 'fn f(l: &str) {\n    serde_json::from_str::<\n        FlowRecord,\n    >(l);\n}',
        "after a test module": '#[cfg(test)]\nmod t {\n    fn a() {}\n}\nfn f(l: &str) { serde_json::from_str::<FlowRecord>(l); }',
    }
    ok = {
        "the reader": ("crates/darkmux-flow/src/reader.rs", 'fn f(v: Value) { serde_json::from_value::<FlowRecord>(v); }'),
        "a test file": ("crates/darkmux-flow/src/foo_tests.rs", 'fn f(l: &str) { serde_json::from_str::<FlowRecord>(l); }'),
        "a tests dir": ("crates/darkmux-flow/tests/x.rs", 'fn f(l: &str) { serde_json::from_str::<FlowRecord>(l); }'),
        "a cfg(test) mod": (prod, '#[cfg(test)]\nmod t {\n    fn a(l: &str) { serde_json::from_str::<FlowRecord>(l); }\n}'),
        "a comment": (prod, '// serde_json::from_str::<FlowRecord>(l) is what not to do\nfn f() {}'),
        "the reader's door": (prod, 'fn f(l: &str) { darkmux_flow::reader::parse_record(l); }'),
        "another type": (prod, 'fn f(l: &str) { serde_json::from_str::<FlowStatus>(l); let r: Foo = serde_json::from_str(l).unwrap(); }'),
        "an allowed hit": (prod, '// flow-reader-guard:allow — reads the hub wire, which is not an archive\nfn f(l: &str) { serde_json::from_str::<FlowRecord>(l); }'),
    }
    problems = []
    for name, src in bad.items():
        if not scan_text(src, prod):
            problems.append(f"missed a hit it must catch: {name}")
    for name, (rel, src) in ok.items():
        hits = scan_text(src, rel)
        if hits:
            problems.append(f"flagged what it must pass: {name}: {hits}")
    two = 'fn f(l: &str) {\n    // flow-reader-guard:allow — one\n    serde_json::from_str::<FlowRecord>(l);\n    serde_json::from_str::<FlowRecord>(l);\n}'
    if len(scan_text(two, prod)) != 1:
        problems.append("an allow marker must cover exactly one hit")
    return problems


def main():
    if "--self-test" in sys.argv:
        problems = self_test()
        for p in problems:
            print(f"self-test: {p}")
        print("flow-reader-guard self-test: " + ("FAILED" if problems else "ok"))
        return 1 if problems else 0
    failures = scan()
    for f in failures:
        print(f)
    print(f"flow-reader-guard: {len(failures)} direct FlowRecord read(s) in production code")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
