/**
 * (#2107 tabbed-drawer packet, restructured #2108 "one card" packet) The
 * phone-only (≤768px) bottom chrome — ONE sheet, one handle, two tabs.
 *
 * **Why a separate component, not a third branch inside `MachineDrawer`:**
 * the tab-bar/drag/per-tab-height machinery here is generic chrome that
 * knows nothing about machine stats OR events — it takes two pre-built
 * panels (`machineTab.body`, an `EventLogColumn` instance) and hosts them.
 * `MachineDrawer.tsx` still owns the DECISION of which skin to render
 * (desktop dialog vs this), and still owns the machine-stats DATA
 * (`useMachineStatsContent`, shared with the desktop dialog so the two
 * skins never duplicate that logic — see that hook's own doc).
 *
 * **Layout, ONE card (#2108, operator finding — real-device review):** the
 * #2107 shape had `.phone-drawer__bar` (handle+tabs) as an ALWAYS-VISIBLE
 * separately-styled strip and `.phone-drawer__body` (content) as a SECOND
 * element sliding independently behind/under it — on a real phone this
 * read as two stacked cards, not one sheet, and the tabs never
 * participated in the drag. `.phone-drawer` is now the ONE sliding/
 * growing element (`position:fixed`, bottom-anchored) and is the sole
 * thing that owns a background/border/border-radius — a real card. It
 * contains, top to bottom: `.phone-drawer__bar` (handle, then the two
 * tabs — the drag handle sits at the sheet's own TOP edge) FIRST, then
 * `.phone-drawer__body` (the active tab's content) SECOND, filling the
 * rest of the sheet's height. Both are permanent children of the SAME
 * subtree; there is no second independently-transformed element.
 *
 * **Height, not transform, drives open/close.** Closed, `.phone-drawer`
 * has NO inline `height` — it falls back to `styles.css`'s own rule
 * (`calc(58px + env(safe-area-inset-bottom, 0px))`, just enough for the
 * handle+tabs, which is all that's visible). Open, an inline
 * `style={{ height: `${openPct}vh` }}` overrides that, and CSS `height`
 * genuinely transitions between the two (both are concrete lengths — no
 * `auto`, so it interpolates smoothly) — `.phone-drawer__bar` stays
 * pinned at the top of whichever height is current, `.phone-drawer__body`
 * (`flex: 1`, gated open) fills what's left below it. The 2026-08-29
 * "always mounted" fix that answered the OLD problem (a conditionally-
 * mounted element can't play an exit transition) still applies here —
 * `.phone-drawer` itself is always mounted, only removing/adding the
 * inline height style, which the browser can transition either direction.
 * Dragging disables the transition via `.phone-drawer--dragging` (so the
 * sheet tracks the finger with zero lag) and it's re-enabled on release.
 *
 * **The body's own content stays gated on `open`** (`{open &&
 * (activeTab === "machine" ? ... : <EventLogColumn/>)}`) even though the
 * wrapper is always mounted — this is what keeps `EventLogColumn` (and the
 * machine stats panel's own daemon polling, gated separately via
 * `onMachineOpenChange` below) from rendering/fetching while the sheet is
 * closed (58px tall — there is no room to show it anyway).
 *
 * **Modal while open, at ANY height (operator finding, phone install
 * review):** the page behind is unusable while the drawer is open — body
 * scroll is locked for the drawer's ENTIRE open lifetime now, not just
 * "past ~50%" the way the original spec had it, and a transparent
 * full-viewport `.phone-drawer__backdrop` (no visible dimming — the sheet
 * itself already reads as the foreground) sits behind the drawer and in
 * front of the page, so a tap anywhere outside the sheet DISMISSES it
 * instead of reaching whatever button/link happened to be under it.
 *
 * **Tap/drag semantics on the handle** (spec, #2107 packet brief; height
 * behavior revised #2108, operator correction — see `lib/drawerStorage.ts`'s
 * own doc for the full "why"):
 * - tapping a CLOSED tab opens the drawer to that tab, at the ONE shared
 *   remembered height (`lib/drawerStorage.ts`, default `DEFAULT_OPEN_PCT`
 *   when nothing is stored yet) — the SAME height for either tab, not a
 *   per-tab lookup.
 * - tapping the ACTIVE tab while open closes the drawer (the height stays
 *   remembered for next time — nothing is persisted on a mere close).
 * - tapping the OTHER tab while open switches to it WITHOUT touching the
 *   height at all — the sheet's height/transform never change on a tab
 *   switch, so the slide transition never replays for one.
 * - tapping the handle (no real drag, see `TAP_SLOP_PX`) closes when open,
 *   opens to the active tab when closed.
 * - dragging the handle live-resizes the sheet between `MIN_OPEN_PCT` and
 *   `MAX_OPEN_PCT`; releasing below `CLOSE_SNAP_PCT` snaps closed instead
 *   of leaving a barely-open sliver, and any other release PERSISTS the
 *   resulting height for the currently active tab only.
 * - Escape, and a click on the backdrop (anywhere outside the sheet), close
 *   it too.
 *
 * Pointer Events (not Touch Events) for the drag, matching this codebase's
 * OWN existing drag convention — `EventLogColumn.tsx`'s `onSplitPointerDown`
 * / `onSplitPointerMove` / `onSplitPointerUp` — rather than the old
 * `MachineDrawer.tsx` sheet's raw `TouchEvent` handlers (which this packet
 * retires along with the sheet they belonged to). Pointer Events cover
 * mouse AND touch through one code path and are what the existing split
 * resizer already proved works for a real phone.
 */
import { useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import { EventLogColumn } from "./EventLogColumn";
import { isLiveRoute, type Route } from "../lib/route";
import type { LiveTailStatus } from "../hooks/useLiveTail";
import type { FlowRecord } from "../types/handwritten";
import {
  loadDrawerHeightPct,
  saveDrawerHeightPct,
  type DrawerTabId,
} from "../lib/drawerStorage";


/** vh used the first time a tab is ever opened (nothing stored yet).
 * (#2108, operator finding) Raised from a half-sheet (50) to most of the
 * viewport (85-90 is the instructed range) — a phone drawer that only
 * shows half the screen leaves the operator scrolling BOTH the sheet and
 * its own content unnecessarily; the sheet should be the primary surface
 * once open, for both tabs. `MAX_OPEN_PCT` already accommodates this. */
const DEFAULT_OPEN_PCT = 88;
/** The spec's own ceiling ("~90vh"). */
const MAX_OPEN_PCT = 90;
/** A floor once genuinely open — below this the sheet reads as "barely
 * there" rather than a deliberate small peek, so a release this low snaps
 * fully closed instead (see `CLOSE_SNAP_PCT`). */
const MIN_OPEN_PCT = 14;
/** Releasing the handle at or below this height closes the drawer instead
 * of leaving it pinned open at a sliver — this is what makes "swipe down"
 * read as a close gesture rather than "resize to almost nothing". */
const CLOSE_SNAP_PCT = 20;
/** A pointerdown/up pair whose net travel is under this many px is a TAP,
 * not a drag — matches `MachineDrawer.tsx`'s retired 80px swipe-to-close
 * threshold in spirit but tuned tighter since this now gates tap-vs-drag
 * on the SAME handle rather than distinguishing a swipe on a separate bar. */
const TAP_SLOP_PX = 6;

function clampPct(pct: number): number {
  return Math.max(MIN_OPEN_PCT, Math.min(MAX_OPEN_PCT, pct));
}

export interface PhoneDrawerMachineTab {
  /** identity + scope + meters/idle + about — `useMachineStatsContent`'s
   * `body`, unchanged, reused rather than re-derived. The tab button
   * itself no longer shows a live line (`compactLine` retired, operator
   * finding: "looks too busy" — see `MachineDrawer.tsx`'s own doc); it
   * reads a static "Machine info" label instead, both open and closed. */
  body: ReactNode;
}

export interface PhoneDrawerEventsTab {
  records: FlowRecord[];
  scopeLabel: string;
  visible: boolean;
  loading: boolean;
  error: { status: number | null; message: string } | null;
  historical: boolean;
  serverTruncated: boolean;
}

export function PhoneDrawer({
  machineTab,
  events,
  liveStatus,
  route,
  onMachineOpenChange,
}: {
  machineTab: PhoneDrawerMachineTab;
  events: PhoneDrawerEventsTab;
  liveStatus: LiveTailStatus;
  route: Route;
  /** (#2107, #1833) Fired whenever "is the Machine tab open right now"
   * changes — `open && activeTab === "machine"`. `MachineDrawer.tsx`
   * mirrors this into its own state so it can gate the daemon-load poll on
   * it: this component owns open/activeTab internally (uncontrolled, so
   * the drag/tap/persistence logic below stays self-contained), but the
   * PARENT is the one that knows whether to poll `/machine/resources`, and
   * it can't call a hook conditionally on state it doesn't hold. Optional
   * — omitted in tests that don't care. */
  onMachineOpenChange?: (open: boolean) => void;
}) {
  const [open, setOpen] = useState(false);
  const [activeTab, setActiveTab] = useState<DrawerTabId>("machine");
  // (#2108, operator correction) ONE shared height for BOTH tabs, not
  // per-tab — a real-device review found switching tabs while open
  // visibly resized the sheet (and replayed the slide transition)
  // whenever the two tabs' stored heights differed, which read as a
  // glitch. `lib/drawerStorage.ts`'s own doc has the full story; this
  // state is never re-derived from `activeTab` again after mount.
  const [openPct, setOpenPct] = useState<number>(
    () => loadDrawerHeightPct() ?? DEFAULT_OPEN_PCT,
  );
  const [dragging, setDragging] = useState(false);
  const dragRef = useRef<{
    startY: number;
    startPct: number;
    startOpen: boolean;
    moved: boolean;
  } | null>(null);
  // (iOS scroll-lock fix) The backdrop's own non-passive `touchmove`
  // listener needs a real DOM node to attach to — see that effect's own
  // doc below.
  const backdropRef = useRef<HTMLDivElement | null>(null);

  function close() {
    setOpen(false);
  }

  // `openPct` is DELIBERATELY untouched here — opening (either tab, from
  // closed) keeps whatever height is already set (the persisted/dragged
  // value, or `DEFAULT_OPEN_PCT` on a fresh session). See this file's own
  // doc on why the height stopped being a per-tab lookup.
  function openTab(tab: DrawerTabId) {
    setActiveTab(tab);
    setOpen(true);
  }

  function onTabClick(tab: DrawerTabId) {
    if (!open) {
      openTab(tab);
      return;
    }
    if (activeTab === tab) {
      close();
      return;
    }
    // Switching tabs while open: ONLY `activeTab` changes. `openPct` is
    // NOT touched — this is the actual fix (see this file's own doc) —
    // so the sheet's height/transform stay exactly as they were, and the
    // slide transition never replays.
    setActiveTab(tab);
  }

  // (#2107, #1833) Report "is the Machine tab open" up to `MachineDrawer`
  // on every change — including the very first render, so a parent that
  // starts assuming closed (matching this component's own initial state)
  // never has to guess.
  useEffect(() => {
    onMachineOpenChange?.(open && activeTab === "machine");
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `onMachineOpenChange` is a `useState` setter in the one real caller (`MachineDrawer.tsx`), whose identity React guarantees stable; omitted from deps so a test that passes a fresh inline arrow each render doesn't re-fire this on every unrelated re-render.
  }, [open, activeTab]);

  // (operator finding, phone install review) The page behind is unusable
  // while the drawer is open, at ANY height — not just "past ~50%" the
  // original spec had it (that graduated threshold is retired; scroll is
  // locked for the drawer's entire open lifetime now, matching the
  // backdrop's own "modal the instant it's open" treatment below).
  //
  // (iOS scroll-lock fix, operator finding — a real iOS Safari Home Screen
  // web app install) `overflow: hidden` on `<body>` alone is IGNORED by
  // iOS Safari: a drag on the open sheet still scrolled the page BEHIND
  // it, worse than having no lock at all (the touch visually passed
  // through the front layer). The iOS-proof form pins the body via
  // `position: fixed` at its CURRENT scroll offset — `top` carries the
  // negative of that offset so nothing visually jumps — and restores both
  // the styles and the exact scroll position on close via
  // `window.scrollTo`. `overflow: hidden` stays alongside it (harmless,
  // and still what non-iOS browsers key their own lock behavior off).
  //
  // A SINGLE effect keyed on `open` covers every exit path uniformly: the
  // cleanup function below fires on close (tab re-tap, Escape, backdrop
  // click — all just flip `open` to `false`) AND on unmount (a route
  // change while the drawer happens to be open), so body can never be
  // left stuck `position: fixed` with no matching restore.
  useEffect(() => {
    if (!open) return undefined;
    const body = document.body;
    const scrollY = window.scrollY || window.pageYOffset || 0;
    const prev = {
      position: body.style.position,
      top: body.style.top,
      left: body.style.left,
      right: body.style.right,
      width: body.style.width,
      overflow: body.style.overflow,
    };
    body.style.position = "fixed";
    body.style.top = `-${scrollY}px`;
    body.style.left = "0";
    body.style.right = "0";
    body.style.width = "100%";
    body.style.overflow = "hidden";
    return () => {
      body.style.position = prev.position;
      body.style.top = prev.top;
      body.style.left = prev.left;
      body.style.right = prev.right;
      body.style.width = prev.width;
      body.style.overflow = prev.overflow;
      window.scrollTo(0, scrollY);
    };
  }, [open]);

  // Escape closes, matching every other dialog-shaped surface in this app.
  useEffect(() => {
    if (!open) return undefined;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open]);

  // (iOS scroll-lock fix) React attaches its own synthetic `onTouchMove` as
  // a PASSIVE native listener (the framework's default for touch/wheel
  // events since React 17), so a JSX `onTouchMove={...}` handler on the
  // backdrop cannot reliably `preventDefault()` a scroll/rubber-band
  // gesture — the browser has already committed to scrolling by the time
  // the passive handler runs. A manually-attached NATIVE listener with
  // `{ passive: false }` is the only way to actually block it; `open` is
  // in the deps because the backdrop itself is conditionally MOUNTED
  // (`{open && <div ... />}` below), so this re-attaches every time a new
  // backdrop node appears.
  useEffect(() => {
    const el = backdropRef.current;
    if (!el) return undefined;
    const onTouchMove = (e: TouchEvent) => {
      e.preventDefault();
    };
    el.addEventListener("touchmove", onTouchMove, { passive: false });
    return () => el.removeEventListener("touchmove", onTouchMove);
  }, [open]);

  function onHandlePointerDown(e: React.PointerEvent) {
    (e.currentTarget as Element).setPointerCapture?.(e.pointerId);
    dragRef.current = {
      startY: e.clientY,
      startPct: open ? openPct : 0,
      startOpen: open,
      moved: false,
    };
    setDragging(true);
  }
  function onHandlePointerMove(e: React.PointerEvent) {
    const drag = dragRef.current;
    if (!drag) return;
    const dy = drag.startY - e.clientY; // positive == dragged UP == taller
    if (Math.abs(dy) > TAP_SLOP_PX) drag.moved = true;
    if (!drag.moved) return;
    const vh = window.innerHeight || 1;
    const deltaPct = (dy / vh) * 100;
    const nextPct = clampPct(drag.startPct + deltaPct);
    if (!open) setOpen(true);
    setOpenPct(nextPct);
  }
  function onHandlePointerUp(e: React.PointerEvent) {
    const drag = dragRef.current;
    dragRef.current = null;
    setDragging(false);
    (e.currentTarget as Element).releasePointerCapture?.(e.pointerId);
    if (!drag) return;
    if (!drag.moved) {
      // A tap, not a drag (spec: "tapping ... the handle ... closes it").
      if (drag.startOpen) close();
      else openTab(activeTab);
      return;
    }
    setOpenPct((pct) => {
      if (pct <= CLOSE_SNAP_PCT) {
        setOpen(false);
        return drag.startPct > 0 ? drag.startPct : DEFAULT_OPEN_PCT;
      }
      saveDrawerHeightPct(pct);
      return pct;
    });
  }

  const dotState: "live" | "reconnecting" | "replay" = !isLiveRoute(route)
    ? "replay"
    : liveStatus === "live"
      ? "live"
      : "reconnecting";

  return (
    <>
      {/* (operator finding) A tap anywhere outside the sheet dismisses it —
          transparent (no additional dimming beyond the sheet's own opaque
          background), sits behind `.phone-drawer` (lower z-index) so the
          drawer's own bar/body/handle still receive their own clicks
          normally; only a tap that lands OUTSIDE those elements reaches
          this backdrop and closes the drawer instead of whatever page
          content happened to be underneath. */}
      {open && (
        <div
          ref={backdropRef}
          className="phone-drawer__backdrop"
          data-act="phone-drawer-backdrop"
          aria-hidden="true"
          onClick={close}
        />
      )}
      <div
        className={`phone-drawer${open ? " phone-drawer--open" : ""}${dragging ? " phone-drawer--dragging" : ""}`}
        data-act="phone-drawer"
        // (#2108, "one card" packet) Closed: NO inline height — falls
        // back to `styles.css`'s own `calc(58px + env(safe-area-inset-
        // bottom, 0px))` rule, just enough for the handle+tabs below.
        // Open: the live-dragged `openPct`, which the browser transitions
        // to/from the closed CSS value smoothly (both are concrete
        // lengths — see this file's own doc on why that works without a
        // JS-side safe-area calculation).
        //
        // (#2108, operator finding — real device) `min(...)` caps the
        // rendered height so the sheet's TOP EDGE never draws over the
        // masthead: a plain `${openPct}vh` let the sheet grow tall enough
        // to cover the logo row on a real iPhone. `--masthead-h` is
        // `App.tsx`'s own `ResizeObserver`-measured value (that effect's
        // doc); `64px` is only the fallback for the one frame before it
        // has run. This only clamps what's PAINTED — `openPct` itself
        // (the drag state, persistence, `MAX_OPEN_PCT`) is untouched, so
        // the drag still tracks the finger 1:1 up to the same ceiling the
        // finger would visually hit.
        style={
          open
            ? { height: `min(${openPct}vh, calc(100vh - var(--masthead-h, 64px) - 8px))` }
            : undefined
        }
      >
        {/* (#2108, "one card" packet) The handle + two tabs are now PART
            of the sliding sheet, not a second, separately-positioned
            element — sitting at the sheet's own TOP edge (its FIRST
            child), so they visibly move/grow WITH the card instead of
            reading as a static strip a second card appears behind. */}
        <div className="phone-drawer__bar" data-act="phone-drawer-bar">
          <div
            className="phone-drawer__handle"
            data-act="phone-drawer-handle"
            role="button"
            tabIndex={0}
            aria-label={open ? "collapse the drawer" : "expand the drawer"}
            onPointerDown={onHandlePointerDown}
            onPointerMove={onHandlePointerMove}
            onPointerUp={onHandlePointerUp}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                if (open) close();
                else openTab(activeTab);
              }
            }}
          >
            <div className="phone-drawer__handle-bar" />
          </div>
          <div className="phone-drawer__tabs">
            <button
              type="button"
              className={`phone-drawer__tab${open && activeTab === "machine" ? " phone-drawer__tab--active" : ""}`}
              data-act="phone-drawer-tab-machine"
              aria-pressed={open && activeTab === "machine"}
              aria-label="Machine info"
              onClick={() => onTabClick("machine")}
            >
              {/* (operator finding) A static label, not a live line — see
                  `PhoneDrawerMachineTab.body`'s own doc. */}
              <span className="phone-drawer__tabvalue">Machine info</span>
            </button>
            <button
              type="button"
              className={`phone-drawer__tab${open && activeTab === "events" ? " phone-drawer__tab--active" : ""}`}
              data-act="phone-drawer-tab-events"
              aria-pressed={open && activeTab === "events"}
              aria-label="Events"
              onClick={() => onTabClick("events")}
            >
              <span className="phone-drawer__tabvalue">
                Events · {events.records.length}
                <span
                  className={`phone-drawer__dot phone-drawer__dot--${dotState}`}
                  aria-hidden="true"
                />
              </span>
            </button>
          </div>
        </div>
        {/* Always mounted (never conditionally unmounted) — same reasoning
            as before the "one card" restructure, just now a plain flex
            section (`flex: 1`) inside the ONE sliding sheet rather than a
            second independently-animated element. The CONTENT inside
            stays gated on `open` so nothing renders/polls/fetches while
            the sheet is closed (58px tall — there is no room for it
            anyway). */}
        <div
          className="phone-drawer__body"
          data-act="phone-drawer-body"
          role={open ? "dialog" : undefined}
          aria-modal={open ? true : undefined}
          aria-hidden={!open}
          aria-label={activeTab === "machine" ? "Machine info" : "Events"}
        >
          {open &&
            (activeTab === "machine" ? (
              <div
                className="phone-drawer__panel"
                data-act="phone-drawer-panel-machine"
              >
                {machineTab.body}
              </div>
            ) : (
              // (#2108, operator correction) The events tab is the SAME
              // list + detail-pane split the desktop events column uses —
              // not a drill-in push, not every row's detail expanded
              // inline (both tried, both wrong: a route change/back button
              // for something that should stay ON the sheet; an unreadable
              // wall of expanded records). Tapping a row selects it and
              // shows its detail in the pane, right here in the sheet — no
              // navigation. `styles.css`'s `.phone-drawer__body .eventlog`
              // block reorders the pane BELOW the list (desktop's own
              // order is pane-above-list) and sizes it via the SAME
              // `detailPct` split state the desktop column already has
              // (default ~38/62, i.e. the instructed "~40/60"; not
              // draggable on a phone — the resize handle's existing
              // `@media (max-width:768px) { .eventlog__split { display:
              // none } }` rule already hides it here, same as it always
              // has for a stacked-width `EventLogColumn`). Neither
              // `pushDetail` nor `inlineDetail` — this is `EventLogColumn`
              // at its plain default.
              <EventLogColumn
                paneId="phone-drawer"
                scopeLabel={events.scopeLabel}
                records={events.records}
                visible={events.visible}
                loading={events.loading}
                error={events.error}
                historical={events.historical}
                serverTruncated={events.serverTruncated}
              />
            ))}
        </div>
      </div>
    </>
  );
}
