// (#2902 step 2a) The viewer's token totals are a PLAIN SUM of usage records.
//
// One function (`sumUsage`, and its per-record half `usageContribution`)
// feeds the fleet hero, the run page's tiles and the mission graph's step
// meter, with the utility part named (#3067: one total everywhere). There is
// no exception: a `dispatch complete` carries no tokens. The shared golden
// fixture `tests/usage-golden/` pins the answer for this module and for step
// 2b's Rust aggregator.
import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import type { NormRecord } from "./ingest";
import { norm, normAll, type RawRecord } from "../testing/records";
import { tokensOffMeter } from "../lenses/fleet/savings";
import { runRegions } from "../lenses/session/sessionRun";
import { applyRecordToMetrics, indexGraph, seedMetricsFromGraph, stepDisplayMetrics, type MetricsMap } from "../lenses/mission/graph";
import { measuredCharsPerToken, averageGenerationRate } from "./tokenRate";
import { turnItems } from "./turnGroups";
import {
  CALL_KIND,
  PURPOSE,
  handleNamesExecution,
  isCompactionUsage,
  isTurnUsage,
  isUsageRecord,
  sumUsage,
  usageContribution,
  usagePurpose,
} from "./usageRecords";
import { ACTION } from "./ingest";

const LMS = "http://127.0.0.1:1234/v1";
const HOSTED = "azure:example.cognitiveservices.azure.com/gpt-5.1";

// ── the shared golden ────────────────────────────────────────────────────

const GOLDEN_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../../tests/usage-golden");
const golden: NormRecord[] = normAll(
  readFileSync(path.join(GOLDEN_DIR, "records.jsonl"), "utf8")
    .trim()
    .split("\n")
    .map((l) => JSON.parse(l) as RawRecord),
);
/** The shape of a usage golden's expected.json, as far as these tests read it. */
type GoldenTotals = { total: number; input: number; generated: number; cached?: number };
interface UsageGolden {
  overall: GoldenTotals;
  excluding_utility: GoldenTotals;
  usage_records: number;
  reported_entries: number;
  by_purpose: Record<string, GoldenTotals>;
  by_call_kind: unknown;
  by_endpoint: unknown;
  by_requested_model?: unknown;
}

const expected = JSON.parse(readFileSync(path.join(GOLDEN_DIR, "expected.json"), "utf8")) as UsageGolden;

/** Group the golden's usage records by one payload field, `(none)` when a
 *  record does not carry it: the breakdown step 2b's aggregator reports. Each
 *  record is summed through `sumUsage` alone, so the breakdown and the total
 *  share one arithmetic. */
function breakdown(field: string): Record<string, number> {
  const out: Record<string, number> = {};
  const counted = golden.filter((r) => r.action === ACTION.TelemetryTokens);
  for (const r of counted) {
    const p = r.payload as Record<string, unknown>;
    const k = typeof p[field] === "string" ? (p[field] as string) : "(none)";
    out[k] = (out[k] ?? 0) + sumUsage([r]).total;
  }
  return out;
}

describe("the shared usage golden (tests/usage-golden)", () => {
  it("overall: a plain sum of the usage records", () => {
    const s = sumUsage(golden);
    expect({ total: s.total, input: s.prompt, generated: s.completion, cached: s.cached }).toEqual(expected.overall);
    expect(s.usageRecords).toBe(expected.usage_records);
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
    const t = tokensOffMeter(golden);
    expect({ total: t.total, input: t.input, generated: t.generated, cached: t.cached }).toEqual(expected.overall);
    expect(t.utility).toBe(expected.by_purpose.utility.total);
  });

  /** (#2902 step 2b review) The ONE value domain, pinned on both sides:
   *  a finite number floors to an integer in [0, 2^53]; a string, bool,
   *  null or negative reads as 0 and reports nothing. `domain.jsonl` covers the low
   *  edge, `clamp.jsonl` the high one (kept apart so the sums stay exactly
   *  representable). */
  it.each([
    ["domain.jsonl", "domain-expected.json"],
    ["clamp.jsonl", "clamp-expected.json"],
  ])("the shared value-domain golden %s", (recordsFile, expectedFile) => {
    const records: NormRecord[] = normAll(
      readFileSync(path.join(GOLDEN_DIR, recordsFile), "utf8")
        .trim()
        .split("\n")
        .map((l) => JSON.parse(l) as RawRecord),
    );
    const exp = JSON.parse(readFileSync(path.join(GOLDEN_DIR, expectedFile), "utf8")) as UsageGolden;
    const s = sumUsage(records);
    expect({ total: s.total, input: s.prompt, generated: s.completion, cached: s.cached }).toEqual(exp.overall);
    expect(s.usageRecords).toBe(exp.usage_records);
    expect(s.reported).toBe(exp.reported_entries);
    if (exp.by_requested_model) {
      const out: Record<string, number> = {};
      const counted = records.filter((r) => r.action === ACTION.TelemetryTokens);
      for (const r of counted) {
        const p = r.payload as Record<string, unknown>;
        const k = typeof p.requested_model === "string" ? p.requested_model : "(none)";
        out[k] = (out[k] ?? 0) + sumUsage([r]).total;
      }
      expect(out).toEqual(exp.by_requested_model);
    }
  });
});

