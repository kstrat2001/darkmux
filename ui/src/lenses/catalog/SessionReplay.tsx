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

      <div className="track">
        <div className="lbl">{view.modelTrackLabel}</div>
        {view.modelTrackLines.map((line, i) => (
          <div key={i}>{line}</div>
        ))}
      </div>

      {/* (#1973) The HARNESS pane. Separated from the model metrics above
          because the operator could not tell which numbers described the model
          and which described darkmux around it — the question that produced
          this redesign. Rendered only when it has something to say, so a step
          with no harness metrics shows no empty frame. */}
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
        <div className="lbl">{view.detectionsLabel}</div>
        {view.detectionLines.map((line, i) => (
          <div key={i}>{line}</div>
        ))}
      </div>
    </div>
  );
}
