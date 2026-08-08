// The extraction logic itself — shared verbatim by extract.spec.ts (writes
// the result to goldens/) and redprove.spec.ts (compares the result against
// the existing golden and asserts they DIFFER). Sharing this module is the
// point: red-prove has to run the SAME extraction, not a hand-written
// lookalike that could quietly diverge from what actually produces the
// goldens and stop meaning anything.

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

/**
 * The "main content region" for a lens: the crumb (breadcrumb/level label),
 * the meta line, the event-log scope label, and the stage (every render*()
 * function's actual output target — see viewer.html, every renderX() writes
 * `$("stage").innerHTML=...`). See README.md for why these four and not the
 * full scrolling event log.
 */
async function extractLensText(page) {
  const [crumb, meta, logscope, stage] = await Promise.all([
    regionText(page, "crumb"),
    regionText(page, "meta"),
    regionText(page, "logscope"),
    regionText(page, "stage"),
  ]);
  return (
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

// QA finding (post-0a review): `page.clock.install({ time })` sets an
// INITIAL time but leaves timers running in real wall-clock time from that
// point — QA measured a 1506ms delta after a 1500ms wait, i.e. the clock
// was never actually frozen. `pauseAt()` is what freezes it (no timer fires
// until `runFor`/`fastForward`/`resume` is explicitly called). Installing
// and immediately pausing at the SAME instant, before navigation, means the
// page never observes real time passing at all.
async function installFrozenClock(page, ms) {
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
};
