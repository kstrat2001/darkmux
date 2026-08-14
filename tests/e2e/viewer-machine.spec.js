// Headless e2e smoke for the #1286 machine memory lens — the live
// potential-vs-current ledger fed by the daemon's /machine/resources. These
// specs route-mock the endpoint against the DAEMON-shaped harness
// (`/index-live.html` — see the note above the first `test()` below for why
// that variant, not `/index.html`) with a real-shaped ledger payload whose
// every string field carries the standard XSS payloads — the machine lens
// renders daemon-supplied text (model identifiers, shrink hints,
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

// (#1806 Stage 1 — two of three un-fixme'd) This whole file exercises the
// daemon's `/machine/resources` memory-LEDGER UI (`.memcard`/`.membar`/
// `.pot`/`.cur`/`.lim`/`.memhint`/`#memstamp`/`.memwarn` — per-model
// capacity bars, a pressure card, a stale-data banner). Until #1806 Stage 1
// (`MemLedgerCards.tsx`), the port's `MachineLens` fetched the SAME
// `/machine/resources` endpoint but rendered it as plain classified text
// lines, with no `.memcard`/`.membar`/`#memstamp`/`.memwarn` anywhere — all
// three tests were `test.fixme`'d for exactly that gap. Stage 1 restored the
// structure (legacy's own class names, on purpose) WITHOUT changing any
// rendered text, so the escaping coverage this file provides — every string
// field of a REAL `ModelLedger` payload (model identifiers, shrink hints,
// attribution notes, warnings, a hostile `state` string) rendered through
// the daemon's own memory-pressure wire shape — now has a real port-side
// walk, for the two tests below that un-fixme'd clean: React's default
// text-node escaping (the same mechanism `viewer-xss.spec.js` already
// proves for other views) against THIS specific payload shape, exercised
// end-to-end rather than asserted only in the abstract. The THIRD test
// (`'unreachable daemon shows...'`) stayed `test.fixme` — see the comment
// directly above it for why: it found a real, separate, pre-existing bug
// in `MachineLens.tsx`'s query-derivation layer, not a structure gap.
//
// Navigates via `/index-live.html` (`playwright.config.js`'s daemon-shaped
// harness — no injected `darkmux-flow-src`), not `/index.html` (the
// static-playback harness every OTHER test in this file's original draft
// copied the `catalog.spec.js` pattern from). The distinction matters here
// specifically: `MachineLens.tsx`'s `daemonBacked` gate
// (`!isStaticBuild()`, #1801) reads the SAME `darkmux-flow-src` meta
// `/index.html` injects to suppress polling on the daemon-less marketing
// demo — a signal legacy never had. Under that gate the machine lens's own
// `/machine/resources` fetch never fires at all, so `page.route()`-mocking
// it against `/index.html` left the lens stuck on its "loading…" placeholder
// forever (verified: `.memcard` never appeared, 30s timeout). Every other
// spec in this suite that route-mocks a live daemon already picks the
// matching non-static harness variant (`index-daemon.html`/`index-lab.html`/
// `index-live.html` — see this repo's own `playwright.config.js`); this file
// is brought in line with that convention.
test('machine lens renders the ledger inertly — bars, states, hints, pressure', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await mockMachineMemory(page);

  await page.goto('/index-live.html');
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

test('deep link #lens=machine boots directly into the machine lens', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await mockMachineMemory(page);

  await page.goto('/index-live.html#lens=machine');
  // No tab click — boot itself must land in the machine lens.
  await expect(page.locator('#lens-machine')).toHaveClass(/\bon\b/);
  await page.waitForSelector('.memcard');
  await expect(page.locator('.memcard .memname').first()).toHaveText('machine total');
  await assertInert(page, 'machine lens deep link');
  expect(pageErrors, `page errors: ${pageErrors.join('\n')}`).toEqual([]);
});

// STILL fixme — NOT a structure gap. #1806 Stage 1's un-fixme pass (the two
// tests above) found the first two thirds of this test genuinely pass
// against the new `.memcard`/`.membar`/`#memstamp` structure. This third
// exposed something else: a pre-existing bug in `MachineLens.tsx`'s query
// derivation (lines computing `resources`/`resourcesErrored` from
// `resourcesQuery.data`, untouched by Stage 1), unrelated to card markup.
//
// `resources` is derived as `resourcesQuery.data?.ok ? resourcesQuery.data.data
// : null` — so the MOMENT a poll after a successful one resolves with
// `{ok:false}` (a real 404, same shape `fetchJson` always returns, never a
// thrown error), `resources` flips straight to `null`. The intended
// behavior — stated explicitly in the retired `healthLines()`'s own comment,
// carried into `MemLedgerCards.tsx`'s `resourcesErrored && !resources` vs
// the stale-banner branch — is to keep showing the LAST GOOD snapshot with a
// "stale" banner layered on top; the actual derivation throws that snapshot
// away instead, so the page falls into the "daemon not reachable" `.none`
// placeholder a poll cycle later, never reaching the stale banner at all.
// Reproduced directly (RTL + a fetch mock that succeeds once then 404s):
// `resources` becomes `null` and `data-state` reads `"error"` on the very
// next poll, confirming this is the query layer, not anything Stage 1 built.
//
// Left `test.fixme` rather than un-fixme'd-with-changed-assertions (this
// spec's own STALE-banner assertion is correct against the intended design;
// weakening it to match the bug would hide a real defect) and rather than
// silently fixed (out of #1806 Stage 1's structural-restoration scope — see
// this packet's own report for the finding, named for a follow-up fix to
// `MachineLens.tsx`'s `resources`/`resourcesErrored` derivation, e.g. via
// `placeholderData`/keeping the last `ok:true` payload across an error poll).
// Tracked as #1812 — the fix restores legacy's keep-last-payload shape;
// this spec already asserts the correct behavior, so it IS the regression
// test rather than something to write afterward.
test.fixme('unreachable daemon shows the no-daemon notice, then a stale banner once data existed', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  // No route mock: the static harness 404s /machine/resources → the lens must
  // say so instead of rendering nothing.
  await page.goto('/index-live.html#lens=machine');
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
