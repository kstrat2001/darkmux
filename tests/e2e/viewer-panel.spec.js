// Headless e2e for the CLI-panel console tab (#1569 packet B2).
//
// Two jobs, and the second is the load-bearing one:
//   1. the console renders a panel's ANSI output as styled DOM, honors the
//      daemon's manual-only contract, and sends the required header;
//   2. it renders ATTACKER-CONTROLLED bytes inertly. `/panel/:id` returns
//      whatever a CLI verb printed — the viewer's only defense is this
//      renderer, so the hostile walk belongs in CI beside `viewer-xss`,
//      not in a one-off shell check.
const { test, expect } = require('@playwright/test');

// A real captured payload shape (see PanelResponse in crates/darkmux-serve/
// src/panel.rs) — SGR + an OSC 8 link, the two escape families the
// renderer is scoped to.
function panelBody(ansi, over = {}) {
  return {
    panel: 'mission-status',
    argv: ['mission', 'status'],
    captured_ts_ms: Date.now(),
    gather_ms: 12,
    exit_code: 0,
    ansi_text: ansi,
    stderr_tail: '',
    cols: 100,
    cache_ttl_ms: 3000,
    age_ms: 0,
    auto_refresh: true,
    ...over,
  };
}

const REAL_ANSI =
  '\x1b[1;36mmission status — 3 missions\x1b[0m\n\n' +
  '\x1b[2mACTIVE (1)\x1b[0m\n' +
  '  ◆ \x1b]8;;http://127.0.0.1:8765/mission/doom-loop-m4/graph\x1b\\doom-loop-m4\x1b]8;;\x1b\\  0/4  ░░░░\n' +
  '\x1b[32m✓ board is clean\x1b[0m\n';

async function routePanels(page, handler) {
  await page.route('**/panel/**', handler);
}

// (#1904 CI fix) The console lens's DEFAULT landing view is now
// `ActivityPanel` (a client-rendered union over `/runs`, no `/panel/*` call
// at all) rather than the `mission-status` CLI panel — every test below
// that actually exercises panel rendering, the manual-only contract, or
// the XSS gate needs a REAL CLI panel on screen to test, so it can no
// longer get there for free by just clicking the console tab. This
// explicitly selects `mission-status` (the same panel these tests always
// meant to exercise — `panelBody()`'s own default `panel: 'mission-status'`
// names it) after landing on console, which is a BETTER fixture than
// depending on the console's default happening to be a CLI panel: it
// keeps working no matter what the lens defaults to next.
async function selectMissionStatus(page) {
  await page.click('[data-act="console"]');
  await page.click('[data-act="setpanel"][data-arg="mission-status"]');
}

test('the console renders a panel as styled DOM and sends the required header', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  let sawHeader = null;
  await routePanels(page, (route) => {
    sawHeader = route.request().headers()['x-darkmux-panel'] || null;
    route.fulfill({ contentType: 'application/json', body: JSON.stringify(panelBody(REAL_ANSI)) });
  });

  await page.goto('/index-lab.html');
  await selectMissionStatus(page);
  await expect(page.locator('.panelout')).toBeVisible();

  // (#1602) The daemon REQUIRES this header — it forces a CORS preflight a
  // foreign page cannot pass. A viewer that forgets it gets a 403, so this
  // pins the client half of that contract.
  expect(sawHeader, 'panel fetch must carry X-Darkmux-Panel').toBe('1');

  // SGR became classes, not literal escapes.
  await expect(page.locator('.panelout .a-fg6').first()).toContainText('mission status');
  await expect(page.locator('.panelout .a-dim').first()).toContainText('ACTIVE');
  expect(await page.locator('.panelout').innerHTML()).not.toContain('\x1b');

  // The chrome states the command, when, and what it cost — each a thing
  // the tab and crumb do NOT already say.
  await expect(page.locator('.panelchrome')).toContainText('$ darkmux mission status');
  await expect(page.locator('.panelchrome')).toContainText('12ms');

  expect(pageErrors, `uncaught: ${pageErrors.join(' | ')}`).toEqual([]);
});

test('a loopback OSC 8 target is rewritten relative so one link works from the phone too', async ({ page }) => {
  await routePanels(page, (route) =>
    route.fulfill({ contentType: 'application/json', body: JSON.stringify(panelBody(REAL_ANSI)) })
  );
  await page.goto('/index-lab.html');
  await selectMissionStatus(page);

  // Packet A bakes ABSOLUTE daemon URLs, and on a standalone machine that
  // means loopback — which tapped from a phone opens the PHONE's localhost.
  // Rewriting daemon-origin hrefs to path-only makes one baked link correct
  // from the desk and over the tailnet both.
  const href = await page.locator('.panelout a.a-link').first().getAttribute('href');
  expect(href).toBe('/mission/doom-loop-m4/graph');
});

