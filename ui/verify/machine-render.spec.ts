import { test, expect } from "@playwright/test";
import { mkdirSync } from "node:fs";
import path from "node:path";

// Packet 2's live render proof — the machine lens's twin of Packet 1's
// `live-render.spec.ts`, against a THROWAWAY daemon on 127.0.0.1:8793
// (never port 8765 — see the module doc there for the full rationale).
// Talks to a REAL running daemon with REAL `/machine/specs` +
// `/machine/resources` data (whatever the operator's LMStudio actually has
// resident) — this is a read-only metadata probe (zero model dispatches,
// same "observer must not join the observed" posture the lens itself is
// built on), not an inference call, so it doesn't touch the no-unattended-
// LMStudio-dispatch boundary.
//
// Gitignored, repo-relative by default (`ui/verify/.gallery/2-machine/`) —
// NOT an operator machine path (QA must-fix, 2026-08-09 — see the parallel
// fix in `tests/parity/next-parity.spec.ts` for the full rationale: a
// committed absolute path bakes one machine's home-directory layout, and a
// session-scoped scratch UUID, into a PUBLIC repo). Override with
// `DARKMUX_GALLERY_DIR` for a real run. `mkdirSync` lives in `beforeAll`,
// not module scope, so importing/collecting this file has zero filesystem
// side effects.
const GALLERY_DIR = process.env.DARKMUX_GALLERY_DIR || path.join(__dirname, ".gallery", "2-machine");

test.beforeAll(() => {
  mkdirSync(GALLERY_DIR, { recursive: true });
});

function screenshotPath(name: string): string {
  return path.join(GALLERY_DIR, name);
}

test("live daemon (8793): the machine lens renders clean — zero pageerrors, no horizontal scroll at 390px, real stage height", async ({
  page,
}) => {
  const pageErrors: string[] = [];
  const consoleErrors: string[] = [];
  page.on("pageerror", (err) => pageErrors.push(String(err)));
  page.on("console", (msg) => {
    if (msg.type() === "error") consoleErrors.push(msg.text());
  });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/next#lens=machine");
  await page.waitForSelector("#stage");
  await page.waitForSelector('.machine-lens__health[data-state]:not([data-state="loading"])', { timeout: 15_000 });

  const stageBox = await page.locator("#stage").boundingBox();
  expect(stageBox, "the #stage region must have a real bounding box").not.toBeNull();
  expect(stageBox!.height).toBeGreaterThan(20);

  // See `ui/verify/live-render.spec.ts`'s identical comment: `document.body
  // .scrollWidth`, NOT `document.documentElement.scrollWidth` — `styles.css`
  // clamps the latter via `overflow-x: hidden` on both html AND body.
  const overflow = await page.evaluate(() => document.body.scrollWidth > document.documentElement.clientWidth);
  expect(overflow, "no horizontal document scroll at 390px").toBe(false);

  expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);
  expect(consoleErrors, `console.error events: ${consoleErrors.join("; ")}`).toHaveLength(0);

  await page.screenshot({ path: screenshotPath("live-8793-390px.png"), fullPage: true });
});

test("live daemon (8793): desktop-width screenshot for review", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto("/next#lens=machine");
  await page.waitForSelector('.machine-lens__health[data-state]:not([data-state="loading"])', { timeout: 15_000 });
  await page.screenshot({ path: screenshotPath("live-8793-1280px.png"), fullPage: true });
});