// ── helpers ──────────────────────────────────────────────────────────────

let clock = 0;
function r(o: RawRecord & { payload?: Record<string, unknown> }): NormRecord {
  clock += 1;
  const ts = new Date(Date.UTC(2026, 8, 26, 0, 0, clock)).toISOString();
  return norm({ ts, ...o, ...(o.payload ? { fields: o.payload } : {}) });
}
function usage(sid: string, handle: string, payload: Record<string, unknown>, mission?: string): NormRecord {
  return r({ action: "telemetry.tokens", category: "telemetry", source: "tokens", session_id: sid, handle, mission_id: mission, payload });
}
const complete = (sid: string, payload: Record<string, unknown>, mission?: string) =>
  r({ action: "dispatch.complete", session_id: sid, handle: "x", mission_id: mission, payload });

describe("usageContribution is sumUsage's per-record half (one arithmetic)", () => {
  const usageOnly = golden.filter(isUsageRecord);

  /** Fold records one at a time the way the mission graph does. */
  function fold(records: NormRecord[], exclude?: typeof PURPOSE.utility) {
    const out = { total: 0, prompt: 0, completion: 0, cached: null as number | null, utility: 0 };
    for (const rec of records) {
      const a = usageContribution(rec, exclude ? { exclude } : {});
      if (!a) continue;
      out.total += a.total;
      out.prompt += a.prompt;
      out.completion += a.completion;
      if (a.cached !== null) out.cached = (out.cached ?? 0) + a.cached;
      if (a.purpose === PURPOSE.utility) out.utility += a.total;
    }
    return out;
  }

  it("folding every golden usage record equals sumUsage over them, cached and utility included", () => {
    const s = sumUsage(usageOnly);
    expect(fold(usageOnly)).toEqual({ total: s.total, prompt: s.prompt, completion: s.completion, cached: s.cached, utility: s.utility });
    const w = sumUsage(usageOnly, { exclude: PURPOSE.utility });
    expect(fold(usageOnly, PURPOSE.utility)).toEqual({ total: w.total, prompt: w.prompt, completion: w.completion, cached: w.cached, utility: 0 });
  });

  it("the provider total wins; a split with no total falls back to prompt + completion", () => {
    clock = 0;
    const amount = (payload: Record<string, unknown>) => usageContribution(usage("s", "h", payload))!;
    expect(amount({ prompt_tokens: 7, completion_tokens: 3, total_tokens: 12 }).total).toBe(12);
    expect(amount({ prompt_tokens: 7, completion_tokens: 3 }).total).toBe(10);
    // (#3067) A reported total of 0 beside non-zero halves is unreported, the Rust `floor_tokens` rule.
    expect(amount({ prompt_tokens: 900, completion_tokens: 40, total_tokens: 0 }).total).toBe(940);
    expect(amount({ prompt_tokens: 900, total_tokens: 0 }).total).toBe(900);
    expect(amount({ total_tokens: 0 }).total).toBe(0);
    expect(amount({ unmanaged_tokens: 5 }).total).toBe(0);
    expect(amount({ remote_tokens: 5 }).total).toBe(0);
  });

  it("cached is null when the record does not report it, and a reported zero stays 0", () => {
    clock = 0;
    const amount = (payload: Record<string, unknown>) => usageContribution(usage("s", "h", payload))!;
    expect(amount({ total_tokens: 4 }).cached).toBeNull();
    expect(amount({ total_tokens: 4, cached_tokens: 0 }).cached).toBe(0);
    expect(amount({ total_tokens: 4, cached_tokens: 2 }).cached).toBe(2);
  });

  it("a record that is not a usage record contributes nothing", () => {
    clock = 0;
    expect(usageContribution(complete("s", { total_tokens: 9 }))).toBeNull();
  });
});

