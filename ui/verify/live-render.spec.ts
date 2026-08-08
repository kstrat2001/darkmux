import { test, expect } from "@playwright/test";
import { execSync } from "node:child_process";
import path from "node:path";

// Packet 1's live render proof (see `ui/verify/playwright.config.ts`'s own
// doc). Talks to a THROWAWAY daemon on 127.0.0.1:8790 — never port 8765.
//
// The daemon PID is read from a file the launching shell writes
// (`DARKMUX_THROWAWAY_PID_FILE`, defaulting to
// `/tmp/darkmux-throwaway-8790.pid`) so this spec can kill it mid-test for
// the error-state proof without hardcoding a PID that changes every run.
const PID_FILE = process.env.DARKMUX_THROWAWAY_PID_FILE || "/tmp/darkmux-throwaway-8790.pid";
const GALLERY_DIR =
  process.env.DARKMUX_GALLERY_DIR ||
  "/private/tmp/claude-501/-Users-kain-de-projects-darkmux-public/652b2a6d-51b7-4543-9ddf-8ef250dd2a4d/scratchpad/ui-port-gallery/1-scaffold";

function screenshotPath(name: string): string {
  return path.join(GALLERY_DIR, name);
}

test("the /next shell renders clean: zero pageerrors, no horizontal scroll at 390px, non-trivial stage height", async ({ page }) => {
  const pageErrors: string[] = [];
  const consoleErrors: string[] = [];
  page.on("pageerror", (err) => pageErrors.push(String(err)));
  page.on("console", (msg) => {
    if (msg.type() === "error") consoleErrors.push(msg.text());
  });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/next");
  await page.waitForSelector("#stage");
  // Give the fleet query a moment to settle out of the pending skeleton.
  await page.waitForSelector('[data-state]:not([data-state="pending"])', { timeout: 10_000 });

  const stageBox = await page.locator("#stage").boundingBox();
  expect(stageBox, "the #stage region must have a real bounding box").not.toBeNull();
  expect(stageBox!.height).toBeGreaterThan(20);

  const overflow = await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth);
  expect(overflow, "no horizontal document scroll at 390px").toBe(false);

  expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);
  expect(consoleErrors, `console.error events: ${consoleErrors.join("; ")}`).toHaveLength(0);

  await page.screenshot({ path: screenshotPath("shell-390px.png") });

  // Real empty state: the throwaway daemon has no DARKMUX_REDIS_URL, so
  // /fleet/machines/live genuinely returns [] — this is the REAL daemon's
  // real answer, not a mock.
  const emptyState = page.locator('[data-state="empty"]');
  await expect(emptyState).toBeVisible();
  await page.screenshot({ path: screenshotPath("2-fleet-empty-real.png") });
});

test("data state: a non-empty fleet renders one card per machine", async ({ page }) => {
  await page.route("**/fleet/machines/live", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify([
        {
          machine_uid: "ABFCA777-9F06-A6BF-52CB-589A5D164929",
          display_name: "MacBook-Pro",
          schema_version: "1.18.0",
          beat_ts_ms: Date.now(),
          specs: "Apple M5 Max · 128 GB · 16 cores",
          loaded_models: ["darkmux:qwen3.6-35b-a3b-turboquant-mlx"],
        },
        {
          machine_uid: "E3F5FE8A-1460-E6FF-663F-760A6D75B9C9",
          display_name: "m1-max-32gb-studio",
          schema_version: "1.18.0",
          beat_ts_ms: Date.now(),
        },
      ]),
    }),
  );
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto("/next");
  await expect(page.locator('[data-state="data"]')).toBeVisible();
  await expect(page.getByText("MacBook-Pro")).toBeVisible();
  await expect(page.getByText("m1-max-32gb-studio")).toBeVisible();
  await page.screenshot({ path: screenshotPath("3-fleet-data-mocked.png") });
});

test("pending state: the skeleton renders before the fetch resolves", async ({ page }) => {
  await page.route("**/fleet/machines/live", async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 5_000));
    await route.fulfill({ status: 200, contentType: "application/json", body: "[]" });
  });
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto("/next");
  await expect(page.locator('[data-state="pending"]')).toBeVisible();
  await page.screenshot({ path: screenshotPath("1-fleet-pending.png") });
});

test("error state: killing the throwaway daemon mid-session renders a visible error, not a blank page", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto("/next");
  // Confirm we start from a real, non-error state (daemon alive).
  await page.waitForSelector('[data-state]:not([data-state="pending"])', { timeout: 10_000 });
  await expect(page.locator('[data-state="error"]')).toHaveCount(0);

  // Kill the THROWAWAY daemon (port 8790 only — never the operator's real
  // daemon on 8765). The already-loaded page's JS bundle stays in memory;
  // only its next fetch (the 5s refetchInterval) now fails.
  const pid = execSync(`cat ${PID_FILE}`).toString().trim();
  execSync(`kill ${pid}`);

  await expect(page.locator('[data-state="error"]')).toBeVisible({ timeout: 15_000 });
  const stageBox = await page.locator("#stage").boundingBox();
  expect(stageBox!.height).toBeGreaterThan(20);
  await page.screenshot({ path: screenshotPath("4-fleet-error-daemon-killed.png") });
});
