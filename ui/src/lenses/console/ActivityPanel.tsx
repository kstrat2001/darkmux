import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import { resolveRunsSrc } from "../../lib/staticSource";
import { missionGraphReachable } from "../../lib/injectedMeta";
import { runActivity, runsMultiMachine } from "../runs/format";
import { RunRow } from "../runs/RunsBoard";
import type { RunsResponse } from "../../types/handwritten";
import type { Run } from "../../types/generated/Run";

/**
 * (#1904) The console's DEFAULT landing view — no panel selection needed.
 *
 * Origin: a `lab run long-agentic` dispatch was executing live, and the
 * console's then-default panel (`mission status`) answered "108 missions,
 * all FINALIZED" — honest about missions, silent about the one thing that
 * was actually running, because a dispatch never mints a mission and
 * `mission status` is mission-scoped by construction. Two false starts
 * preceded this shape (a section bolted above the existing default; a
 * fallback that embedded the `mission-status` CLI output when nothing was
 * running) before the operator settled the final one: the console's
 * DEFAULT is this activity view, full stop — not a panel someone selects,
 * not a strip layered on top of something else.
 *
 * `mission status` keeps its own meaning and stays selectable in
 * `PANELS` — this view is deliberately NOT that CLI command's UI. It reads
 * `/runs` directly, which is already documented as "a READ-SIDE UNION over
 * three existing sources" (`crates/darkmux-serve/src/lib.rs::runs_handler`'s
 * own doc) — mission, dispatch, lab, every kind, kind-tagged. Rows reuse
 * `RunsBoard.tsx`'s own `RunRow` (exported for this) verbatim, so the kind
 * chip, status badge, and subtitle read exactly like the runs lens.
 *
 * ORDERING: the operator's own final call — "the 10 most recent, across
 * the union, newest first... no time window to reason about, just a
 * count." Sorted purely by `runActivity` (the same field the runs lens
 * itself orders by — see that module's own doc on why `updated_ts` is
 * populated per-source so this is a total order), no separate "running"
 * tie-break bolted on top: a dispatch that just started is newest by
 * construction, so it reads at the top the same way any other fresh
 * activity would. `id` is a final deterministic tie-break for the rare
 * case two rows share a timestamp — otherwise Array.sort's stability alone
 * would leave the order to `/runs`' own response ordering, which isn't a
 * contract this view should depend on.
 *
 * THE CAP: ten rows by default — "a phone-sized glance" — with the
 * `mission-status`/`mission-status-all` pairing as the precedent for the
 * escape hatch: `capped=false` (reached via `ConsolePanel`'s own
 * "all activity" tab, or this view's own inline link) renders every run,
 * uncapped, the same way `--all` does for missions. The disclosure line
 * mirrors `src/mission_status.rs`'s own established phrasing shape
 * ("… N more (K of M shown)") rather than inventing new wording — this
 * project's hard-won rule (#1876, #1891) is that a cap must never be
 * reported as if it were the total, and matching an existing, already-
 * reviewed phrasing is cheaper than re-deriving the same discipline badly.
 *
 * THE EMPTY STATE: genuinely empty (`total === 0`, no run of any kind or
 * status has ever been recorded) gets an honest line and nothing else —
 * every other case, including "everything that ever ran is long finished",
 * renders real rows. A machine with history is never blank just because
 * nothing is running RIGHT NOW.
 */
const ACTIVITY_ROW_CAP = 10;

