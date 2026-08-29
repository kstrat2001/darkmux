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
 */
export const MOBILE_BREAKPOINT = 768;

export function useIsMobile(breakpoint: number = MOBILE_BREAKPOINT): boolean {
  const [isMobile, setIsMobile] = useState(() => (typeof window !== "undefined" ? window.innerWidth <= breakpoint : false));
  useEffect(() => {
    const onResize = () => setIsMobile(window.innerWidth <= breakpoint);
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, [breakpoint]);
  return isMobile;
}
