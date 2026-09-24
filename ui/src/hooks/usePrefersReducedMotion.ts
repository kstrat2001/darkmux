import { useEffect, useState } from "react";

/**
 * (#2878) The one place a JS-driven animation (a tween that CSS alone can't
 * express — see `useCountUp.ts`) checks `prefers-reduced-motion`. CSS-only
 * motion (the gauge/battery/context-bar `transition`s, the arrival slide,
 * the turn-header beat) reads the media query directly in `styles.css`
 * (the existing `--beat: none` convention); this hook exists ONLY for the
 * handful of callers that drive a value from `requestAnimationFrame`
 * instead, where there is no CSS property to gate.
 *
 * Same guard shape as `useIsMobile.ts`'s `hasCoarsePointer` — jsdom has no
 * `matchMedia` at all, so every existing test (which never stubs it) keeps
 * getting `false` (motion allowed), same as a real browser with no
 * preference set.
 */
function query(): boolean {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return false;
  try {
    return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  } catch {
    return false;
  }
}

export function usePrefersReducedMotion(): boolean {
  const [reduced, setReduced] = useState(query);
  useEffect(() => {
    if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
    let mql: MediaQueryList;
    try {
      mql = window.matchMedia("(prefers-reduced-motion: reduce)");
    } catch {
      return;
    }
    const onChange = () => setReduced(mql.matches);
    // (Safari < 14 only has the deprecated addListener/removeListener pair;
    // every engine this app targets today has addEventListener, but the
    // optional-chaining fallback costs nothing.)
    mql.addEventListener ? mql.addEventListener("change", onChange) : mql.addListener?.(onChange);
    return () => {
      mql.removeEventListener ? mql.removeEventListener("change", onChange) : mql.removeListener?.(onChange);
    };
  }, []);
  return reduced;
}
