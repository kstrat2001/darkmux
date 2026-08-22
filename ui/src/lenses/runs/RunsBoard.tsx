import { Fragment, useEffect, useRef, useState, type KeyboardEvent } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import { canonicalHash, writeHash } from "../../lib/hashSync";
import { missionGraphReachable } from "../../lib/injectedMeta";
import { isStaticBuild, resolveLabRunsSrc, resolveRunsSrc } from "../../lib/staticSource";
import { RUNS_KINDS, type RunsKind } from "../../lib/route";
import { useFlowWindow } from "../../hooks/useFlowWindow";
import { useLiveMachines } from "../../hooks/useLiveMachines";
import { machineNames, nameOf } from "../../lib/flow";
import { LabRunDetail } from "./LabRunDetail";
import type { RunsResponse, LabRunsResponse, LabRun } from "../../types/handwritten";
import type { Run } from "../../types/generated/Run";
import {
  RUNS_CAP,
  runsFiltered,
  runsForMachine,
  runsMultiMachine,
  runsAgo,
  runSubtitle,
  groupLabRunsByTask,
  labKnobSummary,
  labKnobDiff,
  labCounts,
  runDestination,
  MISSION_GRAPH_UNREACHABLE_NOTICE,
  type LabTaskGroup,
} from "./format";

/**
 * The runs board — `#lens=runs` (kind filter over mission/dispatch/lab,
 * `tracked:false` "untracked" ghost rows, the `◧ series` knob-diff sub-view
 * under kind=lab). Pure port of `renderLabRunsList`/`renderRunsBar`/
 * `renderRunRow`/`renderLabTaskCard` in `viewer.html`'s
 * `── the runs lens ──` section — see `format.ts` for the ported pure
 * functions this component composes.
 *
 * Data: `GET /runs` (the flat cross-source view-model, every kind) and
 * `GET /lab/runs` (the lab-only staffing/bundle extras), fetched TOGETHER on
 * every mount — via `staticSource.ts`'s `resolveRunsSrc()`/
 * `resolveLabRunsSrc()` rather than the two literal paths directly, so a
 * static build (`darkmux-runs-src`/`darkmux-lab-runs-src` metas — #1801,
 * viewer.html:4077/4027) reads its committed fixture files instead of
 * hitting a daemon that isn't there. A daemon-served page is unaffected:
 * both resolvers fall back to the exact literal paths this component always
 * used — matching `window.goRuns`'s `Promise.all([loadRuns(),
 * loadLabRuns()])`, not gated by which kind chip is selected (the chip is a
 * client-side re-filter of already-loaded data, never a new fetch — see
 * `window.setRunsKind`). `/missions` and `/phases` are deliberately NOT
 * fetched here: reading `viewer.html`, those two feed ONLY
 * `renderMissionStatic()`, the daemon-less static fallback for `#mission=<id>`
 * that never runs when a daemon is present (this app always has one) — see
 * `tests/parity/README.md`'s lens inventory for why `#mission=<id>` itself is
 * out of scope for this packet. There is no separate "missions board" render
 * target in the legacy viewer distinct from this one filtered by kind=mission.
 *
 * Failure handling deliberately mirrors legacy's own SILENCE rather than the
 * scaffold's usual `fetchJson`-driven visible-error pattern (`FleetStrip`):
 * `loadRuns()`/`loadLabRuns()` both catch a fetch failure into an EMPTY
 * result (`RUNS=[]; RUNS_LOADED=true`), never a distinct error state — Rule 1
 * (pure port, including its silences) governs here; the three-state
 * empty-is-never-silent contract is a later, separate arc (see the root
 * plan). Ledgered as a improvement candidate for that arc, not taken now.
 *
 * Row-click destinations (drill-in packet — both now real, see `RunRow`'s
 * own doc for the split):
 * - a `kind==="lab"` row opens `LabRunDetail` (`data-act="labrun"` in
 *   legacy, `drillLabRun(dir)`) — an in-component state swap (`labRunDir`),
 *   NOT a route change, matching legacy's own mechanism exactly: `render()`
 *   just swaps `$("stage").innerHTML` and syncs the address bar via
 *   `history.replaceState` (`syncLabHash`), it never fires a real navigation
 *   either. `initialRun` (from `route.run`, itself from the `run=` hash
 *   param) seeds `labRunDir` on mount/deep-link, so pasting a `run=` URL
 *   lands here directly — same `initialKind` echo-guard pattern below,
 *   widened to cover both.
 * - a tracked (mission/dispatch) row opens the mission-graph lens
 *   (`data-act="gomission"` in legacy, `ACTIONS.gomission→
 *   goMissionGraph(id)`, a REAL cross-DOCUMENT navigation there) — this port
 *   instead sets `location.hash = "mission=<id>"` (#1868), an IN-APP hop to
 *   `MissionGraphLens`, when `missionGraphReachable()` (`lib/injectedMeta.ts`)
 *   says a daemon is actually behind this page. The gate itself is
 *   unchanged from before #1868: the lens still needs a real
 *   `/mission/:id/graph.json` endpoint to fetch from, which a daemon-less
 *   static build doesn't have — only the DESTINATION changed, from a
 *   cross-document `location.href` to an in-app hash. The daemon-less
 *   fallback (`renderMissionStatic()`'s static summary) is genuinely out of
 *   scope — see `MISSION_GRAPH_UNREACHABLE_NOTICE`'s own doc for why a
 *   named notice stands in for it instead.
 * - (#1900, widened #1915) an untracked row that carries a `session_id`
 *   opens the session drill instead: `location.hash = "session=<id>"`,
 *   straight to `SessionReplay` (`/flow-session/<id>`) — ungated, no
 *   `missionGraphReachable()` check, same precedent as `FleetLens.tsx`'s
 *   activity-lane bars. This started (#1900) as a `kind==="dispatch"`-only
 *   carve-out — a ghost dispatch always has a real trajectory behind it
 *   even with no mission ever minted (`ghost_runs` only synthesizes one for
 *   a session that actually saw a `dispatch start`) — but that premise
 *   turned out just as true for an untracked MISSION row (a peer's, #1705,
 *   or a local ephemeral with no durable record): the server picks the
 *   same representative session for it that it already computed for role/
 *   model/route, just never carried out to the client before #1915. The
 *   rule is now kind-agnostic: "untracked, has a `session_id`" opens a
 *   session; "untracked, no `session_id`" is the only row with genuinely
 *   nothing to open (see `runDestination`, `format.ts`, for the full
 *   reasoning).
 *
 * Hash write-back (Packet 1.5): a kind-chip click changes no `Route` (no
 * `hashchange` fires — this is purely local state), so `App`'s route-keyed
 * `useSyncHash` effect (`lib/hashSync.ts`) would never see it. `selectKind`
 * therefore calls `writeHash`/`canonicalHash` DIRECTLY, at the moment of the
 * click, constructing the same `{kind:"runs", runsKind:k}` shape the route
 * parser would have produced had the operator arrived via a `kind=`
 * deep-link — so the address bar always names the filter actually on
 * screen, matching legacy's `setRunsKind`'s own `render()` (which calls
 * `syncLabHash` every time, chip clicks included).
 */
