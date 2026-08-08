// Scenario (e) empty-vs-idle — a fresh state dir with ZERO records must
// render its empty state without console errors. Deliberately NOT the
// hub or peer (both are seeded by design) — spins its own throwaway
// third `darkmux serve` container against a brand-new empty tempdir, on
// its own port, with no Redis configured at all (the simplest possible
// "nothing has ever happened here" fleet). Torn down at the end of the
// test regardless of outcome.
import { test, expect } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { collectPageErrors, assertRenderSanity, screenshot } from "../lib/render-sanity.js";
import { GALLERY_DIR, SERVE_TOKEN } from "../lib/paths.js";

const EMPTY_PORT = 18767;
const CONTAINER = "flatsat-empty";

test("a fresh, zero-record state dir renders its empty state without console errors", async ({ page }) => {
  const stateDir = mkdtempSync(path.join(tmpdir(), "flatsat-empty-"));
  execFileSync("docker", [
    "run", "-d", "--rm",
    "--name", CONTAINER,
    "-p", `${EMPTY_PORT}:8765`,
    "-v", `${stateDir}:/state`,
    "-e", "DARKMUX_HOME=/state",
    // DARKMUX_HOME alone does NOT relocate the flows dir (see
    // docker-compose.yml's DARKMUX_FLOWS_DIR comment) — set explicitly
    // even though this scenario expects it empty either way.
    "-e", "DARKMUX_FLOWS_DIR=/state/flows",
    "-e", "DARKMUX_MACHINE_ID=flatsat-empty",
    // Same non-loopback-binding auth requirement as hub/peer — see
    // lib/paths.js's SERVE_TOKEN comment.
    "-e", `DARKMUX_SERVE_TOKEN=${SERVE_TOKEN}`,
    "darkmux-flatsat:local",
    "serve", "--bind", "0.0.0.0", "--port", "8765",
  ]);

  try {
    // Poll /health rather than a fixed sleep — a cold container start can
    // legitimately take a couple seconds.
    await expect(async () => {
      const res = await fetch(`http://127.0.0.1:${EMPTY_PORT}/health`);
      expect(res.ok).toBe(true);
    }).toPass({ timeout: 15000 });

    const pageErrors = collectPageErrors(page);
    await page.goto(`http://127.0.0.1:${EMPTY_PORT}/`);
    await expect(page.locator("#stage .fleet")).toBeAttached({ timeout: 15000 });
    await assertRenderSanity(page, pageErrors);

    const stageText = await page.locator("#stage").innerText();
    expect(stageText.length, "empty-fleet render should still produce SOME content (an empty state, not a blank stage)").toBeGreaterThan(0);
    // The corpus-seeded hub/peer identities must never leak into an
    // unrelated empty instance — would indicate state bleeding across
    // containers via a shared volume or a hardcoded fixture path.
    expect(stageText).not.toContain("flatsat-hub");
    expect(stageText).not.toContain("flatsat-peer");

    await screenshot(page, GALLERY_DIR, "empty-vs-idle");
  } finally {
    execFileSync("docker", ["rm", "-f", CONTAINER], { stdio: "ignore" });
    rmSync(stateDir, { recursive: true, force: true });
  }
});
