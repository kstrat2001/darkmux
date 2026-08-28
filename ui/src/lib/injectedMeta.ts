import { getSource } from "./source";
/**
 * `injectedMeta()` — viewer.html:3808-3811. Both `GET /` and `GET /next`
 * inject `darkmux-version`/`darkmux-flow-schema`/`darkmux-mode`/
 * `darkmux-flow-src` metas via the SAME `inject_mode_meta()`
 * (`crates/darkmux-serve/src/lib.rs`), so reading them here is the real,
 * live mechanism — not a stand-in. Every automated harness this repo has
 * (`tests/parity/playwright.config.js`'s `buildHarness()`,
 * `tests/parity/next-*.playwright.config.js`'s "no meta-injection here" —
 * see that file's own comment) serves the page WITHOUT these metas, so
 * `injectedMeta` returns `null` in every test run today — matching legacy's
 * own `if(vb&&verMeta)`-style gates. Only a REAL daemon (`darkmux serve`)
 * populates it.
 *
 * Extracted from `Masthead.tsx` (its original, still its only pre-drill-in
 * consumer) into this shared module so `RunsBoard.tsx`'s mission-graph
 * reachability check (`missionGraphReachable()`, viewer.html:2732) can read
 * the SAME live mechanism rather than a second, drifting copy.
 */
export function injectedMeta(name: string): string | null {
  const el = document.querySelector(`meta[name="${name}"]`);
  return el ? el.getAttribute("content") : null;
}

/** `missionGraphReachable()` — viewer.html:2732. The predicate outlived its
 * original reason and keeps a NEW one (#1868 third packet): it used to mean
 * "is there a separate mission-graph document to navigate to", back when
 * `/mission/<id>/graph` served a standalone page with its own vendored
 * React Flow bundle. That page is retired and the graph is now a lens
 * inside this app (`ui/src/lenses/mission/`), so navigation is never the
 * question any more.
 *
 * What it means NOW: the lens needs a live daemon to answer
 * `/mission/<id>/graph.json`. `darkmux-mode` present + `darkmux-flow-src`
 * ABSENT is exactly "a real daemon serving a live/playback page"; the meta
 * being present marks a daemon-less static build (the GitHub Pages demo),
 * which has no graph data at all. Do NOT delete this gate on the reasoning
 * that the separate page is gone: without it the demo build renders a
 * permanently-loading mission lens instead of the honest
 * `MISSION_GRAPH_UNREACHABLE_NOTICE` (`RunsBoard.tsx`). */
export function missionGraphReachable(): boolean {
  // (#2065) A static build that ships a committed graphs file
  // (`darkmux-graphs-src`, #2032 packet 2) CAN answer the lens for the
  // missions that file lists — the demo's mission row must open its MAP
  // rather than the daemon-less notice. A mission absent from the file
  // still gets the lens's own honest notice, so this predicate need not
  // know which ids it holds.
  const s = getSource();
  if (s.graphs !== null) return true;
  return !!injectedMeta("darkmux-mode") && s.kind === "daemon";
}

/** `/play/<date>`'s injected date, when the server actually served a PLAYBACK
 * page — `inject_mode_meta(html, "play", Some(date))`
 * (`crates/darkmux-serve/src/lib.rs`). Returns null on a live page, on a
 * daemon-less static build, and in every harness (none inject metas).
 *
 * BOTH metas are required, not just the date: `darkmux-mode` is what says the
 * server meant playback. A date without that mode would be a page this app
 * should not reinterpret, and reading the date alone would make the routing
 * decision on weaker evidence than the server actually gave.
 *
 * The format check is deliberate and not paranoia about the server — it is
 * the same `DATE_RE` shape `parseRoute` demands of a hash or `?date=`, so all
 * three sources of a playback date are held to one standard. A value that
 * fails it falls through to the live fleet view, exactly as a garbage hash
 * does. */
export function injectedPlaybackDate(): string | null {
  if (injectedMeta("darkmux-mode") !== "play") return null;
  const date = injectedMeta("darkmux-date");
  return date && /^\d{4}-\d{2}-\d{2}$/.test(date) ? date : null;
}
