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
  // (#1622) Every other spec collects uncaught errors; these two did not, so a
  // throw during render would go unreported while the assertions still passed.
  page.on('pageerror', (e) => { throw new Error(`uncaught page error: ${e}`); });
  await page.route('**/panel/**', (route) =>
    route.fulfill({ contentType: 'application/json', body: JSON.stringify(panelBody(BOARD_52)) })
  );
});

// The console lens's default landing panel is `run-list` (#1905 step 3;
// briefly `ActivityPanel`, a client-rendered `/runs` union with no
// `/panel/*` call at all, under #1904 — deleted, see `panels.ts`'s own doc
// on `PANELS`), not `mission-status`, the panel these three tests actually
// measure (`.panelout`'s position, its overflow box, the `cols` it asks
// for). Explicitly selecting the panel is a better fixture than depending
// on the console's default happening to be the SAME panel these fixtures
// were written around — it keeps working no matter what the lens defaults
// to next.
async function selectMissionStatus(page) {
  await page.click('[data-act="console"]');
  await page.click('[data-act="setpanel"][data-arg="mission-status"]');
}

// (#1614) Asserted by MEASURED Y position, not by class order in the DOM —
// `order` changes the visual order without changing the DOM, so a
// document-order assertion would pass against the bug.
test('at phone width the chrome reads broadest-scope-first', async ({ page }) => {
  await page.goto('/index-lab.html');
  await selectMissionStatus(page);
  await expect(page.locator('.panelout')).toBeVisible();

  // (port note) `.meta`/`.lenstabs` were legacy's own class names for these
  // two regions; the port kept the SAME two elements at the SAME `#meta` id
  // (still selected by id — `App.tsx`'s own doc: the parity extractor reads
  // `#crumb`/`#meta` by id regardless of parent) but restyled them under its
  // BEM convention (`app-shell__meta`, `app-shell__navtabs` —
  // `NavChrome.tsx`'s own doc calls out that `.lenstabs` is what this bar
  // ports), so neither bare class selects anything on this page anymore.
  // `#meta` is still unique; the tab bar has no id, so its `aria-label` (set
  // for the same reason a `<nav>` landmark needs one) is the stable hook.
  const meta = await page.locator('#meta').first().boundingBox();
  const tabs = await page.locator('nav[aria-label="lens navigation"]').first().boundingBox();
  expect(meta, '#meta must be present to be ordered').not.toBeNull();
  expect(tabs, 'the lens-tab nav must be present to be ordered').not.toBeNull();

  // The regression: machine-scope status ("coder on MacBook-Pro") wrapped to
  // BELOW the tab strip and one line above the panel, where it read as the
  // selected tab's first line of content.
  // (#2071) The tab strip is the top of the sticky block now (tabs +
  // transport, operator decision), and the status line sits directly under
  // it, above the panel. Before #2071 the order was status, tabs, panel.
  expect(
    tabs.y,
    'the tab selector is the top of the sticky block; the status line sits under it'
  ).toBeLessThan(meta.y);
  expect(meta.y, 'the status line sits above the panel').toBeLessThan((await page.locator('.panelout').boundingBox()).y);

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
  await selectMissionStatus(page);
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
  await selectMissionStatus(page);
  await expect(page.locator('.panelout')).toBeVisible();

  expect(asked, 'the panel fetch must carry a cols hint').not.toBeNull();
  expect(asked, 'must not ask below the daemon clamp').toBeGreaterThanOrEqual(36);
  expect(
    asked,
    'a 390px phone must ask for fewer than the old 60-column floor'
  ).toBeLessThan(60);
});

// (#1640) Touch and input sizing on a phone.
//
// One of these two is testable here and one is not, and the difference is worth
// stating: the iOS auto-zoom heuristic is WebKit-only, so Chromium will never
// reproduce it and no assertion here can prove the fix works — only that the
// font-size that triggers it is gone. That is a proxy, and it is named as one.

test('no text input is small enough to trigger iOS auto-zoom on focus', async ({ page }) => {
  // iOS Safari zooms the whole page when a text input with font-size < 16px
  // takes focus. The operator reads this over the tailnet on a phone, so the
  // stream filter yanked the layout every time he typed.
  //
  // PROXY ASSERTION: this checks the computed font-size, not the zoom. Chromium
  // does not implement the heuristic, so the real behaviour is unobservable in
  // this harness. Asserting the trigger condition is the most this suite can
  // honestly do.
  await page.goto('/index-lab.html');
  // (port note) The one text-entry input this app ships (`#logq`, the event
  // log's filter box — `EventLogColumn.tsx`) is a real `type="search"`, not
  // `type="text"`/typeless like legacy's `#fsearch`/`#logq` were. The iOS
  // auto-zoom heuristic this test proxies for fires on ANY text-entry input
  // under 16px regardless of type (`search` included — same OS-level
  // behavior as `text`), so the selector widens rather than narrows: this
  // makes the check MORE accurate to what it claims to guard, not less.
  const sizes = await page.evaluate(() =>
    Array.from(document.querySelectorAll('input[type="text"], input[type="search"], input:not([type])')).map((el) => ({
      id: el.id || el.className,
      px: parseFloat(getComputedStyle(el).fontSize),
    }))
  );
  expect(sizes.length, 'no text inputs found — the check would be vacuous').toBeGreaterThan(0);
  for (const s of sizes) {
    expect(s.px, `input \`${s.id}\` is ${s.px}px — iOS will zoom the page on focus`).toBeGreaterThanOrEqual(16);
  }
});
