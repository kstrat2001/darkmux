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
import { countsInLegacyTokenSums, isTurnUsage } from "./usageRecords";

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
