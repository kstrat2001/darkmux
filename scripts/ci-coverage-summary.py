#!/usr/bin/env python3
"""Turn `cargo llvm-cov --json` into a GitHub step summary. (#1635)

Lives in the repo rather than inline in the workflow for two reasons: an
indented heredoc inside a YAML `run: |` block does not terminate (YAML strips
the common indent, so the closing delimiter keeps its leading spaces and never
matches), and a script in the tree can be run locally before it is trusted in
CI.

The headline percentages are the least interesting output. The number that
matters is the ZERO-coverage file list: three defects that shipped this week
(#1618's graph layout, #1631's unwalked XSS drill-downs, the ghost-runs half of
#1621) lived in regions the suite never executed at all. A bug in a file with no
executed lines cannot be caught by any amount of care in review.

`--badge <path>` additionally writes a shields.io endpoint JSON so the number is
visible on the README. The operator's call (2026-08-13), over the objection
recorded above: a number anyone can see is one the team is accountable for,
where an invisible one is only accountable to whoever remembers to look. The
badge carries the line percentage ONLY — it answers "is this tested?" for
someone passing by. The zero-coverage list above stays here, in CI, where it is
the dev team's work queue rather than front-page furniture.

`--functions <full.json>` reads the PER-FUNCTION export (`cargo llvm-cov report
--json`, no `--summary-only`) and names the functions no test ever executed:
the count, and the files holding the most of them. A file can have executed
lines and still hold a dozen functions nothing calls, which the zero-file list
cannot show. `--title` names the section, so one job can report several
measured trees (the workspace and `runtime/`, which is its own Cargo workspace).
"""
import json
import os
import sys

TOP_FILES = 15


def load_payload(path: str):
    """The first `data` entry of an llvm-cov export, or an error string."""
    try:
        with open(path) as fh:
            data = json.load(fh)
    except (OSError, json.JSONDecodeError) as exc:
        return None, f"could not read `{path}`: {exc}"
    try:
        payload = data["data"][0]
        payload["totals"]
    except (KeyError, IndexError, TypeError):
        return None, f"unexpected llvm-cov JSON shape in `{path}`"
    return payload, None


def relative(path: str) -> str:
    """A path relative to the checkout (the CWD) when it lies inside it."""
    root = os.getcwd().rstrip("/") + "/"
    return path[len(root):] if path.startswith(root) else path


def never_executed(functions: list) -> dict:
    """Source functions no test executed, as {file: count}.

    llvm-cov emits one record per generic INSTANTIATION, all sharing the
    source location of their first region. A source function counts as
    executed when any of its instantiations ran, which is how llvm-cov's own
    `totals.functions` counts it, so records are grouped by (file, line, col)
    first.
    """
    ran: dict = {}
    for f in functions:
        regions = f.get("regions") or []
        files = f.get("filenames") or []
        if not regions or not files:
            continue
        key = (files[0], regions[0][0], regions[0][1])
        ran[key] = ran.get(key, False) or f.get("count", 0) > 0
    per_file: dict = {}
    for (fname, _line, _col), executed in ran.items():
        if not executed:
            per_file[fname] = per_file.get(fname, 0) + 1
    return per_file


def functions_section(path: str) -> list:
    payload, err = load_payload(path)
    if err:
        return ["### Never-executed functions", "", err]
    per_file = never_executed(payload.get("functions", []))
    total = sum(per_file.values())
    out = ["### Never-executed functions", ""]
    out.append(f"**{total} function(s) no test executed**, in {len(per_file)} file(s).")
    # A derivation that disagrees with llvm-cov's own count is itself a
    # finding: say so rather than print a number nobody can reconcile.
    fn_totals = payload["totals"].get("functions") or {}
    if fn_totals and fn_totals["count"] - fn_totals["covered"] != total:
        out.append(
            f"(llvm-cov's own total says {fn_totals['count'] - fn_totals['covered']}; "
            "the per-function export and the summary disagree.)"
        )
    if per_file:
        out += ["", "| file | never executed |", "|---|---|"]
        ranked = sorted(per_file.items(), key=lambda kv: (-kv[1], kv[0]))
        out += [f"| `{relative(f)}` | {n} |" for f, n in ranked[:TOP_FILES]]
        if len(ranked) > TOP_FILES:
            out.append(f"| …and {len(ranked) - TOP_FILES} more files | |")
    return out


