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

test('activity lane brackets a session.end-only session as ended, not in-flight', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index-lifecycle.html');
  await page.waitForSelector('.lane .sbar', { timeout: 15_000 });

  // The session that closed ONLY via session.end exists as a bar...
  await expect(
    page.locator('.sbar[title*="sess-ended-via-sessionend"]')
  ).toHaveCount(1);
  // ...and is NOT marked in-flight (class "a"). THE regression: pre-fix it was.
  await expect(
    page.locator('.sbar.a[title*="sess-ended-via-sessionend"]')
  ).toHaveCount(0);

  // Control: a clean dispatch.complete is also not in-flight.
  await expect(
    page.locator('.sbar.a[title*="sess-clean-complete"]')
  ).toHaveCount(0);

  // Control: a genuinely open session (dispatch.start, NO terminal at all)
  // DOES render in-flight — the fix must not over-close legitimate running work.
  await expect(
    page.locator('.sbar.a[title*="sess-in-flight"]')
  ).toHaveCount(1);

  expect(pageErrors, `viewer threw: ${pageErrors.join('; ')}`).toHaveLength(0);
});
