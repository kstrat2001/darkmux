import { describe, expect, it } from "vitest";
import { groupTimeline, initMinimap, isNarrowViewport, persistMinimap, taskAggMetrics, timelineActive } from "./timeline";
import type { GraphEdge, GraphNode, MetricsMap } from "./graph";

const PHASE: GraphNode = { id: "p1", label: "Investigate", kind: "phase", status: "complete", depth: 0 };
const TASK_A: GraphNode = {
  id: "a",
  label: "bundle",
  kind: "task",
  status: "complete",
  parentId: "p1",
  depth: 0,
  steps: [{ id: "a-step", label: "Shell", kind: "procedural.shell", status: "complete" }],
};
const TASK_B: GraphNode = {
  id: "b",
  label: "probe",
  kind: "task",
  status: "planned",
  parentId: "p1",
  depth: 1,
  steps: [{ id: "b-step", label: "Dispatch", kind: "dispatch.internal", status: "running" }],
};
const EDGES: GraphEdge[] = [{ id: "e", source: "a", target: "b", kind: "depends_on" }];

describe("isNarrowViewport / timelineActive", () => {
  it("is inclusive at exactly 700px, matching the CSS breakpoint", () => {
    expect(isNarrowViewport(700)).toBe(true);
    expect(isNarrowViewport(701)).toBe(false);
  });

  it("auto mode picks the timeline only when narrow", () => {
    expect(timelineActive("auto", true)).toBe(true);
    expect(timelineActive("auto", false)).toBe(false);
  });

  it("an explicit viewMode overrides the viewport", () => {
    expect(timelineActive("timeline", false)).toBe(true);
    expect(timelineActive("canvas", true)).toBe(false);
  });
});

function fakeStorage(initial: Record<string, string> = {}) {
  const store = { ...initial };
  return {
    getItem: (k: string) => (k in store ? store[k] : null),
    setItem: (k: string, v: string) => {
      store[k] = v;
    },
    store,
  };
}

describe("minimap persistence", () => {
  it("defaults OFF with nothing persisted", () => {
    expect(initMinimap(fakeStorage())).toBe(false);
  });

  it("honors a persisted '1'/'0' choice", () => {
    expect(initMinimap(fakeStorage({ "dmux.mmap": "1" }))).toBe(true);
    expect(initMinimap(fakeStorage({ "dmux.mmap": "0" }))).toBe(false);
  });

  it("persistMinimap writes the same key initMinimap reads", () => {
    const storage = fakeStorage();
    persistMinimap(true, storage);
    expect(storage.store["dmux.mmap"]).toBe("1");
    persistMinimap(false, storage);
    expect(storage.store["dmux.mmap"]).toBe("0");
  });

  it("a storage that throws degrades to the default rather than crashing", () => {
    const throwing = {
      getItem: () => {
        throw new Error("blocked");
      },
    };
    expect(initMinimap(throwing)).toBe(false);
  });
});

describe("taskAggMetrics", () => {
  it("aggregates a task's own step metrics and generating state", () => {
    const now = 10_000;
    const metrics: MetricsMap = {
      "b-step": { tokRun: 40, tokFinal: 0, turnRun: 2, turnFinal: 0, toolRun: 0, toolFinal: 0, cloud: true, localOk: false, startTs: now - 5000, endTs: 0, lastTs: now - 1000 },
    };
    const agg = taskAggMetrics(TASK_B, metrics, now);
    expect(agg.tokens).toBe(40);
    expect(agg.turns).toBe(2);
    expect(agg.cloud).toBe(true);
    expect(agg.generating).toBe(true);
    expect(agg.elapsedMs).toBe(5000);
  });

  it("shows even with zero metrics when the task holds an AI-dispatching step", () => {
    const agg = taskAggMetrics(TASK_B, {}, 0);
    expect(agg.show).toBe(true);
  });
});

