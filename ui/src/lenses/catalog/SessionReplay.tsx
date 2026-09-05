import { WorkStatus } from "../../components/WorkStatus";
import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson, type FetchResult } from "../../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS } from "../../lib/queryKeys";
import { useSessionLiveness } from "../../hooks/useSessionLiveness";
import { T, flowToRenderModel } from "../../lib/flow";
import { useNowMs } from "../../lib/clock";
import { clkhm } from "../../lib/format";
import { getSource } from "../../lib/source";
import { useDay } from "../../hooks/useDay";
import { injectedPlaybackDate } from "../../lib/injectedMeta";

/** (#1972) How long a run may go silent before it is treated as abandoned
 *  rather than live. Mirrors the host watchdog's own default
 *  (`DARKMUX_INACTIVITY_TIMEOUT_SECONDS` = 600s): past that point the
 *  container has been hard-killed, so a still-ticking counter would be
 *  asserting something the harness has already ruled out. */
export const STALE_AFTER_MS = 600_000;
import { livenessState } from "../../components/LivenessPulse";
import { runRegions } from "../session/sessionRun";
import type { FlowRecordsResponse } from "../../types/handwritten";

/**
 * `#session=<id>` — `viewer.html`'s `catalogQuery()` session branch, and
 * `drillSession()`'s destination (the "open →" link on a machine page's run
 * row). Fetches `/flow-session/<id>` the same way `boot()` does, runs it
 * through `flowToRenderModel` (`lib/flow.ts`) the same way `boot()`'s own
 * `cq` branch does, then renders `runRegions()`'s (`lenses/session/
 * sessionRun.ts`) derivation — the real port of legacy's
 * `renderSubsystem()`. See `sessionRun.ts`'s own module doc for exactly what
 * this covers (validated BYTE-FOR-BYTE against the one real recorded golden
 * this repo has for legacy's own render, `goldens/session-task-list.txt`)
 * and what it deliberately does not (the two SVG chart regions).
 *
 * DOM shape follows the same "one `<div>`/text-run per visible line"
 * convention `MachineLens`/`LabRunDetail` already establish — see
 * `MachineLens.tsx`'s `Lines` component doc for why this port represents
 * content as line arrays rather than leaning on legacy's CSS-flex-dependent
 * `innerText` line-break behavior.
 *
 * An EMPTY response is a genuine no-data state (this corpus's own
 * `flow-session-task-list.json` fixture is non-empty, so the parity spec
 * exercises the populated branch — the empty branch is honest but
 * unexercised by this corpus).
 */
/** The run detail's `PillCls` predates the shared chip; map it back to the
 * raw status word the chip's vocabulary reads. The visible text stays
 * `pillLabel` (pre-uppercased, golden-pinned). */
function pillStatusWord(cls: "run" | "err" | "done" | "canceled"): string {
  return cls === "run" ? "running" : cls === "err" ? "error" : cls === "done" ? "complete" : "canceled";
}

