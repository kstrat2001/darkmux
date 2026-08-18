// The extraction logic itself. Originally shared verbatim by the legacy
// extractor's extract.spec.ts (wrote goldens/) and redprove.spec.ts
// (asserted a blank-page run DIFFERED from every golden) — both retired in
// #1806 along with viewer.html itself. This module lives on: every
// `next-parity*.spec.ts` suite (and `lib/extract-next-lens.js`, its
// next.html-flavored wrapper) imports it verbatim so the port is graded
// with the SAME extraction/normalization logic the frozen `goldens/*.txt`
// were originally captured with — not a hand-written lookalike that could
// quietly diverge and stop meaning anything.

function normalize(raw) {
  return (
    raw
      .replace(/\r\n/g, "\n")
      .split("\n")
      .map((l) => l.replace(/[ \t]+$/g, "")) // trailing whitespace per line
      .join("\n")
      .replace(/\n{3,}/g, "\n\n") // collapse long blank runs
      .trim() + "\n"
  );
}

async function regionText(page, id) {
  const el = page.locator("#" + id);
  if ((await el.count()) === 0) return "";
  return el.innerText();
}

// The masthead's build-identifier chip (`#verbadge`, viewer.html:3824-3828)
// carries the running darkmux VERSION (a semver that changes every release)
// and a short git BUILD HASH (that changes on literally every commit) —
// `"v"+version+(schema?" · schema "+schema:"")+"  ⓘ"`. Capturing that raw
// into a committed golden would make it fail on the very next release; this
// is the deliberate fix (chosen over the alternative of just EXCLUDING the
// region — see extractTopbarText's own doc for why): normalize the whole
// populated chip to one fixed, stable placeholder BEFORE it's joined into
// the extracted text, the same "normalize the volatile substring, don't
// pretend it isn't there" discipline `normalize()` above already applies to
// whitespace. Matches ANY version string (not just today's scheme) and any
// schema value, so a future version-string format change doesn't silently
// stop matching and re-introduce the flake.
const VERBADGE_RE = /v\S+(?: \([^)]*\))?(?: · schema \S+)?  ⓘ/g;
// NOTE (QA gate): this normalization is UNREACHABLE in the current harness,
// and the honest reason is worth recording rather than leaving the reader to
// discover it. Goldens are extracted from a statically-served page whose
// `buildHarness()` injects only `darkmux-mode` — never `darkmux-version` — so
// the version chip renders EMPTY in every golden and this regex never matches
// anything. Mutating it to an identity function leaves `verify` and every
// next-parity suite green, which was verified.
//
// It stays because the moment a harness DOES inject version metas, the raw
// chip would make the golden fail on the next release. But do not read it as
// an active guard today: it is future-proofing, not coverage.
function normalizeVerbadge(text) {
  return text.replace(VERBADGE_RE, "v<version>  ⓘ");
}

/**
 * The masthead (`.top`, viewer.html:802-816) — brand, the build-identifier
 * chip, the playback-catalog trigger (`#srcbadge`), the live/mode badge
 * (`#modebadge`), the manual-refetch control, and the topnav links. Global
 * chrome, a body-level sibling of `#stage` — structurally invisible to the
 * four-region `extractLensText` below, same class of gap `extractCatalogText`
 * (Packet 4) closed for `#catpanel`. This is what caught the operator-visible
 * bug this packet fixes: a region entirely outside the four captured ones is
 * NOT just "untested", it's LITERALLY UNOBSERVABLE by this harness — the
 * masthead could go missing (or grow a stray element) and every existing
 * golden would stay byte-identical.
 *
 * Unlike `extractCatalogText`, this is folded directly INTO
 * `extractLensText` below (not left as an opt-in fifth region) — the
 * masthead renders identically on every lens (verified: `.top`'s content
 * doesn't depend on `state.level`), so every existing golden gains this
 * section on rebaseline rather than one dedicated golden growing it. See
 * `normalizeVerbadge` above for how the one volatile piece (`#verbadge`) is
 * handled — normalized in place, not excluded, so a real regression INSIDE
 * the volatile chip (e.g. the `ⓘ` affordance silently disappearing) still
 * shows up as a golden diff instead of being invisible twice over.
 */
