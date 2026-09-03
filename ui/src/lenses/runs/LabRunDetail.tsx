import { WorkStatus } from "../../components/WorkStatus";
import { useEffect, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys, LAB_POLL_STEADY_MS, LAB_POLL_BACKFILL_MS, LAB_POLL_FAILURE_THRESHOLD } from "../../lib/queryKeys";
import {
  computeLabPipeline,
  labPipelineLines,
  labCliHint,
  labFeedLines,
  labFeedCountText,
  labFeedStatusSuffix,
  labBadgeText,
} from "./labRun";
import type { LabRunDetailResponse, LabRunEvent, LabRunEventsResponse } from "../../types/handwritten";

/**
 * Lab-run detail — `renderLabRun()` (viewer.html:4848-4869), reached by
 * clicking a `kind==="lab"` row in the runs board (`data-act="labrun"`,
 * `ACTIONS.labrun` → `drillLabRun(dir)`). Previously `RunsBoard`'s row
 * click surfaced `NOT_PORTED_NOTICE` for every interactive row — this
 * component is the real destination for the lab half of that gap (the
 * mission/dispatch half routes to `/mission/<id>/graph`, see
 * `RunsBoard.tsx`'s own doc for that half).
 *
 * Two fetches, matching `drillLabRun`'s own two loads:
 * - `GET /lab/run/detail?dir=` (react-query, one-shot — legacy's
 *   `loadLabRunDetail`) for the envelope(s)/scores that drive the pipeline
 *   synthesis stage + the CLI hint.
 * - `GET /lab/run/events?dir=&offset=` (a self-rescheduling poll, NOT
 *   react-query's `refetchInterval` — the offset/accumulation state is
 *   genuinely sequential, matching legacy's `pollLabEvents`: each tick
 *   fetches from the LAST `next_offset`, appends new lines, and either
 *   backs off to the steady cadence or speeds up to drain a backlog. A
 *   plain `refetchInterval` re-fetch has no way to carry that offset
 *   forward between polls, so this stays a manual `useEffect` + timer,
 *   mirroring the legacy control flow rather than fighting react-query's
 *   shape to force it in).
 *
 *   The poll goes through `fetchJson` (the same discriminated-result
 *   contract every other data path in this app uses), not a raw `fetch` —
 *   a merge-gate finding on an earlier cut of this component caught a raw
 *   `fetch` outside that contract whose thrown-error/non-2xx cases were
 *   both swallowed by a silent `catch`, rescheduling forever with no state
 *   change: a dead daemon rendered byte-identical to an idle live run.
 *   Transient failures still just retry at the steady cadence (a single
 *   dropped request shouldn't flap the UI), but `LAB_POLL_FAILURE_THRESHOLD`
 *   CONSECUTIVE failures flips `pollUnreachable`, which the badge/header
 *   surface as a visible "daemon unreachable — retrying" state (cleared on
 *   the next success) — see `queryKeys.ts`'s doc on the constant for why
 *   N=3. The poll also stops entirely once the detail fetch is known to
 *   have errored (a garbage `run=` deep link) rather than hitting
 *   `/lab/run/events` every tick behind an error page nothing will ever
 *   resolve.
 *
 * (drill-in packet) `onUnresolvable` — the port of legacy's `drillLabRun`
 * fallback (`viewer.html:4101-4126`): `if(!LAB_DETAIL){ ...state.level="runs";
 * state.labRunDir=null; render(); ... }`. A stale/mistyped `run=` deep link,
 * a dir deleted since the list was fetched, or (on a daemon-less static
 * build) the genuine absence of `/lab/run/detail` all land here the same
 * way legacy's does — fired once, from an effect keyed on the detail
 * query's own settled `!ok` state, mirroring legacy's `await
 * loadLabRunDetail(dir); if(!LAB_DETAIL)`. This component does NOT choose
 * the fallback MESSAGE (the daemon-vs-static split legacy's own
 * `missionGraphReachable()` check makes) — that judgment stays in
 * `RunsBoard.tsx`, right beside `MISSION_GRAPH_UNREACHABLE_NOTICE`, the
 * other daemon-reachability notice this board already owns; this component
 * only reports THAT its dir was unresolvable, not why. Before this, the
 * component had no such callback at all: a bad `run=` rendered its own
 * `data-state="error"` page (below) and stayed there permanently — see
 * `viewer-lab.spec.js`'s own fixme note for the confirmed-live finding.
 */
