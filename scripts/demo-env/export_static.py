#!/usr/bin/env python3
"""Capture the demo world's daemon-only routes into committed static fixtures.

`docs/demo` is `next.html` with no daemon behind it. Most of what it shows is
derived from committed flow records, but several surfaces are answered by
something a static file can't be: the console's `/panel/:id`, the machine
lens's `/machine/*`, and (#2032 packet 2) the mission-graph lens's
`GET /mission/:id/graph.json`. Before the panel/machine capture, the console
fetched `/panel/:id` anyway, GitHub Pages answered, and the pane rendered a
`<!DOCTYPE html>` 404 page as command output; the machine lens sat at
`loading…` forever. Before this packet, the mission-graph lens's static gate
(`MissionGraphLens.tsx`'s `daemonBacked`) disabled itself unconditionally on
any static build — there was no fixture to read a graph FROM, only a "needs a
running daemon" notice, on every mission, always.

This reads them from a RUNNING `serve.py` and writes what it got:

    ./serve.py &            # the demo world
    ./export_static.py      # -> docs/demo/demo-panels.json, demo-machine.json,
                             #    demo-missions.json, demo-phases.json,
                             #    demo-graphs.json, demo-flow.jsonl,
                             #    demo-runs.json, demo-lab-runs.json

Capturing from the demo world rather than a real daemon is the point. That
world is fictional by construction (a 256 GB M5 Ultra that exists nowhere) and
`serve.py` scrubs its passthrough, so nothing here can carry a real hostname,
tailnet address or workspace path onto a public page. `/missions`, `/phases`
and `/mission/:id/graph.json` are all in `serve.py`'s PASSTHROUGH set (its own
module doc: "forwards `/`, `/flow/*`, `/runs`, `/missions`, `/fleet/*`, the
mission graph ... untouched") — real responses from the real daemon serve.py
spawns against the isolated demo home, not a second hand-authored fixture
format to keep in sync by hand.

`demo-missions.json`/`demo-phases.json` were, before this packet, hand-authored
fixtures with no capture script behind them at all. Re-exporting them from the
daemon's own `/missions`/`/phases` handlers keeps their SHAPE identical
(`{"missions": [...], "generated_at_ms": ...}` /
`{"phases": [...], "generated_at_ms": ...}` — see `missions_handler`/
`phases_handler` in `crates/darkmux-serve/src/lib.rs`) while making the content
real: whatever missions/phases actually exist in the demo world's isolated
`DARKMUX_HOME`, including any real mission packet 1 (#2032) imports, rather
than four illustrative rows nobody re-derives from `world.json`.

`demo-graphs.json` is NEW (#2032 packet 2): a map from mission id to that
mission's `GET /mission/:id/graph.json` payload, `{"<mission-id>": <graph>,
...}` — one fixture that can carry every mission the demo world knows about,
captured by walking `/missions`'s own id list rather than a second hardcoded
one. `ui/src/lib/staticSource.ts::staticGraphsSrc()` is the ONE static-build
reader; `MissionGraphLens.tsx` looks its routed mission up in the map instead
of fetching `/mission/:id/graph.json` (which no daemon behind `docs/demo`
could ever answer).

`demo-flow.jsonl`/`demo-runs.json`/`demo-lab-runs.json` (#2032 packet 3) were,
before this packet, hand-authored fixtures with no capture script behind them
either — `demo-flow.jsonl` was frozen at a single day in 2026-05, so every
published demo aged in place: the "last active" reading grew stale by exactly
however long it had been since a human last remembered to hand-edit the file,
and the one `running` row in `demo-runs.json` stayed `running` forever,
because nothing ever re-derived it from `build.py`'s actual `live=True`
replay. Re-exporting from the demo world's own daemon fixes both: `/flow-days`
+ `/flow/<date>` (passthrough — see `serve.py`'s own module doc) give the
REAL days the current build wrote, so the committed `.jsonl` always covers
whatever `build.py --now` (default: real now) just anchored the world to; and
`/runs` gives the REAL run list, where exactly the ONE replay `build.py`
marked `live=True` reads as running, because that is the only session missing
its terminal `dispatch complete`/`dispatch error` record (the same bookend
contract 2 states). `demo-lab-runs.json` mirrors `/lab/runs` for the same
"stop hand-authoring what a real route already answers" reason; the demo
world configures no lab dir, so it is expected to keep coming back
`configured: false` — captured rather than assumed, so a future world that DID
configure one would change this file the same way it changes every other
fixture, with no second edit required.

Re-run after changing `world.json` or after a panel's output format moves —
or simply run `./build.py && ./serve.py & ./export_static.py` any time before
a release, since every fixture's "how long ago" reads relative to the moment
`build.py` ran, not to when the fixture was last hand-touched.
"""
import argparse, json, pathlib, re, sys, urllib.error, urllib.request

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


