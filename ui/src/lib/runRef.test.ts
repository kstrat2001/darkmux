import { describe, expect, it } from "vitest";
import { currentRun, grainOf, groupOfRecords, recordsOfGroup, refAt, runIndex, runRecords, sessionRun, __runIndexBuilds } from "./runRef";
import { lifecycleAt, DEFAULT_POLICY } from "./lifecycle";
import { normAll, type RawRecord } from "../testing/records";

const at = (s: number) => new Date(Date.parse("2026-09-27T10:00:00Z") + s * 1000).toISOString();
const rec = (s: number, action: string, extra: RawRecord = {}): RawRecord => ({ ts: at(s), action, session_id: "task-probe", machine_uid: "u1", ...extra });

// Two review missions run the same config, so their probe step shares one
// deterministic session id (#2125). Mission A ran 0-60 s and finished;
// mission B started at 600 s and is still running.
const shared = normAll([
  rec(0, "dispatch.start", { mission_id: "A" }),
  rec(60, "dispatch.complete", { mission_id: "A" }),
  rec(600, "dispatch.start", { mission_id: "B" }),
  rec(610, "dispatch.turn.heartbeat", { mission_id: "B" }),
]);

describe("runIndex: a session id two missions share", () => {
  it("is two runs, one per mission", () => {
    const groups = runIndex(shared).groupsOfSession("task-probe");
    expect(groups.map((g) => g.missionId)).toEqual(["A", "B"]);
  });

  it("never lets one mission's terminal close the other", () => {
    const b = runRecords(shared, { sessionId: "task-probe", missionId: "B", attempt: 0 })!;
    expect(lifecycleAt(b, Date.parse(at(620)), DEFAULT_POLICY).phase).toBe("open");
    const a = runRecords(shared, { sessionId: "task-probe", missionId: "A", attempt: 0 })!;
    expect(lifecycleAt(a, Date.parse(at(620)), DEFAULT_POLICY).phase).toBe("closed");
  });

  it("a session route reads the mission that ran last", () => {
    expect(sessionRun(shared, "task-probe", Date.parse(at(620)))?.ref.missionId).toBe("B");
    expect(sessionRun(shared, "task-probe", Date.parse(at(100)))?.ref.missionId).toBe("A");
  });
});

describe("runIndex: attribution of a record naming no mission", () => {
  it("joins its session's one mission", () => {
    const data = normAll([rec(0, "dispatch.start", { mission_id: "A" }), rec(60, "session.end")]);
    const groups = runIndex(data).groupsOfSession("task-probe");
    expect(groups).toHaveLength(1);
    expect(lifecycleAt(currentRun(groups[0], Date.parse(at(90))), Date.parse(at(90)), DEFAULT_POLICY).phase).toBe("closed");
  });

  it("joins the run open at its time when its session spans several missions, never a group of its own", () => {
    const data = normAll([...shared.map((r) => ({ ...r }) as unknown as RawRecord), rec(700, "session.end")]);
    const groups = runIndex(data).groupsOfSession("task-probe");
    expect(groups.map((g) => g.missionId)).toEqual(["A", "B"]);
    const t = Date.parse(at(720));
    expect(lifecycleAt(currentRun(groups[1], t), t, DEFAULT_POLICY).phase).toBe("closed");
    expect(lifecycleAt(currentRun(groups[0], t), t, DEFAULT_POLICY).close?.edge.kind).toBe("complete");
  });
});

describe("runIndex: machines and caching", () => {
  it("lists a machine's groups", () => {
    const data = normAll([rec(0, "dispatch.start", { session_id: "s1" }), rec(0, "dispatch.start", { session_id: "s2", machine_uid: "u2" })]);
    expect(runIndex(data).groupsOn("u2").map((g) => g.sessionId)).toEqual(["s2"]);
    expect(runIndex(data).groupsOn("u3")).toEqual([]);
  });

  it("is built once per window array", () => {
    const before = __runIndexBuilds();
    runIndex(shared);
    runIndex(shared);
    expect(__runIndexBuilds() - before).toBeLessThanOrEqual(1);
  });
});