/** `goMissionGraph(id)`'s daemon-less fallback (viewer.html:2733-2736:
 * `if(missionGraphReachable()){location.href=...;return;} state.level="mission";...`)
 * renders `renderMissionStatic()` — a whole separate static-summary render
 * surface that only exists to serve the daemon-less GitHub Pages demo (see
 * `missionGraphReachable`'s own doc: real `/next` deployments, served by
 * `darkmux serve`, always inject `darkmux-mode` and never hit this branch).
 * Genuinely out of scope here — building a second summary page for a
 * corner case this app's real deployment target never reaches is scope
 * creep wearing this packet's clothes (the SAME judgment call
 * `MissionReplay.tsx`/`PlaybackLens.tsx` already made for their own
 * daemon-less edges). A named, honest notice stands in instead, per the
 * operator-authored posture: a stuck feature gets a visible placeholder,
 * not a silent no-op.
 *
 * The notice text itself (`MISSION_GRAPH_UNREACHABLE_NOTICE`) moved to
 * `format.ts` (#1904 QA fix) — shared with `runDestination`'s own
 * `unreachable` branch so both places that report this case say the same
 * thing rather than maintaining separate copies (a second caller,
 * `ActivityPanel.tsx`, used to hit the identical branch; it is deleted,
 * #1905 step 3, but the shared-text discipline holds regardless of how
 * many callers there are). Matches `onLabRunUnresolvable`'s own `"run
 * detail needs a running daemon — …"` phrasing below, which stays local
 * since nothing else needs it. */

