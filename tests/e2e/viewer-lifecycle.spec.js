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

// (drill-in packet) The drilling half of the original test — click through
// from the activity lane to the session's own detail view, to prove the fix
// doesn't TypeError on a session.end-only session's undefined close-edge
// fields — is now real. The pre-#1809 machine-page path this test used to
// drive (card → expand a collapsible recent-run row → the "open →" link) is
// gone for good (#1809 removed that whole per-run list — see
// `viewer-session-url.spec.js`'s own module doc for the full history); the
// drill this packet built instead lives directly on the activity lane's own
// bars (`FleetLens.tsx`'s `.sbar` `onClick`, `data-act="session"`), which is
// exactly where THIS test already looks (`.lane .sbar`, the test right above
// this one) — no separate machine-page detour needed.
//
// This asserts more than "no throw": `runRegions()`'s `c` derivation (the
// non-`session.end` close record, `sessionRun.ts`) exists SPECIFICALLY so a
// session.end-only close reads CANCELED, not COMPLETE — the session-drill
// twin of the activity-lane bug this whole file exists to guard (a session
// that closed only via the reconciler's `session.end`, with no clean
// `dispatch.complete`, must not read as though it succeeded). A bare
// "does not throw" assertion would pass even if that mapping regressed to
// "complete" — asserting the label is what makes this a real, non-vacuous
// regression gate for the close-edge, not just the crash.
test('activity lane: drilling a session.end-only session does not throw', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  // The session-drill's own destination fetch (`SessionReplay` →
  // `/flow-session/<id>`) — this static-playback harness has no daemon
  // behind it, so it needs the same real-shaped mock every other
  // `#session=` spec in this suite uses (`viewer-session-url.spec.js`'s
  // `mockSession`), scoped to exactly the fixture's two records for this
  // session id (dispatch.start, then ONLY session.end — no
  // dispatch.complete/error at all).
  await page.route('**/flow-session/sess-ended-via-sessionend', (r) =>
    r.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        records: [
          {
            ts: '2026-01-01T00:01:00Z', level: 'info', category: 'work', tier: 'local', stage: 'dispatch',
            action: 'dispatch.start', handle: 'darkmux/coder', model: 'qwen',
            session_id: 'sess-ended-via-sessionend', machine_id: 'lifecycle-mac', machine_uid: 'lifecycle-mac-uid',
            payload: { prompt_chars: 42 },
          },
          {
            ts: '2026-01-01T00:02:00Z', level: 'info', category: 'machinery', tier: 'local', stage: 'dispatch',
            action: 'session.end', source: 'presence-reconciler',
            session_id: 'sess-ended-via-sessionend', machine_id: 'lifecycle-mac', machine_uid: 'lifecycle-mac-uid',
          },
        ],
        count: 2,
        truncated: false,
        generated_at_ms: 0,
      }),
    })
  );

  await page.goto('/index-lifecycle.html');
  await page.waitForSelector('.lane .sbar', { timeout: 15_000 });

  await page.locator('.sbar[title*="sess-ended-via-sessionend"]').click();

  await expect(page.locator('.session-run')).toBeVisible();
  await expect.poll(() => page.evaluate(() => location.hash)).toContain('session=sess-ended-via-sessionend');
  // The regression gate itself: CANCELED (the fallback for "closed, but not
  // by a clean dispatch.complete"), never COMPLETE.
  await expect(page.locator('.session-run .pill')).toHaveText('CANCELED');
  await expect(page.locator('.session-run .pill')).not.toHaveText('COMPLETE');

  expect(pageErrors, `viewer threw: ${pageErrors.join('; ')}`).toHaveLength(0);
});
