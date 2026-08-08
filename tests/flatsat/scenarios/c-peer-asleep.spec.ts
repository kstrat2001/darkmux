// Scenario (c) peer-asleep — pause the peer container (SIGSTOP via `docker
// pause`, freezing the process without killing it) and confirm liveness
// does not show it "running" forever.
//
// KNOWN ARCHITECTURE GAP, found while building this scenario (not a
// flatsat bug): darkmux's presence heartbeat
// (crates/darkmux-flow/src/presence.rs::spawn_emitter_thread) self-disables
// whenever `darkmux_hardware::machine_uid()` returns `None`, and that
// function returns `None` unconditionally on any non-macOS `std::env::
// consts::OS` (it shells out to `ioreg`, a macOS-only binary). The flatsat
// containers run linux/arm64 (Docker Desktop's VM — see Dockerfile's
// module doc for why a host-built binary can't run there anyway), so
// NEITHER hub NOR peer ever publishes a presence heartbeat: `/fleet/
// machines/live` and `/fleet/sessions/live` are permanently `[]` in this
// harness, on every machine, always — there is no live-presence signal to
// pause-and-watch-expire in the first place. This is a real fleet-wide
// darkmux constraint the flatsat SURFACES rather than one it introduces;
// see the packet report's risk register and the runbook's FOLLOW-UPS
// ledger for the concrete fix shape (a non-macOS machine_uid fallback,
// e.g. `/etc/machine-id`, so presence has something to key on off-Mac).
//
// What this test verifies instead, honestly: the flow-record-derived
// liveness surface (the fleet dashboard's activity attribution, which does
// NOT depend on presence) correctly reflects that the peer stops producing
// NEW activity once paused — the closest true-to-architecture proxy this
// environment can exercise. The stronger presence-TTL claim itself is
// `test.fixme` below with this same reasoning.
import { test, expect } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { collectPageErrors, assertRenderSanity } from "../lib/render-sanity.js";
import { SERVE_TOKEN } from "../lib/paths.js";

// Node-context fetch()es (unlike page.goto/page navigation, which inherits
// playwright.config.js's context-wide extraHTTPHeaders) don't carry the
// bearer token automatically — see lib/paths.js's SERVE_TOKEN comment.
const authed = { headers: { Authorization: `Bearer ${SERVE_TOKEN}` } };

test.afterEach(async () => {
  // Idempotent cleanup: the declarative fixme test never actually pauses
  // the peer, so `docker unpause` on an already-running container is
  // expected to no-op-fail there — never let that fail the suite.
  try {
    execFileSync("bash", ["../inject.sh", "unpause-peer"], { cwd: __dirname, stdio: "ignore" });
  } catch {
    /* not paused — nothing to undo */
  }
});

// Declarative fixme — a real test whose BODY never runs (Playwright's
// `test.fixme(title, body)` form). Left unimplemented on purpose: there is
// nothing to assert here in this environment. Verified empirically before
// writing this file: `curl .../fleet/machines/live` against the live
// flatsat hub returns `[]` even with both hub and peer running — see this
// file's header comment for why (machine_uid() is macOS-only).
test.fixme(
  "presence-based liveness ages the peer out after its heartbeat TTL expires",
  async () => {
    // What a real macOS fleet member WOULD let this assert:
    //   1. GET /fleet/machines/live includes the peer's machine_uid while
    //      it's running.
    //   2. docker pause the peer (heartbeat stops).
    //   3. Within ~15s (DEFAULT_TTL_SECS, presence.rs), the peer's entry
    //      expires out of /fleet/machines/live.
    // Left unimplemented — see the file header for why no container in
    // this harness ever publishes a presence heartbeat in the first place.
  }
);

test("pausing the peer stops it registering fresh flow activity via the shared stream", async ({ page }) => {
  const pageErrors = collectPageErrors(page);
  const beforeRes = await fetch("http://127.0.0.1:18765/flow-missions", authed);
  const before = await beforeRes.json();
  const beforePeerMissions = new Set(
    (before.missions || []).filter((m: any) => (m.machines || []).includes("flatsat-peer")).map((m: any) => m.mission_id)
  );

  execFileSync("bash", ["../inject.sh", "pause-peer"], { cwd: __dirname, stdio: "inherit" });

  // The peer container is frozen — it cannot accept new work or emit new
  // records while paused. Confirm its own daemon is genuinely unreachable
  // (not just quiet) while paused: a frozen process can't answer /health.
  await expect(async () => {
    await expect(fetch("http://127.0.0.1:18766/health", { signal: AbortSignal.timeout(1000) })).rejects.toBeTruthy();
  }).toPass({ timeout: 5000 });

  // The hub's own view must keep rendering regardless of the peer's state.
  await page.goto("/");
  await expect(page.locator("#stage .fleet")).toBeAttached({ timeout: 15000 });
  await assertRenderSanity(page, pageErrors);

  execFileSync("bash", ["../inject.sh", "unpause-peer"], { cwd: __dirname, stdio: "inherit" });
  await expect(async () => {
    const res = await fetch("http://127.0.0.1:18766/health");
    expect(res.ok).toBe(true);
  }).toPass({ timeout: 15000 });

  // No NEW mission_ids attributed to the peer should have appeared while
  // it was frozen (it's a static snapshot; a genuinely-live peer producing
  // work would grow this set — a real, if presence-blind, liveness proxy).
  const afterRes = await fetch("http://127.0.0.1:18765/flow-missions", authed);
  const after = await afterRes.json();
  const afterPeerMissions = new Set(
    (after.missions || []).filter((m: any) => (m.machines || []).includes("flatsat-peer")).map((m: any) => m.mission_id)
  );
  expect([...afterPeerMissions]).toEqual([...beforePeerMissions]);
});
