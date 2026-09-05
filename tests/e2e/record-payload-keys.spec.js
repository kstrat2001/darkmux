// The record PAYLOAD panel's key column, measured rather than read (M1, C1).
//
// jsdom applies no stylesheets and performs no layout, so a unit test can
// assert a rule was typed but never what the column actually does to a long
// field name. Both findings here are cascade bugs — a declaration that is
// present and correct, defeated by another one the author did not revisit:
//
//   M1  #2367 added `max-width: 45%` to `.rv__key` for the desktop clip. The
//       phone block (`@media (max-width: 768px)`) overrides `width` and
//       `flex` to stack label-over-value (#2108) but NOT `max-width`, so on a
//       phone `flex: 1 1 100%` resolved against a 45% cap: the key stayed
//       beside its value and `session id` wrapped mid-token.
//   C1  on desktop `flex: 0 0 auto; min-width: 116px` lets a key BETWEEN
//       116px and the 45% cap set its own column width — `cpu speed limit
//       pct` measured 148px, putting its value at +158 while every neighbor
//       sat at +126. The column is supposed to be a column.
//
// A SYNTHETIC panel rather than a sampled record: the assertion is about the
// stylesheet's geometry for a known set of key lengths, and picking a real
// record whose payload happens to carry both a short and an over-116px key
// makes the test hostage to fixture data. The classes are the ones
// `components/RecordView.tsx` renders (`.rv` > `.rv__row` > `.rv__key` +
// `.rv__val`).
const { test, expect } = require('@playwright/test');

const KEYS = ['role', 'session id', 'cpu speed limit pct', 'inactivity timeout seconds'];

/** Mount a `.rv` panel of {@link KEYS} at a known width and report, per row,
 * the key's width and the value's x-offset from the row's left edge. */
async function measure(page, panelWidth) {
  return page.evaluate(
    ({ keys, panelWidth }) => {
      document.querySelectorAll('#rvprobe').forEach((n) => n.remove());
      const host = document.createElement('div');
      host.id = 'rvprobe';
      host.style.cssText = `position:fixed;left:0;top:0;width:${panelWidth}px;z-index:99999;background:#000`;
      host.innerHTML =
        '<div class="rv">' +
        keys
          .map((k) => `<div class="rv__row"><span class="rv__key">${k}</span><span class="rv__val"><span class="rv__str">value-${k.length}</span></span></div>`)
          .join('') +
        '</div>';
      document.body.appendChild(host);
      const rows = [...host.querySelectorAll('.rv__row')];
      return rows.map((row) => {
        const rb = row.getBoundingClientRect();
        const k = row.querySelector('.rv__key').getBoundingClientRect();
        const v = row.querySelector('.rv__val').getBoundingClientRect();
        return {
          key: row.querySelector('.rv__key').textContent,
          rowWidth: Math.round(rb.width),
          keyWidth: Math.round(k.width),
          keyBottom: Math.round(k.bottom - rb.top),
          valLeft: Math.round(v.left - rb.left),
          valTop: Math.round(v.top - rb.top),
        };
      });
    },
    { keys: KEYS, panelWidth },
  );
}

test.describe('record payload key column', () => {
  test.describe('phone (M1: the key must STACK above its value)', () => {
    test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

    test('every key takes the full row and its value starts on the line below', async ({ page }) => {
      await page.goto('/index.html');
      await page.waitForSelector('.app-shell');
      // 358px ~ the phone drawer's own content width at 390 minus its padding.
      const rows = await measure(page, 358);
      for (const r of rows) {
        expect(r.keyWidth, `key "${r.key}" must span the row (it is flex: 1 1 100%)`).toBe(r.rowWidth);
        expect(r.valLeft, `value after "${r.key}" must start at the row's left edge, not beside the key`).toBe(0);
        expect(r.valTop, `value after "${r.key}" must sit BELOW the key`).toBeGreaterThanOrEqual(r.keyBottom);
      }
    });
  });

  test.describe('desktop (C1: one column, whatever the key length)', () => {
    test.use({ viewport: { width: 1456, height: 900 } });

    test('a long key wraps INSIDE the 116px column instead of widening it', async ({ page }) => {
      await page.goto('/index.html');
      await page.waitForSelector('.app-shell');
      const rows = await measure(page, 520);
      const offsets = new Set(rows.map((r) => r.valLeft));
      expect(
        [...offsets],
        `every value must start at the same x — measured ${JSON.stringify(rows.map((r) => [r.key, r.valLeft]))}`,
      ).toHaveLength(1);
      for (const r of rows) {
        expect(r.keyWidth, `key "${r.key}" must hold the 116px column`).toBe(116);
      }
    });
  });
});
