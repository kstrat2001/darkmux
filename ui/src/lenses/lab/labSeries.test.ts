// The executable specification for the lab lens's pure logic.
//
// Ported behavior — every assertion below is the LEGACY viewer's observable
// behavior (`crates/darkmux-serve/assets/viewer.html`, `labTaskKey` /
// `groupLabRunsByTask` / `labKnobSummary` / `labKnobDiff`). This is a parity
// port: where the legacy output looks odd, the odd output is the requirement.
// Changing behavior is a separate change with its own reason.
import { describe, it, expect } from "vitest";
import {
  shortModel,
  labFieldVal,
  labTaskKey,
  groupLabRunsByTask,
  labKnobSummary,
  labKnobDiff,
} from "./labSeries";
import type { LabRun, LabStaffing } from "./types";

/** A run with only the fields a given assertion cares about. */
function run(over: Partial<LabRun> = {}): LabRun {
  return {
    dir: "review-bench-1",
    mtime_ms: 1_000,
    case_ids: [],
    bundles: 0,
    raw_flags: 0,
    deduped_flags: 0,
    confirmed: 0,
    needs_check: 0,
    archived: 0,
    degenerate: false,
    finished: false,
    has_funnels: false,
    has_events: false,
    ...over,
  };
}

describe("shortModel", () => {
  it("strips the darkmux namespace prefix", () => {
    expect(shortModel("darkmux:qwen3.6-35b-a3b")).toBe("qwen3.6-35b-a3b");
  });
  it("leaves a non-namespaced id alone", () => {
    expect(shortModel("qwen3.6-35b-a3b")).toBe("qwen3.6-35b-a3b");
  });
  it("strips only a LEADING prefix, not an embedded one", () => {
    expect(shortModel("vendor/darkmux:x")).toBe("vendor/darkmux:x");
  });
  it("renders an absent model as the empty string, never 'null'/'undefined'", () => {
    expect(shortModel(null)).toBe("");
    expect(shortModel(undefined)).toBe("");
  });
});

describe("labFieldVal", () => {
  it("renders null and undefined as an em dash", () => {
    expect(labFieldVal(null)).toBe("—");
    expect(labFieldVal(undefined)).toBe("—");
  });
  it("shortens a namespaced model id", () => {
    expect(labFieldVal("darkmux:qwen3-4b")).toBe("qwen3-4b");
  });
  it("passes a plain string through untouched", () => {
    expect(labFieldVal("progressive")).toBe("progressive");
  });
  it("passes a number through as a number, not a string", () => {
    expect(labFieldVal(64_000)).toBe(64_000);
  });
  it("renders zero as zero, NOT as the em dash (0 is a real knob value)", () => {
    expect(labFieldVal(0)).toBe(0);
  });
});

describe("labTaskKey", () => {
  it("joins the case ids with a comma and space", () => {
    expect(labTaskKey(run({ case_ids: ["a", "b"] }))).toBe("a, b");
  });
  it("sorts the ids so run order does not change the series key", () => {
    expect(labTaskKey(run({ case_ids: ["b", "a"] }))).toBe("a, b");
  });
  it("does not mutate the run's own case_ids while sorting", () => {
    const r = run({ case_ids: ["b", "a"] });
    labTaskKey(r);
    expect(r.case_ids).toEqual(["b", "a"]);
  });
  it("buckets a run with no known cases separately rather than merging it", () => {
    expect(labTaskKey(run({ case_ids: [] }))).toBe("(case pending)");
  });
});

