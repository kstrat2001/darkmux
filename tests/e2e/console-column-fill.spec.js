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
// real 390px viewport before writing this fix: `.panelout` scrolls
// horizontally within itself (`styles.css`'s `overflow-x: auto`) and
// `document.body` never gains horizontal scroll — the content was always
// touch-reachable. It was also genuinely cut off on screen: the run-list's
// ID column and the footer's `` `--all` for every run) `` clause render
// past the right edge of a 356px box and are invisible until dragged. Both
// are true at once — a real visual truncation, no data loss — and the fix
// addresses the half that was actually missing: a VISIBLE cue that the
// content is reachable at all. Mobile browsers hide the native scrollbar,
// so nothing signaled a swipe would do anything, which is why off-screen
// text read as permanently gone.
//
// The cue is scoped to bodies that ACTUALLY scroll, which is `.panelout`
// alone today — `.panelerr`/`.panelwarn` are `pre-wrap` with no
// `overflow-x`, and though an unbreakable stderr token overflows them just
// the same, their tail is clipped rather than reachable. The last test below
// is the guard for that: a cue there would promise a drag that does nothing.
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

    // The footer's full sentence is in the DOM. To be exact about what that
    // does and does not claim: the footer IS visually cut at phone width —
    // measured with a Range over the rendered text node at 390px, the body's
    // content lays out 770px wide in a 356px box and the sentence's tail
    // (`` `--all` for every run) ``) is off-screen. What it is NOT is data
    // loss: the string is complete in the DOM and horizontally reachable by
    // dragging the body, which is why the fix here is a scroll CUE rather
    // than a re-layout. That is a deliberate scope decision — keep the CLI's
    // own fixed-width rendering (this panel shows what the command actually
    // printed) and make the escape hatch discoverable — NOT a correction of
    // a mistaken bug report. Re-rendering the run list as phone-shaped cards,
    // so nothing is off-screen at all, is the issue's own alternative and
    // stays open.
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

  // The cue promises a gesture. On an ERROR body that promise cannot be
  // kept: `.panelerr`/`.panelwarn` are `pre-wrap` with no `overflow-x`, so
  // an unbreakable stderr token (this fixture is a real-shaped `sha256:…`
  // digest line) overflows the box — `scrollWidth` genuinely exceeds
  // `clientWidth` — while `overflow-x: visible` plus `.panelwrap`'s
  // `overflow: hidden` clips the tail and leaves `scrollLeft` pinned at 0.
  //
  // So this is NOT the "content fits, no cue" case above. The overflow test
  // alone is TRUE here; only the scrollability test suppresses the cue.
  // Both halves are asserted so the test cannot pass for the wrong reason.
  test('no scroll cue on an error body, which overflows but cannot be scrolled', async ({ page }) => {
    const digestLine = 'error: image darkmux-runtime@sha256:9f2c1ba0d7e64f8a3b5c7d9e1f0a2b4c6d8e0f1a3b5c7d9e1f0a2b4c6d8e0f1a not found';
    await page.route('**/panel/**', (route) =>
      route.fulfill({
        contentType: 'application/json',
        body: JSON.stringify({ ...panelBody(''), exit_code: 2, stderr_tail: digestLine }),
      }),
    );
    await page.goto(`${BASE}#lens=console`);
    await page.waitForSelector('.panelerr');
    await page.waitForTimeout(300);

    const m = await page.evaluate(() => {
      const err = document.querySelector('.panelerr');
      err.scrollLeft = 200;
      const reached = err.scrollLeft;
      err.scrollLeft = 0;
      return {
        scrollWidth: err.scrollWidth,
        clientWidth: err.clientWidth,
        reached,
        pageScrollWidth: document.body.scrollWidth,
        viewportWidth: window.innerWidth,
        cue: !!document.querySelector('.panelwrap__scrollcue'),
      };
    });
    // The premise: this body really does overflow, so a cue keyed on
    // overflow ALONE would fire here.
    expect(m.scrollWidth, `error body scrollWidth ${m.scrollWidth} vs clientWidth ${m.clientWidth} — the fixture must overflow or this test proves nothing`).toBeGreaterThan(m.clientWidth + 1);
    // And it cannot be scrolled — by the body or by the page.
    expect(m.reached, `error body scrolled to ${m.reached}px — if it scrolls now, the cue SHOULD fire and this test needs rewriting`).toBe(0);
    expect(m.pageScrollWidth, `page scrollWidth ${m.pageScrollWidth} vs viewport ${m.viewportWidth}`).toBeLessThanOrEqual(m.viewportWidth + 1);
    // Therefore: no cue.
    expect(m.cue, 'the scroll cue must not promise a drag on a body that cannot scroll').toBe(false);
  });
});
