// Scenario (f) stream-at-cap — seeds 10,500 extra records onto the shared
// Redis stream (config's redis.maxlen default is 10000) so a real MAXLEN ~
// trim actually fires, then confirms the view stays sane: still renders,
// still no horizontal scroll, still zero console errors, and still
// responds within a bounded time (a saturated stream shouldn't make
// XREVRANGE COUNT 10000 hang the page).
//
// Runs LAST alphabetically among the six specs on purpose: MAXLEN ~ trim
// evicts the OLDEST stream entries first, which after this test are the
// (b) hub/peer seed records from `up`'s seed.mjs pass — running this before
// (a) fleet-visible would make that scenario's peer-visibility assertion
// fragile against trim timing. See README's scenario table.
import { test, expect } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { collectPageErrors, assertRenderSanity, screenshot } from "../lib/render-sanity.js";
import { GALLERY_DIR } from "../lib/paths.js";

test("the fleet view stays sane once the shared flow stream rides its MAXLEN cap", async ({ page }) => {
  test.setTimeout(120_000);
  execFileSync("bash", ["../inject.sh", "flood-stream", "10500"], { cwd: __dirname, stdio: "inherit" });

  const pageErrors = collectPageErrors(page);
  const start = Date.now();
  await page.goto("/");
  await expect(page.locator("#stage .fleet")).toBeAttached({ timeout: 30000 });
  const elapsedMs = Date.now() - start;

  await assertRenderSanity(page, pageErrors);
  expect(elapsedMs, `fleet render should stay bounded even with a saturated stream (took ${elapsedMs}ms)`).toBeLessThan(20_000);

  await screenshot(page, GALLERY_DIR, "stream-at-cap");
});
