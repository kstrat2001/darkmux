// (#856/#857) Regression gate for the activity lane's session-lifecycle → visual
// state mapping. A session whose ONLY terminal is the reconciler's `session.end`
// (abandoned / hard-killed / shipped-without-a-clean-complete) must render as
// ENDED in the RECENT ACTIVITY lane — NOT in-flight stretched to the playhead.
//
// The bug: the lane's `dispatchEnd` recognized only `dispatch.complete`/`error`
// and ignored `session.end`, while `machActive` (the card pill) counted it — the
// two derivations diverged, so an idle machine's bar spanned the whole window
// (the card read "idle" while the bar read "active"). Fixed by routing every
// "is this session done / where does its bar end" decision through the shared
// `sessionCloseEdge` helper.
//
// This is the FIRST test on the viewer's lifecycle render semantics: the engine's
// Rust suite can't reach the inline JS, and the only other JS-level gate is the
// XSS one — so this class of bug had no coverage and kept resurfacing.
//
// The served harness is the canonical viewer in static-playback mode over
// tests/fixtures/lifecycle-flow.jsonl (built in playwright.config.js). The
// playhead initializes to tMax (the late trailing record), so all three
// sessions sit to its left and the bracketing is exercised.
const { test, expect } = require('@playwright/test');

// (port note) Split in two. The activity-lane bracketing assertions below
// (the actual regression this file's own header names — a session.end-only
// session rendering in-flight when it should read ended) are real,
// currently-true `/next` behavior: `FleetLens.tsx`'s `.lane`/`.sbar`
// markup is a direct, unrenamed port of the classes this test already
// checks. The DRILL-IN tail (machine card → expand a collapsible run row
// → click through to the session) is NOT — see the `test.fixme`d original
// below for exactly what's missing and why. Kept as two tests rather than
// one gated test so the regression this file exists to catch stays a
// real, enforced gate instead of going dark behind the unrelated drill-in
// gap.
test('activity lane brackets a session.end-only session as ended, not in-flight', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index-lifecycle.html');
  await page.waitForSelector('.lane .sbar', { timeout: 15_000 });

  // The session that closed ONLY via session.end exists as a bar...
  await expect(
    page.locator('.sbar[title*="sess-ended-via-sessionend"]')
  ).toHaveCount(1);
  // ...and is NOT marked running (class "run"; #1071 renamed the in-flight
  // class from "a"). THE regression: pre-fix it was in-flight.
  await expect(
    page.locator('.sbar.run[title*="sess-ended-via-sessionend"]')
  ).toHaveCount(0);

  // Control: a clean dispatch.complete is also not in-flight.
  await expect(
    page.locator('.sbar.run[title*="sess-clean-complete"]')
  ).toHaveCount(0);

  // Control: a genuinely open session (dispatch.start, NO terminal at all)
  // DOES render in-flight — the fix must not over-close legitimate running work.
  await expect(
    page.locator('.sbar.run[title*="sess-in-flight"]')
  ).toHaveCount(1);

  expect(pageErrors, `viewer threw: ${pageErrors.join('; ')}`).toHaveLength(0);
});

// (port gap, reported not papered over) The drilling half of the original
// test — machine card → expand a collapsible recent-run row → click
// through to the session's own detail view, to prove the fix doesn't
// TypeError on a session.end-only session's undefined close-edge fields.
//
// #1809 (finishing #1508 step 4) changed the SHAPE of this gap, not its
// existence. Pre-#1809 the blocker was that `MachineLens`'s run rows were
// plain non-collapsible `<div>`s with no session-drill affordance at all
// (`runLines.ts`'s own module doc named the missing "open →" link). #1809
// removed that whole per-run list — the machine page now links out to the
// Runs lens (`#lens=runs&machine=<uid>`) instead of rendering rows itself.
// `RunsBoard`'s own rows carry their OWN drill-ins now (`/mission/<id>/graph`
// for a tracked mission/dispatch, an in-page lab-run detail for a lab run —
// see `RunsBoard.tsx`'s `activateRun`), but NEITHER of those is a
// `#session=` drill either. So the gap this test names is unchanged in
// substance: no path from a machine-scoped list reaches a bare session-
// subsystem view, only relocated from one lens to another.
//
// The underlying claim this half exists to protect (a session.end-only
// session's detail view renders without throwing) is still worth
// verifying once that gap closes — `SessionReplay.tsx` already renders
// real content off `runRegions()` for any resolvable session id (see
// `viewer-session-url.spec.js`'s own coverage of that component via a
// direct hash boot), so the remaining risk is specifically whether a
// session.end-only close edge reaches it cleanly. Kept here verbatim
// (fixme, not deleted) as the tracked record — the body below is left
// UNUPDATED to the pre-#1809 selectors deliberately: rewriting it to
// "work" against the new DOM would misrepresent a gap that is still open
// as one that is merely stale, and this fixme was never runnable anyway.
test.fixme('activity lane: drilling a session.end-only session does not throw', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index-lifecycle.html');
  await page.waitForSelector('.lane .sbar', { timeout: 15_000 });

  // (#1508) The nav tab now shares data-act="machine" with the fleet cards
  // (they differ by data-arg — a card carries the machine id, the tab drills
  // the local box). Target a CARD (has data-arg), not the tab, to drill a
  // specific machine.
  await page.locator('[data-act="machine"][data-arg]').first().click();
  await page.waitForSelector('.stagehdr');
  // (#1508) The unified machine page lists runs as collapsible recentRow
  // <details>; the session-drill "open →" lives in the expanded body, so
  // expand the run first, then click through to its session detail.
  await page.locator('details[data-expand="recent:sess-ended-via-sessionend"] > summary').first().click();
  await page.locator('[data-act="session"][data-arg="sess-ended-via-sessionend"]').first().click();
  await page.waitForSelector('.sub', { timeout: 10_000 });

  expect(pageErrors, `viewer threw: ${pageErrors.join('; ')}`).toHaveLength(0);
});
