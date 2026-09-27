#!/usr/bin/env python3
# flow-action-guard.py (4.0): no hand-written flow action outside the one
# vocabulary.
#
# Every flow action is a `darkmux_flow::FlowAction` variant, and its wire
# string lives in exactly one place: `crates/darkmux-flow/src/action.rs`
# (retired spellings in `legacy.rs`, beside it). This guard scans production
# Rust for a string literal that IS a flow action, current or retired, and
# fails on it: a producer must build `FlowAction::...`, a consumer must match
# on it.
#
# What counts as a literal action: the literal's text equals a wire string in
# action.rs, or an old spelling or retired action in legacy.rs of two or more
# words. The list
# is READ from those two files, so the guard cannot drift from the
# vocabulary. The one-word retired spellings (`note`, `catch`) are left out:
# as literals they are ordinary words far more often than actions.
#
# What is not scanned: the two vocabulary files; tests (`tests/`, `*_tests.rs`,
# and everything after a file's `#[cfg(test)] mod tests` marker); comment lines.
#
# A literal that shares a spelling with an action but is NOT one (a runtime
# trajectory event type, which `dispatch.turn` also names) is allowlisted with
# a marker on its line or the line above:
#   // flow-action-guard:allow — <reason>
#
# Stdlib only. `--self-test` proves the guard can fail before trusting a pass.

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ACTION_RS = "crates/darkmux-flow/src/action.rs"
LEGACY_RS = "crates/darkmux-flow/src/legacy.rs"
SCAN_DIRS = ["crates", "src"]
ALLOW = "flow-action-guard:allow"
TEST_MARKER = re.compile(r"\n#\[cfg\(test\)\]\n(?:pub(?:\(crate\))? )?mod \w*tests?\w* \{")
LITERAL = re.compile(r'"((?:[^"\\]|\\.)*)"')


def vocabulary(root):
    act = open(os.path.join(root, ACTION_RS)).read()
    leg = open(os.path.join(root, LEGACY_RS)).read()
    current = set(re.findall(r'^\s+\w+ => \w+, "([^"]+)";', act, re.M))
    old_spellings = set()
    for arm in re.findall(r'^\s+((?:"[^"]+"\s*\|\s*)*"[^"]+") => FlowAction::\w+,', leg, re.M):
        old_spellings.update(re.findall(r'"([^"]+)"', arm))
    retired = set(re.findall(r'^\s+\w+ => "([^"]+)";', leg, re.M))
    retired |= {r for r in old_spellings if re.search(r"[ .\-]", r)}
    if len(current) < 20:
        sys.exit(f"flow-action-guard: read only {len(current)} actions from {ACTION_RS}; the parse is broken")
    return current | retired


def production_body(text):
    m = TEST_MARKER.search(text)
    return text[: m.start()] if m else text


def violations(text, vocab):
    out = []
    lines = production_body(text).split("\n")
    for i, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("//"):
            continue
        code = line.split("//", 1)[0] if "//" in line and '"' not in line.split("//", 1)[1] else line
        for lit in LITERAL.findall(code):
            if lit not in vocab:
                continue
            prev = lines[i - 1] if i > 0 else ""
            if ALLOW in line or ALLOW in prev:
                continue
            out.append((i + 1, lit))
    return out


def scan(root, vocab):
    found = []
    skip = {os.path.normpath(ACTION_RS), os.path.normpath(LEGACY_RS)}
    for d in SCAN_DIRS:
        for dirpath, dirnames, filenames in os.walk(os.path.join(root, d)):
            dirnames[:] = [n for n in dirnames if n not in ("target", "tests")]
            for name in filenames:
                if not name.endswith(".rs") or name.endswith("_tests.rs"):
                    continue
                path = os.path.join(dirpath, name)
                rel = os.path.normpath(os.path.relpath(path, root))
                if rel in skip:
                    continue
                for line_no, lit in violations(open(path, encoding="utf-8").read(), vocab):
                    found.append(f"{rel}:{line_no}: \"{lit}\"")
    return found


def self_test():
    vocab = {"dispatch.start", "dispatch start"}
    assert violations('let a = "dispatch.start";', vocab) == [(1, "dispatch.start")]
    assert violations('emit("dispatch start", x)', vocab) == [(1, "dispatch start")]
    assert violations('// "dispatch.start" in prose', vocab) == []
    assert violations('let k = "dispatch.map";', vocab) == []
    assert violations('// flow-action-guard:allow — trajectory event\n"dispatch.start" => x', vocab) == []
    assert violations('"dispatch.start" => x, // flow-action-guard:allow — trajectory event', vocab) == []
    assert violations('fn f() {}\n#[cfg(test)]\nmod tests {\n    let a = "dispatch.start";\n}', vocab) == []
    print("flow-action-guard self-test passed")


def main():
    if "--self-test" in sys.argv:
        self_test()
        return
    vocab = vocabulary(ROOT)
    found = scan(ROOT, vocab)
    if found:
        print("flow-action-guard: flow actions written as string literals (build or match FlowAction instead):")
        for f in found:
            print(f"  {f}")
        sys.exit(1)
    print(f"flow-action-guard passed: no literal flow action outside the vocabulary ({len(vocab)} spellings checked)")


if __name__ == "__main__":
    main()
