import { useLayoutEffect, useRef, type RefObject } from "react";

/** How long a moved or newly appearing item takes to settle. */
export const FLIP_MS = 200;
const FLIP_EASING = "ease-out";

interface Spot {
  left: number;
  top: number;
}

/** Whether the viewer asked for no motion. `matchMedia` is absent in some
 *  test and embedded environments, where motion is simply not asked for. */
function reducedMotion(): boolean {
  return typeof window !== "undefined" && typeof window.matchMedia === "function" && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

/** Where `el` sits INSIDE `container`. `offsetLeft`/`offsetTop` are relative to
 *  the nearest positioned ancestor, which is usually far above the container,
 *  so the container's own offset is taken out: when something above the
 *  container changes height (a hero settling, the cards appearing above the
 *  lanes) every item's offset changes together, and that is not a move of the
 *  item, so it must not animate. */
function spotIn(container: HTMLElement, el: HTMLElement): Spot {
  if (el.offsetParent === container) return { left: el.offsetLeft, top: el.offsetTop };
  return { left: el.offsetLeft - container.offsetLeft, top: el.offsetTop - container.offsetTop };
}

/**
 * FLIP (first, last, invert, play) for a list of items that can change order:
 * every element under `containerRef` carrying `data-flip-key` is measured
 * after each render, and one whose place changed since the last render
 * animates from its old place to its new one over `FLIP_MS`, ease-out. An
 * item whose key is new fades in instead of popping. Items are identified by
 * their `data-flip-key` (the machine's stable identity), never by position.
 *
 * Positions are `offsetLeft`/`offsetTop` inside the container (`spotIn`), which ignore transforms, so an
 * animation already running does not read as a move and is not cut short by
 * the next render (live samples re-render the fleet lens several times a
 * second). The animation is the Web Animations API's, which removes its own
 * transform when it ends: nothing is left on the element to clear. No motion
 * at all under `prefers-reduced-motion`, and none on the first layout or when
 * the container itself resized (a window resize is not a reorder).
 */
export function useFlip(containerRef: RefObject<HTMLElement | null>): void {
  const seen = useRef<{ spots: Map<string, Spot>; width: number } | null>(null);
  useLayoutEffect(() => {
    const container = containerRef.current;
    if (!container) {
      seen.current = null;
      return;
    }
    const items = [...container.querySelectorAll<HTMLElement>("[data-flip-key]")];
    const spots = new Map<string, Spot>();
    for (const el of items) spots.set(el.dataset.flipKey as string, spotIn(container, el));
    const width = container.offsetWidth;
    const before = seen.current;
    seen.current = { spots, width };
    if (!before || before.width !== width || reducedMotion()) return;
    for (const el of items) {
      if (typeof el.animate !== "function") return;
      const now = spots.get(el.dataset.flipKey as string) as Spot;
      const was = before.spots.get(el.dataset.flipKey as string);
      if (!was) {
        if (before.spots.size > 0) el.animate([{ opacity: 0 }, { opacity: 1 }], { duration: FLIP_MS, easing: FLIP_EASING });
        continue;
      }
      const dx = was.left - now.left;
      const dy = was.top - now.top;
      if (dx === 0 && dy === 0) continue;
      el.animate([{ transform: `translate(${dx}px, ${dy}px)` }, { transform: "translate(0, 0)" }], { duration: FLIP_MS, easing: FLIP_EASING });
    }
  });
}