export function SessionReplay({ sessionId, playhead = null }: { sessionId: string; playhead?: number | null }) {
  // (#1972) POLLS while the session is live. Without this the page fetched
  // its records ONCE, which is the defect a live dogfood run exposed: the
  // wall clock advanced (it reads the browser clock), while turns, tokens,
  // signals and `lastBeatMs` all froze at page load — so the pulse went quiet
  // five seconds in and could never beat, on the one page whose entire
  // purpose is watching a run happen.
  //
  // This is the FOURTH live view found fetching once (#1966 was the third).
  // Liveness comes from presence rather than from these records: asking the
  // records whether to keep asking for records is circular, and presence is
  // already the fleet's source of truth for session membership. The gate also
  // stops a replay or a finished run polling forever.
  //
  // (#2011) `shouldPoll` — not bare presence — because a presence-gated
  // interval that simply switches off at the drop never fetches the terminal
  // record, and this page then reports `RUNNING` forever with a clock still
  // counting. `useSessionLiveness` owns that window; it also owns the SAME
  // query key this component reads, so the event log and the stage cannot
  // disagree about when the run ended.
  const { shouldPoll, endedByPresence } = useSessionLiveness(sessionId);

  // (#2065) A static build has no `/flow-session/<id>` to reach — the demo's
  // dispatch-row tap 404'd here. Read the committed file instead (the same
  // `queryKeys.staticFlowSrc` slot the playback lens and `useRouteRecords`
  // fill, so this is cache reuse) and slice this session out of it, shaped
  // like the daemon's response so nothing below has to know. RAW records,
  // not `normalizeRecords`: `/flow-session` hands back raw records too, and
  // `flowToRenderModel` synthesizes the per-session runtime telemetry row
  // itself — normalizing here would add a second copy the daemon path never
  // has. The file's schema-header line carries no `session_id`, so the
  // slice drops it on its own.
  const source = getSource();
  const flowSrc = source.flow;
  const query = useQuery({
    queryKey: queryKeys.flowSession(sessionId),
    queryFn: () => fetchJson<FlowRecordsResponse>(`/flow-session/${encodeURIComponent(sessionId)}`),
    enabled: flowSrc === null,
    refetchInterval: shouldPoll ? PRESENCE_POLL_MS : false,
  });
  // (#2086) The static day comes from the one resolver (the shell already
  // holds it for the transport; same cache slot, no second download).
  const day = useDay(null);
  const staticSlice: FlowRecordsResponse | null = useMemo(() => {
    // RAW, not `day.records`: `/flow-session` hands back raw records and
    // `flowToRenderModel` synthesizes the runtime row itself; the normalized
    // day already carries one, so slicing it would double the row.
    if (flowSrc === null || day.raw === null) return null;
    const recs = day.raw.filter((r) => r.session_id === sessionId);
    return { records: recs, count: recs.length, truncated: false, generated_at_ms: 0 };
  }, [flowSrc, day.raw, sessionId]);
  const session: FetchResult<FlowRecordsResponse> | undefined =
    flowSrc === null ? query.data : staticSlice === null ? undefined : { ok: true, data: staticSlice };

  // (#1972) HOISTED ABOVE EVERY EARLY RETURN, deliberately. React counts
  // hooks per render, so calling `useNowMs` after the loading/error/empty
  // guards below meant the first render called fewer hooks than the second —
  // "change in the order of Hooks", caught immediately by the existing suite
  // when this was first written the obvious way.
  //
  // `base` is the derivation against record time; `useNowMs` subscribes only
  // when that says the run is live, and `view` below re-derives against the
  // ticking clock. Two derivations per second while live, none when not —
  // `runRegions` is pure over a bounded record set, so the second pass costs
  // a fraction of a millisecond, and it is what makes the elapsed counter
  // advance during a STALL rather than freezing at the newest record's
  // timestamp.
  // (#2071) The shell's transport hands this lens the playhead it renders
  // at: the run's turns, tokens and status derive from the records up to
  // that instant, so scrubbing a run detail replays the run rather than
  // narrowing only the event log beside a finished stage. `null` (a live
  // daemon route, no transport) renders the whole slice as before.
  const all = session?.ok ? session.data.records : null;
  const records = all && playhead !== null ? all.filter((r) => !(T(r.ts) > playhead)) : all;
  const data = records ? flowToRenderModel(records) : [];
  const base = records && records.length ? runRegions(data, sessionId) : null;
  // Gated on PLAYBACK too, not just on the run's own liveness. A recorded
  // session that never emitted a terminal record still reads as `live`, and
  // in a static/playback build there is no wall clock it could sensibly
  // advance against — its elapsed time is a fact about when it was recorded.
  // Ticking there would also make the parity corpus non-deterministic, which
  // is this project's own clock rule: no fixture may mix a fixed timestamp
  // with a clock-relative assertion.
  // A run with no terminal record is not automatically LIVE. One that died in
  // January has no `dispatch.complete` either, and ticking its counter up to
  // now would read `17:51:54 so far` and climbing — abandonment rendered as
  // liveness. The host watchdog hard-kills a dispatch after
  // `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` (600s by default), so a run that has
  // emitted nothing for longer than that CANNOT still be running.
  //
  // `Date.now()` here is a plain per-render read feeding a boolean, not a
  // `useSyncExternalStore` snapshot — the value it produces is stable once
  // past the threshold, so it cannot drive the render loop this file's clock
  // is careful to avoid.
  //
  // (#2011) `endedByPresence` is the one signal that OVERRIDES all of this.
  // The quiet-threshold above is a heuristic standing in for knowledge we
  // sometimes actually have: presence watching this session disappear is
  // direct evidence the run stopped, so the counter should not spend a
  // further ten minutes climbing toward the watchdog timeout before it
  // admits that. It is deliberately NOT `!sessionIsLive` — a session presence
  // never listed at all (a replay, a January run, or a machine with Redis
  // switched off, where `/fleet/sessions/live` returns an empty set for
  // everything) is not evidence of anything, and gating on mere absence would
  // freeze the live clock on those machines. Only the observed transition
  // counts. Note the counter can step BACKWARDS at that moment, from the
  // ticked value to the last record's own elapsed time; that is the point —
  // the run's last sign of life is a fact, and the seconds since are not.
  const quietMs = base?.lastBeatMs != null ? Date.now() - base.lastBeatMs : Infinity;
  const plausiblyRunning = (base?.live ?? false) && quietMs < STALE_AFTER_MS && !endedByPresence;
  const ticking = plausiblyRunning && source.kind !== "static" && injectedPlaybackDate() == null;
  const nowMs = useNowMs(ticking);

  if (!session) {
    return (
      <div data-state="pending" role="status" aria-label={`Loading session ${sessionId}`}>
        <div className="stagehdr">session replay</div>
        <div className="none">loading…</div>
      </div>
    );
  }

  if (!session.ok) {
    return (
      <div data-state="error" role="alert">
        <div className="stagehdr">session replay</div>
        <div className="none">
          couldn't reach /flow-session/{sessionId}
          {session.status !== null ? ` (HTTP ${session.status})` : ""}: {session.message}
        </div>
      </div>
    );
  }

  const count = session.data.count;

  if (count === 0) {
    return (
      <div data-state="empty">
        <div className="stagehdr">session replay</div>
        <div className="none">no records found for session {sessionId}.</div>
      </div>
    );
  }


  // `base` is non-null here: the `count === 0` guard above already returned.
  // (#2071) The playhead can sit BEFORE this run's first record (rewind on
  // a day the run started partway into): the cut slice is empty, `base` is
  // null, and the header below would dereference it — measured as "the
  // dispatch lens stopped rendering" through the error boundary. Say what
  // is true instead: at this instant the run has not started.
  if (!base) {
    return (
      <div data-state="before-start" role="status" aria-label={`Session ${sessionId} not started yet`}>
        <div className="stagehdr">session replay</div>
        <div className="none">
          {sessionId} has not started yet at this point of the day{playhead !== null ? ` (${clkhm(playhead)})` : ""}. Scrub forward to see it.
        </div>
      </div>
    );
  }
  const view = ticking ? runRegions(data, sessionId, nowMs) : base;
  const liveness = livenessState({ done: !view.live, animate: ticking, lastBeatMs: view.lastBeatMs, nowMs });

  return (
    <div data-state="data" className="session-run">
      <h2 className="session-run__header">
        {/* (#1972) Whitespace here is load-bearing: the parity golden compares
            `#stage` innerText byte-for-byte, and the pulse contributes NO text
            of its own. So exactly one space separates the pill from `RUN ·` —
            the `{" "}` below — and there must be none before `RUN`. The first
            version added a second and CI caught `RUNNING  RUN ·`, which is
            invisible on screen and unmissable to the golden. */}
        <WorkStatus
          status={pillStatusWord(view.header.pillCls)}
          label={view.header.pillLabel}
          live={liveness.state}
          className="pill"
          title={liveness.label}
        />{" "}
        {/* (#1974) No noun. This view's subject is ONE ROLE EXECUTION — one
            role, one model, its turns, tokens and signals. `RUN` was the one
            word contract 8 says it definitely is not: `run` is the umbrella
            over mission/dispatch/lab, never a grain. `STEP` would be wrong
            too, since a step contains 0..N role executions (a `dispatch.map`
            step holds one per item). `DISPATCH` names the run KIND, not what
            is on screen.
            The role already names the thing, so the noun is dropped rather
            than replaced with a differently-wrong one. */}
        {view.header.role}{" "}
        <span className="session-run__meta">
          ({view.header.sid} on {view.header.machineName})
        </span>
      </h2>

      {view.briefLines.length > 0 && (
        <div className="track brief-grid">
          {/* One block element per entry, same order and same text as before —
              `goldens/session-task-list.txt` pins label and value as separate
              lines, so the DOM shape is deliberately unchanged. The class is
              the only addition, and it is what lets a LABEL stop looking like
              a VALUE. */}
          {view.briefLines.map((entry, i) => (
            <div key={i} className={`brief-${entry.kind}`}>
              {entry.href ? (
                // A real anchor, so it is keyboard-reachable and middle-clickable
                // like any other link. Same text either way — the golden reads
                // `innerText`, which an <a> does not change.
                <a className="brief-link" href={entry.href}>
                  {entry.text}
                </a>
              ) : (
                entry.text
              )}
            </div>
          ))}
        </div>
      )}

      {view.disclosures.map((d) => (
        // (#1973) The payload the brief summarizes, reachable. `<details>`
        // rather than a JS toggle: it is keyboard-operable and
        // find-in-page-searchable for free, and the text is in the DOM whether
        // or not it is open — which is what the golden asserts, since an
        // assertion on the summary line alone would pass against the very bug
        // this fixes.
        <details className="disclosure" key={d.id} data-act={`disclose-${d.id}`}>
          <summary className="disclosure__sum">
            {d.label} · {d.chars} chars{d.truncated ? " · truncated" : ""}
          </summary>
          <pre className="disclosure__body">{d.text}</pre>
        </details>
      ))}

      {/* (#1973) Both panes share one row. HARNESS often holds a SINGLE tile
          (wall clock), and giving it a full-width band of its own made one
          card look stranded under five. Side by side they read as two groups
          of one row rather than as a row and an afterthought.
          A wrapper, not a reordering: MODEL's tiles still precede HARNESS's
          in the DOM, so innerText order — and the parity goldens — are
          unchanged. */}
      <div className="metricbanks">
      {/* Absent, not empty, when the unit did no model work — see
          `hasModelWork`. */}
      {view.metricScope.model.length > 0 && (
      <div className="metrics" data-scope="model" role="group" aria-label="model metrics">
        {view.metricScope.model.map((i) => view.metrics[i]).filter(Boolean).map((m, i) => (
          <div className="met" key={i} title={m.hintTitle}>
            <div className="mv">{m.value}</div>
            <div className="ml" data-hint={m.hint}>{m.label}</div>
            {/* (operator, 2026-09-05) Rendered every tile, empty or not — a
                slot a sibling tile fills and this one doesn't must still
                claim the same line, or the grid goes ragged the moment
                content differs between tiles. See `.session-run .msub`. */}
            <div className="msub">{m.sub ?? ""}</div>
          </div>
        ))}
      </div>
      )}

      {/* (#1973) The SYSTEM pane, ADJACENT to the model pane rather than
          below the model track. Two reasons, and the second is why CI caught
          it: sandwiching `model (lms)` between two metric grids read as a
          mistake on screen, and the split is a GROUPING of one metric row —
          separating the halves with an unrelated block denies that. Keeping
          them adjacent also leaves `innerText` order identical to legacy, so
          `goldens/session-task-list.txt` still passes byte-for-byte; the pane
          labels are CSS-generated (`::before`) and never enter the text. A
          redesign that can keep its golden should. */}
      {view.metricScope.system.length > 0 && (
        <div className="metrics" data-scope="system" role="group" aria-label="system metrics">
          {view.metricScope.system.map((i) => view.metrics[i]).filter(Boolean).map((m, i) => (
            <div className="met" key={i} title={m.hintTitle}>
              <div className="mv">{m.value}</div>
              <div className="ml" data-hint={m.hint}>{m.label}</div>
              <div className="msub">{m.sub ?? ""}</div>
            </div>
          ))}
        </div>
      )}
      </div>

      {view.hasModelWork && (
        <div className="track">
          <div className="lbl">{view.modelTrackLabel}</div>
          {view.modelTrackLines.map((line, i) => (
            <div key={i}>{line}</div>
          ))}
        </div>
      )}


      {/* (#1973) SIGNALS — grouped by kind, severity-coded, run-relative
          times. Was a flat list of grey strings with a `⚠` in front of every
          entry, including the ones that report a successful RECOVERY. */}
      <div className="track signals">
        <div className="lbl">{view.signalsLabel}</div>
        {view.signalGroups.length === 0 ? (
          <>
            <div>✓ clean</div>
            <div>no behavioral flags (cycle, tool-failure, reasoning-loop, edit-drift)</div>
          </>
        ) : (
          view.signalGroups.map((g) => (
            <div className={`signal signal--${g.severity}`} key={g.kind} data-severity={g.severity}>
              <div className="signal__head">
                {/* NOT `aria-hidden`. Severity was carried by this glyph, a
                    class and a `data-` attribute — the latter two invisible to
                    assistive tech — so hiding the glyph left a screen-reader
                    user no way at all to tell a struggle from a recovery,
                    which is the entire distinction this redesign exists to
                    draw. */}
                <span className="signal__glyph" role="img" aria-label={g.severity === "warn" ? "warning" : "recovered"}>
                  {g.severity === "warn" ? "⚠" : "✓"}
                </span>
                <span className="signal__kind">{g.kind}</span>
                {/* Count only when it IS a count. `×1` is noise on every row. */}
                {g.count > 1 && <span className="signal__count">×{g.count}</span>}
              </div>
              {g.signals.map((sig, i) => (
                <div className="signal__row" key={i}>
                  {sig.offsetLabel && <span className="signal__at">{sig.offsetLabel}</span>}
                  <span className="signal__detail">{sig.detail}</span>
                  {sig.fix ? <span className="signal__fix">fix: {sig.fix}</span> : null}
                </div>
              ))}
            </div>
          ))
        )}
      </div>
    </div>
  );
}
