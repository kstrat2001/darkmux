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
"""
import json
import sys


def main(path: str = "cov.json") -> int:
    try:
        with open(path) as fh:
            data = json.load(fh)
    except (OSError, json.JSONDecodeError) as exc:
        # Never fail the job over a report — the measurement already ran.
        print(f"## Coverage\n\ncould not read `{path}`: {exc}")
        return 0

    try:
        payload = data["data"][0]
        totals = payload["totals"]
    except (KeyError, IndexError, TypeError):
        print("## Coverage\n\nunexpected llvm-cov JSON shape — nothing to report")
        return 0

    out = ["## Coverage", "", "| metric | covered | total | % |", "|---|---|---|---|"]
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

    print("\n".join(out))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else "cov.json"))
