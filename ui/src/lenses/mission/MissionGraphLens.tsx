/**
 * The mission-graph lens (#1868, events display unified into the mainstay
 * column — see the operator finding below) — folds `crates/darkmux-serve/
 * assets/mission-graph.html` (the standalone `/mission/:id/graph` page)
 * into the React port as `#mission=<id>`. Reads `graph.ts`/`timeline.ts` for
 * the pure logic; `MissionCanvas.tsx`/`MissionTimelineView.tsx` for the two
 * body renderers. This lens owns the mission-events DATA pipeline (see this
 * module's own data-source doc below for why there is still only ONE SSE
 * consumer in this app) but no longer renders its own `EventLogColumn` —
 * it reports the scoped, deduped list via the `onEvents` prop instead, and
 * `App.tsx` feeds it into the SAME shared/mainstay column every other route
 * already uses.
 *
 * (operator finding, post-#2107/#2108) The `evOpen`/"events" toggle button
 * and this lens's own inline `EventLogColumn` mount are RETIRED. They
 * predate the tabbed-drawer packet that made the events column "a
 * collapsible mainstay on all tabs" (`lib/route.ts`'s own `showsEventLog`
 * doc) everywhere else — mission was the one route still carrying its own
 * separate, collapsed-by-default events chrome, which regressed into a
 * real bug once the mainstay column existed everywhere else too: the phone
 * drawer's universal Events tab showed a nonzero record count (fed
 * unscoped, route-generic records) but rendered BLANK for a mission route,
 * because `showsEventLog` deliberately excluded `mission` to avoid two
 * DIFFERENT-scope event logs disagreeing on one page (#1868's original
 * concern) — leaving mobile with neither surface actually working. Lifting
 * this lens's own already-correctly-scoped `events` upward instead of
 * displaying them here resolves both at once: one display surface again,
 * this time fed the right data everywhere.
 *
 * Data sources, and why each is a REUSED cache slot rather than a new fetch
 * pipeline:
 *
 * - `GET /mission/:id/graph.json` — the initial node/edge snapshot. New
 *   query key (`queryKeys.missionGraph`), nothing else in this app reads it.
 *   **Daemon-backed builds only** (`daemonBacked`, below). On a static build
 *   (#2032 packet 2) this is replaced entirely by `getSource().graphs`'s
 *   committed `{"<mission-id>": <graph>, ...}` fixture
 *   (`queryKeys.staticGraphs`) — one fetch for every mission this build
 *   knows about, this mission's own entry looked up client-side. No flow
 *   backfill, no live tail, no SSE: a static build renders exactly the
 *   snapshot the fixture captured, same as `MachineLens.tsx`'s
 *   `getSource().machine` precedent.
 * - `GET /flow-mission/:id` — the mission's cross-day flow-record backfill.
 *   SAME `queryKeys.flowMission` key `MissionReplay` used to fetch before
 *   this lens replaced it (retired this packet).
 * - `GET /flow/<today>` — today's whole day file, needed because the
 *   scheduler does NOT stamp `mission_id` on step-lifecycle records (see
 *   `mission-graph.html`'s own `backfillEvents` comment) — `/flow-mission`
 *   alone misses them. SAME `queryKeys.flowDate` key `useFlowWindow` (always
 *   mounted at `App.tsx`) already populates — TanStack dedupes, no second
 *   request.
 * - Live tail: this component mounts its OWN `useLiveTail(true)` — the
 *   SHARED SSE primitive (`lib/sse.ts`/`hooks/useLiveTail.ts`), the same one
 *   `App.tsx` mounts for the fleet window, feeding the SAME
 *   `queryKeys.flowTail(date)` cache shape. `App.tsx` never runs its own
 *   copy for THIS route (`isLiveRoute` still excludes `mission` — the
 *   fleet-wide rolling window `useFlowWindow` feeds is not what this lens
 *   needs), so there is exactly one open EventSource while this lens is
 *   mounted, never two.
 *
 * All three record sources are folded through the SAME pure functions
 * (`recordInMission`/`applyFlowRecord`/`applyRecordToMetrics`) regardless of
 * which endpoint they came from — mission-graph.html's own design (backfill
 * and live records share one `recordInMission` filter and one metrics
 * accumulator); this port keeps that by re-deriving from the full known
 * record set on every change (a pure fold) rather than the legacy page's
 * imperative per-record `setState` reducer — see `graph.ts`'s own module doc.
 */
