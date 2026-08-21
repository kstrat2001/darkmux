import { useEffect, useState, type KeyboardEvent } from "react";
import { useQuery } from "@tanstack/react-query";
import { queryKeys, PANEL_CACHE_MS } from "../../lib/queryKeys";
import type { PanelId } from "../../lib/route";
import { PANELS, isManualPanel, panelCols } from "./panels";
import { fetchPanel } from "./fetchPanel";
import { panelAgeLabel } from "./format";
import { AnsiText } from "./ansi";
import { ActivityPanel } from "./ActivityPanel";
import type { PanelResponse } from "../../types/handwritten";

/** (#1904) The "all activity" escape hatch's id — mirrors the
 * `mission-status`/`mission-status-all` pairing already in `PANELS`
 * (`panels.ts`), but this pair is NOT CLI-backed (no `/panel/activity`
 * command exists), so it stays out of `PanelId`/`PANEL_IDS` entirely — see
 * `panels.ts`'s own doc on `PANELS` for why. `ConsoleSelection` below is
 * this component's own local widening of `PanelId` to cover both
 * client-only entries: `""` (the activity default, already the sentinel
 * `parseRoute` produces for "no `panel=` in the URL") and this string. */
const ACTIVITY_ALL_ID = "activity-all";
type ConsoleSelection = PanelId | "" | typeof ACTIVITY_ALL_ID;

/**
 * The console lens — `#lens=console&panel=<id>`. Pure port of
 * `viewer.html`'s `renderConsole()`/`loadPanel()`/`setPanel()`/`goConsole()`
 * (search that file for `── CLI panel:`). Renders an allowlisted CLI
 * command's own ANSI output, so the panel cannot drift from the CLI the way
 * a JS re-implementation could (`crates/darkmux-serve/src/panel.rs`'s own
 * module doc: "twin-drift becomes structurally impossible... a panel cannot
 * diverge from the CLI because it IS the CLI").
 *
 * (#1904) The one deliberate departure from that pure-CLI-mirror shape: the
 * console's DEFAULT landing state (`panelId === ""`) and its "all activity"
 * sibling (`panelId === ACTIVITY_ALL_ID`) are NOT CLI panels — they render
 * `ActivityPanel`, a client-rendered view over `/runs` (see that module's
 * own doc for the full origin story and why). Every OTHER `panelId` value
 * is a real, allowlisted CLI panel and renders exactly as before this
 * packet, via `CliPanelView` below (extracted from what used to be this
 * component's own inline body, unchanged in behavior).
 *
 * DOM/CSS shape is a direct port (`.runsbar`/`.runchip` tabs, `.panelwrap` >
 * `.panelchrome` + a `.panelout`/`.panelerr` body) — see `styles.css`'s own
 * "Console lens (Packet 6)" block for why the flex layout is load-bearing
 * for `innerText` byte-parity, not just visual.
 *
 * Fetch/cache mapping onto TanStack Query (the ONE genuinely non-literal
 * translation `CliPanelView` makes, since legacy hand-rolls its own
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
  // (#1904) `""` (no `panel=` in the URL, or an unrecognized one — both
  // already collapse to `""` in `parseRoute`) now means "land on the
  // activity view", not "fall back to `DEFAULT_PANEL_ID`" — there is no
  // more `DEFAULT_PANEL_ID` (retired; see `panels.ts`'s own doc). A real
  // `PanelId`, or `ACTIVITY_ALL_ID`, means the operator (or a deep link)
  // explicitly picked that tab.
  const [panelId, setPanelId] = useState<ConsoleSelection>(initialPanelId);

  // A fresh deep-link into a DIFFERENT panel while this component is already
  // mounted re-syncs local selection — mirrors `boot()`'s `if(nq)
  // state.panelId=nq` applying on every fresh console entry.
  useEffect(() => {
    if (initialPanelId) setPanelId(initialPanelId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [initialPanelId]);

  let body;
  if (panelId === "") {
    body = <ActivityPanel capped onShowAll={() => setPanelId(ACTIVITY_ALL_ID)} />;
  } else if (panelId === ACTIVITY_ALL_ID) {
    body = <ActivityPanel capped={false} />;
  } else {
    body = <CliPanelView id={panelId} onPanelSwitch={setPanelId} />;
  }

  return (
    <>
      <div className="runsbar">
        <PanelTab id="" label="activity" active={panelId === ""} onSelect={setPanelId} />
        <PanelTab id={ACTIVITY_ALL_ID} label="all activity" active={panelId === ACTIVITY_ALL_ID} onSelect={setPanelId} />
        {PANELS.map((p) => (
          <PanelTab key={p.id} id={p.id} label={p.label} active={p.id === panelId} onSelect={setPanelId} />
        ))}
      </div>
      {body}
    </>
  );
}

/** (#1904) The CLI-panel fetch/chrome/body machinery — the WHOLE of what
 * `ConsolePanel`'s own return used to inline directly, before "" and
 * `ACTIVITY_ALL_ID` needed a genuinely different (non-CLI) render branch.
 * Extracted rather than left inline so those two branches can skip this
 * entirely (component not mounted = its `useQuery` never fires, cleaner
 * than an `enabled` flag guarding a query with no valid `PanelId` to key
 * on) — behavior for every real `PanelId` is byte-for-byte unchanged from
 * before this packet. */
