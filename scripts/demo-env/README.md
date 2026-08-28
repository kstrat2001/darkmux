# demo-env — the world the docs screenshots are shot from

A self-contained darkmux fleet that exists only to be photographed. Every
screenshot on darkmux.com should come from here rather than from a real
machine, for three reasons: a real capture carries the operator's hostnames,
tailnet addresses and workspace paths onto a public page; a real machine is
whatever hardware happens to be on the desk; and a real moment cannot be
re-shot after a UI change.

```bash
./build.py          # generate the world       -> ../../screenshots/
./serve.py          # serve it                 -> http://localhost:8799
node shoot.mjs      # photograph every lens    -> ../../screenshots/*.png
node shoot.mjs --mobile --only machine,fleet
```

`screenshots/` is gitignored. The INPUTS are committed — `world.json`,
`sessions/`, the three scripts — so anyone can regenerate the same world.

## How it works

The design principle is **fixture as little as possible.**

Most of what the viewer shows is DERIVED from flow records by the daemon
itself: `/runs`, `/missions`, `/phases`, `/flow-*`, the mission graph. Writing
those by hand would mean re-deriving in Python what `runs.rs` already derives
in Rust — two implementations of one rule, drifting from the first edit. So
`build.py` writes RECORDS into an isolated `DARKMUX_HOME`, `serve.py` runs a
REAL `darkmux serve` against it, and the derivations happen in the real code.

Only what a probe or a substrate would have to answer is overridden:

| route | why it cannot come from records |
|---|---|
| `/machine/specs`, `/machine/resources`, `/machine/status` | host probes (`vm_stat`, `sysctl`, `lms`). The demo machine is a 256 GB M5 Ultra; the machine you are on is not. |
| `/fleet/machines/live`, `/fleet/sessions/live` | presence rides Redis, which the demo deliberately does not run. |
| `/panel/doctor`, `/panel/machine-status` | these CLI verbs probe the host. Every OTHER panel (`run list`, `flow status`, `config list`, ...) is passed through and renders from demo data for free. |

`serve.py` also filters one record class out of `/flow/<date>`: the daemon
self-emits a `machine.online` at startup carrying the real host's hardware
uid (`presence_reconciler::emit_machine_online_edge`, unconditional — there is
no knob), which would otherwise appear as a fourth machine card duplicating
the hero.

## The world

`world.json` declares the fleet. Edit it and rebuild; every figure is DERIVED
from what you declare, so the parts can never disagree with the totals —
`build.py::ledger_for` computes KV-at-context, potential, current, pool and
projected totals using the rules documented on `MachineResources` in
`ui/src/types/handwritten.ts`.

Three machines, deliberately heterogeneous, because muxing across unlike
hardware is the product:

- `m5-ultra-256gb` — Apple M5 Ultra, 256 GB. The hero; four residents.
- `m1-max-32gb-studio` — Apple M1 Max, 32 GB. The always-on hub.
- `mac-mini-m4-16gb` — Apple M4, 16 GB. An edge node.

**This is illustrative data for documentation. It is not a measurement, and
nothing in it should be read as a benchmark result.**

## Where the records come from

`sessions/crawl-error-discard.jsonl` is a REAL dispatch — a crawler run over
darkmux's own `crates/darkmux-flow/src`, looking for discarded `Result`s —
imported by `import_session.py`, which strips identity and rewrites timestamps
to offsets from the session's own start.

Records replayed from a real emitter are shape-correct by construction. A
hand-written record can encode a shape the daemon could never produce, and the
only symptom is a pane that renders nothing — which reads as "my selector is
wrong", not "my fixture is wrong". This is the same reasoning behind
`wire_fixtures.rs`.

`build.py` replays that one session under ten identities across the three
machines, scaling the recorded token fields per replay. One replay is LIVE:
truncated at `now` and stripped of its terminal record, so the runs board
shows a RUNNING row and the detail lens shows a live pulse and a ticking
clock.

To refresh from a newer session:

```bash
./import_session.py ~/.darkmux/flows/<date>.jsonl <session-id> \
    --out sessions/<name>.jsonl
```

## The scrub is a guard, not a courtesy

`import_session.py` rewrites the known identity carriers (home directories,
tailnet IPs, MagicDNS names) and then SCANS the result, refusing to write the
fixture if anything on the forbidden list survived. `build.py` puts canned CLI
output through the same scan and omits a panel rather than shipping it dirty.

Both have already caught real leaks: `darkmux doctor` prints the daemon's
MagicDNS name, and a first-pass rewrite that matched only the `.ts.net` suffix
left the host label behind. Treat a refusal as correct and add a rewrite rule;
do not weaken the scan.

## Refreshing the published demo

`docs/demo/*` and the screenshots feeding `docs/media/*` are GENERATED, never
hand-edited. Run the four steps in order, from this directory unless noted:

```bash
./build.py                                            # 1. build the isolated world
./serve.py --port <free-port> &                       # 2. serve it (real daemon, isolated home)
./export_static.py --base http://127.0.0.1:<port>     # 3. capture into docs/demo/*
cd ../.. && bash scripts/build-demo.sh                 # 4. regenerate docs/demo/index.html
```

