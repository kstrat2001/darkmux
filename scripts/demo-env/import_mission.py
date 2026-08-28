#!/usr/bin/env python3
"""Import a REAL mission's persisted Phase/Task/Step graph into a committed
demo fixture (#2032 Packet 1).

`/mission/:id/graph.json` is built from `<DARKMUX_HOME>/missions/<id>/` —
`mission.json`, `config-snapshot.json`, `phases/`, `tasks/`, `steps/`,
`envelope.json` (see `crates/darkmux-serve/src/mission_graph.rs`'s module
doc). It is NOT derived from flow records, so — unlike everything else in
this demo world — a real mission graph needs its own on-disk directory
imported, not just a flow session. This importer does both: the mission
directory (for the graph) AND the flow records that correlate to it (for the
runs board / mission-status panel / SSE status layering), using the SAME
scrub and the SAME identity-anchoring philosophy as `import_session.py`.

  ./import_mission.py review-<epoch>-<hex> \\
      --missions-out missions --sessions-out sessions

Identity handled here, beyond what `import_session.py` already does for a
flow session:

  - The mission id ITSELF encodes the real wall-clock second it was minted
    on the operator's machine (`review-<epoch>-<hex>`), and every phase id
    is `<mission-id>-<phase-suffix>` — so the id is an identity carrier too,
    not just the `_ts` fields. Rewritten to a NEW id (`--slug`, or derived
    from the mission's own `case_id` when available) everywhere it appears:
    in file/directory names AND inside every JSON string value.
  - Every `*_ts` field (seconds, unlike a flow record's ms `ts`) is renamed
    to `*_ts_rel` and its value replaced with an OFFSET from the mission's
    own `created_ts`. Renaming (not just re-valuing) mirrors
    `import_session.py`'s `t_ms` convention: a fixture must never carry a
    field that LOOKS like a real epoch and isn't one anymore.
    `build.py::materialize_mission` adds a build-time anchor back to turn
    every `*_ts_rel` back into `*_ts` when it writes the demo home.

A mission whose `mission.json` is not `"status": "finalized"` is refused:
the whole point of this packet is a graph that reads as COMPLETE, and an
in-flight mission has no `envelope.json` yet and `Task`/`Step` files still
being written concurrently by the real dispatch.
"""
import argparse, json, pathlib, sys

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from lib_identity_scrub import scrub, scan_or_die, ts_to_ms  # noqa: E402

# Field names `darkmux_crew::types` puts real epoch-SECONDS in (Mission,
# Phase, Step — see that crate's `types.rs`; Task carries none). Matched by
# exact key name, not suffix, so a coincidental unrelated `*_ts` field never
# gets silently offset against the wrong clock.
TS_FIELDS = {
    "created_ts", "started_ts", "completed_ts", "finalized_ts",
    "abandoned_ts", "paused_ts",
}
# A real epoch-seconds value from this decade is comfortably > 10**9; this
# floor is a defense against a coincidentally-named field holding something
# else (a duration, a count) getting offset as if it were a clock reading.
_EPOCH_FLOOR = 10 ** 9


def rewrite_ts(obj, anchor_s):
    """Recursively rename every `*_ts` key to `*_ts_rel`, offset from `anchor_s`.

    Mirrors `import_session.py`'s `t_ms`: the renamed key can never be
    mistaken for a real dated field, and the offset can be replayed against
    any wall-clock instant by `build.py`.
    """
    if isinstance(obj, dict):
        out = {}
        for k, v in obj.items():
            if (k in TS_FIELDS and isinstance(v, (int, float))
                    and not isinstance(v, bool) and v > _EPOCH_FLOOR):
                out[f"{k}_rel"] = int(v) - anchor_s
            else:
                out[k] = rewrite_ts(v, anchor_s)
        return out
    if isinstance(obj, list):
        return [rewrite_ts(v, anchor_s) for v in obj]
    return obj


