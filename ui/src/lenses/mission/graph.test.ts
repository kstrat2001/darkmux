import { describe, expect, it } from "vitest";
import {
  applyFlowRecord,
  applyRecordToMetrics,
  computeLayout,
  drawnEdges,
  fmtElapsed,
  fmtModel,
  fmtTok,
  foldFlowRecords,
  hhmmss,
  indexGraph,
  isAiKind,
  keepPageStatus,
  mergeGraphs,
  missionTotals,
  normalizeMissionStatus,
  phaseOrderEdges,
  recordInMission,
  seedMetricsFromGraph,
  statusFromRecord,
  statusRank,
  stepDisplayMetrics,
  stepForRecord,
  stepLead,
  stepMeterFor,
  stepSeat,
  STEP_LIVENESS_WINDOW_MS,
  tsToMs,
  type GraphNode,
  type MetricsMap,
  type MissionGraph,
} from "./graph";
import type { FlowRecord } from "../../types/handwritten";

function rec(over: Partial<FlowRecord> = {}): FlowRecord {
  return { ts: "2026-08-19T00:00:00Z", ...over };
}

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
  status: "complete",
  parentId: "p1",
  depth: 1,
  steps: [{ id: "b-step", label: "Dispatch", kind: "dispatch.single_shot", status: "complete" }],
};

function baseGraph(): MissionGraph {
  return {
    mission_id: "m1",
    mission_status: "active",
    nodes: [PHASE, TASK_A, TASK_B],
    edges: [
      { id: "e1", source: "p1", target: "a", kind: "contains" },
      { id: "e2", source: "p1", target: "b", kind: "contains" },
      { id: "e3", source: "a", target: "b", kind: "depends_on" },
    ],
  };
}

describe("computeLayout", () => {
  it("rebases each phase band to its own first column", () => {
    const deepTask: GraphNode = { ...TASK_B, id: "c", depth: 5, parentId: "p2" };
    const phase2: GraphNode = { id: "p2", label: "Adjudicate", kind: "phase", status: "planned", depth: 1 };
    const layout = computeLayout([PHASE, TASK_A, TASK_B, phase2, deepTask]);
    // phase2's band has one task at raw depth 5 — rebased to column 0
    // (minDepth=5), so its x matches TASK_A's own column-0 x, not a
    // depth-5 offset.
    expect(layout.positions["c"].x).toBe(layout.positions["a"].x);
  });

  it("stacks tasks within a column by taskPitch (grows with step count)", () => {
    const manySteps: GraphNode = {
      ...TASK_A,
      id: "many",
      steps: Array.from({ length: 5 }, (_, i) => ({ id: `s${i}`, label: `s${i}`, kind: "procedural.shell", status: "planned" })),
    };
    const sibling: GraphNode = { ...TASK_A, id: "sib" };
    const layout = computeLayout([PHASE, manySteps, sibling]);
    // sib is laid out after manySteps in the same column; its y must clear
    // manySteps's taller pitch.
    expect(layout.positions["sib"].y).toBeGreaterThan(layout.positions["many"].y + 100);
  });

  it("gives every phase a container box sized to its own band", () => {
    const layout = computeLayout([PHASE, TASK_A, TASK_B]);
    expect(layout.boxes["p1"]).toBeDefined();
    expect(layout.boxes["p1"].w).toBeGreaterThan(0);
    expect(layout.boxes["p1"].h).toBeGreaterThan(0);
  });
});

describe("statusRank / keepPageStatus", () => {
  it("ranks planned < running < terminal", () => {
    expect(statusRank("planned")).toBeLessThan(statusRank("running"));
    expect(statusRank("running")).toBeLessThan(statusRank("complete"));
  });

  it("an unknown status wins on ARRIVAL", () => {
    expect(keepPageStatus("running", "some-future-status")).toBe(false);
  });

  it("an unknown status never wins once HELD", () => {
    expect(keepPageStatus("some-future-status", "complete")).toBe(false);
  });

  it("equal rank keeps the page's existing value (the ratchet)", () => {
    expect(keepPageStatus("complete", "error")).toBe(true);
  });

  it("aborted ranks as a real terminal, not planned", () => {
    expect(statusRank("aborted")).toBe(statusRank("complete"));
  });

  it("normalizes the pre-rename 'closed' spelling to 'finalized'", () => {
    expect(normalizeMissionStatus("closed")).toBe("finalized");
    expect(normalizeMissionStatus("finalized")).toBe("finalized");
  });
});

