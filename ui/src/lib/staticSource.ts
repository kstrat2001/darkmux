/**
 * The ONE place that answers "is this a daemon-less static build, and if so
 * where does it read from" (#1801). `darkmux.com/demo` is `viewer.html` in
 * playback mode with NO daemon behind it — `scripts/build-demo.sh` injects
 * `<meta name="darkmux-flow-src" content="./demo-flow.jsonl">` (plus
 * `-runs-src`/`-lab-runs-src`; `-missions-src`/`-phases-src` have no
 * consumer in this port yet — see `RunsBoard.tsx`'s own doc) and legacy
 * reads every one of them at boot (viewer.html:3861/4077/4027).
 *
 * Before this module, the fix for "make the React port buildable as the
 * static demo" would have meant scattering `injectedMeta("darkmux-*-src")`
 * calls across `route.ts`, `useRouteRecords.ts`, `PlaybackLens.tsx`,
 * `RunsBoard.tsx`, and `Masthead.tsx` — five independent readings of the
 * same signal, free to drift the way `injectedMeta.ts`'s own module doc
 * warns about ("two places encode 'which viewer is the real one', and
 * nothing keeps them agreeing" — #1801's own framing). Every consumer below
 * imports from here instead.
 *
 * `isStaticBuild()`/`staticFlowSrc()` read the SAME meta by construction
 * (`staticFlowSrc() !== null` iff `isStaticBuild()`) — two names because
 * call sites want different things: a route decision wants the boolean, a
 * fetch wants the path. `resolveRunsSrc()`/`resolveLabRunsSrc()` fold in
 * their own daemon-default fallback (`/runs`/`/lab/runs`) so a consumer
 * never hardcodes the live path a second time next to the override.
 */
import { injectedMeta } from "./injectedMeta";

/** Is this page a daemon-less static build (the marketing demo, or any
 * future daemon-less harness that injects the same meta)? Mirrors legacy's
 * own gate at viewer.html:3936 (`if(!flowSrc && mode!=="no-daemon")`) —
 * everywhere legacy asks "is a real daemon behind me", this is the modern
 * equivalent for the ONE signal a static build actually sets. */
export function isStaticBuild(): boolean {
  return injectedMeta("darkmux-flow-src") !== null;
}

/** `darkmux-flow-src` — viewer.html:3861. The committed `.jsonl` to read
 * playback records from instead of `/flow/<date>`. Null on every daemon-
 * served page and every test harness (none inject this meta — see
 * `injectedMeta.ts`'s own doc for why that is the REAL, not stand-in,
 * signal). */
export function staticFlowSrc(): string | null {
  return injectedMeta("darkmux-flow-src");
}

/** `darkmux-runs-src` — viewer.html:4077 (`injectedMeta("darkmux-runs-src")
 * || "/runs"`). The runs board reads from this instead of `GET /runs` on a
 * static build; the daemon-default fallback lives HERE so a consumer never
 * repeats the literal `/runs` path next to the override. */
export function resolveRunsSrc(): string {
  return injectedMeta("darkmux-runs-src") ?? "/runs";
}

/** `darkmux-lab-runs-src` — viewer.html:4027 (`injectedMeta("darkmux-lab-runs-src")
 * || "/lab/runs"`). Same shape as `resolveRunsSrc()` above, for the lab-only
 * staffing/bundle extras endpoint. */
export function resolveLabRunsSrc(): string {
  return injectedMeta("darkmux-lab-runs-src") ?? "/lab/runs";
}

/** `darkmux-panels-src` — the committed JSON map of `panelId -> PanelResponse`
 * a daemon-less build serves the console from, in place of `GET /panel/:id`.
 *
 * This exists because the console had no static gate at all, and its fetch
 * DELIBERATELY renders the response body as its error message (the daemon's
 * own 404 text names the allowlist — see `fetchPanel.ts`'s module doc). That
 * is right with a daemon behind the page and catastrophic without one: on
 * GitHub Pages `/panel/:id` is answered by Pages itself, so the console
 * rendered 9,379 bytes of `<!DOCTYPE html>` as command output. */
export function staticPanelsSrc(): string | null {
  return injectedMeta("darkmux-panels-src");
}

/** `darkmux-machine-src` — the committed `{specs, resources}` a daemon-less
 * build shows the machine lens from, in place of `/machine/*`.
 *
 * The machine lens already gates its QUERIES on `daemonBacked`, so unlike the
 * console it never issued a bad fetch — it simply rendered an empty shell with
 * nothing saying why. Host probes (`vm_stat`, `sysctl`, `lms`) can't be
 * derived from records, so a static build needs a fixture or an explanation;
 * absent this meta the lens now says so rather than looking dead. */
export function staticMachineSrc(): string | null {
  return injectedMeta("darkmux-machine-src");
}

/** `darkmux-graphs-src` (#2032 packet 2) — the committed
 * `{"<mission-id>": <graph.json payload>, ...}` map a daemon-less build
 * reads `MissionGraphLens` from, in place of per-mission
 * `GET /mission/:id/graph.json` calls (`scripts/demo-env/export_static.py`
 * captures it from a running demo-world daemon — see that script's own
 * doc). Same shape as `staticMachineSrc`/`staticPanelsSrc` above (a bare
 * `string | null`, no daemon-default fallback) rather than
 * `resolveRunsSrc`'s `?? "/runs"` shape: there is no single daemon route
 * this could fall back to for an ARBITRARY mission id, so a static build
 * with no fixture published has nothing to default to — the lens's own
 * "needs a running daemon" notice is what fills that gap, same as it does
 * today with no fixture at all. */
export function staticGraphsSrc(): string | null {
  return injectedMeta("darkmux-graphs-src");
}
