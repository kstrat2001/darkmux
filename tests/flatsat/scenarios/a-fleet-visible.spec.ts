// Scenario (a) fleet-visible — the #1705 incident class: "a mission owned
// by another machine had no durable record here... every mission executing
// anywhere else in the fleet was absent from the missions lens while its
// records sat in the daemon's own reach." Proves the FIX still holds against
// a real two-machine fleet on real Redis, not just the #1705 unit tests.
//
// Loaded against the HUB (playwright.config.js's baseURL) — the peer's
// seeded work (mission ids "peer-demo-refactor"/"peer-demo-docs", machine_id
// "flatsat-peer") only reaches the hub's view via the shared Redis stream
// (seed.mjs XADDs both machines' records there, mirroring TeeSink). If this
// regresses, the hub would show only its OWN (corpus-derived) missions.
import { test, expect } from "@playwright/test";
import path from "node:path";
import { collectPageErrors, assertRenderSanity, screenshot } from "../lib/render-sanity.js";
import { GALLERY_DIR } from "../lib/paths.js";

test("both machines' work is visible from the hub's fleet dashboard + runs lens", async ({ page }) => {
  const pageErrors = collectPageErrors(page);

  await page.goto("/");
  await expect(page.locator("#stage .fleet")).toBeAttached({ timeout: 15000 });
  await assertRenderSanity(page, pageErrors);

  const fleetText = await page.locator("#stage").innerText();
  expect(fleetText, "fleet dashboard should name the hub machine").toContain("flatsat-hub");
  expect(fleetText, "fleet dashboard should name the peer machine (#1705 class)").toContain("flatsat-peer");

  // Runs lens — "the peer's mission rows render" (packet brief, verbatim).
  await page.click("#lens-runs");
  await expect(page.locator("#stage .lablist")).toBeAttached({ timeout: 15000 });
  await assertRenderSanity(page, pageErrors);

  const runsText = await page.locator("#stage").innerText();
  expect(runsText, "runs lens should surface the peer's seeded mission activity").toContain("peer-demo");

  await screenshot(page, GALLERY_DIR, "fleet-visible");
});
