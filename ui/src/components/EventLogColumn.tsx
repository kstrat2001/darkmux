import { useMemo, useRef, useState, type KeyboardEvent } from "react";
import type { FlowRecord } from "../types/handwritten";
import { recKey } from "../lib/flow";
import { LIVE_WINDOW_MS } from "../lib/flow";
import { clk } from "../lib/format";
import { RecordView } from "./RecordView";

/** `activityOf()` — viewer.html:1014-1038, the subset this column's "model
 * only" quick filter and row tags need (reasoning/tool-call/turn/dispatch
 * lifecycle/telemetry). Not the full mapping (session end, machine
 * online/offline aren't reachable from a live-window record set this
 * column ever renders) — extend if a future consumer needs those labels. */
function activityOf(r: FlowRecord): string {
  const a = r.action || "";
  if (a === "dispatch.reasoning") return "reasoning";
  if (a === "dispatch.tool") return "tool call";
  if (a === "dispatch.turn") return "turn";
  if (a === "dispatch.turn.heartbeat") return "heartbeat";
  if (a === "dispatch.start" || a === "dispatch start") return "dispatch start";
  if (a === "dispatch.complete" || a === "dispatch complete") return "dispatch end";
  if (a === "dispatch.error" || a === "dispatch error") return "dispatch error";
  if (a === "tier-decision") return "routing";
  if (a === "dispatch.compaction" || r.source === "compaction") return "compaction";
  if (r.category === "telemetry") return r.source || "telemetry";
  return a || "other";
}

/** `ACT_ICON`'s "model only" subset — viewer.html:861's `onlymodel` quick
 * filter (`onlyModelActivity()`), reproduced as a fixed set rather than the
 * full checkbox-per-facet filter modal (`#modalbg`/`#filterbody`,
 * viewer.html:857-863) — see this file's module doc for why the modal
 * itself is a named follow-up, not reproduced here. */
const MODEL_ACTIVITIES = new Set(["reasoning", "tool call", "turn"]);

/** Row cap — `renderLog()`'s `all.slice(-50).reverse()` (viewer.html:2443):
 * newest 50, newest-first. */
const LOG_CAP = 50;

/** The live window the counter reports, in hours — the status bar used to
 *  state this and no longer does (it belongs beside the records). */
const WINDOW_HOURS = Math.round(LIVE_WINDOW_MS / 3600000);

const MIN_DETAIL_PCT = 15;
const MAX_DETAIL_PCT = 70;
const DEFAULT_DETAIL_PCT = 38;

/** Enter/Space activates a `role="button"` `<div>` the same way a native
 * `<button>` would (matching `RunsBoard.tsx`'s own `onActivateKeyDown`) —
 * needed because `.eventlog__rec` is a click-only div with no other
 * keyboard path to selecting a record from the log. */
function onActivateKeyDown(onActivate: () => void) {
  return (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onActivate();
    }
  };
}

