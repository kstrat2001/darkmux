// Narrow-width chrome, measured rather than eyeballed (#1613, #1614).
//
// Both defects were reported from the phone and neither has a Rust seam that
// can catch a regression: one is a CSS `order` inside a media query, the other
// is the viewer HALF of a column negotiation whose daemon half is unit-tested
// separately. This file is the only place either can be pinned.
const { test, expect } = require('@playwright/test');

const PHONE = { width: 390, height: 844 };

// A real board row at the width a phone actually gets. 52 columns wide — the
// number `panelCols` computes for a 390px screen, and the number the old floor
// of 60 rounded UP, which is how eight columns ended up off-screen.
const BOARD_52 =
  '\x1b[1;36mmission status — 84 missions\x1b[0m\n\n' +
  '\x1b[2mFINALIZED (84)\x1b[0m\n' +
  '  • \x1b]8;;http://127.0.0.1:8765/mission/dispatch-code-reviewer-1785589698-5d6a-0/graph\x1b\\code-reviewer\x1b]8;;\x1b\\  \x1b[2m5d6a\x1b[0m    \x1b[2m1d\x1b[0m    1/1  ▓▓▓▓\n' +
  '\x1b[32m✓ board is clean\x1b[0m\n';

function panelBody(ansi, over = {}) {
  return {
    panel: 'mission-status',
    argv: ['mission', 'status'],
    captured_ts_ms: Date.now(),
    gather_ms: 12,
    exit_code: 0,
    ansi_text: ansi,
    stderr_tail: '',
    cols: 52,
    cache_ttl_ms: 3000,
    age_ms: 0,
    auto_refresh: false,
    ...over,
  };
}

test.use({ viewport: PHONE });

test.beforeEach(async ({ page }) => {
  await page.route('**/panel/**', (route) =>
    route.fulfill({ contentType: 'application/json', body: JSON.stringify(panelBody(BOARD_52)) })
  );
});

// (#1614) Asserted by MEASURED Y position, not by class order in the DOM —
// `order` changes the visual order without changing the DOM, so a
// document-order assertion would pass against the bug.
test('at phone width the chrome reads broadest-scope-first', async ({ page }) => {
  await page.goto('/index-lab.html');
  await page.click('[data-act="console"]');
  await expect(page.locator('.panelout')).toBeVisible();

  const meta = await page.locator('.meta').first().boundingBox();
  const tabs = await page.locator('.lenstabs').first().boundingBox();
  expect(meta, '.meta must be present to be ordered').not.toBeNull();
  expect(tabs, '.lenstabs must be present to be ordered').not.toBeNull();

  // The regression: machine-scope status ("coder on MacBook-Pro") wrapped to
  // BELOW the tab strip and one line above the panel, where it read as the
  // selected tab's first line of content.
  expect(
    meta.y,
    'global machine status must sit ABOVE the tab selector, not between it and the panel'
  ).toBeLessThan(tabs.y);

  // ...and the tab strip ends up directly above the panel it selects, which is
  // the association a tab bar exists to carry.
  const panel = await page.locator('.panelout').boundingBox();
  expect(tabs.y).toBeLessThan(panel.y);
});

// (#1613) The defect the operator actually reported: scrolling sideways to see
// a mission's progress.
//
// This mock ECHOES the asked width instead of serving a fixed fixture, because
// that is the real mechanism — the viewer asks, the daemon clamps, and the CLI
// renders to exactly that. A fixed-width fixture cannot reproduce the bug at
// all: it stays 52 columns however wrong the ask was, so the test passes
// against the defect. (It did. That is why it echoes.)
//
// The overflow is INSIDE `.panelout`, not on the document — the panel is an
// `overflow-x: auto` container by design, so measuring the body would also
// have passed against the defect.
test('the phone never scrolls sideways to read the board', async ({ page }) => {
  await page.route('**/panel/**', (route) => {
    const cols = Number(new URL(route.request().url()).searchParams.get('cols') || 100);
    // One board row rendered to exactly `cols`, the way the CLI would.
    const row = ('  • code-reviewer  5d6a    1d    1/1  ▓▓▓▓').padEnd(cols, ' ').slice(0, cols);
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify(panelBody(`${row}\n${row}\n`, { cols })),
    });
  });

  await page.goto('/index-lab.html');
  await page.click('[data-act="console"]');
  await expect(page.locator('.panelout')).toBeVisible();

  const over = await page.evaluate(() => {
    const el = document.querySelector('.panelout');
    return { scroll: el.scrollWidth, client: el.clientWidth };
  });
  expect(
    over.scroll - over.client,
    `the panel rendered ${over.scroll}px into a ${over.client}px box — the operator scrolls sideways`
  ).toBeLessThanOrEqual(0);
});

// The client half of the column negotiation. The daemon clamps to [36, 200]
// (unit-tested in panel.rs); this pins that the viewer's own floor moved with
// it, so a phone asks for what it has rather than for the old 60.
test('the panel asks for a column count the phone can actually show', async ({ page }) => {
  let asked = null;
  await page.route('**/panel/**', (route) => {
    const u = new URL(route.request().url());
    const c = u.searchParams.get('cols');
    if (c !== null) asked = Number(c);
    route.fulfill({ contentType: 'application/json', body: JSON.stringify(panelBody(BOARD_52)) });
  });

  await page.goto('/index-lab.html');
  await page.click('[data-act="console"]');
  await expect(page.locator('.panelout')).toBeVisible();

  expect(asked, 'the panel fetch must carry a cols hint').not.toBeNull();
  expect(asked, 'must not ask below the daemon clamp').toBeGreaterThanOrEqual(36);
  expect(
    asked,
    'a 390px phone must ask for fewer than the old 60-column floor'
  ).toBeLessThan(60);
});