describe("attempts", () => {
  const relaunched = normAll([rec(0, "dispatch.start"), rec(60, "dispatch.complete"), rec(300, "dispatch.start")]);
  const g = groupOfRecords(relaunched);

  it("a relaunch under the same id is a second attempt", () => {
    expect(g.attempts).toHaveLength(2);
    expect(refAt(g, Date.parse(at(100))).attempt).toBe(0);
    expect(refAt(g, Date.parse(at(400))).attempt).toBe(1);
  });

  it("before the relaunch the run is closed, after it open", () => {
    expect(lifecycleAt(currentRun(g, Date.parse(at(100))), Date.parse(at(100)), DEFAULT_POLICY).phase).toBe("closed");
    expect(lifecycleAt(currentRun(g, Date.parse(at(400))), Date.parse(at(400)), DEFAULT_POLICY).phase).toBe("open");
  });

  it("an attempt a relaunch superseded with no close of its own reads stopped", () => {
    const g2 = groupOfRecords(normAll([rec(0, "dispatch.start"), rec(300, "dispatch.start")]));
    const t = Date.parse(at(310));
    expect(lifecycleAt(recordsOfGroup(g2, { sessionId: "task-probe", missionId: null, attempt: 0 }), t, DEFAULT_POLICY).phase).toBe("stale");
    expect(lifecycleAt(currentRun(g2, t), t, DEFAULT_POLICY).phase).toBe("open");
  });

  it("a wait before the start is the same attempt as the start", () => {
    const waited = groupOfRecords(normAll([rec(0, "budget.wait", { payload: { wait_seconds: 30 } }), rec(40, "budget.resume"), rec(41, "dispatch.start")]));
    expect(waited.attempts).toHaveLength(1);
    expect(waited.attempts[0].start?.tMs).toBe(Date.parse(at(41)));
  });
});

describe("grainOf", () => {
  it("a mission's whole-run bookend is run grain", () => {
    expect(grainOf(normAll([rec(0, "dispatch.start", { source: "mission" })]))).toBe("run");
  });
  it("a dispatch, a budget wait or a heartbeat is execution grain", () => {
    expect(grainOf(normAll([rec(0, "dispatch.start", { source: "crew_dispatch" })]))).toBe("execution");
    expect(grainOf(normAll([rec(0, "budget.wait")]))).toBe("execution");
    expect(grainOf(normAll([rec(0, "dispatch.turn.heartbeat")]))).toBe("execution");
  });
  it("scheduler and mission bookkeeping is lifecycle grain", () => {
    expect(grainOf(normAll([rec(0, "step.start"), rec(1, "mission.start")]))).toBe("lifecycle");
  });
});

describe("a mission's whole-run bookend (lifecycle rule 6)", () => {
  // The bookend never beats; its step does, on its own session. The run is
  // in flight while its step is, and waits while its step waits.
  const data = normAll([
    rec(0, "dispatch.start", { session_id: "m1", mission_id: "m1", source: "mission" }),
    rec(5, "step.start", { session_id: "task-probe-m1", mission_id: "m1" }),
    rec(10, "budget.wait", { session_id: "task-probe-m1", mission_id: "m1", payload: { wait_seconds: 86_000 } }),
  ]);
  const bookend = runIndex(data).groupsOfSession("m1")[0];

  it("reads its mission's other runs as its own activity", () => {
    expect(bookend.grain).toBe("run");
    expect(bookend.siblings.map((g) => g.sessionId)).toEqual(["task-probe-m1"]);
    const t = Date.parse(at(3 * 3600));
    expect(lifecycleAt(currentRun(bookend, t), t, DEFAULT_POLICY).phase).toBe("waiting");
  });

  it("an execution's siblings are not its activity", () => {
    expect(runIndex(data).groupsOfSession("task-probe-m1")[0].siblings).toEqual([]);
  });
});