async function extractTopbarText(page) {
  const el = page.locator(".top");
  if ((await el.count()) === 0) return `=== topbar ===\n(empty)\n\n`;
  const raw = await el.innerText();
  return `=== topbar ===\n${normalize(normalizeVerbadge(raw) || "(empty)")}\n`;
}

/**
 * The "main content region" for a lens: the topbar (masthead — see
 * `extractTopbarText`'s own doc for why this joined in directly rather than
 * staying a separate opt-in region), the crumb (breadcrumb/level label),
 * the meta line, the event-log scope label, and the stage (every render*()
 * function's actual output target — see viewer.html, every renderX() writes
 * `$("stage").innerHTML=...`). See README.md for why these regions and not
 * the full scrolling event log. `=== stage ===` STAYS LAST — `stageSectionOf`
 * (`lib/extract-next-lens.js`) slices a golden from that marker to end-of-
 * string, so any section added here must go BEFORE it, never after.
 */
async function extractLensText(page) {
  const [topbar, crumb, meta, logscope, stage] = await Promise.all([
    extractTopbarText(page),
    regionText(page, "crumb"),
    regionText(page, "meta"),
    regionText(page, "logscope"),
    regionText(page, "stage"),
  ]);
  return (
    topbar +
    `=== crumb ===\n${normalize(crumb || "(empty)")}\n` +
    `=== meta ===\n${normalize(meta || "(empty)")}\n` +
    `=== logscope ===\n${normalize(logscope || "(empty)")}\n` +
    `=== stage ===\n${normalize(stage || "(empty)")}\n`
  );
}

/**
 * Packet 4 addition (ADDITIVE ONLY — `extractLensText`'s signature and
 * output are untouched above, so every existing golden this function's
 * sibling produces stays byte-identical; see the module doc's opening
 * paragraph). `#catpanel` (the playback-catalog day/mission picker, #691) is
 * a MODAL OVERLAY — a body-level sibling of `#stage`, not part of it — so
 * the four-region `extractLensText` structurally cannot see it (the README's
 * own "KNOWN COVERAGE GAPS" named this explicitly: "the extractor doesn't
 * capture it at all; it's a modal overlay, not part of #stage"). This closes
 * that gap as a FIFTH, OPTIONAL region rather than widening the four-region
 * function, so callers that don't need it (every existing test) are
 * unaffected.
 */
async function extractCatalogText(page) {
  const catalog = await regionText(page, "catpanel");
  return `=== catalog ===\n${normalize(catalog || "(empty)")}\n`;
}

/** `extractLensText` (unchanged) + the catalog region appended — for the ONE
 * golden that needs both (the catalog panel opened over a lens's normal
 * render). Composing rather than modifying `extractLensText` itself is what
 * keeps this additive. */
async function extractLensTextWithCatalog(page) {
  const base = await extractLensText(page);
  const catalog = await extractCatalogText(page);
  return base + catalog;
}