import { useEffect, useMemo, useState } from "react";
import { useQuery, useQueryClient, skipToken } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys, RECONCILE_BACKSTOP_MS } from "../../lib/queryKeys";
import { getSource } from "../../lib/source";
import { useLiveTail } from "../../hooks/useLiveTail";
import { asRecordArray, bodyTruncated, todayUTC } from "../../lib/flow";
import { MissionCanvas } from "./MissionCanvas";
import { MissionTimelineView } from "./MissionTimelineView";
import {
  applyRecordToMetrics,
  fmtTok,
  foldFlowRecords,
  indexGraph,
  missionTotals,
  normalizeMissionStatus,
  recordInMission,
  seedMetricsFromGraph,
  statusRank,
  type MetricsMap,
  type MissionGraph,
} from "./graph";
import { initMinimap, isNarrowViewport, persistMinimap, timelineActive } from "./timeline";
import type { FlowRecord } from "../../types/handwritten";

/** Dedup key — this port's counterpart to mission-graph.html's own
 * `backfillEvents` dedup (`ts + action + handle`): two sources
 * (`/flow-mission/:id` and `/flow/<today>`) can carry the same record, and
 * live SSE can race a backfill that just landed.
 *
 * WIDER than legacy's own key on purpose. Legacy's `ts+action+handle` key
 * is only ever applied within `backfillEvents`, which dedupes a fetched
 * record against rows ALREADY HELD — a narrow, events-panel-only concern.
 * This port folds the SAME deduped `allRecords` set into BOTH the events
 * pane AND the per-step metrics accumulator (`applyRecordToMetrics`, which
 * legacy never dedupes at all — every SSE record is folded exactly once,
 * unconditionally). Two genuinely DIFFERENT records sharing one
 * `ts+action+handle` are common in test fixtures (and can arise for real
 * — e.g. two sibling seats erroring in the same wall-clock second) and are
 * disambiguated by `session_id`/`mission_id` in practice; the full
 * `JSON.stringify` is the safe general case: two records are "the same"
 * only when EVERY field agrees, which is what "the same JSON object,
 * fetched twice" actually means. Caught live, not theorized: an early
 * version of this key (`ts+action+handle` alone) silently collapsed two
 * same-mission-id-less test records issued in the same second into one,
 * dropping real tokens from the metrics fold. */
function recKey(r: FlowRecord): string {
  return JSON.stringify(r);
}

/** `lookupOwningMachine` — mission-graph.html. Best-effort: which MACHINE a
 * mission ran on, when this daemon can't render its graph (a peer's local
 * disk holds the mission, this daemon's flow stream still carries its
 * records with `machine_id` stamped). */
async function lookupOwningMachine(missionId: string): Promise<string | null> {
  const res = await fetchJson<unknown>(`/flow/${todayUTC()}`);
  if (!res.ok) return null;
  const rows = asRecordArray(res.data);
  for (let i = rows.length - 1; i >= 0; i--) {
    const rec = rows[i];
    if (rec && rec.mission_id === missionId && rec.machine_id) return rec.machine_id;
  }
  return null;
}

function useNow(anyRunning: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!anyRunning) return;
    setNow(Date.now());
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [anyRunning]);
  return now;
}

/** (#1483) How recent a `telemetry.process` sample must be (client receive
 * time) to still count as fresh — mission-graph.html's own literal `12000`. */
const PROC_FRESH_MS = 12000;

interface ProcSample {
  cpu?: number;
  gpu?: number;
  mem?: number;
  /** Our OWN receive wall-clock, not the record's `ts` — see this hook's
   *  own doc for why (host/client clock skew). */
  rx: number;
}

function isTelemetryProcessRecord(r: FlowRecord): boolean {
  return (
    (r.action === "telemetry.process" || (r.category === "telemetry" && r.source === "process")) &&
    !!r.payload &&
    typeof r.payload === "object"
  );
}

