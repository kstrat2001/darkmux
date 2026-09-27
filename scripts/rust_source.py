"""Where test code sits in a Rust source file, for the repo's static guards
(`flow-action-guard.py`, `complexity-ratchet.py`): one reading, so the guards
cannot disagree about what counts as a test."""

import os
import re

TEST_MOD = re.compile(r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")
RAW_START = re.compile(r'b?r(#*)"')
CHAR_LIT = re.compile(r"'(?:\\.[^']{0,8}|[^\\'])'")


def is_rust_test_file(rel):
    """A file that is test code whole: under a `tests/` directory, a
    `*_tests.rs` sibling module, or a `tests.rs` module."""
    base = os.path.basename(rel)
    return "/tests/" in "/" + rel or base.endswith("_tests.rs") or base == "tests.rs"


def skip_token(text, i):
    """If a string, char literal or comment starts at `i`, the index just past
    it; otherwise `i`. Braces inside those never count toward nesting."""
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
    if text[i] == '"':
        j = i + 1
        while j < len(text) and text[j] != '"':
            j += 2 if text[j] == "\\" else 1
        return j + 1
    m = CHAR_LIT.match(text, i)
    return m.end() if m else i


def block_end(text, i):
    """The index just past the `}` closing the block whose `{` ends at `i`."""
    depth = 1
    while i < len(text) and depth:
        j = skip_token(text, i)
        if j != i:
            i = j
            continue
        depth += {"{": 1, "}": -1}.get(text[i], 0)
        i += 1
    return i


def test_lines(text):
    """The 0-based line numbers inside a `#[cfg(test)] mod x { ... }` block,
    found by brace matching, so code AFTER a test module is not one."""
    lines = set()
    for m in TEST_MOD.finditer(text):
        first = text.count("\n", 0, m.start())
        last = text.count("\n", 0, block_end(text, m.end()))
        lines.update(range(first, last + 1))
    return lines


MOD_BLOCK = re.compile(r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*\{", re.M)
CFG_ATTR = re.compile(r"^#\[cfg\((.*)\)\]$")


def mod_blocks(text):
    """(name, first line, last line) of every inline `mod name { ... }`,
    0-based, found by the same brace matching as `test_lines`."""
    out = []
    for m in MOD_BLOCK.finditer(text):
        first = text.count("\n", 0, m.start())
        last = text.count("\n", 0, block_end(text, m.end()))
        out.append((m.group(1), first, last))
    return out


def cfgs_above(lines, idx):
    """The `#[cfg(...)]` attributes on the item at 0-based line `idx`, read
    upward through its other attributes and doc comments, whitespace dropped."""
    cfgs = []
    j = idx - 1
    while j >= 0 and lines[j].strip().startswith(("#[", "///", "//")):
        m = CFG_ATTR.match(lines[j].strip())
        if m:
            cfgs.insert(0, re.sub(r"\s+", "", m.group(1)))
        j -= 1
    return cfgs
