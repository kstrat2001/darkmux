#!/usr/bin/env python3
"""Additive-only gate for darkmux's wire contracts (D2, #3035).

From 5.0 on, the daemon's HTTP routes and the `--json` shapes are semver
contracts. Three checked-in files pin them:

  tests/cli-json.golden                     what each verb prints under --json
  crates/darkmux-serve/route-table.golden   every route and its response fields
  ui/src/types/generated/*.ts               the generated twin of every wire type

A pull request may ADD to these files. It may not remove or change what is
already there unless it carries the `breaking-v6` label.

How a file is compared (against the merge-base with the PR's base ref):

  * the two goldens, line by line: a base line that is not in the head is
    reported as removed or changed (blank lines are ignored);
  * a generated .ts file, member by member: each field of an object type is
    one member (`Type.name: type`), each variant of a union is one member, and
    doc comments and whitespace are ignored. A removed field, a retyped field,
    a removed type and a deleted file all fail; a new field, a new type and a
    new file pass. A field of an inline nested object type is part of its
    parent field's type, so adding to it reads as a retype of the parent.

When it applies. The contract starts at 5.0, and this branch legitimately
rewrites these files before 5.0 ships. So the gate is a no-op, with a notice,
until a `v5.*` tag exists in the checkout; from then on it compares every PR
against its base. CI therefore checks out with full history and tags.

Labels come from `--labels a,b`, else the pull_request event payload
(`GITHUB_EVENT_PATH`), else `gh pr view`.

  scripts/contract-additive-guard.py [--base REF] [--labels a,b] [--force]
  scripts/contract-additive-guard.py --self-test
"""
import argparse
import io
import json
import os
import re
import subprocess
import sys
import tempfile
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LINE_FILES = ["tests/cli-json.golden", "crates/darkmux-serve/route-table.golden"]
TS_DIR = "ui/src/types/generated"
BREAKING_LABEL = "breaking-v6"
TAG_GLOB = "v5.*"


def git(root, *args, check=True):
    r = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)
    if check and r.returncode != 0:
        sys.exit(f"git {' '.join(args)} failed: {r.stderr.strip()}")
    return r


def show(root, ref, path):
    """The file at `ref`, or None when it did not exist there."""
    r = git(root, "show", f"{ref}:{path}", check=False)
    return r.stdout if r.returncode == 0 else None


def line_members(text):
    """(line number, member) for a golden file.

    An indented line is a field of the unindented header above it, and its
    member names that header, so a field moved to another type is a removal
    from the first. A header of the form `Name = A | B` is a union: each
    variant is its own member, an object variant `{ tag; field; ... }` is
    keyed by its first field, so adding a variant (or a field to one) is an
    addition and removing one is not. Any other line is one member.
    """
    out, header = [], ""
    for i, ln in enumerate(text.splitlines(), 1):
        if not ln.strip():
            continue
        if ln[0].isspace():
            out.append((i, f"{header} :: {norm(ln)}"))
            continue
        header = ln.split(" = ")[0].strip()
        if " = " not in ln:
            out.append((i, norm(ln)))
            continue
        for variant, _ in split_top(ln.split(" = ", 1)[1], "|"):
            v = variant.strip()
            if v.startswith("{") and v.endswith("}") and len(split_top(v, "|")) == 1 and ";" in v:
                fields = [norm(f) for f, _ in split_top(v[1:-1], ";") if f.strip()]
                out.extend((i, f"{header} | {fields[0]} :: {f}") for f in fields)
            else:
                out.append((i, f"{header} | {norm(v)}"))
    return out


def blank_comments(src):
    """`src` with every /* ... */ comment blanked, keeping each newline so line
    numbers stay those of the original file."""
    return re.sub(r"/\*.*?\*/", lambda m: re.sub(r"[^\n]", " ", m.group(0)), src, flags=re.S)


def split_top(text, sep, offset=0):
    """Split `text` on `sep` at nesting depth 0 outside quotes. Returns
    (piece, index of its first character) pairs."""
    parts, depth, quote, start = [], 0, None, 0
    for i, c in enumerate(text):
        if quote:
            quote = None if c == quote else quote
        elif c in "\"'":
            quote = c
        elif c in "{([<":
            depth += 1
        elif c in "})]>":
            depth -= 1
        elif c == sep and depth == 0:
            parts.append((text[start:i], offset + start))
            start = i + 1
    parts.append((text[start:], offset + start))
    return parts


def norm(s):
    return " ".join(s.split())


