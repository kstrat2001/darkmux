import { describe, it, expect } from "vitest";
import {
  shortModel,
  labFieldVal,
  runActivity,
  runsAgo,
  runSubtitle,
  runsMultiMachine,
  runsFiltered,
  labTaskKey,
  groupLabRunsByTask,
  labKnobSummary,
  labKnobDiff,
  labCounts,
} from "./format";
import type { Run } from "../../types/generated/Run";
import type { LabRun } from "../../types/handwritten";

function run(over: Partial<Run> & Pick<Run, "id" | "kind" | "status" | "tracked">): Run {
  return over;
}

describe("shortModel", () => {
  it("strips the darkmux: load-namespace prefix", () => {
    expect(shortModel("darkmux:qwen3.6-35b-a3b")).toBe("qwen3.6-35b-a3b");
  });
  it("passes through a bare model id unchanged", () => {
    expect(shortModel("gpt-4o")).toBe("gpt-4o");
  });
  it("treats null/undefined as empty", () => {
    expect(shortModel(null)).toBe("");
    expect(shortModel(undefined)).toBe("");
  });
});

describe("labFieldVal", () => {
  it("renders an em-dash for null/undefined", () => {
    expect(labFieldVal(null)).toBe("—");
    expect(labFieldVal(undefined)).toBe("—");
  });
  it("shortens a darkmux-namespaced string value", () => {
    expect(labFieldVal("darkmux:foo")).toBe("foo");
  });
  it("stringifies a non-string value verbatim", () => {
    expect(labFieldVal(4)).toBe("4");
  });
});

describe("runActivity", () => {
  it("prefers updated_ts, then completed_ts, then started_ts", () => {
    expect(runActivity(run({ id: "a", kind: "dispatch", status: "complete", tracked: true, updated_ts: 3, completed_ts: 2, started_ts: 1 }))).toBe(3);
    expect(runActivity(run({ id: "a", kind: "dispatch", status: "complete", tracked: true, completed_ts: 2, started_ts: 1 }))).toBe(2);
    expect(runActivity(run({ id: "a", kind: "dispatch", status: "complete", tracked: true, started_ts: 1 }))).toBe(1);
    expect(runActivity(run({ id: "a", kind: "dispatch", status: "complete", tracked: true }))).toBe(0);
  });
});

describe("runsAgo", () => {
  const now = 1_000_000 * 1000; // ms
  it("renders empty for a run with no activity timestamp at all", () => {
    expect(runsAgo(run({ id: "a", kind: "dispatch", status: "complete", tracked: true }), now)).toBe("");
  });
  it("formats seconds/minutes/hours/days per the legacy thresholds", () => {
    const base = Math.floor(now / 1000);
    expect(runsAgo(run({ id: "a", kind: "dispatch", status: "complete", tracked: true, updated_ts: base - 30 }), now)).toBe("30s ago");
    expect(runsAgo(run({ id: "a", kind: "dispatch", status: "complete", tracked: true, updated_ts: base - 120 }), now)).toBe("2m ago");
    expect(runsAgo(run({ id: "a", kind: "dispatch", status: "complete", tracked: true, updated_ts: base - 7200 }), now)).toBe("2h ago");
    expect(runsAgo(run({ id: "a", kind: "dispatch", status: "complete", tracked: true, updated_ts: base - 172800 }), now)).toBe("2d ago");
  });
});

describe("runSubtitle", () => {
  it("joins role/model/route/machine with the middle-dot separator", () => {
    const r = run({
      id: "a",
      kind: "dispatch",
      status: "complete",
      tracked: true,
      role: "code-reviewer",
      model: "darkmux:qwen3.6-35b-a3b",
      machine: "MacBook-Pro",
    });
    expect(runSubtitle(r, true)).toBe("code-reviewer · qwen3.6-35b-a3b · MacBook-Pro");
  });
  it("hides the machine when showMachine is false", () => {
    const r = run({ id: "a", kind: "dispatch", status: "complete", tracked: true, role: "x", machine: "MacBook-Pro" });
    expect(runSubtitle(r, false)).toBe("x");
  });
  it("prefixes route with 'via '", () => {
    const r = run({ id: "a", kind: "mission", status: "complete", tracked: true, model: "gpt-4o", route: "azure:host/dep" });
    expect(runSubtitle(r, false)).toBe("gpt-4o · via azure:host/dep");
  });
  it("is empty when nothing applies", () => {
    expect(runSubtitle(run({ id: "a", kind: "mission", status: "complete", tracked: true }), false)).toBe("");
  });
});

describe("runsMultiMachine", () => {
  it("is false for zero or one distinct machine", () => {
    expect(runsMultiMachine([])).toBe(false);
    expect(runsMultiMachine([run({ id: "a", kind: "dispatch", status: "complete", tracked: true, machine: "m1" })])).toBe(false);
  });
  it("is true once a second distinct machine appears", () => {
    const runs = [
      run({ id: "a", kind: "dispatch", status: "complete", tracked: true, machine: "m1" }),
      run({ id: "b", kind: "dispatch", status: "complete", tracked: true, machine: "m2" }),
    ];
    expect(runsMultiMachine(runs)).toBe(true);
  });
});

