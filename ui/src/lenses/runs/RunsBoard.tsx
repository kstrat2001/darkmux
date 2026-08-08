import { Fragment, useEffect, useState, type KeyboardEvent } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import { canonicalHash, writeHash } from "../../lib/hashSync";
import { RUNS_KINDS, type RunsKind } from "../../lib/route";
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
 * every mount — matching `window.goRuns`'s `Promise.all([loadRuns(),
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
 * Row-click destinations (lab-run detail, the mission-graph page — both
 * genuinely out of scope this packet, see `RunRow`'s own doc) are NOT silent
 * no-ops: clicking a still-interactive row surfaces `NOT_PORTED_NOTICE`, a
 * visible one-line `.labnotice` naming the gap and pointing at the classic
 * viewer, per the operator-authored posture (a stuck feature gets a visible
 * placeholder, not just a code comment nobody but a future dev will read).
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
const NOT_PORTED_NOTICE = "run detail isn't in /next yet — open it in the classic viewer at /";

export function RunsBoard({ initialKind }: { initialKind: RunsKind }) {
  const [kind, setKind] = useState<RunsKind>(initialKind);
  const [series, setSeries] = useState(false);
  const [showAll, setShowAll] = useState(false);
  const [rowClickNotice, setRowClickNotice] = useState<string | null>(null);
  const onRowActivate = () => setRowClickNotice(NOT_PORTED_NOTICE);

  // A fresh deep-link into a DIFFERENT kind while this component is already
  // mounted (route.runsKind changes without the runs lens itself
  // unmounting) re-syncs local filter state — mirrors `window.goRuns`
  // resetting `state.runsAll=false` on every fresh entry into the lens.
  useEffect(() => {
    setKind(initialKind);
    setSeries(false);
    setShowAll(false);
    setRowClickNotice(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [initialKind]);

  const runsQuery = useQuery({
    queryKey: queryKeys.runs(),
    queryFn: () => fetchJson<RunsResponse>("/runs"),
  });
  const labRunsQuery = useQuery({
    queryKey: queryKeys.labRuns(),
    queryFn: () => fetchJson<LabRunsResponse>("/lab/runs"),
  });

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
            groups.map((g) => <LabTaskCard key={g.key} group={g} onRowActivate={onRowActivate} />)
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
              <RunRow key={r.id} run={r} showMachine={showMachine} onActivate={onRowActivate} />
            ))}
            {more > 0 && (
              <div className="runmore" role="button" tabIndex={0} onClick={() => setShowAll(true)}>
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
        >
          {k}
          <span className="runchipn"> {counts[k] ?? 0}</span>
        </span>
      ))}
      {kind === "lab" && (
        <span className={`runchip${series ? " on" : ""}`} data-arg="series" role="button" tabIndex={0} onClick={onSeries}>
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
 * (lab-run detail, the mission-graph page) are BOTH out of scope for this
 * packet (see `tests/parity/README.md`'s KNOWN COVERAGE GAPS and lens
 * inventory) — the row keeps its `role="button"`/`tabIndex` affordance for
 * tracked rows (DOM-shape parity), and activating it (click or Enter/Space)
 * surfaces `NOT_PORTED_NOTICE` via `onActivate` rather than silently doing
 * nothing (operator-authored posture: a stuck feature gets a VISIBLE
 * placeholder, not a code comment). */
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
 * pane) — same not-ported-yet notice on activation as `RunRow`. */
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

/** viewer.html: `function renderLabTaskCard(group)`. */
function LabTaskCard({ group, onRowActivate }: { group: LabTaskGroup; onRowActivate: () => void }) {
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
            <LabRunRow run={r} onActivate={onRowActivate} />
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