export function RunsBoard({
  initialKind,
  initialRun,
  initialMachineUid,
}: {
  initialKind: RunsKind;
  initialRun: string | null;
  /** (#1809) The route's `machine=` pin — `null` means every machine, the
   * pre-existing behavior. Seeded the same way `initialKind`/`initialRun`
   * are (local state, re-synced on a genuine deep-link change below) rather
   * than read live off the route on every render, for the same reason: the
   * kind chips already own a piece of state outside `Route` (see this
   * file's own module doc), and the machine pin composes with it the same
   * way. */
  initialMachineUid: string | null;
}) {
  const [kind, setKind] = useState<RunsKind>(initialKind);
  const [series, setSeries] = useState(false);
  const [showAll, setShowAll] = useState(false);
  const [rowClickNotice, setRowClickNotice] = useState<string | null>(null);
  const [machineUid, setMachineUid] = useState<string | null>(initialMachineUid);
  // `state.labRunDir` (viewer.html) — which lab run (if any) this board is
  // showing the detail pane for. Seeded from `initialRun`, independent of
  // `kind` — a lab row (and so this drill-in) is reachable from BOTH
  // kind=all and kind=lab (every other kind filter excludes lab rows
  // entirely, see `runsFiltered`), matching legacy's own
  // `state.level==="lab-run"` gate, which is independent of
  // `state.runsKind` too (see `route.ts`'s widened `run` doc).
  const [labRunDir, setLabRunDir] = useState<string | null>(initialRun);

  // `drillLabRun(dir)` (viewer.html:4101-4131), reduced to the address-bar
  // half — `LabRunDetail` itself owns the two real fetches (detail +
  // events poll). An in-component state swap, NOT a route change: legacy's
  // own mechanism here is a `render()` stage-swap + `history.replaceState`
  // sync, never a real navigation — see this file's own module doc.
  // `runsKind: kind` (not hardcoded "lab") preserves whichever kind filter
  // was actually active when the operator clicked in — matching legacy's
  // own `syncLabHash`, which writes `kind=` from `state.runsKind` and `run=`
  // from `state.labRunDir` as two independent fields on the same hash.
  function openLabRun(dir: string) {
    setLabRunDir(dir);
    setRowClickNotice(null);
    writeHash(canonicalHash({ kind: "runs", runsKind: kind, run: dir, machine: machineUid }));
  }

  // The lab-run detail's own "‹ runs" back link (viewer.html:4852/4862,
  // `data-act="runs"` → `window.goRuns`... except `goRuns` ALSO re-fetches;
  // this board's `/runs`+`/lab/runs` queries are still live/cached from
  // before the drill-in, so a bare state-clear is the faithful equivalent
  // here, not a redundant re-fetch).
  function closeLabRun() {
    setLabRunDir(null);
    writeHash(canonicalHash({ kind: "runs", runsKind: kind, run: null, machine: machineUid }));
  }

  // (drill-in packet) A one-shot suppression flag for the deep-link re-sync
  // guard just below `onLabRunUnresolvable`, needed for a race that ONLY
  // this callback hits (measured live, not theorized): every OTHER writer
  // here (`openLabRun`/`closeLabRun`/`selectKind`/`clearMachinePin`) fires
  // from a DOM `onClick`, so React's own event-batching keeps its state
  // setters and `writeHash`'s synthetic `hashchange` in the SAME commit —
  // the guard's `initialRun === labRunDir` check sees both sides already
  // agreeing and never fires. `onLabRunUnresolvable` instead fires from
  // `LabRunDetail`'s `useEffect` (a passive effect, outside any click's
  // batch scope): `hashchange` forces an EARLIER, separate synchronous
  // commit carrying the new route but the OLD `labRunDir` state, queuing
  // the guard's effect against that stale pairing — REGARDLESS of whether
  // `writeHash` or the state setters run first in this function (both
  // orderings were measured live and both raced: state-then-hash let the
  // guard's reset land AFTER this function's own `setRowClickNotice`,
  // clobbering the message; hash-then-state let the guard's reset win
  // instead, since its own queued update lands after this function's). The
  // flag sidesteps the ordering question entirely: set it before touching
  // anything, and the guard (below) treats its own firing as an ECHO of
  // THIS call rather than a fresh external deep-link, skipping its reset.
  const suppressResyncRef = useRef(false);

  // (drill-in packet) `LabRunDetail`'s `onUnresolvable` — the port of
  // legacy's `drillLabRun` fallback (see that component's own doc):
  // `state.level="runs"; state.labRunDir=null;` plus a notice, never a stuck
  // error page. `MISSION_GRAPH_UNREACHABLE_NOTICE`-style daemon-awareness
  // lives here (not in `LabRunDetail`) because this board already owns the
  // other daemon-reachability notice, via the SAME `missionGraphReachable()`
  // predicate legacy's own `drillLabRun` branches on — the honest reason on
  // a daemon-less static build is the missing capability, not a stale link
  // (matching `MISSION_GRAPH_UNREACHABLE_NOTICE`'s identical judgment call).
  function onLabRunUnresolvable() {
    const dir = labRunDir;
    suppressResyncRef.current = true;
    setLabRunDir(null);
    setRowClickNotice(
      !missionGraphReachable()
        ? "run detail needs a running daemon — this static build lists runs without their per-run pipeline and event feed."
        : `couldn't open run "${dir}" — it may have been removed, or the link is stale. Showing the run list.`,
    );
    writeHash(canonicalHash({ kind: "runs", runsKind: kind, run: null, machine: machineUid }));
  }

  // (#1809) Clears the machine pin — the "back to all machines" half of
  // "visible AND clearable" the packet brief requires. Preserves whichever
  // kind filter/lab-run drill-in was active, matching `selectKind`'s own
  // "only the ONE thing that changed" discipline below.
  function clearMachinePin() {
    setMachineUid(null);
    setShowAll(false);
    writeHash(canonicalHash({ kind: "runs", runsKind: kind, run: labRunDir, machine: null }));
  }

  // `ACTIONS.labrun`/`ACTIONS.gomission` (viewer.html:2991, folded per-row
  // in `renderRunRow`'s own `data-act` choice) — the row-click dispatch
  // every interactive `RunRow` funnels through. `LabRunRow` (the series
  // view) skips this entirely and calls `openLabRun` directly — every row
  // there is unconditionally a lab row, so there's no kind to dispatch on.
  //
  // (#1900) `tracked` used to gate whether a row could be opened at all —
  // "an untracked ghost has nothing to open". That premise was false for a
  // `kind: "dispatch"` row: server-side, `ghost_runs` (`crates/darkmux-
  // serve/src/runs.rs`) only ever synthesizes an untracked dispatch row for
  // a flow session that saw a real `dispatch start` record (its `has_start`
  // gate), so it always has a real `/flow-session/<id>` behind it — a
  // trajectory, metrics, per-step records — even with no mission graph.
  //
  // (#1915) The same premise turned out false for an untracked MISSION row
  // too — 40 of 104 rows on the reported machine, the entire newest page a
  // person actually sees. The server now carries the SAME representative-
  // session pick uniformly, as `Run.session_id`, for every kind that has
  // one; the client-side rule generalized to match: "untracked, but carries
  // a `session_id`" opens a session, ANY kind, no more per-kind arms. The
  // one case that still has genuinely nothing to open is an untracked row
  // with no `session_id` at all — a peer mission this daemon knows only
  // from a terminal record, with no dispatch session ever joined to it
  // (`flow_mission_to_run`, #1705). `runDestination`'s own doc (`format.ts`)
  // has the full reasoning, including why a TRACKED mission never takes
  // this branch even though it also carries a `session_id`.
  //
  // (#1904 QA fix) The decision itself moved to the shared `runDestination`
  // (`format.ts`) — this function only decides what to DO with each
  // outcome: an in-page state swap for "lab" (`openLabRun`), a hash
  // navigation for everything else.
  function activateRun(run: Run) {
    const dest = runDestination(run, missionGraphReachable());
    switch (dest.kind) {
      case "lab":
        openLabRun(dest.dir);
        return;
      case "hash":
        // (#1868) In-app navigation (a real `hashchange`, not a
        // cross-document `location.href` or a silent `history.replaceState`)
        // so both the session-drill and mission-graph destinations are
        // normal, back-button-friendly navigations, matching how an
        // operator reaches every other in-app destination.
        location.hash = dest.hash;
        return;
      case "unreachable":
        setRowClickNotice(MISSION_GRAPH_UNREACHABLE_NOTICE);
        return;
      case "none":
        return; // a peer mission with no local session: nothing to open here
    }
  }

  // A fresh deep-link into a DIFFERENT kind or run while this component is
  // already mounted (route.runsKind/route.run changes without the runs
  // lens itself unmounting) re-syncs local filter state — mirrors
  // `window.goRuns` resetting `state.runsAll=false` on every fresh entry
  // into the lens.
  //
  // QA must-fix (2026-08-09): the guard below protects against the
  // WRITE-BACK ECHO Packet 1.5 armed. `selectKind`/`openLabRun`/
  // `closeLabRun`'s own `writeHash` calls fire `history.replaceState`,
  // which — same as legacy's `syncLabHash` — never dispatches
  // `hashchange`. `useHashRoute`'s module-level `cachedHref` therefore
  // goes stale; the NEXT App re-render for ANY unrelated reason (the
  // presence poll, a query refetch — anything that touches
  // `location.href` freshly) recomputes a `Route` whose `runsKind`/`run`
  // now match what the operator already clicked, and without this guard
  // `initialKind`/`initialRun` would look like a FRESH deep-link, silently
  // resetting `series`/`showAll`/the row-click notice out from under an
  // operator who touched nothing. The guard is exactly "only a GENUINE
  // change resets" — true both for a real deep-link (arriving on a
  // different `kind=`/`run=`) and for the FIRST render after mount (`kind`/
  // `labRunDir` seeded from the initial props, so they're already equal
  // and this effect no-ops on mount too, matching its prior behavior
  // there). Two alternatives that look right and aren't (ruled out during
  // the original kind-only version of this guard): pinning `useHashRoute`'s
  // cache would leave THIS board stale after a later nav-tab click away and
  // back; switching the writes to `location.hash = ...` would fire
  // `hashchange` (fixing this symptom) but ALSO push a new history entry
  // per click, breaking legacy's "lens hops must not spam history" contract
  // (`syncLabHash`'s own comment) — `replaceState` is the whole reason
  // legacy's mechanism exists.
  // (#1809) `initialMachineUid` joins the same guard, same reasoning: a
  // genuine deep-link onto a DIFFERENT machine pin re-syncs local state;
  // this component's OWN `clearMachinePin`/machine-chip writes echo back
  // through `writeHash` without a `hashchange`, so without the guard they'd
  // look like a fresh deep-link and reset `series`/`showAll` right after
  // the operator clicked.
  useEffect(() => {
    // (drill-in packet) See `suppressResyncRef`'s own doc above
    // `onLabRunUnresolvable` — this run of the effect IS that call's own
    // echo (its `writeHash` is what changed `initialRun`), so treat it as
    // already handled rather than re-deriving the same reset a second,
    // conflicting way.
    if (suppressResyncRef.current) {
      suppressResyncRef.current = false;
      return;
    }
    if (initialKind === kind && initialRun === labRunDir && initialMachineUid === machineUid) return;
    setKind(initialKind);
    setSeries(false);
    setShowAll(false);
    setRowClickNotice(null);
    setLabRunDir(initialRun);
    setMachineUid(initialMachineUid);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [initialKind, initialRun, initialMachineUid]);

  // These stay unconditional (React's rules-of-hooks — a hook can't sit
  // after the early return below) even though the lab-run-detail branch
  // doesn't need their data: `LabRunDetail` owns its own fetches
  // (`/lab/run/detail` + the events poll), so this is cache warmth for
  // when the operator backs out, not a blocking dependency (matching
  // legacy: `drillLabRun` never blocks on `loadRuns()`/`loadLabRuns()`
  // either — the two are genuinely independent fetches there too).
  const runsQuery = useQuery({
    queryKey: queryKeys.runs(),
    queryFn: () => fetchJson<RunsResponse>(resolveRunsSrc()),
  });
  const labRunsQuery = useQuery({
    queryKey: queryKeys.labRuns(),
    queryFn: () => fetchJson<LabRunsResponse>(resolveLabRunsSrc()),
  });

  // (#1809) Also unconditional, same rules-of-hooks reason as the two
  // queries above — needed only to resolve a machine PIN's full alias set
  // (`machineNames`, see `format.ts::runsForMachine`'s own doc for why a
  // single resolved label isn't enough), but App.tsx ALREADY runs both of
  // these on every route (its own `flowWindow`/`liveMachines`, used for
  // `#meta` and the machine-lens crumb) — same "cache reuse, not a second
  // network round trip" TanStack dedup App.tsx's own module doc names for
  // `MachineLens`. `useLiveMachines` is gated the same way every OTHER
  // live-only poll in this app is (`isStaticBuild()` — see that hook's own
  // doc and `MachineLens.tsx`'s identical gate): a daemon-less static build
  // has no `/fleet/machines/live` to poll.
  const nowMs = Date.now();
  const daemonBacked = !isStaticBuild();
  const flowWindow = useFlowWindow(nowMs);
  const liveMachines = useLiveMachines(daemonBacked);

  // The lab-run detail pane is its own top-level render, reached without
  // waiting on the two queries above and independent of `kind` (see
  // `labRunDir`'s own doc above for why it isn't gated to kind==="lab").
  if (labRunDir) {
    return <LabRunDetail dir={labRunDir} onBack={closeLabRun} onUnresolvable={onLabRunUnresolvable} />;
  }

  // Pending: neither query has resolved yet (matches `RUNS_LOADED===null`).
  if (!runsQuery.data || !labRunsQuery.data) {
    return (
      <div data-state="pending" role="status" aria-label="Loading runs">
        <div className="stagehdr">runs</div>
        <div className="none">loading…</div>
      </div>
    );
  }

  const runs: Run[] = runsQuery.data.ok ? runsQuery.data.data.runs : [];
  const labConfigured = labRunsQuery.data.ok ? labRunsQuery.data.data.configured !== false : false;
  const labDir = labRunsQuery.data.ok ? labRunsQuery.data.data.dir : null;
  const labDirExists = labRunsQuery.data.ok ? labRunsQuery.data.data.exists : null;
  const labRuns: LabRun[] = labRunsQuery.data.ok ? labRunsQuery.data.data.runs : [];

  // (#1809) The machine pin, applied ONCE here so every derivation below
  // (kind counts, the lab-source notice, `showMachine`, the series grouping,
  // the flat row list) sees the already-scoped set rather than each
  // re-deriving its own filter — see `format.ts::runsForMachine`'s own doc
  // for the alias-matching rationale and the "50 missions + 15 dispatches
  // carry no machine at all" exclusion it names.
  const pinnedMachineName = machineUid != null ? nameOf(flowWindow.data, liveMachines, machineUid) : null;
  const scopedRuns = machineUid != null ? runsForMachine(runs, machineNames(flowWindow.data, liveMachines, machineUid)) : runs;
  // `LabRun` (the `/lab/runs` series-view source) carries no machine field
  // at all (`crates/darkmux-serve/src/lib.rs::LabRunSummary` — verified,
  // not assumed) — bridged via the ONE field the two sources share: a lab
  // run's `dir` IS its `Run.id` (`runs.rs::lab_summary_to_run`: `id:
  // summary.dir.clone()`). So the lab-kind subset of `scopedRuns` (already
  // machine-filtered) names exactly which dirs belong to the pin.
  const scopedLabRuns =
    machineUid != null
      ? labRuns.filter((r) => scopedRuns.some((run) => run.kind === "lab" && run.id === r.dir))
      : labRuns;

  function selectKind(k: RunsKind) {
    setKind(k);
    setShowAll(false);
    setRowClickNotice(null);
    if (k !== "lab") setSeries(false);
    writeHash(canonicalHash({ kind: "runs", runsKind: k, run: null, machine: machineUid }));
  }

  // viewer.html: `labSourceNotice()`.
  let notice: string | null = null;
  if (kind === "lab" && !scopedRuns.some((r) => r.kind === "lab")) {
    if (!labConfigured) {
      notice = "this daemon has no lab-run source wired — darkmux doctor shows the resolved dirs.lab and where it came from.";
    } else if (labDirExists === false) {
      notice = `the configured lab dir does not exist yet${labDir ? ` (${labDir})` : ""} — it appears with the first run.`;
    } else {
      notice = `no lab runs found under the configured lab dir${labDir ? ` (${labDir})` : ""}.`;
    }
  }

  const showMachine = runsMultiMachine(scopedRuns);
  const bar = (
    <RunsBar
      counts={countsByKind(scopedRuns)}
      kind={kind}
      series={series}
      onKind={selectKind}
      onSeries={() => setSeries((s) => !s)}
      pinnedMachineName={pinnedMachineName}
      onClearMachine={clearMachinePin}
    />
  );

  if (kind === "lab" && series) {
    const groups = groupLabRunsByTask(scopedLabRuns);
    return (
      <div data-state="data">
        <div className="stagehdr">
          runs · lab series · {scopedLabRuns.length} run{scopedLabRuns.length === 1 ? "" : "s"} · {groups.length} task
          {groups.length === 1 ? "" : "s"}
        </div>
        {bar}
        {rowClickNotice && (
          <div className="labnotice" role="status">
            {rowClickNotice}
          </div>
        )}
        {notice && <div className="labnotice">{notice}</div>}
        <div className="lablist">
          {groups.length ? (
            groups.map((g) => <LabTaskCard key={g.key} group={g} onRowActivate={openLabRun} />)
          ) : (
            <div className="none">
              no lab runs with a recorded corpus yet — run <code>darkmux lab eval --funnel …</code> to produce one.
            </div>
          )}
        </div>
      </div>
    );
  }

  const rows = runsFiltered(scopedRuns, kind);
  const shown = showAll ? rows : rows.slice(0, RUNS_CAP);
  const more = rows.length - shown.length;
  const scope = kind === "all" ? "" : ` · ${kind}`;
  const count = showAll || more <= 0 ? `${rows.length} run${rows.length === 1 ? "" : "s"}` : `newest ${shown.length} of ${rows.length}`;

  return (
    <div data-state="data">
      <div className="stagehdr">
        runs{scope} · {count}
      </div>
      {bar}
      {rowClickNotice && (
        <div className="labnotice" role="status">
          {rowClickNotice}
        </div>
      )}
      {notice && <div className="labnotice">{notice}</div>}
      <div className="lablist">
        {shown.length ? (
          <>
            {shown.map((r) => (
              <RunRow key={r.id} run={r} showMachine={showMachine} onActivate={() => activateRun(r)} />
            ))}
            {more > 0 && (
              <div
                className="runmore"
                role="button"
                tabIndex={0}
                onClick={() => setShowAll(true)}
                onKeyDown={onActivateKeyDown(() => setShowAll(true))}
              >
                show all {rows.length} — {more} more
              </div>
            )}
          </>
        ) : (
          <div className="none">no {kind === "all" ? "" : `${kind} `}runs recorded yet.</div>
        )}
      </div>
    </div>
  );
}

function countsByKind(runs: Run[]): Record<string, number> {
  const counts: Record<string, number> = { all: runs.length };
  RUNS_KINDS.slice(1).forEach((k) => {
    counts[k] = runs.filter((r) => r.kind === k).length;
  });
  return counts;
}

/** viewer.html: `function renderRunsBar()` — PLUS the machine-pin chip
 * (#1809), which has no legacy namesake (the runs lens gained a machine
 * dimension only in this port). Deliberately reuses the `.runchip` idiom
 * the kind/series chips already establish rather than inventing a second
 * visual language for "a filter is active, click to change it" — see
 * `RunsBoard.tsx`'s own module doc / #1809 for why this is the pinned
 * state's ONE required property: visible (the chip names the machine) and
 * clearable (clicking it is `clearMachinePin`, the "back to all machines"
 * path). Rendered only when a pin is actually set — an unpinned board's
 * `.runsbar` is byte-identical to before this packet, which is what keeps
 * `goldens/runs.txt`'s existing byte-parity assertion untouched. */
function RunsBar({
  counts,
  kind,
  series,
  onKind,
  onSeries,
  pinnedMachineName,
  onClearMachine,
}: {
  counts: Record<string, number>;
  kind: RunsKind;
  series: boolean;
  onKind: (k: RunsKind) => void;
  onSeries: () => void;
  /** `null` = no machine pin — the pre-existing "every machine" board. */
  pinnedMachineName: string | null;
  onClearMachine: () => void;
}) {
  return (
    <div className="runsbar">
      {RUNS_KINDS.map((k) => (
        <span
          key={k}
          className={`runchip${kind === k ? " on" : ""}`}
          data-arg={k}
          role="button"
          tabIndex={0}
          onClick={() => onKind(k)}
          onKeyDown={onActivateKeyDown(() => onKind(k))}
        >
          {k}
          <span className="runchipn"> {counts[k] ?? 0}</span>
        </span>
      ))}
      {kind === "lab" && (
        <span
          className={`runchip${series ? " on" : ""}`}
          data-act="runsseries"
          data-arg="series"
          role="button"
          tabIndex={0}
          onClick={onSeries}
          onKeyDown={onActivateKeyDown(onSeries)}
        >
          ◧ series
        </span>
      )}
      {pinnedMachineName != null && (
        <span
          className="runchip on"
          data-act="clearmachine"
          role="button"
          tabIndex={0}
          onClick={onClearMachine}
          onKeyDown={onActivateKeyDown(onClearMachine)}
          title="clear the machine filter — show runs from every machine"
        >
          machine: {pinnedMachineName} ✕
        </span>
      )}
    </div>
  );
}

/** Enter/Space activates a `role="button"` `<div>` the same way a native
 * `<button>` would — needed because none of these rows ARE a real `button`
 * element (matching legacy's own non-semantic `data-act` div pattern), so
 * the browser grants no built-in keyboard activation. */
function onActivateKeyDown(onActivate: () => void) {
  return (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onActivate();
    }
  };
}

