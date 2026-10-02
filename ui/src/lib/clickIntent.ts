/**
 * "Did this click end a text selection or a drag?" (5.0 operator bug: selecting
 * text on a fleet card clicked through the card's link.)
 *
 * A drag that selects text ends in `mouseup`, and the browser then fires
 * `click` on the common ancestor, so a clickable container that holds readable
 * text navigates on a gesture the operator never meant as a click. Every such
 * container routes its click handler through `onIntentClick`, which swallows
 * the click when it ended a selection or a drag. Keyboard activation
 * (Enter/Space) never produces a click event here and is unaffected.
 */

/** Pointer travel (px) between pointerdown and click beyond which it was a drag. */
export const DRAG_THRESHOLD_PX = 4;

interface ClickLike {
  currentTarget: EventTarget | null;
  clientX: number;
  clientY: number;
  detail: number;
}

let downAt: { x: number; y: number } | null = null;

if (typeof document !== "undefined") {
  // Capture phase, so a descendant's stopPropagation cannot hide the press.
  document.addEventListener(
    "pointerdown",
    (e) => {
      downAt = { x: (e as MouseEvent).clientX, y: (e as MouseEvent).clientY };
    },
    true,
  );
}

/** A non-collapsed selection that touches `el`. */
function selectionTouches(el: Element): boolean {
  const sel = typeof window !== "undefined" ? window.getSelection() : null;
  if (!sel || sel.rangeCount === 0 || sel.isCollapsed) return false;
  for (let i = 0; i < sel.rangeCount; i++) {
    if (sel.getRangeAt(i).intersectsNode(el)) return true;
  }
  return false;
}

function dragged(e: ClickLike): boolean {
  // detail 0 is a keyboard- or script-synthesized click: no pointer press
  // preceded it, so a remembered pointerdown belongs to some earlier gesture.
  if (!downAt || e.detail === 0) return false;
  return Math.hypot(e.clientX - downAt.x, e.clientY - downAt.y) > DRAG_THRESHOLD_PX;
}

/**
 * True when `e` is the tail of a text selection (a non-collapsed selection
 * touching the clicked element), of a drag (pointer moved past the threshold
 * since pointerdown), or the second click of a double-click (word select).
 */
export function endedSelectionOrDrag(e: ClickLike): boolean {
  const el = e.currentTarget instanceof Element ? e.currentTarget : null;
  if (el && selectionTouches(el)) return true;
  if (dragged(e)) return true;
  return e.detail >= 2;
}

/** Wrap a click handler so a selection- or drag-ending click does nothing. */
export function onIntentClick<E extends ClickLike>(handler: (e: E) => void): (e: E) => void {
  return (e) => {
    if (endedSelectionOrDrag(e)) return;
    handler(e);
  };
}
