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

  // (port note) `tokensOffMeter()` is a pure function taking flow records as
  // an argument (`ui/src/lenses/fleet/savings.ts`) — the port holds no
  // global mirroring legacy's `state`/`DATA` for `page.evaluate` to call
  // into, and its exact scenarios (endpoint-without-tokens, the review
  // path's `remote_tokens`-only completion, an unknown/no-bookend session)
  // are already unit-tested directly against that function in
  // `savings.test.ts` — see the `describe("tokensOffMeter")` block there,
  // one `it` per defect this spec's own header names. What's NOT covered
  // there is whether the computed split actually REACHES the rendered
  // hero, which is what this e2e spec exists to prove — so it reads the
  // numbers off the DOM `SavingsHero` paints instead of recomputing them.
  async function savNum(selector) {
    const text = await page.locator(selector).innerText();
    return Number(text.replace(/,/g, ''));
  }

  // Fixture: local 1000 · cloud 5000 (telemetry) + 700 (a telemetry-LESS
  // session whose only spend figure is `remote_tokens`) · unknown 300 (a
  // `task:` seat with no bookend of its own) + 900 from a hosted review
  // that ERRORED (its start named no endpoint — the review classifies
  // itself only on a clean close — and its terminal is `dispatch error`,
  // so nothing ever says where it ran; a rule that accepted any
  // endpoint-less bookend would call that local — the flattering error
  // surviving on the error path).
  const cloud = await savNum('.savlead.cloud .savnum');
  expect(cloud).toBe(5700); // (1): endpoint registered without token fields present

  const unknown = await savNum('.savlead.unknown .savnum');
  expect(unknown).toBe(1200); // (2): not silently local

  const local = await savNum('.savlead:not(.cloud):not(.unknown) .savnum');
  expect(local).toBe(1000); // the genuinely-local session is untouched

  // The unattributed figure is SHOWN, not just computed — a number excluded
  // from the savings claim without appearing anywhere would be a quieter
  // version of the same dishonesty.
  await expect(page.locator('.savlead.unknown .savnum')).toHaveText('1,200');

  // The telemetry-less session's spend has no prompt/completion split, so it
  // must land in `unclassified` rather than vanishing from the class row —
  // otherwise the headline silently exceeds the chips beneath it.
  await expect(page.locator('.savc.uncls .scv')).toHaveText('700');
  expect(pageErrors).toEqual([]);
});
