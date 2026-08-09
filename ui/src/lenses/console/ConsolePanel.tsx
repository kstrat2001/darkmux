import { useEffect, useState, type KeyboardEvent } from "react";
import { useQuery } from "@tanstack/react-query";
import { queryKeys, PANEL_CACHE_MS } from "../../lib/queryKeys";
import type { PanelId } from "../../lib/route";
import { PANELS, DEFAULT_PANEL_ID, isManualPanel, panelCols } from "./panels";
import { fetchPanel } from "./fetchPanel";
import { panelAgeLabel } from "./format";
import { AnsiText } from "./ansi";
import type { PanelResponse } from "../../types/handwritten";

/**
 * The console lens — `#lens=console&panel=<id>`. Pure port of
 * `viewer.html`'s `renderConsole()`/`loadPanel()`/`setPanel()`/`goConsole()`
 * (search that file for `── CLI panel:`). Renders an allowlisted CLI
 * command's own ANSI output, so the panel cannot drift from the CLI the way
 * a JS re-implementation could (`crates/darkmux-serve/src/panel.rs`'s own
 * module doc: "twin-drift becomes structurally impossible... a panel cannot
 * diverge from the CLI because it IS the CLI").
 *
 * DOM/CSS shape is a direct port (`.runsbar`/`.runchip` tabs, `.panelwrap` >
 * `.panelchrome` + a `.panelout`/`.panelerr` body) — see `styles.css`'s own
 * "Console lens (Packet 6)" block for why the flex layout is load-bearing
 * for `innerText` byte-parity, not just visual.
 *
 * Fetch/cache mapping onto TanStack Query (the ONE genuinely non-literal
 * translation this component makes, since legacy hand-rolls its own
 * `PANEL_STATE` cache + `loadPanel`):
 * - One `useQuery` keyed by `queryKeys.panel(id)` — switching tabs reuses
 *   Query's own cache exactly the way `PANEL_STATE[id]` reuse worked
 *   (revisiting an already-loaded panel shows the cached body immediately,
 *   no re-fetch), with no hand-rolled state object needed.
 * - `doctor` (`isManualPanel`) is `enabled: false` PERMANENTLY — selecting
 *   the tab NEVER fetches (matches `MANUAL_PANELS`'s guard on every
 *   auto-fetch call site: `goConsole`/`setPanel`/the visibilitychange
 *   handler). Only the explicit "run"/"re-run" button's `refetch()` call
 *   (which works regardless of `enabled`) ever populates it — same
 *   operator-must-ask contract as legacy (#1286).
 * - `staleTime: PANEL_CACHE_MS` (`queryKeys.ts`'s already-documented mirror
 *   of the daemon's own `PANEL_CACHE_TTL`) + `refetchOnWindowFocus` stands
 *   in for legacy's own `visibilitychange` listener (re-check only when the
 *   tab regains focus AND the cached copy has passed its TTL, never a
 *   timer) — a deliberate, DOCUMENTED adaptation to Query's idioms rather
 *   than a byte-for-byte port of the DOM listener, since no golden exercises
 *   that specific code path (see the packet report's ledger).
 * - `query.isFetching` maps to `st.loading` — true during ANY fetch
 *   (including a re-run), which legacy's own body-branch checks FIRST,
 *   before looking at whether stale data from a prior load still exists —
 *   matched here the same way (loading always wins the BODY branch; the
 *   CHROME branch below checks the data independently, exactly like
 *   legacy's `st.body`-gated chrome staying visible-but-stale mid-reload).
 */