// QA finding (post-0a review): the original `waitSettled` relied on
// `page.waitForLoadState('networkidle')`, which samples network state at the
// moment it's called — for a fetch a CLICK just kicked off asynchronously
// (goRuns/goMachine's "paint a loading placeholder, await fetch, re-render"
// pattern), there's a real race where networkidle resolves before the new
// request has even started, and the extraction commits the "loading…"
// placeholder as if it were real content. QA proved this by injecting 1.2s
// of latency on `/runs`: the resulting golden was just "runs / loading…"
// and the whole check suite stayed green.
//
// The fix: every navigation names an explicit POST-FETCH CONTENT SELECTOR —
// a CSS class that ONLY appears once the lens's own render*() function has
// real data (never in that lens's own "loading…"/"running…" placeholder
// branch; see the per-lens comments in extract.spec.ts/redprove.spec.ts for
// which selector and why). `expect(...).toBeAttached()` RETRIES until that
// selector exists (or times out and fails loudly), which is immune to the
// networkidle race because it doesn't care about network timing at all — it
// polls the actual DOM for the actual evidence of completion.
//
// Belt-and-suspenders: when `previousText` is passed, this ALSO asserts the
// stage text differs from what it was before the navigation — catches the
// same failure class for any FUTURE lens whose content selector policy
// turns out to be imprecise (e.g. a marker that's also present, empty, in
// the loading state).
// `contentSelector` may be a CSS string OR a pre-built Locator/Locator-union
// (e.g. `locA.or(locB)`, needed where no single CSS selector cleanly
// distinguishes "settled" from "loading" — see redprove.spec.ts's machine
// lens, whose settled-but-daemon-unreachable state shares the SAME `.none`
// class the loading placeholder uses, so the two have to be told apart by
// text content, which CSS alone can't express).
async function waitSettled(page, expect, contentSelector, opts = {}) {
  const { timeout = 15000, previousText = null } = opts;
  if (!contentSelector) {
    throw new Error("waitSettled requires an explicit post-fetch content selector — see the module doc for why 'networkidle' alone is not enough");
  }
  const locator = typeof contentSelector === "string" ? page.locator(contentSelector).first() : contentSelector.first();
  await expect(locator, `post-fetch content marker "${contentSelector}" never appeared`).toBeAttached({ timeout });
  if (previousText !== null) {
    await expect(async () => {
      const now = await regionText(page, "stage");
      if (now === previousText) throw new Error("stage text unchanged from before navigation — render may not have actually happened");
    }, "stage text should differ from its pre-navigation snapshot").toPass({ timeout });
  }
}

// What the goldens actually need is a fixed `Date.now()` — so a rendered
// "3m ago" or an absolute timestamp is the same on every run. They do NOT
// need timers stopped, and stopping them is actively harmful.
//
// History, because this was gotten wrong twice in opposite directions:
//
//  1. `page.clock.install({ time })` alone sets an INITIAL time but lets
//     timers keep running in real wall-clock time (QA measured a 1506ms
//     delta after a 1500ms wait) — so the clock was never frozen and the
//     goldens were only accidentally stable.
//  2. Adding `pauseAt()` did freeze it, and froze the TIMERS with it. React
//     18 schedules non-urgent updates through the timer queue, so a state
//     update that lands after the pause is never flushed: the fetch
//     resolves, the data is in the cache, and the component renders
//     `loading…` forever. This passed locally (the update won the race on a
//     fast machine, ~1.6s for the whole suite) and failed on CI's slower
//     Linux runner, where the same suite took 1.1m and four machine-lens
//     assertions timed out. The traces showed all seven endpoint requests
//     returning 200 — the data arrived; only the render never happened.
//
// `setFixedTime` is the option that matches the actual requirement: pin what
// the page READS as now, leave the event loop alone.
async function installFrozenClock(page, ms) {
  await page.clock.setFixedTime(ms);
}

// The other clock need, kept SEPARATE on purpose.
//
// `installFrozenClock` above pins what the page reads as `now` and leaves the
// event loop alone — right for golden extraction, which wants a stable
// timestamp and a live scheduler. But it is `setFixedTime`, which does not
// install a controllable clock: `runFor`, `fastForward` and `setSystemTime`
// all require `install()`, and silently do nothing useful without it.
//
// A test that must SIMULATE time passing — crossing UTC midnight, say — needs
// the controllable clock instead, and must then PUMP it (`runFor`) rather
// than waiting on wall time. Pumping is what flushes React's scheduler here,
// so such a test does not hit the render-wedge that made `pauseAt` wrong as a
// global default.
async function installControllableClock(page, ms) {
  await page.clock.install({ time: ms });
  await page.clock.pauseAt(ms);
}

module.exports = {
  normalize,
  regionText,
  extractLensText,
  extractCatalogText,
  extractLensTextWithCatalog,
  waitSettled,
  installFrozenClock,
  installControllableClock,
};