describe("statusFromRecord / applyFlowRecord", () => {
  it("mission abort resolves to its own aborted terminal, not close", () => {
    expect(statusFromRecord(rec({ action: "mission abort" }))).toBe("aborted");
  });

  it("advances a node's status on a matching handle", () => {
    const idx = indexGraph(baseGraph());
    const g = applyFlowRecord({ ...baseGraph(), nodes: [{ ...PHASE, status: "planned" }, TASK_A, TASK_B] }, rec({ action: "phase start", handle: "p1" }), idx, "m1");
    expect(g.nodes[0].status).toBe("running");
  });

  it("never regresses a terminal status via a stale/replayed delta", () => {
    const idx = indexGraph(baseGraph());
    const g = applyFlowRecord(baseGraph(), rec({ action: "phase start", handle: "p1" }), idx, "m1");
    // p1 is already "complete" (rank 2); "running" (rank 1) must not win.
    expect(g.nodes[0].status).toBe("complete");
    expect(g).toBe(applyFlowRecord(g, rec({ action: "phase start", handle: "p1" }), idx, "m1"));
  });

  it("flips a step ROW inside its owning task, not the task's own status", () => {
    const idx = indexGraph(baseGraph());
    const running = { ...baseGraph(), nodes: [PHASE, { ...TASK_A, status: "running", steps: [{ ...TASK_A.steps![0], status: "planned" }] }, TASK_B] };
    const g = applyFlowRecord(running, rec({ action: "step start", handle: "a-step" }), idx, "m1");
    expect(g.nodes[1].steps![0].status).toBe("running");
    expect(g.nodes[1].status).toBe("running"); // untouched by the step flip
  });

  it("a record stamped for a DIFFERENT mission never flips this mission's status", () => {
    const idx = indexGraph(baseGraph());
    const g = applyFlowRecord({ ...baseGraph(), nodes: [{ ...PHASE, status: "planned" }, TASK_A, TASK_B] }, rec({ action: "phase start", handle: "p1", mission_id: "other-mission" }), idx, "m1");
    expect(g.nodes[0].status).toBe("planned");
  });

  it("a legacy record with no mission_id still flows through (present-is-authoritative, absent falls through)", () => {
    const idx = indexGraph(baseGraph());
    const g = applyFlowRecord({ ...baseGraph(), nodes: [{ ...PHASE, status: "planned" }, TASK_A, TASK_B] }, rec({ action: "phase start", handle: "p1" }), idx, "m1");
    expect(g.nodes[0].status).toBe("running");
  });

  it("returns the SAME reference when nothing changed (no state churn)", () => {
    const idx = indexGraph(baseGraph());
    const g = baseGraph();
    expect(applyFlowRecord(g, rec({ action: "dispatch.turn", handle: "unrelated" }), idx, "m1")).toBe(g);
  });

  it("foldFlowRecords applies a whole record set in order", () => {
    const idx = indexGraph(baseGraph());
    const planned = { ...baseGraph(), nodes: [{ ...PHASE, status: "planned" }, TASK_A, TASK_B] };
    const g = foldFlowRecords(
      planned,
      [rec({ action: "phase start", handle: "p1" }), rec({ action: "phase complete", handle: "p1" })],
      idx,
      "m1",
    );
    expect(g.nodes[0].status).toBe("complete");
  });
});

describe("mergeGraphs", () => {
  it("keeps the page's more-advanced status over a lagging disk snapshot", () => {
    const prev: MissionGraph = { ...baseGraph(), nodes: [{ ...PHASE, status: "running" }, TASK_A, TASK_B] };
    const fresh: MissionGraph = { ...baseGraph(), nodes: [{ ...PHASE, status: "planned" }, TASK_A, TASK_B] };
    const merged = mergeGraphs(prev, fresh);
    expect(merged.nodes[0].status).toBe("running");
  });

  it("also monotone-merges per-STEP status inside a task", () => {
    const prev: MissionGraph = { ...baseGraph(), nodes: [PHASE, { ...TASK_A, steps: [{ ...TASK_A.steps![0], status: "running" }] }, TASK_B] };
    const fresh: MissionGraph = { ...baseGraph(), nodes: [PHASE, { ...TASK_A, steps: [{ ...TASK_A.steps![0], status: "planned" }] }, TASK_B] };
    const merged = mergeGraphs(prev, fresh);
    expect(merged.nodes[1].steps![0].status).toBe("running");
  });

  it("with no prior graph, the fresh snapshot wins outright", () => {
    const fresh = baseGraph();
    expect(mergeGraphs(null, fresh)).toBe(fresh);
  });
});

