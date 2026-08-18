// Headless e2e smoke for the #1286 machine memory lens — the live
// potential-vs-current ledger fed by the daemon's /machine/resources. These
// specs route-mock the endpoint against the DAEMON-shaped harness
// (`/index-live.html` — see the note above the first `test()` below for why
// that variant, not `/index.html`) with a real-shaped ledger payload whose
// every string field carries the standard XSS payloads — the machine lens
// renders daemon-supplied text (model identifiers, shrink hints,
// attribution notes, warnings), so it rides the same output-encoding gate
// as every other view.
//
// (#1806 Stage 2/3 — the machine-lens redesign, docs/design/machine-lens/proposal.md in the design
// packet) This file was rewritten for the new markup: a bezel-less
// semicircle gauge (`.mm-gauge`), a tell-tale lamp row (`.mm-lamps`), the
// odometer tiles (`.mm-odo`), and model rows (`.mm-row`) replace Stage 1's
// `.memcard`/`.membar` ledger. The INTENT of every assertion below is
// unchanged from before the rewrite — every hostile string in the payload
// still has to render inertly, the observer-cost stamp is still visible,
// the shrink hint is still visible — only the SELECTORS moved to match the
// real structure. `MachineHealthRegion.test.tsx` covers the same honesty
// rules (absence vs zero, hostile-state degrade) at the component level
// without a browser; this file's job is the end-to-end XSS walk plus a few
// structural landmarks a component test can't see (the tab/hash wiring, a
// real `#stage` render).
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
  pool: { capacity_bytes: 137438953472, used_bytes: 69300000000, available_bytes: 63000000000, free_bytes: 3738599424 },
  pressure: {
    swap_used_bytes: 0,
    compressor_bytes: 2000000000,
    margin_percent: 43,
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
  // #1821: `warnings: string[]` -> `messages: [{severity, text}]`. This
  // fixture's hostile string carries `warn` severity so the existing
  // `.memmsg-warn` assertion below still exercises the XSS-inertness path.
  messages: [{ severity: 'warn', text: `probe degraded ${XSS}` }],
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

// Navigates via `/index-live.html` (`playwright.config.js`'s daemon-shaped
// harness — no injected `darkmux-flow-src`), not `/index.html` (the
// static-playback harness). The distinction matters here specifically:
// `MachineLens.tsx`'s `daemonBacked` gate (`!isStaticBuild()`, #1801) reads
// the SAME `darkmux-flow-src` meta `/index.html` injects to suppress
// polling on the daemon-less marketing demo — a signal legacy never had.
// Under that gate the machine lens's own `/machine/resources` fetch never
// fires at all, so `page.route()`-mocking it against `/index.html` left the
// lens stuck on its "loading…" placeholder forever.
test('machine lens renders the ledger inertly — gauge, lamps, odometer, rows, hints, pressure', async ({ page }) => {
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

  // The hero gauge renders off the payload.
  await page.waitForSelector('.mm-gauge');
  // The dial renders off the payload, and asserts NOTHING about it. The
  // `machine total GREEN` chip that stood here interpreted data the reader
  // can already see; the arc's color is now a ramp fixed to the dial, so a
  // severity class on the band would be a regression to a verdict.
  await expect(page.locator('.mm-gauge-val')).toHaveAttribute('stroke', 'url(#mm-gauge-ramp)');
  expect(await page.locator('.mm-gcap').count()).toBe(0);
  expect(await page.locator('.mm-chip').count()).toBe(0);
  // Two model rows, grouped darkmux-first (LEDGER's judge=darkmux,
  // devstral=user).
  expect(await page.locator('.mm-row').count()).toBe(2);
  await expect(page.locator('.mm-grouphdr').first()).toContainText('DARKMUX-MANAGED');
  await expect(page.locator('.mm-grouphdr').nth(1)).toContainText('USER-LOADED');
  // Both models are PRICED in this fixture — both rows draw a committed
  // (`.mm-row-pot`) layer, plus their current fill.
  expect(await page.locator('.mm-row-pot').count()).toBe(2);
  expect(await page.locator('.mm-row-cur').count()).toBe(2);
  // The tell-tale lamp row and odometer tiles render, unconditionally.
  // SIX lamps, not seven: the STATE lamp is gone. It relabelled itself with
  // the machine state AND changed its lit-ness, so a healthy machine showed
  // the word "GREEN" in gray beside the same word in green on the machine
  // chip. That chip has since been removed outright, which is what makes the
  // lamp row the ONLY channel left here — and every lamp in it keys on a
  // server-declared CONDITION (pressure, over-limit, unpriced), never on an
  // assessment of whether the machine is doing well.
  expect(await page.locator('.mm-lamp').count()).toBe(6);
  expect(await page.locator('.mm-lamp').filter({ hasText: /^STATE/ })).toHaveCount(0);
  expect(await page.locator('.mm-odo').count()).toBe(3);
  // The MACHINE's own shrink hint renders (escaped) — it sits ABOVE the
  // model rows (right after the machine k/v detail row it's a footnote to).
  await expect(page.locator('.mm-hint').first()).toContainText('shrink several contexts');
  // The per-model shrink hint (judge's row) also renders (escaped) —
  // distinct from the machine-level one above.
  await expect(page.locator('.mm-hint').last()).toContainText('reload judge at ctx 32768');
  // Observer-cost stamp line (#1286 constraint 3) is visible.
  await expect(page.locator('#memstamp')).toContainText('gather 42 ms');
  // The hostile per-model state string degraded to the unknown class.
  expect(await page.locator('.mm-row-cur.is-unknown').count()).toBe(1);
  // The message card still renders full text, with severity-keyed styling
  // (#1821 — `.memmsg-warn`, not the old uniformly-amber `.memwarn`).
  await expect(page.locator('.memmsg-warn').first()).toContainText('probe degraded');

  await assertInert(page, 'machine lens');
  expect(pageErrors, `page errors: ${pageErrors.join('\n')}`).toEqual([]);

  // Leaving the lens clears its hash param and re-activates fleet.
  await page.click('#lens-fleet');
  await expect(page.locator('#lens-fleet')).toHaveClass(/\bon\b/);
  await expect.poll(() => page.evaluate(() => location.hash)).not.toContain('lens=machine');
});

// The one thing jsdom cannot check: that the red state actually REACHES the
// seven-segment glyphs. They are `<polygon fill="currentColor">`, so the
// `.mm-gauge-center-val.lit { color: var(--bad) }` rule is what reddens them.
// The rule that stood there before targeted a `<text>` element the
// seven-segment rewrite deleted, and the readout silently stayed white on a
// red machine — a regression only a real cascade can catch, hence Playwright.
test('a red machine reddens the seven-segment readout itself, not just its glow', async ({ page }) => {
  const readoutFill = async () =>
    page.evaluate(() => {
      const poly = document.querySelector('.mm-gauge-center-val polygon');
      const probe = document.createElement('span');
      probe.style.color = 'var(--bad)';
      document.body.appendChild(probe);
      const bad = getComputedStyle(probe).color;
      probe.remove();
      return { fill: poly ? getComputedStyle(poly).fill : null, bad };
    });

  // Green first: the readout is NOT the redline color.
  await mockMachineMemory(page, { ...LEDGER, machine: { ...LEDGER.machine, state: 'green' } });
  await page.goto('/index-live.html');
  await page.waitForSelector('#lens-machine');
  await page.click('#lens-machine');
  await page.waitForSelector('.mm-gauge-center-val polygon');
  const green = await readoutFill();
  expect(green.fill).not.toBeNull();
  expect(green.fill).not.toBe(green.bad);
  expect(await page.locator('.mm-gauge-center-val.lit').count()).toBe(0);

  // Then red: the SAME polygons resolve `currentColor` to `--bad`.
  await page.unroute('**/machine/resources*');
  await mockMachineMemory(page, { ...LEDGER, machine: { ...LEDGER.machine, state: 'red' } });
  await page.waitForSelector('.mm-gauge-center-val.lit', { timeout: 15000 });
  const red = await readoutFill();
  expect(red.fill).toBe(red.bad);
});

test('deep link #lens=machine boots directly into the machine lens', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await mockMachineMemory(page);

  await page.goto('/index-live.html#lens=machine');
  // No tab click — boot itself must land in the machine lens.
  await expect(page.locator('#lens-machine')).toHaveClass(/\bon\b/);
  await page.waitForSelector('.mm-gauge');
  await expect(page.locator('.mm-kv--machine')).toContainText('limit source');
  await assertInert(page, 'machine lens deep link');
  expect(pageErrors, `page errors: ${pageErrors.join('\n')}`).toEqual([]);
});

