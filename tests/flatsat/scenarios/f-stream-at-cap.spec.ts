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
import { GALLERY_DIR, REDIS_MAXLEN } from "../lib/paths.js";

test("the fleet view stays sane once the shared flow stream rides its MAXLEN cap", async ({ page }) => {
  test.setTimeout(120_000);
  execFileSync("bash", ["../inject.sh", "flood-stream", "10500"], { cwd: __dirname, stdio: "inherit" });

  // QA finding (TAKE 1): assert the scenario's own precondition — without
  // this, a broken/no-op flood step would silently leave the stream small
  // and every assertion below would still pass for the wrong reason (there
  // would be nothing to actually saturate). `~` MAXLEN trim is approximate
  // (it may leave the stream a little ABOVE the cap between trims), so this
  // checks "at or past the cap", not an exact count.
  //
  // `docker exec redis-cli` rather than lib/redis.mjs's Bun.RedisClient
  // wrapper — Playwright's own test workers run under Node (verified live:
  // `bunx playwright test` still executes spec files in a Node worker
  // process), where the `Bun` global doesn't exist. lib/redis.mjs stays
  // Bun-only for the scripts that ARE run via `bun run` (seed.mjs,
  // flood-stream.mjs); this is the one call site that needs a
  // runtime-agnostic path, matching inject.sh's own docker-exec pattern.
  const xlenOut = execFileSync("docker", ["exec", "flatsat-redis", "redis-cli", "XLEN", "darkmux:flow"], { encoding: "utf8" });
  const streamLen = parseInt(xlenOut.trim(), 10);
  expect(streamLen, `stream should have reached its MAXLEN ~ ${REDIS_MAXLEN} cap after flooding`).toBeGreaterThanOrEqual(REDIS_MAXLEN);

  const pageErrors = collectPageErrors(page);
  const start = Date.now();
  await page.goto("/");
  await expect(page.locator("#stage .fleet")).toBeAttached({ timeout: 30000 });
  const elapsedMs = Date.now() - start;

  await assertRenderSanity(page, pageErrors);
  expect(elapsedMs, `fleet render should stay bounded even with a saturated stream (took ${elapsedMs}ms)`).toBeLessThan(20_000);

  await screenshot(page, GALLERY_DIR, "stream-at-cap");
});
