#!/usr/bin/env python3
# flow-action-guard.py (4.0): no flow action outside the one vocabulary.
#
# Every flow action is a `darkmux_flow::FlowAction` variant, and its wire
# string lives in exactly one place: `crates/darkmux-flow/src/action.rs` (old
# spellings and retired actions in `legacy.rs`, beside it). The guard READS
# the vocabulary from those two files, so it cannot drift from them.
#
# Two tiers, by where a string lives:
#
# STRICT: production Rust (every crate, `src/`, `runtime/`, `plugins/`, test
# code excluded). No flow action is written by hand at all, current or old,
# in any of these shapes:
#   * a string literal (plain or raw) that IS an action;
#   * a format string that builds one: `format!("mission.run.{}", verb)`, any
#     literal whose text before its first `{` is a dotted prefix of an action;
#   * a prefix test: `.starts_with("dispatch ")` / `.strip_prefix(..)` /
#     `.ends_with(..)` / `.contains(..)` on a scope prefix or action prefix;
#   * `concat!` of literals that joins into an action;
#   * an action inside a JSON string literal: `"action":"dispatch start"`.
#
# CURRENT-ONLY: Rust test code (`tests/` directories, `*_tests.rs`, a
# `tests.rs` module, each `#[cfg(test)] mod x { ... }` block), `tests/**` and
# `ui/src/**` TypeScript/JavaScript, docs (`docs/**/*.html`, every `*.md`),
# `skills/**`, `templates/**`, and JSON/JSONL fixtures. A current wire
# spelling is fine there; flagged is any string that LOOKS like a flow action
# and is not one:
#   * an old or retired spelling (`dispatch start`, `mission.run.start`);
#   * a `<scope>.<event>[.<detail>]` string whose scope is a `FlowScope` and
#     which is not a current action (`dispatch.started`).
# In code, the string must be a whole literal; in docs, a whole code span
# (`...` or <code>...</code>); in fixtures, a JSON `"action"` value. An old
# SPACED spelling in TypeScript/JavaScript counts only on a line that names
# an action, because the viewer's activity labels (`"dispatch start"`) share
# those words.
#
# Not flow actions, though they share the grammar, and each READ from its own
# source: the step-kind ids (`fn id(&self)` in crates/ and src/), the
# trajectory event types (`crates/darkmux-trajectory/src/event.rs`), and the
# config keys (`src/config_cmd.rs`), the record stages (the generated
# `Stage.ts`: `tier-decision` is a stage as well as an old action), and, in a
# doc, the `darkmux mission` verbs (`src/cli.rs`: `mission abort` is a
# command as well as an old action). Also not: file names (`step.rs`) and
# field paths into an action's record (`hook.fired.payload.error`).
#
# Not scanned: comments (a comment may name a spelling to explain history);
# the vocabulary sources themselves; the generated TS bindings; and recorded
# archives, which carry whatever spelling was written when they were recorded:
# ARCHIVES below.
#
# Anything else that must carry an old spelling (a test of the lenient reader,
# say) takes a marker on its line or the line above; one marker covers ONE hit:
#   // flow-action-guard:allow — <reason>
# A run of such lines sits between `flow-action-guard:allow-start — <reason>`
# and `flow-action-guard:allow-end`.
#
# Stdlib only. `--self-test` proves each shape can fail before trusting a pass.

import glob
import os
import re
import subprocess
import sys

