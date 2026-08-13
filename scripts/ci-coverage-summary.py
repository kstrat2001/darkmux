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
"""
import json
import sys


def main(path: str = "cov.json", badge_path: str | None = None) -> int:
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


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    badge = None
    if "--badge" in sys.argv:
        i = sys.argv.index("--badge")
        badge = sys.argv[i + 1] if i + 1 < len(sys.argv) else "coverage-badge.json"
        args = [a for a in args if a != badge]
    sys.exit(main(args[0] if args else "cov.json", badge))
