// (#2902 step 1a) Per-call usage records change NOTHING on screen yet.
//
// Every model call now emits one `telemetry.tokens` record with additive
// fields (`call_kind`, `requested_model`, `reported_model`, `endpoint`,
// `token_source`), and three paths emit one for the first time (the hosted
// and local single-shot dispatches and the `dispatch.single_shot` step), plus
// count-less `token_source: "absent"` records for calls whose reply carried no
// usage. Each consumer below is fed the SAME stream twice, as it looked
// before 1a and as it looks after, and must produce the identical result.
import { describe, it, expect } from "vitest";
import type { FlowRecord } from "../types/handwritten";
import { tokensOffMeter } from "../lenses/fleet/savings";
import { runRegions } from "../lenses/session/sessionRun";
import { applyRecordToMetrics, indexGraph, type MetricsMap } from "../lenses/mission/graph";
import { measuredCharsPerToken, averageGenerationRate } from "./tokenRate";
import { turnItems } from "./turnGroups";
import { countsInExecutionTokenSums, countsInLegacyTokenSums, handleNamesExecution, isCompactionUsage, isTurnUsage } from "./usageRecords";

const LMS = "http://127.0.0.1:1234/v1";
const HOSTED = "azure:example.cognitiveservices.azure.com/gpt-5.1";

let clock = 0;
function r(o: Partial<FlowRecord> & { payload?: Record<string, unknown> }): FlowRecord {
  clock += 1;
  const ts = new Date(Date.UTC(2026, 8, 26, 0, 0, clock)).toISOString();
  return { ts, ...o, ...(o.payload ? { fields: o.payload } : {}) } as FlowRecord;
}
function usage(sid: string, handle: string, payload: Record<string, unknown>, mission?: string): FlowRecord {
  return r({ action: "telemetry.tokens", category: "telemetry", source: "tokens", session_id: sid, handle, mission_id: mission, payload });
}
/** The 1a fields, stripped: what a pre-1a producer wrote for the same record. */
function pre1a(rec: FlowRecord): FlowRecord {
  if (rec.action !== "telemetry.tokens") return rec;
  const p = { ...((rec as { payload?: Record<string, unknown> }).payload || {}) };
  for (const k of ["call_kind", "requested_model", "reported_model", "endpoint", "token_source"]) delete p[k];
  return { ...rec, payload: p, fields: p } as FlowRecord;
}

/** One container run (two turns, the second with no usage), one local
 * single-shot, one hosted single-shot, one mission step each for
 * `dispatch.single_shot` (local) and `dispatch.map` (hosted, one item with no
 * usage). Returns `[before, after]`. */
