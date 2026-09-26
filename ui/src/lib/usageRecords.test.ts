// (#2902 step 2a) The viewer's token totals are a PLAIN SUM of usage records.
//
// One function (`sumUsage`, and its per-record half `usageContribution`)
// feeds the fleet hero, the run page's tiles and the mission graph's step
// meter. The only exception is legacy data (a run with no usage records
// counts its token-bearing `dispatch complete`), isolated in
// `legacyCompleteCounts`. The shared golden fixture `tests/usage-golden/`
// pins the answer for this module and for step 2b's Rust aggregator.
import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import type { FlowRecord } from "../types/handwritten";
import { tokensOffMeter } from "../lenses/fleet/savings";
import { runRegions } from "../lenses/session/sessionRun";
import { applyRecordToMetrics, indexGraph, stepDisplayMetrics, type MetricsMap } from "../lenses/mission/graph";
import { measuredCharsPerToken, averageGenerationRate } from "./tokenRate";
import { turnItems } from "./turnGroups";
import {
  CALL_KIND,
  PURPOSE,
  handleNamesExecution,
  isCompactionUsage,
  isTurnUsage,
  legacyCompleteCounts,
  sumUsage,
  usagePurpose,
  type UsageRecordLike,
} from "./usageRecords";

const LMS = "http://127.0.0.1:1234/v1";
const HOSTED = "azure:example.cognitiveservices.azure.com/gpt-5.1";

// ── the shared golden ────────────────────────────────────────────────────

const GOLDEN_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../tests/usage-golden");
const golden: UsageRecordLike[] = readFileSync(path.join(GOLDEN_DIR, "records.jsonl"), "utf8")
  .trim()
  .split("\n")
  .map((l) => JSON.parse(l));
const expected = JSON.parse(readFileSync(path.join(GOLDEN_DIR, "expected.json"), "utf8"));

/** Group the golden's counted entries (usage records + legacy completes) by
 *  one payload field, `(none)` when an entry does not carry it: the
 *  breakdown step 2b's aggregator reports. Each entry is summed through
 *  `sumUsage` alone, so the breakdown and the total share one arithmetic. */
function breakdown(field: string): Record<string, number> {
  const out: Record<string, number> = {};
  const counted = [...golden.filter((r) => r.action === "telemetry.tokens"), ...legacyCompleteCounts(golden)];
  for (const r of counted) {
    const p = r.payload as Record<string, unknown>;
    const k = typeof p[field] === "string" ? (p[field] as string) : "(none)";
    out[k] = (out[k] ?? 0) + sumUsage([r]).total;
  }
  return out;
}

describe("the shared usage golden (tests/usage-golden)", () => {
  it("overall: a plain sum plus the legacy fallback", () => {
    const s = sumUsage(golden);
    expect({ total: s.total, input: s.prompt, generated: s.completion, cached: s.cached }).toEqual(expected.overall);
    expect(s.usageRecords).toBe(expected.usage_records);
    expect(s.legacyCompletes).toBe(expected.legacy_completes_counted);
    expect(s.utility).toBe(expected.by_purpose.utility.total);
  });

  it("excluding utility: an execution's own numbers", () => {
    const s = sumUsage(golden, { exclude: PURPOSE.utility });
    expect({ total: s.total, input: s.prompt, generated: s.completion, cached: s.cached }).toEqual(expected.excluding_utility);
    expect(s.utility).toBe(0);
  });

  it("by purpose", () => {
    for (const purpose of [PURPOSE.work, PURPOSE.utility]) {
      const other = purpose === PURPOSE.work ? PURPOSE.utility : PURPOSE.work;
      const s = sumUsage(golden, { exclude: other });
      expect({ total: s.total, input: s.prompt, generated: s.completion }).toEqual(expected.by_purpose[purpose]);
    }
  });

  it("by call_kind, endpoint and requested_model", () => {
    expect(breakdown("call_kind")).toEqual(expected.by_call_kind);
    expect(breakdown("endpoint")).toEqual(expected.by_endpoint);
    expect(breakdown("requested_model")).toEqual(expected.by_requested_model);
  });

  it("the fleet hero reads the same sum", () => {
    const t = tokensOffMeter(golden as FlowRecord[]);
    expect({ total: t.total, input: t.input, generated: t.generated, cached: t.cached }).toEqual(expected.overall);
    expect(t.utility).toBe(expected.by_purpose.utility.total);
  });
});

