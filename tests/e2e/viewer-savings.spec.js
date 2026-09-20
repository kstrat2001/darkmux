// (#1607 / #2834) The savings hero's token accounting.
//
// #1607's defect: the attribution split was binary — `endpoint present →
// cloud`, **`absent → local`** — so anything darkmux could not attribute was
// credited to "tokens kept off the meter". On a machine running every
// dispatch against a hosted endpoint the hero read 471,930 local / 0 cloud,
// and all of it was gpt-4o on Foundry. The number was not merely missing a
// category; it claimed the opposite of the truth, in the direction that
// flatters.
//
// #2834 WITHDREW that split from the render rather than refining it, because
// the predicate cannot carry the distinction in either direction: `is_remote()`
// is `endpoint.url.is_some()`, so a local inference server on 127.0.0.1 also
// reads as cloud. The hero now states one figure — every token darkmux
// dispatched — and makes no claim about what any of them cost.
//
// So this spec's job changed with the surface, and the two halves below are
// what remains falsifiable at the DOM layer:
//
//   1. the unattributable tokens are still COUNTED. Dropping them from the
//      headline would be a quieter version of #1607's dishonesty — the
//      flattering direction is reached by omission just as well as by
//      mislabeling. The fixture's 1,200 unattributable tokens are inside the
//      total, and this assertion fails if they stop being.
//   2. the hero makes NO attribution claim. #1607's defect is unreachable by
//      construction rather than by guard, and this pins that it stays so.
//
// The split itself is still COMPUTED (`tokensOffMeter()` in
// `ui/src/lenses/fleet/savings.ts`) and still unit-tested per-defect in
// `savings.test.ts`'s `describe("tokensOffMeter")` block — one `it` per
// mechanism #1607 named. What is gone is the RENDER of it, which is all this
// file ever asserted.
//
// The fixture still uses the SPACE spelling of `dispatch start` on purpose —
// that keeps `flowToRenderModel`'s normalizer under test, so if it ever stops
// normalizing the space form to dotted, this spec notices.

const { test, expect } = require('@playwright/test');

test('the hero states one total, counts the unattributable, and claims no attribution (#2834)', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));

  await page.goto('/index-savings.html');
  await page.waitForSelector('.savings', { timeout: 15_000 });

  // Fixture (`tests/fixtures/savings-flow.jsonl`), by tier:
  //   local   1,000  — `sess-local`, bookends and no endpoint anywhere
  //   cloud   5,700  — `sess-cloud` 5,000 (completion names an endpoint and
  //                    reports spend only as `remote_tokens`, the review
  //                    path's real shape) + `sess-direct` 700
  //   unknown 1,200  — `task:probe-seat` 300 (a seat session with tokens and
  //                    no bookend of its own) + `sess-errored` 900 (its start
  //                    named no endpoint and its terminal is `dispatch
  //                    error`, so nothing ever says where it ran — the tier
  //                    where #1607's flattering default used to land)
  //                            total 7,900
  await expect(page.locator('.savlead .savnum')).toHaveText('7,900');

  // (1) Red-prove anchor: were `unknown` dropped from the headline the way
  // #1607's remedy might tempt someone to "fix" it, this reads 6,700.
  const headline = Number((await page.locator('.savlead .savnum').innerText()).replace(/,/g, ''));
  expect(headline, 'the unattributable 1,200 must be inside the total, not dropped from it').toBe(7900);

  // (2) No attribution claim survives anywhere in the hero — headline, chip
  // labels, or a tooltip. Moving the claim into a `title` would not have made
  // it true, so a relocation fails this too.
  const heroText = (await page.locator('.savings').innerText()).toLowerCase();
  for (const claim of ['local', 'cloud', 'unattributed', 'off the meter', 'off-meter']) {
    expect(heroText, `hero must not claim "${claim}": ${heroText}`).not.toContain(claim);
  }
  const titles = await page.locator('.savings [title]').count();
  expect(titles, 'the split tooltip is withdrawn, not relocated').toBe(0);

  // The single-tier leads are gone with the split — not merely emptied.
  expect(await page.locator('.savlead.cloud, .savlead.unknown').count()).toBe(0);
  expect(await page.locator('.savlead').count()).toBe(1);

  // The class row still partitions the SAME total: `sess-direct`'s spend has
  // no prompt/completion split, so it must land in `unclassified` rather than
  // vanishing — otherwise the headline silently exceeds the chips beneath it.
  await expect(page.locator('.savc.uncls .scv')).toHaveText('700');

  expect(pageErrors).toEqual([]);
});
