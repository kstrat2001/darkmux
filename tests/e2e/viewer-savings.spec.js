// (#1607) The savings hero's local/cloud/unknown attribution.
//
// The bug this pins: the split was binary — `endpoint present → cloud`,
// **`absent → local`** — so anything darkmux could not attribute was credited
// to "tokens kept off the meter". On a machine running every dispatch against
// a hosted endpoint the hero read 471,930 local / 0 cloud, and all of it was
// gpt-4o on Foundry. The number was not merely missing a category; it claimed
// the opposite of the truth, in the direction that flatters.
//
// Three failures had to line up, and each is asserted below:
//   1. the classifier matched only `dispatch.start`/`dispatch.complete`
//      (dot-separated) while production emits the SPACE form;
//   2. it registered a session's endpoint only from inside a branch that also
//      required token fields — and the review path's completion names its
//      endpoint while reporting spend as `remote_tokens`, leaving the other
//      three null;
//   3. unattributable sessions defaulted to local.
const { test, expect } = require('@playwright/test');

test('the hero splits local / cloud / unknown and never credits the unattributable to local', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index-savings.html');
  await page.waitForSelector('.savings', { timeout: 15_000 });

  const t = await page.evaluate(() => {
    const o = tokensOffMeter();
    return { local: o.local, cloud: o.cloud, unknown: o.unknown, total: o.total };
  });

  // Fixture: local 1000 · cloud 5000 (space-spelled bookend, remote_tokens
  // only) · unknown 300 (a `task:` seat with no bookend).
  expect(t.cloud).toBe(5000);   // (1)+(2): space spelling AND endpoint-without-token-fields
  expect(t.unknown).toBe(300);  // (3): not silently local
  expect(t.local).toBe(1000);   // and the genuinely-local session is untouched
  expect(t.local + t.cloud + t.unknown).toBe(t.total); // the three partition the whole

  // The unattributed figure is SHOWN, not just computed — a number excluded
  // from the savings claim without appearing anywhere would be a quieter
  // version of the same dishonesty.
  await expect(page.locator('.savlead.unknown .savnum')).toHaveText('300');
  expect(pageErrors).toEqual([]);
});
