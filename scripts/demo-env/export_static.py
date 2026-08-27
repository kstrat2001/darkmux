#!/usr/bin/env python3
"""Capture the demo world's daemon-only routes into committed static fixtures.

`docs/demo` is `next.html` with no daemon behind it. Most of what it shows is
derived from committed flow records, but two surfaces are answered by a HOST
PROBE and cannot be: the console's `/panel/:id` and the machine lens's
`/machine/*`. Before this, the console fetched `/panel/:id` anyway, GitHub
Pages answered, and the pane rendered a `<!DOCTYPE html>` 404 page as command
output; the machine lens sat at `loading…` forever.

This reads them from a RUNNING `serve.py` and writes what it got:

    ./serve.py &            # the demo world
    ./export_static.py      # -> docs/demo/demo-panels.json, demo-machine.json

Capturing from the demo world rather than a real daemon is the point. That
world is fictional by construction (a 256 GB M5 Ultra that exists nowhere) and
`serve.py` scrubs its passthrough, so nothing here can carry a real hostname,
tailnet address or workspace path onto a public page.

Re-run after changing `world.json` or after a panel's output format moves.
"""
import argparse, json, pathlib, sys, urllib.error, urllib.request

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent.parent
OUT = ROOT / "docs" / "demo"

# The daemon's own allowlist (`crates/darkmux-serve/src/panel.rs::PANEL_IDS`).
# Duplicated deliberately and asserted below: if the allowlist grows, this
# script should FAIL rather than silently publish a demo missing a tab.
PANEL_IDS = ["mission-status", "role-list", "machine-status", "config-list",
             "flow-status", "lab-fixture-list", "run-list", "doctor"]

# The width the console's own default lands near. A fixture is captured at ONE
# width — `fetchPanel` ignores `cols` on the static path rather than pretend
# otherwise, because re-wrapping fixed-width ANSI client-side would corrupt it.
COLS = 120


def get(base, path, headers=None):
    req = urllib.request.Request(base + path)
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://127.0.0.1:8799",
                    help="a running serve.py (default: %(default)s)")
    a = ap.parse_args()

    # Fail loudly if the allowlist moved out from under this list.
    spec = (ROOT / "crates" / "darkmux-serve" / "src" / "panel.rs").read_text()
    declared = {ln.strip().strip('",') for ln in spec.split("PANEL_IDS: &[&str] = &[", 1)[-1]
                .split("];", 1)[0].splitlines() if ln.strip().startswith('"')}
    if declared and declared != set(PANEL_IDS):
        sys.exit(f"panel allowlist drifted — panel.rs has {sorted(declared)}, this script has "
                 f"{sorted(PANEL_IDS)}. Update PANEL_IDS (and re-export) rather than shipping a "
                 f"demo whose console is missing a tab.")

    try:
        panels = {}
        for pid in PANEL_IDS:
            panels[pid] = get(a.base, f"/panel/{pid}?cols={COLS}",
                              {"X-Darkmux-Panel": "1", "accept": "application/json"})
        machine = {"specs": get(a.base, "/machine/specs"),
                   "resources": get(a.base, "/machine/resources")}
    except urllib.error.URLError as e:
        sys.exit(f"cannot reach {a.base} ({e}). Start the demo world first: ./serve.py")

    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "demo-panels.json").write_text(json.dumps(panels, indent=2, sort_keys=True) + "\n")
    (OUT / "demo-machine.json").write_text(json.dumps(machine, indent=2, sort_keys=True) + "\n")

    print(f"wrote {OUT / 'demo-panels.json'} ({len(panels)} panels)")
    for pid in PANEL_IDS:
        n = len(panels[pid].get("ansi_text", ""))
        flag = "  ⚠ empty-ish" if n < 150 else ""
        print(f"    {pid:<20} {n:>6} chars{flag}")
    print(f"wrote {OUT / 'demo-machine.json'} "
          f"({machine['specs'].get('machine_id')}, {machine['specs'].get('cpu_brand')})")


if __name__ == "__main__":
    main()
