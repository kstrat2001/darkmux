// Headless e2e smoke for the #1247 Part 3 lab observer lens. The static XSS
// harness has no daemon behind it, so `/lab/runs` isn't reachable there —
// this spec uses the `index-lab.html` variant (playwright.config.js) which
// injects `darkmux-lab-runs-src` pointing at a committed fixture
// (tests/fixtures/lab-runs-fixture.json), the same static-fixture-override
// pattern the missions/sprints lens already uses. Confirms the tab renders,
// the run list populates from the fixture route, series grouping + the
// knob-diff line compute correctly, and drilling into a run navigates
// without throwing.
const { test, expect } = require('@playwright/test');

test('lab lens tab renders and the run list populates from a fixture route', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index-lab.html');
  await page.waitForSelector('#lens-lab');

  await page.click('#lens-lab');
  await expect(page.locator('#lens-lab')).toHaveClass(/\bon\b/);
  await expect(page.locator('#lens-fleet')).not.toHaveClass(/\bon\b/);

  // Two task cards: the fixture's two `demo-case-a` runs group into one
  // series; the single `demo-case-b` live run is its own card.
  const cards = page.locator('.labtaskcard');
  await expect(cards).toHaveCount(2);

  const seriesCard = page.locator('.labtaskcard', { hasText: 'demo-case-a' });
  await expect(seriesCard.locator('.labrunrow')).toHaveCount(2);
  // Only the newer run gets a diff line (compared against the older one);
  // the knob diff between the fixture's two runs (probe k 1→2) renders as a
  // plain (single-variable) diff line, not the multi-variable warning.
  await expect(seriesCard.locator('.labdiffline')).toHaveCount(1);
  await expect(seriesCard.locator('.labdiffline')).toContainText('demo-probe.k 1→2');
  await expect(seriesCard.locator('.labdiffline.warn')).toHaveCount(0);

  const liveCard = page.locator('.labtaskcard', { hasText: 'demo-case-b' });
  await expect(liveCard.locator('.labbadge.live')).toHaveCount(1);
  await expect(liveCard).toContainText('staffing pending');

  // Drilling into a run navigates to the detail crumb without throwing (the
  // detail fetch itself 404s in this static harness — that's expected and
  // handled by the loading state, not asserted here).
  await seriesCard.locator('.labrunrow').first().click();
  await expect(page.locator('#crumb')).toContainText('demo-case');

  expect(pageErrors, `uncaught page errors: ${pageErrors.join(' | ')}`).toEqual([]);
});