describe("indexGraph / recordInMission / stepForRecord", () => {
  it("indexes phase/task/step ids and session correlation keys", () => {
    const idx = indexGraph(baseGraph());
    expect(idx.phaseIds.has("p1")).toBe(true);
    expect(idx.taskIds.has("a")).toBe(true);
    expect(idx.stepIds.has("a-step")).toBe(true);
    expect(idx.stepToTask["a-step"]).toBe("a");
    expect(idx.sessionToStep["step-a-step"]).toBe("a-step");
    expect(idx.sessions.has("task-a")).toBe(true);
  });

  it("recordInMission is authoritative on a present mission_id, even across a handle collision", () => {
    const idx = indexGraph(baseGraph());
    expect(recordInMission(rec({ mission_id: "other", handle: "a-step" }), idx, "m1")).toBe(false);
    expect(recordInMission(rec({ mission_id: "m1", handle: "unrelated" }), idx, "m1")).toBe(true);
  });

  it("recordInMission falls back to proxy matching when mission_id is absent", () => {
    const idx = indexGraph(baseGraph());
    expect(recordInMission(rec({ handle: "a-step" }), idx, "m1")).toBe(true);
    expect(recordInMission(rec({ session_id: "task-a" }), idx, "m1")).toBe(true);
    expect(recordInMission(rec({ payload: { step_id: "b-step" } }), idx, "m1")).toBe(true);
    expect(recordInMission(rec({ handle: "nope" }), idx, "m1")).toBe(false);
  });

  it("stepForRecord tries payload.step_id, then session_id, then handle, in order", () => {
    const idx = indexGraph(baseGraph());
    expect(stepForRecord(rec({ payload: { step_id: "b-step" } }), idx, "m1")).toBe("b-step");
    expect(stepForRecord(rec({ session_id: "step-a-step" }), idx, "m1")).toBe("a-step");
    expect(stepForRecord(rec({ handle: "b-step" }), idx, "m1")).toBe("b-step");
    expect(stepForRecord(rec({ handle: "not-a-step" }), idx, "m1")).toBeNull();
  });

  it("stepForRecord returns null for a different mission's record", () => {
    const idx = indexGraph(baseGraph());
    expect(stepForRecord(rec({ mission_id: "other", handle: "a-step" }), idx, "m1")).toBeNull();
  });
});

describe("isAiKind", () => {
  it("dispatch.* and review.probe/judge/verify kinds are AI-dispatching", () => {
    expect(isAiKind("dispatch.internal")).toBe(true);
    expect(isAiKind("review.probe:seat1")).toBe(true);
    expect(isAiKind("review.judge")).toBe(true);
    expect(isAiKind("mission.coder")).toBe(true);
  });
  it("procedural kinds are not", () => {
    expect(isAiKind("procedural.shell")).toBe(false);
    expect(isAiKind("review.bundle")).toBe(false);
  });
  it("-render kinds are excluded even though they share a probe/verify prefix", () => {
    expect(isAiKind("review.probe-render")).toBe(false);
    expect(isAiKind("review.verify-render")).toBe(false);
  });
});