// ── the legacy fallback ──────────────────────────────────────────────────

let clock = 0;
function r(o: Partial<FlowRecord> & { payload?: Record<string, unknown> }): FlowRecord {
  clock += 1;
  const ts = new Date(Date.UTC(2026, 8, 26, 0, 0, clock)).toISOString();
  return { ts, ...o, ...(o.payload ? { fields: o.payload } : {}) } as FlowRecord;
}
function usage(sid: string, handle: string, payload: Record<string, unknown>, mission?: string): FlowRecord {
  return r({ action: "telemetry.tokens", category: "telemetry", source: "tokens", session_id: sid, handle, mission_id: mission, payload });
}
const complete = (sid: string, payload: Record<string, unknown>, mission?: string) =>
  r({ action: "dispatch complete", session_id: sid, handle: "x", mission_id: mission, payload });

describe("the legacy fallback (a run with no usage records counts its complete)", () => {
  it("a run with ZERO usage records counts each token-bearing complete once", () => {
    const recs = [complete("old", { total_tokens: 100, prompt_tokens: 90, completion_tokens: 10 })];
    expect(sumUsage(recs)).toMatchObject({ total: 100, prompt: 90, completion: 10, cached: null, legacyCompletes: 1 });
  });

  it("a run with any usage record never reads its complete, even an `absent` one", () => {
    const withTurn = [usage("s", "coder", { call_kind: CALL_KIND.turn, total_tokens: 40, prompt_tokens: 30, completion_tokens: 10 }), complete("s", { total_tokens: 40 })];
    expect(sumUsage(withTurn).total).toBe(40);
    const absentOnly = [usage("s", "coder", { call_kind: CALL_KIND.single_shot, token_source: "absent" }), complete("s", { total_tokens: 999 })];
    expect(sumUsage(absentOnly)).toMatchObject({ total: 0, legacyCompletes: 0 });
  });

  it("keys on the RUN (session_id, mission_id), not the bare session id", () => {
    // Mission A has usage records; mission B under the same deterministic
    // session id has only a legacy complete. Both count.
    const recs = [
      usage("task-t", "s", { call_kind: CALL_KIND.map_item, total_tokens: 5 }, "A"),
      complete("task-t", { total_tokens: 5 }, "A"),
      complete("task-t", { total_tokens: 7 }, "B"),
    ];
    expect(sumUsage(recs).total).toBe(12);
  });

  it("a complete carries cached tokens only when it reports them", () => {
    expect(sumUsage([complete("a", { total_tokens: 10, cached_tokens: 4 })]).cached).toBe(4);
    expect(sumUsage([complete("a", { total_tokens: 10 })]).cached).toBeNull();
  });

  it("a remote_tokens-only legacy complete still counts its total", () => {
    expect(sumUsage([complete("rv", { remote_tokens: 40 })]).total).toBe(40);
  });
});

describe("purpose", () => {
  it("a record's own purpose wins; legacy: compaction is utility, anything else work", () => {
    expect(usagePurpose({ purpose: PURPOSE.utility, call_kind: CALL_KIND.single_shot })).toBe(PURPOSE.utility);
    expect(usagePurpose({ purpose: PURPOSE.work, call_kind: CALL_KIND.compaction })).toBe(PURPOSE.work);
    expect(usagePurpose({ call_kind: CALL_KIND.compaction })).toBe(PURPOSE.utility);
    expect(usagePurpose({ call_kind: CALL_KIND.turn })).toBe(PURPOSE.work);
    expect(usagePurpose({})).toBe(PURPOSE.work);
    expect(usagePurpose({ purpose: "bogus", call_kind: CALL_KIND.compaction })).toBe(PURPOSE.utility);
  });
});

// ── the three consumers ──────────────────────────────────────────────────

/** A container run with compactor calls (one 1.59, one 1.58 with no
 *  `purpose`), and a mission step that compacts. `withU` adds the utility
 *  records; everything else is identical. */
