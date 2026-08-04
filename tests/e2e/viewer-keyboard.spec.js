// (#1642) The first keyboard tests in this suite.
//
// `grep -rn "\.press(\|keyboard\." tests/e2e/*.spec.js` returned ZERO before
// this file. Not one spec had ever pressed a key — despite the source carrying
// detailed comments about keyboard activation (#1090) and Escape-closes-any-
// modal (#1569 sweep). The code was more careful than its coverage, which is
// how three real keyboard defects sat in a heavily-reviewed file.
//
// This matters more than the count suggests: mouse-only testing cannot observe
// any of it, and the population it breaks for — keyboard-only, switch, screen
// reader — is exactly who the managed-focus work was built to serve.
const { test, expect } = require('@playwright/test');

async function boot(page) {
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.goto('/index-lab.html');
  return errors;
}

const openCount = (page) =>
  page.evaluate(() =>
    ['modalbg', 'nmodalbg', 'imodalbg'].filter((id) => {
      const m = document.getElementById(id);
      return m && m.style.display !== 'none' && m.style.display !== '';
    })
  );

test('Tab cannot walk out of an open dialog', async ({ page }) => {
  // The overlays are opaque `position:fixed;inset:0`, but nothing kept focus
  // inside. 31 Tab presses from an open Filters landed on the page header link
  // — visually buried under the backdrop, so a keyboard user is operating
  // controls they cannot see.
  const errors = await boot(page);
  await page.locator('[data-act="filters"]').first().click();
  await expect(page.locator('#modalbg')).toBeVisible();

  for (let i = 0; i < 40; i++) {
    await page.keyboard.press('Tab');
    const inside = await page.evaluate(() =>
      document.getElementById('modalbg').contains(document.activeElement)
    );
    expect(inside, `focus escaped the dialog after ${i + 1} Tab press(es)`).toBe(true);
  }

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('Shift+Tab wraps backwards instead of escaping', async ({ page }) => {
  const errors = await boot(page);
  await page.locator('[data-act="filters"]').first().click();
  await expect(page.locator('#modalbg')).toBeVisible();

  for (let i = 0; i < 6; i++) {
    await page.keyboard.press('Shift+Tab');
    const inside = await page.evaluate(() =>
      document.getElementById('modalbg').contains(document.activeElement)
    );
    expect(inside, `focus escaped backwards after ${i + 1} Shift+Tab press(es)`).toBe(true);
  }

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('one Escape closes the dialog the operator is actually looking at', async ({ page }) => {
  // Two dialogs could be open at once, and `closeOpenModal` walks a FIXED list
  // closing the first one found — not the topmost. With About over Filters, one
  // Escape closed Filters (invisible, underneath) while About stayed covering
  // the screen. It reads as the UI ignoring you.
  //
  // Fixed by making one-at-a-time structural: opening any dialog closes any
  // other, so "topmost" and "only" become the same thing and the fixed-list
  // walk is correct by construction rather than by ordering luck.
  //
  // Driven through `openModalEl` DIRECTLY rather than by clicking the version
  // chip. A first draft clicked `[data-act="info"]`, which this harness does
  // not render (the chip needs an injected build id) — so it silently took a
  // fallback branch and proved nothing while reporting green. Calling the
  // function under test is less pretty and actually exercises it.
  const errors = await boot(page);

  await page.locator('[data-act="filters"]').first().click();
  expect(await openCount(page)).toEqual(['modalbg']);

  const opened = await page.evaluate(() => {
    if (typeof openModalEl !== 'function') return null;
    openModalEl('imodalbg');
    return true;
  });
  expect(opened, 'openModalEl must be reachable or this test proves nothing').toBe(true);

  expect(
    await openCount(page),
    'opening a second dialog must close the first — two open is what broke Escape'
  ).toEqual(['imodalbg']);

  await page.keyboard.press('Escape');
  expect(await openCount(page), 'one Escape must leave nothing open').toEqual([]);

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});

test('focus returns to the control that opened the dialog', async ({ page }) => {
  // Escape / ✕ / backdrop-click all restore focus. Without it a keyboard user
  // is dropped at the top of the document and has to tab back to where they
  // were, every time.
  const errors = await boot(page);
  const trigger = page.locator('[data-act="filters"]').first();
  await trigger.click();
  await expect(page.locator('#modalbg')).toBeVisible();

  await page.keyboard.press('Escape');
  const restored = await page.evaluate(
    () => document.activeElement && document.activeElement.getAttribute('data-act')
  );
  expect(restored, 'focus should be back on the filters trigger').toBe('filters');

  expect(errors, `uncaught: ${errors.join(' | ')}`).toEqual([]);
});
