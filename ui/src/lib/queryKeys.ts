/**
 * The queryKey registry — every `useQuery` call in this app imports its key
 * from here rather than hand-rolling an array literal at the call site, so a
 * key never drifts between two components that mean to share a cache entry.
 *
 * Cadences are DOCUMENTED, not adaptive-silent (the observability doctrine in
 * the root `CLAUDE.md`'s "cadence is a recorded knob" section) — each
 * `refetchInterval` below is copied from the legacy `viewer.html`'s own
 * polling constants, named at the source line that set the precedent:
 *
 * | Cadence constant           | ms     | Legacy source (`viewer.html`)                    |
 * |-----------------------------|--------|---------------------------------------------------|
 * | `PRESENCE_POLL_MS`           | 5000   | `LIVE_SESS_TIMER=setInterval(...,5000)` — the     |
 * |                              |        | fleet/sessions/machines + machine/specs poll.     |
 * | `RECONCILE_BACKSTOP_MS`      | 20000  | Every 4th presence tick (`tick%4===0`,            |
 * |                              |        | `reconcileLiveWindow()`) — backstops a dropped SSE|
 * |                              |        | reconnect gap. Not wired to a query in this       |
 * |                              |        | packet (no SSE-fed query exists yet); recorded    |
 * |                              |        | here for the live/SSE lens packet to pick up.     |
 * | `MACHINE_RESOURCES_CACHE_MS` | 2000   | Server-side cache TTL                             |
 * |                              |        | (`MACHINE_RESOURCES_CACHE_TTL` in                 |
 * |                              |        | `crates/darkmux-serve/src/lib.rs`) — the daemon   |
 * |                              |        | itself won't produce a fresher answer than this,  |
 * |                              |        | so polling faster than it wastes a round trip.    |
 * | `MACHINE_MEM_POLL_MS`        | 5000   | `MACHINE_MEM_POLL_MS` — the machine lens's client |
 * |                              |        | poll cadence for `/machine/resources`.            |
 * | `PANEL_CACHE_MS`             | 3000   | `PANEL_CACHE_TTL` in                              |
 * |                              |        | `crates/darkmux-serve/src/panel.rs` — server-side |
 * |                              |        | cache TTL for auto-refreshing console panels.     |
 *
 * Only `PRESENCE_POLL_MS` is wired to an actual query in this scaffold packet
 * (the fleet-machines strip, the packet's one proof region) — the rest are
 * recorded so a lens packet doesn't have to re-derive them from the legacy
 * source.
 */
export const PRESENCE_POLL_MS = 5_000;
export const RECONCILE_BACKSTOP_MS = 20_000;
export const MACHINE_RESOURCES_CACHE_MS = 2_000;
export const MACHINE_MEM_POLL_MS = 5_000;
export const PANEL_CACHE_MS = 3_000;

export const queryKeys = {
  fleetMachinesLive: () => ["fleet", "machines", "live"] as const,
  fleetSessionsLive: () => ["fleet", "sessions", "live"] as const,
  runs: () => ["runs"] as const,
  labRuns: () => ["lab", "runs"] as const,
  machineSpecs: () => ["machine", "specs"] as const,
  machineResources: () => ["machine", "resources"] as const,
  panel: (id: string) => ["panel", id] as const,
  flowTail: (date: string) => ["flow", date, "tail"] as const,
  /** `GET /flow/<date>` — the full day's records (distinct from `flowTail`'s
   * SSE stream key above). Consumed by `useFlowWindow` (`hooks/
   * useFlowWindow.ts`), which fetches yesterday+today per `loadLiveWindow()`
   * (viewer.html:3497) — see `lib/flow.ts`'s module doc for the fetch-order
   * subtlety that makes the two-day merge order load-bearing. */
  flowDate: (date: string) => ["flow", date] as const,
};