describe("groupLabRunsByTask", () => {
  it("groups runs over the same corpus under one key", () => {
    const groups = groupLabRunsByTask([
      run({ dir: "r1", case_ids: ["a"], mtime_ms: 1 }),
      run({ dir: "r2", case_ids: ["a"], mtime_ms: 2 }),
    ]);
    expect(groups).toHaveLength(1);
    expect(groups[0].key).toBe("a");
  });

  it("orders runs inside a group newest-first", () => {
    const groups = groupLabRunsByTask([
      run({ dir: "old", case_ids: ["a"], mtime_ms: 1 }),
      run({ dir: "new", case_ids: ["a"], mtime_ms: 9 }),
      run({ dir: "mid", case_ids: ["a"], mtime_ms: 5 }),
    ]);
    expect(groups[0].runs.map((r) => r.dir)).toEqual(["new", "mid", "old"]);
  });

  it("orders groups by their newest run, newest-first", () => {
    const groups = groupLabRunsByTask([
      run({ dir: "a1", case_ids: ["a"], mtime_ms: 10 }),
      run({ dir: "b1", case_ids: ["b"], mtime_ms: 50 }),
      run({ dir: "a2", case_ids: ["a"], mtime_ms: 20 }),
    ]);
    expect(groups.map((g) => g.key)).toEqual(["b", "a"]);
  });

  it("does not reorder the caller's array", () => {
    const input = [
      run({ dir: "old", case_ids: ["a"], mtime_ms: 1 }),
      run({ dir: "new", case_ids: ["a"], mtime_ms: 9 }),
    ];
    groupLabRunsByTask(input);
    expect(input.map((r) => r.dir)).toEqual(["old", "new"]);
  });

  it("returns no groups for no runs (never a phantom empty series)", () => {
    expect(groupLabRunsByTask([])).toEqual([]);
  });
});

describe("labKnobSummary", () => {
  it("distinguishes an OLD artifact from a run still in flight", () => {
    // The two reasons `staffing` can be absent are not the same fact, and the
    // operator acts differently on each — an unfinished run will grow one.
    expect(labKnobSummary(run({ finished: true }))).toBe("staffing not recorded (older artifact)");
    expect(labKnobSummary(run({ finished: false }))).toBe(
      "staffing pending — first case hasn't completed yet",
    );
  });

  it("summarizes probe count and each probe's k", () => {
    const staffing: LabStaffing = { probes: [{ name: "p1", k: 3 }, { name: "p2", k: 5 }] };
    expect(labKnobSummary(run({ staffing }))).toBe("2 probes (k=3,5)");
  });

  it("uses the singular for one probe", () => {
    expect(labKnobSummary(run({ staffing: { probes: [{ name: "p1", k: 2 }] } }))).toBe(
      "1 probe (k=2)",
    );
  });

  it("says so explicitly when a snapshot exists but has no probes", () => {
    expect(labKnobSummary(run({ staffing: { probes: [] } }))).toBe("no probes");
  });

  it("appends the judge seat with its k and context in thousands", () => {
    const staffing: LabStaffing = {
      probes: [{ name: "p1", k: 1 }],
      judge: { name: "judge", model: "darkmux:qwen3.6-35b", k: 2, n_ctx: 64_000 },
    };
    expect(labKnobSummary(run({ staffing }))).toBe(
      "1 probe (k=1) · judge qwen3.6-35b k=2 · 64K ctx",
    );
  });

  it("rounds the judge context rather than truncating it", () => {
    const staffing: LabStaffing = {
      judge: { name: "judge", model: "m", k: 1, n_ctx: 65_536 },
    };
    // 65536/1000 = 65.536 -> 66
    expect(labKnobSummary(run({ staffing }))).toBe("no probes · judge m k=1 · 66K ctx");
  });

  it("renders a judge with no recorded context as 0K rather than NaN", () => {
    const staffing: LabStaffing = { judge: { name: "judge", model: "m", k: 1 } };
    expect(labKnobSummary(run({ staffing }))).toBe("no probes · judge m k=1 · 0K ctx");
  });

  it("omits the judge segment entirely when there is no judge seat", () => {
    expect(labKnobSummary(run({ staffing: { probes: [{ name: "p", k: 1 }] } }))).toBe(
      "1 probe (k=1)",
    );
  });
});