from rust_source import is_rust_test_file, test_lines

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ACTION_RS = "crates/darkmux-flow/src/action.rs"
LEGACY_RS = "crates/darkmux-flow/src/legacy.rs"
TRAJECTORY_EVENTS_RS = "crates/darkmux-trajectory/src/event.rs"
CONFIG_KEYS_RS = "src/config_cmd.rs"
STAGE_TS = "ui/src/types/generated/Stage.ts"
CLI_RS = "src/cli.rs"
VOCAB_SOURCES = (ACTION_RS, LEGACY_RS, TRAJECTORY_EVENTS_RS)
GENERATED_TS = "ui/src/types/generated/"
ARCHIVES = (
    "docs/demo/",
    "tests/parity/corpus/",
    "tests/parity/goldens/",
    "scripts/demo-env/sessions/",
    "tests/flow-archive-golden/",
    "crates/darkmux-lab/tests/fixtures/legacy-runs/",
    "CHANGELOG.md",
)
RUST_ROOTS = ("crates/", "src/", "runtime/", "plugins/", "tests/")
TSJS_ROOTS = ("tests/", "ui/src/")
FILE_EXTS = {"rs", "ts", "tsx", "js", "mjs", "cjs", "json", "jsonl", "md", "html",
             "py", "sh", "toml", "yml", "yaml", "txt", "css", "lock", "log", "png", "svg"}
ALLOW = "flow-action-guard:allow"
ALLOW_START = "flow-action-guard:allow-start"
ALLOW_END = "flow-action-guard:allow-end"
PLAIN = re.compile(r'(?<![A-Za-z0-9_#])"((?:[^"\\]|\\.)*)"')
RAW = re.compile(r'r(#+)"(.*?)"\1')
TS_LIT = re.compile(r'''"((?:[^"\\]|\\.)*)"|'((?:[^'\\]|\\.)*)'|`((?:[^`\\]|\\.)*)`''')
DOC_SPAN = re.compile(r"`([^`\n]+)`|<code>([^<\n]*)</code>")
PREFIX_CALL = re.compile(r'\.(starts_with|strip_prefix|ends_with|contains)\(\s*"((?:[^"\\]|\\.)*)"')
CONCAT = re.compile(r'concat!\(([^)]*)\)')
JSON_ACTION = re.compile(r'\\?"action\\?"\s*:\s*\\?"([^"\\]+)')
ACTION_WORD = re.compile(r"\baction\b")


class Vocab:
    """The flow-action vocabulary and the other dotted vocabularies that share
    its grammar, every one read from its own source."""

    def __init__(self, current, old, scopes, others, verbs=()):
        self.current = set(current)
        self.verbs = set(verbs)
        self.old = set(old)
        self.written = self.current | self.old
        self.others = set(others)
        self.grammar = re.compile(r"(?:%s)(?:\.[a-z][a-z0-9_-]*)+" % "|".join(sorted(scopes)))

    def looks_like_action(self, s):
        if not self.grammar.fullmatch(s) or s in self.others:
            return False
        if s.rsplit(".", 1)[1] in FILE_EXTS:
            return False
        return not any(s.startswith(a + ".payload") for a in self.current)

    def judge(self, s, strict):
        """Why `s` is flagged, or None. `strict` flags a current spelling too."""
        if s in self.current:
            return "a hand-written action" if strict else None
        if s in self.others:
            return None
        if s in self.old:
            return "an old or retired spelling"
        if self.looks_like_action(s):
            return "not a FlowAction"
        return None


def read(root, rel):
    return open(os.path.join(root, rel), encoding="utf-8").read()


def vocabulary(root):
    act, leg = read(root, ACTION_RS), read(root, LEGACY_RS)
    current = set(re.findall(r'^\s+\w+ => \w+, "([^"]+)";', act, re.M))
    scopes = set(re.findall(r'^\s+\w+ => "([a-z]+)";', act, re.M))
    old = set(re.findall(r'^\s+\("([^"]+)", FlowAction::\w+\),', leg, re.M))
    retired = set(re.findall(r'^\s+\w+ => "([^"]+)";', leg, re.M))
    if len(current) < 20 or len(scopes) < 10 or len(old) < 20 or len(retired) < 5:
        sys.exit(f"flow-action-guard: vocabulary parse is broken ({len(current)}/{len(scopes)}/{len(old)}/{len(retired)})")
    # The one-word old spellings (`note`, `catch`) are left out: as literals
    # they are ordinary words far more often than actions.
    multiword = {w for w in old | retired if re.search(r"[ .\-]", w)}
    return Vocab(current, multiword, scopes, other_vocabularies(root), mission_verbs(root))


