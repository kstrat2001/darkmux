import { useEffect, useState } from "react";

/**
 * The phone/desktop chrome breakpoint — the SAME 768px `styles.css`'s
 * `.machine-drawer`/`.phone-drawer` media queries use, so JS-rendered
 * markup and CSS always agree on where the line is.
 *
 * Extracted from `MachineDrawer.tsx` (#2107 tabbed-drawer packet) so
 * `App.tsx` can make the SAME phone/desktop call `MachineDrawer` already
 * makes — it needs to know which skin is active to decide whether the
 * App-level inline `EventLogColumn` mount or the phone drawer's own owns
 * the events pane (see `App.tsx`'s own doc for why there can only ever be
 * one at a time). Two independent `window.innerWidth` listeners (one per
 * call site) rather than a single shared subscription: cheap, and it keeps
 * each caller self-contained/testable via its own `isMobileOverride`
 * rather than threading a shared value through props no test needs to
 * fake.
 *
 * **Landscape-phone fallback (#2108 review finding 10, WebKit-proven).** A
 * phone rotated to landscape (an iPhone 14: 844×390) is WIDER than 768 —
 * plain width comparison alone flipped `MachineDrawer` from `PhoneDrawer`'s
 * sheet to the desktop `<Dialog>` skin mid-rotation, UNMOUNTING the
 * uncontrolled open/activeTab state `PhoneDrawer` deliberately owns
 * internally (see that component's own doc on why it stays uncontrolled
 * rather than lifted). Of the two fixes the review named — keep phone
 * chrome for coarse-pointer devices regardless of width, or preserve the
 * sheet's state across the flip — this is the SIMPLER one: preserving
 * state would mean lifting `PhoneDrawer`'s state against that documented
 * design call, and threading it through a remount boundary; this instead
 * widens the ONE shared "am I phone chrome" test both `App.tsx` and
 * `MachineDrawer.tsx` already call through, so the sheet simply never
 * unmounts in the first place.
 *
 * **Touch detection: `matchMedia('(pointer: coarse)')`, not
 * `navigator.maxTouchPoints` or `'ontouchstart' in window`** — both of the
 * more obvious options were tried and measured wrong against a REAL
 * WebKit run (Playwright, `devices['iPhone 14']` touch emulation, the
 * review's own required proof engine): WebKit's touch emulation never
 * sets `navigator.maxTouchPoints` above `0` (Chromium's does; WebKit's
 * doesn't — an engine difference, not a bug in either), and jsdom (this
 * repo's vitest environment) defines `'ontouchstart' in window` as `true`
 * UNCONDITIONALLY, even with no touch emulation at all — either choice
 * either misses real WebKit phones or wrongly flags every desktop test as
 * mobile. `matchMedia('(pointer: coarse)').matches` measured correctly on
 * BOTH engines' touch emulation (`true`) and on a genuine no-touch desktop
 * context (`false`) — it is the one signal that actually tracks "coarse
 * pointer" rather than an engine's incidental touch-API surface. jsdom has
 * no `matchMedia` implementation at all (calling it throws
 * "is not a function") — guarded by `typeof window.matchMedia ===
 * "function"`, so every existing jsdom-based test (which never stubs
 * `matchMedia`) keeps behaving exactly as before: `touch` reads `false`,
 * and this whole branch is a no-op, same as the `navigator.maxTouchPoints`
 * version was BY COINCIDENCE (`undefined > 0` also evaluates `false`) —
 * the difference is this one is deliberate, not an accident of jsdom's
 * `undefined` default. Capped at `1024px` so a genuine touch-capable
 * desktop/tablet window in landscape never gets caught by this branch —
 * only a device both coarse-pointer AND narrow-when-rotated qualifies.
 */
export const MOBILE_BREAKPOINT = 768;
const LANDSCAPE_PHONE_MAX_WIDTH = 1024;

function hasCoarsePointer(): boolean {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return false;
  try {
    return window.matchMedia("(pointer: coarse)").matches;
  } catch {
    return false;
  }
}

function computeIsMobile(breakpoint: number): boolean {
  if (typeof window === "undefined") return false;
  if (window.innerWidth <= breakpoint) return true;
  const landscape = window.innerWidth > window.innerHeight;
  return hasCoarsePointer() && landscape && window.innerWidth <= LANDSCAPE_PHONE_MAX_WIDTH;
}

export function useIsMobile(breakpoint: number = MOBILE_BREAKPOINT): boolean {
  const [isMobile, setIsMobile] = useState(() => computeIsMobile(breakpoint));
  useEffect(() => {
    const onResize = () => setIsMobile(computeIsMobile(breakpoint));
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, [breakpoint]);
  return isMobile;
}
