// Scenario (b) redis-blip — stop Redis mid-view: remote (peer) rows must
// NOT silently vanish into an empty-looking lens. The daemon's own doc
// comments (crates/darkmux-serve/src/lib.rs's fleet_flow_records) promise a
// last-known-good CACHE on the SERVER side (a 2s TTL, serving the stale
// snapshot if the live refresh fails) — but that cache is per-process and
// per-request-window; a blip that outlasts it degrades to "no fleet
// records", not a crash. What matters here is that the HUB's OWN local
// data (never Redis-dependent) keeps rendering, and nothing goes fully
// blank/erroring.
import { test, expect } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { collectPageErrors, assertRenderSanity, screenshot } from "../lib/render-sanity.js";
import { GALLERY_DIR } from "../lib/paths.js";

test.afterEach(async () => {
  // Always restore Redis regardless of pass/fail, so later scenarios in
  // this run aren't left with a dead coordination substrate.
  execFileSync("bash", ["../inject.sh", "start-redis"], { cwd: __dirname, stdio: "ignore" });
  await new Promise((r) => setTimeout(r, 1000));
});

test("hub's own local records keep rendering when Redis drops mid-view", async ({ page }) => {
  const pageErrors = collectPageErrors(page);

  await page.goto("/");
  await expect(page.locator("#stage .fleet")).toBeAttached({ timeout: 15000 });
  await assertRenderSanity(page, pageErrors);
  const beforeText = await page.locator("#stage").innerText();
  expect(beforeText).toContain("flatsat-hub");

  execFileSync("bash", ["../inject.sh", "stop-redis"], { cwd: __dirname, stdio: "inherit" });
  // Past the 2s server-side fleet-cache TTL and the presence-key TTL, so
  // this is genuinely observing the degraded (not cached) state.
  await new Promise((r) => setTimeout(r, 3000));

  await page.reload();
  await expect(page.locator("#stage .fleet")).toBeAttached({ timeout: 15000 });
  await assertRenderSanity(page, pageErrors);

  const afterText = await page.locator("#stage").innerText();
  expect(afterText, "the hub's OWN local flow history is file-backed, never Redis-dependent — it must still be there").toContain("flatsat-hub");

  await screenshot(page, GALLERY_DIR, "redis-blip");
});