def mission_verbs(root):
    """`mission <verb>` for each `darkmux mission` subcommand: in a doc, a
    code span `mission abort` is the command far more often than the old
    spelling of `mission.abort`."""
    text = read(root, CLI_RS)
    body = text.split("pub(crate) enum MissionCmd {", 1)[1].split("\n}\n", 1)[0]
    names = re.findall(r"^    ([A-Z]\w*)\b", body, re.M)
    if len(names) < 5:
        sys.exit(f"flow-action-guard: mission verb parse is broken ({len(names)})")
    return {"mission " + re.sub(r"(?<!^)([A-Z])", r"-\1", n).lower() for n in names}


def other_vocabularies(root):
    kinds = set()
    for d in ("crates", "src"):
        for path in glob.glob(os.path.join(root, d, "**", "*.rs"), recursive=True):
            text = open(path, encoding="utf-8").read()
            kinds |= set(re.findall(r"fn id\(&self\) -> &'static str \{\s*\"([^\"]+)\"", text))
    events = set(re.findall(r'#\[serde\(rename = "([^"]+)"\)\]', read(root, TRAJECTORY_EVENTS_RS)))
    keys = set(re.findall(r'^\s+\("([a-z_.]+)", Ty::', read(root, CONFIG_KEYS_RS), re.M))
    stages = set(re.findall(r'"([^"]+)"', read(root, STAGE_TS).split("export type Stage", 1)[1]))
    if len(kinds) < 5 or len(events) < 10 or len(keys) < 10 or len(stages) < 5:
        sys.exit(f"flow-action-guard: other-vocabulary parse is broken ({len(kinds)}/{len(events)}/{len(keys)}/{len(stages)})")
    return kinds | events | keys | stages


# --- Rust -------------------------------------------------------------------

def is_prefix(s, vocab):
    return len(s) >= 3 and any(w.startswith(s) and w != s for w in vocab.written)


def rust_strict_shapes(literals, code, vocab):
    """The shapes only production code may not write: a format prefix, a
    prefix test, a `concat!`."""
    found = []
    for lit in literals:
        head = lit.split("{", 1)[0]
        if "{" in lit and head.endswith(".") and is_prefix(head, vocab):
            found.append(lit)
    for _call, arg in PREFIX_CALL.findall(code):
        if arg not in vocab.written and is_prefix(arg, vocab) and arg[-1] in ". ":
            found.append(arg)
    for args in CONCAT.findall(code):
        joined = "".join(re.findall(r'"((?:[^"\\]|\\.)*)"', args))
        if joined in vocab.written or (joined.endswith(".") and is_prefix(joined, vocab)):
            found.append(f"concat!({joined})")
    return found


def rust_hits(line, vocab, strict):
    raws = [m.group(2) for m in RAW.finditer(line)]
    code = RAW.sub(" ", line)
    literals = [m.group(1) for m in PLAIN.finditer(code)] + raws
    found = []
    for lit in literals:
        if vocab.judge(lit, strict):
            found.append(lit)
        elif any(vocab.judge(a, strict) for a in JSON_ACTION.findall(lit)):
            found.append(lit)
    return found + (rust_strict_shapes(literals, code, vocab) if strict else [])


def rust_code(line):
    """The line without a trailing `//` comment, unless that comment holds a
    quote (then the `//` may sit inside a string)."""
    if "//" in line and '"' not in line.split("//", 1)[1]:
        return line.split("//", 1)[0]
    return line


def rust_violations(text, vocab, all_test=False):
    tests = test_lines(text)

    def hits(i, line):
        if line.strip().startswith("//"):
            return []
        return rust_hits(rust_code(line), vocab, strict=not (all_test or i in tests))

    return marked_violations(text.split("\n"), hits)


