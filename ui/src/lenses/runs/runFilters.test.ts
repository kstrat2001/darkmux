import { describe, it, expect } from "vitest";
import type { Run } from "../../types/generated/Run";
import { machineKeyOfRuns } from "./format";
import { NOT_SET, applyFilters, canNarrow, facetChoices, facetTotal, kindCounts, runsCountText, valueOf, type FilterEnv } from "./runFilters";
import { emptyFilterSel, filterSelPairs, parseFilterSel, type FilterSel } from "../../lib/runsFilterQuery";
import { canonicalHash } from "../../lib/hashSync";

// Frozen clock: every window assertion is relative to this, never to Date.now().
const NOW = Date.UTC(2026, 9, 1, 12, 0, 0);
const NOW_S = NOW / 1000;
const HOUR = 3600;

let n = 0;
const run = (o: Partial<Run>): Run => ({ id: `r${++n}`, kind: "dispatch", status: "complete", tracked: true, updated_ts: NOW_S - 60, ...o }) as Run;
const envFor = (runs: Run[]): FilterEnv => ({ now: NOW, machineOf: machineKeyOfRuns(runs) });
const sel = (o: Partial<FilterSel>): FilterSel => ({ ...emptyFilterSel(), ...o });
const counts = (c: { value: string; count: number }[]) => Object.fromEntries(c.map((x) => [x.value, x.count]));

const RUNS: Run[] = [
  run({ id: "a", status: "complete", role: "coder", model: "darkmux:qwen", machine: "studio", machine_uid: "U1" }),
  run({ id: "b", status: "complete", role: "coder", model: "qwen", machine: "laptop", machine_uid: "U2" }),
  run({ id: "c", status: "running", role: "reviewer", model: "llama", machine: "laptop", machine_uid: "U2" }),
  run({ id: "d", status: "complete", machine: "studio", machine_uid: "U1" }),
];

describe("facet counts", () => {
  it("OR within a dimension, AND across dimensions", () => {
    const env = envFor(RUNS);
    const ids = (s: FilterSel) => applyFilters(RUNS, s, env).map((r) => r.id);
    expect(ids(sel({ role: ["coder", "reviewer"] }))).toEqual(["a", "b", "c"]);
    expect(ids(sel({ role: ["coder", "reviewer"], status: ["running"] }))).toEqual(["c"]);
  });

  it("a dimension's counts ignore its own selection but obey every other one", () => {
    const env = envFor(RUNS);
    const s = sel({ role: ["coder"], status: ["running"] });
    // Role counts see status=running only: just the reviewer, and the selected
    // coder value still lists at 0 so it can be unticked.
    expect(counts(facetChoices(RUNS, s, "role", env))).toEqual({ reviewer: 1, coder: 0 });
    // Status counts see role=coder only.
    expect(counts(facetChoices(RUNS, s, "status", env))).toEqual({ complete: 2, running: 0 });
  });

  it("(not set) is a value, listed last, and selectable", () => {
    const env = envFor(RUNS);
    const choices = facetChoices(RUNS, sel({}), "role", env);
    expect(choices.map((c) => c.value)).toEqual(["coder", "reviewer", NOT_SET]);
    expect(applyFilters(RUNS, sel({ role: [NOT_SET] }), env).map((r) => r.id)).toEqual(["d"]);
  });

  it("a run with no status is (not set), not a crash (#1622 degraded rows)", () => {
    const rs = [run({ id: "ok" }), run({ id: "nostatus", status: undefined as unknown as Run["status"] })];
    const env = envFor(rs);
    expect(() => facetChoices(rs, sel({}), "status", env)).not.toThrow();
    expect(counts(facetChoices(rs, sel({}), "status", env))).toEqual({ complete: 1, [NOT_SET]: 1 });
    expect(applyFilters(rs, sel({ status: [NOT_SET] }), env).map((r) => r.id)).toEqual(["nostatus"]);
  });

  it("strips the darkmux: prefix so one model is one value", () => {
    const env = envFor(RUNS);
    expect(counts(facetChoices(RUNS, sel({}), "model", env))).toEqual({ qwen: 2, llama: 1, [NOT_SET]: 1 });
  });

  it("workload and verify apply to lab runs only", () => {
    const rs = [
      run({ id: "l1", kind: "lab", workload: "w", verify_passed: true }),
      run({ id: "l2", kind: "lab", workload: "w", verify_passed: false }),
      run({ id: "l3", kind: "lab" }),
      run({ id: "x" }),
    ];
    const env = envFor(rs);
    expect(valueOf(rs[3], "workload", env)).toBeNull();
    expect(counts(facetChoices(rs, sel({}), "workload", env))).toEqual({ w: 2, [NOT_SET]: 1 });
    expect(counts(facetChoices(rs, sel({}), "verify", env))).toEqual({ pass: 1, FAIL: 1, "—": 1 });
    expect(applyFilters(rs, sel({ verify: ["FAIL"] }), env).map((r) => r.id)).toEqual(["l2"]);
  });

  it("time counts the runs inside each window, by latest activity", () => {
    const rs = [run({ id: "t1", updated_ts: NOW_S - 600 }), run({ id: "t2", updated_ts: NOW_S - 5 * HOUR }), run({ id: "t3", updated_ts: NOW_S - 3 * 86400 })];
    const env = envFor(rs);
    expect(counts(facetChoices(rs, sel({}), "time", env))).toEqual({ "1h": 1, "24h": 2, "7d": 3, "30d": 3 });
    expect(applyFilters(rs, sel({ time: ["24h"] }), env).map((r) => r.id)).toEqual(["t1", "t2"]);
  });

  it("kind counts follow the dimension filters", () => {
    const rs = [run({ kind: "mission", role: "x" }), run({ kind: "lab", role: "x" }), run({ kind: "lab" })];
    const kept = applyFilters(rs, sel({ role: ["x"] }), envFor(rs));
    expect(kindCounts(kept, ["all", "mission", "dispatch", "lab"])).toEqual({ all: 2, mission: 1, dispatch: 0, lab: 1 });
  });
});