def main(path: str = "cov.json", badge_path: str | None = None,
         functions_path: str | None = None, title: str = "Coverage") -> int:
    # Never fail the job over a report: the measurement step owns the verdict.
    payload, err = load_payload(path)
    if err:
        print(f"## {title}\n\n{err}")
        return 0
    totals = payload["totals"]

    out = [f"## {title}", "", "| metric | covered | total | % |", "|---|---|---|---|"]
    for key in ("lines", "functions", "regions"):
        t = totals.get(key)
        if not t:
            continue
        out.append(f"| {key} | {t['covered']} | {t['count']} | {t['percent']:.1f}% |")
    out.append("")

    zero = [
        f["filename"]
        for f in payload.get("files", [])
        if f.get("summary", {}).get("lines", {}).get("count")
        and not f["summary"]["lines"]["covered"]
    ]
    if zero:
        out.append(f"**{len(zero)} file(s) with ZERO executed lines.** A bug in any of these")
        out.append("cannot be caught by the suite at all — no assertion reaches them.")
        out.append("")
        out += [f"- `{p}`" for p in sorted(zero)[:40]]
        if len(zero) > 40:
            out.append(f"- …and {len(zero) - 40} more")
    else:
        out.append("Every file with measurable lines has at least one executed.")

    if functions_path:
        out += [""] + functions_section(functions_path)

    print("\n".join(out))

    if badge_path:
        lines_t = totals.get("lines") or {}
        pct = lines_t.get("percent")
        if pct is None:
            return 0
        # Two facts, not one: the percentage answers "how much runs", the
        # zero-file count answers "how much is unreachable by any test" — and
        # the second is the one that predicted real defects here.
        # The percentage ONLY. The badge answers one question for a passing
        # visitor — "is this tested?" — and a number is the whole answer. The
        # zero-coverage list printed above is the DEV TEAM's work queue; it
        # belongs in CI where it is actionable, not on the front page where it
        # is a backlog item shown to strangers who did not ask.
        msg = f"{pct:.0f}%"
        # Bands are deliberately wide. A badge that changes colour on a 1%
        # move invites chasing the colour instead of the gap.
        colour = "brightgreen" if pct >= 80 else "green" if pct >= 70 else "yellow" if pct >= 55 else "orange"
        with open(badge_path, "w") as fh:
            json.dump({"schemaVersion": 1, "label": "coverage",
                       "message": msg, "color": colour}, fh)

    return 0


def self_test() -> int:
    """Prove the per-function derivation against hand-built llvm-cov records
    (the shape `cargo llvm-cov report --json` emits: one record per generic
    instantiation, sharing its first region's line and column)."""
    import tempfile

    def rec(fname, line, count, col=1):
        return {"name": f"f{line}", "count": count, "filenames": [fname],
                "regions": [[line, col, line, 9, count, 0, 0, 0]]}

    root = os.getcwd().rstrip("/")
    a, b = f"{root}/src/a.rs", "/elsewhere/b.rs"
    fns = [
        rec(a, 1, 3), rec(a, 1, 0),  # a generic: one instantiation ran
        rec(a, 5, 0), rec(a, 5, 0),  # a generic no instantiation ran
        rec(a, 9, 0),                # a plain function never run
        rec(a, 9, 2, col=30),        # same line, another function: ran
        rec(b, 2, 0),
    ]
    per_file = never_executed(fns)
    assert per_file == {a: 2, b: 1}, per_file
    assert relative(a) == "src/a.rs" and relative(b) == b

    with tempfile.TemporaryDirectory() as d:
        full = os.path.join(d, "full.json")
        doc = {"data": [{"functions": fns, "files": [],
                         "totals": {"functions": {"count": 5, "covered": 2}}}]}
        with open(full, "w") as fh:
            json.dump(doc, fh)
        text = "\n".join(functions_section(full))
        assert "**3 function(s) no test executed**, in 2 file(s)." in text, text
        assert "| `src/a.rs` | 2 |" in text and "disagree" not in text, text
        doc["data"][0]["totals"]["functions"]["covered"] = 4
        with open(full, "w") as fh:
            json.dump(doc, fh)
        assert "llvm-cov's own total says 1" in "\n".join(functions_section(full))
        assert "could not read" in "\n".join(functions_section(os.path.join(d, "absent.json")))
    print("ci-coverage-summary self-test: ok")
    return 0


def parse_args(argv: list):
    """(path, {flag: value}) for `--badge`, `--functions`, `--title`."""
    opts: dict = {}
    rest = []
    i = 0
    while i < len(argv):
        a = argv[i]
        if a in ("--badge", "--functions", "--title"):
            if i + 1 >= len(argv):
                sys.exit(f"{a} needs a value")
            opts[a] = argv[i + 1]
            i += 2
            continue
        rest.append(a)
        i += 1
    return (rest[0] if rest else "cov.json"), opts


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        sys.exit(self_test())
    cov_path, opts = parse_args(sys.argv[1:])
    sys.exit(main(cov_path, opts.get("--badge"), opts.get("--functions"),
                  opts.get("--title", "Coverage")))