# --- TypeScript / JavaScript ------------------------------------------------

def is_comment(line):
    s = line.strip()
    return s.startswith(("//", "/*", "*"))


def ts_hits(line, vocab):
    if is_comment(line):
        return []
    code = line.split(" //", 1)[0] if " //" in line and "://" not in line else line
    names_action = bool(ACTION_WORD.search(code))
    found = []
    for m in TS_LIT.finditer(code):
        lit = next(g for g in m.groups() if g is not None)
        why = vocab.judge(lit, strict=False)
        if why and (" " not in lit or names_action):
            found.append(lit)
        elif any(vocab.judge(a, strict=False) for a in JSON_ACTION.findall(lit)):
            found.append(lit)
    return found


def blank_block_comments(text):
    """`text` with every `/* ... */` comment blanked (newlines kept), so a
    comment's continuation lines are not read as code. Strings are stepped
    over, so a `/*` inside one (a glob) opens nothing."""
    out, i, n = list(text), 0, len(text)
    while i < n:
        c = text[i]
        if c in "\"'`":
            j = i + 1
            while j < n and text[j] != c and not (c != "`" and text[j] == "\n"):
                j += 2 if text[j] == "\\" else 1
            i = j + 1
        elif text.startswith("//", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
        elif text.startswith("/*", i):
            j = text.find("*/", i + 2)
            j = n if j < 0 else j + 2
            for k in range(i, j):
                if out[k] != "\n":
                    out[k] = " "
            i = j
        else:
            i += 1
    return "".join(out)


def ts_violations(text, vocab):
    lines = text.split("\n")
    code = blank_block_comments(text).split("\n")
    return marked_violations(lines, lambda i, _line: ts_hits(code[i], vocab))


# --- docs and fixtures --------------------------------------------------------

def doc_hits(line, vocab):
    found = []
    for m in DOC_SPAN.finditer(line):
        span = (m.group(1) if m.group(1) is not None else m.group(2)).strip().strip('"')
        if span not in vocab.verbs and vocab.judge(span, strict=False):
            found.append(span)
    return found + fixture_hits(line, vocab)


def fixture_hits(line, vocab):
    return [a for a in JSON_ACTION.findall(line) if vocab.judge(a, strict=False)]


def doc_violations(text, vocab):
    return marked_violations(text.split("\n"), lambda _i, line: doc_hits(line, vocab))


def fixture_violations(text, vocab):
    return marked_violations(text.split("\n"), lambda _i, line: fixture_hits(line, vocab))


# --- markers ------------------------------------------------------------------

def has_allow(line):
    return ALLOW in line and ALLOW_START not in line and ALLOW_END not in line


def marked_violations(lines, hits_at):
    """Every hit `hits_at(i, line)` names, less the ones a marker covers: a
    marker on the line covers one hit there; a marker-only line covers one hit
    on the line below; a start/end pair covers every line between."""
    out = []
    in_block = False
    for i, line in enumerate(lines):
        if ALLOW_START in line:
            in_block = True
        if ALLOW_END in line:
            in_block = False
            continue
        if in_block:
            continue
        hits = hits_at(i, line)
        above = i > 0 and has_allow(lines[i - 1]) and not hits_at(i - 1, lines[i - 1])
        allowed = int(has_allow(line)) + int(above and not has_allow(line))
        out.extend((i + 1, hit) for hit in hits[allowed:])
    return out


# --- the scan -----------------------------------------------------------------

def tracked_files(root):
    out = subprocess.run(["git", "ls-files", "-z"], cwd=root, capture_output=True, check=True)
    return [p for p in out.stdout.decode().split("\0") if p]


def checker_for(rel):
    """The violations function for a tracked path, or None if it is not
    scanned."""
    if rel in VOCAB_SOURCES or rel.startswith(GENERATED_TS) or rel.startswith(ARCHIVES):
        return None
    ext = rel.rsplit(".", 1)[-1] if "." in os.path.basename(rel) else ""
    if ext == "rs" and rel.startswith(RUST_ROOTS):
        return lambda text, v: rust_violations(text, v, all_test=is_rust_test_file(rel))
    if ext in ("ts", "tsx", "js", "mjs", "cjs") and rel.startswith(TSJS_ROOTS):
        return ts_violations
    if ext in ("json", "jsonl"):
        return fixture_violations
    if ext == "md" or (ext == "html" and rel.startswith("docs/")):
        return doc_violations
    if rel.startswith(("skills/", "templates/")) and ext in ("txt", "html", "py", "sh", "toml", "yml", "yaml"):
        return doc_violations
    return None


def scan(root, vocab):
    found = []
    for rel in tracked_files(root):
        check = checker_for(rel)
        if check is None or not os.path.isfile(os.path.join(root, rel)):
            continue
        try:
            text = read(root, rel)
        except UnicodeDecodeError:
            continue
        for line_no, lit in check(text, vocab):
            found.append(f"{rel}:{line_no}: \"{lit}\" ({vocab.judge(lit, True) or 'builds or tests an action'})")
    return found


# --- self-test ----------------------------------------------------------------

SELF_TEST_VOCAB = Vocab(
    current={"dispatch.start", "mission.run.finalize", "hook.fired"},
    old={"dispatch start", "mission.run.start", "tier-decision", "mission abort"},
    scopes={"dispatch", "mission", "hook"},
    others={"dispatch.map", "dispatch.start", "tier-decision"},
    verbs={"mission abort"},
)

RUST_CASES = [
    ('let a = "dispatch.start";', 1),
    ('emit("dispatch start", x)', 1),
    ('let a = r#"dispatch.start"#;', 1),
    ('let a = format!("mission.run.{}", verb);', 1),
    ('if a.starts_with("dispatch ") {}', 1),
    ('if a.starts_with("dispatch.") {}', 1),
    ('let a = concat!("dispatch", ".start");', 1),
    ('let j = "{\\"action\\":\\"dispatch start\\"}";', 1),
    ('let j = r#"{"action":"dispatch.start","x":1}"#;', 1),
    ('let a = "dispatch.started";', 1),
    ('// "dispatch.start" in prose', 0),
    ('let k = "dispatch.map";', 0),
    ('let k = "tier-decision";', 0),
    ('let k = "dispatch.rs";', 0),
    ('let k = "hook.fired.payload.error";', 0),
    ('let k = format!("step {} of {}", a, b);', 0),
    ('// flow-action-guard:allow — trajectory event\n"dispatch.start" => x', 0),
    ('"dispatch.start" => x, // flow-action-guard:allow — trajectory event', 0),
    ('// flow-action-guard:allow — one only\nf("dispatch.start", "dispatch start")', 1),
    ('f("dispatch.start") // flow-action-guard:allow — this line\ng("dispatch start")', 1),
    ('// flow-action-guard:allow-start — a block\nf("dispatch start");\ng("dispatch start");\n// flow-action-guard:allow-end\nh("dispatch start");', 1),
    # A test module: a current spelling is fine, an old or made-up one is not.
    ('fn f() {}\n#[cfg(test)]\nmod tests {\n    let a = "dispatch.start";\n}', 0),
    ('#[cfg(test)]\nmod tests {\n    let a = "dispatch start";\n}', 1),
    ('#[cfg(test)]\nmod tests {\n    let a = "dispatch.started";\n}', 1),
    ('#[cfg(test)]\nmod tests {\n    assert_eq!(v["action"], "mission.run.start");\n}', 1),
    ('#[cfg(test)]\nmod tests {\n    let j = json!({"action": "dispatch.start"});\n}', 0),
    ('#[cfg(test)]\nmod tests {\n    let a = format!("mission.run.{}", v);\n}', 0),
    ('#[cfg(test)]\nmod tests {\n    fn t() { let a = 1; }\n}\nfn after() { let a = "dispatch.start"; }', 1),
    ('#[cfg(test)]\nmod tests {\n    fn t() { let a = "{"; let b = \'{\'; }\n}\nfn after() { let a = "dispatch.start"; }', 1),
]

TS_CASES = [
    ('expect(rec.action).toBe("dispatch start");', 1),
    ('const r = { action: "mission.run.start" };', 1),
    ('const a = "dispatch.started";', 1),
    ("const a = 'dispatch.started';", 1),
    ("const a = `dispatch.started`;", 1),
    ('const a = "dispatch.start";', 0),
    ('const label = "dispatch start";', 0),
    ('expect(r.action).toBe("mission abort");', 1),
    ('// action "dispatch start" in a comment', 0),
    (' * the retired `mission.run.start`', 0),
    ('const j = \'{"action":"dispatch start"}\';', 1),
    ('raw("dispatch start") // flow-action-guard:allow — the lenient reader', 0),
    ('{/* lights the CURRENT\n    `thermal.state` here */}', 0),
    ('const g = "src/**/*.ts";\nconst a = "dispatch.started";', 1),
]

DOC_CASES = [
    ('The record `dispatch start` opens a run.', 1),
    ('The record <code>dispatch.started</code> opens a run.', 1),
    ('The record `dispatch.start` opens a run.', 0),
    ('A dispatch start opens a run.', 0),
    ('`"action": "mission.run.start"`', 1),
    ('<!-- flow-action-guard:allow — history -->\nwas `dispatch start`', 0),
    ('Run `mission abort` to stop it.', 0),
]

FIXTURE_CASES = [
    ('{"ts":"t","action":"dispatch start"}', 1),
    ('{"ts":"t","action":"dispatch.started"}', 1),
    ('{"ts":"t","action":"dispatch.start"}', 0),
    ('{"ts":"t","kind":"dispatch start"}', 0),
]


def self_test():
    v = SELF_TEST_VOCAB
    suites = [
        (RUST_CASES, lambda s: rust_violations(s, v)),
        (TS_CASES, lambda s: ts_violations(s, v)),
        (DOC_CASES, lambda s: doc_violations(s, v)),
        (FIXTURE_CASES, lambda s: fixture_violations(s, v)),
    ]
    for cases, check in suites:
        for src, want in cases:
            got = len(check(src))
            assert got == want, f"{src!r}: {got} hits, want {want}"
    assert rust_violations('let a = "dispatch start";', v, all_test=True), "a test file still flags an old spelling"
    assert not rust_violations('let a = "dispatch.start";', v, all_test=True), "a test file may name a current action"
    for rel, want in [("docs/demo/x.jsonl", None), ("tests/parity/corpus/a.jsonl", None),
                      ("crates/darkmux-flow/src/legacy.rs", None), ("ui/src/types/generated/FlowAction.ts", None),
                      ("tests/fixtures/a.jsonl", fixture_violations), ("ui/src/lib/flow.ts", ts_violations),
                      ("skills/x/SKILL.md", doc_violations), ("docs/guide/a.html", doc_violations)]:
        assert checker_for(rel) is want, f"{rel}: wrong checker"
    assert checker_for("crates/darkmux-flow/src/lib.rs") is not None
    print("flow-action-guard self-test passed")


def main():
    if "--self-test" in sys.argv:
        self_test()
        return
    vocab = vocabulary(ROOT)
    found = scan(ROOT, vocab)
    if found:
        print("flow-action-guard: strings that are, or look like, flow actions outside the vocabulary")
        print("(build or match FlowAction in Rust; spell a current action elsewhere; mark a deliberate old spelling):")
        for f in found:
            print(f"  {f}")
        print(f"{len(found)} hit(s)")
        sys.exit(1)
    print(f"flow-action-guard passed ({len(vocab.written)} spellings, {len(vocab.others)} other dotted ids)")


if __name__ == "__main__":
    main()