describe("applyRecordToMetrics", () => {
  const idx = indexGraph(baseGraph());

  it("gates live token/turn folding on the step having actually STARTED", () => {
    let m: MetricsMap = {};
    // A telemetry.tokens record arrives before any start bookend. It still
    // updates `lastTs` (this port's liveness signal — see this module's own
    // doc), but must NOT fold into the token accounting: a not-yet-started
    // step must never show a phantom running total.
    m = applyRecordToMetrics(m, rec({ handle: "a-step", action: "telemetry.tokens", payload: { total_tokens: 500 } }), idx, "m1");
    expect(stepDisplayMetrics(m["a-step"]).tokens).toBe(0);
  });

  it("folds a full start -> turn -> token -> complete sequence", () => {
    let m: MetricsMap = {};
    m = applyRecordToMetrics(m, rec({ handle: "a-step", action: "dispatch start" }), idx, "m1");
    m = applyRecordToMetrics(m, rec({ handle: "a-step", action: "dispatch.turn", payload: { turns_so_far: 3 } }), idx, "m1");
    m = applyRecordToMetrics(m, rec({ handle: "a-step", action: "telemetry.tokens", category: "telemetry", source: "tokens", payload: { total_tokens: 120 } }), idx, "m1");
    m = applyRecordToMetrics(m, rec({ handle: "a-step", action: "dispatch complete", payload: { total_tokens: 500, total_turns: 3 } }), idx, "m1");
    const d = stepDisplayMetrics(m["a-step"]);
    expect(d.tokens).toBe(500); // finalized total wins over the running sum
    expect(d.turns).toBe(3);
    expect(d.localOk).toBe(true); // clean terminal, no endpoint -> positive local evidence
  });

  it("three-state attribution: an endpoint marks cloud; absence alone never implies local", () => {
    let m: MetricsMap = {};
    m = applyRecordToMetrics(m, rec({ handle: "b-step", action: "dispatch start" }), idx, "m1");
    m = applyRecordToMetrics(m, rec({ handle: "b-step", action: "dispatch.turn", payload: { endpoint: "https://x" } }), idx, "m1");
    m = applyRecordToMetrics(m, rec({ handle: "b-step", action: "dispatch error", payload: { endpoint: "https://x" } }), idx, "m1");
    expect(m["b-step"].cloud).toBe(true);
    expect(m["b-step"].localOk).toBe(false); // errored, no clean terminal -> no local claim
  });

  it("returns the SAME map reference when a record changes nothing", () => {
    let m: MetricsMap = {};
    m = applyRecordToMetrics(m, rec({ handle: "a-step", action: "dispatch start" }), idx, "m1");
    const m2 = applyRecordToMetrics(m, rec({ handle: "unrelated-step", action: "dispatch.turn" }), idx, "m1");
    expect(m2).toBe(m);
  });

  it("tool-call count tracks the authoritative tool_calls_so_far when present", () => {
    let m: MetricsMap = {};
    m = applyRecordToMetrics(m, rec({ handle: "a-step", action: "dispatch start" }), idx, "m1");
    m = applyRecordToMetrics(m, rec({ handle: "a-step", action: "dispatch.tool", payload: { tool_calls_so_far: 7 } }), idx, "m1");
    expect(stepDisplayMetrics(m["a-step"]).tools).toBe(7);
  });
});

describe("seedMetricsFromGraph", () => {
  it("seeds finalized totals and takes the max against a live value already climbing", () => {
    const g: MissionGraph = { ...baseGraph(), nodes: [PHASE, { ...TASK_A, steps: [{ ...TASK_A.steps![0], tokensFinal: 900, turnsFinal: 4, cloud: true }] }, TASK_B] };
    let m: MetricsMap = { "a-step": { tokRun: 950, tokFinal: 0, turnRun: 0, turnFinal: 0, toolRun: 0, toolFinal: 0, cloud: false, localOk: false, startTs: 0, endTs: 0, lastTs: 0 } };
    m = seedMetricsFromGraph(m, g);
    expect(m["a-step"].tokFinal).toBe(900);
    // the live running sum is untouched, but `stepDisplayMetrics` prefers
    // the FINALIZED total once one exists (mission-graph.html's own
    // `stepDisplayMetrics`: "the finalized total when the dispatch closed,
    // else the running per-turn sum" — not a max of the two).
    expect(stepDisplayMetrics(m["a-step"]).tokens).toBe(900);
  });

  it("is a no-op (same reference) when nothing in the graph has any backfill data", () => {
    const m: MetricsMap = {};
    expect(seedMetricsFromGraph(m, baseGraph())).toBe(m);
  });
});

describe("missionTotals", () => {
  it("splits local / cloud / unknown, never folding unknown into local", () => {
    const m: MetricsMap = {
      a: { tokRun: 0, tokFinal: 100, turnRun: 0, turnFinal: 1, toolRun: 0, toolFinal: 0, cloud: false, localOk: true, startTs: 0, endTs: 0, lastTs: 0 },
      b: { tokRun: 0, tokFinal: 50, turnRun: 0, turnFinal: 1, toolRun: 0, toolFinal: 0, cloud: true, localOk: false, startTs: 0, endTs: 0, lastTs: 0 },
      c: { tokRun: 0, tokFinal: 30, turnRun: 0, turnFinal: 1, toolRun: 0, toolFinal: 0, cloud: false, localOk: false, startTs: 0, endTs: 0, lastTs: 0 },
    };
    const tot = missionTotals(m);
    expect(tot).toEqual({ local: 100, cloud: 50, unknown: 30, total: 180, turns: 3 });
  });
});