test('a manual-only panel never runs until asked', async ({ page }) => {
  const asked = [];
  await routePanels(page, (route) => {
    const id = route.request().url().split('/panel/')[1].split('?')[0];
    asked.push(id);
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify(panelBody('doctor output', { panel: id, argv: ['doctor'], auto_refresh: false })),
    });
  });

  await page.goto('/index-lab.html');
  await selectMissionStatus(page);
  await expect(page.locator('.panelout')).toBeVisible();
  expect(asked).toEqual(['mission-status']);

  // Selecting `doctor` must NOT fetch: it probes the machine, and running
  // it unasked is the observer joining the observed (#1286). This once
  // regressed because `!st || (… && !manual)` short-circuits — the guard
  // worked only on repeat visits, i.e. never when it mattered.
  await page.click('[data-act="setpanel"][data-arg="doctor"]');
  await page.waitForTimeout(400);
  expect(asked, 'selecting a manual panel must not run it').toEqual(['mission-status']);
  await expect(page.locator('.panelout')).toContainText('not run yet');

  // …and an explicit run does.
  await page.click('[data-act="refreshpanel"]');
  await expect(page.locator('.panelchrome')).toContainText('manual-run only');
  expect(asked).toEqual(['mission-status', 'doctor']);
});

test('the daemon\'s own refusal is shown verbatim, not reworded', async ({ page }) => {
  await routePanels(page, (route) =>
    route.fulfill({ status: 429, contentType: 'text/plain', body: 'panel "doctor" is manual-run only and ran 3s ago — floored at 30s\n' })
  );
  await page.goto('/index-lab.html');
  await selectMissionStatus(page);
  // Inventing viewer-side wording for a server rule is the twin-drift this
  // whole packet exists to kill.
  await expect(page.locator('.panelerr')).toContainText('floored at 30s');
});

test('the console renders attacker-controlled panel bytes inertly', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  // `/panel/:id` returns whatever a CLI verb printed. Each payload is a
  // shape that must NOT become live DOM.
  const hostile = [
    '<img src=x onerror=window.__xss=1>',
    '<svg onload=window.__xss=1>',
    '</span><img src=x onerror=window.__xss=1>',
    '\x1b]8;;javascript:window.__xss=1\x1b\\click\x1b]8;;\x1b\\',
    '\x1b]8;;http://x/" onmouseover=window.__xss=1 y="\x1b\\t\x1b]8;;\x1b\\',
    '\x1b[999;999mweird\x1b[0m',      // unknown SGR params
    'trunc\x1b[1;36',                  // truncated CSI
    'trunc\x1b]8;;http://x/',          // truncated OSC
    'a\x1bZb',                         // unknown escape introducer
  ].join('\n');

  await routePanels(page, (route) =>
    route.fulfill({ contentType: 'application/json', body: JSON.stringify(panelBody(hostile)) })
  );
  await page.goto('/index-lab.html');
  await selectMissionStatus(page);
  await expect(page.locator('.panelout')).toBeVisible();

  const fired = await page.evaluate(() => window.__xss);
  expect(fired, 'XSS canary fired inside a panel').toBeUndefined();
  const live = await page.evaluate(() =>
    document.querySelectorAll(
      '.panelout img, .panelout svg, .panelout [onerror], .panelout [onmouseover], .panelout [onload], .panelout a[href^="javascript:"]'
    ).length
  );
  expect(live, 'hostile payload became live DOM').toBe(0);
  // The javascript: target degrades to inert TEXT, never a dead link that
  // still looks clickable.
  await expect(page.locator('.panelout')).toContainText('click');

  expect(pageErrors, `uncaught: ${pageErrors.join(' | ')}`).toEqual([]);
});

