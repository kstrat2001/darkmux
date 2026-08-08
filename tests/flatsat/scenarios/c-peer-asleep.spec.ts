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
// What this test verifies instead, honestly (QA finding TAKE 2 — an
// earlier version here claimed a flow-record-derived "peer stops producing
// NEW activity" proxy, but that assertion was VACUOUS: nothing in the test
// produces new activity whether or not the peer is actually paused, so it
// passed identically with the pause step deleted): the peer's own daemon
// becomes genuinely unreachable while frozen (not just quiet — connections
// hang, confirmed live), and the hub keeps rendering sanely throughout.
// Real, verified properties; not a liveness proxy. The stronger
// presence-TTL claim itself is `test.fixme` below with the reasoning
// above.
import { test, expect } from "@playwright/test";
import { execFileSync } from "node:child_process";
import { collectPageErrors, assertRenderSanity } from "../lib/render-sanity.js";

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

// QA finding (TAKE 2): the title + closing assertion below previously
// claimed to prove the peer "stops registering fresh flow activity via
// the shared stream" — but nothing in this test ever produces NEW
// activity in the first place (paused or not), so that before/after
// mission-id-set equality passed identically with the `pause-peer` call
// deleted entirely. It was vacuous, not evidence. Renamed to what this
// test actually demonstrates — the two properties it DOES verify, for
// real, with teeth: the peer becomes genuinely unreachable while frozen
// (not just quiet), and the hub keeps rendering sanely throughout.
test("pausing the peer makes it unreachable while the hub keeps rendering sanely", async ({ page }) => {
  const pageErrors = collectPageErrors(page);

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
});