function utilityStreams(): [FlowRecord[], FlowRecord[]] {
  clock = 0;
  const M = "m-2";
  const without: FlowRecord[] = [];
  const withU: FlowRecord[] = [];
  const both = (x: FlowRecord) => { without.push(x); withU.push(x); };
  const only = (x: FlowRecord) => { withU.push(x); };
  const compaction = (sid: string, total: number, extra: Record<string, unknown> = {}, mission?: string) =>
    ({ ...usage(sid, "compactor", { call_kind: CALL_KIND.compaction, purpose: PURPOSE.utility, requested_model: "darkmux:c4b", endpoint: LMS, token_source: "provider", prompt_tokens: total - 80, completion_tokens: 80, total_tokens: total, ...extra }, mission), model: "darkmux:c4b" }) as FlowRecord;

  both(r({ action: "dispatch.start", session_id: "h1", handle: "coder", model: "gpt-5.1", payload: { runtime: "internal", endpoint: HOSTED } }));
  both(r({ action: "dispatch.turn.heartbeat", session_id: "h1", payload: { turn_seq: 1, generated_chars: 20000, sampled_at_ms: 1000 } }));
  both(r({ action: "dispatch.turn", session_id: "h1", payload: { turn_seq: 1, generation_ms: 2000 } }));
  both(usage("h1", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, requested_model: "gpt-5.1", endpoint: HOSTED, token_source: "provider", turn_seq: 1, prompt_tokens: 900, completion_tokens: 100, total_tokens: 1000 }));
  only(compaction("h1", 580));
  only(compaction("h1", 120, { purpose: undefined }));
  both(r({ action: "dispatch.turn", session_id: "h1", payload: { turn_seq: 2, generation_ms: 500 } }));
  both(usage("h1", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, requested_model: "gpt-5.1", endpoint: HOSTED, token_source: "provider", turn_seq: 2, prompt_tokens: 1100, completion_tokens: 100, total_tokens: 1200 }));
  both(r({ action: "dispatch.complete", session_id: "h1", handle: "coder", payload: { runtime: "internal", endpoint: HOSTED, result_class: "ok", total_turns: 2, prompt_tokens: 2000, completion_tokens: 200, total_tokens: 2200 } }));

  both(r({ action: "dispatch start", session_id: "task-t3", handle: "cs", mission_id: M, payload: { step_id: "cs", kind: "dispatch.internal" } }));
  both(usage("task-t3", "cs", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, requested_model: "darkmux:q", endpoint: LMS, token_source: "provider", turn_seq: 1, prompt_tokens: 400, completion_tokens: 50, total_tokens: 450, step_id: "cs" }, M));
  only(compaction("task-t3", 300, { step_id: "cs" }, M));
  both(r({ action: "dispatch complete", session_id: "task-t3", handle: "cs", mission_id: M, payload: { step_id: "cs", kind: "dispatch.internal", total_tokens: 450, total_turns: 1 } }));
  return [without, withU];
}

const tile = (recs: FlowRecord[], sid: string, label: string) =>
  runRegions(recs, sid, Date.UTC(2026, 8, 27)).metrics.find((m) => m.label === label)?.value;

