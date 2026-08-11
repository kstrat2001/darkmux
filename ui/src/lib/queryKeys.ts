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
/** `LAB_POLL_STEADY_MS`/`LAB_POLL_BACKFILL_MS` (viewer.html:4020-4021) — the
 * lab-run detail's event-feed poll cadence (`LabRunDetail.tsx`): steady
 * once caught up, rapid while draining a backlog (a big historical run's
 * playback). Not wired to a `refetchInterval` query — see that component's
 * own doc for why the poll is a manual self-rescheduling loop instead. */
export const LAB_POLL_STEADY_MS = 3_000;
export const LAB_POLL_BACKFILL_MS = 150;
/** How many CONSECUTIVE `/lab/run/events` poll failures (thrown fetch or a
 * non-2xx response — see `fetchJson`'s discriminated result) before
 * `LabRunDetail` flips its badge/header to a visible "daemon unreachable"
 * state, per the merge-gate finding that a raw silent-catch made a dead
 * daemon byte-identical to an idle run. N=3: at the steady 3s cadence
 * that's ~9s of confirmed silence — long enough that one dropped request
 * (a transient blip) doesn't flap the UI, short enough that an operator
 * watching a live run over the tailnet notices within a couple of refresh
 * cycles rather than staring at a frozen "● live" feed. Resets to 0 on the
 * next successful poll. */
export const LAB_POLL_FAILURE_THRESHOLD = 3;

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
  // (Packet 4) The playback catalog (`#691`) — day/mission history browser.
  flowDays: () => ["flow", "days"] as const,
  flowMissions: () => ["flow", "missions"] as const,
  flowMission: (id: string) => ["flow", "mission", id] as const,
  flowSession: (id: string) => ["flow", "session", id] as const,
  /** `GET /lab/run/detail?dir=` — the lab-run detail view's one-shot fetch
   * (`LabRunDetail.tsx`). The event-feed poll (`/lab/run/events`) is NOT a
   * react-query key — see that component's own doc. */
  labRunDetail: (dir: string) => ["lab", "run", "detail", dir] as const,
};

/** How often a live view re-checks whether the UTC day has rolled over.
 *  Same 5s cadence legacy's live poll used for the same check — a named
 *  constant rather than a literal, per the recorded-knob doctrine. */
export const DATE_ROLLOVER_CHECK_MS = 5000;