def ts_members(src):
    """{member: line number} for one generated .ts file."""
    text = blank_comments(src)
    m = re.search(r"export type (\w+)\s*(?:<[^=]*>)?\s*=", text)
    if not m:
        return {}
    name = m.group(1)
    body_start = m.end()
    body = text[body_start:].rstrip().rstrip(";")

    def line_of(idx):
        return text.count("\n", 0, idx) + 1

    def obj_members(obj, offset):
        inner = obj.strip()[1:-1]
        lead = len(obj) - len(obj.lstrip()) + 1
        return [
            (norm(p), offset + lead + i + len(p) - len(p.lstrip()))
            for p, i in split_top(inner, ",")
            if p.strip()
        ]

    out = {}
    stripped = body.strip()
    base = body_start + (len(body) - len(body.lstrip()))
    if stripped.startswith("{") and stripped.endswith("}") and len(split_top(stripped, "|")) == 1:
        for member, idx in obj_members(stripped, base):
            out[f"{name}.{member}"] = line_of(idx)
        return out
    for piece, idx in split_top(stripped, "|"):
        piece_s = piece.strip()
        if not piece_s:
            continue
        if piece_s.startswith("{") and piece_s.endswith("}"):
            # An object variant: its members are keyed under its first member
            # (the tag), so adding a field to one variant is an addition.
            members = obj_members(piece_s, base + idx + (len(piece) - len(piece.lstrip())))
            tag = members[0][0] if members else ""
            for member, midx in members:
                out[f"{name} | {tag} :: {member}"] = line_of(midx)
        else:
            out[f"{name} | {norm(piece_s)}"] = line_of(base + idx)
    return out


def ts_files(root, ref):
    r = git(root, "ls-tree", "-r", "--name-only", ref, "--", TS_DIR, check=False)
    return [p for p in r.stdout.splitlines() if p.endswith(".ts")]


def head_text(root, path):
    p = Path(root) / path
    return p.read_text() if p.is_file() else None


def violations(root, base):
    """Everything the base had that the working tree no longer has."""
    found = []
    for path in LINE_FILES:
        old = show(root, base, path)
        if old is None:
            continue
        new = head_text(root, path)
        if new is None:
            found.append(f"{path}: the file was deleted")
            continue
        remaining = {}
        for _, ln in line_members(new):
            remaining[ln] = remaining.get(ln, 0) + 1
        for num, ln in line_members(old):
            if remaining.get(ln, 0) > 0:
                remaining[ln] -= 1
            else:
                found.append(f"{path}:{num}: removed or changed: {ln}")
    for path in ts_files(root, base):
        old = ts_members(show(root, base, path))
        new_src = head_text(root, path)
        if new_src is None:
            found.append(f"{path}: the file was deleted")
            continue
        new = ts_members(new_src)
        for member, num in old.items():
            if member not in new:
                found.append(f"{path}:{num}: removed or changed: {member}")
    return found


def gate_active(root):
    return bool(git(root, "tag", "-l", TAG_GLOB, check=False).stdout.split())


def labels_from_env():
    path = os.environ.get("GITHUB_EVENT_PATH")
    if path and Path(path).is_file():
        event = json.loads(Path(path).read_text())
        pr = event.get("pull_request")
        if pr is not None:
            return [lbl["name"] for lbl in pr.get("labels", [])]
    r = subprocess.run(["gh", "pr", "view", "--json", "labels", "-q", ".labels[].name"], capture_output=True, text=True)
    return r.stdout.split() if r.returncode == 0 else []


def default_base(root):
    ref = os.environ.get("GITHUB_BASE_REF") or "main"
    return git(root, "merge-base", "HEAD", f"origin/{ref}").stdout.strip()


def run(root, base, labels, force):
    if not force and not gate_active(root):
        print(f"contract-additive-guard: no {TAG_GLOB} tag in this checkout, so the 5.0 contract has not started; skipping.")
        return 0
    found = violations(root, base)
    if not found:
        print("contract-additive-guard: contract files only grew.")
        return 0
    if BREAKING_LABEL in labels:
        print(f"contract-additive-guard: {len(found)} removal(s) allowed by the `{BREAKING_LABEL}` label:")
        print("\n".join(f"  {f}" for f in found))
        return 0
    print("contract-additive-guard: a wire contract may only grow. Removed or changed:", file=sys.stderr)
    print("\n".join(f"  {f}" for f in found), file=sys.stderr)
    print(f"Add the `{BREAKING_LABEL}` label to the PR if the break is intended.", file=sys.stderr)
    return 1


# ---- self-test -------------------------------------------------------------

BASE_FILES = {
    "tests/cli-json.golden": (
        "# verbs\nflow status  FlowStatus\nmachine list  FleetView\n\n# types\n"
        'Mode = "a" | "b"\nTypeA\n  f: string\nTypeB\n  g: string\n'
    ),
    "crates/darkmux-serve/route-table.golden": "GET /health  json HealthResponse\n\n# response types\nHealthResponse.build: string\n",
    f"{TS_DIR}/HealthResponse.ts": (
        "// generated\nimport type { A } from \"./A\";\n/** doc */\n"
        "export type HealthResponse = { build: string, \n/** why */\nn: number | null, };\n"
    ),
    f"{TS_DIR}/Generic.ts": "export type Generic<T> = { a: T, b: string };\n",
    f"{TS_DIR}/Kind.ts": 'export type Kind = { "k": "a", x: number } | { "k": "b" };\n',
}


