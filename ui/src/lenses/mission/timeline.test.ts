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