test('a panel deep link switches in page instead of navigating', async ({ page }) => {
  // The verb emits this when it knows it is rendering into a panel (see
  // `panel_deep_link`): "`--all` for every mission" is actionable advice in a
  // terminal and a dead end here, so the flag names itself as a link instead.
  const WITH_LINK =
    '\x1b[2m  … 81 more (3 of 84 shown)\x1b[0m\n' +
    '  \x1b]8;;http://127.0.0.1:8765/#lens=console&panel=mission-status-all\x1b\\' +
    '→ show every mission\x1b]8;;\x1b\\\n';
  const asked = [];
  await routePanels(page, (route) => {
    const id = new URL(route.request().url()).pathname.split('/').pop();
    asked.push(id);
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify(
        id === 'mission-status-all'
          ? panelBody('\x1b[2m  the whole board\x1b[0m\n', { panel: id, argv: ['mission', 'status', '--all'] })
          : panelBody(WITH_LINK)
      ),
    });
  });

  await page.goto('/index-lab.html');
  await selectMissionStatus(page);
  await expect(page.locator('.panelout')).toContainText('81 more');

  const before = page.url();
  await page.click('.panelout a:has-text("show every mission")');
  await expect(page.locator('.panelout')).toContainText('the whole board');

  // Switched IN PAGE: the target panel was fetched, and following the href
  // would have reloaded the viewer to land one tab from where it already was.
  expect(asked).toContain('mission-status-all');
  expect(page.url().split('#')[0]).toBe(before.split('#')[0]);
});

test('a manual panel stays unrun across leaving and re-entering the tab', async ({ page }) => {
  // The guard on `setPanel` alone was not enough: selecting `doctor` returns
  // BEFORE creating any state entry, so `goConsole`'s "no state yet -> load"
  // re-entry path probed the machine unasked. The server's rate floor bounds
  // a loop; it never stopped the first run. Doctor spawns lms + a GitHub API
  // call + a Keychain read on the measured host, so an unasked run is the
  // #1286 failure, not a performance nit.
  const asked = [];
  await routePanels(page, (route) => {
    const id = new URL(route.request().url()).pathname.split('/').pop();
    asked.push(id);
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify(panelBody(REAL_ANSI, { panel: id, auto_refresh: id !== 'doctor' })),
    });
  });

  await page.goto('/index-lab.html');
  await selectMissionStatus(page);
  await expect(page.locator('.panelout')).toBeVisible();
  await page.click('[data-act="setpanel"][data-arg="doctor"]');
  await page.waitForTimeout(300);
  expect(asked).not.toContain('doctor');

  // Leave and come back — `doctor` is still the selected panel.
  await page.click('[data-act="fleet"]');
  await page.waitForTimeout(200);
  await page.click('[data-act="console"]');
  await page.waitForTimeout(600);
  expect(asked).not.toContain('doctor');
});

test('a console deep link boots straight into that panel and stays addressable', async ({ page }) => {
  // The CLI emits `#lens=console&panel=<id>` links, so they have to survive
  // being treated as URLs — middle-click, copy-link, a phone long-press
  // "open in new tab". Without a parser they worked on click and died
  // everywhere else, which is the same dead end in a second modality.
  await routePanels(page, (route) => {
    const id = new URL(route.request().url()).pathname.split('/').pop();
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify(
        panelBody('\x1b[2m  the whole board\x1b[0m\n', { panel: id, argv: ['mission', 'status', '--all'] })
      ),
    });
  });

  await page.goto('/index-lab.html#lens=console&panel=mission-status-all');
  await expect(page.locator('.panelout')).toContainText('the whole board');
  await expect(page.locator('[data-act="setpanel"][data-arg="mission-status-all"]')).toHaveClass(/\bon\b/);

  // And the address bar still describes what is on screen, so what the
  // operator copies out of it comes back to the same place.
  expect(page.url()).toContain('lens=console');
  expect(page.url()).toContain('panel=mission-status-all');
});

test('the console shows no crumb — the picker and the chrome line already say which panel', async ({ page }) => {
  await routePanels(page, (route) =>
    route.fulfill({ contentType: 'application/json', body: JSON.stringify(panelBody(REAL_ANSI)) })
  );
  await page.goto('/index-lab.html');
  await page.click('[data-act="console"]');
  // (#1904 CI fix) This test's own subject is the CRUMB, which `ConsolePanel`
  // never touches regardless of which of its own views is showing — it does
  // not need a CLI panel selected, just the console lens actually mounted.
  // Was `.panelout` (true only because the default happened to render one);
  // `.consoleactivity` is what the bare landing view — its default now —
  // actually renders, so this no longer depends on that default surviving
  // the next redesign either.
  await expect(page.locator('.consoleactivity')).toBeVisible();
  expect((await page.locator('#crumb').innerText()).trim()).toBe('');
});