/** viewer.html: `function runStatusBadge(r)` + `function renderRunRow(r,
 * showMachine)`. The `data-act="labrun"/"gomission"` click destinations
 * (drill-in packet: both real now — lab-run detail opens in-page,
 * mission/dispatch opens `/mission/<id>/graph` — see `RunsBoard`'s own
 * `activateRun` for the dispatch) — the row keeps its `role="button"`/
 * `tabIndex` affordance for openable rows (see `interactive` below), and
 * `onActivate` is now the REAL per-row action, not a placeholder notice.
 *
 * (#1915) `interactive` is now "has a destination", period — no more
 * kind-specific arms. It used to read `run.kind === "lab" || run.tracked
 * || run.kind === "dispatch"`, which is the exact bug #1915 reported: that
 * expression hard-codes "dispatch" as the one kind allowed to be untracked
 * AND openable, so an untracked MISSION row — the kind a person actually
 * browses most, and 40 of 104 rows on the reported machine — read as flat
 * even when it carried a perfectly good session to open. `runDestination`
 * is the single source of truth for whether a row has anywhere to go
 * (`kind !== "none"`); asking it here instead of re-deriving a parallel
 * rule means a future kind that gains the same shape needs no new arm on
 * either side. `missionGraphReachable()` deliberately does NOT gate this:
 * an "unreachable" destination is still a REAL destination (the row shows
 * `MISSION_GRAPH_UNREACHABLE_NOTICE` on click, not silence), so it must
 * stay interactive — only `kind === "none"` means truly nothing to open.
 *
 * (#1904) Was exported for `ActivityPanel.tsx` (the console lens's then
 * default recent-activity landing view) to reuse verbatim, rather than
 * re-deriving the badge/kind-chip/subtitle DOM a second time. That view is
 * deleted (#1905 step 3 — `run-list`, a real CLI panel, supersedes it),
 * leaving `RunsBoard` as the only caller; kept module-private now rather
 * than exported with no consumer. */