/**
 * The event-log column (`.log`, viewer.html:829-849) — the per-record
 * stream, its search box + "model only" quick filter, the follow-latest
 * toggle, the drag-to-resize `.split` handle, and the `#detail`
 * selected-event panel. Rendered by `App.tsx` only when
 * `lib/route.ts`'s `showsEventLog(route)` is true (fleet / a session
 * drill-in / a bare-date playback / the mission-redirect fallback — see
 * that function's own doc for the verified visibility rule, which corrects
 * a wrong packet-brief claim about `console`).
 *
 * **`records` is whatever `useRouteRecords` says this ROUTE means** — the
 * rolling live 2-day window on live routes, and the FETCHED SLICE on
 * `session` (`/flow-session/<id>`) and `playback` (`/flow/<date>`).
 *
 * (#1800 P1) It used to be the live window on every route — named here as a
 * deliberate deferral, which is honest but meant a `#session=` route listed
 * unrelated live traffic beside a stage headed "session replay". Legacy never
 * had that: `boot()` re-scopes `RAW` to the fetched slice before rendering.
 * The fix needed no second pipeline, because this column already took
 * `records` as a prop — only the routing decision was missing.
 *
 * `loading` and `error` exist for the same reason: refusing to fall back to
 * live records on a failed fetch is only honest if the column can say WHY it
 * is empty. A dead daemon, a 500, a typo'd session id and a genuinely quiet
 * day must not render identically.
 *
 * **`#logscope`** moves here from its previous App-level standalone
 * `<span>` (see `App.tsx`'s own doc) — legacy nests it INSIDE `.loglist`'s
 * `<h3>` (`event log · <span id="logscope">`), and rendering it loose
 * above the stage regardless of lens produced the stray uppercase "FLEET"
 * the operator caught on `/next` (this packet's whole reason for existing).
 *
 * **`visible` — this component is ALWAYS MOUNTED, never conditionally
 * unmounted** (a correction mid-packet: an earlier version of this
 * component only rendered when `showsEventLog(route)` was true, which
 * seemed right — "hidden lens, no log column" — until `next-parity.spec.ts`
 * proved it wrong. Legacy's own `#logscope` ALSO exists, with REAL non-empty
 * text, on `runs`/`console`/`machine` — it's just visually hidden (an
 * ANCESTOR gets `display:none`, verified: a Playwright probe showed
 * `#logscope`'s `innerText` on the hidden `machine` lens still reads
 * "MacBook-Pro", because `innerText` on a non-rendered element falls back
 * to `textContent`, not empty string — same finding `showsEventLog`'s own
 * doc cites). `goldens/machine.txt`/`machine-deeplink.txt` capture that
 * REAL, non-empty text, and `next-parity.spec.ts` byte-compares `/next`
 * against those goldens — so unmounting this component on `machine` made
 * `#logscope` genuinely absent, which is MORE different from legacy than
 * the bug this packet exists to fix. The fix: mount unconditionally, and
 * hide the whole column via a CSS class (`eventlog--hidden`,
 * `display:none`) exactly mirroring legacy's own mechanism — `#logscope`
 * keeps real text, `innerText` still finds it (same non-rendered-element
 * fallback), and `#stage` still gets the full row width (a `display:none`
 * flex item doesn't participate in flex layout, so `.app-shell__stage`'s
 * `flex:1` expands on its own — no separate CSS class needed on the row).
 *
 * **The filter MODAL is a named, deliberate cut, not a half-build.**
 * `MODEL_ACTIVITIES` above reproduces exactly the modal footer's one
 * always-available quick action ("model only"); the full category/tier/
 * source/activity checkbox grid is a real follow-up (`.fbtn` here toggles
 * the quick filter directly instead of opening a modal with checkboxes).
 *
 * **(Mobile fix pass) `#fbtn` no longer renders legacy's funnel glyph.**
 * Legacy's `ICON.filter` is a real funnel SVG, and its own `title`/
 * `aria-label` literally say "filters" — an honest name for a control that
 * opens the full modal. This button does something narrower (a one-shot
 * "model activity only" toggle), and the `title`/`aria-label` text already
 * said so — but `title` is a HOVER affordance, invisible on a phone with no
 * mouse. The operator tapped it expecting the modal anyway: the glyph was
 * the only thing a touch user actually sees before tapping, and an
 * ambiguous icon in the funnel's old visual slot still reads as "open
 * filters" regardless of what the hidden title says. Fixed by replacing the
 * glyph with a real VISIBLE label ("MODEL") — no hover required to know
 * what tapping it does — rather than reproducing the funnel shape (which
 * would keep implying the modal) or any other icon that still needs a
 * tooltip to explain itself. `title`/`aria-label` are unchanged and still
 * carry the fuller sentence for pointer/screen-reader users. Untested by
 * every parity golden (`extractLensText` only reads `#topbar`/`#crumb`/
 * `#meta`/`#logscope`/`#stage` — `.eventlog__head`'s buttons are outside
 * all five regions), so this is a zero-golden-risk change; verified no
 * golden references either the old glyph or "MODEL".
 */