describe("machine identity", () => {
  it("one uid under two names is one value; two uids under one name are two", () => {
    const rs = [
      run({ machine: "studio", machine_uid: "AA", updated_ts: NOW_S - 100 }),
      run({ machine: "studio-renamed", machine_uid: "aa", updated_ts: NOW_S - 10 }),
      run({ machine: "Mac", machine_uid: "BB" }),
      run({ machine: "Mac", machine_uid: "CC" }),
    ];
    const env = envFor(rs);
    const c = facetChoices(rs, sel({}), "machine", env);
    expect(c).toHaveLength(3);
    expect(c[0]).toEqual({ value: "uid:aa", count: 2 });
  });

  it("a run with no uid falls back to its name; with neither it is (not set)", () => {
    const rs = [run({ machine: "box" }), run({})];
    const c = facetChoices(rs, sel({}), "machine", envFor(rs));
    expect(c.map((x) => x.value)).toEqual(["name:box", NOT_SET]);
  });
});

describe("the stable bar", () => {
  it("every dimension keeps a choice list however the others are selected, and says when it cannot narrow", () => {
    const env = envFor(RUNS);
    const s = sel({ status: ["running"] });
    const total = facetTotal(RUNS, s, "role", env);
    // Within the status=running view all runs share one role: cannot narrow...
    expect(canNarrow(facetChoices(RUNS, s, "role", env), total, [])).toBe(false);
    // ...until it holds a selection, which it must still let you clear.
    expect(canNarrow(facetChoices(RUNS, sel({ status: ["running"], role: ["reviewer"] }), "role", env), total, ["reviewer"])).toBe(true);
    // Status itself, over the unfiltered view, can narrow.
    expect(canNarrow(facetChoices(RUNS, s, "status", env), facetTotal(RUNS, s, "status", env), ["running"])).toBe(true);
  });
});

describe("hash round-trip", () => {
  it("writes every selected value and reads the same selection back", () => {
    const s = sel({ status: ["complete", "running"], machine: ["studio"], model: ["qwen"], time: ["24h"], role: [NOT_SET] });
    const hash = canonicalHash({ kind: "runs", runsKind: "lab", lab: null, machine: "studio", filters: s }) as string;
    const p = new URLSearchParams(hash);
    expect(p.get("kind")).toBe("lab");
    expect(p.getAll("status")).toEqual(["complete", "running"]);
    expect(parseFilterSel((n2) => p.getAll(n2))).toEqual(s);
    expect(filterSelPairs(emptyFilterSel())).toEqual([]);
  });

  it("drops an unknown time window instead of guessing", () => {
    expect(parseFilterSel((n2) => (n2 === "time" ? ["2y"] : [])).time).toEqual([]);
  });

  it("a bare machine pin still writes machine=", () => {
    expect(canonicalHash({ kind: "runs", runsKind: "all", lab: null, machine: "studio" })).toBe("lens=runs&machine=studio");
  });
});

/** (#2925) The runs board has ONE count line: what is on screen, out of what
 * matches, out of everything, saying only the parts that differ. */
describe("runsCountText", () => {
  it("says only the total when nothing is filtered or capped", () => {
    expect(runsCountText({ rendered: 12, matching: 12, total: 12 })).toBe("Showing 12 runs");
    expect(runsCountText({ rendered: 1, matching: 1, total: 1 })).toBe("Showing 1 run");
  });
  it("says newest when the list is capped and nothing is filtered", () => {
    expect(runsCountText({ rendered: 25, matching: 858, total: 858 })).toBe("Showing newest 25 of 858 runs");
  });
  it("says matching of total when filtered and not capped", () => {
    expect(runsCountText({ rendered: 7, matching: 7, total: 858 })).toBe("Showing 7 of 858 runs");
  });
  it("says all three when filtered and capped", () => {
    expect(runsCountText({ rendered: 25, matching: 120, total: 858 })).toBe("Showing newest 25 of 120 matching (858 runs)");
  });
});
