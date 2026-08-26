import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS } from "../../lib/queryKeys";
import { useLiveSessionIds } from "../../hooks/useLiveSessionIds";
import { staticFlowSrc } from "../../lib/staticSource";
import { flowToRenderModel } from "../../lib/flow";
import { useNowMs } from "../../lib/clock";
import { isStaticBuild } from "../../lib/staticSource";
import { injectedPlaybackDate } from "../../lib/injectedMeta";

/** (#1972) How long a run may go silent before it is treated as abandoned
 *  rather than live. Mirrors the host watchdog's own default
 *  (`DARKMUX_INACTIVITY_TIMEOUT_SECONDS` = 600s): past that point the
 *  container has been hard-killed, so a still-ticking counter would be
 *  asserting something the harness has already ruled out. */
export const STALE_AFTER_MS = 600_000;
import { LivenessPulse } from "../../components/LivenessPulse";
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
export function SessionReplay({ sessionId }: { sessionId: string }) {
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
  const liveSessions = useLiveSessionIds(staticFlowSrc() === null);
  const sessionIsLive = liveSessions.has(sessionId);

  const query = useQuery({
    queryKey: queryKeys.flowSession(sessionId),
    queryFn: () => fetchJson<FlowRecordsResponse>(`/flow-session/${encodeURIComponent(sessionId)}`),
    refetchInterval: sessionIsLive ? PRESENCE_POLL_MS : false,
  });

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
  const records = query.data?.ok ? query.data.data.records : null;
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
  // now would read `1071:54 so far` and climbing — abandonment rendered as
  // liveness. The host watchdog hard-kills a dispatch after
  // `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` (600s by default), so a run that has
  // emitted nothing for longer than that CANNOT still be running.
  //
  // `Date.now()` here is a plain per-render read feeding a boolean, not a
  // `useSyncExternalStore` snapshot — the value it produces is stable once
  // past the threshold, so it cannot drive the render loop this file's clock
  // is careful to avoid.
  const quietMs = base?.lastBeatMs != null ? Date.now() - base.lastBeatMs : Infinity;
  const plausiblyRunning = (base?.live ?? false) && quietMs < STALE_AFTER_MS;
  const ticking = plausiblyRunning && !isStaticBuild() && injectedPlaybackDate() == null;
  const nowMs = useNowMs(ticking);

  if (!query.data) {
    return (
      <div data-state="pending" role="status" aria-label={`Loading session ${sessionId}`}>
        <div className="stagehdr">session replay</div>
        <div className="none">loading…</div>
      </div>
    );
  }

  if (!query.data.ok) {
    return (
      <div data-state="error" role="alert">
        <div className="stagehdr">session replay</div>
        <div className="none">
          couldn't reach /flow-session/{sessionId}
          {query.data.status !== null ? ` (HTTP ${query.data.status})` : ""}: {query.data.message}
        </div>
      </div>
    );
  }

  const count = query.data.data.count;

  if (count === 0) {
    return (
      <div data-state="empty">
        <div className="stagehdr">session replay</div>
        <div className="none">no records found for session {sessionId}.</div>
      </div>
    );
  }


  // `base` is non-null here: the `count === 0` guard above already returned.
  const view = ticking ? runRegions(data, sessionId, nowMs) : base!;

  return (
    <div data-state="data" className="session-run">
      <h2 className="session-run__header">
        {/* (#1972) Whitespace here is load-bearing: the parity golden compares
            `#stage` innerText byte-for-byte, and the pulse contributes NO text
            of its own. So exactly one space separates the pill from `RUN ·` —
            the `{" "}` below — and there must be none before `RUN`. The first
            version added a second and CI caught `RUNNING  RUN ·`, which is
            invisible on screen and unmissable to the golden. */}
        <span className={`pill pill--${view.header.pillCls}`}>{view.header.pillLabel}</span>{" "}
        <LivenessPulse done={!view.live} animate={ticking} lastBeatMs={view.lastBeatMs} />
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
        <div className="track">
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

      {/* Absent, not empty, when the unit did no model work — see
          `hasModelWork`. */}
      {view.metricScope.model.length > 0 && (
      <div className="metrics" data-scope="model" role="group" aria-label="model metrics">
        {view.metricScope.model.map((i) => view.metrics[i]).filter(Boolean).map((m, i) => (
          <div className="met" key={i}>
            <div className="mv">{m.value}</div>
            <div className="ml">{m.label}</div>
          </div>
        ))}
      </div>
      )}

      {/* (#1973) The HARNESS pane, ADJACENT to the model pane rather than
          below the model track. Two reasons, and the second is why CI caught
          it: sandwiching `model (lms)` between two metric grids read as a
          mistake on screen, and the split is a GROUPING of one metric row —
          separating the halves with an unrelated block denies that. Keeping
          them adjacent also leaves `innerText` order identical to legacy, so
          `goldens/session-task-list.txt` still passes byte-for-byte; the pane
          labels are CSS-generated (`::before`) and never enter the text. A
          redesign that can keep its golden should. */}
      {view.metricScope.harness.length > 0 && (
        <div className="metrics" data-scope="harness" role="group" aria-label="harness metrics">
          {view.metricScope.harness.map((i) => view.metrics[i]).filter(Boolean).map((m, i) => (
            <div className="met" key={i}>
              <div className="mv">{m.value}</div>
              <div className="ml">{m.label}</div>
            </div>
          ))}
        </div>
      )}

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
