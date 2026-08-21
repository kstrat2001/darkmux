import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import { resolveRunsSrc } from "../../lib/staticSource";
import { missionGraphReachable } from "../../lib/injectedMeta";
import { runActivity, runsMultiMachine, runDestination, MISSION_GRAPH_UNREACHABLE_NOTICE } from "../runs/format";
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

/** Row destinations use the SAME `runDestination` decision `RunsBoard.tsx`'s
 * own `activateRun` does (`format.ts` — see that function's doc for the
 * full four-branch reasoning); only what happens with each outcome differs
 * here: `lab` navigates (this component isn't the runs lens, so it has no
 * in-page detail pane to swap to — `lens=runs&kind=lab&run=<dir>`, the
 * same canonical hash `RunsBoard`'s `openLabRun` writes), `hash` navigates
 * directly, and `unreachable` shows `MISSION_GRAPH_UNREACHABLE_NOTICE`
 * (the SAME text `RunsBoard` shows for the identical case) via
 * `onUnreachable` rather than silently doing nothing.
 *
 * (#1904 QA fix) This function used to hand-roll its own copy of the
 * decision tree and dropped the `unreachable` case entirely — a tracked
 * mission/dispatch row on the daemon-less static demo rendered as
 * interactive (`RunRow`'s own `interactive` gate has nothing to do with
 * graph reachability) but did nothing on click: a dead affordance, the
 * exact #1900 failure class this module's own header doc invokes. Sharing
 * `runDestination` with `RunsBoard` is what makes that branch impossible
 * to drop a second time. */
function activityRunActivate(run: Run, onUnreachable: () => void): void {
  const dest = runDestination(run, missionGraphReachable());
  switch (dest.kind) {
    case "lab":
      location.hash = `lens=runs&kind=lab&run=${encodeURIComponent(dest.dir)}`;
      return;
    case "hash":
      location.hash = dest.hash;
      return;
    case "unreachable":
      onUnreachable();
      return;
    case "none":
      return; // a peer's mission with no local session: nothing to open here
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
  // (#1904 QA fix) Same UX `RunsBoard.tsx`'s own `rowClickNotice` gives the
  // identical `runDestination` "unreachable" outcome — a status line, not
  // a silent no-op, on the daemon-less static demo.
  const [rowClickNotice, setRowClickNotice] = useState<string | null>(null);

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
  //
  // (#1904 QA fix) These two branches used to be missing — `query.data`
  // being `undefined` (still loading) or `ok: false` (the fetch failed)
  // both fell through to `all = []`, `total === 0`, and the EMPTY-daemon
  // branch below, which asserted "no activity recorded yet — nothing has
  // run on this daemon" as though that were a verified historical fact.
  // It flashed on every entry while loading and stuck, permanently wrong,
  // on any daemon error — since this view IS the console's landing page
  // now, that is no longer a rare corner. Loading and error get their own
  // honest states instead, matching the shape `RunsBoard.tsx:390-401`
  // already uses for the same `/runs` endpoint (pending → "loading…";
  // `!query.data` there degrades silently only on a settled EMPTY result,
  // never on a genuine in-flight or error state).
  if (!query.data) {
    return (
      <div className="consoleactivity" data-state="activity-pending" role="status" aria-label="Loading activity">
        <div className="none">loading…</div>
      </div>
    );
  }
  if (!query.data.ok) {
    return (
      <div className="consoleactivity" data-state="activity-error">
        <div className="panelerr">{query.data.message}</div>
      </div>
    );
  }

  const all: Run[] = Array.isArray(query.data.data?.runs) ? query.data.data.runs : [];
  const sorted = [...all].sort((a, b) => runActivity(b) - runActivity(a) || a.id.localeCompare(b.id));
  const total = sorted.length;

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
      {rowClickNotice && (
        <div className="labnotice" role="status">
          {rowClickNotice}
        </div>
      )}
      <div className="lablist">
        {shown.map((r) => (
          <RunRow
            key={r.id}
            run={r}
            showMachine={showMachine}
            onActivate={() => activityRunActivate(r, () => setRowClickNotice(MISSION_GRAPH_UNREACHABLE_NOTICE))}
          />
        ))}
      </div>
      {/* (#1904 QA fix) The disclosure text and the link are now two
          INDEPENDENT conditions — `hidden > 0` alone decides whether the
          count renders at all; `onShowAll` only decides whether there's a
          link to click through with. Was `hidden > 0 && onShowAll && (...)`,
          which silently dropped the WHOLE disclosure (count included)
          whenever a caller passed no `onShowAll` — reporting the cap as if
          it were the total, the exact #1876/#1891 violation this module's
          own doc names as the rule. */}
      {hidden > 0 && (
        <div className="none">
          … {hidden} more ({shown.length} of {total} shown)
        </div>
      )}
      {hidden > 0 && onShowAll && (
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
      )}
    </div>
  );
}