describe("labKnobDiff", () => {
  it("returns null when either side is missing — nothing to compare against", () => {
    expect(labKnobDiff(null, run())).toBeNull();
    expect(labKnobDiff(run(), null)).toBeNull();
  });

  it("reports no changes between two identical runs", () => {
    const staffing: LabStaffing = { probes: [{ name: "p", model: "m", k: 1 }] };
    expect(labKnobDiff(run({ staffing }), run({ staffing }))).toEqual([]);
  });

  it("reports a crew change", () => {
    expect(labKnobDiff(run({ crew: "a" }), run({ crew: "b" }))).toEqual(["crew a→b"]);
  });

  it("reports an exec_mode change", () => {
    expect(labKnobDiff(run({ exec_mode: "serial" }), run({ exec_mode: "parallel" }))).toEqual([
      "exec_mode serial→parallel",
    ]);
  });

  it("renders an absent side of a changed field as an em dash", () => {
    expect(labKnobDiff(run({ crew: null }), run({ crew: "b" }))).toEqual(["crew —→b"]);
  });

  it("reports each changed probe field, matched by seat NAME not position", () => {
    const prev = run({ staffing: { probes: [{ name: "a", k: 1 }, { name: "b", k: 2 }] } });
    const curr = run({ staffing: { probes: [{ name: "b", k: 2 }, { name: "a", k: 9 }] } });
    // Reordering alone is not a change; only a's k actually moved.
    expect(labKnobDiff(prev, curr)).toEqual(["a.k 1→9"]);
  });

  it("reports every changed field of one seat", () => {
    const prev = run({
      staffing: { probes: [{ name: "a", model: "darkmux:m1", k: 1, max_tokens: 10, n_ctx: 100 }] },
    });
    const curr = run({
      staffing: { probes: [{ name: "a", model: "darkmux:m2", k: 2, max_tokens: 20, n_ctx: 200 }] },
    });
    expect(labKnobDiff(prev, curr)).toEqual([
      "a.model m1→m2",
      "a.k 1→2",
      "a.max_tokens 10→20",
      "a.n_ctx 100→200",
    ]);
  });

  it("reports an added and a removed probe seat", () => {
    const prev = run({ staffing: { probes: [{ name: "gone", model: "darkmux:m" }] } });
    const curr = run({ staffing: { probes: [{ name: "fresh", model: "darkmux:m2" }] } });
    // Seat names are visited in sorted order: "fresh" before "gone".
    expect(labKnobDiff(prev, curr)).toEqual(["+probe fresh (m2)", "-probe gone"]);
  });

  it("reports changed judge fields", () => {
    const prev = run({ staffing: { judge: { name: "j", model: "m", k: 1 } } });
    const curr = run({ staffing: { judge: { name: "j", model: "m", k: 3 } } });
    expect(labKnobDiff(prev, curr)).toEqual(["judge.k 1→3"]);
  });

  it("reports a judge seat appearing or disappearing", () => {
    const none = run({ staffing: { probes: [] } });
    const withJudge = run({ staffing: { probes: [], judge: { name: "j", model: "darkmux:m" } } });
    expect(labKnobDiff(none, withJudge)).toEqual(["+judge (m)"]);
    expect(labKnobDiff(withJudge, none)).toEqual(["-judge"]);
  });

  it("reports NO judge change when neither side has one", () => {
    // The inverted case, and a real regression once: diffing only when BOTH
    // sides had a judge was a false "no knob change" for the appear/disappear
    // case. Both-null must stay silent without reintroducing that.
    const a = run({ staffing: { probes: [{ name: "p", k: 1 }] } });
    const b = run({ staffing: { probes: [{ name: "p", k: 1 }] } });
    expect(labKnobDiff(a, b)).toEqual([]);
  });

  it("reports the snapshot becoming available when only one side has staffing", () => {
    const before = run();
    const after = run({ staffing: { probes: [{ name: "p", k: 1 }] } });
    expect(labKnobDiff(before, after)).toEqual(["staffing snapshot became available"]);
  });

  it("accumulates unrelated changes into one list", () => {
    const prev = run({ crew: "a", staffing: { probes: [{ name: "p", k: 1 }] } });
    const curr = run({ crew: "b", staffing: { probes: [{ name: "p", k: 2 }] } });
    expect(labKnobDiff(prev, curr)).toEqual(["crew a→b", "p.k 1→2"]);
  });
});