function streams(): [FlowRecord[], FlowRecord[]] {
  clock = 0;
  const before: FlowRecord[] = [];
  const after: FlowRecord[] = [];
  const both = (x: FlowRecord) => { before.push(pre1a(x)); after.push(x); };
  const only = (x: FlowRecord) => { after.push(x); };

  // Container run: bookends + turns, heartbeats for the rate.
  both(r({ action: "dispatch.start", session_id: "c1", handle: "coder", model: "darkmux:q", payload: { runtime: "internal" } }));
  both(r({ action: "dispatch.turn.heartbeat", session_id: "c1", payload: { turn_seq: 1, generated_chars: 20000, sampled_at_ms: 1000 } }));
  both(r({ action: "dispatch.turn", session_id: "c1", payload: { turn_seq: 1, generation_ms: 2000 } }));
  both(usage("c1", "coder", { call_kind: "turn", requested_model: "darkmux:q", endpoint: LMS, token_source: "provider", turn_seq: 1, prompt_tokens: 900, completion_tokens: 100, total_tokens: 1000 }));
  both(r({ action: "dispatch.turn", session_id: "c1", payload: { turn_seq: 2, generation_ms: 500 } }));
  only(usage("c1", "coder", { call_kind: "turn", requested_model: "darkmux:q", endpoint: LMS, token_source: "absent", turn_seq: 2 }));
  both(r({ action: "dispatch.complete", session_id: "c1", handle: "coder", payload: { result_class: "ok", total_turns: 2, prompt_tokens: 900, completion_tokens: 100, total_tokens: 1000 } }));

  // Local single-shot (radio): tokens lived only on the complete before 1a.
  both(r({ action: "dispatch.start", session_id: "s-local", handle: "radio-router", payload: { runtime: "direct" } }));
  only(usage("s-local", "radio-router", { call_kind: "single_shot", requested_model: "darkmux:r", reported_model: "r", endpoint: LMS, token_source: "provider", prompt_tokens: 50, completion_tokens: 7, total_tokens: 57 }));
  both(r({ action: "dispatch.complete", session_id: "s-local", handle: "radio-router", payload: { runtime: "direct", result_class: "ok", total_turns: 1, prompt_tokens: 50, completion_tokens: 7, total_tokens: 57 } }));

  // Hosted single-shot.
  both(r({ action: "dispatch.start", session_id: "s-hosted", handle: "pr-reviewer", payload: { runtime: "direct", endpoint: HOSTED } }));
  only(usage("s-hosted", "pr-reviewer", { call_kind: "single_shot", requested_model: "gpt-5.1", reported_model: "gpt-5.1-2026", endpoint: HOSTED, token_source: "provider", prompt_tokens: 300, completion_tokens: 20, total_tokens: 330 }));
  both(r({ action: "dispatch.complete", session_id: "s-hosted", handle: "pr-reviewer", payload: { runtime: "direct", endpoint: HOSTED, result_class: "ok", total_turns: 1, prompt_tokens: 300, completion_tokens: 20, total_tokens: 330 } }));

  // A local single-shot whose reply carried NO usage: the complete says
  // null, the new record says `token_source: "absent"` with no counts.
  both(r({ action: "dispatch.start", session_id: "s-nousage", handle: "radio-router", payload: { runtime: "direct" } }));
  only(usage("s-nousage", "radio-router", { call_kind: "single_shot", requested_model: "darkmux:r", endpoint: LMS, token_source: "absent" }));
  both(r({ action: "dispatch.complete", session_id: "s-nousage", handle: "radio-router", payload: { runtime: "direct", result_class: "ok", total_turns: 1, prompt_tokens: null, completion_tokens: null, total_tokens: null } }));

  // Mission steps: a LOCAL `dispatch.single_shot` and a HOSTED `dispatch.map`,
  // under a mission whose own run session carries no telemetry (so the run
  // page rolls its inner sessions up).
  const M = "m-1";
  both(r({ action: "mission start", session_id: "mission-m-1", handle: "mission", mission_id: M, payload: {} }));
  both(r({ action: "dispatch start", session_id: "task-t1", handle: "ss", mission_id: M, payload: { step_id: "ss", kind: "dispatch.single_shot" } }));
  only(usage("task-t1", "ss", { call_kind: "single_shot", requested_model: "darkmux:q", endpoint: LMS, token_source: "provider", total_tokens: 70, prompt_tokens: 60, completion_tokens: 10 }, M));
  both(r({ action: "dispatch complete", session_id: "task-t1", handle: "ss", mission_id: M, payload: { step_id: "ss", kind: "dispatch.single_shot", result_class: "ok", total_tokens: 70 } }));
  both(r({ action: "dispatch start", session_id: "task-t2", handle: "mp", mission_id: M, payload: { step_id: "mp", kind: "dispatch.map", endpoint: HOSTED } }));
  both(usage("task-t2", "mp", { call_kind: "map_item", requested_model: "gpt-5.1", endpoint: HOSTED, token_source: "provider", total_tokens: 40, prompt_tokens: 30, completion_tokens: 10, remote: true, index: 0 }, M));
  only(usage("task-t2", "mp", { call_kind: "map_item", requested_model: "gpt-5.1", endpoint: HOSTED, token_source: "absent", remote: true, index: 1 }, M));
  both(r({ action: "dispatch complete", session_id: "task-t2", handle: "mp", mission_id: M, payload: { step_id: "mp", kind: "dispatch.map", endpoint: HOSTED, result_class: "ok", remote_tokens: 40 } }));
  return [before, after];
}

