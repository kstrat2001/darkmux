#!/usr/bin/env python3
"""Build the demo world: an isolated DARKMUX_HOME plus the fixtures that
override the routes a real daemon can only answer from real hardware.

The split is the whole design. Most of what the viewer shows is DERIVED from
flow records by the daemon itself (`/runs`, `/missions`, `/phases`, `/flow-*`,
`/fleet/*`, the mission graph). Hand-authoring those would mean re-deriving,
in Python, what `runs.rs` already derives in Rust — two implementations of one
rule, drifting from the first edit. So this writes RECORDS and lets the real
daemon derive the rest.

Only two families genuinely cannot come from records, and both are overridden
by `serve.py` from fixtures written here:

  /machine/*  — host probes (`vm_stat`, `sysctl`, `lms`). The demo machine is
                a 256 GB M5 Ultra; this laptop is not, and no amount of
                seeding changes what a probe reads.
  /panel/*    — SOME panels shell out to real CLI verbs that probe the machine
                (`doctor`, `machine status`). Those are canned. The panels
                that read the demo home instead (`run list`, `flow status`,
                `config list`, ...) are passed straight through to the daemon,
                which renders them from demo data for free.

Usage:
    ./build.py                      # -> ../../screenshots/{demo-home,fixtures}
    ./build.py --now 2026-08-26T14:30:00Z
"""
import argparse, json, pathlib, random, subprocess, sys, zlib
from datetime import datetime, timedelta, timezone

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent.parent
GIB = 1024 ** 3

# ---------------------------------------------------------------- world math

def ledger_for(machine, world, now_ms):
    """Derive a `/machine/resources` ledger from a machine's declaration.

    Every figure is COMPUTED from the residents and the machine's declared
    headroom, never written down twice, so editing world.json can't produce a
    ledger whose parts disagree with its totals. The derivation rules are the
    ones documented on `MachineResources` in ui/src/types/handwritten.ts:
      other_used      = declared non-darkmux usage
      pool.used       = other_used + darkmux current
      projected_total = other_used + darkmux potential
      state           = projected_total vs limit (amber when it would not fit)
    """
    cap = machine["ram_gb"] * GIB
    kvpt, wts = world["kv_per_token_bytes"], world["weights_bytes"]

    models = []
    for r in machine["residents"]:
        key = r["model"]
        kv_at_ctx = kvpt[key] * r["ctx"]
        weights = wts[key]
        potential = weights + kv_at_ctx
        current = weights + int(kv_at_ctx * r["kv_used_frac"])
        models.append({
            "identifier": f"darkmux:{key}", "model_key": key, "owner": "darkmux",
            "loaded_ctx": r["ctx"], "weights_bytes": weights,
            "kv_per_token_bytes": kvpt[key], "kv_bytes_at_ctx": kv_at_ctx,
            "potential_bytes": potential, "potential_source": "arch",
            "current_bytes": current, "state": "green",
        })

    potential = sum(m["potential_bytes"] for m in models)
    current = sum(m["current_bytes"] for m in models)
    other_used = machine["other_used_gb"] * GIB
    pool_used = other_used + current
    projected = other_used + potential
    margin = machine["margin_percent"]

    # The real cascade, in the real order: margin is the sole red trigger;
    # otherwise it is a fit question against the limit.
    if margin < 15:
        state = "red"
    elif projected > cap:
        state = "amber"
    else:
        state = "green"
    for m in models:
        m["state"] = state

    return {
        "schema_version": "2.1", "generated_at_ms": now_ms,
        "gather_ms": random.randint(180, 460),
        "limit_bytes": cap, "limit_source": "physical_pool",
        "pool": {
            "capacity_bytes": cap, "used_bytes": pool_used,
            "available_bytes": cap - pool_used,
            "free_bytes": int((cap - pool_used) * 0.22),
        },
        "pressure": {
            "swap_used_bytes": machine["swap_used_gb"] * GIB,
            "compressor_bytes": machine["compressor_gb"] * GIB,
            "margin_percent": margin, "red": margin < 15,
        },
        "models": models,
        "machine": {
            "potential_bytes": potential, "unpriced_models": 0,
            "estimated_models": 0, "over_price_models": 0,
            "current_bytes": current, "other_used_bytes": other_used,
            "projected_total_bytes": projected, "state": state,
        },
        "attribution": "per_process",
        "attribution_note": (
            f"{len(models)} worker(s) for {len(models)} resident(s) — per-model "
            "footprint (max of rss and phys_footprint), workers rank-matched to "
            "models by weights (largest worker <-> largest weights)"
        ),
        "messages": [], "cache_ttl_ms": 2000,
    }


