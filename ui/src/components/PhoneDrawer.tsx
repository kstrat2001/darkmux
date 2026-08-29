/**
 * (#2107 tabbed-drawer packet) The phone-only (≤768px) bottom chrome — a
 * persistent, always-in-flow "mainstay" bar with two tabs, `Machine` and
 * `Events`, that opens into a draggable-height sheet. Replaces the earlier
 * single-purpose `machine-bottombar`/single-sheet phone chrome
 * `MachineDrawer.tsx` used to render inline (see that file's own doc for
 * the desktop skin, which is unchanged by this packet).
 *
 * **Why a separate component, not a third branch inside `MachineDrawer`:**
 * the tab-bar/drag/per-tab-height machinery here is generic chrome that
 * knows nothing about machine stats OR events — it takes two pre-built
 * panels (`machineTab.body`, an `EventLogColumn` instance) and hosts them.
 * `MachineDrawer.tsx` still owns the DECISION of which skin to render
 * (desktop pill/dialog vs this), and still owns the machine-stats DATA
 * (`useMachineStatsContent`, shared with the desktop dialog so the two
 * skins never duplicate that logic — see that hook's own doc).
 *
 * **Layout:** `.phone-drawer` is a `position:fixed` column pinned to the
 * viewport bottom. Its LAST child, `.phone-drawer__bar` (handle + two tab
 * buttons), has a FIXED height and is always rendered — the "mainstay".
 * Its FIRST child, `.phone-drawer__body`, is ALWAYS MOUNTED too (revised
 * #2107/#1833 "animate the slide" packet — see below); only its CONTENT
 * (the active tab's panel) is conditionally rendered while `open`. Its
 * height is the live-dragged `openPct` (vh); because the container is
 * bottom-anchored and the bar's height never changes, the body simply
 * grows the whole column upward as it opens/drags — no separate
 * "collapsed vs full" class swap the way the old single-sheet chrome
 * needed.
 *
 * **The open/close slide (operator finding, phone install review):** a
 * conditionally-MOUNTED body (`{open && <div>...}`, the original #2107
 * shape) can never play an exit transition — React removes it from the DOM
 * the instant `open` flips, before any CSS transition gets a frame to
 * animate. The body is now always in the DOM; `styles.css`'s
 * `.phone-drawer__body` base rule sits at `transform: translateY(100%)`
 * (fully below the viewport, `visibility: hidden`), and
 * `.phone-drawer--open .phone-drawer__body` slides it to
 * `translateY(0)` over ~220ms ease-out — a real CSS `transition` on a
 * persistent element, not a keyframe replayed on every mount. Dragging
 * disables the transition via the `.phone-drawer--dragging` class (so the
 * sheet tracks the finger with zero lag) and it's re-enabled the instant
 * the pointer releases. `visibility`'s own transition is asymmetric on
 * purpose — instant on open (so the slide-up is visible from frame one),
 * DELAYED on close (so the sheet stays visible for the full 220ms slide-
 * down instead of vanishing first) — see the CSS rule's own comment.
 * `@media (prefers-reduced-motion: reduce)` already existed for this
 * class's height-only transition and now covers the transform too.
 *
 * **The body's own content stays gated on `open`** (`{open &&
 * (activeTab === "machine" ? ... : <EventLogColumn/>)}`) even though the
 * wrapper is always mounted — this is what keeps `EventLogColumn` (and the
 * machine stats panel's own daemon polling, gated separately via
 * `onMachineOpenChange` below) from rendering/fetching while the sheet is
 * closed and merely sliding off past the edge of the viewport.
 *
 * **Modal while open, at ANY height (operator finding, same review):** the
 * page behind is unusable while the drawer is open — body scroll is locked
 * for the drawer's ENTIRE open lifetime now, not just "past ~50%" the way
 * the original spec had it, and a transparent full-viewport
 * `.phone-drawer__backdrop` (no visible dimming — the sheet itself already
 * reads as the foreground) sits behind the drawer and in front of the
 * page, so a tap anywhere outside the sheet DISMISSES it instead of
 * reaching whatever button/link happened to be under it.
 *
 * **Tap/drag semantics on the handle** (spec, #2107 packet brief):
 * - tapping a CLOSED tab opens the drawer to that tab, at its own
 *   remembered height (`lib/drawerStorage.ts`, default `DEFAULT_OPEN_PCT`
 *   when nothing is stored yet).
 * - tapping the ACTIVE tab while open closes the drawer (its height stays
 *   remembered for next time — nothing is persisted on a mere close).
 * - tapping the OTHER tab while open switches to it, snapping the sheet to
 *   THAT tab's own remembered height (not the current tab's).
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

/** vh used the first time a tab is ever opened (nothing stored yet) — a
 * sensible half-sheet, matching common bottom-sheet defaults. */
const DEFAULT_OPEN_PCT = 50;
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
  const [openPct, setOpenPct] = useState<number>(
    () => loadDrawerHeightPct("machine") ?? DEFAULT_OPEN_PCT,
  );
  const [dragging, setDragging] = useState(false);
  const dragRef = useRef<{
    startY: number;
    startPct: number;
    startOpen: boolean;
    moved: boolean;
  } | null>(null);

  function close() {
    setOpen(false);
  }

  function openTab(tab: DrawerTabId) {
    setActiveTab(tab);
    setOpenPct(loadDrawerHeightPct(tab) ?? DEFAULT_OPEN_PCT);
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
    setActiveTab(tab);
    setOpenPct(loadDrawerHeightPct(tab) ?? DEFAULT_OPEN_PCT);
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
  useEffect(() => {
    if (!open) return undefined;
    const prevOverflow = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.body.style.overflow = prevOverflow;
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
      saveDrawerHeightPct(activeTab, pct);
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
          className="phone-drawer__backdrop"
          data-act="phone-drawer-backdrop"
          aria-hidden="true"
          onClick={close}
        />
      )}
      <div
        className={`phone-drawer${open ? " phone-drawer--open" : ""}${dragging ? " phone-drawer--dragging" : ""}`}
        data-act="phone-drawer"
      >
        {/* Always mounted (never conditionally unmounted) so the CSS
            `transform`/`visibility` transition on `.phone-drawer--open
            .phone-drawer__body` has a persistent element to animate — see
            this file's own doc on why a conditionally-mounted body can
            never play an exit transition. The CONTENT inside stays gated
            on `open` so nothing renders/polls/fetches while the sheet is
            merely slid off past the viewport edge. */}
        <div
          className="phone-drawer__body"
          data-act="phone-drawer-body"
          style={{ height: `${openPct}vh` }}
          role={open ? "dialog" : undefined}
          aria-modal={open ? true : undefined}
          aria-hidden={!open}
          aria-label={activeTab === "machine" ? "Machine stats" : "Events"}
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
              <EventLogColumn
                paneId="phone-drawer"
                scopeLabel={events.scopeLabel}
                records={events.records}
                visible={events.visible}
                loading={events.loading}
                error={events.error}
                historical={events.historical}
                serverTruncated={events.serverTruncated}
                pushDetail
              />
            ))}
        </div>
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
              aria-label="Machine stats"
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
      </div>
    </>
  );
}
