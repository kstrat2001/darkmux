// Phone tap targets, HIT-TESTED rather than eyeballed (U1-1, U1-2, U1-4).
//
// Same reason `chrome-order.spec.js` and `event-log-chrome.spec.js` exist:
// jsdom applies no stylesheets and performs no layout, so a unit test can
// assert that a 44px rule was TYPED but never that the region it describes
// survives its ancestors' clipping and painting. U1-2 is exactly that gap —
// the `.nav-tab::after` extension had been in the sheet for months while
// `overflow: hidden` on `.app-shell__navtabs` amputated it, so the top of
// every tab's advertised target was dead.
//
// The three findings, all measured on an iPhone-14-shaped viewport:
//   U1-1  `.mm-odo-i` (the machine odometer's "(i)") was 14x14 with
//         `padding: 0`, while its own CSS comment claimed "the padding does
//         the touch work".
//   U1-2  a point 4px above the tab strip resolved to `.app-shell__sticky`.
//   U1-4  the transport's scrub TRACK was 16px tall. (Its buttons were a
//         false positive in the same pass — 40x34 painted, but
//         `.scrub button::after` already extends them; asserted here so a
//         change to that rule is caught in CI rather than on a phone.)
//
// 44px is Apple's minimum and this codebase's own floor — the painted
// controls deliberately stay small (a filled 44px slab around 9-11px text
// reads as an empty block), so every assertion below probes a point OUTSIDE
// the control's own box and expects the control to answer anyway.
const { test, expect } = require('@playwright/test');

const PHONE = { width: 390, height: 844 };
const APPLE_MIN = 44;

test.use({ viewport: PHONE, hasTouch: true, isMobile: true });

test.beforeEach(async ({ page }) => {
  page.on('pageerror', (e) => { throw new Error(`uncaught page error: ${e}`); });
});

/**
 * What `document.elementFromPoint` answers at a point offset OUTWARD from
 * `sel`'s own border box — `ok` is true when the control (or its own glyph)
 * answers there.
 *
 * `scrollIntoView({ block: 'center' })` first: the phone drawer's bar is a
 * fixed overlay, and a control that happens to sit under it at the initial
 * scroll position resolves to the bar no matter how large its hit region is
 * (a real overlap question, separate from tap-target size — met while
 * red-proving U1-1).
 */
async function resolvesAt(page, sel, side, px) {
  return page.evaluate(
    ({ sel, side, px }) => {
      const el = document.querySelector(sel);
      if (!el) return { ok: false, hit: 'MISSING', box: [0, 0] };
      el.scrollIntoView({ block: 'center' });
      const b = el.getBoundingClientRect();
      let x = b.left + b.width / 2;
      let y = b.top + b.height / 2;
      if (side === 'above') y = b.top - px;
      if (side === 'below') y = b.bottom + px;
      if (side === 'left') x = b.left - px;
      if (side === 'right') x = b.right + px;
      if (side === 'inside-top') y = b.top + px;
      const hit = document.elementFromPoint(Math.round(x), Math.round(y));
      return {
        ok: !!hit && (hit === el || el.contains(hit)),
        hit: hit ? `${hit.tagName}.${hit.className}` : null,
        box: [Math.round(b.width), Math.round(b.height)],
      };
    },
    { sel, side, px },
  );
}

// A minimal, well-formed `/machine/resources` ledger — enough for the lens to
// render its odometer tiles and their "(i)" buttons. The hostile-string walk
// over this same endpoint lives in `viewer-machine.spec.js`; this fixture is
// deliberately boring, because the claim here is about geometry.
const LEDGER = {
  schema_version: '1.0',
  generated_at_ms: 1767225600000,
  gather_ms: 7,
  cache_ttl_ms: 2000,
  limit_bytes: 137438953472,
  limit_source: 'physical_pool',
  pool: { capacity_bytes: 137438953472, used_bytes: 69300000000, available_bytes: 63000000000, free_bytes: 3738599424 },
  pressure: { swap_used_bytes: 0, compressor_bytes: 2000000000, margin_percent: 43, red: false },
  models: [
    {
      identifier: 'darkmux:judge',
      model_key: 'judge',
      owner: 'darkmux',
      loaded_ctx: 65536,
      weights_bytes: 20000000000,
      kv_per_token_bytes: 100,
      kv_bytes_at_ctx: 6553600,
      potential_bytes: 26553600000,
      current_bytes: 20006553600,
      state: 'green',
    },
  ],
  machine: { potential_bytes: 26553600000, unpriced_models: 0, current_bytes: 20006553600, state: 'green' },
  attribution: 'per_process',
  attribution_note: '1 worker rank-matched',
  messages: [],
};