describe("#2902 step 1a: per-call usage records change no consumer's result", () => {
  it("savings hero (tokensOffMeter): totals, split, chips and run counts", () => {
    const [before, after] = streams();
    const b = tokensOffMeter(before);
    expect(tokensOffMeter(after)).toEqual(b);
    // The fixture exercises the fallback it protects: both single-shot runs
    // are counted from their completes.
    expect(b.total).toBe(1000 + 57 + 330 + 70 + 40);
    expect(b.runs).toBeGreaterThan(0);
  });

  it("run page (runRegions): every session's tiles", () => {
    const [before, after] = streams();
    for (const sid of ["c1", "s-local", "s-hosted", "s-nousage", "mission-m-1", "task-t1", "task-t2"]) {
      expect(runRegions(after, sid, Date.UTC(2026, 8, 27))).toEqual(runRegions(before, sid, Date.UTC(2026, 8, 27)));
    }
  });

  it("mission graph fold (applyRecordToMetrics): step tokens and the local/cloud marker", () => {
    const [before, after] = streams();
    const idx = indexGraph({ nodes: [
      { id: "t1", kind: "task", steps: [{ id: "ss", kind: "dispatch.single_shot" }] },
      { id: "t2", kind: "task", steps: [{ id: "mp", kind: "dispatch.map" }] },
    ] as never });
    const fold = (recs: FlowRecord[]) => recs.reduce<MetricsMap>((m, x) => applyRecordToMetrics(m, x, idx, "m-1"), {});
    const b = fold(before);
    const a = fold(after);
    expect(a).toEqual(b);
    // The local step stays local although its usage record names an endpoint.
    expect(a.ss?.cloud).toBe(false);
    expect(a.ss?.tokFinal).toBe(70);
    expect(a.mp?.cloud).toBe(true);
    expect(a.mp?.tokRun).toBe(40);
  });

  it("live rate and calibration (tokenRate): single-shot records never enter a turn's bucket", () => {
    const [before, after] = streams();
    expect(measuredCharsPerToken(after)).toBe(measuredCharsPerToken(before));
    expect(averageGenerationRate([after])).toEqual(averageGenerationRate([before]));
    // A single-shot record carrying a turn_seq-less completion count must not
    // create a bucket either (the guard, not the missing key, keeps it out).
    const stray = usage("c1", "coder", { call_kind: "single_shot", turn_seq: 1, completion_tokens: 999, endpoint: LMS });
    expect(measuredCharsPerToken(before)).not.toBe(4); // calibrated, not the default
    expect(measuredCharsPerToken([...after, stray])).toBe(measuredCharsPerToken(before));
    expect(averageGenerationRate([[...after, stray]])).toEqual(averageGenerationRate([before]));
  });

  it("turn groups (turnItems): per-turn output tokens", () => {
    const [before, after] = streams();
    const vis = (recs: FlowRecord[]) => recs.filter((x) => x.action !== "telemetry.tokens");
    // No guard needed here: turn grouping keys on `turn_seq`, which only a
    // turn's usage record carries (single-shot and map-item records have none).
    expect(turnItems(vis(after), after)).toEqual(turnItems(vis(before), before));
  });

  it("a retried map item's per-call records total the same as its old single per-item record", () => {
    // (#2902 review) Before: one record per ITEM with the attempts' counts
    // accumulated. After: one per CALL. Three attempts of 10 tokens each.
    clock = 0;
    const M = "m-1";
    const head = [
      r({ action: "dispatch start", session_id: "task-t9", handle: "mp", mission_id: M, payload: { step_id: "mp", kind: "dispatch.map" } }),
    ];
    const one = usage("task-t9", "mp", { total_tokens: 30, prompt_tokens: 24, completion_tokens: 6, remote: false, index: 0 }, M);
    const per = [0, 1, 2].map(() =>
      ({ ...usage("task-t9", "mp", { call_kind: "map_item", requested_model: "q", endpoint: LMS, token_source: "provider", total_tokens: 10, prompt_tokens: 8, completion_tokens: 2, remote: false, index: 0 }, M), ts: one.ts }),
    );
    const tail = [
      r({ action: "dispatch complete", session_id: "task-t9", handle: "mp", mission_id: M, payload: { step_id: "mp", kind: "dispatch.map", result_class: "ok" } }),
    ];
    const before = [...head, one, ...tail];
    const after = [...head, ...per, ...tail];
    expect(tokensOffMeter(after)).toEqual(tokensOffMeter(before));
    expect(tokensOffMeter(after).total).toBe(30);
    expect(runRegions(after, "task-t9", Date.UTC(2026, 8, 27))).toEqual(runRegions(before, "task-t9", Date.UTC(2026, 8, 27)));
    const idx = indexGraph({ nodes: [{ id: "t9", kind: "task", steps: [{ id: "mp", kind: "dispatch.map" }] }] as never });
    const fold = (recs: FlowRecord[]) => recs.reduce<MetricsMap>((m, x) => applyRecordToMetrics(m, x, idx, M), {});
    const a = fold(after);
    const b = fold(before);
    expect(a.mp?.tokRun).toBe(30);
    expect({ ...a.mp, lastTs: 0 }).toEqual({ ...b.mp, lastTs: 0 });
  });

  it("the predicates", () => {
    expect(countsInLegacyTokenSums({})).toBe(true);
    expect(countsInLegacyTokenSums({ call_kind: "turn", token_source: "provider" })).toBe(true);
    expect(countsInLegacyTokenSums({ call_kind: "map_item" })).toBe(true);
    expect(countsInLegacyTokenSums({ call_kind: "single_shot" })).toBe(false);
    expect(countsInLegacyTokenSums({ call_kind: "turn", token_source: "absent" })).toBe(false);
    expect(isTurnUsage({})).toBe(true);
    expect(isTurnUsage({ call_kind: "turn" })).toBe(true);
    expect(isTurnUsage({ call_kind: "single_shot" })).toBe(false);
    expect(isTurnUsage({ call_kind: "map_item" })).toBe(false);
  });
});