def replace_id(obj, old_id, new_id):
    """Recursively substring-replace `old_id` with `new_id` in every string.

    Covers the mission id embedded standalone (`Mission.id`,
    `Phase.mission_id`, `Envelope.mission_id`) AND embedded as a prefix of a
    larger id (`Phase.id` = `<mission-id>-<suffix>`) — a plain substring
    replace handles both without a second id-shape parser.
    """
    if isinstance(obj, dict):
        return {k: replace_id(v, old_id, new_id) for k, v in obj.items()}
    if isinstance(obj, list):
        return [replace_id(v, old_id, new_id) for v in obj]
    if isinstance(obj, str):
        return obj.replace(old_id, new_id)
    return obj


def slugify(text):
    import re
    s = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    return s or "mission"


def read_json(path):
    return json.loads(path.read_text())


def dump_json(obj):
    return json.dumps(obj, indent=2, sort_keys=True) + "\n"


def collect_mission_files(mission_dir, old_id):
    """Every JSON file this fixture needs, as `(relpath_template, obj)`.

    `relpath_template` still names the OLD id — `write_files` does the
    id substitution on both content and path in one place, so the two can
    never drift out of sync (a content-only rename would leave a
    `phases/<old-id>-investigate.json` file holding a `<new-id>` payload,
    which `loader::load_phases` — a directory WALK, not an id lookup — would
    still find, but which a human reading the tree would find confusing and
    a future direct-path helper would miss).
    """
    files = {"mission.json": read_json(mission_dir / "mission.json")}
    snap = mission_dir / "config-snapshot.json"
    if snap.exists():
        files["config-snapshot.json"] = read_json(snap)
    env = mission_dir / "envelope.json"
    if env.exists():
        files["envelope.json"] = read_json(env)
    for phase_file in sorted((mission_dir / "phases").glob("*.json")):
        files[f"phases/{phase_file.name}"] = read_json(phase_file)
    for sub in ("tasks", "steps"):
        subdir = mission_dir / sub
        if not subdir.exists():
            continue
        for phase_dir in sorted(subdir.iterdir()):
            if not phase_dir.is_dir():
                continue
            for f in sorted(phase_dir.glob("*.json")):
                files[f"{sub}/{phase_dir.name}/{f.name}"] = read_json(f)
    return files


def collect_flow_records(flows_dir, session_ids, mission_id, ts_lo_ms, ts_hi_ms):
    """Records correlated to ONE mission run.

    `session_id` alone is NOT enough: the review pipeline's per-task
    sessions (`task-review-bundle-task`, `task-review-probe-high-task`, …)
    are named after the TASK, which is the same generic id on every review
    mission ever run — not scoped to one run. Without a time bound this
    silently pulls in every OTHER review mission's task-level records too
    (caught in dev-testing: a mission that ran for 77 minutes came back
    spanning 28 days of unrelated history). The mission's own
    `[created_ts, finalized_ts]` window (padded) is the real correlation key
    for those sessions; `mission_id` on the whole-run bookend records needs
    no such bound, since it names this run and no other by construction.
    """
    recs = []
    for f in sorted(flows_dir.glob("*.jsonl")):
        for lineno, line in enumerate(f.read_text().splitlines(), 1):
            if not line.strip():
                continue
            try:
                r = json.loads(line)
            except json.JSONDecodeError as e:
                # A day file is a casual append target (LocalFileSink), not a
                # hash-chained one, and old scratch/test runs have left at
                # least one genuinely malformed line (two records mashed
                # onto one, from an unrelated 2026-05-19 test session) —
                # skip it loudly rather than let it abort a real import.
                print(f"  ! skipping malformed line {f}:{lineno} ({e})",
                      file=sys.stderr)
                continue
            if r.get("mission_id") == mission_id:
                recs.append(r)
                continue
            if r.get("session_id") in session_ids:
                t = ts_to_ms(r["ts"])
                if ts_lo_ms <= t <= ts_hi_ms:
                    recs.append(r)
    return recs