export function EventLogColumn({
  records,
  visible,
  scopeLabel,
  loading = false,
  error = null,
}: {
  records: FlowRecord[];
  visible: boolean;
  /** (#1800 P1) A historical slice still in flight. Without this, "loading"
   *  and "genuinely empty" both render "no events yet". */
  loading?: boolean;
  /** (#1800 P1) Why the slice is empty, when it failed. A dead daemon and a
   *  quiet day must not render identically — see `useRouteRecords`. */
  error?: { status: number | null; message: string } | null;
  /** Not shown — written into a hidden span purely so this port's parity
   *  extraction matches legacy's. See App's `routeChrome` note. */
  scopeLabel: string;
}) {
  const [query, setQuery] = useState("");
  const [modelOnly, setModelOnly] = useState(false);
  const [follow, setFollow] = useState(true);
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [detailPct, setDetailPct] = useState(DEFAULT_DETAIL_PCT);

  const columnRef = useRef<HTMLDivElement | null>(null);
  const dragRef = useRef<{ startY: number; startPct: number } | null>(null);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return records.filter((r) => {
      if (modelOnly && !MODEL_ACTIVITIES.has(activityOf(r))) return false;
      if (q && !JSON.stringify(r).toLowerCase().includes(q)) return false;
      return true;
    });
  }, [records, query, modelOnly]);

  const capped = filtered.length > LOG_CAP;
  const visibleRecs = useMemo(() => filtered.slice(-LOG_CAP).reverse(), [filtered]);

  const selected = useMemo(() => {
    if (follow) return visibleRecs[0] ?? null;
    return visibleRecs.find((r) => recKey(r) === selectedKey) ?? null;
  }, [follow, visibleRecs, selectedKey]);

  function selectRecord(r: FlowRecord) {
    setSelectedKey(recKey(r));
    setFollow(false);
  }

  function toggleFollow() {
    setFollow((f) => {
      const next = !f;
      if (next) setSelectedKey(null);
      return next;
    });
  }

  // `.split` drag-to-resize — pointer events (not mouse-only) so it works
  // on the phone-width layout too, even though `.eventlog__split` is
  // display:none there (mirrors legacy's own "the row-resize handle isn't
  // useful when stacked" call, viewer.html:679).
  function onSplitPointerDown(e: React.PointerEvent<HTMLDivElement>) {
    dragRef.current = { startY: e.clientY, startPct: detailPct };
    e.currentTarget.setPointerCapture(e.pointerId);
  }
  function onSplitPointerMove(e: React.PointerEvent<HTMLDivElement>) {
    const drag = dragRef.current;
    const col = columnRef.current;
    if (!drag || !col) return;
    const totalH = col.getBoundingClientRect().height || 1;
    const deltaPct = ((e.clientY - drag.startY) / totalH) * 100;
    const next = Math.min(MAX_DETAIL_PCT, Math.max(MIN_DETAIL_PCT, drag.startPct + deltaPct));
    setDetailPct(next);
  }
  function onSplitPointerUp(e: React.PointerEvent<HTMLDivElement>) {
    dragRef.current = null;
    e.currentTarget.releasePointerCapture(e.pointerId);
  }

  const q = query.length > 0;
  const qcountText = q
    ? filtered.length
      ? `${filtered.length} match${filtered.length === 1 ? "" : "es"}${capped ? ` · ${LOG_CAP} shown` : ""}`
      : "no match"
    // (operator) "newest 50 of 734" -> "50 of 734". The word carried no
    // information the newest-first ordering does not already show, and this
    // chip sits beside a live stream where every extra word is noise.
    //
    // The CAP itself is legacy's (`all.slice(-50)`, viewer.html:2443) — what
    // this port adds is SAYING so. Legacy hides 684 records in silence; the
    // label is the honest half and worth keeping.
    : capped
      ? `${LOG_CAP} of ${filtered.length} events`
      : `${filtered.length} events`;

  return (
    <div className={`eventlog${visible ? "" : " eventlog--hidden"}`} ref={columnRef}>
      <div className="eventlog__detail" id="detail" style={{ flexBasis: `${detailPct}%` }}>
        {/* (operator) No "selected event" title. It was static chrome
            competing with the record's own headline — `RecordView` already
            leads with the action in accent colour, so the label was a second
            heading fighting the real one, and one more thing to read before
            reaching the content. The empty-state line below still explains
            the panel when nothing is selected, which is the only moment a
            title would have earned its place. Free to remove: this panel sits
            outside every extracted golden region. */}
        <div id="detailbody" className="eventlog__detailbody">
          {selected ? <EventDetail record={selected} /> : <div className="eventlog__none">select an event from the log to inspect it</div>}
        </div>
      </div>
      <div
        className="eventlog__split"
        id="split"
        title="drag to resize"
        onPointerDown={onSplitPointerDown}
        onPointerMove={onSplitPointerMove}
        onPointerUp={onSplitPointerUp}
      />
      <div className="eventlog__list">
        <div className="eventlog__head">
          <h3>
            {/* (operator) The header names the WINDOW; the outer UI owns
                context. `#logscope` repeated what the active tab or the crumb
                had already established in six of its eight states — and in
                two of those it was VAGUER than the crumb beside it ("mission"
                against `◆ <mission id>"). Kept in the DOM, empty and hidden,
                so legacy's extraction and this port's agree; the element
                itself is legacy's and dies with it at the flip. */}
            <span>
              events last {WINDOW_HOURS}h<span id="logscope" hidden>{scopeLabel}</span>
            </span>
            <span className="eventlog__headbtns">
              <button
                type="button"
                className={`eventlog__follow${follow ? " on" : ""}`}
                id="follow"
                title={follow ? "following the latest events" : "click to follow latest"}
                aria-label={follow ? "following the latest events" : "click to follow latest"}
                onClick={toggleFollow}
              >
                ⏱
              </button>
              <button
                type="button"
                className={`eventlog__fbtn${modelOnly ? " on" : ""}`}
                id="fbtn"
                title={modelOnly ? "showing model activity only" : "show only reasoning, tool calls, and turns"}
                aria-label={modelOnly ? "showing model activity only" : "show only reasoning, tool calls, and turns"}
                aria-pressed={modelOnly}
                onClick={() => setModelOnly((v) => !v)}
              >
                MODEL
              </button>
            </span>
          </h3>
          <div className="eventlog__search">
            <div className="eventlog__searchbox">
              <span className="eventlog__sicon" aria-hidden="true">
                ⌕
              </span>
              <input
                id="logq"
                type="search"
                placeholder="filter the stream…"
                autoComplete="off"
                spellCheck={false}
                aria-label="search events"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
              />
              {query ? (
                <button
                  className="eventlog__sclear"
                  id="sclear"
                  type="button"
                  aria-label="clear search"
                  title="clear search"
                  onClick={() => setQuery("")}
                >
                  ✕
                </button>
              ) : null}
            </div>
            <span className={`eventlog__qcount${qcountText ? " show" : ""}${q && filtered.length === 0 ? " zero" : ""}`} id="qcount" aria-live="polite">
              {qcountText}
            </span>
          </div>
        </div>
        <div id="logbody" className="eventlog__body">
          {visibleRecs.length ? (
            visibleRecs.map((r) => {
              const key = recKey(r);
              return (
                <div
                  key={key}
                  className={`eventlog__rec${selected && recKey(selected) === key ? " sel" : ""}`}
                  data-act="rec"
                  role="button"
                  tabIndex={0}
                  onClick={() => selectRecord(r)}
                  onKeyDown={onActivateKeyDown(() => selectRecord(r))}
                >
                  <span className="eventlog__rectime">{clk(Date.parse(r.ts))}</span>{" "}
                  <span className="eventlog__ractivity">{activityOf(r)}</span>
                  {r.machine_id ? <span className="eventlog__recmachine"> · {r.machine_id}</span> : null}
                  {r.session_id ? <span className="eventlog__recsession"> · {r.session_id}</span> : null}
                </div>
              );
            })
          ) : (
            <div className="eventlog__empty" data-state={error ? "error" : loading ? "loading" : "empty"} role={error ? "alert" : undefined}>
              {error
                ? `couldn't load events${error.status !== null ? ` (HTTP ${error.status})` : ""}: ${error.message}`
                : loading
                  ? "loading…"
                  : query
                    ? "no events match your search"
                    : "no events yet"}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

/** `renderDetail()`'s fallback branch (viewer.html:2417-2418,
 * `` `<pre>${pretty(r)}</pre>` ``) — this column doesn't reproduce the
 * structured per-action cards legacy builds for reasoning/tool/compaction/
 * tier-decision records (viewer.html:2370-2415); every selected record
 * renders through this ONE pretty-printed-JSON view instead. A real,
 * working detail pane (the operator can inspect any field of any selected
 * event) — just not the bespoke per-action layout, named as a follow-up
 * rather than partially reproducing four separate card shapes. */
function EventDetail({ record }: { record: FlowRecord }) {
  return (
    <div className="eventlog__detailcard">
      <RecordView record={record as unknown as Record<string, unknown>} />
    </div>
  );
}