/** (#1868, #1483) The header's `.mproc` host-activity readout — the OFF-
 * MODEL corroboration that the box is actually working (host kernel
 * counters, zero model dispatches — observability doctrine), not chrome.
 * Ported from mission-graph.html's own `proc`/`setProc` state, which its
 * SSE `onMessage` stamps directly: `rec.action==="telemetry.process" ? setProc({...,
 * rx: Date.now()}) : ...`.
 *
 * This port has no per-record `onMessage` callback (the pure-fold
 * architecture re-derives from the whole known record set on every change —
 * see this module's own doc), so "stamp OUR OWN receive time" is reproduced
 * via a `useEffect` keyed on the IDENTITY of the latest qualifying record:
 * `flowTailQuery.data` only grows (new SSE records append; `mergeTailRecords`
 * never re-clones an existing entry — see that function's own doc), so the
 * object reference selected as "latest" changes ONLY when a genuinely NEW
 * sample streams in. The effect then fires once, for that sample, at
 * (very nearly) the moment it actually arrived — same invariant legacy's
 * inline stamp gives, reached a different way because this port's data flow
 * is a fold, not a per-message reducer.
 *
 * MACHINE-level, not mission-scoped, matching legacy exactly: `tail` is the
 * live tail UNFILTERED by mission (`recordInMission` is never applied to
 * this source) — a host sample corroborates the whole box, not one mission. */
function useProcReadout(tail: FlowRecord[] | undefined): ProcSample | null {
  const latest = useMemo(() => {
    if (!tail || !tail.length) return null;
    let found: FlowRecord | null = null;
    for (const r of tail) {
      if (!r || typeof r !== "object" || !isTelemetryProcessRecord(r)) continue;
      if (!found || (r.ts ?? "") >= (found.ts ?? "")) found = r;
    }
    return found;
  }, [tail]);

  const [proc, setProc] = useState<ProcSample | null>(null);
  useEffect(() => {
    if (!latest || !latest.payload) return;
    const p = latest.payload as { cpu?: unknown; gpu?: unknown; mem?: unknown };
    setProc({
      cpu: typeof p.cpu === "number" ? p.cpu : undefined,
      gpu: typeof p.gpu === "number" ? p.gpu : undefined,
      mem: typeof p.mem === "number" ? p.mem : undefined,
      rx: Date.now(),
    });
  }, [latest]);
  return proc;
}

function ProcEl({ proc }: { proc: ProcSample | null }) {
  const fresh = !!proc && Date.now() - proc.rx < PROC_FRESH_MS;
  if (!fresh || !proc) return null;
  return (
    <span className="mproc" title="host activity (telemetry.process)">
      {typeof proc.gpu === "number" ? (
        <span>
          gpu <b className={proc.gpu >= 60 ? "hot" : ""}>{proc.gpu}%</b>
        </span>
      ) : null}
      {typeof proc.cpu === "number" ? <span>cpu {proc.cpu}%</span> : null}
    </span>
  );
}

