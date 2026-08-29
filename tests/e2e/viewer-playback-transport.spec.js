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

  // (#2120, operator finding — "almost none of it is meaningful") The
  // clock's own "N/M rec" readout is gone — the range input is the
  // progress indicator now, and the record/machine counts moved to the
  // Machine info modal's `playback` row. This fixture's mission id is an
  // XSS payload string with no "review" token, so `humanMissionLabel`
  // (`ui/src/lib/replayMeta.ts`) derives no label either — the clock
  // renders bare `HH:MM` throughout this run, which at this fixture's
  // 13-SECOND span never even changes minute. The clock is therefore no
  // longer a usable movement signal for this spec; the range value and
  // the play/pause button title carry the whole proof below, same as they
  // always did independently of the (now-removed) rec count.
  //
  // The 13-second fixture day plays inside ONE 100 ms tick at the default
  // 1h/s, so asserting the "rewound and playing" state from the test side
  // is a race against the loop. Record the transitions from INSIDE the page
  // instead: a requestAnimationFrame sampler (~16 ms, finer than the tick)
  // captures every distinct (range value, button title) state the
  // transport passes through; the assertions read that record once the run
  // has completed. Pressing play while pinned at the end restarts from the
  // beginning (`togglePlay`'s "restart if at the end" rule, ported from
  // legacy's `if(state.t>=tMax)state.t=tMin;`), so the first state seen
  // must be the range at 0 with the button reading "pause".
  await page.evaluate(() => {
    const range = document.querySelector('.scrub input[type="range"]');
    const btn = document.querySelector('.scrub button.primary');
    window.__transport = [];
    const sample = () => {
      const state = `${range.value}|${btn.title}`;
      if (state !== window.__transport[window.__transport.length - 1]) window.__transport.push(state);
      window.__transportRaf = requestAnimationFrame(sample);
    };
    sample();
  });
  await playBtn.click();

  // Real wall-clock, real `setInterval` — poll until the playhead has
  // measurably moved off zero. The fixture's ~13s span at 1x advances the
  // full range in ~12s (legacy's own `(tMax-tMin)/120` step every 100ms),
  // so any nonzero value within a few real seconds is genuine motion, not
  // a fluke.
  // Speed is a real multiplier of elapsed time now ("1× doesn't seem 1×",
  // the #2071 follow-up): at the default 1h/s this 13-second fixture day
  // plays out inside ONE tick, so sampling mid-flight is a race against
  // the loop. The advance is proven by the run COMPLETING instead: the
  // range returns to 100 and the play button flips back from "pause" —
  // two independent signals of a playhead that moved, cross-checked
  // against the transitions record below for the REWOUND state in between.
  await expect(range).toHaveValue('100', { timeout: 5_000 });
  await expect(playBtn).toHaveAttribute('title', 'play');
  const transitions = await page.evaluate(() => {
    cancelAnimationFrame(window.__transportRaf);
    return window.__transport;
  });
  const rewound = transitions.find((st) => st === '0|pause');
  expect(rewound, `transitions seen: ${transitions.join(' → ')}`).toBeTruthy();
  expect(transitions[transitions.length - 1]).toBe('100|play');

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
