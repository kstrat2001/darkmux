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
import argparse, json, re, sys, pathlib

# Anything matching these never reaches a committed fixture. The scan is a
# BACKSTOP, not the mechanism: the rewrites below remove the known carriers,
# and this fails the import loudly if an unknown one survives. A fixture is
# published to a public docs site, so "probably clean" is not a standard.
FORBIDDEN = [
    (re.compile(r"/Users/[^/\"\s]+"),          "a host home directory"),
    (re.compile(r"\b100\.\d{1,3}\.\d{1,3}\.\d{1,3}\b"), "a tailnet IP"),
    (re.compile(r"(?i)\bfinhero\b"),           "an engagement name"),
    (re.compile(r"(?i)\bextragalaxies\b"),     "an engagement name"),
    (re.compile(r"(?i)sisters inspire"),       "an engagement name"),
    (re.compile(r"\bSYS-\d+\b"),               "an internal ticket key"),
    (re.compile(r"(?i)[\w.+-]+@[\w-]+\.[\w.]+"), "an email address"),
    (re.compile(r"(?i)\b[\w-]+\.ts\.net\b"),   "a MagicDNS name"),
    # The host's own name. It survives rewrites aimed at the DOMAIN (it is the
    # left-most label), and it is the single most identifying string a
    # `doctor` or `flow status` capture carries.
    (re.compile(r"(?i)\b(?:macbook-pro|kains?-mac(?:book|-studio)?|mac-studio)\b"),
     "the operator's hostname"),
]

def scrub(obj):
    """Rewrite the KNOWN identity carriers. Anything left is caught below."""
    if isinstance(obj, dict):
        return {k: scrub(v) for k, v in obj.items()}
    if isinstance(obj, list):
        return [scrub(v) for v in obj]
    if isinstance(obj, str):
        # A sandbox path is real content (the run's workspace), so it is
        # rewritten to an equivalent demo path rather than blanked — the run
        # detail lens renders it, and an empty workspace row reads as a bug.
        s = re.sub(r"/Users/[^/\"\s]+/\.darkmux", "/home/demo/.darkmux", obj)
        s = re.sub(r"/Users/[^/\"\s]+", "/home/demo", s)
        # Network identity. Both carriers name the operator's private tailnet,
        # and both show up in output that otherwise belongs in the docs
        # (`darkmux doctor` prints the daemon's bind host). Rewritten to a
        # demo-domain equivalent rather than blanked, so the surrounding line
        # still reads as the real check output it is.
        # Consume the WHOLE FQDN, host label included. Matching only the
        # `<tailnet>.ts.net` suffix left the host label behind, so
        # `macbook-pro.taild<...>.ts.net` scrubbed to
        # `macbook-pro.demo-hub.internal` — still the operator's real machine
        # name, in a string that goes on a public docs page.
        s = re.sub(r"\b[\w-]+(?:\.[\w-]+)*\.ts\.net\b", "demo-hub.internal", s)
        s = re.sub(r"\b100\.\d{1,3}\.\d{1,3}\.\d{1,3}\b", "10.0.0.21", s)
        return s
    return obj

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
    def ms(r):
        from datetime import datetime
        return int(datetime.strptime(r["ts"], "%Y-%m-%dT%H:%M:%SZ").timestamp() * 1000)
    base = min(ms(r) for r in recs)
    recs.sort(key=ms)

    out = []
    for r in recs:
        r = scrub(r)
        r["t_ms"] = ms(r) - base
        # Identity is assigned at build time from world.json, never inherited.
        for k in ("ts", "machine_id", "machine_uid"):
            r.pop(k, None)
        out.append(r)

    blob = "\n".join(json.dumps(r, sort_keys=True) for r in out)
    for pat, what in FORBIDDEN:
        m = pat.search(blob)
        if m:
            sys.exit(f"REFUSING to write {a.out}: found {what} ({m.group(0)!r}).\n"
                     f"Add a rewrite rule to scrub() — do not weaken the scan.")

    p = pathlib.Path(a.out); p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(blob + "\n")
    span = (max(r["t_ms"] for r in out)) / 1000.0
    print(f"wrote {p} — {len(out)} records spanning {span:.0f}s, identity stripped")

if __name__ == "__main__":
    main()