def _repo(tmp, head_files, tag=True):
    root = Path(tmp)
    git(root, "init", "-q")
    git(root, "config", "user.email", "t@example.com")
    git(root, "config", "user.name", "t")
    for rel, text in BASE_FILES.items():
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_text(text)
    git(root, "add", "-A")
    git(root, "commit", "-q", "-m", "base")
    base = git(root, "rev-parse", "HEAD").stdout.strip()
    if tag:
        git(root, "tag", "v5.0.0")
    for rel, text in head_files.items():
        p = root / rel
        if text is None:
            p.unlink()
        else:
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)
    return root, base


def self_test():
    ts = f"{TS_DIR}/HealthResponse.ts"
    cases = [
        ("an addition passes", {
            "tests/cli-json.golden": BASE_FILES["tests/cli-json.golden"] + "doctor  DoctorReport\n",
            "crates/darkmux-serve/route-table.golden": BASE_FILES["crates/darkmux-serve/route-table.golden"] + "HealthResponse.extra: boolean\n",
            ts: BASE_FILES[ts].replace("n: number | null,", "n: number | null, extra?: string,"),
            f"{TS_DIR}/Brand.ts": "export type Brand = { a: number };\n",
            f"{TS_DIR}/Kind.ts": 'export type Kind = { "k": "a", x: number, y: string } | { "k": "b" } | { "k": "c" };\n',
        }, [], 0),
        ("a doc-comment edit passes", {ts: BASE_FILES[ts].replace("why", "a better why")}, [], 0),
        ("a removed route-golden line fails", {
            "crates/darkmux-serve/route-table.golden": "GET /health  json HealthResponse\n\n# response types\n",
        }, [], 1),
        ("a field moved from one cli-json type to another fails", {
            "tests/cli-json.golden": BASE_FILES["tests/cli-json.golden"].replace("TypeA\n  f: string\nTypeB\n  g: string\n", "TypeA\nTypeB\n  g: string\n  f: string\n"),
        }, [], 1),
        ("an added cli-json enum variant passes", {
            "tests/cli-json.golden": BASE_FILES["tests/cli-json.golden"].replace('"a" | "b"', '"a" | "b" | "c"'),
        }, [], 0),
        ("a removed cli-json enum variant fails", {
            "tests/cli-json.golden": BASE_FILES["tests/cli-json.golden"].replace('"a" | "b"', '"a"'),
        }, [], 1),
        ("a removed field of a generic type fails", {f"{TS_DIR}/Generic.ts": "export type Generic<T> = { b: string };\n"}, [], 1),
        ("a retyped cli-json line fails", {
            "tests/cli-json.golden": BASE_FILES["tests/cli-json.golden"].replace("FlowStatus", "FlowStatusV2"),
        }, [], 1),
        ("a retyped generated field fails", {ts: BASE_FILES[ts].replace("n: number | null", "n: number")}, [], 1),
        ("a removed generated field fails", {ts: BASE_FILES[ts].replace("build: string, ", "")}, [], 1),
        ("a removed union variant fails", {f"{TS_DIR}/Kind.ts": 'export type Kind = { "k": "a", x: number };\n'}, [], 1),
        ("a deleted generated file fails", {ts: None}, [], 1),
        ("a removal with the breaking-v6 label passes", {
            "crates/darkmux-serve/route-table.golden": "GET /health  json HealthResponse\n\n# response types\n",
        }, [BREAKING_LABEL], 0),
    ]
    failures = 0

    def check(name, got, want):
        nonlocal failures
        ok = got == want
        failures += 0 if ok else 1
        print(f"  {'ok  ' if ok else 'FAIL'} {name}" + ("" if ok else f" (got {got!r}, want {want!r})"))

    print("contract-additive-guard self-test")
    for name, head, labels, want in cases:
        with tempfile.TemporaryDirectory() as tmp:
            root, base = _repo(tmp, head)
            print(f"  [{name}]")
            out, err = io.StringIO(), io.StringIO()
            with redirect_stdout(out), redirect_stderr(err):
                code = run(root, base, labels, force=False)
            for ln in (out.getvalue() + err.getvalue()).splitlines():
                print(f"      {ln}")
            check(name, code, want)
    with tempfile.TemporaryDirectory() as tmp:
        root, base = _repo(tmp, {"tests/cli-json.golden": ""}, tag=False)
        print("  [before any v5.* tag the gate skips with a notice]")
        check("no v5.* tag: skipped", run(root, base, [], force=False), 0)
        with redirect_stderr(io.StringIO()):
            check("--force overrides the window", run(root, base, [], force=True), 1)
    print("self-test " + ("FAILED" if failures else "passed"))
    return 1 if failures else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--base", help="ref to compare against (default: merge-base with origin/$GITHUB_BASE_REF or origin/main)")
    ap.add_argument("--labels", help="comma-separated PR labels (default: the event payload, else gh)")
    ap.add_argument("--force", action="store_true", help="apply the gate even before a v5.* tag exists")
    ap.add_argument("--self-test", action="store_true")
    a = ap.parse_args()
    if a.self_test:
        sys.exit(self_test())
    labels = a.labels.split(",") if a.labels is not None else labels_from_env()
    sys.exit(run(ROOT, a.base or default_base(ROOT), labels, a.force))


if __name__ == "__main__":
    main()
