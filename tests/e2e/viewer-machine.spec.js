// Headless e2e smoke for the #1286 machine memory lens — the live
// potential-vs-current ledger fed by the daemon's /machine/resources. The
// static harness has no daemon behind it, so these specs route-mock the
// endpoint (the catalog.spec pattern) with a real-shaped ledger payload
// whose every string field carries the standard XSS payloads — the machine
// lens renders daemon-supplied text (model identifiers, shrink hints,
// attribution notes, warnings), so it rides the same output-encoding gate
// as every other view.
const { test, expect } = require('@playwright/test');

const XSS = `<img src=x onerror=window.__xss=1>`;

// Real-shaped /machine/resources payload (the ModelLedger JSON of
// crates/darkmux-profiles/src/model_ledger.rs) with hostile strings in every
// field the lens interpolates into HTML.
const LEDGER = {
  schema_version: '1.0',
  generated_at_ms: 1767225600000,
  gather_ms: 42,
  cache_ttl_ms: 2000,
  limit_bytes: 137438953472,
  limit_source: 'physical_pool',
  pool: { capacity_bytes: 137438953472, available_bytes: 3738599424 },
  pressure: {
    swap_used_bytes: 0,
    compressor_bytes: 2000000000,
    memory_free_percent: 43,
    red: false,
  },
  models: [
    {
      identifier: `darkmux:judge ${XSS}`,
      model_key: `judge ${XSS}`,
      owner: 'darkmux',
      loaded_ctx: 65536,
      weights_bytes: 17180000000,
      kv_per_token_bytes: 20480,
      kv_bytes_at_ctx: 1342177280,
      potential_bytes: 19272177280,
      current_bytes: 18000000000,
      state: 'amber',
      shrink_hint: `reload judge at ctx 32768 ${XSS}`,
    },
    {
      identifier: `devstral ${XSS}`,
      model_key: `devstral ${XSS}`,
      owner: 'user',
      loaded_ctx: 32768,
      weights_bytes: 13000000000,
      kv_per_token_bytes: 163840,
      kv_bytes_at_ctx: 5368709120,
      potential_bytes: 19118709120,
      current_bytes: 15000000000,
      // A hostile state string must degrade to the "unknown" class, never
      // land raw inside a class attribute.
      state: `red" onmouseover=window.__xss=1 x="`,
    },
  ],
  machine: {
    potential_bytes: 38390886400,
    unpriced_models: 0,
    current_bytes: 33000000000,
    state: 'amber',
    shrink_hint: `shrink several contexts ${XSS}`,
  },
  attribution: 'per_process',
  attribution_note: `2 worker(s) rank-matched ${XSS}`,
  warnings: [`probe degraded ${XSS}`],
};

function mockMachineMemory(page, body) {
  return page.route('**/machine/resources*', (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(body ?? LEDGER) })
  );
}

async function assertInert(page, where) {
  const fired = await page.evaluate(() => window.__xss);
  expect(fired, `XSS canary fired at: ${where}`).toBeUndefined();
  const injected = await page.evaluate(
    () => document.querySelectorAll('img[src="x"], img[onerror], [onmouseover]').length
  );
  expect(injected, `injected element rendered at: ${where}`).toBe(0);
}

