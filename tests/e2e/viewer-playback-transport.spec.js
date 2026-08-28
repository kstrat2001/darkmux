// (#1869) The playback transport — real-browser proof that pressing play
// actually advances the playhead, on the STATIC build. Unit-level coverage
// (`ui/src/lenses/catalog/PlaybackLens.test.tsx`) already drives the same
// interval with fake timers; this is the live-browser complement the issue
// itself calls for — a real `setInterval`, real wall-clock, on the same
// static-playback render path `scripts/build-demo.sh` ships to
// darkmux.com/demo (mode=play + darkmux-flow-src, no daemon behind it).
//
// Uses the harness's default `/index.html` (built in playwright.config.js
// from `tests/fixtures/xss-flow.jsonl`, mode=play) — the same static-source
// render path `viewer-xss.spec.js` already exercises, just for the
// transport instead of the escaping contract. The fixture spans ~13
// wall-clock seconds of recorded time (00:00:00–00:00:13), short enough
// that a couple of real seconds of play visibly moves the range.
const { test, expect } = require('@playwright/test');

test('pressing play on the static build actually advances the playhead', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index.html');
  await page.waitForSelector('.scrub', { timeout: 15_000 });

  const range = page.locator('.scrub input[type="range"]');
  const playBtn = page.locator('.scrub button.primary');

  // Boots pinned at the newest record — the pre-#1869 default, still true
  // for an un-scrubbed playhead.
  await expect(range).toHaveValue('100');
  await expect(playBtn).toHaveAttribute('title', 'play');

  const clock = page.locator('[data-testid="scrubber-clock"]');
  const clockBefore = await clock.innerText();
  // "HH:MM · N/M rec" — read the fixture's own total off the DOM rather
  // than hardcoding it, so this spec never has to be re-numbered if the
  // fixture (shared with `viewer-xss.spec.js`) grows or shrinks a record.
  const total = clockBefore.match(/\/(\d+) rec$/)[1];

  await playBtn.click();

  // Pressing play while pinned at the end restarts from the beginning
  // (`onTogglePlay`'s own "restart if at the end" rule, ported from
  // legacy's `if(state.t>=tMax)state.t=tMin;`) — so the FIRST observable
  // effect is the range dropping to 0, before it climbs again.
  await expect(range).toHaveValue('0');
  await expect(playBtn).toHaveAttribute('title', 'pause');
  // The record count half of the clock readout drops too — rewound to the
  // start, only the record(s) at-or-before tMin remain visible. Asserting
  // the NUMERATOR here, not the denominator: `total` (the day's whole
  // count) is invariant across the entire test, so a denominator-only
  // assertion would pass even if the numerator never moved at all.
  const clockAfterRewind = await clock.innerText();
  const numeratorAfterRewind = Number(clockAfterRewind.match(/(\d+)\/\d+ rec$/)[1]);
  expect(numeratorAfterRewind).toBeLessThan(Number(total));

  // Real wall-clock, real `setInterval` — poll until the playhead has
  // measurably moved off zero. The fixture's ~13s span at 1x advances the
  // full range in ~12s (legacy's own `(tMax-tMin)/120` step every 100ms),
  // so any nonzero value within a few real seconds is genuine motion, not
  // a fluke.
  await expect.poll(async () => Number(await range.inputValue()), { timeout: 5_000 }).toBeGreaterThan(0);

  // Speed is a real multiplier of elapsed time now ("1× doesn't seem 1×",
  // the #2071 follow-up): at the default 1h/s this 13-second fixture day
  // plays out inside ONE tick, so sampling the clock mid-flight is a race
  // against the loop. The advance is proven by the run COMPLETING instead:
  // the range returns to 100, the play button flips back from "pause", and
  // the clock (real DOM text, not the input's own `.value`) counts every
  // record again — three independent signals of a playhead that moved.
  await expect(range).toHaveValue('100', { timeout: 5_000 });
  await expect(playBtn).toHaveAttribute('title', 'play');
  expect(await clock.innerText()).toMatch(new RegExp(`${total}/${total} rec$`));

  expect(pageErrors, `pageerror events: ${pageErrors.join('; ')}`).toHaveLength(0);
});

test('the transport is absent on a live (daemon, no-hash) route', async ({ page }) => {
  // `index-live.html` — the daemon-shaped harness with no flow-src meta at
  // all (playwright.config.js's own doc: "the shape a real daemon serves").
  // Route the flow endpoints so the fleet lens actually renders instead of
  // hanging on a real (nonexistent) daemon.
  await page.route('**/flow/*', (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  await page.route('**/fleet/machines/live', (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify({ machines: [], meta: { sources: { fleet: { state: 'off' } }, complete: true } }) })
  );
  await page.route('**/fleet/sessions/live', (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify({ sessions: [], meta: { sources: { fleet: { state: 'off' } }, complete: true } }) })
  );
  await page.route('**/machine/specs', (r) => r.fulfill({ status: 404, contentType: 'application/json', body: '{}' }));

  await page.goto('/index-live.html');
  await page.waitForSelector('.savings', { timeout: 15_000 });

  await expect(page.locator('.scrub')).toHaveCount(0);
});
