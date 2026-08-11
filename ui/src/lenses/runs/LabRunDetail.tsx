import { useEffect, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys, LAB_POLL_STEADY_MS, LAB_POLL_BACKFILL_MS } from "../../lib/queryKeys";
import { computeLabPipeline, labPipelineLines, labCliHint, labFeedLines, labBadgeText } from "./labRun";
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
 */
export function LabRunDetail({ dir, onBack }: { dir: string; onBack: () => void }) {
  const detailQuery = useQuery({
    queryKey: queryKeys.labRunDetail(dir),
    queryFn: () => fetchJson<LabRunDetailResponse>(`/lab/run/detail?dir=${encodeURIComponent(dir)}`),
  });

  const [events, setEvents] = useState<LabRunEvent[]>([]);
  const [finished, setFinished] = useState(false);
  // `useRef` for the offset (not state) — a poll reads/advances it between
  // renders without itself triggering one; only `setEvents`/`setFinished`
  // (real content changes) do, matching legacy's `renderLabRun()` firing
  // only when `gotLines` is truthy (viewer.html:4064).
  const offsetRef = useRef(0);

  useEffect(() => {
    // Fresh entry into a (possibly different) run dir: legacy resets
    // `LAB_EVENTS=[]; LAB_EVENTS_OFFSET=0` on every `drillLabRun` call
    // (viewer.html:4103) — this effect's own dependency array (`[dir]`)
    // reruns it exactly then, so the reset lives here rather than at the
    // component's mount-only initial state.
    offsetRef.current = 0;
    setEvents([]);
    setFinished(false);

    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;

    async function poll() {
      if (cancelled) return;
      let body: LabRunEventsResponse | null = null;
      try {
        const res = await fetch(`/lab/run/events?dir=${encodeURIComponent(dir)}&offset=${offsetRef.current}`, {
          headers: { accept: "application/json" },
        });
        if (res.ok) body = await res.json();
      } catch {
        // transient — retry next tick, matching legacy's own silent catch.
      }
      if (cancelled) return;

      let gotLines = 0;
      if (body) {
        if (Array.isArray(body.lines) && body.lines.length) {
          setEvents((prev) => prev.concat(body!.lines));
          gotLines = body.lines.length;
        }
        if (typeof body.next_offset === "number") offsetRef.current = body.next_offset;
        if (body.finished) setFinished(true);
        if (body.finished && gotLines === 0) return; // fully drained a finished run — stop polling
      }
      timer = setTimeout(poll, gotLines ? LAB_POLL_BACKFILL_MS : LAB_POLL_STEADY_MS);
    }

    poll();

    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [dir]);

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
        <span className={`labbadge ${isFinished ? "finished" : "live"}`}>{labBadgeText(isFinished)}</span>
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
          event feed{isFinished ? " (playback)" : " — live, polling"} · {events.length} record{events.length === 1 ? "" : "s"}
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