function RunRow({ run, showMachine, onActivate }: { run: Run; showMachine: boolean; onActivate: () => void }) {
  const interactive = runDestination(run, missionGraphReachable()).kind !== "none";
  const ago = runsAgo(run);
  const subtitle = runSubtitle(run, showMachine);
  return (
    <div
      className={`labrunrow${interactive ? "" : " flat"}`}
      {...(interactive
        ? { role: "button" as const, tabIndex: 0, onClick: onActivate, onKeyDown: onActivateKeyDown(onActivate) }
        : {})}
    >
      <div className="labrunmain">
        <span className={`labbadge ${run.status}`}>{run.status}</span>
        <span className={`runkind ${run.kind}`}>{run.kind}</span>
        <span className="labruncrew">{run.id}</span>
        {ago && <span className="labrundir">{ago}</span>}
        {!run.tracked && <span className="rununtracked">untracked</span>}
      </div>
      {subtitle && <div className="labrunmeta dim">{subtitle}</div>}
    </div>
  );
}

/** viewer.html: `function labBadge(run)` — the SEPARATE lab-series badge
 * (finished/live), distinct from `runStatusBadge`'s six-status badge above
 * (#1881 added `unparseable`). */
function LabBadge({ finished }: { finished: boolean }) {
  return <span className={`labbadge ${finished ? "finished" : "live"}`}>{finished ? "finished" : "● live"}</span>;
}