What each writes:

1. **`build.py`** — an isolated `DARKMUX_HOME` under `screenshots/demo-home`
   (flow records, the imported mission(s), config, profiles) plus the
   machine/panel fixtures under `screenshots/fixtures`. Never touches
   `~/.darkmux`; safe to run from any checkout, including a worktree.
2. **`serve.py`** — a REAL `darkmux serve` against that isolated home. Every
   route the daemon can answer from records is passed through untouched;
   `/machine/*` and `/panel/doctor`/`/panel/machine-status` are overridden
   from the fixtures `build.py` wrote (those probe THIS machine, not the
   demo one); every OTHER passthrough response is scrubbed live before it
   leaves the process (see `_scrub_passthrough`'s own doc — this is the
   backstop that catches a probe- or daemon-computed value the import-time
   scrub never saw, and it now covers every route this handler proxies, not
   only `/panel/*`).
3. **`export_static.py`** — `docs/demo/demo-panels.json`, `demo-machine.json`,
   `demo-missions.json`, `demo-phases.json`, `demo-graphs.json`,
   `demo-runs.json`, `demo-lab-runs.json`, `demo-flow.jsonl`. Every one of
   these is CAPTURED from the running daemon's own routes (`/panel/*`,
   `/machine/*`, `/missions`, `/phases`, `/mission/:id/graph.json`, `/runs`,
   `/lab/runs`, `/flow-days` + `/flow/:date`) — none is hand-authored, so
   none can independently go stale the way a hand-edited fixture can.
4. **`scripts/build-demo.sh`** — `docs/demo/index.html`, generated from
   whichever built viewer asset the daemon actually serves (never a second
   hardcoded source — see that script's own doc).

Screenshots (`docs/media/*.png`) come from `node shoot.mjs` (run from this
directory, against step 2's `serve.py`) for the lens shots it already knows
about, plus a targeted Playwright capture of `#mission=<id>` for anything
mission-graph-specific `shoot.mjs`'s generic ready-selector shooter doesn't
cover (a zoomed crop on one phase, for instance) — same Chrome-channel,
`deviceScaleFactor: 2`, dark-colorScheme conventions `shoot.mjs` itself uses,
so a screenshot the harness didn't generate still reads as one that would
have.

**Before committing, prove — don't assert — each of these:**

- [ ] every `ts` in `demo-flow.jsonl` is within about a day of the moment
      `build.py` ran (its clock, not whenever the file was last hand-touched):
      `python3 -c "import json,sys; from datetime import datetime,timezone; recs=[json.loads(l) for l in open('../../docs/demo/demo-flow.jsonl') if l.strip()]; print(min(r['ts'] for r in recs), max(r['ts'] for r in recs))"`
- [ ] no run in `demo-runs.json` has `"status": "running"` except the one
      `build.py`'s `REPLAYS` marks `live=True`, and that one's `started_ts` is
      minutes old, not days
- [ ] `demo-graphs.json` contains every mission under `missions/`, each with
      `"mission_status": "finalized"`
- [ ] `grep -rn "/Users/<you>\|<your-hostname>\|ts\.net" docs/demo/` returns
      nothing (broaden this per-machine — the four literal patterns are a
      floor, not the whole scan; a worktree checkout also leaks its own
      layout unless the repo-root replacement in `canned_doctor`/
      `_scrub_passthrough` catches it, which is exactly the class of leak
      that motivated adding it)
- [ ] any NEW or CHANGED screenshot gets LOOKED AT before committing, not
      just grepped — an image is opaque to every text-based sentinel guard
      the repo runs, `scripts/lib_vocab.py` included

Every mission and session in this world is a REAL run of darkmux's own public
code — imported via `import_session.py`/`import_mission.py`, which strip
identity and re-anchor timestamps at import time — never a flow record or
mission JSON typed by hand. That is the whole design principle this file
opens with ("fixture as little as possible"): a hand-written record can encode
a shape the daemon could never produce, and the only symptom is a pane that
renders nothing, which reads as "my selector is wrong" rather than "my
fixture is wrong." Refreshing the demo is re-running real code against a
real (if isolated) daemon, never re-typing its output.

**Cadence:** the demo is refreshed at RELEASE, not continuously — the
`darkmux-point-release` skill runs the four steps above (and their proof
checklist) as a standard step in every version-bump PR, so darkmux.com/demo
never reads as more than one release behind main. A demo that rebuilt on
every commit would recapture screenshots for work nobody has shipped yet;
release cadence is the point where "what the demo shows" and "what a
`brew install`ed operator actually gets" are supposed to agree.

## Known rough edge

`darkmux doctor` reports on the machine it RUNS on, so the canned panel carries
host-level findings (daemon freshness, installed skills) that describe your
machine rather than the demo one. The demo home isolates what it can — flows,
crew, profiles, audit dir — but a host probe is a host probe. Prefer
`console-runs` as the console screenshot, or hand-author a doctor fixture if
the docs need a specific posture.
