import { useMemo, useRef, useEffect, useState, useCallback } from "react";
import { useIsMobile } from "./hooks/useIsMobile";
import { useHashRoute } from "./lib/useHashRoute";
import { getSource } from "./lib/source";
import { useDay } from "./hooks/useDay";
import { usePlaybackTransport } from "./hooks/usePlaybackTransport";
import { Scrubber } from "./lenses/catalog/Scrubber";
import { useSyncHash, writeHash, canonicalHash } from "./lib/hashSync";
import { FleetLens } from "./lenses/fleet/FleetLens";
import { LensPlaceholder } from "./components/LensPlaceholder";
import { NavChrome } from "./components/NavChrome";
import { Masthead } from "./components/Masthead";
import { MachineDrawer } from "./components/MachineDrawer";
import { EventLogColumn } from "./components/EventLogColumn";
import { LensErrorBoundary } from "./components/LensErrorBoundary";
import { MachineLens } from "./lenses/machine/MachineLens";
import { RunsBoard } from "./lenses/runs/RunsBoard";
import { ConsolePanel } from "./lenses/console/ConsolePanel";
import { MissionGraphLens } from "./lenses/mission/MissionGraphLens";
import { StepHeaderBlock } from "./lenses/mission/StepHeaderBlock";
import { stepDispatchSessions, type StepHeaderField } from "./lenses/mission/graph";
import { SessionReplay } from "./lenses/catalog/SessionReplay";
import { PlaybackLens } from "./lenses/catalog/PlaybackLens";
import { useFlowWindow } from "./hooks/useFlowWindow";
import { useRouteRecords } from "./hooks/useRouteRecords";
import { useLiveMachines } from "./hooks/useLiveMachines";
import { useLiveTail } from "./hooks/useLiveTail";
import { computeMetaLines, readyParts } from "./lib/metaLine";
import { replayMetaLines, replayMetaParts } from "./lib/replayMeta";
import { ReadyHeadline } from "./components/ReadyHeadline";
import { T, asRecordArray, firstRecordDate, localMachineUid, nameOf, todayUTC } from "./lib/flow";
import { isLiveRoute, showsEventLog } from "./lib/route";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "./lib/fetcher";
import { queryKeys } from "./lib/queryKeys";
import type { FlowRecord, MachineSpecs } from "./types/handwritten";
import type { Route } from "./lib/route";

/**
 * The app shell. A `switch` over the parsed [[Route]] (see `lib/route.ts` for
 * the hash-grammar port) drives `#stage`; `fleet` (`FleetLens`, Packet 8 —
 * the savings hero + machine cards + activity timeline; supersedes the
 * scaffold's original `FleetStrip` presence-only proof region, still tested
 * standalone in `components/FleetStrip.test.tsx` but no longer mounted
 * here), `runs` (`RunsBoard`, Packet 3), and `machine` (`MachineLens`,
 * Packet 2) are real regions driven by `useQuery`; `mission`
 * (`MissionGraphLens`, #1868) is a real, self-contained region with its own
 * header/events pane; `session`/`playback` (Packet 4) do REAL fetches/
 * navigation wiring per the catalog+replay
 * lens's own doc comments; every other lens renders [[LensPlaceholder]]
 * naming what still needs to be built, per the render-sanity contract
 * (never a blank page).
 *
 * `#crumb` and `#logscope` are LENS-SPECIFIC (legacy: `renderCrumb()`'s
 * `$("crumb").innerHTML=...` per `state.level`, and each `render*()`
 * function's own `$("logscope").textContent=...`) — the `{crumb, logscope}`
 * pair is computed here (`routeChrome`, below) per route rather than inside
 * each lens component, since `#crumb` is an App-level sibling of `#stage`,
 * not a descendant of it. `#meta` is LENS-INDEPENDENT (legacy's `renderMeta()`
 * runs on every render() regardless of `state.level` — confirmed:
 * `goldens/fleet.txt` and `goldens/machine.txt` carry byte-identical
 * `=== meta ===` sections), so it's computed here rather than per-lens — but
 * it is NOT mode-independent: `renderMeta` branches live-vs-replay
 * (viewer.html:1330-1340), and #1800 wired the replay arm (`lib/replayMeta.ts`)
 * off `routeRecords`. Lens-independent, mode-dependent. The underlying
 * `useFlowWindow`/`useLiveMachines`/`machineSpecs` queries are ALSO used
 * inside `MachineLens` — TanStack Query dedupes by queryKey, so this is
 * cache reuse, not a second network round trip.
 *
 * **(Chrome packet) `#logscope` moved OUT of this file** — it now lives
 * inside `EventLogColumn` (see that component's own doc), the computed
 * `logscope` STRING still comes from `routeChrome` here and is passed down
 * as a prop, but the DOM node only exists when `showsEventLog(route)` is
 * true. Rendering it unconditionally at the App level (the pre-this-packet
 * shape) was the direct cause of the stray uppercase "FLEET" the operator
 * caught floating above the hero — legacy nests the equivalent node INSIDE
 * the (sometimes-hidden) log column's own header, never loose.
 *
 * `CatalogPanel` mounts inside `<Masthead>` now (moved this packet — see
 * that component's own doc for why: legacy's `#catpanel` toggle,
 * `#srcbadge`, lives in `.top`, not the crumbbar) rather than here directly
 * — still global chrome (`viewer.html`'s `#catpanel`, reachable from every
 * lens), not a routed destination itself.
 *
 * Packet 1.5 additions (nav chrome + hash write-back — the scaffold gap
 * both the machine and runs lens packets independently flagged as a hard
 * blocker for the eventual `/next` → `/` flip):
 *
 * - `<NavChrome>` (see that component's own doc) is a new sibling INSIDE a
 *   `.app-shell__crumbbar` wrapper alongside `#crumb`/`#meta` — a pure DOM
 *   restructuring, not a content change: the parity extractor
 *   (`tests/parity/lib/extract-lens.js`) selects `#crumb`/`#meta` BY ID
 *   regardless of parent, so moving their container doesn't touch
 *   byte-parity. `#logscope`/`#stage` are untouched siblings, same as
 *   before.
 * - `useSyncHash` (see `lib/hashSync.ts`) is the `/next` port of legacy's
 *   `syncLabHash()` — reflects the current `Route` back into `location.hash`
 *   via `replaceState` on every route change, so every view is bookmarkable
 *   (matches legacy's own reasoning: the phone dashboard is the first-class
 *   consumer). This is also what performs the legacy `#lens=lab` →
 *   `#lens=runs&kind=lab` upgrade, since arriving on the alias parses to
 *   the canonical `Route` already and the write-back just names it.
 *   `RunsBoard`'s kind chips are the one piece of lens state that changes
 *   WITHOUT a route change (no `hashchange` fires) — that write goes
 *   straight from `RunsBoard.tsx`'s `selectKind` to `hashSync.ts`'s
 *   `writeHash`, not through this route-keyed effect (see that file's own
 *   doc for why).
 */