def specs_for(machine, world, ledger, now_ms, version, schema):
    def gb(n):  # the `lms ps` size string, same rounding LMStudio prints
        return f"{n / 1e9:.2f} GB"
    loaded = [{
        "identifier": m["identifier"], "model": m["model_key"], "status": "idle",
        "size": gb(m["weights_bytes"]), "context": m["loaded_ctx"],
    } for m in ledger["models"]]
    util = next((r for r in machine["residents"] if r.get("utility")), None)
    return {
        "darkmux_version": version, "flow_schema_version": schema,
        "machine_id": machine["id"], "os": "macos aarch64",
        "ram_total_bytes": machine["ram_gb"] * GIB,
        "ram_free_for_ai_bytes": ledger["pool"]["available_bytes"],
        "cpu_brand": machine["cpu_brand"], "loaded_models": loaded,
        "lms_unreachable": False,
        "utility_model": ({"id": f"darkmux:{util['model']}", "loaded": True}
                          if util else None),
        # Never a real endpoint: this field is typed but unrendered, and a
        # committed fixture is the wrong place to find out that changed.
        "redis_url_redacted": "redis://demo-hub.internal:6379",
        "generated_at_ms": now_ms,
    }


# ------------------------------------------------------------- record replay

# What the demo day contains. Each entry replays the committed session under a
# new identity, so every record keeps the shape its real emitter gave it while
# the fleet reads as a day of heterogeneous work.
#
# `ends_ago_min` places the run's END relative to `now`; a `live` entry is
# truncated at `now` instead and never emits its terminal record, which is what
# makes the runs board show a RUNNING row and the detail lens show a pulse.
REPLAYS = [
    dict(slug="crawl-error-discard",  machine="m5-ultra-256gb",     model="qwen3.6-35b-a3b-turboquant-mlx", role="crawler",  ends_ago_min=14,  scale=1.00),
    dict(slug="review-flow-sinks",    machine="m5-ultra-256gb",     model="qwen3.5-122b-a10b",              role="reviewer", ends_ago_min=52,  scale=1.60),
    dict(slug="crawl-unwrap-paths",   machine="m5-ultra-256gb",     model="qwen3.6-35b-a3b-turboquant-mlx", role="crawler",  ends_ago_min=97,  scale=0.72),
    dict(slug="doc-drift-sweep",      machine="m1-max-32gb-studio", model="qwen3-4b-instruct-2507",         role="scribe",   ends_ago_min=133, scale=0.28),
    dict(slug="review-serve-routes",  machine="m5-ultra-256gb",     model="qwen3-coder-next-mlx",           role="reviewer", ends_ago_min=181, scale=1.20),
    dict(slug="crawl-lock-ordering",  machine="m5-ultra-256gb",     model="qwen3.5-122b-a10b",              role="analyst",  ends_ago_min=244, scale=1.85),
    dict(slug="estimate-backlog",     machine="mac-mini-m4-16gb",   model="qwen3-4b-instruct-2507",         role="estimator",ends_ago_min=298, scale=0.18),
    dict(slug="crawl-error-discard-2",machine="m5-ultra-256gb",     model="qwen3.6-35b-a3b-turboquant-mlx", role="crawler",  ends_ago_min=355, scale=0.94),
    dict(slug="compact-trajectories", machine="m1-max-32gb-studio", model="qwen3-4b-instruct-2507",         role="compactor",ends_ago_min=412, scale=0.22),
    # The live one. Started `started_ago_min` ago and still going, so the
    # runs board, the fleet lens and the detail lens all have something in
    # flight to render.
    dict(slug="crawl-discarded-locks", machine="m5-ultra-256gb", model="qwen3.6-35b-a3b-turboquant-mlx", role="crawler", started_ago_min=6, live=True, scale=1.0),
]