test('(U1-1) the machine odometer\'s (i) buttons answer a tap 12px outside their 14px glyph', async ({ page }) => {
  await page.route('**/machine/resources*', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(LEDGER) }),
  );
  await page.goto('/index-live.html#lens=machine');
  await page.waitForSelector('.mm-odo-i');

  // The painted control stays SMALL on purpose — this asserts the hit
  // region, which is the distinction the old comment got wrong.
  const first = await resolvesAt(page, '.mm-odo-i', 'above', 12);
  expect(first.box[0]).toBeLessThan(APPLE_MIN);
  for (const side of ['above', 'below', 'left', 'right']) {
    const r = await resolvesAt(page, '.mm-odo-i', side, 12);
    expect(r.ok, `${side}: resolved to ${r.hit}`).toBe(true);
  }
});

test('(U1-2) a nav tab answers a tap 4px above the strip — the 44px extension is not clipped', async ({ page }) => {
  await page.goto('/index.html#lens=fleet');
  await page.waitForSelector('.nav-tab');
  const r = await resolvesAt(page, '.nav-tab', 'above', 4);
  expect(r.ok, `resolved to ${r.hit}`).toBe(true);
});

test('(U1-4) every playback transport control meets the 44px floor', async ({ page }) => {
  await page.goto('/index.html#lens=fleet');
  await page.waitForSelector('.scrub input[type=range]');
  // The TRACK carries its hit region as its own box (a replaced element has
  // no usable pseudo-element), so the box is the assertion.
  const track = await resolvesAt(page, '.scrub input[type=range]', 'inside-top', 4);
  expect(track.box[1], 'the scrub track is the transport control a thumb misses').toBeGreaterThanOrEqual(APPLE_MIN);
  expect(track.ok, `track: resolved to ${track.hit}`).toBe(true);
  for (const sel of ['.scrub button.icon', '.scrub button:not(.icon)']) {
    const r = await resolvesAt(page, sel, 'above', 4);
    expect(r.ok, `${sel}: resolved to ${r.hit}`).toBe(true);
  }
});

// (U1-3) The fleet activity lane's session bars were 2-18px wide `role=button`
// targets on a phone. `.sbar` is absolutely positioned by percent inside its
// lane's `.tltrack`, so a short session on a 24-hour window paints a sliver —
// `min-width: 2px` was the only floor, and 2px is not a tap target.
//
// A SYNTHETIC lane rather than a sampled one: the assertion is about the
// stylesheet's floor for a known set of widths, and whether the demo day
// happens to contain a sub-minute session is not something this test should
// depend on. The classes are the ones `FleetLens.tsx` renders
// (`.lane` > `.tltrack` > `.sbar[data-arg=<sid>]`).
//
// The residual, named rather than hidden: bars in ONE lane are drawn in time
// order and a later sibling paints over an earlier one, so two back-to-back
// sub-minute sessions still overlap once both are widened. What the floor
// guarantees is that every bar owns its own START — the point a finger aims
// at — which is what the last assertion below measures.
test.describe('(U1-3) fleet activity session bars', () => {
  const SLIVER_MIN = 24;

  async function mountLane(page, widthsPct) {
    return page.evaluate(
      ({ widths }) => {
        document.querySelectorAll('#lanepobe').forEach((n) => n.remove());
        const host = document.createElement('div');
        host.id = 'lanepobe';
        host.style.cssText = 'position:fixed;left:0;top:0;width:340px;z-index:99999;background:#000';
        host.innerHTML =
          '<div class="lane"><div class="lname">probe</div><div class="tltrack">' +
          widths
            .map((w, i) => `<div class="sbar done" data-act="session" data-arg="s${i}" role="button" style="left:${i * 22}%;width:${w}%"></div>`)
            .join('') +
          '</div></div>';
        document.body.appendChild(host);
        return [...host.querySelectorAll('.sbar')].map((b) => {
          const r = b.getBoundingClientRect();
          const own = document.elementFromPoint(r.left + 2, r.top + r.height / 2);
          return { sid: b.dataset.arg, width: Math.round(r.width), ownsItsStart: own === b };
        });
      },
      { widths: widthsPct },
    );
  }

  test('a sub-minute session is still at least 24px of tappable bar, and owns its own start', async ({ page }) => {
    await page.goto('/index.html#lens=fleet');
    await page.waitForSelector('.app-shell');
    // 0.5% of a 340px track is 1.7px — the shape of the finding.
    const bars = await mountLane(page, [0.5, 1.5, 4, 0.5]);
    expect(bars).toHaveLength(4);
    for (const b of bars) {
      expect(b.width, `bar ${b.sid} is ${b.width}px — untappable`).toBeGreaterThanOrEqual(SLIVER_MIN);
      expect(b.ownsItsStart, `a tap at bar ${b.sid}'s own start must resolve to ${b.sid}`).toBe(true);
    }
  });
});