describe("a dispatch complete carries no tokens (no legacy fallback)", () => {
  it("a run with ONLY a complete is unmeasured, whatever the complete says", () => {
    const recs = [complete("old", { total_tokens: 100, prompt_tokens: 90, completion_tokens: 10, remote_tokens: 40 })];
    expect(sumUsage(recs)).toMatchObject({ total: 0, prompt: 0, completion: 0, cached: null, usageRecords: 0, reported: 0 });
  });

  it("a run with a usage record counts that record and never its complete, even an `absent` one", () => {
    const withTurn = [usage("s", "coder", { call_kind: CALL_KIND.turn, total_tokens: 40, prompt_tokens: 30, completion_tokens: 10 }), complete("s", { total_tokens: 999 })];
    expect(sumUsage(withTurn).total).toBe(40);
    const absentOnly = [usage("s", "coder", { call_kind: CALL_KIND.single_shot, token_source: "absent" }), complete("s", { total_tokens: 999 })];
    expect(sumUsage(absentOnly)).toMatchObject({ total: 0, usageRecords: 1, reported: 0 });
  });

  it("counts every usage record whatever execution it names: the sum is of records", () => {
    const named = (rec: NormRecord, execution: string): NormRecord => ({ ...rec, execution_id: execution });
    const recs = [
      named(usage("m.task.t", "s", { call_kind: CALL_KIND.map_item, total_tokens: 10 }, "m"), "exec-a"),
      named(usage("m.task.t2", "s", { call_kind: CALL_KIND.turn, total_tokens: 7 }, "m"), "exec-a"),
    ];
    expect(sumUsage(recs)).toMatchObject({ total: 17, usageRecords: 2 });
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
function utilityStreams(): [NormRecord[], NormRecord[]] {
  clock = 0;
  const M = "m-2";
  const without: NormRecord[] = [];
  const withU: NormRecord[] = [];
  const both = (x: NormRecord) => { without.push(x); withU.push(x); };
  const only = (x: NormRecord) => { withU.push(x); };
  const compaction = (sid: string, total: number, extra: Record<string, unknown> = {}, mission?: string) =>
    ({ ...usage(sid, "compactor", { call_kind: CALL_KIND.compaction, purpose: PURPOSE.utility, requested_model: "darkmux:c4b", endpoint: LMS, token_source: "provider", prompt_tokens: total - 80, completion_tokens: 80, total_tokens: total, ...extra }, mission), model: "darkmux:c4b" }) as NormRecord;

  both(r({ action: "dispatch.start", session_id: "h1", handle: "coder", model: "gpt-5.1", payload: { endpoint: HOSTED } }));
  both(r({ action: "dispatch.turn.heartbeat", session_id: "h1", payload: { turn_seq: 1, generated_chars: 20000, sampled_at_ms: 1000 } }));
  both(r({ action: "dispatch.turn", session_id: "h1", payload: { turn_seq: 1, generation_ms: 2000 } }));
  both(usage("h1", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, requested_model: "gpt-5.1", endpoint: HOSTED, token_source: "provider", turn_seq: 1, prompt_tokens: 900, completion_tokens: 100, total_tokens: 1000 }));
  only(compaction("h1", 580));
  only(compaction("h1", 120, { purpose: undefined }));
  both(r({ action: "dispatch.turn", session_id: "h1", payload: { turn_seq: 2, generation_ms: 500 } }));
  both(usage("h1", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, requested_model: "gpt-5.1", endpoint: HOSTED, token_source: "provider", turn_seq: 2, prompt_tokens: 1100, completion_tokens: 100, total_tokens: 1200 }));
  both(r({ action: "dispatch.complete", session_id: "h1", handle: "coder", payload: { endpoint: HOSTED, result_class: "ok", total_turns: 2, prompt_tokens: 2000, completion_tokens: 200, total_tokens: 2200 } }));

  both(r({ action: "dispatch.start", session_id: "task-t3", handle: "cs", mission_id: M, payload: { step_id: "cs", kind: "dispatch.internal" } }));
  both(usage("task-t3", "cs", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, requested_model: "darkmux:q", endpoint: LMS, token_source: "provider", turn_seq: 1, prompt_tokens: 400, completion_tokens: 50, total_tokens: 450, step_id: "cs" }, M));
  only(compaction("task-t3", 300, { step_id: "cs" }, M));
  both(r({ action: "dispatch.complete", session_id: "task-t3", handle: "cs", mission_id: M, payload: { step_id: "cs", kind: "dispatch.internal", total_tokens: 450, total_turns: 1 } }));
  return [without, withU];
}

const tile = (recs: NormRecord[], sid: string, label: string) =>
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

  it("names the tokens on no run, so each run's tokens plus them are the total (#3067)", () => {
    clock = 0;
    const recs = [
      usage("s1", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, prompt_tokens: 80, completion_tokens: 20, total_tokens: 100 }),
      usage("s2", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, total_tokens: 40 }, "m1"),
      usage("", "radio-router", { call_kind: CALL_KIND.single_shot, purpose: PURPOSE.utility, prompt_tokens: 7, completion_tokens: 2, total_tokens: 9 }),
      usage("", "radio-router", { call_kind: CALL_KIND.single_shot, purpose: PURPOSE.utility, total_tokens: 4 }),
      // A mission-only record is on the mission's row, so not on no run.
      usage("", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, total_tokens: 6 }, "m2"),
    ];
    const t = tokensOffMeter(recs);
    expect(t.noRun).toEqual({ calls: 2, tokens: 13 });
    const rows = sumUsage(recs.filter((x) => x.session_id || x.mission_id)).total;
    expect(rows + t.noRun.tokens).toBe(t.total);
    // With the listing: m1 (a row, claiming s2's record by mission) and s1 are
    // listed; m2 has no row, so it is unlisted, and the rows, no run and
    // unlisted are the total.
    const listed = tokensOffMeter(recs, new Set(["s1", "m1"]));
    expect(listed.unlisted).toEqual({ calls: 1, tokens: 6 });
    const listedRows = sumUsage(recs.filter((x) => x.session_id === "s1" || x.mission_id === "m1")).total;
    expect(listedRows + listed.noRun.tokens + listed.unlisted.tokens).toBe(listed.total);
    expect(tokensOffMeter(recs).unlisted).toEqual({ calls: 0, tokens: 0 });
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
    const g = tokensOffMeter(golden);
    expect(g.generated + g.input).toBe(g.total);
  });

  it("a provider total above prompt + completion shows ALL TOKENS above the two tiles, with no filler chip", () => {
    // A reasoning model reporting its reasoning outside completion_tokens.
    const recs = [usage("rz", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, prompt_tokens: 100, completion_tokens: 20, total_tokens: 150, reasoning_tokens: 30 })];
    const t = tokensOffMeter(recs);
    expect(t).toMatchObject({ total: 150, input: 100, generated: 20 });
    expect(Object.keys(t).sort()).toEqual(["cached", "generated", "input", "noRun", "runs", "total", "unlisted", "utility"]);
  });

  it("CACHED sums only reporting records, and is absent (null) when none report", () => {
    const [without] = utilityStreams();
    expect(tokensOffMeter(without).cached).toBeNull();
    const withCache = [...without, usage("h1", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, prompt_tokens: 10, completion_tokens: 1, total_tokens: 11, cached_tokens: 0 })];
    expect(tokensOffMeter(withCache).cached).toBe(0);
  });
});

