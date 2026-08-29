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
 * Its FIRST child, `.phone-drawer__body`, only renders while `open` is
 * true and its height is the live-dragged `openPct` (vh); because the
 * container is bottom-anchored and the bar's height never changes, the
 * body simply grows the whole column upward as it opens/drags — no
 * separate "collapsed vs full" class swap the way the old single-sheet
 * chrome needed.
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
 * - Escape, and a click on the backdrop, close it too.
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
import { loadDrawerHeightPct, saveDrawerHeightPct, type DrawerTabId } from "../lib/drawerStorage";

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
/** "past ~50%" (spec) — the vh height at/above which the page behind the
 * drawer stops scrolling. */
const SCROLL_LOCK_PCT = 50;

function clampPct(pct: number): number {
  return Math.max(MIN_OPEN_PCT, Math.min(MAX_OPEN_PCT, pct));
}

export interface PhoneDrawerMachineTab {
  /** `CPU 34 · GPU 68 · MEM 62` — shown on the closed/open Machine tab
   * button itself (the tab IS its own label; see this file's own doc on
   * why Machine carries no separate word the way Events does). */
  compactLine: string;
  /** identity + scope + meters/idle + about — `useMachineStatsContent`'s
   * `body`, unchanged, reused rather than re-derived. */
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
}: {
  machineTab: PhoneDrawerMachineTab;
  events: PhoneDrawerEventsTab;
  liveStatus: LiveTailStatus;
  route: Route;
}) {
  const [open, setOpen] = useState(false);
  const [activeTab, setActiveTab] = useState<DrawerTabId>("machine");
  const [openPct, setOpenPct] = useState<number>(() => loadDrawerHeightPct("machine") ?? DEFAULT_OPEN_PCT);
  const [dragging, setDragging] = useState(false);
  const dragRef = useRef<{ startY: number; startPct: number; startOpen: boolean; moved: boolean } | null>(null);

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

  // (spec) "the page behind does not scroll while the drawer is past ~50%".
  useEffect(() => {
    if (!open || openPct < SCROLL_LOCK_PCT) {
      return undefined;
    }
    const prevOverflow = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.body.style.overflow = prevOverflow;
    };
  }, [open, openPct]);

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
    dragRef.current = { startY: e.clientY, startPct: open ? openPct : 0, startOpen: open, moved: false };
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

  const dotState: "live" | "reconnecting" | "replay" = !isLiveRoute(route) ? "replay" : liveStatus === "live" ? "live" : "reconnecting";

  return (
    <div className={`phone-drawer${open ? " phone-drawer--open" : ""}`} data-act="phone-drawer">
      {open && (
        <div
          className="phone-drawer__body"
          data-act="phone-drawer-body"
          style={{ height: `${openPct}vh`, transition: dragging ? "none" : undefined }}
          role="dialog"
          aria-modal="true"
          aria-label={activeTab === "machine" ? "Machine stats" : "Events"}
        >
          {activeTab === "machine" ? (
            <div className="phone-drawer__panel" data-act="phone-drawer-panel-machine">
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
          )}
        </div>
      )}
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
            <span className="phone-drawer__tabvalue">{machineTab.compactLine}</span>
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
              <span className={`phone-drawer__dot phone-drawer__dot--${dotState}`} aria-hidden="true" />
            </span>
          </button>
        </div>
      </div>
    </div>
  );
}
