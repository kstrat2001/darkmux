import { Fragment, useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
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
 */
export function RunsBoard({ initialKind }: { initialKind: RunsKind }) {
  const [kind, setKind] = useState<RunsKind>(initialKind);
  const [series, setSeries] = useState(false);
  const [showAll, setShowAll] = useState(false);

  // A fresh deep-link into a DIFFERENT kind while this component is already
  // mounted (route.runsKind changes without the runs lens itself
  // unmounting) re-syncs local filter state — mirrors `window.goRuns`
  // resetting `state.runsAll=false` on every fresh entry into the lens.
  useEffect(() => {
    setKind(initialKind);
    setSeries(false);
    setShowAll(false);
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
    if (k !== "lab") setSeries(false);
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
        {notice && <div className="labnotice">{notice}</div>}
        <div className="lablist">
          {groups.length ? (
            groups.map((g) => <LabTaskCard key={g.key} group={g} />)
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
      {notice && <div className="labnotice">{notice}</div>}
      <div className="lablist">
        {shown.length ? (
          <>
            {shown.map((r) => (
              <RunRow key={r.id} run={r} showMachine={showMachine} />
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

/** viewer.html: `function runStatusBadge(r)` + `function renderRunRow(r,
 * showMachine)`. The `data-act="labrun"/"gomission"` click destinations
 * (lab-run detail, the mission-graph page) are BOTH out of scope for this
 * packet (see `tests/parity/README.md`'s KNOWN COVERAGE GAPS and lens
 * inventory) — the row keeps its `role="button"`/`tabIndex` affordance for
 * tracked rows (DOM-shape parity) but the click is a documented no-op until
 * those destinations exist in `/next`. */
function RunRow({ run, showMachine }: { run: Run; showMachine: boolean }) {
  const interactive = run.kind === "lab" || run.tracked;
  const ago = runsAgo(run);
  const subtitle = runSubtitle(run, showMachine);
  return (
    <div
      className={`labrunrow${interactive ? "" : " flat"}`}
      {...(interactive ? { role: "button" as const, tabIndex: 0 } : {})}
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
 * `LabRun`'s own richer fields — NOT `renderRunRow`/`Run` above). */
function LabRunRow({ run }: { run: LabRun }) {
  return (
    <div className="labrunrow" role="button" tabIndex={0}>
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
function LabTaskCard({ group }: { group: LabTaskGroup }) {
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
            <LabRunRow run={r} />
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
