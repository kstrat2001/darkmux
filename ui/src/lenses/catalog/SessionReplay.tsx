import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import { flowToRenderModel } from "../../lib/flow";
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
  const query = useQuery({
    queryKey: queryKeys.flowSession(sessionId),
    queryFn: () => fetchJson<FlowRecordsResponse>(`/flow-session/${encodeURIComponent(sessionId)}`),
  });

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

  const data = flowToRenderModel(query.data.data.records);
  const view = runRegions(data, sessionId);

  return (
    <div data-state="data" className="session-run">
      <h2 className="session-run__header">
        <span className={`pill pill--${view.header.pillCls}`}>{view.header.pillLabel}</span> RUN · {view.header.role}{" "}
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

      <div className="metrics" data-scope="model">
        {view.metricScope.model.map((i) => view.metrics[i]).filter(Boolean).map((m, i) => (
          <div className="met" key={i}>
            <div className="mv">{m.value}</div>
            <div className="ml">{m.label}</div>
          </div>
        ))}
      </div>

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
        <div className="metrics" data-scope="harness">
          {view.metricScope.harness.map((i) => view.metrics[i]).filter(Boolean).map((m, i) => (
            <div className="met" key={i}>
              <div className="mv">{m.value}</div>
              <div className="ml">{m.label}</div>
            </div>
          ))}
        </div>
      )}

      <div className="track">
        <div className="lbl">{view.modelTrackLabel}</div>
        {view.modelTrackLines.map((line, i) => (
          <div key={i}>{line}</div>
        ))}
      </div>


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
                <span className="signal__glyph" aria-hidden="true">
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