def get_optional(base, path, headers=None):
    """Like `get`, but a non-2xx response is a WARNING, not a fatal exit.

    Used only for the per-mission graph capture below: mission ids come from
    `/missions`'s own listing, so a 404 here would mean the daemon's two
    routes disagree about which missions exist — worth a loud stderr line,
    but not worth aborting the whole export over one mission's graph when
    every other fixture is fine. Returns `None` on any HTTP error.
    """
    try:
        return get(base, path, headers)
    except urllib.error.HTTPError as e:
        print(f"  WARNING: {path} -> HTTP {e.code}, skipping this mission's graph", file=sys.stderr)
        return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="http://127.0.0.1:8799",
                    help="a running serve.py (default: %(default)s)")
    a = ap.parse_args()

    # Fail loudly if the allowlist moved out from under this list.
    # An EMPTY parse is a failure, not a pass. The first cut guarded with
    # `if declared and declared != set(PANEL_IDS)`, so reformatting the Rust
    # declaration (wrapping `PANEL_IDS: &[&str] = &[` onto two lines) made the
    # split find nothing, `declared` empty, and the guard silently agree that
    # a renamed panel was fine. A drift guard that cannot see the thing it
    # guards must SAY SO, not shrug. Found by a QA agent that reformatted the
    # declaration rather than reading the parser.
    spec = (ROOT / "crates" / "darkmux-serve" / "src" / "panel.rs").read_text()
    m = re.search(r"PANEL_IDS\s*:\s*&\[&str\]\s*=\s*&\[(.*?)\]\s*;", spec, re.S)
    if not m:
        sys.exit("cannot find PANEL_IDS in crates/darkmux-serve/src/panel.rs. Refusing to "
                 "export a demo whose console tabs cannot be checked against the daemon's "
                 "real allowlist.")
    declared = set(re.findall(r'"([^"]+)"', m.group(1)))
    if not declared:
        sys.exit("PANEL_IDS parsed EMPTY. Refusing to export — see above.")
    if declared != set(PANEL_IDS):
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
        # (#2032 packet 2) `/missions` and `/phases` are real daemon routes
        # `serve.py` passes straight through to the isolated demo-world
        # daemon (see this script's own module doc) — re-exporting them
        # replaces the two hand-authored fixtures with the demo world's
        # ACTUAL missions/phases, in the identical wire shape
        # (`missions_handler`/`phases_handler`,
        # `crates/darkmux-serve/src/lib.rs`), so nothing downstream needs to
        # change to read them.
        missions_resp = get(a.base, "/missions")
        phases_resp = get(a.base, "/phases")
        # One graph per mission id `/missions` actually listed — never a
        # second hardcoded id list to drift against the first (the same
        # discipline `PANEL_IDS`'s drift guard above enforces for panels).
        graphs = {}
        for mission in missions_resp.get("missions", []):
            mid = mission.get("id")
            if not mid:
                continue
            g = get_optional(a.base, f"/mission/{mid}/graph.json")
            if g is not None:
                graphs[mid] = g
        # (#2032 packet 3) `/runs` and `/lab/runs` are both in `serve.py`'s
        # passthrough set too — real aggregation, same "capture, don't
        # hand-author" discipline as missions/phases/graphs above.
        runs_resp = get(a.base, "/runs")
        lab_runs_resp = get(a.base, "/lab/runs")
        # `/flow-days` names every day the current build actually wrote
        # (never a hardcoded date — a rebuild anchored to a different `--now`
        # or spanning a day boundary changes what this lists, and the export
        # follows without a second edit). `/flow/<date>` returns that day's
        # records as a bare JSON array (`flow_handler`,
        # `crates/darkmux-serve/src/lib.rs`); concatenating every day and
        # sorting by `ts` reproduces exactly what the daemon would serve
        # across the whole (short, demo-world) history, in the chronological
        # order `parseFlowJsonl`/`firstRecordDate` (`ui/src/lib/flow.ts`)
        # expect — the static reader derives the playback date from
        # `records[0].ts`, so an unsorted write would silently misdate the
        # demo.
        days_resp = get(a.base, "/flow-days")
        flow_records = []
        for day in days_resp.get("days", []):
            date = day.get("date") if isinstance(day, dict) else day
            if not date:
                continue
            flow_records.extend(get(a.base, f"/flow/{date}"))
        flow_records.sort(key=lambda r: r.get("ts") or "")
    except urllib.error.URLError as e:
        sys.exit(f"cannot reach {a.base} ({e}). Start the demo world first: ./serve.py")

    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "demo-panels.json").write_text(json.dumps(panels, indent=2, sort_keys=True) + "\n")
    (OUT / "demo-machine.json").write_text(json.dumps(machine, indent=2, sort_keys=True) + "\n")
    (OUT / "demo-missions.json").write_text(json.dumps(missions_resp, indent=2, sort_keys=True) + "\n")
    (OUT / "demo-phases.json").write_text(json.dumps(phases_resp, indent=2, sort_keys=True) + "\n")
    (OUT / "demo-graphs.json").write_text(json.dumps(graphs, indent=2, sort_keys=True) + "\n")
    (OUT / "demo-runs.json").write_text(json.dumps(runs_resp, indent=2, sort_keys=True) + "\n")
    (OUT / "demo-lab-runs.json").write_text(json.dumps(lab_runs_resp, indent=2, sort_keys=True) + "\n")
    # NOT `sort_keys`/indented JSON: this is JSONL, one compact record object
    # per line, matching exactly what a real `<date>.jsonl` day file on disk
    # looks like and what `parseFlowJsonl` (`ui/src/lib/flow.ts`) parses —
    # line-oriented, not a pretty-printed array.
    (OUT / "demo-flow.jsonl").write_text(
        "\n".join(json.dumps(r, sort_keys=True) for r in flow_records) + "\n")

    print(f"wrote {OUT / 'demo-panels.json'} ({len(panels)} panels)")
    for pid in PANEL_IDS:
        n = len(panels[pid].get("ansi_text", ""))
        flag = "  ⚠ empty-ish" if n < 150 else ""
        print(f"    {pid:<20} {n:>6} chars{flag}")
    print(f"wrote {OUT / 'demo-machine.json'} "
          f"({machine['specs'].get('machine_id')}, {machine['specs'].get('cpu_brand')})")
    print(f"wrote {OUT / 'demo-missions.json'} ({len(missions_resp.get('missions', []))} missions)")
    print(f"wrote {OUT / 'demo-phases.json'} ({len(phases_resp.get('phases', []))} phases)")
    print(f"wrote {OUT / 'demo-graphs.json'} ({len(graphs)} of "
          f"{len(missions_resp.get('missions', []))} missions' graphs captured)")
    print(f"wrote {OUT / 'demo-runs.json'} ({len(runs_resp.get('runs', []))} runs, "
          f"{sum(1 for r in runs_resp.get('runs', []) if r.get('status') == 'running')} running)")
    print(f"wrote {OUT / 'demo-lab-runs.json'} (configured={lab_runs_resp.get('configured')})")
    print(f"wrote {OUT / 'demo-flow.jsonl'} ({len(flow_records)} records across "
          f"{len(days_resp.get('days', []))} day(s))")


if __name__ == "__main__":
    main()