function CliPanelView({ id, onPanelSwitch }: { id: PanelId; onPanelSwitch: (id: PanelId) => void }) {
  const manual = isManualPanel(id);
  const query = useQuery({
    queryKey: queryKeys.panel(id),
    queryFn: () => fetchPanel(id, panelCols(document.querySelector(".panelout"))),
    enabled: !manual,
    staleTime: PANEL_CACHE_MS,
    refetchOnWindowFocus: !manual,
  });

  const loadedBody = query.data?.ok === true ? query.data.data : null;
  const errorMessage = query.data?.ok === false ? query.data.message : null;

  return (
    <div className="panelwrap">
      <div className="panelchrome">
        {loadedBody ? (
          <LoadedChrome body={loadedBody} fetchedAt={query.dataUpdatedAt} onRerun={() => query.refetch()} />
        ) : (
          <NotLoadedChrome id={id} loading={query.isFetching} onRun={() => query.refetch()} />
        )}
      </div>
      <PanelBody
        loading={query.isFetching}
        errorMessage={errorMessage}
        ansiText={loadedBody ? loadedBody.ansi_text || "" : null}
        exitCode={loadedBody ? loadedBody.exit_code : null}
        stderrTail={loadedBody ? loadedBody.stderr_tail || "" : ""}
        onPanelSwitch={onPanelSwitch}
      />
    </div>
  );
}

/** viewer.html: one `.runchip[data-act="setpanel"]` entry of the tab bar. */
function PanelTab({ id, label, active, onSelect }: { id: ConsoleSelection; label: string; active: boolean; onSelect: (id: ConsoleSelection) => void }) {
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
 * for why the tag choice (not just the class) matters.
 *
 * (#1908) The loaded branch used to render `ansi_text` (stdout) alone, so a
 * command that failed with empty stdout was indistinguishable from one that
 * succeeded with nothing to say — same header, same timing, same empty box.
 * The daemon already sends `exit_code` and `stderr_tail`
 * (`crates/darkmux-serve/src/panel.rs`) for exactly this; the viewer just
 * never read them.
 *
 * `failed` treats anything OTHER than a real `0` as not-a-clean-exit — this
 * covers `null` (`Command::status().code()`'s own signal-killed case) and,
 * defensively, a malformed/absent field despite the declared `number |
 * null` type (never leak the literal word "undefined" into the fallback
 * sentence below). `hasStdout`/`hasStderr` are then independent of
 * `failed`: a ZERO exit can still have written a warning to stderr, and
 * that warning is real content, not noise to bury under "no output" (QA
 * finding on this issue — dropping it would be the same silent-failure
 * shape #1908 exists to kill, one branch over). So the three regions
 * compose rather than branch exclusively:
 *   - `hasStdout` → the ANSI-rendered pre, unchanged from before this
 *     packet.
 *   - `hasStderr` (regardless of exit code) → `stderr_tail`, verbatim, in
 *     the SAME `.panelerr` treatment the HTTP-refusal path already uses
 *     (`errorMessage` above) — never reworded, per the daemon's own
 *     "surfaced so a failure isn't silent" comment.
 *   - `failed && !hasStderr` → a plain sentence naming the exit status
 *     instead of falling back to a blank box again. This one line isn't
 *     the daemon's own text (there isn't any to quote), but it still
 *     describes only what actually happened, never invents what the
 *     command might have meant.
 * Only when NONE of the three apply (clean exit, empty stdout, empty
 * stderr) does the plain "no output" empty state render — distinct
 * wording from "not run yet" (never fetched) so the two stay
 * distinguishable. */
function PanelBody({
  loading,
  errorMessage,
  ansiText,
  exitCode,
  stderrTail,
  onPanelSwitch,
}: {
  loading: boolean;
  errorMessage: string | null;
  ansiText: string | null;
  exitCode: number | null;
  stderrTail: string;
  onPanelSwitch: (id: PanelId) => void;
}) {
  if (loading) return <div className="panelout">running…</div>;
  if (errorMessage) return <div className="panelerr">{errorMessage}</div>;
  if (ansiText !== null) {
    const failed = typeof exitCode !== "number" || exitCode !== 0;
    const hasStdout = ansiText !== "";
    const hasStderr = stderrTail !== "";

    if (!failed && !hasStdout && !hasStderr) return <div className="panelout">no output</div>;

    return (
      <>
        {hasStdout && (
          <pre className="panelout">
            <AnsiText text={ansiText} onPanelSwitch={onPanelSwitch} />
          </pre>
        )}
        {(hasStderr || failed) && (
          <div className="panelerr">
            {hasStderr
              ? stderrTail
              : typeof exitCode === "number"
                ? `command exited with status ${exitCode} and printed nothing to stderr`
                : "command did not exit cleanly (killed) and printed nothing to stderr"}
          </div>
        )}
      </>
    );
  }
  return <div className="panelout">not run yet — this panel probes the machine, so it runs only when you ask.</div>;
}