// (#2902 step 1b) The runtime's COMPACTOR calls now emit their own usage
// records (`call_kind: "compaction"`), attributed to the compactor (record
// `handle: "compactor"`, its own `model`). Each stream below is fed with and
// without them.
describe("#2902 step 1b: compaction usage records", () => {
  const M = "m-2";
  function compactionStreams(): [FlowRecord[], FlowRecord[]] {
    clock = 0;
    const without: FlowRecord[] = [];
    const withC: FlowRecord[] = [];
    const both = (x: FlowRecord) => { without.push(x); withC.push(x); };
    const only = (x: FlowRecord) => { withC.push(x); };
    const compaction = (sid: string, total: number, extra: Record<string, unknown> = {}, mission?: string) =>
      ({ ...usage(sid, "compactor", { call_kind: "compaction", requested_model: "darkmux:c4b", reported_model: "c4b", endpoint: LMS, token_source: "provider", generation: 1, parent_role_id: "coder", parent_model: "darkmux:q", prompt_tokens: total - 80, completion_tokens: 80, total_tokens: total, ...extra }, mission), model: "darkmux:c4b" }) as FlowRecord;

    // A HOSTED-brain container run that compacts once between its turns.
    both(r({ action: "dispatch.start", session_id: "h1", handle: "coder", model: "gpt-5.1", payload: { runtime: "internal", endpoint: HOSTED } }));
    both(r({ action: "dispatch.turn.heartbeat", session_id: "h1", payload: { turn_seq: 1, generated_chars: 20000, sampled_at_ms: 1000 } }));
    both(r({ action: "dispatch.turn", session_id: "h1", payload: { turn_seq: 1, generation_ms: 2000 } }));
    both(usage("h1", "coder", { call_kind: "turn", requested_model: "gpt-5.1", endpoint: HOSTED, token_source: "provider", turn_seq: 1, prompt_tokens: 900, completion_tokens: 100, total_tokens: 1000 }));
    only(compaction("h1", 580));
    both(r({ action: "dispatch.turn", session_id: "h1", payload: { turn_seq: 2, generation_ms: 500 } }));
    both(usage("h1", "coder", { call_kind: "turn", requested_model: "gpt-5.1", endpoint: HOSTED, token_source: "provider", turn_seq: 2, prompt_tokens: 1100, completion_tokens: 100, total_tokens: 1200 }));
    both(r({ action: "dispatch.complete", session_id: "h1", handle: "coder", payload: { runtime: "internal", endpoint: HOSTED, result_class: "ok", total_turns: 2, prompt_tokens: 2000, completion_tokens: 200, total_tokens: 2200 } }));

    // A mission step (container path) that compacts: the graph's step meter.
    both(r({ action: "dispatch start", session_id: "task-t3", handle: "cs", mission_id: M, payload: { step_id: "cs", kind: "dispatch.internal" } }));
    both(usage("task-t3", "cs", { call_kind: "turn", requested_model: "darkmux:q", endpoint: LMS, token_source: "provider", turn_seq: 1, prompt_tokens: 400, completion_tokens: 50, total_tokens: 450, step_id: "cs" }, M));
    only(compaction("task-t3", 300, { step_id: "cs" }, M));
    return [without, withC];
  }

  it("the fleet hero counts compaction in its total", () => {
    const [without, withC] = compactionStreams();
    const a = tokensOffMeter(without);
    const b = tokensOffMeter(withC);
    expect(b.total).toBe(a.total + 580 + 300);
    // The turn re-read decomposition is unchanged; the compactor calls' input
    // lands in the unclassified bucket rather than breaking the turn sequence.
    expect(b.fresh).toBe(a.fresh);
    expect(b.reread).toBe(a.reread);
    expect(b.uncls).toBe(a.uncls + 500 + 220);
    expect(b.runs).toBe(a.runs);
    expect(b.cloudRuns).toBe(a.cloudRuns);
  });

  it("the run page never blends compaction into the execution's own tiles (contract 8)", () => {
    const [without, withC] = compactionStreams();
    // `lastBeatMs` is liveness, not accounting: a compactor call IS activity
    // in the session, so the last beat may move to it. Everything else holds.
    const tiles = (recs: FlowRecord[], sid: string) => ({ ...runRegions(recs, sid, Date.UTC(2026, 8, 27)), lastBeatMs: 0 });
    for (const sid of ["h1", "task-t3"]) {
      expect(tiles(withC, sid)).toEqual(tiles(without, sid));
    }
  });

  it("the mission graph's step meter never blends compaction in (contract 8)", () => {
    const [without, withC] = compactionStreams();
    const idx = indexGraph({ nodes: [{ id: "t3", kind: "task", steps: [{ id: "cs", kind: "dispatch.internal" }] }] as never });
    const fold = (recs: FlowRecord[]) => recs.reduce<MetricsMap>((m, x) => applyRecordToMetrics(m, x, idx, M), {});
    const a = fold(withC);
    expect(a.cs?.tokRun).toBe(450);
    expect({ ...a.cs, lastTs: 0 }).toEqual({ ...fold(without).cs, lastTs: 0 });
  });

  it("live rate and calibration exclude compaction records, even one carrying a turn_seq", () => {
    const [without, withC] = compactionStreams();
    expect(measuredCharsPerToken(without)).not.toBe(4); // calibrated, not the default
    expect(measuredCharsPerToken(withC)).toBe(measuredCharsPerToken(without));
    expect(averageGenerationRate([withC])).toEqual(averageGenerationRate([without]));
    const stray = usage("h1", "compactor", { call_kind: "compaction", turn_seq: 1, completion_tokens: 999, endpoint: LMS });
    expect(measuredCharsPerToken([...withC, stray])).toBe(measuredCharsPerToken(without));
    expect(averageGenerationRate([[...withC, stray]])).toEqual(averageGenerationRate([without]));
  });

  it("turn groups are unchanged", () => {
    const [without, withC] = compactionStreams();
    const vis = (recs: FlowRecord[]) => recs.filter((x) => x.action !== "telemetry.tokens");
    expect(turnItems(vis(withC), withC)).toEqual(turnItems(vis(without), without));
  });

  it("the predicates", () => {
    expect(countsInExecutionTokenSums({ call_kind: "compaction", token_source: "provider" })).toBe(false);
    expect(countsInExecutionTokenSums({ call_kind: "turn", token_source: "provider" })).toBe(true);
    expect(countsInExecutionTokenSums({})).toBe(true);
    expect(countsInExecutionTokenSums({ call_kind: "single_shot" })).toBe(false);
    expect(countsInLegacyTokenSums({ call_kind: "compaction", token_source: "provider" })).toBe(true);
    expect(isTurnUsage({ call_kind: "compaction" })).toBe(false);
    expect(isCompactionUsage({ call_kind: "compaction" })).toBe(true);
    expect(isCompactionUsage({ call_kind: "turn" })).toBe(false);
    expect(handleNamesExecution({ handle: "compactor", action: "telemetry.tokens", payload: { call_kind: "compaction" } } as never)).toBe(false);
    expect(handleNamesExecution({ handle: "coder", action: "telemetry.tokens", payload: { call_kind: "turn" } } as never)).toBe(true);
    expect(handleNamesExecution({ handle: "coder", action: "dispatch.turn" } as never)).toBe(true);
  });
});
