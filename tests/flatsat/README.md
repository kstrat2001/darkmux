# Flatsat (Packet 0b)

A two-machine darkmux fleet on the bench — `hub` + `peer` + real `redis`,
via docker-compose — so the viewer port can be tested against LIVE
daemons and injected failures, not just the static parity corpus (Packet
0a, `tests/parity/`, which this reuses read-only for seed data). Needed
before the SSE/live lens packet lands; not needed for static lens ports.

Self-contained: its own `package.json`/bun scripts, its own compose
project (`darkmux-flatsat`), its own ports. Never touches `tests/parity/`
or `tests/e2e/`, and never the operator's real daemon (port 8765).

## One-command usage

```bash
cd tests/flatsat
bun install            # once
bun run check           # up -> scenarios -> down, always tears down
```

Or drive the phases separately:

```bash
bun run up               # build image (once) + seed + start hub/peer/redis
bun run scenarios        # playwright test (fleet already up)
bun run down              # tear down containers + network
./inject.sh status       # one-line status per container
./inject.sh pause-peer   # failure injection — see inject.sh --help-shaped usage comment
```

Ports (never 8765): hub `18765`, peer `18766`, redis `16379`.

## Why the binary is built in a Dockerfile, not bind-mounted

The packet brief says "bind-mount `target/release/darkmux`". On every
machine this project's own fleet doctrine describes (Macs on a tailnet),
that binary is Mach-O; the compose services run linux/arm64 (Docker
Desktop's VM). A Mach-O binary cannot execute under a Linux kernel — no
host in this project ever produces a container-runnable binary directly.
`Dockerfile` compiles the workspace's `darkmux` binary via a `rust:alpine`
builder stage (mirrors `runtime/Dockerfile`'s own shape) into ONE image
tag (`darkmux-flatsat:local`) that both `hub` and `peer` reference —
`up.sh` builds it exactly once, preserving the brief's actual intent
(build once, not per-service).

## The six scenarios

| Scenario | File | Incident lineage | Verdict |
|---|---|---|---|
| fleet-visible | `a-fleet-visible.spec.ts` | #1705 — a peer's mission activity was invisible on the hub | **pass** |
| redis-blip | `b-redis-blip.spec.ts` | coordination substrate drops mid-view | **pass** (asserts the hub's own local, non-Redis-dependent history survives) |
| peer-asleep | `c-peer-asleep.spec.ts` | liveness must not show a dead machine "running" forever | **`test.fixme`** — architecture gap, not a viewer bug (see below) |
| daemon-restart | `d-daemon-restart.spec.ts` | #1709 — the black-screen class | **pass** |
| empty-vs-idle | `e-empty-vs-idle.spec.ts` | a zero-record fleet must render its empty state cleanly | **pass** |
| stream-at-cap | `f-stream-at-cap.spec.ts` | the shared flow stream at its `MAXLEN` cap | **pass** |

Files are lettered so Playwright's default alphabetical run order matches
dependency order: `f-stream-at-cap` floods the shared Redis stream past
its 10k cap and MUST run last, since `MAXLEN ~` trims the OLDEST entries
first — the hub/peer seed records `a-fleet-visible` depends on seeing.

**peer-asleep is `test.fixme`, honestly, for an architecture reason found
while building this scenario:** `darkmux_hardware::machine_uid()`
(`crates/darkmux-hardware`) returns `None` on any non-macOS OS (it shells
out to `ioreg`), which self-disables the presence heartbeat
(`crates/darkmux-flow/src/presence.rs::spawn_emitter_thread`). Every
flatsat container runs Linux, so `/fleet/machines/live` is permanently
`[]` — there is no live heartbeat to pause-and-watch-expire in a
container, on any machine, ever. The spec still exercises the honest proxy
available (flow-record attribution stays static while the peer is frozen)
and documents the gap inline. Real fix shape: a non-macOS `machine_uid`
fallback (e.g. `/etc/machine-id`) so presence has something to key on off
a Mac — named as a follow-up, not fixed here (out of this packet's scope).

Every scenario asserts render-sanity (main `#stage` region visible with
non-trivial height, no horizontal document scroll at phone width, zero
pageerror/console-error events) and drops a full-page PNG at
`scratchpad/ui-port-gallery/0b-flatsat/<scenario>.png` for the operator's
morning review — screenshots are not committed to the repo.

## Seeding (`seed/seed.mjs`)

The hub gets the FULL parity corpus (`tests/parity/corpus/*.json`) reused
read-only, with every timestamp shifted by a whole-day delta so "today"/
"yesterday" land on the real current date (see `lib/timeshift.mjs`) and
`machine_id`/`machine_uid` rewritten to `flatsat-hub`. The peer gets a
small hand-authored fixture (`seed/peer-fixture.mjs`) with its own
distinct mission ids, so `a-fleet-visible` has genuinely different content
to prove is reachable from the hub, not a copy. Both machines' records are
also `XADD`-ed to the shared Redis stream (mirroring `TeeSink`), which is
how the peer's work becomes visible from the hub's aggregated views.

Every byte seed.mjs writes is re-scanned by the parity harness's OWN
tripwire (`tests/parity/lib/sanitize.mjs`'s `scanForSentinels`, imported
directly — not reimplemented) before `up.sh` starts the daemons; a hit
wipes `.state/` and aborts.
