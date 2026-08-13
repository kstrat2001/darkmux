import { Fragment, useEffect, useState, type KeyboardEvent } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import { canonicalHash, writeHash } from "../../lib/hashSync";
import { missionGraphReachable } from "../../lib/injectedMeta";
import { resolveLabRunsSrc, resolveRunsSrc } from "../../lib/staticSource";
import { RUNS_KINDS, type RunsKind } from "../../lib/route";
import { LabRunDetail } from "./LabRunDetail";
import type { RunsResponse, LabRunsResponse, LabRun } from "../../types/handwritten";
import type { Run } from "../../types/generated/Run";
import {
  RUNS_CAP,
  runsFiltered,
  runsMultiMachine,
  runsAgo,
  runSubtitle,
  groupLabRunsByTask,
  labKnobSummary,
  labKnobDiff,
  labCounts,
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
 * - a tracked (mission/dispatch) row opens `/mission/<id>/graph`
 *   (`data-act="gomission"`, `ACTIONS.gomission→goMissionGraph(id)`) — a
 *   REAL cross-document navigation (`location.href=`), exactly matching
 *   legacy, when `missionGraphReachable()` (`lib/injectedMeta.ts`) says a
 *   daemon is actually behind this page. The daemon-less fallback
 *   (`renderMissionStatic()`'s static summary) is genuinely out of scope —
 *   see `MISSION_GRAPH_UNREACHABLE_NOTICE`'s own doc for why a named notice
 *   stands in for it instead.
 * - an untracked ghost row has nothing to open (unchanged: `interactive`
 *   stays false, no click affordance at all).
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
 * not a silent no-op. */
const MISSION_GRAPH_UNREACHABLE_NOTICE =
  "mission graph needs a running daemon behind this page, which this one doesn't have — open it in the classic viewer at / instead";

export function RunsBoard({ initialKind, initialRun }: { initialKind: RunsKind; initialRun: string | null }) {
  const [kind, setKind] = useState<RunsKind>(initialKind);
  const [series, setSeries] = useState(false);
  const [showAll, setShowAll] = useState(false);
  const [rowClickNotice, setRowClickNotice] = useState<string | null>(null);
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
    writeHash(canonicalHash({ kind: "runs", runsKind: kind, run: dir }));
  }

  // The lab-run detail's own "‹ runs" back link (viewer.html:4852/4862,
  // `data-act="runs"` → `window.goRuns`... except `goRuns` ALSO re-fetches;
  // this board's `/runs`+`/lab/runs` queries are still live/cached from
  // before the drill-in, so a bare state-clear is the faithful equivalent
  // here, not a redundant re-fetch).
  function closeLabRun() {
    setLabRunDir(null);
    writeHash(canonicalHash({ kind: "runs", runsKind: kind, run: null }));
  }

  // `ACTIONS.labrun`/`ACTIONS.gomission` (viewer.html:2991, folded per-row
  // in `renderRunRow`'s own `data-act` choice) — the row-click dispatch
  // every interactive `RunRow` funnels through. `LabRunRow` (the series
  // view) skips this entirely and calls `openLabRun` directly — every row
  // there is unconditionally a lab row, so there's no kind to dispatch on.
  function activateRun(run: Run) {
    if (run.kind === "lab") {
      openLabRun(run.id);
      return;
    }
    if (!run.tracked) return; // an untracked ghost has nothing to open
    if (missionGraphReachable()) {
      location.href = `/mission/${encodeURIComponent(run.id)}/graph`;
      return;
    }
    setRowClickNotice(MISSION_GRAPH_UNREACHABLE_NOTICE);
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
  useEffect(() => {
    if (initialKind === kind && initialRun === labRunDir) return;
    setKind(initialKind);
    setSeries(false);
    setShowAll(false);
    setRowClickNotice(null);
    setLabRunDir(initialRun);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [initialKind, initialRun]);

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

  // The lab-run detail pane is its own top-level render, reached without
  // waiting on the two queries above and independent of `kind` (see
  // `labRunDir`'s own doc above for why it isn't gated to kind==="lab").
  if (labRunDir) {
    return <LabRunDetail dir={labRunDir} onBack={closeLabRun} />;
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

  function selectKind(k: RunsKind) {
    setKind(k);
    setShowAll(false);
    setRowClickNotice(null);
    if (k !== "lab") setSeries(false);
    writeHash(canonicalHash({ kind: "runs", runsKind: k, run: null }));
  }

  // viewer.html: `labSourceNotice()`.
  let notice: string | null = null;
  if (kind === "lab" && !runs.some((r) => r.kind === "lab")) {
    if (!labConfigured) {
      notice = "this daemon has no lab-run source wired — darkmux doctor shows the resolved dirs.lab and where it came from.";
    } else if (labDirExists === false) {
      notice = `the configured lab dir does not exist yet${labDir ? ` (${labDir})` : ""} — it appears with the first run.`;
    } else {
      notice = `no lab runs found under the configured lab dir${labDir ? ` (${labDir})` : ""}.`;
    }
  }

  const showMachine = runsMultiMachine(runs);
  const bar = (
    <RunsBar counts={countsByKind(runs)} kind={kind} series={series} onKind={selectKind} onSeries={() => setSeries((s) => !s)} />
  );

  if (kind === "lab" && series) {
    const groups = groupLabRunsByTask(labRuns);
    return (
      <div data-state="data">
        <div className="stagehdr">
          runs · lab series · {labRuns.length} run{labRuns.length === 1 ? "" : "s"} · {groups.length} task{groups.length === 1 ? "" : "s"}
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

  const rows = runsFiltered(runs, kind);
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

/** viewer.html: `function renderRunsBar()`. */
function RunsBar({
  counts,
  kind,
  series,
  onKind,
  onSeries,
}: {
  counts: Record<string, number>;
  kind: RunsKind;
  series: boolean;
  onKind: (k: RunsKind) => void;
  onSeries: () => void;
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
 * `tabIndex` affordance for tracked/lab rows (unchanged), and
 * `onActivate` is now the REAL per-row action, not a placeholder notice. */
function RunRow({ run, showMachine, onActivate }: { run: Run; showMachine: boolean; onActivate: () => void }) {
  const interactive = run.kind === "lab" || run.tracked;
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
 * (finished/live), distinct from `runStatusBadge`'s five-status badge above. */
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
