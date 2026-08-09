// Scenario (d) daemon-restart — kill-hub/start-hub (the #1709 black-screen
// incident class): the daemon must either recover or HONESTLY show
// unreachable. Two phases, both asserted for real (never weakened):
//   1. while hub is down, a fresh navigation must FAIL LOUDLY (a connection
//      error), not silently render a blank "success" page — that failure
//      IS the honest "unreachable" outcome the packet brief accepts.
//   2. once hub is back, a reload must fully recover — full render-sanity,
//      zero console errors, no leftover error state.
import { test, expect } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { collectPageErrors, assertRenderSanity, screenshot } from "../lib/render-sanity.js";
import { GALLERY_DIR } from "../lib/paths.js";

test.afterEach(async () => {
  execFileSync("bash", ["../inject.sh", "start-hub"], { cwd: __dirname, stdio: "ignore" });
});

test("hub survives a hard kill: honest unreachable while down, full recovery after restart", async ({ page }) => {
  const pageErrors = collectPageErrors(page);

  await page.goto("/");
  await expect(page.locator("#stage .fleet")).toBeAttached({ timeout: 15000 });
  await assertRenderSanity(page, pageErrors);

  execFileSync("bash", ["../inject.sh", "kill-hub"], { cwd: __dirname, stdio: "inherit" });
  await new Promise((r) => setTimeout(r, 500));

  // Phase 1: a fresh navigation while hub is down must fail loudly — the
  // honest "unreachable" outcome, not a silent blank success.
  let navigationFailed = false;
  try {
    await page.goto("/", { timeout: 5000 });
  } catch {
    navigationFailed = true;
  }
  expect(navigationFailed, "navigating to a killed daemon should fail loudly, not silently render blank").toBe(true);

  // Phase 2: bring hub back, confirm full recovery.
  execFileSync("bash", ["../inject.sh", "start-hub"], { cwd: __dirname, stdio: "inherit" });
  await expect(async () => {
    const res = await fetch("http://127.0.0.1:18765/health");
    expect(res.ok).toBe(true);
  }).toPass({ timeout: 20000 });

  const recoveryErrors = collectPageErrors(page);
  await page.goto("/");
  await expect(page.locator("#stage .fleet")).toBeAttached({ timeout: 15000 });
  await assertRenderSanity(page, recoveryErrors);

  await screenshot(page, GALLERY_DIR, "daemon-restart");
});
