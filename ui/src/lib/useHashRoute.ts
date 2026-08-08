import { useSyncExternalStore } from "react";
import { parseRoute, type Route } from "./route";

function subscribe(callback: () => void): () => void {
  window.addEventListener("hashchange", callback);
  return () => window.removeEventListener("hashchange", callback);
}

/**
 * Memoized snapshot cache, keyed on the raw `location.href` (hash + search —
 * everything `parseRoute()` reads from). `useSyncExternalStore` REQUIRES its
 * `getSnapshot` to return a referentially stable value when nothing actually
 * changed — `parseRoute()` itself returns a fresh object literal on every
 * call, and passing it straight to `useSyncExternalStore` produced an
 * infinite render loop (React error #185, "Maximum update depth exceeded"),
 * caught live by this packet's Playwright render-sanity proof (a blank
 * `#stage` that never mounted — exactly the failure mode the render-sanity
 * gate exists to catch). This cache is the fix: recompute only when the URL
 * actually moved; otherwise hand back the SAME object reference.
 */
let cachedHref = "";
let cachedRoute: Route = parseRoute();

function getSnapshot(): Route {
  const href = location.href;
  if (href !== cachedHref) {
    cachedHref = href;
    cachedRoute = parseRoute();
  }
  return cachedRoute;
}

/** React binding over `parseRoute()` — re-renders on every `hashchange`.
 * `useSyncExternalStore` (not a `useState` + effect pair) so a hash change
 * that fires between render and effect-commit is never missed, and so
 * server-rendering (not used here, but the primitive is the correct one
 * regardless) has a defined snapshot. */
export function useHashRoute(): Route {
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}