// (#1812 — un-fixme'd) This test used to be `test.fixme` with a comment
// naming the exact bug it was blocked on: `MachineLens.tsx` derived
// `resources` fresh from the query's raw data every render, so the MOMENT a
// poll after a successful one resolved with `{ok:false}` (a real 404, the
// same shape `fetchJson` always returns, never a thrown error), `resources`
// flipped straight to `null` — discarding the last good snapshot the stale
// banner exists to sit over. The fix (`MachineLens.tsx`) holds the last-good
// payload in state, updated ONLY on `ok:true`. This spec already asserted
// the correct (legacy-matching) behavior before the fix landed — it IS the
// regression test, not something written after.
test('unreachable daemon shows the no-daemon notice, then a stale banner once data existed', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  // No route mock: the static harness 404s /machine/resources → the lens must
  // say so instead of rendering nothing.
  await page.goto('/index-live.html#lens=machine');
  await expect(page.locator('#lens-machine')).toHaveClass(/\bon\b/);
  await expect(page.locator('.none')).toContainText('daemon not reachable');

  // Daemon comes up: the next poll paints the ledger.
  await mockMachineMemory(page);
  await page.waitForSelector('.mm-gauge', { timeout: 10_000 });

  // Daemon goes away again: the cached snapshot stays BUT is labeled
  // stale — a silently frozen gauge is the failure mode this banner
  // prevents. The reading itself must still be on screen, not blanked — and
  // it is the MACHINE's used memory (pool.used_bytes 69.3e9 = 64.5 GiB), the
  // figure the needle points at, NOT darkmux's own share. Those were two
  // different subjects on one instrument until #1821 made the readout follow
  // the needle.
  await page.unroute('**/machine/resources*');
  await expect(page.locator('.mm-stalebanner').first()).toContainText('stale', { timeout: 10_000 });
  await expect(page.locator('.mm-hero')).toHaveClass(/is-stale/);
  // The reading survives the stale poll — never blanked. It is drawn as
  // seven-segment polygons and so carries no text; the gauge's own aria
  // narrative states the same figure, which is the stronger place to pin it
  // because the two must stay in step.
  await expect(page.locator('.mm-gauge svg[role="img"]')).toHaveAttribute('aria-label', /64\.5/);
  expect(await page.locator('.mm-gauge-center-val .mm-gauge-odo-cell').count()).toBeGreaterThan(0);

  expect(pageErrors, `page errors: ${pageErrors.join('\n')}`).toEqual([]);
});
