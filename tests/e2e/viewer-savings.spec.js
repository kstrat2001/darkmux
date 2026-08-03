// (#1607) The savings hero's local/cloud/unknown attribution.
//
// The bug this pins: the split was binary — `endpoint present → cloud`,
// **`absent → local`** — so anything darkmux could not attribute was credited
// to "tokens kept off the meter". On a machine running every dispatch against
// a hosted endpoint the hero read 471,930 local / 0 cloud, and all of it was
// gpt-4o on Foundry. The number was not merely missing a category; it claimed
// the opposite of the truth, in the direction that flatters.
//
// Two defects had to line up, and each is asserted below:
//   1. the classifier registered a session's endpoint only from INSIDE a
//      branch that also required token fields — and the review path's
//      completion names its endpoint while reporting spend as
//      `remote_tokens`, leaving the other three null, so the branch never
//      ran and the session never registered;
//   2. unattributable sessions defaulted to local.
//
// A third "defect" was investigated and disproven: the classifier matches the
// dotted `dispatch.start` while the wire carries `dispatch start`, which looks
// like a mismatch until you read `flowToRenderModel`, which normalizes the
// space form to dotted on ingest. Red-proving showed a leniency helper for it
// changed nothing, and it was removed. The fixture below still uses the
// SPACE spelling on purpose — that keeps the normalizer itself under test, so
// if it ever stops normalizing, this spec notices.

const { test, expect } = require('@playwright/test');

test('the hero splits local / cloud / unknown and never credits the unattributable to local', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index-savings.html');
  await page.waitForSelector('.savings', { timeout: 15_000 });

  const t = await page.evaluate(() => {
    const o = tokensOffMeter();
    return { local: o.local, cloud: o.cloud, unknown: o.unknown, total: o.total, uncls: o.uncls };
  });

  // Fixture: local 1000 · cloud 5000 (telemetry) + 700 (a telemetry-LESS
  // session whose only spend figure is `remote_tokens`) · unknown 300 (a
  // `task:` seat with no bookend of its own).
  expect(t.cloud).toBe(5700);   // (1): endpoint registered without token fields present
  // 300 from the bookend-less `task:` seat + 900 from a hosted review that
  // ERRORED: its start named no endpoint (the review classifies itself only on
  // a clean close) and its terminal is `dispatch error`, so nothing ever says
  // where it ran. A rule that accepted any endpoint-less bookend would call
  // that local — the flattering error surviving on the error path.
  expect(t.unknown).toBe(1200); // (2): not silently local
  expect(t.local).toBe(1000);   // and the genuinely-local session is untouched
  expect(t.local + t.cloud + t.unknown).toBe(t.total); // the three partition the whole

  // The unattributed figure is SHOWN, not just computed — a number excluded
  // from the savings claim without appearing anywhere would be a quieter
  // version of the same dishonesty.
  await expect(page.locator('.savlead.unknown .savnum')).toHaveText('1,200');

  // The telemetry-less session's spend has no prompt/completion split, so it
  // must land in `unclassified` rather than vanishing from the class row —
  // otherwise the headline silently exceeds the chips beneath it.
  expect(t.uncls).toBe(700);
  expect(pageErrors).toEqual([]);
});