describe("runsFiltered", () => {
  const runs = [
    run({ id: "a", kind: "mission", status: "complete", tracked: true, updated_ts: 1 }),
    run({ id: "b", kind: "dispatch", status: "complete", tracked: true, updated_ts: 3 }),
    run({ id: "c", kind: "lab", status: "complete", tracked: true, updated_ts: 2 }),
  ];
  it("kind=all returns every row, newest-activity-first", () => {
    expect(runsFiltered(runs, "all").map((r) => r.id)).toEqual(["b", "c", "a"]);
  });
  it("filters to one kind", () => {
    expect(runsFiltered(runs, "mission").map((r) => r.id)).toEqual(["a"]);
  });
});

function labRun(over: Partial<LabRun> & Pick<LabRun, "dir" | "mtime_ms">): LabRun {
  return {
    case_ids: [],
    bundles: 0,
    raw_flags: 0,
    deduped_flags: 0,
    confirmed: 0,
    needs_check: 0,
    archived: 0,
    degenerate: false,
    finished: false,
    ...over,
  };
}

describe("labTaskKey / groupLabRunsByTask", () => {
  it("groups runs with no case_ids into '(case pending)'", () => {
    expect(labTaskKey(labRun({ dir: "a", mtime_ms: 1 }))).toBe("(case pending)");
  });
  it("sorts case_ids before joining, so run order doesn't fragment a series", () => {
    expect(labTaskKey(labRun({ dir: "a", mtime_ms: 1, case_ids: ["b", "a"] }))).toBe("a, b");
    expect(labTaskKey(labRun({ dir: "b", mtime_ms: 1, case_ids: ["a", "b"] }))).toBe("a, b");
  });
  it("groups by key and orders groups + within-group runs newest-first", () => {
    const runs = [
      labRun({ dir: "old-a", mtime_ms: 10, case_ids: ["c1"] }),
      labRun({ dir: "new-a", mtime_ms: 30, case_ids: ["c1"] }),
      labRun({ dir: "solo", mtime_ms: 20 }),
    ];
    const groups = groupLabRunsByTask(runs);
    expect(groups.map((g) => g.key)).toEqual(["c1", "(case pending)"]);
    expect(groups[0].runs.map((r) => r.dir)).toEqual(["new-a", "old-a"]);
  });
});

describe("labKnobSummary", () => {
  it("names why staffing is absent, distinguishing in-flight from an older finished artifact", () => {
    expect(labKnobSummary(labRun({ dir: "a", mtime_ms: 1, finished: false }))).toMatch(/pending/);
    expect(labKnobSummary(labRun({ dir: "a", mtime_ms: 1, finished: true }))).toMatch(/older artifact/);
  });
  it("summarizes probe count/k and the judge model/k/ctx", () => {
    const r = labRun({
      dir: "a",
      mtime_ms: 1,
      staffing: {
        probes: [
          { name: "p1", model: "m1", k: 3 },
          { name: "p2", model: "m2", k: 2 },
        ],
        judge: { name: "j", model: "darkmux:big-model", k: 2, n_ctx: 68000 },
      },
    });
    expect(labKnobSummary(r)).toBe("2 probes (k=3,2) · judge big-model k=2 · 68K ctx");
  });
});

describe("labKnobDiff", () => {
  it("returns null when either side is missing", () => {
    expect(labKnobDiff(undefined, labRun({ dir: "a", mtime_ms: 1 }))).toBeNull();
  });
  it("reports 'no change' as an empty array when nothing differs", () => {
    const a = labRun({ dir: "a", mtime_ms: 1, crew: "c1" });
    const b = labRun({ dir: "b", mtime_ms: 2, crew: "c1" });
    expect(labKnobDiff(a, b)).toEqual([]);
  });
  it("names a crew change", () => {
    const a = labRun({ dir: "a", mtime_ms: 1, crew: "c1" });
    const b = labRun({ dir: "b", mtime_ms: 2, crew: "c2" });
    expect(labKnobDiff(a, b)).toEqual(["crew c1→c2"]);
  });
  it("names a per-seat staffing field change by seat name", () => {
    const a = labRun({ dir: "a", mtime_ms: 1, staffing: { probes: [{ name: "p1", model: "m1", k: 2 }] } });
    const b = labRun({ dir: "b", mtime_ms: 2, staffing: { probes: [{ name: "p1", model: "m1", k: 4 }] } });
    expect(labKnobDiff(a, b)).toEqual(["p1.k 2→4"]);
  });
});

describe("labCounts", () => {
  it("formats bundle/flag/confirm counts", () => {
    const r = labRun({ dir: "a", mtime_ms: 1, bundles: 5, raw_flags: 10, deduped_flags: 7, confirmed: 3, needs_check: 2, archived: 1 });
    expect(labCounts(r)).toBe("bundles 5 · flags 10→7 · confirmed 3 · needs_check 2 · archived 1");
  });
});
