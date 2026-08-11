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

/** `missionGraphReachable()` — viewer.html:2732. `/mission/<id>/graph` is a
 * SEPARATE document (the vendored React Flow mission-graph page) that only
 * exists behind a real daemon (`darkmux-mode` present) serving a genuinely
 * live/playback page (`darkmux-flow-src` ABSENT — that meta marks a
 * daemon-less static build, e.g. the GitHub Pages demo, which has no
 * `/mission/<id>/graph` route to navigate to at all). */
export function missionGraphReachable(): boolean {
  return !!injectedMeta("darkmux-mode") && !injectedMeta("darkmux-flow-src");
}