/** Row destinations mirror `RunsBoard.tsx`'s own `activateRun` in
 * miniature (see that function's doc for the full reasoning): an untracked
 * dispatch has no mission graph but a real trajectory behind it, so it
 * opens the session drill (`#session=<id>`, #1900); a tracked mission (or
 * a tracked dispatch — the crew-of-one shape, which mints its own mission
 * id) opens the mission-graph lens (`#mission=<id>`); an untracked mission
 * (a peer's mission this daemon only knows via the fleet stream, #1705)
 * stays non-interactive, matching `RunRow`'s own `interactive` gate. A lab
 * run has no session/mission id of its own — it opens the runs lens pinned
 * to `kind=lab` with its own `run=<dir>` deep link, the same canonical hash
 * `RunsBoard`'s `openLabRun` writes.
 *
 * Deliberately a SMALLER function than `activateRun` — no in-page state
 * swap (this component isn't the runs lens, so lab always navigates rather
 * than swapping to an in-page detail pane) and no `rowClickNotice` banner
 * for the rare daemon-less-demo case where a mission graph is unreachable
 * (that row just renders non-interactive instead, via `RunRow`'s own
 * gate). Kept separate from `activateRun` rather than shared, because the
 * two callers' post-decision behavior genuinely differs. */
function activityRunActivate(run: Run): void {
  if (run.kind === "lab") {
    location.hash = `lens=runs&kind=lab&run=${encodeURIComponent(run.id)}`;
    return;
  }
  if (run.kind === "dispatch" && !run.tracked) {
    location.hash = `session=${encodeURIComponent(run.id)}`;
    return;
  }
  if (!run.tracked) return; // a peer's mission with no local session: nothing to open here
  if (missionGraphReachable()) {
    location.hash = `mission=${encodeURIComponent(run.id)}`;
  }
}

export function ActivityPanel({
  capped,
  onShowAll,
}: {
  /** `false` for the "all activity" escape hatch — renders every run,
   * uncapped, same as `mission-status-all`'s own `--all` never re-hiding
   * anything it was asked to show in full. */
  capped: boolean;
  /** Only called when there IS a hidden tail to reveal (`capped` true and
   * more rows exist than the cap) — the "all activity" view itself has
   * nothing to disclose, so it may omit this. */
  onShowAll?: () => void;
}) {
  const query = useQuery({
    queryKey: queryKeys.runs(),
    queryFn: () => fetchJson<RunsResponse>(resolveRunsSrc()),
  });
  const all: Run[] = query.data?.ok && Array.isArray(query.data.data?.runs) ? query.data.data.runs : [];
  const sorted = [...all].sort((a, b) => runActivity(b) - runActivity(a) || a.id.localeCompare(b.id));
  const total = sorted.length;

  // `data-state` values here are deliberately NOT the generic "data"/
  // "pending"/"empty" vocabulary several other lenses share (RunsBoard,
  // LabRunDetail, SessionReplay, PlaybackLens) — a nav-chrome harness test
  // (`nav-chrome.spec.ts`) waits on `[data-state="data"]` to mean
  // SPECIFICALLY "the runs lens settled", and reusing that exact value here
  // made this component's own content a false-positive match during a
  // console→runs tab transition (caught live: the harness read `.on` off
  // the wrong tab because BOTH elements briefly satisfied the same
  // selector). Prefixed so this component's states can never collide with
  // any other lens's.
  if (total === 0) {
    return (
      <div className="consoleactivity" data-state="activity-empty">
        <div className="none">no activity recorded yet — nothing has run on this daemon.</div>
      </div>
    );
  }

  const shown = capped ? sorted.slice(0, ACTIVITY_ROW_CAP) : sorted;
  const hidden = total - shown.length;
  const showMachine = runsMultiMachine(shown);

  return (
    <div className="consoleactivity" data-state="activity-loaded">
      <div className="stagehdr">
        activity — {total} run{total === 1 ? "" : "s"}
      </div>
      <div className="lablist">
        {shown.map((r) => (
          <RunRow key={r.id} run={r} showMachine={showMachine} onActivate={() => activityRunActivate(r)} />
        ))}
      </div>
      {hidden > 0 && onShowAll && (
        <>
          <div className="none">
            … {hidden} more ({shown.length} of {total} shown)
          </div>
          <div
            className="hyblink"
            role="button"
            tabIndex={0}
            onClick={onShowAll}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                onShowAll();
              }
            }}
          >
            → show every run
          </div>
        </>
      )}
    </div>
  );
}