describe("the fleet hero (tokensOffMeter)", () => {
  it("UTILITY is its own chip; the run count does not move", () => {
    const [without, withU] = utilityStreams();
    const a = tokensOffMeter(without);
    const b = tokensOffMeter(withU);
    expect(b.utility).toBe(580 + 120 + 300);
    expect(a.utility).toBe(0);
    expect(b.total).toBe(a.total + 580 + 120 + 300);
    expect(b.runs).toBe(a.runs);
  });

  it("a compactor call or a single-shot record alone never opens an in-flight dispatch", () => {
    clock = 0;
    const only = [
      usage("c-only", "compactor", { call_kind: CALL_KIND.compaction, purpose: PURPOSE.utility, total_tokens: 90, prompt_tokens: 80, completion_tokens: 10 }),
      usage("ss-only", "analyst", { call_kind: CALL_KIND.single_shot, purpose: PURPOSE.work, total_tokens: 9, prompt_tokens: 8, completion_tokens: 1 }),
    ];
    expect(tokensOffMeter(only)).toMatchObject({ runs: 0, total: 99, utility: 90 });
    // A turn record with no terminal yet IS a dispatch in flight.
    const turn = usage("t-only", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, total_tokens: 5, prompt_tokens: 4, completion_tokens: 1, turn_seq: 1 });
    expect(tokensOffMeter([...only, turn]).runs).toBe(1);
  });

  it("GENERATED + INPUT reconcile with ALL TOKENS when providers report total = prompt + completion", () => {
    const [, withU] = utilityStreams();
    const t = tokensOffMeter(withU);
    expect(t.generated + t.input).toBe(t.total);
    const g = tokensOffMeter(golden as FlowRecord[]);
    expect(g.generated + g.input).toBe(g.total);
  });

  it("a provider total above prompt + completion shows ALL TOKENS above the two tiles, with no filler chip", () => {
    // A reasoning model reporting its reasoning outside completion_tokens.
    const recs = [usage("rz", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, prompt_tokens: 100, completion_tokens: 20, total_tokens: 150, reasoning_tokens: 30 })];
    const t = tokensOffMeter(recs);
    expect(t).toMatchObject({ total: 150, input: 100, generated: 20 });
    expect(Object.keys(t).sort()).toEqual(["cached", "generated", "input", "runs", "total", "utility"]);
  });

  it("CACHED sums only reporting records, and is absent (null) when none report", () => {
    const [without] = utilityStreams();
    expect(tokensOffMeter(without).cached).toBeNull();
    const withCache = [...without, usage("h1", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, prompt_tokens: 10, completion_tokens: 1, total_tokens: 11, cached_tokens: 0 })];
    expect(tokensOffMeter(withCache).cached).toBe(0);
  });
});

describe("the run page and the mission graph exclude utility (contract 8)", () => {
  it("the run page's tiles are unchanged by utility records", () => {
    const [without, withU] = utilityStreams();
    for (const sid of ["h1", "task-t3"]) {
      for (const label of ["TOKENS IN", "TOKENS OUT"]) {
        expect(tile(withU, sid, label)).toBe(tile(without, sid, label));
      }
    }
    expect(tile(withU, "h1", "TOKENS IN")).toBe("2.00k");
  });

  it("a session whose only calls are utility jobs (radio routing) IS that job: its page shows them", () => {
    clock = 0;
    const recs = [
      r({ action: "dispatch.start", session_id: "rr", handle: "radio-router", payload: { runtime: "direct" } }),
      usage("rr", "radio-router", { call_kind: CALL_KIND.single_shot, purpose: PURPOSE.utility, prompt_tokens: 50, completion_tokens: 7, total_tokens: 57 }),
      r({ action: "dispatch.complete", session_id: "rr", handle: "radio-router", payload: { runtime: "direct", total_tokens: 57, prompt_tokens: 50, completion_tokens: 7 } }),
    ];
    expect(tile(recs, "rr", "TOKENS IN")).toBe("50");
    expect(tile(recs, "rr", "TOKENS OUT")).toBe("7");
  });

  it("a single-shot run's tiles read its usage record, not its complete", () => {
    clock = 0;
    const recs = [
      r({ action: "dispatch.start", session_id: "ss", handle: "analyst", payload: { runtime: "direct" } }),
      usage("ss", "analyst", { call_kind: CALL_KIND.single_shot, purpose: PURPOSE.work, prompt_tokens: 60, completion_tokens: 9, total_tokens: 69 }),
      r({ action: "dispatch.complete", session_id: "ss", handle: "analyst", payload: { runtime: "direct", total_tokens: 1, prompt_tokens: 1, completion_tokens: 0 } }),
    ];
    expect(tile(recs, "ss", "TOKENS IN")).toBe("60");
    expect(tile(recs, "ss", "TOKENS OUT")).toBe("9");
  });

  it("a legacy session (complete only) reads its complete", () => {
    clock = 0;
    const recs = [
      r({ action: "dispatch.start", session_id: "lg", handle: "analyst", payload: { runtime: "direct" } }),
      r({ action: "dispatch.complete", session_id: "lg", handle: "analyst", payload: { runtime: "direct", total_tokens: 70, prompt_tokens: 61, completion_tokens: 9 } }),
    ];
    expect(tile(recs, "lg", "TOKENS IN")).toBe("61");
  });

  it("the mission graph's step meter is the usage sum without utility; a legacy step reads its complete", () => {
    const [without, withU] = utilityStreams();
    const idx = indexGraph({ nodes: [{ id: "t3", kind: "task", steps: [{ id: "cs", kind: "dispatch.internal" }] }] as never });
    const fold = (recs: FlowRecord[]) => recs.reduce<MetricsMap>((m, x) => applyRecordToMetrics(m, x, idx, "m-2"), {});
    expect(stepDisplayMetrics(fold(withU).cs).tokens).toBe(450);
    expect(stepDisplayMetrics(fold(without).cs).tokens).toBe(450);
    // Legacy: no usage record for the step, a finalized total on its complete.
    const legacy = withU.filter((x) => !(x.action === "telemetry.tokens" && x.session_id === "task-t3"));
    expect(stepDisplayMetrics(fold(legacy).cs).tokens).toBe(450);
    // Usage records win over a complete that disagrees.
    const bumped = withU.map((x) => (x.action === "dispatch complete" && x.session_id === "task-t3" ? { ...x, payload: { ...(x.payload as object), total_tokens: 9999 } } : x));
    expect(stepDisplayMetrics(fold(bumped).cs).tokens).toBe(450);
  });

  it("a retried map item's per-call records sum to the item's tokens on every surface", () => {
    clock = 0;
    const M = "m-1";
    const start = r({ action: "dispatch start", session_id: "task-t9", handle: "mp", mission_id: M, payload: { step_id: "mp", kind: "dispatch.map" } });
    const per = [0, 1, 2].map(() =>
      usage("task-t9", "mp", { call_kind: CALL_KIND.map_item, purpose: PURPOSE.work, requested_model: "q", endpoint: LMS, token_source: "provider", total_tokens: 10, prompt_tokens: 8, completion_tokens: 2, remote: false, index: 0 }, M),
    );
    const recs = [
      start,
      ...per,
      r({ action: "dispatch complete", session_id: "task-t9", handle: "mp", mission_id: M, payload: { step_id: "mp", kind: "dispatch.map", result_class: "ok" } }),
    ];
    expect(tokensOffMeter(recs).total).toBe(30);
    expect(tile(recs, "task-t9", "TOKENS IN")).toBe("24");
    const idx = indexGraph({ nodes: [{ id: "t9", kind: "task", steps: [{ id: "mp", kind: "dispatch.map" }] }] as never });
    const m = recs.reduce<MetricsMap>((acc, x) => applyRecordToMetrics(acc, x, idx, M), {});
    expect(stepDisplayMetrics(m.mp).tokens).toBe(30);
  });
});