describe("groupTimeline", () => {
  it("groups tasks under their phase, sorted by depth, with per-task wait labels", () => {
    const groups = groupTimeline([PHASE, TASK_A, TASK_B], EDGES, {}, 0);
    expect(groups).toHaveLength(1);
    expect(groups[0].phase.id).toBe("p1");
    expect(groups[0].tasks.map((t) => t.task.id)).toEqual(["a", "b"]);
    expect(groups[0].tasks[1].waitsOn).toEqual(["bundle"]);
    expect(groups[0].tasks[0].waitsOn).toEqual([]);
  });

  it("attaches a per-step meter to every step row", () => {
    const groups = groupTimeline([PHASE, TASK_A], [], {}, 0);
    expect(groups[0].tasks[0].steps).toHaveLength(1);
    expect(groups[0].tasks[0].steps[0].step.id).toBe("a-step");
    expect(groups[0].tasks[0].steps[0].meter).toBeDefined();
  });

  it("a freeform mission (tasks, no phase nodes) gets one implicit 'tasks' section", () => {
    const freeformTask: GraphNode = { ...TASK_A, parentId: undefined };
    const groups = groupTimeline([freeformTask], [], {}, 0);
    expect(groups).toHaveLength(1);
    expect(groups[0].phase.id).toBe("__none__");
    expect(groups[0].phase.label).toBe("tasks");
  });

  it("a mission with no tasks at all produces zero sections", () => {
    expect(groupTimeline([], [], {}, 0)).toHaveLength(0);
  });
});

// (#2269) The task row's timer read the RUNNING step's elapsed — it restarted
// on every step of a sequential task and vanished once the last step
// finished — while tokens and turns on the same row were summed.
describe("taskAggMetrics task-level duration (#2269)", () => {
  const T0 = 1_756_900_000_000; // epoch ms: `tsToMs` reads small numbers as SECONDS
  const seq = (s1: Partial<GraphNode["steps"] extends (infer S)[] | undefined ? S : never>, s2: typeof s1, status = "running"): GraphNode => ({
    id: "t",
    label: "crawl",
    kind: "task",
    status,
    depth: 1,
    steps: [
      { id: "s1", label: "u-0001", kind: "dispatch.internal", status: "complete", ...s1 },
      { id: "s2", label: "u-0002", kind: "dispatch.internal", status: "running", ...s2 },
    ],
  });

  it("spans from the earliest step start to now while any step runs, not from the running step's start", () => {
    const now = T0 + 100_000;
    const task = seq({ startedTs: T0, completedTs: T0 + 60_000 }, { startedTs: T0 + 61_000 });
    const agg = taskAggMetrics(task, {}, now);
    expect(agg.generating).toBe(true);
    expect(agg.spanMs).toBe(100_000);
    expect(agg.elapsedMs).toBe(100_000);
    expect(agg.sumMs).toBe(60_000 + 39_000);
  });

  it("keeps a duration once every step is done: earliest start to latest end", () => {
    const now = T0 + 500_000;
    const task = seq({ startedTs: T0, completedTs: T0 + 60_000 }, { startedTs: T0 + 61_000, completedTs: T0 + 90_000, status: "complete" }, "complete");
    const agg = taskAggMetrics(task, {}, now);
    expect(agg.generating).toBe(false);
    expect(agg.spanMs).toBe(90_000);
    expect(agg.sumMs).toBe(60_000 + 29_000);
  });

  it("prefers the metrics stream's own start/end over the node's timestamps, like the per-step meter does", () => {
    const now = T0 + 100_000;
    const task = seq({ startedTs: T0 + 5_000 }, { startedTs: T0 + 70_000 });
    const metrics: MetricsMap = {
      s1: { tokRun: 0, tokFinal: 0, turnRun: 0, turnFinal: 0, toolRun: 0, toolFinal: 0, cloud: false, localOk: false, startTs: T0, endTs: T0 + 50_000, lastTs: T0 + 50_000 },
    };
    const agg = taskAggMetrics(task, metrics, now);
    expect(agg.spanMs).toBe(100_000);
    expect(agg.sumMs).toBe(50_000 + 30_000);
  });

  it("a task whose steps never started has no duration", () => {
    const task: GraphNode = { id: "t", label: "x", kind: "task", status: "planned", depth: 1, steps: [{ id: "s", label: "s", kind: "dispatch.internal", status: "planned" }] };
    const agg = taskAggMetrics(task, {}, T0);
    expect(agg.spanMs).toBe(0);
    expect(agg.sumMs).toBe(0);
    expect(agg.elapsedMs).toBe(0);
  });
});