# Token-bearing payload fields, scaled per replay so the fleet totals read as a
# real mixed workload instead of ten identical runs. Scaling the RECORDED
# fields (rather than inventing new ones) keeps every derived total — the
# savings hero, the run detail banks — consistent with its own records.
TOKEN_FIELDS = ("prompt_tokens", "completion_tokens", "total_tokens",
                "cumulative_chars", "prompt_chars", "system_chars",
                "reasoning_chars", "args_chars", "result_chars",
                "stdout_chars", "stderr_chars", "used")


def iso(ms):
    return datetime.fromtimestamp(ms / 1000, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def replay(source, plan, machine, now_ms, session_id):
    """Re-anchor + re-identify one recorded session."""
    # The model the recorded session actually dispatched TO, read from the
    # records rather than assumed, so re-importing a different session does
    # not silently re-skin the wrong half of its telemetry.
    source_primary = next((r.get("model") for r in source
                           if r.get("action") == "dispatch start" and r.get("model")), None)
    # The runs board reads a run's ROLE from the record `handle`
    # (`runs.rs:1403` — `agg.role` falls back to the bookend's handle), so a
    # replay that renames only the session id shows every run as the recorded
    # session's role. Read the source handle rather than assuming "crawler",
    # so re-importing a different session stays correct.
    source_role = next((r.get("handle") for r in source
                        if r.get("action") == "dispatch start" and r.get("handle")), None)
    span = max(r["t_ms"] for r in source)
    live = plan.get("live", False)
    if live:
        start = now_ms - plan["started_ago_min"] * 60_000
        cutoff = now_ms - start
    else:
        start = now_ms - plan["ends_ago_min"] * 60_000 - span
        cutoff = None

    scale, out = plan["scale"], []
    for rec in source:
        if cutoff is not None and rec["t_ms"] > cutoff:
            continue
        r = json.loads(json.dumps(rec))          # deep copy; source is reused
        t = r.pop("t_ms")
        # A live run has not finished, so it must not carry a terminal record.
        # Dropping it is what the daemon reads as "still running" — the same
        # bookend rule contract 2 states, honored rather than simulated.
        if live and r.get("action") in ("dispatch complete", "dispatch error"):
            continue
        r["ts"] = iso(start + t)
        r["session_id"] = session_id
        r["machine_id"] = machine["id"]
        r["machine_uid"] = machine["uid"]
        if r.get("model") == source_primary:
            r["model"] = plan["model"]
        if r.get("handle") == source_role:
            r["handle"] = plan["role"]
        p = r.get("payload")
        if isinstance(p, dict):
            for f in TOKEN_FIELDS:
                if isinstance(p.get(f), (int, float)) and not isinstance(p.get(f), bool):
                    p[f] = int(p[f] * scale)
            # Only the PRIMARY model is re-skinned. A session names more than
            # one model — `telemetry.lms` records the compactor's load too —
            # and blanket-rewriting made the run detail's "loaded models" list
            # the same model twice, at two different sizes. The utility model
            # is already a real resident in world.json, so leaving it alone is
            # both correct and consistent with the machine lens.
            if p.get("model") == source_primary:
                p["model"] = plan["model"]
            if isinstance(p.get("workspace"), str):
                p["workspace"] = f"/home/demo/.darkmux/runs/{plan['slug']}/sandbox"
        out.append(r)
    return out, start


# ------------------------------------------------------------ canned panels

def canned_machine_status(machine, ledger):
    """`darkmux machine status`, rendered from the demo ledger.

    Canned because the real verb shells out to `lms` and would report THIS
    machine. Rendered from `ledger` rather than written out by hand so it
    cannot disagree with the machine lens standing next to it in the docs.
    """
    lines = [
        "\x1b[1;36mloaded models — darkmux-managed\x1b[0m",
        "  IDENTIFIER                                    MODEL                              CTX       SIZE",
    ]
    for m in ledger["models"]:
        lines.append(f"  {m['identifier']:<45} {m['model_key']:<34} {m['loaded_ctx']:<9} "
                     f"{m['weights_bytes'] / 1e9:.2f} GB")
    lines += [
        "",
        "\x1b[1;36muser state — not managed by darkmux\x1b[0m",
        "  (none)",
        "",
        f"  pool {ledger['pool']['capacity_bytes'] // GIB} GB · darkmux holds "
        f"{ledger['machine']['current_bytes'] / 1e9:.1f} GB · committed "
        f"{ledger['machine']['potential_bytes'] / 1e9:.1f} GB · "
        f"\x1b[32m{ledger['machine']['state'].upper()}\x1b[0m",
    ]
    return "\n".join(lines) + "\n"


def panel_env(home):
    """Env that pins every CLI shell-out to the demo home."""
    import os
    e = dict(os.environ)
    e.update({
        "DARKMUX_HOME": str(home),
        "DARKMUX_FLOWS_DIR": str(home / "flows"),
        "DARKMUX_CREW_DIR": str(home / "crew"),
        "DARKMUX_MACHINE_ID": "m5-ultra-256gb",
        # Never let a demo shell-out reach a real coordination substrate.
        "DARKMUX_REDIS_URL": "",
        # Point at the demo's own (empty) audit dir rather than unsetting:
        # an empty value falls back to the config tier, which resolved to the
        # operator's real chain and reported 11 legacy files in a panel that
        # is supposed to describe the demo machine.
        "DARKMUX_AUDIT_DIR": str(home / "audit"),
        "DARKMUX_PROFILES": str(home / "profiles.json"),
    })
    return e


# The widths the canned doctor is captured at. The real daemon re-renders per
# request (`panel.rs` passes the client's measured width as COLUMNS, and
# `doctor` honors it as of #1995); a canned panel frozen at ONE width served a
# 99-column render to a phone asking for 42, which made the demo misrepresent
# the product as worse than it is. Serving the nearest capture restores the
# behavior the real thing has.
# 40 is `output_width()`'s own floor in darkmux-doctor; capturing below it
# would misrepresent the product, and capturing no lower than 48 made the
# demo overflow on a phone where the real daemon would not.
DOCTOR_COLS = (40, 48, 64, 80, 100, 140, 200)


def canned_doctor(home, cols=120):
    """`darkmux doctor` against the demo home, scrubbed.

    Run rather than written: doctor's output is dense, colored and changes
    with the release, so a hand-copied version is stale the moment a check is
    added. It still gets the importer's scrub + refusal scan, because doctor
    reports on the machine it runs on and this one is not the demo machine.
    """
    from import_session import scrub, FORBIDDEN
    import re
    try:
        env = panel_env(home)
        env["COLUMNS"] = str(cols)
        p = subprocess.run(["darkmux", "doctor"], env=env,
                           capture_output=True, text=True, timeout=120)
        text = p.stdout or ""
    except Exception as exc:                       # noqa: BLE001 - advisory
        print(f"  ! doctor shell-out failed ({exc}); panel omitted", file=sys.stderr)
        return None
    # The demo home lives under the repo's gitignored `screenshots/`, so its
    # absolute path is both long and obviously a dev scratch dir. Map it to
    # the path a real operator's home actually has BEFORE the generic scrub:
    # shorter (an unbreakable path token is the one thing wrapping cannot
    # help) and honest about what the reader would see on their own machine.
    text = text.replace(str(home), "/home/demo/.darkmux")
    text = scrub(text)
    for pat, what in FORBIDDEN:
        m = pat.search(text)
        if m:
            print(f"  ! doctor output still contains {what} ({m.group(0)!r}); "
                  f"panel omitted rather than shipped", file=sys.stderr)
            return None
    return text


def panel_payload(pid, argv, text, now_ms, cols=120):
    return {
        "panel": pid, "argv": argv, "opts": {},
        "captured_ts_ms": now_ms, "gather_ms": 3, "exit_code": 0,
        "ansi_text": text, "stderr_tail": "", "cols": cols,
        "cache_ttl_ms": 3000, "age_ms": 0,
        # doctor PROBES, so it must never auto-refresh (#1286: the observer
        # must not join the observed). Mirrors MANUAL_PANELS in the viewer.
        "auto_refresh": pid != "doctor",
    }


# --------------------------------------------------------------------- main

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--now", help="anchor instant, e.g. 2026-08-26T14:30:00Z (default: real now)")
    ap.add_argument("--out", default=str(ROOT / "screenshots"))
    ap.add_argument("--seed", type=int, default=7, help="jitter seed; fixed so rebuilds are stable")
    a = ap.parse_args()
    random.seed(a.seed)

    now = (datetime.strptime(a.now, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
           if a.now else datetime.now(timezone.utc))
    now_ms = int(now.timestamp() * 1000)

    world = json.loads((HERE / "world.json").read_text())
    by_id = {m["id"]: m for m in world["machines"]}
    hero = by_id[world["hero_machine"]]

    src = [json.loads(l) for l in (HERE / "sessions" / "crawl-error-discard.jsonl")
           .read_text().splitlines() if l.strip()]

    out = pathlib.Path(a.out)
    home, fx = out / "demo-home", out / "fixtures"
    for d in (home / "flows", home / "crew", fx / "machine", fx / "panel"):
        d.mkdir(parents=True, exist_ok=True)

    version = subprocess.run(["darkmux", "--version"], capture_output=True, text=True
                             ).stdout.split()[1] if True else "2.12.0"
    schema = "1.20.0"

    # ---- flow records -----------------------------------------------------
    records, minted = [], []
    for plan in REPLAYS:
        machine = by_id[plan["machine"]]
        # crc32, not hash(): `hash()` on a str is salted per interpreter run,
        # so ids would change on every rebuild and a deep link into a run
        # would rot between builds.
        digest = zlib.crc32(plan["slug"].encode()) % 10**10
        sid = f"darkmux-{plan['role']}-{plan['slug']}-{digest}"
        recs, _ = replay(src, plan, machine, now_ms, sid)
        minted.append((sid, plan))
        records.extend(recs)

    # Presence: one `machine.online` per machine, recent enough that the fleet
    # lens shows all three as live. Same shape the presence reconciler emits.
    for m in world["machines"]:
        records.append({
            "ts": iso(now_ms - 90_000), "level": "info", "category": "machinery",
            "tier": "local", "stage": "dispatch", "action": "machine.online",
            "handle": m["id"], "source": "presence-reconciler",
            "machine_id": m["id"], "machine_uid": m["uid"],
        })

    records.sort(key=lambda r: r["ts"])
    by_day = {}
    for r in records:
        by_day.setdefault(r["ts"][:10], []).append(r)
    for day, rs in by_day.items():
        (home / "flows" / f"{day}.jsonl").write_text(
            "\n".join(json.dumps(r) for r in rs) + "\n")

    # ---- demo home config -------------------------------------------------
    (home / "config.json").write_text(json.dumps({
        "schema_version": "1.2", "machine_id": hero["id"], "orchestrator": "claude-code",
        "lms_bin": "lms", "lmstudio_url": "http://localhost:1234",
        "redis": {"enabled": False, "host": "127.0.0.1", "port": 6379,
                  "stream": "darkmux:flow", "maxlen": 10000},
        "audit": {"enabled": False, "dir": "~/.darkmux/audit"},
        "runtime": {"inactivity_timeout_seconds": 600, "strict_selection": False,
                    "feedback_injection": True, "check_updates": False},
        "remote": {"max_tokens_per_execution": 500000},
        "fleet": {"mode": hero["fleet_mode"]},
    }, indent=2) + "\n")

    # ---- demo profiles registry ------------------------------------------
    # `doctor` and several console panels read the profiles registry. Without
    # a demo one they read the OPERATOR's, and the canned panel then reports
    # on profiles that do not exist in the demo world (a real capture showed
    # `bakeoff-a10b has fields not consumed by the internal runtime` — a true
    # statement about the wrong machine).
    roles = {"coder": "qwen3.6-35b-a3b-turboquant-mlx", "reviewer": "qwen3-coder-next-mlx",
             "analyst": "qwen3.5-122b-a10b"}
    profiles = {}
    for name, mid in roles.items():
        profiles[name] = {
            "description": f"{name} seat — {mid}",
            "models": [
                {"id": mid, "n_ctx": 131072, "role": "primary"},
                {"id": "qwen3-4b-instruct-2507", "n_ctx": 32768, "role": "compactor"},
            ],
            "runtime": {"contextTokens": 120000, "compaction": {"mode": "default"}},
        }
    (home / "profiles.json").write_text(json.dumps({
        "_comment": "Demo profiles. Generated by scripts/demo-env/build.py.",
        "profiles": profiles,
        "default_profile": "coder",
        "internal": {"utility": "qwen3-4b-instruct-2507"},
        "hooks": {},
    }, indent=2) + "\n")
    (home / "audit").mkdir(exist_ok=True)

    # ---- machine fixtures (one per machine; serve.py picks the hero) ------
    ledgers = {}
    for m in world["machines"]:
        led = ledger_for(m, world, now_ms)
        ledgers[m["id"]] = led
        spec = specs_for(m, world, led, now_ms, version, schema)
        (fx / "machine" / f"{m['id']}.resources.json").write_text(json.dumps(led, indent=2))
        (fx / "machine" / f"{m['id']}.specs.json").write_text(json.dumps(spec, indent=2))
        (fx / "machine" / f"{m['id']}.status.json").write_text(json.dumps({
            "models": spec["loaded_models"], "lms_unreachable": False,
            "generated_at_ms": now_ms,
        }, indent=2))
    (fx / "machine" / "HERO").write_text(hero["id"] + "\n")
    (fx / "demo-uids.json").write_text(json.dumps(
        [m["uid"] for m in world["machines"]], indent=2))

    # ---- canned panels ----------------------------------------------------
    ms = canned_machine_status(hero, ledgers[hero["id"]])
    (fx / "panel" / "machine-status.json").write_text(json.dumps(
        panel_payload("machine-status", ["machine", "status"], ms, now_ms), indent=2))
    captured = []
    for cols in DOCTOR_COLS:
        doc = canned_doctor(home, cols)
        if doc:
            (fx / "panel" / f"doctor.{cols}.json").write_text(json.dumps(
                panel_payload("doctor", ["doctor"], doc, now_ms, cols), indent=2))
            captured.append(cols)
    if captured:
        (fx / "panel" / "doctor-widths.json").write_text(json.dumps(captured))

    live_ids = [sid for sid, plan in minted if plan.get("live")]
    (fx / "fleet-machines-live.json").write_text(json.dumps({
        # `specs` is what a REMOTE card renders as its hardware line
        # (cards.ts::specOf falls through to the beat for any machine that is
        # not the one /machine/specs describes). Without it every peer reads
        # "hardware not reported", which is the one thing a fleet screenshot
        # exists to show.
        "machines": [{
            "machine_uid": m["uid"], "display_name": m["id"],
            "schema_version": schema, "beat_ts_ms": now_ms,
            "darkmux_version": version,
            "specs": f"{m['cpu_brand']} · {m['ram_gb']} GB",
            "loaded_models": [f"darkmux:{r['model']}" for r in m["residents"]],
        } for m in world["machines"]],
        "meta": {"sources": {"fleet": {"state": "ok"}}, "complete": True},
    }, indent=2))
    (fx / "fleet-sessions-live.json").write_text(json.dumps({
        "sessions": [{"session_id": s} for s in live_ids],
        "meta": {"sources": {"fleet": {"state": "ok"}}, "complete": True},
    }, indent=2))

    live = [p for p in REPLAYS if p.get("live")]
    print(f"demo world built -> {out}")
    print(f"  flows      {sum(len(v) for v in by_day.values())} records "
          f"across {len(by_day)} day(s), {len(REPLAYS)} dispatches "
          f"({len(live)} live)")
    print(f"  machines   {', '.join(m['id'] for m in world['machines'])} "
          f"(hero: {hero['id']}, {hero['ram_gb']} GB {hero['cpu_brand']})")
    print(f"  panels     {', '.join(sorted(p.stem for p in (fx / 'panel').glob('*.json')))} canned; "
          f"the rest render from the demo home")
    print(f"\nnext:  ./serve.py")


if __name__ == "__main__":
    main()