// ── neighbors that read usage records for other reasons ──────────────────

describe("per-turn readers (rate, calibration, turn groups)", () => {
  it("exclude compaction records, even one carrying a turn_seq", () => {
    const [without, withU] = utilityStreams();
    expect(measuredCharsPerToken(without)).not.toBe(4); // calibrated, not the default
    expect(measuredCharsPerToken(withU)).toBe(measuredCharsPerToken(without));
    expect(averageGenerationRate([withU])).toEqual(averageGenerationRate([without]));
    const stray = usage("h1", "compactor", { call_kind: CALL_KIND.compaction, turn_seq: 1, completion_tokens: 999, endpoint: LMS });
    expect(measuredCharsPerToken([...withU, stray])).toBe(measuredCharsPerToken(without));
    const vis = (recs: FlowRecord[]) => recs.filter((x) => x.action !== "telemetry.tokens");
    expect(turnItems(vis(withU), withU)).toEqual(turnItems(vis(without), without));
  });

  it("the predicates", () => {
    expect(isTurnUsage({ call_kind: CALL_KIND.compaction })).toBe(false);
    expect(isTurnUsage({ call_kind: CALL_KIND.turn })).toBe(true);
    expect(isTurnUsage({})).toBe(true);
    expect(isCompactionUsage({ call_kind: CALL_KIND.compaction })).toBe(true);
    expect(isCompactionUsage({ call_kind: CALL_KIND.turn })).toBe(false);
    expect(handleNamesExecution({ handle: "compactor", action: "telemetry.tokens", payload: { call_kind: CALL_KIND.compaction } } as never)).toBe(false);
    expect(handleNamesExecution({ handle: "coder", action: "telemetry.tokens", payload: { call_kind: CALL_KIND.turn } } as never)).toBe(true);
    expect(handleNamesExecution({ handle: "coder", action: "dispatch.turn" } as never)).toBe(true);
  });
});