describe("formatting helpers", () => {
  it("fmtTok compacts large numbers", () => {
    expect(fmtTok(0)).toBe("0");
    expect(fmtTok(999)).toBe("999");
    expect(fmtTok(1500)).toBe("1.5k");
    expect(fmtTok(15000)).toBe("15k");
    expect(fmtTok(2_500_000)).toBe("2.5m");
  });

  it("fmtModel strips the darkmux namespace and any leading vendor path", () => {
    expect(fmtModel("darkmux:qwen/qwen3.6-27b")).toBe("qwen3.6-27b");
    expect(fmtModel("plain-model")).toBe("plain-model");
    expect(fmtModel(undefined)).toBe("");
  });

  it("tsToMs detects seconds vs milliseconds epochs", () => {
    expect(tsToMs(1_700_000_000)).toBe(1_700_000_000_000); // seconds -> ms
    expect(tsToMs(1_700_000_000_000)).toBe(1_700_000_000_000); // already ms
    expect(tsToMs("2026-08-19T00:00:00Z")).toBe(Date.parse("2026-08-19T00:00:00Z"));
    expect(tsToMs(undefined)).toBe(0);
    expect(tsToMs("not a date")).toBe(0);
  });

  it("fmtElapsed renders m:ss, and h:mm:ss past an hour", () => {
    expect(fmtElapsed(0)).toBe("0:00");
    expect(fmtElapsed(65_000)).toBe("1:05");
    expect(fmtElapsed(3_661_000)).toBe("1:01:01");
  });

  it("hhmmss renders a local HH:MM:SS clock, empty for an unparseable ts", () => {
    expect(hhmmss("not a date")).toBe("");
    expect(hhmmss(Date.now())).toMatch(/^\d{2}:\d{2}:\d{2}$/);
  });

  it("stepLead falls back label -> kind -> 'step'; stepSeat pulls the colon suffix", () => {
    expect(stepLead({ id: "s", label: "", kind: "review.probe", status: "planned" })).toBe("review.probe");
    expect(stepSeat("review.probe:seat-1")).toBe("seat-1");
    expect(stepSeat("review.judge")).toBe("");
  });
});

describe("stepMeterFor liveness", () => {
  const step = { id: "a-step", label: "Shell", kind: "dispatch.internal", status: "running" };

  it("shows a generating pulse while the last signal is within the liveness window", () => {
    const now = 10_000_000;
    const m: MetricsMap = { "a-step": { tokRun: 0, tokFinal: 0, turnRun: 0, turnFinal: 0, toolRun: 0, toolFinal: 0, cloud: false, localOk: false, startTs: now - 5000, endTs: 0, lastTs: now - 1000 } };
    expect(stepMeterFor(step, m, now).generating).toBe(true);
  });

  it("stops claiming 'generating' once the last signal is older than the liveness window (a hard-killed dispatch)", () => {
    const now = 10_000_000;
    const m: MetricsMap = {
      "a-step": { tokRun: 0, tokFinal: 0, turnRun: 0, turnFinal: 0, toolRun: 0, toolFinal: 0, cloud: false, localOk: false, startTs: now - STEP_LIVENESS_WINDOW_MS - 5000, endTs: 0, lastTs: now - STEP_LIVENESS_WINDOW_MS - 1000 },
    };
    expect(stepMeterFor(step, m, now).generating).toBe(false);
  });
});

describe("drawnEdges / phaseOrderEdges", () => {
  it("drops contains edges (drawn by phase-container enclosure) and synthesizes phase-order edges", () => {
    const g = baseGraph();
    const drawn = drawnEdges(g.edges, g.nodes);
    expect(drawn.some((e) => e.kind === "contains")).toBe(false);
    expect(drawn.some((e) => e.kind === "depends_on")).toBe(true);
  });

  it("produces zero phase-order edges for a single-phase mission", () => {
    expect(phaseOrderEdges([PHASE])).toHaveLength(0);
  });

  it("produces N-1 consecutive phase-order edges for N phases, ordered by depth", () => {
    const p2: GraphNode = { id: "p2", label: "Adjudicate", kind: "phase", status: "planned", depth: 1 };
    const p3: GraphNode = { id: "p3", label: "Report", kind: "phase", status: "planned", depth: 2 };
    const edges = phaseOrderEdges([p3, PHASE, p2]);
    expect(edges.map((e) => [e.source, e.target])).toEqual([
      ["p1", "p2"],
      ["p2", "p3"],
    ]);
  });
});