export function LabRunDetail({
  dir,
  onBack,
  onUnresolvable,
}: {
  dir: string;
  onBack: () => void;
  /** Called once the detail fetch is known to have failed for THIS `dir` —
   * see the module doc above. `RunsBoard` responds by clearing `labRunDir`
   * (falling back to the run list) and showing its own notice text. */
  onUnresolvable: () => void;
}) {
  const detailQuery = useQuery({
    queryKey: queryKeys.labRunDetail(dir),
    queryFn: () => fetchJson<LabRunDetailResponse>(`/lab/run/detail?dir=${encodeURIComponent(dir)}`),
  });

  const [events, setEvents] = useState<LabRunEvent[]>([]);
  const [finished, setFinished] = useState(false);
  // Consecutive-failure signal for the events poll below — see this
  // component's own doc + `LAB_POLL_FAILURE_THRESHOLD`'s doc in
  // `queryKeys.ts`. Real state (not a ref): it drives the visible
  // badge/header text.
  const [pollUnreachable, setPollUnreachable] = useState(false);
  // `useRef` for the offset (not state) — a poll reads/advances it between
  // renders without itself triggering one; only `setEvents`/`setFinished`/
  // `setPollUnreachable` (real content/status changes) do, matching
  // legacy's `renderLabRun()` firing only when `gotLines` is truthy
  // (viewer.html:4064).
  const offsetRef = useRef(0);
  const failCountRef = useRef(0);

  // The detail fetch's own error state, gating the events poll below (the
  // secondary MUST FIX: a garbage `run=` deep link must not keep hitting
  // `/lab/run/events` every tick behind a page that will never resolve).
  const detailErrored = detailQuery.data ? !detailQuery.data.ok : false;

  // (drill-in packet) Reports the unresolvable dir to `RunsBoard` exactly
  // once — a ref, not state, since it exists purely to de-dupe a callback
  // firing (`onUnresolvable` change identity, a refetch retriggering the
  // same failure) rather than to drive a render. Guarded per-`dir` rather
  // than a bare boolean, matching legacy's own `state.labRunDir===dir`
  // check in `drillLabRun` — the same defense against a race where the
  // operator has already moved on to a DIFFERENT run by the time an older
  // fetch settles.
  const reportedUnresolvableRef = useRef<string | null>(null);
  useEffect(() => {
    if (detailErrored && reportedUnresolvableRef.current !== dir) {
      reportedUnresolvableRef.current = dir;
      onUnresolvable();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [detailErrored, dir]);

  useEffect(() => {
    // Fresh entry into a (possibly different) run dir: legacy resets
    // `LAB_EVENTS=[]; LAB_EVENTS_OFFSET=0` on every `drillLabRun` call
    // (viewer.html:4103) — this effect's own dependency array (`[dir]`)
    // reruns it exactly then, so the reset lives here rather than at the
    // component's mount-only initial state.
    offsetRef.current = 0;
    setEvents([]);
    setFinished(false);
    failCountRef.current = 0;
    setPollUnreachable(false);

    // The detail fetch already errored — nothing under this dir is ever
    // going to resolve, so don't start (or continue) polling behind it.
    if (detailErrored) return;

    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;

    async function poll() {
      if (cancelled) return;
      const result = await fetchJson<LabRunEventsResponse>(
        `/lab/run/events?dir=${encodeURIComponent(dir)}&offset=${offsetRef.current}`,
        { headers: { accept: "application/json" } },
      );
      if (cancelled) return;

      let gotLines = 0;
      if (result.ok) {
        failCountRef.current = 0;
        setPollUnreachable(false);
        const body = result.data;
        if (Array.isArray(body.lines) && body.lines.length) {
          setEvents((prev) => prev.concat(body.lines));
          gotLines = body.lines.length;
        }
        if (typeof body.next_offset === "number") offsetRef.current = body.next_offset;
        if (body.finished) setFinished(true);
        if (body.finished && gotLines === 0) return; // fully drained a finished run — stop polling
      } else {
        // Transient retry either way — but after `LAB_POLL_FAILURE_THRESHOLD`
        // CONSECUTIVE misses, surface it: a dead daemon must not stay
        // byte-identical to an idle live run (the merge-gate finding this
        // fixes).
        failCountRef.current += 1;
        if (failCountRef.current >= LAB_POLL_FAILURE_THRESHOLD) setPollUnreachable(true);
      }
      timer = setTimeout(poll, gotLines ? LAB_POLL_BACKFILL_MS : LAB_POLL_STEADY_MS);
    }

    poll();

    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [dir, detailErrored]);

  if (!detailQuery.data) {
    return (
      <div data-state="pending" role="status" aria-label={`Loading run ${dir}`}>
        <div className="stagehdr">
          <button type="button" className="labrun-back" onClick={onBack}>
            ‹ runs
          </button>
          {` · ${dir}`}
        </div>
        <div className="none">loading…</div>
      </div>
    );
  }

  if (!detailQuery.data.ok) {
    return (
      <div data-state="error" role="alert">
        <div className="stagehdr">
          <button type="button" className="labrun-back" onClick={onBack}>
            ‹ runs
          </button>
          {` · ${dir}`}
        </div>
        <div className="none">
          couldn't reach /lab/run/detail{detailQuery.data.status !== null ? ` (HTTP ${detailQuery.data.status})` : ""}: {detailQuery.data.message}
        </div>
      </div>
    );
  }

  const detail = detailQuery.data.data;
  const env = detail.funnels[0] ?? null;
  const scores = detail.scores;
  const pipe = computeLabPipeline(events);
  // (#1434) No task-finished record exists anymore; a run is "finished"
  // when its completed envelope or scores artifact is present — OR the
  // events poll itself observed `finished` (the `scores.json`-exists check
  // the server makes on every poll, viewer.html:2218/`renderLabRun`'s own
  // `!!env || (scores!=null)` check, folded with the poll's own signal so a
  // run that just completed doesn't wait for a full remount to say so).
  const isFinished = !!env || scores != null || finished;
  const pipelineLines = labPipelineLines(pipe, env);
  const cliHint = labCliHint(env, scores);
  const feedLines = labFeedLines(events);

  return (
    <div data-state="data">
      <div className="stagehdr">
        <button type="button" className="labrun-back" onClick={onBack}>
          ‹ runs
        </button>
        {` · ${dir} `}
        <WorkStatus status={isFinished ? "finished" : pollUnreachable ? "error" : "live"} label={labBadgeText(isFinished, pollUnreachable)} className="labbadge" />
      </div>

      <div className="labpipe">
        {chunk2(pipelineLines).map(([name, meta], i) => (
          <div className="labstage" key={i}>
            <div className="labstagename">{name}</div>
            <div className="labstagemeta">{meta}</div>
          </div>
        ))}
      </div>

      <div className="labcli">
        <span className="labclilbl">try it yourself</span>
        <code>{cliHint}</code>
      </div>

      <div className="labfeedwrap">
        <div className="labfeedhdr">
          event feed{labFeedStatusSuffix(isFinished, pollUnreachable)} · {labFeedCountText(events.length)}
        </div>
        <div className="labfeed">
          {feedLines.length ? (
            chunk3(feedLines).map(([ts, tag, text], i) => (
              <div className="labfeedrow" key={i}>
                <span className="labfeedts">{ts}</span>
                {tag && <span className="labfeedtag">{tag}</span>}
                <span className="labfeedtext">{text}</span>
              </div>
            ))
          ) : (
            <div className="none">no events yet.</div>
          )}
        </div>
      </div>
    </div>
  );
}

function chunk2<T>(arr: T[]): [T, T][] {
  const out: [T, T][] = [];
  for (let i = 0; i < arr.length; i += 2) out.push([arr[i], arr[i + 1]]);
  return out;
}

function chunk3<T>(arr: T[]): [T, T, T][] {
  const out: [T, T, T][] = [];
  for (let i = 0; i < arr.length; i += 3) out.push([arr[i], arr[i + 1], arr[i + 2]]);
  return out;
}
