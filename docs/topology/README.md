# /topology — fleet observability diagram

Renders the darkmux fleet's shape from `FlowRecord` streams. Live or replay; same render pipeline either way.

## Modes

- **Fleet** (default) — orchestrators on the left, machines (with crew role count) in the middle, missions on the right.
- **Focus** — one machine deep-dive: crew roles and recent stages expanded to their own nodes.
- **Mission** — cross-cutting filter by mission_id; non-mission nodes dim, mission edges highlighted.

Switch modes via the pill row at the top.

## Data sources

| Source | How to load |
|---|---|
| Live | Default. Subscribes to `darkmux serve` on `http://127.0.0.1:8765/flow/<today>/stream`. Source pill reads `source: live`. |
| Fixture | `?fixture=fleet-3-machines` or `?fixture=single-machine-dispatch` — loads from `fixtures/<name>.jsonl`. |
| Drag-drop | Drop any `.jsonl` of flow records onto the canvas. |

## Replay scrubber

Only visible in replay mode (when a fixture is loaded). At the bottom of the canvas:

- ▸| — step to next record.
- Speed selector — pause / 1× / 4× / 16×.
- Range slider — drag to scrub through the fixture's ts range.
- Readout — current wall-clock + visible/total record count.

## Live cues

- **Edge motion** — dashed-stroke animation on edges whose most recent record is within the last 5 seconds.
- **Node pulse** — 2-second halo on nodes that just received a record.
- **Wall-clock arc** — SVG ring around a crew role node when a `dispatch start` has been seen but `dispatch complete` has not yet; shows elapsed seconds.

## Tests

Inline harness — open `?test=1` for the unit-test panel. Tests cover the pure transforms (`aggregateMachines`, `aggregateOrchestrators`, `aggregateMissions`, `aggregateRoles`, `deriveEdges`). Visual surfaces (modes, cues, scrubber) are verified manually + via Playwright snapshots during development.

## Composes with

- [#162](https://github.com/kstrat2001/darkmux/issues/162) — fleet-aware-darkmux epic; this is UI Step 3 / Phase 4.
- [#168](https://github.com/kstrat2001/darkmux/issues/168) — UI Step 1 plumbing (shared shell + tab nav).
- [#169](https://github.com/kstrat2001/darkmux/issues/169) — design spec.
- [#211](https://github.com/kstrat2001/darkmux/issues/211) — flow schema 1.6.0 (event types this renders).

## Known gaps

- Live SSE is single-machine (the daemon reads its local file sink, not Redis). True fleet-aggregated live mode lands when daemon gains a Redis-aware endpoint.
- Mobile responsive: usable below 720px but the diagram is densest at desktop widths.