export function App() {
  const route = useHashRoute();
  const nowMs = Date.now();
  // (#2107 tabbed-drawer packet) The SAME phone/desktop call
  // `MachineDrawer.tsx` makes internally (see that file's own doc) —
  // needed here too, since a phone route's events pane now lives INSIDE
  // the drawer's own Events tab rather than in this file's inline
  // `EventLogColumn` mount below. Two independent measurements of the
  // same `window.innerWidth`, deliberately (see `useIsMobile`'s own doc
  // for why that's the right call, not a shared subscription).
  const isMobile = useIsMobile();

  // (Packet 5) The SSE tail + reconcile backstop + date-rollover handler —
  // gated by `isLiveRoute` (see that function's own doc) so a genuinely
  // historical route (`playback`/`session`) doesn't run
  // a live tail behind it, matching legacy's own `wantsPlayback` gate on
  // `startLiveTail`. Feeds `flowWindow` below via the Query cache
  // (`useFlowWindow`'s own doc), not a direct return-value dependency here.
  const liveStatus = useLiveTail(isLiveRoute(route));

  const flowWindow = useFlowWindow(nowMs);
  // (#1800 P1) The event log follows the ROUTE, not the clock. On a
  // `#session=`/`#<date>` route this is that slice; on a live route it is
  // still the rolling window. Before this, every route got the live window,
  // so a session's stage and its event log described different things.
  const routeRecords = useRouteRecords(route, flowWindow);
  // (#1869 code review) The event log's own playhead scope, on a playback
  // route only. `EventLogColumn` is App-level chrome — a SIBLING of
  // `PlaybackLens` (mounted inside `#stage` by `renderRoute` below), not a
  // descendant of it — so it can't read `PlaybackLens`'s own `t` state
  // directly. `PlaybackLens` reports its resolved playhead up through
  // `onPlaybackPlayheadChange` (threaded through `renderRoute`, playback-
  // route-only; every other lens ignores the extra argument) each time it
  // changes, and this is where that value lands.
  //
  // Before this, the log stayed on `routeRecords.records` UNSCOPED on every
  // route — a no-op on live routes (nothing there ever narrows it), but on
  // playback it meant the log kept listing the WHOLE day regardless of
  // where the scrubber sat, while `FleetLens`'s own hero already scoped
  // itself to the same playhead (`scopedData`, `FleetLens.tsx`) — two
  // surfaces on the same screen disagreeing about how much of the day had
  // "happened yet". Measured live: rewound to the day's start, the hero
  // read zero tokens/dispatches while the log still listed rows stamped
  // hours later.
  //
  // `null` means "no scrub has happened yet on this playback mount" (the
  // pre-transport default: playhead == the day's ceiling, i.e. everything),
  // so no filtering — matches `PlaybackLens`'s own `t ?? tMax` convention.
  // Guarded on `route.kind === "playback"` so a stale value left over from
  // a previous playback visit can never leak into a live route's log.
  // (#2071) The loaded day the shell's transport scrubs. A static build has
  // ONE committed file and loads it on every route (one cached download; the
  // landing route fetches it anyway), so play/pause and the scrubber sit in
  // the sticky block on every tab and the run-detail and mission lenses
  // replay against the same clock. A daemon build has a day only on its
  // `/play/<date>` route; live routes have nothing to scrub.
  // (#2086) One resolver for the loaded day — static file on every route,
  // `/flow/<date>` on a daemon playback, nothing on a live route.
  const source = getSource();
  // The day a daemon replay belongs to. A playback route names it; a
  // dispatch or mission replay does not, so it is derived from the replayed
  // records themselves (the earliest one's day) — the shell then loads that
  // day, the transport appears, and the chip names the date instead of the
  // bare "RESULT" the demo never showed (operator: "says replay not the
  // date"). The mission records ride the same `queryKeys.flowMission` slot
  // the mission lens fetches, so this is cache reuse.
  const missionRecordsQuery = useQuery({
    queryKey: queryKeys.flowMission(route.kind === "mission" ? route.missionId : ""),
    queryFn: () => fetchJson<unknown>(`/flow-mission/${encodeURIComponent(route.kind === "mission" ? route.missionId : "")}`),
    enabled: source.kind === "daemon" && route.kind === "mission",
  });
  const replayDate = useMemo(() => {
    if (route.kind === "playback") return route.date;
    if (source.kind !== "daemon") return null;
    // A dispatch that is still RUNNING is a live view, not a replay: no day,
    // no badge, no transport over a frozen snapshot (review finding).
    if (route.kind === "dispatch") return routeRecords.historical ? earliestDate(routeRecords.records) : null;
    if (route.kind === "mission") return missionRecordsQuery.data?.ok ? earliestDate(asRecordArray(missionRecordsQuery.data.data)) : null;
    return null;
  }, [route, source.kind, routeRecords.records, routeRecords.historical, missionRecordsQuery.data]);
  const day = useDay(replayDate);
  const dayRecords = day.records;
  const transport = usePlaybackTransport(dayRecords);
  // (#2071 review) Show the transport only where something on screen
  // answers it. The mission lens takes no playhead — its own events fold
  // is always the full historical set, never scoped to a scrubbed time
  // (see `eventLogRecords`'s own mission branch above) — so play/pause
  // there would move nothing.
  const transportShown = transport.active && route.kind !== "mission";
  // Lenses and the log scope to the playhead only once it has MOVED
  // (`transport.scrubbed`): at rest the playhead is the loaded day's end,
  // which can sit before the end of a run that crossed midnight, and the
  // default render must be the whole run (measured: a session page lost
  // half its elapsed time to the cut before this guard).
  // `t < tMax`, not just `scrubbed`: after a play-through the playhead rests
  // at the loaded day's end with `scrubbed` still true, and the cut would
  // return by the transport's own primary gesture (review finding).
  const playhead = transport.active && transport.scrubbed && transport.t < transport.tMax ? transport.t : null;
  // (mainstay-unification finding) `MissionGraphLens` owns the mission
  // events DATA pipeline (flow-mission + flow-today + live-tail, deduped,
  // mission-scoped) and reports its result here via `onMissionEvents` —
  // see that component's own doc for why the DISPLAY moved out to the
  // shared/mainstay column instead of a second bespoke surface. `null`
  // until the lens's first fold resolves (or whenever we're not even on a
  // mission route); the mainstay-column render sites below treat that the
  // same as "no records yet", never a thrown/undefined read.
  const [missionEvents, setMissionEvents] = useState<{ records: FlowRecord[]; truncated: boolean } | null>(null);
  const onMissionEvents = useCallback((records: FlowRecord[], truncated: boolean) => {
    setMissionEvents({ records, truncated });
  }, []);
  // (#2223) The same records, held in a REF purely so `onSelectStep` can
  // read them without taking `missionEvents` as a dependency. That
  // callback is handed down to `MissionGraphLens` and on to the React Flow
  // canvas; depending on state that changes on every records fold would
  // give it a new identity on every fold, churning the canvas's renders
  // for a value only ever read INSIDE a click handler, long after render.
  const missionRecordsRef = useRef<FlowRecord[]>([]);
  missionRecordsRef.current = missionEvents?.records ?? [];
  // (#2189, step drill-in) `route.stepId` — App.tsx owns the route/hash, so
  // the WRITE lives here too: a click on a node/row calls this, which
  // writes the canonical `mission=<id>&step=<id>` (or drops `step` on
  // clear) via `writeHash(canonicalHash(...))`, mirroring `RunsBoard`'s own
  // `openLabRun`/`closeLabRun` — see `hashSync.ts`'s `mission` case doc.
  // `MissionGraphLens` never touches `location.hash` itself; it only reads
  // the selection back (to highlight) and calls this on a click.
  const onSelectStep = useCallback(
    (stepId: string | null) => {
      if (route.kind !== "mission") return;
      // (#2223) Drill in as deep as the data allows. When the step's own
      // records name a real dispatch, THAT is the drill-in the operator
      // asked for -- the detail view, with the model's token counts,
      // context headroom, host peaks and signals. #2189 could only scope
      // the events column because the step carries no dispatch id itself;
      // `stepDispatchSessions` recovers it from the records the lens has
      // already fetched, so this needs no second request.
      //
      // A REAL navigation (`location.hash =`, a history entry) rather than
      // `writeHash`'s `replaceState`, matching how every other cross-lens
      // jump moves -- `FleetLens`'s activity bars use this exact form. The
      // history entry is the point on a phone, where the back gesture is
      // how the operator returns to the mission.
      if (stepId) {
        const dispatchId = stepDispatchSessions(missionRecordsRef.current, route.missionId)[stepId];
        if (dispatchId) {
          location.hash = `dispatch=${encodeURIComponent(dispatchId)}`;
          return;
        }
      }
      // No dispatch behind this step (a procedural step, or one whose
      // records predate `step_id` stamping) -- keep #2189's scoping, which
      // stays the best available view of it.
      writeHash(canonicalHash({ kind: "mission", missionId: route.missionId, stepId }));
    },
    [route],
  );
  // (#2189) The selected step's header-block fields, reported by
  // `MissionGraphLens` — `null` whenever no step is selected (mirrors
  // `missionEvents`'s own "null until the lens's first fold resolves"
  // convention).
  const [stepHeaderFields, setStepHeaderFields] = useState<StepHeaderField[] | null>(null);
  const onStepHeader = useCallback((fields: StepHeaderField[] | null) => {
    setStepHeaderFields(fields);
  }, []);
  const selectedMissionStepId = route.kind === "mission" ? route.stepId : null;
  // (#2189) The small block rendered ABOVE the mainstay events column on
  // BOTH surfaces (desktop's inline mount and the phone drawer's Events
  // tab) — `null` unless a step is actually selected AND its fields have
  // resolved, so a step selected an instant before the graph/metrics fold
  // catches up never flashes an empty block (`EventLogColumn`'s own
  // `headerExtra` doc). `missionId` is read from `route` directly (never
  // undefined here — `selectedMissionStepId` is non-null only on a
  // `mission` route).
  const stepHeaderExtra =
    selectedMissionStepId && stepHeaderFields && route.kind === "mission" ? (
      <StepHeaderBlock missionId={route.missionId} fields={stepHeaderFields} onBack={() => onSelectStep(null)} />
    ) : null;
  const eventLogRecords = useMemo(() => {
    // Mission has no playhead concept (`transportShown` already excludes
    // it below) — its own fold is always the full, historical record set,
    // never scoped to a scrubbed time.
    if (route.kind === "mission") {
      const all = missionEvents?.records ?? [];
      // (#2189) `step_id` EQUALITY, same rule `buildStepHeaderFields`'s own
      // doc names — the mainstay column scopes to exactly this step's own
      // records when one is selected, never a re-fetch (one source of
      // records, filtered here, at the point they're handed to the
      // column — see #2189's own issue text).
      return route.stepId ? all.filter((r) => r.payload && r.payload.step_id === route.stepId) : all;
    }
    if (playhead === null) return routeRecords.records;
    // A static build's runs/machine/console routes have no slice of their
    // own (the live window is empty there); the day's log, scoped to the
    // playhead, is what the transport is scrubbing. Playback and dispatch
    // routes keep their own slice, scoped the same way.
    const own = route.kind === "playback" || route.kind === "dispatch" || source.kind === "daemon";
    const base = own ? routeRecords.records : (dayRecords ?? []);
    // `!(ts > t)`, not `ts <= t`: a record with an unparseable `ts` stays
    // in the log, as it did before the transport scoped every route.
    return base.filter((r) => !(T(r.ts) > playhead));
  }, [route.kind, selectedMissionStepId, missionEvents, playhead, routeRecords.records, source.kind, dayRecords]);
  // (#2071) The sticky block's measured height feeds `--chrome-h`, the
  // offset the event log column sticks under on desktop. It used to be a
  // 97px constant that assumed the masthead + one chrome row.
  const stickyRef = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    const el = stickyRef.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const apply = () => el.parentElement?.style.setProperty("--chrome-h", `${Math.round(el.getBoundingClientRect().height)}px`);
    apply();
    const ro = new ResizeObserver(apply);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  // (#2108, operator finding — real device) The masthead's own measured
  // height feeds `--masthead-h`, so the phone drawer can cap its OPEN
  // height and never draw its top edge over the logo row — a fixed 88vh
  // covered the masthead on a real iPhone, where the ~64px the backdrop's
  // own scrim clearance assumed wasn't quite what the real masthead
  // rendered at. Queried directly by class rather than via a ref threaded
  // through `<Masthead>` — that component's DOM is a parity-golden
  // surface (`no-danger.test.ts` et al.); a `ref` prop wouldn't change
  // its rendered output, but this keeps that file from needing to know
  // about a purely-measurement need elsewhere. Same `ResizeObserver`
  // pattern as `--chrome-h` above; set on `documentElement` rather than
  // `.app-shell` since `PhoneDrawer`'s sheet is `position: fixed` and a
  // custom property only needs to be somewhere in its ancestor chain —
  // the root is the least assumption-laden place for that.
  useEffect(() => {
    if (typeof ResizeObserver === "undefined") return undefined;
    const el = document.querySelector(".masthead");
    if (!el) return undefined;
    const apply = () =>
      document.documentElement.style.setProperty(
        "--masthead-h",
        `${Math.round(el.getBoundingClientRect().height)}px`,
      );
    apply();
    const ro = new ResizeObserver(apply);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  // (#1800 P2, QA gate) Route-gated for the same reason `useLiveTail` above
  // is, and the gate is load-bearing in a way that is easy to get wrong:
  // `FleetLens` already passed `enabled: false` on a replay, but a DISABLED
  // TanStack observer still reads its key's shared cache — and this call,
  // running on EVERY route with the same key, kept that cache warm and kept
  // polling `/fleet/machines/live` behind a replay. Measured: 2 polls in 9s,
  // plus a fleet card rendered for a machine with zero records that day. The
  // lens's own test passed throughout, because a lens rendered in ISOLATION
  // has no second observer. Gating the fetch is not enough; the consumer has
  // to be gated too, and this is the consumer.
  //
  // The same rule caught a THIRD endpoint one commit later: `/machine/specs`
  // (below) was ungated on both sides, so a replay rendered today's CPU and
  // RAM on the machine card. Legacy's own statement of the rule is general —
  // `pollLiveMachines`, `pollLiveSessions` and `pollMachineSpecs` are all
  // live-mode-only polls, and a replay starts NONE of them. Anything added
  // here that describes NOW belongs in that list.
  const liveMachines = useLiveMachines(isLiveRoute(route));
  // Gated for the SAME reason, and via the same two-sided rule: `/machine/specs`
  // is live-only (viewer.html:2696 — "playback mode never starts that poll"),
  // and an ungated observer here would keep the shared cache warm for
  // `FleetLens`'s gated one exactly as `useLiveMachines` did. Gating one side
  // and not the other is indistinguishable from gating neither.
  const specsQuery = useQuery({
    enabled: isLiveRoute(route),
    queryKey: queryKeys.machineSpecs(),
    queryFn: () => fetchJson<MachineSpecs>("/machine/specs"),
  });
  const specs = isLiveRoute(route) && specsQuery.data?.ok ? specsQuery.data.data : null;

  const localUid = useMemo(
    () => localMachineUid(flowWindow.data, liveMachines, specs?.machine_id ?? null),
    [flowWindow.data, liveMachines, specs],
  );
  const localName = localUid != null ? nameOf(flowWindow.data, liveMachines, localUid) : null;
  // (drill-in packet) The MACHINE route's own target — the local machine
  // for `uid: null`, or the drilled uid's own name otherwise. `nameOf` is
  // uid-generic (works for a remote uid too, via its presence beat or flow
  // records — see `lib/flow.ts`), so this is the same lookup `MachineLens`
  // itself does for its header, not a second implementation.
  const targetMachineName =
    route.kind === "machine" ? (route.uid != null ? nameOf(flowWindow.data, liveMachines, route.uid) : localName) : null;

  // (#1800) `#meta` takes legacy's REPLAY branch on a replay. Until now it
  // computed from `flowWindow` (the live rolling window) on every route, so a
  // `#<date>` page's status bar described TODAY while the stage beside it
  // described the recorded day — the loudest of the three surfaces that
  // disagreed about what day the page was showing.
  //
  // No new fetch: `routeRecords` is already that day's records (P1), so the
  // replay line reads the same set the stage does. That is the property worth
  // having — a second fetch could drift; one source cannot.
  const replayMeta = route.kind === "playback" ? routeRecords.records : null;
  const ready = useMemo(
    () => (replayMeta ? null : readyParts(flowWindow.data, liveMachines, nowMs)),
    [replayMeta, flowWindow.data, liveMachines, nowMs],
  );
  // (#2072) `computeMetaLines` describes a DAEMON's idle state ("waiting for
  // a machine", "N machines · last dispatch …"); a static build has no
  // daemon, so a non-playback route there gets no meta line rather than a
  // live-only phrase about a machine that will never arrive.
  const staticIdle = source.kind === "static" && !replayMeta;

  // (#1801) A static-build playback route carries `date: null` at parse time
  // (`route.ts`'s own doc on the widened variant) — the real date is only
  // knowable once the flow-src fetch resolves, exactly like legacy's own
  // `RAW[0].ts` derivation (`lib/flow.ts::firstRecordDate`). `routeRecords`
  // already IS that fetch's result (`useRouteRecords`' static branch reads
  // the identical source), so this reads the one fetch already in flight
  // rather than adding a second. Falls back to `todayUTC()` while the fetch
  // is pending or the file is empty/unreachable — a placeholder, not a
  // crash, matching legacy's own "date stays whatever it defaulted to" on
  // that identical gap (`firstRecordDate`'s own doc).
  //
  // `displayRoute` is used ONLY for TEXT below (the meta line, the masthead
  // badge) — `route` itself keeps driving every behavioral decision (which
  // lens renders, whether the live tail runs, which fetch `PlaybackLens`
  // issues), so a still-loading static date never flips any of those.
  //
  // Judgment call: `firstRecordDate` is documented against RAW file-order
  // records (`records[0]`, matching legacy's un-sorted `RAW[0].ts` exactly,
  // header-line quirk included), but `routeRecords.records` here has already
  // been through `normalizeRecords` — sorted by ts, header line dropped. For
  // a flow file that is itself roughly chronological (the only kind
  // `build-demo.sh` ever commits), the two agree; a hand-edited or
  // deliberately-reordered fixture could show a different label than
  // legacy's literal quirk would. Reading `useRouteRecords`' RAW pre-shape
  // array here instead would mean plumbing a second field through that
  // hook's return type for this one display niche — not worth it for a
  // label whose only failure mode is "names a different real date from the
  // file", never a crash or an empty page.
  const displayRoute: Route =
    route.kind === "playback" && route.date === null
      ? { ...route, date: firstRecordDate(routeRecords.records) ?? todayUTC() }
      : route;

  const metaLines = useMemo(
    () =>
      replayMeta
        ? replayMetaLines(replayMeta, displayRoute.kind === "playback" ? (displayRoute.date ?? "") : "")
        : staticIdle
          ? []
          : computeMetaLines(flowWindow.data, liveMachines, nowMs),
    [replayMeta, displayRoute, flowWindow.data, liveMachines, nowMs, staticIdle],
  );

  // `logscope` is no longer SHOWN — the outer UI owns context (see
  // EventLogColumn's header). It is still computed and still rendered into a
  // HIDDEN span, because legacy's own span keeps its text and `innerText`
  // falls back to `textContent` for an unrendered element: if this port
  // emitted nothing, the two would disagree in the parity extraction. The
  // values are lowercase now for the same reason — CSS `text-transform`
  // never applies to text that is not rendered, so legacy's raw text is what
  // both sides must match. All of this dies with legacy at the flip.
  const replayParts = useMemo(
    () => (replayMeta ? replayMetaParts(replayMeta, displayRoute.kind === "playback" ? (displayRoute.date ?? "") : "") : null),
    [replayMeta, displayRoute],
  );
  const { crumb, logscope } = routeChrome(route, targetMachineName);
  // (mainstay-unification finding) `routeChrome` carries no mission
  // `logscope` (`mission` STILL "carries none" there — that function's own
  // doc — because the mission id is the natural scope label and belongs
  // here, not duplicated into that lookup for one route). Mission's own
  // records are ALWAYS a cross-day, cross-restart fold, never a rolling
  // "last 24h" window — `historical: true` unconditionally, matching what
  // `MissionGraphLens`'s own retired inline mount used to pass.
  // (#2189) Step-scoped filter-picks/persistence key — appends the step id
  // so a step's own remembered filter picks (`EventLogColumn`'s own
  // `storedFilterPicks`) don't collide with the whole mission's.
  const eventLogScopeLabel =
    route.kind === "mission" ? (route.stepId ? `${route.missionId}:${route.stepId}` : route.missionId) : logscope;
  const eventLogHistorical = route.kind === "mission" ? true : routeRecords.historical;
  const eventLogServerTruncated = route.kind === "mission" ? (missionEvents?.truncated ?? false) : false;
  // (#2120, operator finding — "reads like a variable name") The transport's
  // OWN mission label, replacing the raw-id `◆ <mission>` crumb the sticky
  // row used to carry for a playback route (see `routeChrome`'s own doc on
  // why that branch now returns an empty crumb instead). Desktop only —
  // the caller (below) omits the `label` prop entirely on a phone route, so
  // `Scrubber` never has to make that call itself. `resolvedMissionLabel`
  // (#2121) prefers a REAL title when the mission's own records carry one,
  // falling back to `humanMissionLabel`'s id-derived heuristic — either way
  // returns `null` when neither is available; this deliberately does NOT
  // fall back to the raw id here (per the operator's earlier refinement:
  // "the raw id lives only in the Machine info modal's playback row") —
  // the transport shows a human label or nothing, never the id.

  useSyncHash(route);

  return (
    <div className="app-shell">
      {/* (Chrome packet) The masthead — brand, build chip, the catalog
          trigger, the live/mode badge, refresh, topnav — moved out of this
          function into its own component; see `Masthead.tsx`'s own doc for
          why `<LiveStatusBadge>`'s live-route gating (the QA note that used
          to live on this line) now lives there instead. Precedes
          `.app-shell__crumbbar`, matching legacy's DOM order (`.top` before
          `.crumbbar`). */}
      <Masthead
        route={displayRoute}
        liveStatus={liveStatus}
        replayDate={route.kind === "playback" ? null : replayDate}
      />
      {/* (#2107) Global machine-stats pill/drawer — a SIBLING of `<Masthead>`,
          not a child of it, so it can never touch that component's own
          byte-parity-golden DOM (see `Masthead.tsx`'s doc). Fixed-position
          via CSS, so it renders identically regardless of where in the DOM
          it sits. Reads the SAME `routeRecords`/`flowWindow`/`localUid`
          this file already resolves for the meta line and the machine
          lens — cache reuse, not a second fetch; see
          `lib/machineDrawerScope.ts` for the mission/dispatch-vs-rolling-
          window scope rule.

          (#2107 tabbed-drawer packet) The `eventLog*` props are the SAME
          values the inline `<EventLogColumn>` mount below receives —
          `MachineDrawer` only actually uses them on a phone route, where
          they become the drawer's Events tab (see that component's own
          doc), and ignores them entirely on desktop. Passed
          unconditionally rather than gated here too, since the cost is a
          few extra props on an already-cheap render, not a second fetch. */}
      <MachineDrawer
        route={route}
        routeRecords={routeRecords.records}
        flowWindow={flowWindow.data}
        localUid={localUid}
        liveMachines={liveMachines}
        specs={specs}
        liveStatus={liveStatus}
        eventLogRecords={eventLogRecords}
        eventLogScopeLabel={eventLogScopeLabel}
        eventLogVisible={showsEventLog(route)}
        eventLogLoading={routeRecords.loading}
        eventLogError={routeRecords.error}
        eventLogHistorical={eventLogHistorical}
        eventLogServerTruncated={eventLogServerTruncated}
        eventLogHeaderExtra={stepHeaderExtra}
      />
      {/* (#2071, superseded by #2108 rounds 1+2 below) Originally: sticky row
          holds only the tab strip + transport; the masthead, crumb and meta
          line scroll away, and the row's height is route-independent
          because the meta line is NOT in it. Both crumb and meta have since
          moved into this row (see their own doc comments a few lines down)
          — the masthead is the only chrome still left scrolling away above
          it. Kept as history rather than deleted: it's the reasoning that
          stopped the original 32px tab-strip jump, still worth knowing when
          touching this row again. */}
      <div className="app-shell__sticky" ref={stickyRef}>
        <NavChrome route={route} />
        {transportShown ? (
          <Scrubber
            t={transport.t}
            tMin={transport.tMin}
            tMax={transport.tMax}
            playing={transport.playing}
            speed={transport.speed}
            onScrub={transport.scrub}
            onRewind={transport.rewind}
            onTogglePlay={transport.togglePlay}
            onCycleSpeed={transport.cycleSpeed}
          />
        ) : null}
        {/* (#2108, operator finding — desktop tab-row fold) `#crumb` moved
            HERE from its old home in `.app-shell__crumbbar` — on desktop it
            now reads on the SAME row as the tabs ("subtitle folded into the
            tab row"), a real DOM move (not a CSS trick), safe for parity
            since the extractor selects `#crumb` BY ID regardless of parent
            (this file's own module doc, Packet 1.5). `styles.css`'s mobile
            override puts crumb back on its OWN row below the tabs —
            "phones keep two rows". */}
        {/* (#2073, superseded by #2120) `is-replay` used to matter because a
            playback route's crumb repeated the meta line's own lead (`◆
            <mission>`) and the narrow stylesheet dropped that copy. #2120
            emptied the playback crumb entirely (`routeChrome`'s own doc on
            that branch) — the mission now shows as a human label on the
            transport itself instead — so this class is inert today (an
            empty header either way), kept rather than stripped only
            because removing it buys nothing: the element itself still
            needs to exist/not-exist by `route.kind`, which is what the
            class was never responsible for. */}
        {/* (operator finding, phone screenshot) `route.kind === "machine"` is
            excluded from rendering `#crumb` AT ALL — not just emptied. The
            #2108 round-N "desktop tab-row fold" comment above USED to claim
            the machine-name crumb was handled by folding it into the tab
            row; that only changed WHERE it sat, never WHETHER it rendered,
            and the mobile-only override two rules below
            (`.app-shell__crumb { flex: 1 1 100%; }`, `styles.css`) gives an
            in-DOM `#crumb` its own full-width row regardless of whether its
            text is empty or "MacBook-Pro" — a real element in a `gap`-ed
            flex row still consumes a gap slot even with zero text. So the
            machine name kept showing as a whole standalone line on a phone,
            directly above `MachineLens`'s own `.machine-lens__hdr`
            breadcrumb ("fleet › machine — <spec>", which already dropped
            its OWN copy of the name for exactly this reason — see that
            component's own doc). Root cause was DOM presence, not text
            content, hence unmounting the element here rather than returning
            `crumb: ""` from `routeChrome`'s `machine` branch (which still
            feeds `logscope`, used elsewhere, and is left untouched).
            `tests/parity/next-parity.spec.ts`'s machine-lens goldens are
            updated to match — see that file's own `normalizeMachineCrumb`
            doc for the byte-parity side of this call. */}
        {route.kind !== "machine" && (
          <header className={`app-shell__crumb${route.kind === "playback" ? " is-replay" : ""}`} id="crumb">
            {crumb}
          </header>
        )}
        {/* (#2108, operator finding — round 2) `#meta` moved HERE too, from
            the now-deleted `.app-shell__crumbbar` wrapper, so the "N ⚙ ·
            last dispatch …" summary shares the tab row with the tabs and
            `#crumb` instead of sitting on its own full-height row beneath
            it. This SUPERSEDES the other half of the #2071 sticky packet's
            original call ("masthead, crumb AND meta line scroll away") —
            round 1 already moved crumb into the sticky row; meta joining it
            here means the whole row, meta included, is sticky now. `#meta`
            keeps its own id/content (pure DOM move, parity-safe same as
            crumb's move above). `.app-shell__meta`'s existing
            `margin-left: auto` is what pins it to the right of the row;
            `styles.css`'s mobile override gives it back its OWN row below
            crumb, same "phones keep two rows" treatment. */}
        {/* (#2120, operator decision — "in playback mode the transport IS
            the summary") This whole block used to render unconditionally;
            it now doesn't render AT ALL while the sticky row's playback
            transport is mounted, at any width. Before this, a wide desktop
            wrapped the folded replay summary onto a second right-aligned
            line under the transport (the bug report's own screenshot), and
            a phone got its OWN full-width row for it (`styles.css`'s
            mobile override) — both were the same underlying problem: the
            transport already names the mission (via `Scrubber`'s own
            `label` prop, above) and the day/range/census this line used to
            carry moved into the Machine info modal's `playback` kv row
            (`machineStatsContent.tsx`), so there is nothing left for this
            block to say that isn't either duplicated or stale. Non-playback
            routes are untouched: `transportShown` is false there (a live
            route never loads a day to scrub — `usePlaybackTransport`'s own
            doc), so this renders exactly as it did before. */}
        {!transportShown && (
          <div className="app-shell__meta" id="meta">
            {/* `whiteSpace: "pre"` — the idle headline's literal double space
                before "· last run" (see `metaLine.ts`'s module doc) is an
                artifact of legacy's icon SPAN breaking the whitespace-collapse
                run; default `white-space: normal` would collapse it back to
                one space here, since there's no element in the way. Preserving
                it verbatim is simpler and more robust than reproducing the
                icon-boundary quirk with a real (empty) element. */}
            {ready && !staticIdle ? (
              <div><ReadyHeadline n={ready.n} ago={ready.ago} /></div>
            ) : replayParts ? (
              /* (#2073) Same text as `metaLines[0]`; the source + span sit in
                 their own span so the narrow stylesheet can drop what the chip
                 and the activity timeline already say. */
              <div className="app-shell__metaline">
                {replayParts.head}
                <span className="app-shell__metasrc">{` · ${replayParts.source} · ${replayParts.span}`}</span>
              </div>
            ) : (
              <div className="app-shell__metaline">{metaLines[0]}</div>
            )}
            <div className="app-shell__metaline">{metaLines[1]}</div>
          </div>
        )}
      </div>
      {/* (Chrome packet) `.wrap` — `#stage` beside the event-log column,
          exactly the legacy DOM shape (`.stage` then `.log`, siblings inside
          `.wrap`). `EventLogColumn` (which now owns `#logscope`, moved out
          of the always-rendered standalone span this file used to have —
          see that component's own doc for why: rendering it loose above the
          stage regardless of lens produced the stray uppercase "FLEET" the
          operator caught) is ALWAYS mounted on DESKTOP —
          `visible={showsEventLog(route)}` toggles a CSS `display:none`
          class on it instead of conditionally unmounting (see
          `EventLogColumn.tsx`'s own `visible` doc for why unmounting is
          wrong: legacy's real `#logscope` stays present, with real text,
          even when its ancestor is hidden — `next-parity.spec.ts`'s
          byte-parity goldens for the machine lens depend on that — those
          goldens are captured at a desktop viewport, so gating this mount
          on `!isMobile` below never touches them). A hidden flex item
          doesn't consume row width, so `#stage` still fills the row on its
          own — no separate CSS class needed here.

          (#2107 tabbed-drawer packet) On a PHONE this mount is gone
          entirely — not CSS-hidden, actually unmounted — because the
          events pane now lives inside `<MachineDrawer>`'s own Events tab
          (`PhoneDrawer.tsx`), fed the identical `eventLog*` props passed
          to `MachineDrawer` above. Two live mounts of the same pane at
          once would fight over `dialogManager`'s `modalbg` id and over
          which one "the" event log is; only one of {this mount, the
          drawer's} is ever actually in the DOM for a given viewport. This
          is also the "remove the inline section from the page flow on
          phones so the lens above gets the full height" requirement — the
          lens in `#stage` is no longer followed by a full-width event-log
          section pushing the page's scroll further down. */}
      <div className="app-shell__content">
        <main className="app-shell__stage" id="stage">
          {/* (#2027) There was no error boundary anywhere in this app, so ANY
              render throw in ANY lens unmounted the whole tree and left a
              blank page — every other lens gone with it. Reachable with
              committed data: the machine lens dereferences
              `resources.machine.*` unguarded while chaining
              `resources.pool?.*` beside it, so a trimmed or schema-drifted
              `demo-machine.json` white-screened darkmux.com/demo entirely.

              Keyed on the route so a crash does not persist across
              navigation: switching tabs remounts the boundary, which is the
              recovery an operator will reach for first. */}
          <LensErrorBoundary key={route.kind} name={route.kind}>
            {renderRoute(route, playhead, onMissionEvents, onSelectStep, onStepHeader)}
          </LensErrorBoundary>
        </main>
        {!isMobile && (
          <EventLogColumn
            scopeLabel={eventLogScopeLabel}
            records={eventLogRecords}
            visible={showsEventLog(route)}
            loading={routeRecords.loading}
            error={routeRecords.error}
            historical={eventLogHistorical}
            serverTruncated={eventLogServerTruncated}
            headerExtra={stepHeaderExtra}
          />
        )}
      </div>
    </div>
  );
}

/** `renderCrumb()` (viewer.html:2476-2568) + each lens's own
 * `$("logscope").textContent=` assignment, folded into one lookup keyed on
 * [[Route]]. `machine`/`fleet`/`session`/`playback` all have a real,
 * source-cited `logscope` mapping (added once `EventLogColumn` gave
 * `#logscope` somewhere to render — see that function's own doc); `mission`
 * carries none (its own component owns that chrome, see the `mission`
 * branch below). `unknown` stays at the empty `crumb`/`logscope`
 * default (matching legacy's actual default for those levels — see e.g.
 * `goldens/fleet.txt`'s `(empty)` crumb).
 *
 * `console`/`runs` DO carry a logscope ("console"/"runs", matching
 * `goldens/console.txt` and `runs.txt`) even though their column is hidden.
 * Legacy keeps `#logscope` in the DOM and hides only `.log` via CSS, so
 * `innerText` still falls back to `textContent` there — which is exactly why
 * the column is ALWAYS mounted and merely CSS-hidden (see `EventLogColumn`'s
 * `visible` doc). Corrected at the QA gate: the previous wording claimed both
 * the opposite scope AND conditional unmounting, contradicting the code
 * twenty lines below it. None of
 * these are byte-parity targets for `#crumb` (see each component's own doc
 * for why), so inventing crumb text for them would be UX decoration, not a
 * port. */
function routeChrome(route: Route, targetMachineName: string | null): { crumb: string; logscope: string } {
  if (route.kind === "machine") {
    // `$("crumb").innerHTML = state.machine!=null ? escN(state.machine) :
    // "this machine"` (viewer.html:2537); `$("logscope").textContent =
    // m!=null?nameOf(m):"machine"` (viewer.html:1799). `targetMachineName`
    // (computed below in `App()`) resolves the ROUTE's machine — the local
    // one for `uid: null`, or the drilled uid's own name for a fleet-card
    // drill — matching `escN(state.machine)`'s uid-generic lookup, not
    // hardcoded to "this daemon's own name" the way the pre-drill-in
    // single-route version of this function was.
    return { crumb: targetMachineName ?? "this machine", logscope: targetMachineName ?? "machine" };
  }
  if (route.kind === "fleet") {
    // `$("logscope").textContent="fleet"` (viewer.html:1668) — legacy's
    // literal string is lowercase; `goldens/fleet.txt` shows it UPPERCASE
    // because `#logscope` sits inside `.loglist h3`, which carries
    // `text-transform:uppercase` (that whole event-log sidebar isn't ported
    // yet — see `ui/README.md`'s deferred list). This port has no
    // `.app-shell__logscope` CSS rule at all (the machine lens's real
    // machine-name `logscope` above must render mixed-case, unchanged), so
    // — same discipline as `lib/format.ts`'s "uppercase the STRING
    // directly" helpers — the string here is ALREADY uppercase rather than
    // leaning on a CSS rule this port doesn't have.
    return { crumb: "", logscope: "fleet" };
  }
  // (Chrome packet) `#logscope`'s CASE depends on VISIBILITY, not just its
  // raw JS-set value — a real, verified legacy quirk, not an assumption:
  // `.loglist h3{text-transform:uppercase}` only applies while `#logscope`
  // is actually RENDERED (`showsEventLog(route)` true); once an ancestor
  // gets `display:none` (`runs`/`console`/`machine`), the element is "not
  // rendered" per the CSS spec, and `innerText` falls back to raw
  // `textContent` — NO text-transform applied — same finding
  // `showsEventLog`'s own doc cites for TEXT PRESENCE, extended here to
  // CASE. Proven two ways: (1) `goldens/session-task-list.txt`'s logscope
  // reads "TASK-LIST" (uppercased — session drill-in is a VISIBLE-log
  // route) while `goldens/machine.txt`'s reads "MacBook-Pro" (raw,
  // unchanged — machine is a HIDDEN-log route), both from the SAME
  // `.loglist h3` rule; (2) a throwaway Playwright probe read
  // `getComputedStyle('#logscope').textTransform` as "uppercase" in BOTH
  // states while `innerText` only reflected it in the fleet (visible)
  // state — confirming the CSS cascade is identical, only RENDEREDNESS
  // differs. So each branch below pre-uppercases (or doesn't) to match
  // what CSS would visually do for THAT route's fixed visibility — same
  // "uppercase the STRING directly" discipline as the `fleet`/`machine`
  // branches above, just now visibility-aware instead of uniformly assumed
  // visible.
  if (route.kind === "dispatch") {
    // `$("logscope").textContent=sid` (viewer.html:2042,
    // `renderSubsystem()`) — a VISIBLE-log route, so uppercased.
    return { crumb: "", logscope: route.dispatchId };
  }
  if (route.kind === "playback") {
    // A bare-date hash never reassigns `state.level` away from its `"fleet"`
    // default (verified: no `state.level=` assignment sits on the
    // `targetDate()`/playback boot path — see `showsEventLog`'s doc for the
    // same read), so legacy's `renderFleet()` sets the same `"fleet"`
    // logscope (viewer.html:1668) it does on a live fleet view — VISIBLE,
    // already uppercase.
    //
    // (#2120, operator finding — "reads like a variable name") Used to take
    // the same FLEET-LEVEL crumb branch legacy does, `◆ ${primaryMission()}`
    // — a RAW mission id, right next to the transport's own controls. Empty
    // now: the transport carries a HUMAN label for the same mission itself
    // (`App.tsx`'s own `playbackMissionLabel`, threaded into `Scrubber`'s
    // `label` prop), and the raw id this crumb used to show moved to the
    // Machine info modal's `playback` kv row instead — a real DOM/behavior
    // change from legacy, not a port, made once the transport could carry
    // this information on its own.
    return { crumb: "", logscope: "fleet" };
  }
  if (route.kind === "mission") {
    // `MissionGraphLens` (#1868) owns its own header. Its events pane is
    // retired though (mainstay-unification packet) — the App level now
    // overrides this empty `logscope` with the mission id for the shared
    // column (see `eventLogScopeLabel`'s own doc, above `routeChrome`'s call
    // site). `#crumb` stays empty regardless: the mission header already
    // names the mission, so a crumb would only repeat it.
    return { crumb: "", logscope: "" };
  }
  if (route.kind === "console") {
    // `$("logscope").textContent="console"` (viewer.html:4513) — a HIDDEN-
    // log route (`showsEventLog`), so left RAW/lowercase, matching what
    // legacy's own `textContent` fallback would show if inspected the same
    // way (never visually seen either way, but real for DOM fidelity).
    return { crumb: "", logscope: "console" };
  }
  if (route.kind === "runs") {
    // `$("logscope").textContent="runs"` (viewer.html:4676) — HIDDEN, raw.
    // `$("crumb").innerHTML = state.level==="lab-run" ? esc(state.labRunDir||"—")
    // : ""` (viewer.html:2575, `inRuns` branch) — drill-in packet: `route.run`
    // is only ever populated once the operator is genuinely looking at a
    // lab-run-detail pane (see `route.ts`'s widened `run` doc), matching
    // legacy's own gate exactly.
    return { crumb: route.run ?? "", logscope: "runs" };
  }
  return { crumb: "", logscope: "" };
}

/**
 * `onPlaybackPlayheadChange` is only ever read by the `playback` case below
 * — every other lens ignores the extra argument. Threaded through here
 * (rather than a context) because this function is App's own single
 * dispatch point for `#stage`'s content, not a shared ancestor of unrelated
 * lenses: only `App` (which owns the `playbackPlayheadMs` state this
 * callback writes) and `PlaybackLens` (which is the only lens that ever
 * calls it) know this parameter exists. See `App`'s own
 * `playbackPlayheadMs`/`eventLogRecords` doc for why the event log needs it.
 */
/** The day of the earliest record with a usable timestamp, or null. Not
 * `firstRecordDate` (file order): a session or mission slice from the daemon
 * is not guaranteed to arrive sorted. */
function earliestDate(records: FlowRecord[]): string | null {
  let min = Infinity;
  for (const r of records) {
    const t = T(r.ts);
    if (!Number.isNaN(t) && t < min) min = t;
  }
  return Number.isFinite(min) ? new Date(min).toISOString().slice(0, 10) : null;
}

function renderRoute(
  route: Route,
  playhead: number | null,
  onMissionEvents: (events: FlowRecord[], srvTruncated: boolean) => void,
  onSelectStep: (stepId: string | null) => void,
  onStepHeader: (fields: StepHeaderField[] | null) => void,
) {
  switch (route.kind) {
    case "fleet":
      return <FleetLens />;
    case "runs":
      return <RunsBoard initialKind={route.runsKind} initialRun={route.run} initialMachineUid={route.machine} />;
    case "machine":
      return <MachineLens uid={route.uid} />;
    case "console":
      // (#1911) `route.opts` — already sanitized against the panel's own
      // table by `parseRoute` (or forced by `PANEL_ALIASES`) — seeds the
      // console's per-pill selection memory so a shared link reproduces
      // panel AND variant.
      return <ConsolePanel initialPanelId={route.panelId} initialOpts={route.opts} />;
    case "dispatch":
      // Packet 4: a real fetch to /flow-session/<id> — see SessionReplay's
      // own doc for why the RENDER (not the fetch) is still a not-ported
      // notice.
      return <SessionReplay sessionId={route.dispatchId} playhead={playhead} />;
    case "mission":
      // #1868: the mission-graph lens, folded in-place — see
      // `MissionGraphLens`'s own doc for the data sources and why this
      // replaces the earlier `MissionReplay` full-navigation stub.
      return (
        <MissionGraphLens
          missionId={route.missionId}
          onEvents={onMissionEvents}
          selectedStepId={route.stepId}
          onSelectStep={onSelectStep}
          onStepHeader={onStepHeader}
        />
      );
    case "playback":
      // (#1800 P2) A bare #<date> hash — a REAL historical render now: the
      // fleet hero over that day's records, with every one of legacy's
      // replay-mode branches taken. See PlaybackLens's own doc.
      return <PlaybackLens date={route.date} playhead={playhead} />;
    case "unknown":
      return <LensPlaceholder label="unrecognized" hash={route.hash} />;
  }
}
