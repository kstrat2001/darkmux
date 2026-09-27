#!/usr/bin/env python3
# flow-action-guard.py (4.0): no hand-written flow action outside the one
# vocabulary.
#
# Every flow action is a `darkmux_flow::FlowAction` variant, and its wire
# string lives in exactly one place: `crates/darkmux-flow/src/action.rs` (old
# spellings and retired actions in `legacy.rs`, beside it). This guard scans
# production Rust (every crate, `src/`, `runtime/`, `plugins/`) and fails on
# a flow action written by hand, in any of these shapes:
#
#   * a string literal (plain or raw) that IS an action, current or old;
#   * a format string that builds one: `format!("mission.run.{}", verb)`, any
#     literal whose text before its first `{` is a dotted prefix of an action;
#   * a prefix test: `.starts_with("dispatch ")` / `.strip_prefix(..)` /
#     `.ends_with(..)` / `.contains(..)` on a scope prefix or action prefix;
#   * `concat!` of literals that joins into an action;
#   * an action inside a JSON string literal: `"action":"dispatch start"`,
#     escaped or raw.
#
# The vocabulary is READ from action.rs and legacy.rs, so the guard cannot
# drift from it. The one-word old spellings (`note`, `catch`) are left out:
# as literals they are ordinary words far more often than actions.
#
# Tests are not scanned: `tests/` directories, `*_tests.rs`, a `tests.rs`
# module, and each `#[cfg(test)] mod <name> { ... }` block, found by brace
# matching, so code AFTER a test module is still scanned. Comment lines are
# skipped. Nor is the trajectory event vocabulary
# (`crates/darkmux-trajectory/src/event.rs`), the one place a runtime
# trajectory event type is spelled; some of those share a spelling with a
# flow action (`dispatch.checkpoint`).
#
# Any other literal that shares a spelling with an action but is NOT one is
# allowed with a marker on its line or the line above; one marker covers ONE
# hit:
#   // flow-action-guard:allow — <reason>
#
# Stdlib only. `--self-test` proves each shape can fail before trusting a pass.

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ACTION_RS = "crates/darkmux-flow/src/action.rs"
LEGACY_RS = "crates/darkmux-flow/src/legacy.rs"
TRAJECTORY_EVENTS_RS = "crates/darkmux-trajectory/src/event.rs"
SCAN_DIRS = ["crates", "src", "runtime", "plugins"]
ALLOW = "flow-action-guard:allow"
TEST_MOD = re.compile(r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")
PLAIN = re.compile(r'(?<![A-Za-z0-9_#])"((?:[^"\\]|\\.)*)"')
RAW = re.compile(r'r(#+)"(.*?)"\1')
PREFIX_CALL = re.compile(r'\.(starts_with|strip_prefix|ends_with|contains)\(\s*"((?:[^"\\]|\\.)*)"')
CONCAT = re.compile(r'concat!\(([^)]*)\)')
RAW_START = re.compile(r'b?r(#*)"')
CHAR_LIT = re.compile(r"'(?:\\.[^']{0,8}|[^\\'])'")
JSON_ACTION = re.compile(r'\\?"action\\?"\s*:\s*\\?"([^"\\]+)')


def vocabulary(root):
    act = open(os.path.join(root, ACTION_RS)).read()
    leg = open(os.path.join(root, LEGACY_RS)).read()
    current = set(re.findall(r'^\s+\w+ => \w+, "([^"]+)";', act, re.M))
    old = set(re.findall(r'^\s+\("([^"]+)", FlowAction::\w+\),', leg, re.M))
    retired = set(re.findall(r'^\s+\w+ => "([^"]+)";', leg, re.M))
    if len(current) < 20 or len(old) < 20 or len(retired) < 5:
        sys.exit(f"flow-action-guard: vocabulary parse is broken ({len(current)}/{len(old)}/{len(retired)})")
    multiword = {w for w in old | retired if re.search(r"[ .\-]", w)}
    return current | multiword


def skip_token(text, i):
    """If a string, char literal or comment starts at `i`, the index just past
    it; otherwise `i`. Braces inside those never count toward nesting."""
    c = text[i]
    if text.startswith("//", i):
        j = text.find("\n", i)
        return len(text) if j < 0 else j
    if text.startswith("/*", i):
        j = text.find("*/", i + 2)
        return len(text) if j < 0 else j + 2
    m = RAW_START.match(text, i)
    if m:
        end = text.find('"' + m.group(1), m.end())
        return len(text) if end < 0 else end + 1 + len(m.group(1))
    if c == '"':
        j = i + 1
        while j < len(text) and text[j] != '"':
            j += 2 if text[j] == "\\" else 1
        return j + 1
    m = CHAR_LIT.match(text, i)
    if m:
        return m.end()
    return i


def strip_test_modules(text):
    """Blank out every `#[cfg(test)] mod x { ... }` block, keeping line count.
    The block's end is found by brace matching that skips strings, char
    literals and comments."""
    out = list(text)
    for m in TEST_MOD.finditer(text):
        depth, i = 1, m.end()
        while i < len(text) and depth:
            j = skip_token(text, i)
            if j != i:
                i = j
                continue
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            i += 1
        for j in range(m.start(), i):
            if out[j] != "\n":
                out[j] = " "
    return "".join(out)


def is_prefix(s, vocab):
    return len(s) >= 3 and any(w.startswith(s) and w != s for w in vocab)


def hits_in_line(line, vocab):
    found = []
    raws = [m.group(2) for m in RAW.finditer(line)]
    plain_line = RAW.sub(" ", line)
    literals = [m.group(1) for m in PLAIN.finditer(plain_line)] + raws
    for lit in literals:
        if lit in vocab:
            found.append(lit)
            continue
        if "{" in lit:
            head = lit.split("{", 1)[0]
            if head.endswith(".") and is_prefix(head, vocab):
                found.append(lit)
                continue
        for a in JSON_ACTION.findall(lit):
            if a in vocab:
                found.append(lit)
    for _call, arg in PREFIX_CALL.findall(plain_line):
        if arg not in vocab and is_prefix(arg, vocab) and arg[-1] in ". ":
            found.append(arg)
    for args in CONCAT.findall(plain_line):
        joined = "".join(re.findall(r'"((?:[^"\\]|\\.)*)"', args))
        if joined in vocab or (joined.endswith(".") and is_prefix(joined, vocab)):
            found.append(f"concat!({joined})")
    return found


def marker_only(line, vocab):
    """True for a line carrying an allow marker and no hit of its own: only
    such a line lends its marker to the line below it."""
    if ALLOW not in line:
        return False
    return not hits_in_line(line.split("//", 1)[0], vocab)


def violations(text, vocab):
    out = []
    lines = strip_test_modules(text).split("\n")
    for i, line in enumerate(lines):
        if line.strip().startswith("//"):
            continue
        code = line.split("//", 1)[0] if "//" in line and '"' not in line.split("//", 1)[1] else line
        hits = hits_in_line(code, vocab)
        allowed = int(ALLOW in line) + int(i > 0 and marker_only(lines[i - 1], vocab) and ALLOW not in line)
        for hit in hits[allowed:]:
            out.append((i + 1, hit))
    return out


def scan(root, vocab):
    found = []
    skip = {os.path.normpath(p) for p in (ACTION_RS, LEGACY_RS, TRAJECTORY_EVENTS_RS)}
    for d in SCAN_DIRS:
        for dirpath, dirnames, filenames in os.walk(os.path.join(root, d)):
            dirnames[:] = [n for n in dirnames if n not in ("target", "tests", "node_modules")]
            for name in filenames:
                if not name.endswith(".rs") or name.endswith("_tests.rs") or name == "tests.rs":
                    continue
                path = os.path.join(dirpath, name)
                rel = os.path.normpath(os.path.relpath(path, root))
                if rel in skip:
                    continue
                for line_no, lit in violations(open(path, encoding="utf-8").read(), vocab):
                    found.append(f"{rel}:{line_no}: \"{lit}\"")
    return found


def self_test():
    vocab = {"dispatch.start", "dispatch start", "mission.run.finalize"}
    cases = [
        ('let a = "dispatch.start";', 1),
        ('emit("dispatch start", x)', 1),
        ('let a = r#"dispatch.start"#;', 1),
        ('let a = format!("mission.run.{}", verb);', 1),
        ('if a.starts_with("dispatch ") {}', 1),
        ('if a.starts_with("dispatch.") {}', 1),
        ('let a = concat!("dispatch", ".start");', 1),
        ('let j = "{\\"action\\":\\"dispatch start\\"}";', 1),
        ('let j = r#"{"action":"dispatch.start","x":1}"#;', 1),
        ('// "dispatch.start" in prose', 0),
        ('let k = "dispatch.map";', 0),
        ('let k = format!("step {} of {}", a, b);', 0),
        ('// flow-action-guard:allow — trajectory event\n"dispatch.start" => x', 0),
        ('"dispatch.start" => x, // flow-action-guard:allow — trajectory event', 0),
        ('// flow-action-guard:allow — one only\nf("dispatch.start", "dispatch start")', 1),
        ('f("dispatch.start") // flow-action-guard:allow — this line\ng("dispatch start")', 1),
        ('fn f() {}\n#[cfg(test)]\nmod tests {\n    let a = "dispatch.start";\n}', 0),
        ('#[cfg(test)]\nmod tests {\n    fn t() { let a = 1; }\n}\nfn after() { let a = "dispatch.start"; }', 1),
        ('#[cfg(test)]\nmod tests {\n    fn t() { let a = "{"; let b = \'{\'; }\n}\nfn after() { let a = "dispatch.start"; }', 1),
    ]
    for src, want in cases:
        got = len(violations(src, vocab))
        assert got == want, f"{src!r}: {got} hits, want {want}"
    print("flow-action-guard self-test passed")


def main():
    if "--self-test" in sys.argv:
        self_test()
        return
    vocab = vocabulary(ROOT)
    found = scan(ROOT, vocab)
    if found:
        print("flow-action-guard: flow actions written by hand (build or match FlowAction instead):")
        for f in found:
            print(f"  {f}")
        sys.exit(1)
    print(f"flow-action-guard passed: no hand-written flow action outside the vocabulary ({len(vocab)} spellings checked)")


if __name__ == "__main__":
    main()
