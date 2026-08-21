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

test('a panel deep link resolves through the real anchor (#1911: the JS in-page switch no longer intercepts a retired alias id)', async ({ page }) => {
  // The verb emits this when it knows it is rendering into a panel (see
  // `panel_deep_link`): "`--all` for every mission" is actionable advice in a
  // terminal and a dead end here, so the flag names itself as a link instead.
  // The LINK TARGET is unchanged server-side (`panel_deep_link` in
  // `src/mission_status.rs` still bakes `panel=mission-status-all` — that is
  // the CLI's own one-release compat posture, #1911).
  const WITH_LINK =
    '\x1b[2m  … 81 more (3 of 84 shown)\x1b[0m\n' +
    '  \x1b]8;;http://127.0.0.1:8765/#lens=console&panel=mission-status-all\x1b\\' +
    '→ show every mission\x1b]8;;\x1b\\\n';
  const asked = [];
  await routePanels(page, (route) => {
    const url = new URL(route.request().url());
    const id = url.pathname.split('/').pop();
    const all = url.searchParams.get('opt.all') === 'all';
    asked.push(all ? `${id}?opt.all=all` : id);
    route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify(
        all
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

  // (#1911) `mission-status-all` is no longer a base panel id (see
  // `PANEL_IDS` in `ui/src/lib/route.ts`) — `panelSwitchId` (`ansi.tsx`)
  // recognizes a link's raw `panel=` value against that closed set BEFORE
  // any alias resolution, so this link no longer matches and the JS
  // in-page-switch shortcut does not fire. The browser instead follows the
  // anchor's own hash-only `href` natively — a real navigation, which then
  // reaches `parseRoute`'s alias table (`PANEL_ALIASES`) and resolves to
  // `mission-status` + `opt.all=all`; the fetch below proves it landed.
  expect(asked).toContain('mission-status?opt.all=all');

  // Same known harness gap `next-parity-console.spec.ts`'s "the REAL
  // in-corpus OSC-8 deep link" test already documents for the SAME
  // recorded link (verbatim comment there): `panel_deep_link` bakes an
  // ABSOLUTE daemon URL whose pathname is "/" (the console is served at
  // root in real production, `GET /`). In production the operator is
  // ALREADY on that same root path, so following the href is a genuine
  // same-document hash-only change. THIS harness serves the fixture at
  // `/index-lab.html`, a path the recorded link was never baked against —
  // a harness artifact, not a production behavior, and the reason this
  // assertion checks the PATHNAME actually reached (root, matching the
  // link) rather than asserting it stayed put. Before #1911 this never
  // surfaced because the JS switch intercepted the click before the real
  // href was ever followed at all.
  expect(new URL(page.url()).pathname).toBe('/');
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
  await expect(page.locator('[data-act="setpanel"][data-arg="mission-status"]')).toHaveClass(/\bon\b/);
  await expect(page.getByRole('switch', { name: '--all' })).toHaveAttribute('aria-checked', 'true');

  // (#1911) `mission-status-all` folded into `mission-status`'s own `--all`
  // opt — `parseRoute` resolves the alias synchronously at first parse (see
  // `route.ts::PANEL_ALIASES`), so the panel renders correctly straight
  // off this boot with no extra round trip. The address bar then upgrades
  // to the canonical `opt.*` form — the SAME `#lens=lab` ->
  // `#lens=runs&kind=lab` upgrade path this app already used, applied here.
  await expect.poll(() => page.url()).toContain('panel=mission-status&');
  expect(page.url()).toContain('opt.all=all');
  expect(page.url()).not.toContain('mission-status-all');
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
