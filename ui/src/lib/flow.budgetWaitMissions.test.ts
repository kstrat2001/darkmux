// (#2902 step 5, 6th review MF) Two missions from ONE config share their
// hosted step's task id. Mission A waits on its endpoint's budget and is
// stopped; later mission B's same step waits. B must read REST
// "budget · <endpoint>" on the fleet card, its mission's run page and its
// step's run page. The producer scopes every budget record to its run
// (`budget.rs::record` -> `scope_to_run`, the id `mission_launch` stamps on
// the run's other records): with the bare `task-judge` both missions wrote
// before, A's `budget.stop` closed B's wait on every surface.
process.env.TZ = "UTC";
import { describe, it, expect } from "vitest";
import { shapeRecords, flowToRenderModel } from "./flow";
import { normAll } from "../testing/records";
import { runRegions } from "../lenses/session/sessionRun";
import { buildFleetCardBase, withLiveReadings } from "../lenses/fleet/cards";

const t0 = Date.parse("2026-09-27T10:00:00Z");
const iso = (ms: number) => new Date(ms).toISOString();
const A = "mission-A";
const B = "mission-B";
// The session a budget record carries: its run's (what the producer writes).
const taskOf = (m: string) => `task-judge-${m}`;
const rec = (at: number, action: string, sid: string, extra: Record<string, unknown> = {}) =>
  ({ ts: iso(at), action, session_id: sid, machine_uid: "M", machine_id: "M", payload: {}, ...extra });
const wait = (at: number, m: string) =>
  rec(at, "budget.wait", taskOf(m), {
    mission_id: m, category: "telemetry", source: "budget",
    payload: { scope: "endpoint", endpoint_id: "azure", wait_seconds: 86_000, resume_at: iso(at + 86_000_000) },
  });

describe("a later mission's wait is not closed by an earlier mission's stop", () => {
  const data = shapeRecords(normAll([
    rec(t0 - 20_000, "mission.start", A, { mission_id: A, source: "mission" }),
    rec(t0 - 19_000, "dispatch.start", A, { mission_id: A, source: "mission" }),
    rec(t0 - 15_000, "step.start", taskOf(A), { mission_id: A, source: "scheduler", handle: "judge" }),
    wait(t0 - 10_000, A),
    rec(t0 - 5_000, "budget.stop", taskOf(A), { mission_id: A, source: "budget", payload: { endpoint_id: "azure", reason: "mission `mission-A` is aborted" } }),
    rec(t0 - 4_000, "dispatch.error", A, { mission_id: A, source: "mission" }),
    rec(t0, "mission.start", B, { mission_id: B, source: "mission" }),
    rec(t0 + 1_000, "dispatch.start", B, { mission_id: B, source: "mission" }),
    rec(t0 + 2_000, "step.start", taskOf(B), { mission_id: B, source: "scheduler", handle: "judge" }),
    wait(t0 + 3_000, B),
  ]));
  const t = t0 + 3600_000;

  it("the fleet card reads REST budget · azure", () => {
    const card = withLiveReadings(buildFleetCardBase(data, new Map(), null, new Set(), false, "M", false, t), t);
    expect(card.liveTokState).toBe("rest");
  });

  it("B's mission run page and its step's run page read REST budget · azure", () => {
    for (const sid of [B, taskOf(B)]) {
      const v = runRegions(flowToRenderModel(data), sid, t);
      expect(v.liveTokScope?.state, sid).toBe("rest");
      expect((v.liveTokScope as { restReason?: string } | undefined)?.restReason, sid).toBe("budget · azure");
    }
  });
});