def self_test():
    """No fixture, no daemon, no `~/.darkmux` needed — pure logic checks for
    the two things this module has to get exactly right: the timestamp
    rebase (round-trips through `build.py`'s inverse) and the scrub (still
    refuses a planted leak). Run with `--self-test`.
    """
    failures = []

    def check(name, cond):
        print(f"  {'PASS' if cond else 'FAIL'}  {name}")
        if not cond:
            failures.append(name)

    # ---- rewrite_ts round-trips with build.py's inverse --------------------
    sys.path.insert(0, str(HERE))
    import build as _build  # local import: only needed for this check

    anchor_s = 1700000000  # arbitrary fixed epoch; a synthetic anchor, not a real run's
    original = {
        "id": "review-1700000000-abc123",
        "created_ts": anchor_s,
        "started_ts": anchor_s,
        "finalized_ts": anchor_s + 4616,
        "nested": {
            # A real TS_FIELDS key at a REAL epoch — must rewrite, nested or not.
            "completed_ts": anchor_s + 3,
            # A real TS_FIELDS key, but a value far too small to be a real
            # epoch (a duration, a count) — the `_EPOCH_FLOOR` guard exists
            # for exactly this, and only a VALUE check catches it (the key
            # name alone can't tell "42 seconds since 1970" from "42 turns").
            "paused_ts": 42,
            # Not a TS_FIELDS key at all — the exact-name-match guard, a
            # DIFFERENT defense than the floor above (this one can't fire
            # regardless of value; testing it with a big value keeps the two
            # guards from masking each other).
            "not_a_ts": 4616,
        },
        "phase_ids": ["review-1700000000-abc123-investigate"],
    }
    rewritten = rewrite_ts(json.loads(json.dumps(original)), anchor_s)
    check("rewrite_ts renames every top-level *_ts key to *_ts_rel",
          {"created_ts_rel", "started_ts_rel", "finalized_ts_rel"} <= set(rewritten)
          and not any(k in rewritten for k in ("created_ts", "started_ts", "finalized_ts")))
    check("rewrite_ts offsets created_ts to 0 (the mission's own start)",
          rewritten["created_ts_rel"] == 0)
    check("rewrite_ts offsets finalized_ts to the real span",
          rewritten["finalized_ts_rel"] == 4616)
    check("rewrite_ts rewrites a NESTED real-TS_FIELDS key too",
          rewritten["nested"].get("completed_ts_rel") == 3
          and "completed_ts" not in rewritten["nested"])
    check("rewrite_ts leaves a TS_FIELDS key with a too-small value alone "
          "(the _EPOCH_FLOOR guard)",
          rewritten["nested"]["paused_ts"] == 42
          and "paused_ts_rel" not in rewritten["nested"])
    check("rewrite_ts leaves a non-TS_FIELDS key alone regardless of value "
          "(the exact-name guard)",
          rewritten["nested"]["not_a_ts"] == 4616
          and "not_a_ts_rel" not in rewritten["nested"])

    # A different `start_s` at materialize time — proves the offset, not the
    # original absolute value, is what survives the round trip.
    build_start_s = 1800000000
    reanchored = _build._reanchor_ts_rel(json.loads(json.dumps(rewritten)), build_start_s)
    check("reanchor(rewrite_ts(x)) restores *_ts keys with NO *_ts_rel left",
          {"created_ts", "started_ts", "finalized_ts"} <= set(reanchored)
          and not any(k.endswith("_ts_rel") for k in reanchored))
    check("reanchored created_ts lands exactly on the NEW anchor",
          reanchored["created_ts"] == build_start_s)
    check("reanchored finalized_ts preserves the ORIGINAL span (not the "
          "original absolute value)",
          reanchored["finalized_ts"] - reanchored["created_ts"] == 4616
          and reanchored["finalized_ts"] != original["finalized_ts"])
    check("reanchored nested completed_ts preserves ITS offset (+3) too",
          reanchored["nested"]["completed_ts"] == build_start_s + 3)
    check("fields the floor/name guards skipped survive the round trip "
          "unchanged (never touched twice)",
          reanchored["nested"]["paused_ts"] == 42
          and reanchored["nested"]["not_a_ts"] == 4616)

    # ---- replace_id renames the mission id AND its phase-id compositions ---
    new_id = "demo-review-nameof-recency"
    renamed = replace_id(json.loads(json.dumps(original)), original["id"], new_id)
    check("replace_id renames the standalone mission id",
          renamed["id"] == new_id)
    check("replace_id renames the mission id EMBEDDED as a phase-id prefix",
          renamed["phase_ids"] == [f"{new_id}-investigate"])
    check("replace_id leaves a non-identity int field untouched",
          renamed["nested"]["not_a_ts"] == original["nested"]["not_a_ts"])

    # ---- the scrub backstop still refuses a planted leak --------------------
    leaked = {"note": "nobody@example.com leaked"}
    paths = scrub({"a": "/Users/nobody/proj", "b": "/Users/nobody/de-things/some-checkout/src",
                   "c": "/Users/nobody/.darkmux/flows", "d": "/home/demo/darkmux/templates/x"})
    check("scrub() rewrites the host path", "/Users/nobody" not in paths["a"])
    check("scrub() collapses the path tail after the username (a project root, "
          "a checkout name are identity too)",
          paths["b"] == "/home/demo/workspace" and paths["a"] == "/home/demo/workspace")
    check("scrub() keeps the demo home and the rewritten repo root",
          paths["c"] == "/home/demo/.darkmux/flows" and paths["d"] == "/home/demo/darkmux/templates/x")
    caught = False
    try:
        scan_or_die(json.dumps(leaked), "self-test")
    except SystemExit:
        caught = True
    check("scan_or_die refuses a sentinel that scrub() didn't rewrite "
          "(the sentinel scrub, unlike the host-path one, only runs at the "
          "batch scan — this proves the backstop, not scrub() itself, "
          "catches it)",
          caught)

    if failures:
        sys.exit(f"\n{len(failures)} self-test check(s) failed: {failures}")
    print("\nall self-test checks passed")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--self-test", action="store_true",
                     help="run internal checks (no ~/.darkmux, no fixture, "
                          "no network) and exit")
    ap.add_argument("mission_id", nargs="?")
    ap.add_argument("--home", default="~/.darkmux",
                     help="real DARKMUX_HOME to read from (default ~/.darkmux)")
    ap.add_argument("--slug", default=None,
                     help="demo mission id / fixture dirname (default: the "
                          "mission's own case_id from envelope.payload.case_id, "
                          "slugified; falls back to the mission id itself)")
    ap.add_argument("--missions-out", default=str(HERE / "missions"))
    ap.add_argument("--sessions-out", default=str(HERE / "sessions"))
    ap.add_argument("--flows-dir", default=None,
                     help="default: <home>/flows")
    a = ap.parse_args()

    if a.self_test:
        self_test()
        return
    if not a.mission_id:
        ap.error("mission_id is required unless --self-test is given")

    home = pathlib.Path(a.home).expanduser()
    mission_dir = home / "missions" / a.mission_id
    if not mission_dir.is_dir():
        sys.exit(f"no mission directory at {mission_dir}")

    mission = read_json(mission_dir / "mission.json")
    old_id = mission["id"]
    if mission.get("status") != "finalized":
        sys.exit(f"mission `{old_id}` is `{mission.get('status')}`, not "
                  f"`finalized` — refusing to import. The graph this packet "
                  f"proves out must read as COMPLETE; an in-flight mission "
                  f"has no envelope yet and its Task/Step files are still "
                  f"being written by the real dispatch. Wait for it to "
                  f"finish, then re-run.")
    anchor_s = mission["created_ts"]

    files = collect_mission_files(mission_dir, old_id)

    # The review pipeline's whole-run dispatch bookend uses `case_id` itself
    # as its flow-record `session_id` (`mission_launch_review.rs` — distinct
    # from the `mission-<id>`/`task-<id>` sessions `darkmux-crew`'s scheduler
    # mints), so it has to be read now, before slug/session-id derivation
    # can use it for either purpose.
    env = files.get("envelope.json")
    case_id = (env or {}).get("payload", {}).get("case_id")

    # Slug: prefer the mission's own case_id (operator-chosen at launch —
    # `demo-review-nameof-recency` was picked FOR this purpose) over the
    # mission id, which encodes a real wall-clock second.
    slug = a.slug or (slugify(case_id) if case_id else slugify(old_id))
    new_id = slug

    # ---- mission directory ------------------------------------------------
    out_files = {}
    for relpath, obj in files.items():
        obj = rewrite_ts(obj, anchor_s)
        obj = replace_id(obj, old_id, new_id)
        obj = scrub(obj)
        new_relpath = relpath.replace(old_id, new_id)
        out_files[new_relpath] = dump_json(obj)

    blob = "\n".join(out_files.values())
    missions_out = pathlib.Path(a.missions_out) / slug
    scan_or_die(blob, missions_out)

    for relpath, text in out_files.items():
        p = missions_out / relpath
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)

    # ---- correlated flow records -------------------------------------------
    task_ids = [
        obj["id"] for relpath, obj in files.items()
        if relpath.startswith("tasks/")
    ]
    session_ids = {f"mission-{old_id}"} | {f"task-{tid}" for tid in task_ids}
    if case_id:
        # The crew-level session (`single_shot_chat` telemetry/tokens/lms +
        # its own `dispatch start`/`dispatch complete` bookend) — the bulk
        # of a review's host-telemetry sampling lives here, and it is named
        # by `case_id` alone (see the comment above), so it needs the same
        # treatment as the scheduler's own sessions.
        session_ids.add(case_id)
    flows_dir = pathlib.Path(a.flows_dir).expanduser() if a.flows_dir else home / "flows"
    # Padded window around the mission's own lifetime — see
    # `collect_flow_records`'s doc for why the pad matters (task-level
    # session ids are generic, not mission-scoped) and why it's a doubled
    # duration rather than a fixed few seconds: floor of 10 minutes for a
    # near-instant mission, otherwise the mission's own length again on
    # each side, to absorb ordinary clock/flush skew at the edges without
    # still reaching into an adjacent unrelated run recorded hours away.
    finalized_s = mission.get("finalized_ts") or (anchor_s + 3600)
    pad_s = max(600, (finalized_s - anchor_s))
    ts_lo_ms = (anchor_s - pad_s) * 1000
    ts_hi_ms = (finalized_s + pad_s) * 1000
    recs = collect_flow_records(flows_dir, session_ids, old_id, ts_lo_ms, ts_hi_ms)
    if not recs:
        sys.exit(f"no flow records found for mission `{old_id}` under {flows_dir} "
                  f"(looked for session_id in {sorted(session_ids)} or "
                  f"mission_id == {old_id!r})")

    anchor_ms = anchor_s * 1000
    recs.sort(key=lambda r: ts_to_ms(r["ts"]))
    out_recs = []
    for r in recs:
        t_ms = ts_to_ms(r["ts"]) - anchor_ms
        r = replace_id(r, old_id, new_id)
        r = scrub(r)
        r["t_ms"] = t_ms
        # Identity is assigned at build time from world.json, never
        # inherited — same convention as `import_session.py`.
        for k in ("ts", "machine_id", "machine_uid"):
            r.pop(k, None)
        out_recs.append(r)

    sess_blob = "\n".join(json.dumps(r, sort_keys=True) for r in out_recs)
    sessions_out = pathlib.Path(a.sessions_out) / f"{slug}.jsonl"
    scan_or_die(sess_blob, sessions_out)
    sessions_out.parent.mkdir(parents=True, exist_ok=True)
    sessions_out.write_text(sess_blob + "\n")

    def find_ts_rel(obj):
        if isinstance(obj, dict):
            for k, v in obj.items():
                if k.endswith("_ts_rel") and isinstance(v, (int, float)):
                    yield v
                yield from find_ts_rel(v)
        elif isinstance(obj, list):
            for v in obj:
                yield from find_ts_rel(v)

    span_s = max(
        (v for f in out_files.values() for v in find_ts_rel(json.loads(f))),
        default=0,
    )
    span_ms = max((r["t_ms"] for r in out_recs), default=0)
    print(f"imported mission `{old_id}` -> `{new_id}` (slug {slug})")
    print(f"  mission dir  {missions_out} — {len(out_files)} files, "
          f"span {span_s}s (directory) / {span_ms / 1000:.0f}s (records)")
    print(f"  sessions     {sessions_out} — {len(out_recs)} records, "
          f"{len(session_ids)} session(s) + mission_id correlation")


if __name__ == "__main__":
    main()
