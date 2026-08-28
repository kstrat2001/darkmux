#!/usr/bin/env python3
"""Import a REAL dispatch session from a flow log into a committed demo fixture.

Records emitted by the real emitters are shape-correct by construction, which
is why the demo replays a real session instead of synthesizing one: a
hand-written record can encode a shape the daemon could never produce, and the
only symptom is a pane that renders nothing (the same failure `wire_fixtures.rs`
exists to stop for `/runs` and `graph.json`).

What this does NOT preserve is identity. Machine ids, host paths and absolute
timestamps are stripped here, once, so the committed fixture carries none of
them and `build.py` never has to remember to scrub. Timestamps are rewritten
relative to the session's own first record (`t_ms`), so `build.py` can anchor
the replay to any wall-clock instant.

  ./import_session.py ~/.darkmux/flows/2026-08-25.jsonl <session-id> \
      --out sessions/crawl-error-discard.jsonl

Re-run it to refresh a fixture after a record-vocabulary change.
"""
import argparse, json, sys, pathlib

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))

# `FORBIDDEN` + `scrub()` moved to `lib_identity_scrub.py` (#2032 Packet 1)
# so `import_mission.py` can reuse the SAME patterns instead of a second
# copy that drifts. Re-exported here so `from import_session import scrub,
# FORBIDDEN` (used by `build.py::canned_doctor`) keeps working unchanged.
from lib_identity_scrub import scrub, FORBIDDEN, scan_or_die, ts_to_ms  # noqa: F401

ROOT = pathlib.Path(__file__).resolve().parent.parent.parent

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("flow_file"); ap.add_argument("session_id")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()

    recs = []
    for line in pathlib.Path(a.flow_file).read_text().splitlines():
        if not line.strip():
            continue
        r = json.loads(line)
        if r.get("session_id") == a.session_id:
            recs.append(r)
    if not recs:
        sys.exit(f"no records for session {a.session_id} in {a.flow_file}")

    # Normalize time to an offset from the session's first record so the
    # replay can be anchored anywhere. Kept as `t_ms` rather than a rewritten
    # `ts`, so a fixture can never be mistaken for a real dated log.
    base = min(ts_to_ms(r["ts"]) for r in recs)
    recs.sort(key=lambda r: ts_to_ms(r["ts"]))

    out = []
    for r in recs:
        t_ms = ts_to_ms(r["ts"]) - base
        r = scrub(r)
        r["t_ms"] = t_ms
        # Identity is assigned at build time from world.json, never inherited.
        for k in ("ts", "machine_id", "machine_uid"):
            r.pop(k, None)
        out.append(r)

    blob = "\n".join(json.dumps(r, sort_keys=True) for r in out)
    scan_or_die(blob, a.out)

    p = pathlib.Path(a.out); p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(blob + "\n")
    span = (max(r["t_ms"] for r in out)) / 1000.0
    print(f"wrote {p} — {len(out)} records spanning {span:.0f}s, identity stripped")

if __name__ == "__main__":
    main()
