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

// Relative to the harness's baseURL (tests/e2e/playwright.config.js), like every sibling spec.
const BASE = '/index.html';

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

// (#2077) On a phone, a fixed-width CLI table (`run-list`'s
// KIND/STATUS/STARTED/DURATION/ID/MACHINE columns) is wider than the panel's
// own box. Verified against the demo's own captured `run-list` fixture at a
// real 390px viewport before writing this fix: `.panelout` already scrolls
// horizontally within itself (`styles.css`'s `overflow-x: auto`) and
// `document.body` never gains horizontal scroll — the content was always
// touch-reachable. What was missing, and is asserted here, is a VISIBLE cue
// that it's reachable at all: mobile browsers hide the native scrollbar, so
// nothing signaled a swipe would do anything, and the same off-screen text
// (the run-list's ID column, and the footer's `` `--all` for every run) ``
// clause) read as truncated rather than merely scrolled.
const RUN_LIST_ANSI =
  'KIND      STATUS    STARTED    DURATION  ID                                              MACHINE\n' +
  Array.from({ length: 8 }, (_, i) =>
    `dispatch  complete  1d ago     9m        run-${1000 + i}-abcdef0123456789abcdef0123456789 Workstation`,
  ).join('\n') +
  '\n\nshowing 10 of 11 runs (1 more not shown — `--all` for every run)\n';

const SHORT_ANSI = 'darkmux doctor — 0 checks\n';

function panelBody(ansi) {
  return {
    panel: 'run-list',
    argv: ['run', 'list'],
    captured_ts_ms: Date.now(),
    gather_ms: 5,
    exit_code: 0,
    ansi_text: ansi,
    stderr_tail: '',
    cols: 100,
    cache_ttl_ms: 3000,
    age_ms: 0,
    auto_refresh: true,
  };
}

test.describe('(#2077) console lens on a phone: wide panel output scrolls in its own box, never the page', () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

  test('a run-list-shaped table never widens the page, and the footer sentence is intact in the DOM', async ({ page }) => {
    await page.route('**/panel/**', (route) =>
      route.fulfill({ contentType: 'application/json', body: JSON.stringify(panelBody(RUN_LIST_ANSI)) }),
    );
    await page.goto(`${BASE}#lens=console`);
    await page.waitForSelector('.panelout');
    await page.waitForTimeout(300);

    const geo = await page.evaluate(() => {
      const out = document.querySelector('.panelout');
      return {
        viewportWidth: window.innerWidth,
        bodyScrollWidth: document.body.scrollWidth,
        outScrollWidth: out.scrollWidth,
        outClientWidth: out.clientWidth,
      };
    });
    // The PAGE never scrolls sideways, even though the panel's own content
    // does — this is the acceptance bar, not "the table fits."
    expect(geo.bodyScrollWidth, `body scrollWidth ${geo.bodyScrollWidth} vs viewport ${geo.viewportWidth}`).toBeLessThanOrEqual(geo.viewportWidth + 1);
    // The fixture is genuinely wider than its box — a vacuous pass (a
    // fixture that happens to fit) would prove nothing about the scroll
    // affordance below.
    expect(geo.outScrollWidth, 'fixture must actually overflow the panel box for this test to mean anything').toBeGreaterThan(geo.outClientWidth + 50);

    // The footer's full sentence is in the DOM — "cut mid-sentence" was a
    // rendering (visible-viewport) read of the same overflow, not a real
    // truncation; the CLI's own text survives past what's on-screen without
    // scrolling.
    const text = await page.locator('.panelout').innerText();
    expect(text).toContain('showing 10 of 11 runs (1 more not shown — `--all` for every run)');
  });

  test('the scroll cue appears above the body (never overlapping it) when content overflows, and is absent when it fits', async ({ page }) => {
    await page.route('**/panel/**', (route) =>
      route.fulfill({ contentType: 'application/json', body: JSON.stringify(panelBody(RUN_LIST_ANSI)) }),
    );
    await page.goto(`${BASE}#lens=console`);
    await page.waitForSelector('.panelout');
    await page.waitForTimeout(300);

    const wide = await page.evaluate(() => {
      const cue = document.querySelector('.panelwrap__scrollcue');
      const out = document.querySelector('.panelout');
      if (!cue || !out) return { cueExists: !!cue };
      const c = cue.getBoundingClientRect();
      const o = out.getBoundingClientRect();
      return { cueExists: true, cueBottom: Math.round(c.bottom), outTop: Math.round(o.top) };
    });
    expect(wide.cueExists, 'the scroll cue must render for content that overflows its box').toBe(true);
    // Geometry, not a CSS string: the cue's own box must end at-or-above the
    // body's box, so it can never sit on top of real table/footer text (the
    // regression this file's own module doc names — an earlier absolutely
    // positioned version landed directly on the footer line).
    expect(wide.cueBottom, `cue bottom ${wide.cueBottom} vs panelout top ${wide.outTop} — the cue overlaps the content it points at`).toBeLessThanOrEqual(wide.outTop);

    // Re-run selecting a panel whose output comfortably fits the phone
    // width — the cue must not persist from the previous (wide) selection.
    await page.route('**/panel/**', (route) =>
      route.fulfill({ contentType: 'application/json', body: JSON.stringify({ ...panelBody(SHORT_ANSI), panel: 'doctor', argv: ['doctor'] }) }),
    );
    await page.click('[data-act="setpanel"][data-arg="doctor"]');
    await page.click('[data-act="refreshpanel"]');
    await expect(page.locator('.panelout')).toContainText('darkmux doctor');
    const narrow = await page.evaluate(() => !!document.querySelector('.panelwrap__scrollcue'));
    expect(narrow, 'the scroll cue must not render for content that already fits').toBe(false);
  });
});
