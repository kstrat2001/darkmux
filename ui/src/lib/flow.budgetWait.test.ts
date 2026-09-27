// (#2902 step 5, 5th review MF1) A HOSTED call held by its endpoint's budget
// writes `budget.wait` BEFORE any `dispatch start` (contract 2: the gate runs
// before the bookends open). The liveness gates used to open only on a
// `dispatch start`, so a standalone hosted dispatch waiting on its budget read
// as nothing at all. An OPEN wait (no `budget.resume`/`budget.stop`/terminal
// after it, and not silent past its resume time) is live work.
process.env.TZ = "UTC";
import { describe, it, expect } from "vitest";
import { shapeRecords, sessionRunning, flowLiveSessions, openBudgetWait, BUDGET_WAIT_GRACE_MS, FLOW_LIVE_TTL_MS } from "./flow";
import { normAll } from "../testing/records";

const t0 = Date.parse("2026-09-27T10:00:00Z");
const iso = (ms: number) => new Date(ms).toISOString();
const wait = (at: number, secs: number, sid = "s1") => ({
  ts: iso(at),
  action: "budget.wait",
  session_id: sid,
  machine_id: "M",
  payload: { scope: "endpoint", endpoint_id: "azure", wait_seconds: secs, resume_at: iso(at + secs * 1000) },
});
const rec = (at: number, action: string, sid = "s1") => ({ ts: iso(at), action, session_id: sid, machine_id: "M", payload: {} });

describe("an open budget wait is live work", () => {
  // A day window: the wait is announced once, hours before the probe, far
  // past the flow TTL. Its announced resume time is what bounds it.
  const day = 23 * 3600 + 53 * 60;
  const data = shapeRecords(normAll([wait(t0, day)]));

  it("reads running (replay: no presence) through a long wait, far past the flow TTL", () => {
    const probe = t0 + 3 * 3600_000;
    expect(probe - t0).toBeGreaterThan(FLOW_LIVE_TTL_MS);
    expect(openBudgetWait(data, probe)?.action).toBe("budget.wait");
    expect(sessionRunning(data, new Set(), "s1", probe)).toBe(true);
    expect(flowLiveSessions(data, probe, true).has("s1")).toBe(true);
  });

  it("is not open before it was announced (playback before the wait)", () => {
    expect(sessionRunning(data, new Set(), "s1", t0 - 1_000)).toBe(false);
  });

  it("closes on budget.resume, budget.stop, or a terminal", () => {
    for (const end of ["budget.resume", "budget.stop", "dispatch.error"]) {
      const d = shapeRecords(normAll([wait(t0, 600), rec(t0 + 60_000, end)]));
      expect(openBudgetWait(d, t0 + 61_000), end).toBeNull();
      expect(sessionRunning(d, new Set(), "s1", t0 + 61_000), end).toBe(false);
      expect(flowLiveSessions(d, t0 + 61_000, true).has("s1"), end).toBe(false);
    }
  });

  it("a waiter silent past its resume time plus the grace has died: not live", () => {
    const d = shapeRecords(normAll([wait(t0, 600)]));
    expect(sessionRunning(d, new Set(), "s1", t0 + 600_000 + BUDGET_WAIT_GRACE_MS - 1)).toBe(true);
    const after = t0 + 600_000 + BUDGET_WAIT_GRACE_MS + FLOW_LIVE_TTL_MS + 1;
    expect(openBudgetWait(d, t0 + 600_000 + BUDGET_WAIT_GRACE_MS + 1)).toBeNull();
    expect(sessionRunning(d, new Set(), "s1", after)).toBe(false);
    expect(flowLiveSessions(d, after, true).has("s1")).toBe(false);
  });

  it("a re-announced wait (still full at its resume time) stays open to the NEW resume time", () => {
    const d = shapeRecords(normAll([wait(t0, 60), wait(t0 + 61_000, 600)]));
    expect(openBudgetWait(d, t0 + 61_000 + 300_000)).not.toBeNull();
  });
});
