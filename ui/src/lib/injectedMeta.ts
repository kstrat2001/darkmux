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