export function ConsolePanel({ initialPanelId }: { initialPanelId: PanelId | "" }) {
  const [panelId, setPanelId] = useState<PanelId>(initialPanelId || DEFAULT_PANEL_ID);

  // A fresh deep-link into a DIFFERENT panel while this component is already
  // mounted re-syncs local selection — mirrors `boot()`'s `if(nq)
  // state.panelId=nq` applying on every fresh console entry.
  useEffect(() => {
    if (initialPanelId) setPanelId(initialPanelId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [initialPanelId]);

  const manual = isManualPanel(panelId);
  const query = useQuery({
    queryKey: queryKeys.panel(panelId),
    queryFn: () => fetchPanel(panelId, panelCols(document.querySelector(".panelout"))),
    enabled: !manual,
    staleTime: PANEL_CACHE_MS,
    refetchOnWindowFocus: !manual,
  });

  const loadedBody = query.data?.ok === true ? query.data.data : null;
  const errorMessage = query.data?.ok === false ? query.data.message : null;

  return (
    <>
      <div className="runsbar">
        {PANELS.map((p) => (
          <PanelTab key={p.id} id={p.id} label={p.label} active={p.id === panelId} onSelect={setPanelId} />
        ))}
      </div>
      <div className="panelwrap">
        <div className="panelchrome">
          {loadedBody ? (
            <LoadedChrome body={loadedBody} fetchedAt={query.dataUpdatedAt} onRerun={() => query.refetch()} />
          ) : (
            <NotLoadedChrome id={panelId} loading={query.isFetching} onRun={() => query.refetch()} />
          )}
        </div>
        <PanelBody
          loading={query.isFetching}
          errorMessage={errorMessage}
          ansiText={loadedBody ? loadedBody.ansi_text || "" : null}
          onPanelSwitch={setPanelId}
        />
      </div>
    </>
  );
}

/** viewer.html: one `.runchip[data-act="setpanel"]` entry of the tab bar. */
function PanelTab({ id, label, active, onSelect }: { id: PanelId; label: string; active: boolean; onSelect: (id: PanelId) => void }) {
  return (
    <span
      className={`runchip${active ? " on" : ""}`}
      data-act="setpanel"
      data-arg={id}
      role="button"
      tabIndex={0}
      onClick={() => onSelect(id)}
      onKeyDown={(e: KeyboardEvent<HTMLSpanElement>) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onSelect(id);
        }
      }}
    >
      {label}
    </span>
  );
}

/** viewer.html: `renderConsole()`'s `if(st.body)` chrome branch. */
function LoadedChrome({
  body,
  fetchedAt,
  onRerun,
}: {
  body: PanelResponse;
  fetchedAt: number;
  onRerun: () => void;
}) {
  const a = panelAgeLabel(body, fetchedAt);
  return (
    <>
      <span className="pc-cmd">$ darkmux {(body.argv || []).join(" ")}</span>
      <span>
        captured {a.hhmmss}
        {a.served}
      </span>
      {a.stale && <span className="pc-stale">· stale ({a.ageSec}s)</span>}
      {body.auto_refresh === false && <span className="pc-manual">· manual-run only</span>}
      <span>· {body.gather_ms}ms</span>
      <span className="pc-spacer"></span>
      <button className="pcbtn" data-act="refreshpanel" onClick={onRerun}>
        re-run
      </button>
    </>
  );
}

/** viewer.html: `renderConsole()`'s `else` chrome branch (no body yet —
 * manual-never-run, or an auto panel whose fetch failed). */
function NotLoadedChrome({ id, loading, onRun }: { id: PanelId; loading: boolean; onRun: () => void }) {
  return (
    <>
      <span className="pc-cmd">$ darkmux {id.replace(/-/g, " ")}</span>
      <span className="pc-spacer"></span>
      <button className="pcbtn" data-act="refreshpanel" disabled={loading} onClick={onRun}>
        run
      </button>
    </>
  );
}

/** viewer.html: `renderConsole()`'s body switch (`.panelout`/`.panelerr`,
 * loading > error > loaded > not-yet-run precedence). The loaded branch is
 * the ONLY one using a real `<pre>` element — see `styles.css`'s module doc
 * for why the tag choice (not just the class) matters. */
function PanelBody({
  loading,
  errorMessage,
  ansiText,
  onPanelSwitch,
}: {
  loading: boolean;
  errorMessage: string | null;
  ansiText: string | null;
  onPanelSwitch: (id: PanelId) => void;
}) {
  if (loading) return <div className="panelout">running…</div>;
  if (errorMessage) return <div className="panelerr">{errorMessage}</div>;
  if (ansiText !== null)
    return (
      <pre className="panelout">
        <AnsiText text={ansiText} onPanelSwitch={onPanelSwitch} />
      </pre>
    );
  return <div className="panelout">not run yet — this panel probes the machine, so it runs only when you ask.</div>;
}
