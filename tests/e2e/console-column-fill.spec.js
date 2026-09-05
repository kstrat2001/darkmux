// (U2-4) The console panel fills its column on a sparse day.
//
// Measured, not eyeballed: on a 1456px viewport a short panel body ended at
// y=524 in an 948px-tall stage, leaving ~45% of the column blank beside an
// events column that stretches the full height. That reads as a broken
// layout rather than as "there is not much output today".
//
// Both directions are asserted, because the fix's whole risk is the second:
// a short panel now GROWS to the stage's bottom, and a long one must still
// be allowed to exceed it (the page scrolls, exactly as before — `flex: 1 1
// auto` with the default `min-height: auto` never shrinks below content).
const { test, expect } = require('@playwright/test');

const BASE = process.env.SHOT_BASE || 'http://127.0.0.1:47955/index.html';

test.describe('(U2-4) console lens column fill', () => {
  test.use({ viewport: { width: 1456, height: 900 } });

  test('a short panel reaches the bottom of the stage instead of stopping a third of the way down', async ({ page }) => {
    await page.goto(`${BASE}#lens=console`);
    await page.waitForSelector('.panelwrap');
    await page.waitForTimeout(1500);
    const m = await page.evaluate(() => {
      const stage = document.querySelector('.app-shell__stage').getBoundingClientRect();
      const wrap = document.querySelector('.panelwrap').getBoundingClientRect();
      return { stageBottom: Math.round(stage.bottom), wrapBottom: Math.round(wrap.bottom), wrapHeight: Math.round(wrap.height) };
    });
    // Within the stage's own 16px bottom padding.
    expect(
      m.stageBottom - m.wrapBottom,
      `panel bottom ${m.wrapBottom} vs stage bottom ${m.stageBottom} — ${m.stageBottom - m.wrapBottom}px of dead column`,
    ).toBeLessThanOrEqual(20);
    expect(m.wrapBottom, 'the panel must not overshoot its own stage either').toBeLessThanOrEqual(m.stageBottom + 1);
  });

  test('a panel LONGER than the column still grows past it — the fill is a floor, not a clamp', async ({ page }) => {
    await page.goto(`${BASE}#lens=console`);
    await page.waitForSelector('.panelout, .panelerr');
    await page.waitForTimeout(1500);
    const grew = await page.evaluate(() => {
      const body = document.querySelector('.panelout, .panelerr');
      const before = document.querySelector('.panelwrap').getBoundingClientRect().height;
      body.textContent = Array.from({ length: 400 }, (_, i) => `line ${i}`).join('\n');
      const after = document.querySelector('.panelwrap').getBoundingClientRect().height;
      return { before: Math.round(before), after: Math.round(after) };
    });
    expect(grew.after, `panel clamped at ${grew.after}px for 400 lines of output`).toBeGreaterThan(grew.before);
  });
});
