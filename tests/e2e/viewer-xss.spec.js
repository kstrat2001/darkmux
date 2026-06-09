// Headless e2e gate for the observability viewer's output-encoding hardening
// (replaces the manual "open /play/<date> and check window.__xss" walkthrough).
//
// The served harness is the canonical viewer loading tests/fixtures/xss-flow.jsonl
// — a valid flow-schema day file whose every string field carries an
// HTML-injection payload (`<img src=x onerror=window.__xss=1>`), a JS-string
// breakout (`'); window.__xss=1;//`), and a double-quoted-attribute breakout
// (`" onmouseover=window.__xss=1 x="`). If any field reaches the DOM unescaped,
// one of those fires `window.__xss` or injects a live element. We drill through
// every render path and assert it never does.
const { test, expect } = require('@playwright/test');

async function assertInert(page, where) {
  // The canary: no payload executed in any context.
  const fired = await page.evaluate(() => window.__xss);
  expect(fired, `XSS canary fired at: ${where}`).toBeUndefined();
  // And no attacker <img>/<svg> was parsed into the live DOM (an escaped
  // payload renders as text, never as an element with the malicious src).
  const injected = await page.evaluate(
    () => document.querySelectorAll('img[src="x"], img[onerror], [onmouseover]').length
  );
  expect(injected, `injected element rendered at: ${where}`).toBe(0);
}

test('viewer renders attacker-controlled flow records inertly across every view', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index.html');

  // boot() is async (fetches + parses the flow file) — wait for the fleet to render.
  await page.waitForSelector('[data-act="machine"]', { timeout: 15_000 });
  await assertInert(page, 'fleet');

  // Drill into a machine (its name/spec come from attacker-controlled fields).
  await page.locator('[data-act="machine"]').first().click();
  await page.waitForSelector('.stagehdr');
  await assertInert(page, 'machine');

  // Expand a recent-run row if the live/playback split surfaced one.
  const rr = page.locator('details.rr').first();
  if (await rr.count()) {
    await rr.locator('summary').click();
    await assertInert(page, 'recent-run expanded');
  }

  // Drill into a session subsystem (handle/model/detector text rendered there).
  const sess = page.locator('[data-act="session"]').first();
  if (await sess.count()) {
    await sess.click();
    await page.waitForSelector('.sub');
    await assertInert(page, 'subsystem');
  }

  // Mission view (mission_id + per-dispatch role/machine/model rows).
  const miss = page.locator('[data-act="mission"]').first();
  if (await miss.count()) {
    await miss.click();
    await assertInert(page, 'mission');
  }

  // Filters modal renders the record-derived category/tier/source values.
  await page.locator('[data-act="filters"]').click();
  await assertInert(page, 'filters modal');

  // Full-text search forces the event log to render every matching record's fields.
  const search = page.locator('#fsearch');
  if (await search.count()) {
    await search.fill('img');
    await page.waitForTimeout(100);
    await assertInert(page, 'log search');
  }

  // No uncaught page errors anywhere in the walk (a broken handler / parse would surface here).
  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});

test('the harness actually loaded the malicious fixture (guards against a no-op pass)', async ({ page }) => {
  // If the fixture failed to load, the walk above would trivially pass against an
  // empty viewer. Assert the records are present so the inertness check means something.
  await page.goto('/index.html');
  await page.waitForSelector('[data-act="machine"]', { timeout: 15_000 });
  const records = await page.evaluate(() => (typeof DATA !== 'undefined' ? DATA.length : 0));
  expect(records, 'fixture did not load — inertness assertions would be vacuous').toBeGreaterThan(8);
});
