// Render-sanity assertions shared by every scenario spec (the packet
// brief's requirement, verbatim): main content region visible with
// non-trivial height; NO horizontal document scroll at phone width (390px,
// the chrome-order precedent tests/e2e already established); ZERO
// pageerror/console-error events during the lens render (catches the
// #1709 black-screen class). CommonJS, matching tests/parity's
// lib/extract-lens.js / lib/mock-routes.js convention (imported from
// ESM-flavored .spec.ts files via `import ... from "./lib/render-sanity.js"`,
// which Playwright's TS loader interops transparently).
const fs = require("node:fs");
const path = require("node:path");
const { expect } = require("@playwright/test");

/** Attach BEFORE navigation. Returns the array pageerror/console-error
 * strings accumulate into — pass the SAME array to assertRenderSanity. */
function collectPageErrors(page) {
  const errors = [];
  page.on("pageerror", (err) => errors.push(`pageerror: ${err.message || err}`));
  page.on("console", (msg) => {
    if (msg.type() === "error") errors.push(`console.error: ${msg.text()}`);
  });
  return errors;
}

/** The three render-sanity checks. `mainSelector` defaults to `#stage` —
 * the same output target the parity harness's extraction targets (every
 * `render*()` function in viewer.html writes here). */
async function assertRenderSanity(page, pageErrors, { mainSelector = "#stage", minHeight = 20 } = {}) {
  const main = page.locator(mainSelector).first();
  await expect(main, `${mainSelector} should be visible`).toBeVisible();

  const box = await main.boundingBox();
  expect(box, `${mainSelector} should have a bounding box`).not.toBeNull();
  expect(box.height, `${mainSelector} height should be non-trivial (>= ${minHeight}px)`).toBeGreaterThanOrEqual(minHeight);

  const { scrollWidth, clientWidth } = await page.evaluate(() => ({
    scrollWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
  }));
  expect(scrollWidth, `no horizontal document scroll at phone width (scrollWidth=${scrollWidth}, clientWidth=${clientWidth})`).toBeLessThanOrEqual(clientWidth + 1);

  expect(pageErrors, `zero pageerror/console-error events, got: ${pageErrors.join(" | ")}`).toHaveLength(0);
}

/** Full-page PNG to the operator's morning-review gallery. Screenshots are
 * NOT committed to the repo (packet brief) — outDir is the scratchpad
 * gallery path, gitignored by construction (outside the repo entirely). */
async function screenshot(page, outDir, name) {
  fs.mkdirSync(outDir, { recursive: true });
  await page.screenshot({ path: path.join(outDir, `${name}.png`), fullPage: true });
}

module.exports = { collectPageErrors, assertRenderSanity, screenshot };
