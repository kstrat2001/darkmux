// Playback parity audit (2026-09-24, finding #7 at commit 02ce641d).
// `sessionRunning` used to run two DIFFERENT algorithms depending on
// `liveMode`: live kept `liveSet` (presence + a flow-derived TTL fallback
// measured from `Date.now()`); replay asked only "is there a close edge
// before t", with no staleness check at all. A session that started, sent
// one heartbeat, then went silent for 30 minutes with no terminal record
// reads `running=true` in replay and `running=false` under the live
// fallback at the same instant — the parity violation.
//
// The fix (Change A, one clock one derivation): ONE algorithm, over
// records up to `t`, with the same TTL measured from `t` in both modes.
// Presence is an optional ADDITIVE input — it can only make a session read
// as running, never suppress it — so it is exercised here as an empty set,
// matching what a replay caller always passes.
process.env.TZ = "UTC";
import { describe, it, expect } from "vitest";
import { normalizeRecords, sessionRunning, FLOW_LIVE_TTL_MS } from "./flow";

describe("sessionRunning: one algorithm, TTL measured from t (#7)", () => {
  const t0 = Date.parse("2026-09-24T03:00:00Z");
  const data = normalizeRecords([
    { ts: new Date(t0).toISOString(), action: "dispatch.start", session_id: "orph", machine_id: "M" },
    { ts: new Date(t0 + 10_000).toISOString(), action: "dispatch.turn.heartbeat", session_id: "orph", machine_id: "M" },
  ] as never);

  it("reads running while inside the TTL of its last heartbeat", () => {
    const probe = t0 + 10_000 + FLOW_LIVE_TTL_MS - 1_000;
    expect(sessionRunning(data, new Set(), "orph", probe)).toBe(true);
  });

  it("reads NOT running once probed past the TTL with no terminal record (the orphan case)", () => {
    // Probed 30 minutes after the session's last activity, well past
    // FLOW_LIVE_TTL_MS (5 min) and with no close edge at all. The OLD
    // replay algorithm (no close edge => running) read this as `true`;
    // the live flow-derived fallback already read it as `false`.
    const probe = t0 + 30 * 60_000;
    expect(sessionRunning(data, new Set(), "orph", probe)).toBe(false);
  });

  it("presence ADDS to the running set past the TTL rather than selecting a branch", () => {
    const probe = t0 + 30 * 60_000;
    expect(sessionRunning(data, new Set(["orph"]), "orph", probe)).toBe(true);
  });

  it("a close edge before t still wins over presence being silent", () => {
    const closed = normalizeRecords([
      ...data,
      { ts: new Date(t0 + 20_000).toISOString(), action: "dispatch.complete", session_id: "orph", machine_id: "M" },
    ] as never);
    expect(sessionRunning(closed, new Set(), "orph", t0 + 25_000)).toBe(false);
  });
});