/** viewer.html: `function renderLabRunRow(run)` (the series-view row, reading
 * `LabRun`'s own richer fields — NOT `renderRunRow`/`Run` above). Every
 * series row is interactive in legacy (it always opens the lab-run detail
 * pane) — `onActivate` now really does. */
function LabRunRow({ run, onActivate }: { run: LabRun; onActivate: () => void }) {
  return (
    <div className="labrunrow" role="button" tabIndex={0} onClick={onActivate} onKeyDown={onActivateKeyDown(onActivate)}>
      <div className="labrunmain">
        <LabBadge finished={run.finished} />
        <span className="labruncrew">{run.crew || "(crew unknown)"}</span>
        <span className="labrundir">{run.dir}</span>
      </div>
      <div className="labrunmeta">{labKnobSummary(run)}</div>
      <div className="labrunmeta dim">
        {labCounts(run)}
        {run.degenerate && <span className="labdegenerate"> · ⚠ degenerate</span>}
      </div>
    </div>
  );
}

/** viewer.html: `function renderLabTaskCard(group)`. `onRowActivate` takes
 * the DIR to open (curried per-row below) — `openLabRun` itself. */
function LabTaskCard({ group, onRowActivate }: { group: LabTaskGroup; onRowActivate: (dir: string) => void }) {
  return (
    <div className="labtaskcard">
      <div className="labtaskhdr">
        {group.key} <span className="labtaskcount">{group.runs.length} run{group.runs.length === 1 ? "" : "s"}</span>
      </div>
      {group.runs.map((r, i) => {
        const prev = group.runs[i + 1]; // newest-first; i+1 = next OLDER run
        const diff = prev ? labKnobDiff(prev, r) : null;
        return (
          <Fragment key={r.dir}>
            <LabRunRow run={r} onActivate={() => onRowActivate(r.dir)} />
            {prev &&
              (diff && diff.length ? (
                <div className={`labdiffline${diff.length > 1 ? " warn" : ""}`}>
                  {diff.length > 1 ? "⚠ multi-variable change: " : "changed vs previous run: "}
                  {diff.join(" · ")}
                </div>
              ) : (
                <div className="labdiffline dim">no knob change vs previous run</div>
              ))}
          </Fragment>
        );
      })}
    </div>
  );
}
