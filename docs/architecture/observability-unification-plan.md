# Observability unification plan

**Status:** planned · **Epic:** [#556](https://github.com/kstrat2001/darkmux/issues/556) · **Drafted:** 2026-06-01

One stream, one drill-down viewer, one recording format. This doc is the *why* behind the
[#556](https://github.com/kstrat2001/darkmux/issues/556) arc; the issues carry the *what*.

---

## The problem

darkmux's observability surface is three viewers (`topology`, `flow`, `lab`) plus a redirect
(`viewer` → `lab`), backed by **two** recording formats:

- the **flow stream** — always-on `FlowRecord`s under `~/.darkmux/flows/` (+ optional Redis, +
  optional audit), served live by `darkmux serve` at `/flow/<date>` and `/flow/<date>/stream`;
- per-run **`instruments.jsonl`** — `lab run --instrument` telemetry samples
  (`source: lms|process|meta`), read by the `lab` viewer via **file-drop only** (no daemon path).

That structure produces three visible failures:

1. **Broken on darkmux.com.** Every viewer hardcodes `http://127.0.0.1:8765`. Served from
   `https://darkmux.com`, those fetches are blocked twice over — CORS (foreign origin) **and**
   mixed-content (HTTPS→HTTP). The topology page renders a blank canvas; `lab` shows console
   errors and a "no daemon" pill it has no use for. (Originally [#553](https://github.com/kstrat2001/darkmux/issues/553).)
2. **Can't see your fleet.** No URL a fleet operator can open that just works — darkmux.com can't
   reach their daemon, and `file://` only sees one machine. (Originally [#554](https://github.com/kstrat2001/darkmux/issues/554).)
3. **Two recordings that don't compose.** Instrument detail can't go live and can't be viewed
   without dragging a second file with no association to the flow records it belongs to.

These are not three bugs. They are one architecture problem wearing three faces.

## The root fault

The design conflates **deployment context** (where the page is served from) with **data source**
(where its data comes from), and it grew **two recording formats** for **one subject** — *work
getting done by your AI fleet*.

`topology` and `flow` read the *same* records (the flow stream) — one is the map, one is the log.
`lab` reads a *different* shape (per-run instrument samples) — but it isn't a different kind of
thing; it's the **deepest zoom**: one run's internals. The three are lenses on one subject, wired
as three separate pages that each independently dial the daemon, with the deepest lens stuck on a
second file format.

### How we got here

| When | What |
|---|---|
| **2026-05-10** | `lab run --instrument` + `instruments.jsonl` — the instrument/telemetry capability and its viewer. |
| **2026-05-13** | the flow stream + flow viewer — built without lab in mind. |
| later | flow's extensible event model (the `payload` JSON map, [#204](https://github.com/kstrat2001/darkmux/issues/204), schema 1.6). |

Instruments predate the flow stream by three days, **and** predate the `payload` mechanism that
would have absorbed them. The split is historical, not architectural — and the `FlowRecord` schema
proves it (below). The `lab` viewer file looks newer than `flow` only because it was renamed from
`/viewer/` to `/lab/` on 2026-05-17 ([#171](https://github.com/kstrat2001/darkmux/issues/171)).

## The unified design

### 1. One stream — telemetry as a flow event family ([#557](https://github.com/kstrat2001/darkmux/issues/557))

Fold per-run telemetry into the flow stream as `FlowRecord`s. The schema is already shaped for it:

```
FlowRecord { ts, level, category, tier, stage, action, handle,
             sprint_id?, session_id?, source?, model?, mission_id?, machine_id?,
             orchestrator?, prev_hash?, hash?, payload?, work_id?, attempt? }
```

- `source` is the *same* field instrument samples already use (`lms`/`process`/`meta`).
- `payload` is a JSON map added in schema 1.6 ([#204](https://github.com/kstrat2001/darkmux/issues/204))
  whose documented purpose is *"give new event types a place to carry their event-specific fields
  without growing the struct"* — i.e., exactly an instrument sample.

A telemetry record carries the dispatch's `session_id`/`mission_id`, so it associates with the work
it describes **for free**, in **one timestamped sequence**. No second file, no association problem,
no playhead sync, and — because there is only ever one sequence — **no two-source race**.

**Always-on.** The `--instrument` flag is removed; every run is instrumented by default. The packets
are small and the always-available audit detail strengthens the flow stream's role as the audit
substrate. This also retires the earlier worry that the subsystem view is "inaccessible without
instrumentation" — the data is always there.

Retiring `instruments.jsonl`: pre-1.0 + small audience → clean removal, **no migration shim**.

Schema impact: **minor** flow-schema bump (additive event family; older viewers safely ignore
telemetry records — they never gate on the major). The eureka/anomaly rules engine ports to
evaluate over telemetry records in the stream; `RULES_SCHEMA_VERSION` is unaffected.

### 2. One viewer — drill, not pages ([#558](https://github.com/kstrat2001/darkmux/issues/558))

A single app over the one stream:

- **Drill: fleet → machine → subsystem.** Fleet = machines as nodes (today's topology). Machine =
  what's running on it. Subsystem = today's `lab` run-internals view, now reading telemetry records
  from the stream.
- **The log is a lens**, not a fourth page — today's `flow` view, scoped to whatever you've drilled
  into.
- **Live vs playback are visually distinct.** Live = subscribe to `/flow/<date>/stream`, current
  theme — "what's happening now." Playback = one recorded stream loaded, **amber** theme — "a movie
  of something that already happened."

Because live has exactly **one** source, the cross-tab desync and two-source race concerns dissolve
by construction. The metadata endpoints (`/machine/specs`, `/missions`, `/model/status`) are
lookups that enrich a node, not a second sequence — nothing to interleave.

Retire `docs/{topology,flow,lab,viewer}/index.html` as separate pages.

### 3. The demo is a playback session ([#559](https://github.com/kstrat2001/darkmux/issues/559))

darkmux.com hosts the unified viewer as a **playback** of a curated fixture — one flow stream that
includes telemetry records, coherent at every drill level by construction. Amber theme, a persistent
**"Demo - sample data"** badge, **no daemon fetch, no daemon chrome**. The home page links are
labeled as demo. No origin-sniffing magic; the demo is explicitly a demo, not the working tool.

### 4. The daemon hosts the viewer ([#554](https://github.com/kstrat2001/darkmux/issues/554))

`darkmux serve` serves the one viewer at its own origin (`include_str!`), so single-machine
(`http://localhost:8765/`) and fleet (`https://hub.tailnet.ts.net/` via Tailscale Serve) are both
**same-origin** — CORS/mixed-content impossible by construction. darkmux.com is the frozen demo;
the operator's working tool is only ever reached by running the tool. Docs name the three URL
contexts ([#555](https://github.com/kstrat2001/darkmux/issues/555)).

## "Drop the lab" — scope

Drop the separate **format** (`instruments.jsonl`), the separate **viewer app**, and the separate
data path. **Keep the capability** — the subsystem/run-internals view (model loads, process activity,
and the eureka/anomaly detection, sequenced under the observability theme, #557), now a drill-lens over telemetry
records in the one stream. Kill the debt, keep the analysis.

## Why this kills the whole bug class

- **CORS / mixed-content:** impossible — the viewer only ever fetches its own origin (daemon-hosted)
  or loads a bundled fixture (demo). It never addresses a foreign origin.
- **Cross-tab desync:** impossible — one app, one loaded session; no second tab to go stale.
- **Two-source live race:** impossible — live is single-source; telemetry and dispatch events share
  one sequence.
- **"Inaccessible without instrumentation":** gone — always-on instrumentation.
- **Demo confusion:** gone — the demo is an explicit, badged playback, structurally separate from the
  daemon-hosted tool.

## Rejected alternatives

- **Daemon CORS allowlist for darkmux.com** — mixed-content blocks first; can't work over HTTPS→HTTP.
- **Tailscale Funnel** (expose the daemon publicly) — violates the privacy posture.
- **Origin-sniffing adaptive viewer** (same page, demo-or-live by hostname) — blurs demo and tool
  when an operator holds both in tabs; a banner is too weak a signal. The demo must be a *separate*,
  explicit thing.
- **Keep two formats and "bridge" them in the viewer** (drag two files / associate / sync playheads)
  — the tech debt itself; opens the edge cases this plan removes.

## Open tunables (not blockers)

- **Instrument cadence / volume.** Always-on telemetry needs a default sample interval; operator-
  overridable. Keep packets small.
- **Playback accent hue.** Warm amber/sepia against the live cyan — final value picked by looking at
  it locally.
- **Routing disambiguation** for the daemon-hosted viewer: `/flow/` (HTML) vs `/flow/<date>` (API).
  Confirm axum handles the path-shape split cleanly before designing around it (see #554).

## Sequencing

1. **#557** — unified recording (telemetry → flow event family; always-instrument; retire the old
   format). Foundation; defines the schema the fixture and viewer target.
2. **#558** — the one drill-down viewer. Built to be run locally and iterated on by looking.
3. **#559** — the demo (a playback fixture of #558) ships on darkmux.com. *Can land once #557's
   schema is defined and #558's playback path works — before the backend rewire is fully done.*
4. **#554** — daemon hosts the unified viewer.
5. **#555** — fleet-mode docs.

## Principle

Operator sovereignty + KISS: one recording, one viewer, an explicit live/playback distinction, no
magic origin detection, no second file to associate, no compat baggage pre-1.0. The destination is a
single drillable view of the fleet; the demo is just a playback session of it.
