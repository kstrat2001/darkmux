// (#1639) The session drill-down is a destination; destinations have URLs.
//
// This was the ONE level with no URL representation. Drilling in left
// `location.hash` untouched, so a reload — or a backgrounded PWA relaunch, or
// a tab reopened from history — silently dropped the operator back to the
// fleet hero with no error and no explanation. The documented `#session=<id>`
// deep link had the mirror problem: `catalogQuery()` parsed it, the daemon
// returned correctly-scoped records, and `boot()` rendered the fleet hero
// anyway because only the `mission` kind was ever honored.
//
// Both halves are asserted here, in the order an operator hits them: drill
// writes the URL, boot reads it back, and navigating away clears it.
const { test, expect } = require('@playwright/test');

async function boot(page, hash) {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.goto(`/index-lab.html${hash || ''}`);
  await page.waitForFunction(() => typeof window.drillSession === 'function');
  return errors;
}

// Boot-time deep links need the DAEMON shape: `boot()` computes
// `cq = flowSrc ? null : catalogQuery()`, so any page pinning a static flow
// source can never reach the `#session=` branch. `index-live.html` carries no
// flow-src (see playwright.config.js) and its fetches are stubbed here.
async function bootLive(page, hash) {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.route(/\/flow\/\d{4}-\d{2}-\d{2}(\?.*)?$/, (r) =>
    r.fulfill({ contentType: 'application/json', body: '[]' })
  );
  await page.route(/\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/, (r) =>
    r.fulfill({ contentType: 'text/event-stream', body: '' })
  );
  await page.route('**/runs*', (r) =>
    r.fulfill({ contentType: 'application/json', body: '{"generated_at_ms":0,"runs":[]}' })
  );
  await page.goto(`/index-live.html${hash || ''}`);
  await page.waitForFunction(() => typeof window.drillSession === 'function');
  return errors;
}

const hash = (page) => page.evaluate(() => location.hash);
const level = (page) => page.evaluate(() => typeof state !== "undefined" && state.level);
const session = (page) => page.evaluate(() => typeof state !== "undefined" && state.session);

test('drilling into a session writes it to the address bar', async ({ page }) => {
  const errors = await boot(page);
  await page.evaluate(() => window.drillSession('sess-in-flight'));
  expect(
    await hash(page),
    'the session level must be addressable — this is what a reload reads back'
  ).toContain('session=sess-in-flight');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('booting on #session= restores the session view, not the fleet hero', async ({ page }) => {
  const errors = await bootLive(page, '#session=sess-in-flight');
  expect(await level(page), 'the URL named a session; boot must honor it').toBe('subsystem');
  expect(await session(page)).toBe('sess-in-flight');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('a drill survives a reload — the whole point', async ({ page }) => {
  // The end-to-end path the issue describes: click in, Cmd+R, still there.
  // Asserted as one flow rather than trusting that the two halves compose,
  // because the halves live in different functions and nothing else pins
  // that the string one writes is the string the other parses.
  const errors = await bootLive(page);
  await page.evaluate(() => window.drillSession('sess-reload'));
  await page.reload();
  await page.waitForFunction(() => typeof window.drillSession === 'function');
  expect(await level(page), 'reload must not silently reset to the fleet hero').toBe('subsystem');
  expect(await session(page)).toBe('sess-reload');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('navigating up out of a session clears it from the URL', async ({ page }) => {
  // Without this the id lingers, and the NEXT reload drops the operator back
  // into a view they deliberately left — the same failure, inverted.
  const errors = await boot(page);
  await page.evaluate(() => window.drillSession('sess-leaving'));
  expect(await hash(page)).toContain('session=sess-leaving');
  await page.evaluate(() => window.goRuns && window.goRuns());
  expect(
    await hash(page),
    'a stale session id must not survive navigating to another lens'
  ).not.toContain('sess-leaving');
  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});