const tileHint = (recs: NormRecord[], sid: string, label: string) =>
  runRegions(recs, sid, Date.UTC(2026, 8, 27)).metrics.find((m) => m.label === label)?.hintTitle;

describe("the run page and the mission graph count utility and name it (#3067)", () => {
  it("the run page's tiles count every usage record, and name the utility part on hover", () => {
    const [without, withU] = utilityStreams();
    // h1: work 900 + 1100 in, plus two compactions (500 + 40 in).
    expect(tile(without, "h1", "TOKENS IN")).toBe("2.00k");
    expect(tile(withU, "h1", "TOKENS IN")).toBe("2.54k");
    expect(tileHint(withU, "h1", "TOKENS IN")).toContain("700 tokens of utility calls");
    expect(tileHint(without, "h1", "TOKENS IN")).toBeUndefined();
  });

  it("a total-only record: the tiles name the row's total instead of showing a short split (#3067)", () => {
    clock = 0;
    const recs = [
      r({ action: "dispatch.start", session_id: "to", handle: "analyst", payload: {} }),
      usage("to", "analyst", { call_kind: CALL_KIND.single_shot, purpose: PURPOSE.work, total_tokens: 1200 }),
    ];
    expect(sumUsage(recs).total).toBe(1200);
    // No record reports a split: a dash, never a measured 0.
    expect(tile(recs, "to", "TOKENS IN")).toBe("—");
    expect(tile(recs, "to", "TOKENS OUT")).toBe("—");
    expect(tileHint(recs, "to", "TOKENS IN")).toContain("1.20k tokens in total");
    expect(tileHint(recs, "to", "TOKENS OUT")).toContain("1.20k tokens in total");
    // Only part split: the split part shows, the hover names the total.
    const part = [
      r({ action: "dispatch.start", session_id: "pt", handle: "coder", payload: {} }),
      usage("pt", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, prompt_tokens: 60, completion_tokens: 9, total_tokens: 69 }),
      usage("pt", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, total_tokens: 100 }),
    ];
    expect(tile(part, "pt", "TOKENS IN")).toBe("60");
    expect(tile(part, "pt", "TOKENS OUT")).toBe("9");
    expect(tileHint(part, "pt", "TOKENS IN")).toContain("169 tokens in total");
    // A split that adds up to the total says nothing extra.
    const whole = [r({ action: "dispatch.start", session_id: "wh", handle: "analyst", payload: {} }), usage("wh", "analyst", { call_kind: CALL_KIND.single_shot, purpose: PURPOSE.work, prompt_tokens: 60, completion_tokens: 9, total_tokens: 69 })];
    expect(tileHint(whole, "wh", "TOKENS IN")).toBeUndefined();
    // A provider total BELOW the split gets its own wording, never "not all split".
    const below = [r({ action: "dispatch.start", session_id: "bl", handle: "coder", payload: {} }), usage("bl", "coder", { call_kind: CALL_KIND.turn, purpose: PURPOSE.work, prompt_tokens: 60, completion_tokens: 9, total_tokens: 50 })];
    expect(tileHint(below, "bl", "TOKENS IN")).toContain("below input + generated");
    expect(tileHint(below, "bl", "TOKENS IN")).not.toContain("not all of it");
  });

  it("a session whose only calls are utility jobs (radio routing) IS that job: its page shows them", () => {
    clock = 0;
    const recs = [
      r({ action: "dispatch.start", session_id: "rr", handle: "radio-router", payload: {} }),
      usage("rr", "radio-router", { call_kind: CALL_KIND.single_shot, purpose: PURPOSE.utility, prompt_tokens: 50, completion_tokens: 7, total_tokens: 57 }),
      r({ action: "dispatch.complete", session_id: "rr", handle: "radio-router", payload: { total_tokens: 57, prompt_tokens: 50, completion_tokens: 7 } }),
    ];
    expect(tile(recs, "rr", "TOKENS IN")).toBe("50");
    expect(tile(recs, "rr", "TOKENS OUT")).toBe("7");
  });

  it("a single-shot run's tiles read its usage record, not its complete", () => {
    clock = 0;
    const recs = [
      r({ action: "dispatch.start", session_id: "ss", handle: "analyst", payload: {} }),
      usage("ss", "analyst", { call_kind: CALL_KIND.single_shot, purpose: PURPOSE.work, prompt_tokens: 60, completion_tokens: 9, total_tokens: 69 }),
      r({ action: "dispatch.complete", session_id: "ss", handle: "analyst", payload: { total_tokens: 1, prompt_tokens: 1, completion_tokens: 0 } }),
    ];
    expect(tile(recs, "ss", "TOKENS IN")).toBe("60");
    expect(tile(recs, "ss", "TOKENS OUT")).toBe("9");
  });

  it("a pre-5.0 session (complete only) is unmeasured: its tiles read a dash", () => {
    clock = 0;
    const recs = [
      r({ action: "dispatch.start", session_id: "lg", handle: "analyst", payload: {} }),
      r({ action: "dispatch.complete", session_id: "lg", handle: "analyst", payload: { total_tokens: 70, prompt_tokens: 61, completion_tokens: 9 } }),
    ];
    expect(tile(recs, "lg", "TOKENS IN")).toBe("—");
  });

  it("the mission graph's step meter is the usage sum with its utility part named; a complete is never read", () => {
    const [without, withU] = utilityStreams();
    const idx = indexGraph({ nodes: [{ id: "t3", kind: "task", steps: [{ id: "cs", kind: "dispatch.internal" }] }] as never });
    const fold = (recs: NormRecord[]) => recs.reduce<MetricsMap>((m, x) => applyRecordToMetrics(m, x, idx, "m-2"), {});
    expect(stepDisplayMetrics(fold(withU).cs)).toMatchObject({ tokens: 450 + 300, utility: 300 });
    expect(stepDisplayMetrics(fold(without).cs)).toMatchObject({ tokens: 450, utility: 0 });
    // No usage record for the step: unmeasured, whatever its complete says.
    const legacy = withU.filter((x) => !(x.action === ACTION.TelemetryTokens && x.session_id === "task-t3"));
    expect(stepDisplayMetrics(fold(legacy).cs).tokens).toBe(0);
    // A complete that disagrees changes nothing.
    const bumped = withU.map((x) => (x.action === ACTION.DispatchComplete && x.session_id === "task-t3" ? { ...x, payload: { ...(x.payload as object), total_tokens: 9999 } } : x));
    expect(stepDisplayMetrics(fold(bumped).cs).tokens).toBe(750);
  });

  it("the server's per-step total (seeded from the graph) and the live fold are one figure: the larger, never the sum", () => {
    const [, withU] = utilityStreams();
    const idx = indexGraph({ nodes: [{ id: "t3", kind: "task", steps: [{ id: "cs", kind: "dispatch.internal" }] }] as never });
    const live = withU.reduce<MetricsMap>((m, x) => applyRecordToMetrics(m, x, idx, "m-2"), {});
    const seeded = seedMetricsFromGraph(live, { nodes: [{ id: "t3", kind: "task", steps: [{ id: "cs", kind: "dispatch.internal", tokensFinal: 750, tokensUtility: 300 }] }] } as never);
    expect(stepDisplayMetrics(seeded.cs)).toMatchObject({ tokens: 750, utility: 300 });
    const fresh = seedMetricsFromGraph({}, { nodes: [{ id: "t3", kind: "task", steps: [{ id: "cs", kind: "dispatch.internal", tokensFinal: 750, tokensUtility: 300, startedTs: 1790000000 }] }] } as never);
    expect(stepDisplayMetrics(fresh.cs)).toMatchObject({ tokens: 750, utility: 300 });
  });

  it("a retried map item's per-call records sum to the item's tokens on every surface", () => {
    clock = 0;
    const M = "m-1";
    const start = r({ action: "dispatch.start", session_id: "task-t9", handle: "mp", mission_id: M, payload: { step_id: "mp", kind: "dispatch.map" } });
    const per = [0, 1, 2].map(() =>
      usage("task-t9", "mp", { call_kind: CALL_KIND.map_item, purpose: PURPOSE.work, requested_model: "q", endpoint: LMS, token_source: "provider", total_tokens: 10, prompt_tokens: 8, completion_tokens: 2, index: 0 }, M),
    );
    const recs = [
      start,
      ...per,
      r({ action: "dispatch.complete", session_id: "task-t9", handle: "mp", mission_id: M, payload: { step_id: "mp", kind: "dispatch.map", result_class: "ok" } }),
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
    const vis = (recs: NormRecord[]) => recs.filter((x) => x.action !== ACTION.TelemetryTokens);
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