function useIsMobile(): boolean {
  const [isMobile, setIsMobile] = useState(() => (typeof window !== "undefined" ? isNarrowViewport(window.innerWidth) : false));
  useEffect(() => {
    const onResize = () => setIsMobile(isNarrowViewport(window.innerWidth));
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);
  return isMobile;
}

function legendDots() {
  const entries: Array<[string, string]> = [
    ["var(--dim)", "planned"],
    ["var(--ml-run)", "running"],
    ["var(--good)", "complete"],
    ["var(--bad)", "error"],
    ["var(--dim)", "abandoned"],
  ];
  return entries.map(([color, label]) => (
    <span key={label}>
      <span className="dot" style={{ background: color }} />
      {label}
    </span>
  ));
}

function MeterEl({ tot }: { tot: ReturnType<typeof missionTotals> }) {
  if (!tot.total && !tot.turns) return null;
  const attributed = tot.cloud || tot.unknown;
  return (
    <span className="mmeter">
      {tot.total ? <b>{fmtTok(tot.total) + " tok"}</b> : null}
      {tot.total && attributed ? (
        <span className="split">
          {fmtTok(tot.local) + " local"}
          {tot.cloud ? (
            <span>
              {" + "}
              <span className="cloud">{fmtTok(tot.cloud) + " cloud"}</span>
            </span>
          ) : null}
          {tot.unknown ? <span className="unknown">{" + " + fmtTok(tot.unknown) + " unattributed"}</span> : null}
        </span>
      ) : null}
      {tot.turns ? <span className="split">{"· " + tot.turns + (tot.turns === 1 ? " turn" : " turns")}</span> : null}
    </span>
  );
}

export function MissionGraphLens({
  missionId,
  onEvents,
}: {
  missionId: string;
  /** (#2107/#2108 mainstay unification) Reports this mission's own scoped,
   * deduped, display-ordered event list every time it changes — `App.tsx`
   * lifts it into the SAME shared/mainstay `EventLogColumn` every other
   * route already uses (desktop's standalone mount, and the phone drawer's
   * Events tab), rather than this lens rendering its own separate copy.
   * `events`/`srvTruncated` stay computed HERE (this component still owns
   * the mission-events data pipeline — the flow-mission/flow-today/live-tail
   * fold below is unchanged), only the DISPLAY moved out. Optional so
   * existing test call sites that don't care about events keep working.
   * See the removed `evOpen`/`evbtn`/inline `<EventLogColumn>` this
   * replaces — superseded now that the mainstay column can show a
   * mission's events on its own, closing the "two event logs disagreeing
   * about scope" gap #1868 used to require a second, bespoke surface for. */
  onEvents?: (events: FlowRecord[], srvTruncated: boolean) => void;
}) {
  const queryClient = useQueryClient();
  const source = getSource();
  const daemonBacked = source.kind === "daemon";
  const today = todayUTC();

  // (#1628 lineage) `refetchInterval` — the safety-net reconcile poll for a
  // disk-level status delta that has NO accompanying flow record (e.g. an
  // out-of-band write to the mission's state file). Folding the known flow
  // record set (`foldFlowRecords`, below) replays every STATUS TRANSITION
  // this page has ever heard of, which already self-heals a REGRESSED
  // snapshot value (a lagging disk write reporting a stale status the fold
  // then re-advances past) — but it cannot invent a transition that arrived
  // only as a fresh snapshot value, never as a record. Same 20s cadence
  // mission-graph.html's own `setInterval(reconcile, 20000)` used — see
  // `RECONCILE_BACKSTOP_MS`'s own doc in `queryKeys.ts`.
  const graphQuery = useQuery({
    queryKey: queryKeys.missionGraph(missionId),
    queryFn: () => fetchJson<MissionGraph>(`/mission/${encodeURIComponent(missionId)}/graph.json`),
    enabled: daemonBacked,
    refetchInterval: daemonBacked ? RECONCILE_BACKSTOP_MS : false,
  });

  // (#2032 packet 2) The static-build twin of `graphQuery` above — one
  // fetch of the WHOLE committed mission-id -> graph map, this mission's
  // entry read out of it below. `enabled` on BOTH `!daemonBacked` and a
  // published fixture: with no `darkmux-graphs-src` meta at all (no fixture
  // was ever exported for this build) there is nothing to fetch, matching
  // `staticMachineQuery`'s own `enabled: machineSrc !== null` precedent in
  // `MachineLens.tsx` — a disabled query with no fixture is a KNOWN answer
  // ("nothing published"), not a pending one.
  const graphsSrc = source.graphs;
  const staticGraphsQuery = useQuery({
    queryKey: queryKeys.staticGraphs(graphsSrc ?? ""),
    queryFn: () => fetchJson<Record<string, MissionGraph>>(graphsSrc as string),
    enabled: !daemonBacked && graphsSrc !== null,
    staleTime: Infinity,
  });

  const flowMissionQuery = useQuery({
    queryKey: queryKeys.flowMission(missionId),
    queryFn: () => fetchJson<unknown>(`/flow-mission/${encodeURIComponent(missionId)}`),
    enabled: daemonBacked,
  });
  const flowTodayQuery = useQuery({
    queryKey: queryKeys.flowDate(today),
    queryFn: () => fetchJson<unknown>(`/flow/${today}`),
    enabled: daemonBacked,
  });

  // The shared SSE tail — see this module's own doc for why this is the
  // ONLY EventSource open for this route (`App.tsx` never runs its own copy
  // here). `flowTailQuery` reads the SAME cache slot `useLiveTail` writes,
  // via `skipToken` (never fetches on its own), matching `useFlowWindow`'s
  // own precedent for reading a tail it doesn't own.
  const liveStatus = useLiveTail(daemonBacked);
  const flowTailQuery = useQuery<FlowRecord[]>({ queryKey: queryKeys.flowTail(today), queryFn: skipToken });

  const [ownedBy, setOwnedBy] = useState<string | null>(null);
  useEffect(() => {
    setOwnedBy(null);
    if (graphQuery.data && !graphQuery.data.ok && graphQuery.data.status === 404) {
      let cancelled = false;
      void lookupOwningMachine(missionId).then((m) => {
        if (!cancelled) setOwnedBy(m);
      });
      return () => {
        cancelled = true;
      };
    }
  }, [missionId, graphQuery.data]);

  const [viewMode, setViewMode] = useState<"auto" | "canvas" | "timeline">("auto");
  const isMobile = useIsMobile();
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [minimapOn, setMinimapOn] = useState(() => initMinimap());
  // (#1868) The mobile tap-to-open legend popover — `mission-graph.html`'s
  // own `legendOpen` state (`_leg`). Below the ~700px breakpoint `.legend`
  // hides via CSS (ported already) but nothing replaced it, leaving phones
  // with zero status-legend affordance — the timeline renderer's primary
  // audience per #1404. `.legbtn` is itself hidden above that breakpoint
  // (CSS), so this button only ever shows where the popover is the only way
  // to reach the legend.
  const [legendOpen, setLegendOpen] = useState(false);
  const toggleTask = (id: string) => setExpanded((prev) => ({ ...prev, [id]: !prev[id] }));
  const toggleMinimap = () =>
    setMinimapOn((prev) => {
      const next = !prev;
      persistMinimap(next);
      return next;
    });

  // (#2032 packet 2) `staticGraph` — this mission's entry in the committed
  // fixture map, or `null` when the map hasn't resolved yet / doesn't carry
  // this mission. `baseGraph` picks ONE source by build kind, never both:
  // a daemon-backed build never reads the static map (it has no meta to
  // resolve one from), and a static build never reads `graphQuery` (it's
  // `enabled: false`, so `graphQuery.data` stays `undefined` forever).
  const staticGraph = staticGraphsQuery.data?.ok ? (staticGraphsQuery.data.data[missionId] ?? null) : null;
  const baseGraph = daemonBacked ? (graphQuery.data?.ok ? graphQuery.data.data : null) : staticGraph;

  // All three record sources, deduped — see this module's own doc.
  const allRecords = useMemo(() => {
    const sources: FlowRecord[] = [];
    if (flowMissionQuery.data?.ok) sources.push(...asRecordArray(flowMissionQuery.data.data));
    if (flowTodayQuery.data?.ok) sources.push(...asRecordArray(flowTodayQuery.data.data));
    if (flowTailQuery.data?.length) sources.push(...flowTailQuery.data);
    const seen = new Set<string>();
    const out: FlowRecord[] = [];
    for (const r of sources) {
      if (!r || typeof r !== "object") continue;
      const k = recKey(r);
      if (seen.has(k)) continue;
      seen.add(k);
      out.push(r);
    }
    return out;
  }, [flowMissionQuery.data, flowTodayQuery.data, flowTailQuery.data]);

  // Whether the SERVER capped its own `/flow-mission/:id` response
  // (`MAX_CATALOG_RECORDS`) — see `bodyTruncated`'s own doc. `/flow/<today>`
  // and the live tail carry no equivalent cap for a single day, so this is
  // scoped to the mission-spanning source alone, same as legacy's own
  // `srvTruncRef` (mission-graph.html reads `bodies[0].truncated` only).
  const srvTruncated = flowMissionQuery.data?.ok ? bodyTruncated(flowMissionQuery.data.data) : false;

  const idx = useMemo(() => (baseGraph ? indexGraph(baseGraph) : null), [baseGraph]);

  const ascendingRecords = useMemo(() => [...allRecords].sort((a, b) => (a.ts < b.ts ? -1 : a.ts > b.ts ? 1 : 0)), [allRecords]);

  const graph = useMemo(() => {
    if (!baseGraph || !idx) return null;
    return foldFlowRecords(baseGraph, ascendingRecords, idx, missionId);
  }, [baseGraph, idx, ascendingRecords, missionId]);

  const metrics: MetricsMap = useMemo(() => {
    if (!baseGraph || !idx) return {};
    let m = seedMetricsFromGraph({}, baseGraph);
    for (const rec of ascendingRecords) m = applyRecordToMetrics(m, rec, idx, missionId);
    return m;
  }, [baseGraph, idx, ascendingRecords, missionId]);

  // `EventLogColumn` (the shared component, reused here as this lens's own
  // events pane) expects ASCENDING input (oldest first) — it takes
  // `.slice(-LOG_CAP).reverse()` internally to show the most recent rows
  // newest-first, the same shape every other caller feeds it
  // (`normalizeRecords`'s own ascending sort).
  //
  // This is built in two steps, not one ascending sort, because of a real
  // tie-break bug caught while writing the parity harness
  // (`next-parity-graph.spec.ts`): mission-graph.html's own events panel
  // sorts DESCENDING (newest-first) with a STABLE sort, so records sharing
  // one timestamp keep their ORIGINAL backfill-array order. A single
  // ascending sort here (also stable) preserves that same original
  // tie-order — but `EventLogColumn`'s OWN `.reverse()` then flips it,
  // inverting same-timestamp records relative to legacy's order. Sorting
  // DESCENDING first (matching legacy's own comparator exactly, same
  // tie-break) and then reversing OURSELVES undoes `EventLogColumn`'s own
  // reverse in advance, so the two reversals cancel and the FINAL display
  // order — including tie order — matches legacy's byte-for-byte (see the
  // parity spec's events-triple assertion).
  //
  // No pre-cap here (an earlier version sliced to a 250-record `EVENTS_CAP`
  // before handing the list to `EventLogColumn`) — that was the exact
  // truncation-as-total bug #1569 removed, one layer up: `EventLogColumn`
  // renders `${LOG_CAP} of ${filtered.length}` honestly, but `filtered.length`
  // was already the 250 cap by the time it got here, so a 400-record mission
  // read "50 of 250" instead of "50 of 400". `EventLogColumn` owns its own
  // display cap (`LOG_CAP`) and discloses the real total on its own — the
  // full scoped list is what it needs to do that honestly.
  const events = useMemo(() => {
    if (!idx) return [];
    const scoped = allRecords.filter((r) => recordInMission(r, idx, missionId));
    scoped.sort((a, b) => (b.ts < a.ts ? -1 : b.ts > a.ts ? 1 : 0));
    return scoped.slice().reverse();
  }, [allRecords, idx, missionId]);

  // Report this mission's own scoped events upward every time the fold
  // produces a new list — see `onEvents`'s own doc on why the DISPLAY moved
  // to the mainstay column while the data pipeline stays owned here.
  useEffect(() => {
    onEvents?.(events, srvTruncated);
  }, [events, srvTruncated, onEvents]);

  const anyRunning = !!(
    graph &&
    graph.nodes.some((n) => n.status === "running" || (n.steps || []).some((s) => s.status === "running"))
  );
  const now = useNow(anyRunning);
  const proc = useProcReadout(flowTailQuery.data);

  function refresh() {
    void queryClient.invalidateQueries({ queryKey: queryKeys.missionGraph(missionId) });
    void queryClient.invalidateQueries({ queryKey: queryKeys.flowMission(missionId) });
    void queryClient.invalidateQueries({ queryKey: queryKeys.flowDate(today) });
  }

  if (daemonBacked) {
    if (!graphQuery.data) {
      return (
        <div className="missionlens" data-state="loading">
          <div className="stagehdr">mission graph</div>
          <div className="none" role="status" aria-label={`Loading mission ${missionId}`}>
            loading mission graph…
          </div>
        </div>
      );
    }

    if (!graphQuery.data.ok) {
      const notFound = graphQuery.data.status === 404;
      const msg = notFound
        ? ownedBy
          ? `This mission ran on \`${ownedBy}\`, not on this machine. Its graph is built from local mission data, so open darkmux on \`${ownedBy}\` to see it.`
          : "This mission's graph data isn't available — it may have been an ephemeral or cleared run."
        : `darkmux mission graph: ${graphQuery.data.message}`;
      return (
        <div className="missionlens" data-state="error">
          <div className="stagehdr">mission graph</div>
          <div className="msg none" role="alert">
            {msg}
          </div>
          <button type="button" className="evbtn" title="retry — refetch graph" onClick={refresh}>
            ↻
          </button>
        </div>
      );
    }
  } else {
    // (#2032 packet 2) `graphQuery` never runs on a static build (`enabled:
    // daemonBacked`, above), so these three states — no fixture published
    // at all, the published fixture still resolving, this mission absent
    // from a resolved fixture — are decided from `graphsSrc`/
    // `staticGraphsQuery` instead of `graphQuery.data`.
    if (graphsSrc === null) {
      return (
        <div className="missionlens" data-state="no-daemon">
          <div className="stagehdr">mission graph</div>
          <div className="none">
            mission graph needs a running daemon behind this page — this static build has no mission graph data to show.
          </div>
        </div>
      );
    }

    if (staticGraphsQuery.isPending) {
      return (
        <div className="missionlens" data-state="loading">
          <div className="stagehdr">mission graph</div>
          <div className="none" role="status" aria-label={`Loading mission ${missionId}`}>
            loading mission graph…
          </div>
        </div>
      );
    }

    if (!staticGraph) {
      // A mission absent from the fixture map — never captured in this
      // demo world, or the fixture predates it. Reuses the SAME wording and
      // `data-state="error"`/`role="alert"` shape as the daemon-backed 404
      // branch above — the lens's existing empty/absent state, not a new
      // one and not a raw fetch-failure render. No retry button: there is
      // no daemon behind this page for a retry to reach.
      return (
        <div className="missionlens" data-state="error">
          <div className="stagehdr">mission graph</div>
          <div className="msg none" role="alert">
            This mission's graph data isn't available — it may have been an ephemeral or cleared run.
          </div>
        </div>
      );
    }
  }

  if (!graph) return null; // graph/idx derive from baseGraph, which is set once the active data source resolved successfully — defensive only

  const useTimeline = timelineActive(viewMode, isMobile);
  const tot = missionTotals(metrics);
  const status = normalizeMissionStatus(graph.mission_status);
  // Static builds never show the live pill. `useLiveTail(false)` never opens
  // a connection and its status stays at its `useState` INITIAL value —
  // `"live"`, forever (see that hook's own doc) — which would otherwise
  // render a confident "● live" on a page with no daemon anywhere near it.
  const showLivePill = daemonBacked && !(liveStatus === "live" && statusRank(status) >= 2);

  return (
    <div className="missionlens">
      <div className="top missionlens__top">
        <span className="midname" title={graph.mission_id}>
          {graph.mission_id}
        </span>
        <span className={`mstatus ${status}`}>{status}</span>
        {showLivePill ? (
          <span className={`livepill${liveStatus === "live" ? " on" : " off"}`}>{liveStatus === "live" ? "● live" : "○ reconnecting"}</span>
        ) : null}
        <MeterEl tot={tot} />
        <ProcEl proc={proc} />
        {/* (#2032 packet 2) No daemon behind a static build to refetch
            against — `refresh()` would only invalidate queries that are
            `enabled: false` and never re-run. Hidden rather than left as a
            dead click. */}
        {daemonBacked ? (
          <button type="button" className="evbtn" title="refresh — refetch graph + events" onClick={refresh}>
            ↻
          </button>
        ) : null}
        <button type="button" className="evbtn" title="switch renderer" onClick={() => setViewMode(useTimeline ? "canvas" : "timeline")}>
          {useTimeline ? "graph" : "list"}
        </button>
        {!useTimeline ? (
          <button type="button" className={`evbtn${minimapOn ? " on" : ""}`} title="toggle minimap" onClick={toggleMinimap}>
            map
          </button>
        ) : null}
        {/* (#1868) `.legbtn` — hidden above the ~700px breakpoint (CSS),
            visible only where `.legend` itself is hidden. A real `<button>`
            gets Tab/Enter/Space activation for free, matching every other
            control in this header. */}
        <button type="button" className="legbtn" title="status legend" onClick={() => setLegendOpen((v) => !v)}>
          legend
        </button>
        <div className="legend">{legendDots()}</div>
        {legendOpen ? <div className="legendpop on">{legendDots()}</div> : null}
      </div>
      <div className="body missionlens__body">
        {useTimeline ? (
          <MissionTimelineView nodes={graph.nodes} edges={graph.edges} metrics={metrics} now={now} note={graph.note} expanded={expanded} onToggleTask={toggleTask} />
        ) : (
          <MissionCanvas nodes={graph.nodes} edges={graph.edges} metrics={metrics} now={now} note={graph.note} minimapOn={minimapOn} />
        )}
      </div>
    </div>
  );
}