// (port gap, reported not papered over — applies to all three tests below)
// This whole file exercises legacy's `/machine/resources` memory-LEDGER UI
// (`.memcard`/`.membar`/`.pot`/`.cur`/`.lim`/`.memhint`/`#memstamp`/`.memwarn`
// — per-model capacity bars, a pressure card, a stale-data banner) — the
// port's `MachineLens` (`ui/src/lenses/machine/MachineLens.tsx`) has never
// built this. It fetches the SAME `/machine/resources` endpoint (see
// `resourcesQuery` in that file) but renders it as plain classified text
// lines (`healthLines()`/`memoryLedgerLines.ts`: hint/warn/state/owner/meta
// lines styled by content pattern, not bars) — a genuinely different,
// simpler design for the same data, not a renamed selector set. There is
// no `.memcard`, no `.membar`, no `#memstamp`, no `.memwarn` anywhere in
// the port.
//
// This means the escaping coverage this file provides — every string
// field of a REAL `ModelLedger` payload (model identifiers, shrink hints,
// attribution notes, warnings, a hostile `state` string) rendered through
// the daemon's own memory-pressure wire shape — has NO port-side
// equivalent walk today. `healthLines()`'s output DOES get escaped the
// same way every other record-derived text in this app does (React's
// default text-node escaping, same mechanism the passing `viewer-xss.spec.js`
// tests already prove for other views), but that claim has not been
// walked against THIS SPECIFIC payload shape and its own field set — kept
// here verbatim (fixme, not deleted) as the tracked record of what a
// ledger-bars implementation (or a dedicated port-shaped equivalent test
// against `healthLines()`) needs to satisfy.
test.fixme('machine lens renders the ledger inertly — bars, states, hints, pressure', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await mockMachineMemory(page);

  await page.goto('/index.html');
  await page.waitForSelector('#lens-machine');

  await page.click('#lens-machine');
  await expect(page.locator('#lens-machine')).toHaveClass(/\bon\b/);
  await expect(page.locator('#lens-fleet')).not.toHaveClass(/\bon\b/);
  // (#1286 deep-link) Lens navigation reflects into the address bar.
  await expect.poll(() => page.evaluate(() => location.hash)).toContain('lens=machine');

  // Machine total + 2 model cards + pressure card render off the payload.
  await page.waitForSelector('.memcard');
  await expect(page.locator('.memcard .memname').first()).toHaveText('machine total');
  expect(await page.locator('.memcard').count()).toBeGreaterThanOrEqual(4);
  // Current fill INSIDE the potential outline, plus the limit tick.
  expect(await page.locator('.membar .pot').count()).toBeGreaterThanOrEqual(3);
  expect(await page.locator('.membar .cur').count()).toBeGreaterThanOrEqual(3);
  expect(await page.locator('.membar .lim').count()).toBeGreaterThanOrEqual(1);
  // The amber "made it by luck" shrink hint renders (escaped).
  await expect(page.locator('.memhint').first()).toContainText('shrink several contexts');
  // Observer-cost stamp line (#1286 constraint 3) is visible.
  await expect(page.locator('#memstamp')).toContainText('gather 42 ms');
  // The hostile per-model state string degraded to the unknown class.
  expect(await page.locator('.membar .cur.unknown').count()).toBe(1);

  await assertInert(page, 'machine lens');
  expect(pageErrors, `page errors: ${pageErrors.join('\n')}`).toEqual([]);

  // Leaving the lens clears its hash param and re-activates fleet.
  await page.click('#lens-fleet');
  await expect(page.locator('#lens-fleet')).toHaveClass(/\bon\b/);
  await expect.poll(() => page.evaluate(() => location.hash)).not.toContain('lens=machine');
});

test.fixme('deep link #lens=machine boots directly into the machine lens', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await mockMachineMemory(page);

  await page.goto('/index.html#lens=machine');
  // No tab click — boot itself must land in the machine lens.
  await expect(page.locator('#lens-machine')).toHaveClass(/\bon\b/);
  await page.waitForSelector('.memcard');
  await expect(page.locator('.memcard .memname').first()).toHaveText('machine total');
  await assertInert(page, 'machine lens deep link');
  expect(pageErrors, `page errors: ${pageErrors.join('\n')}`).toEqual([]);
});

test.fixme('unreachable daemon shows the no-daemon notice, then a stale banner once data existed', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  // No route mock: the static harness 404s /machine/resources → the lens must
  // say so instead of rendering nothing.
  await page.goto('/index.html#lens=machine');
  await expect(page.locator('#lens-machine')).toHaveClass(/\bon\b/);
  await expect(page.locator('.none')).toContainText('daemon not reachable');

  // Daemon comes up: the next poll paints the ledger.
  await mockMachineMemory(page);
  await page.waitForSelector('.memcard', { timeout: 10_000 });

  // Daemon goes away again: the cached snapshot stays BUT is labeled stale —
  // a silently frozen gauge is the failure mode this banner prevents.
  await page.unroute('**/machine/resources*');
  await expect(page.locator('.memwarn').first()).toContainText('stale', { timeout: 10_000 });
  await expect(page.locator('.memcard .memname').first()).toHaveText('machine total');

  expect(pageErrors, `page errors: ${pageErrors.join('\n')}`).toEqual([]);
});
