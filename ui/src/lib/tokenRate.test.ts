import { describe, expect, it } from "vitest";
import type { FlowRecord } from "../types/handwritten";
import {
  DEFAULT_CHARS_PER_TOKEN,
  STALL_AFTER_MS,
  aggregateLiveState,
  aggregateTokenRate,
  charsPerSecond,
  currentTokenRate,
  deriveLiveState,
  executionRole,
  executionTokenReading,
  heartbeatSamples,
  isStalled,
  liveStatePriority,
  measuredCharsPerToken,
  averageGenerationRate,
  liveStateLabel,
  liveStateWhileConnected,
  lastHeartbeatMs,
} from "./tokenRate";

const SID = "darkmux-coder-1790125784225";
const atSec = (sec: number) => new Date(Date.UTC(2026, 8, 23, 1, 9, 0) + sec * 1000).toISOString();

/** New-shape heartbeat: carries `sampled_at_ms` (ms) + `generated_chars`
 *  (content + reasoning), same fields dispatch_internal.rs::heartbeat_payload
 *  now forwards (#2877). */
const beat = (sampledAtMs: number, generatedChars: number, cumulativeChars = generatedChars): FlowRecord =>
  ({
    ts: new Date(sampledAtMs).toISOString(),
    action: "dispatch.turn.heartbeat",
    session_id: SID,
    payload: { sampled_at_ms: sampledAtMs, generated_chars: generatedChars, cumulative_chars: cumulativeChars },
  }) as unknown as FlowRecord;

/** Old-shape heartbeat, exactly what a pre-#2877 runtime forwards: no
 *  `sampled_at_ms`, no `generated_chars` — only the whole-second flow `ts`
 *  and the answer-only `cumulative_chars`. */
const oldBeat = (sec: number, cumulativeChars: number): FlowRecord =>
  ({
    ts: atSec(sec),
    action: "dispatch.turn.heartbeat",
    session_id: SID,
    payload: { cumulative_chars: cumulativeChars },
  }) as unknown as FlowRecord;

const tokensRecord = (completionTokens: number): FlowRecord =>
  ({
    ts: atSec(0),
    action: "telemetry.tokens",
    session_id: SID,
    payload: { completion_tokens: completionTokens },
  }) as unknown as FlowRecord;

describe("heartbeatSamples", () => {
  it("reads new-shape sampled_at_ms + generated_chars", () => {
    const samples = heartbeatSamples([beat(1_000, 40), beat(3_000, 120)]);
    expect(samples).toEqual([
      { atMs: 1_000, chars: 40 },
      { atMs: 3_000, chars: 120 },
    ]);
  });

  it("falls back to whole-second ts + cumulative_chars on an older runtime's heartbeat", () => {
    const samples = heartbeatSamples([oldBeat(0, 10), oldBeat(2, 30)]);
    expect(samples).toEqual([
      { atMs: Date.parse(atSec(0)), chars: 10 },
      { atMs: Date.parse(atSec(2)), chars: 30 },
    ]);
  });

  it("ignores non-heartbeat records and never crashes on a missing chars field", () => {
    const weird = { ts: atSec(0), action: "dispatch.turn.heartbeat", session_id: SID, payload: {} } as unknown as FlowRecord;
    const other = { ts: atSec(0), action: "dispatch.turn", session_id: SID, payload: { cumulative_chars: 999 } } as unknown as FlowRecord;
    expect(heartbeatSamples([weird, other])).toEqual([]);
  });

  it("sorts samples into time order regardless of input order", () => {
    const samples = heartbeatSamples([beat(3_000, 120), beat(1_000, 40)]);
    expect(samples.map((s) => s.atMs)).toEqual([1_000, 3_000]);
  });
});

describe("charsPerSecond", () => {
  it("computes Δchars/Δms as chars/sec", () => {
    // 80 chars over 2000ms = 40 chars/sec
    expect(charsPerSecond({ atMs: 1_000, chars: 40 }, { atMs: 3_000, chars: 120 })).toBeCloseTo(40, 5);
  });

  it("returns null when time did not advance (guards a divide-by-zero/negative-rate reading)", () => {
    expect(charsPerSecond({ atMs: 1_000, chars: 40 }, { atMs: 1_000, chars: 120 })).toBeNull();
    expect(charsPerSecond({ atMs: 2_000, chars: 40 }, { atMs: 1_000, chars: 120 })).toBeNull();
  });

  it("returns null when the char counter went backward (a session reset, not a negative rate)", () => {
    expect(charsPerSecond({ atMs: 1_000, chars: 120 }, { atMs: 2_000, chars: 40 })).toBeNull();
  });
});

describe("measuredCharsPerToken", () => {
  it("falls back to DEFAULT_CHARS_PER_TOKEN before any billed usage lands", () => {
    expect(measuredCharsPerToken([beat(1_000, 40)])).toBe(DEFAULT_CHARS_PER_TOKEN);
  });

  it("measures generated chars over summed billed completion tokens once usage lands", () => {
    // 4,000 chars over 1,000 completion tokens = 4 chars/token exactly.
    const ratio = measuredCharsPerToken([beat(1_000, 4_000), tokensRecord(600), tokensRecord(400)]);
    expect(ratio).toBeCloseTo(4, 5);
  });

  // The live shape that read 11 tok/s for a model generating ~50: chars reset
  // every turn, and the in-flight turn has chars but no billed tokens yet.
  // Taking the max chars across turns over the finished turns' tokens
  // divided turn 2's 33k chars by turn 1's 391 tokens.
  const turnBeat = (turn: number, ms: number, chars: number): FlowRecord =>
    ({ ...beat(ms, chars), payload: { sampled_at_ms: ms, generated_chars: chars, turn_seq: turn } }) as unknown as FlowRecord;
  const turnTokens = (turn: number, completion: number): FlowRecord =>
    ({ ...tokensRecord(completion), payload: { completion_tokens: completion, turn_seq: turn } }) as unknown as FlowRecord;

  it("pairs each turn's chars with that turn's tokens and ignores the in-flight turn", () => {
    const ratio = measuredCharsPerToken([
      turnBeat(1, 1_000, 1_200),
      turnBeat(1, 3_000, 2_400),
      turnTokens(1, 600),
      turnBeat(2, 5_000, 8_000),
      turnBeat(2, 7_000, 40_000),
    ]);
    expect(ratio).toBeCloseTo(4, 5);
  });

  it("does not calibrate from a short turn: its tail after the last heartbeat and its tool-call tokens dominate", () => {
    // Turn 1 of the live run: 566 chars seen, 391 tokens billed (1.45).
    const ratio = measuredCharsPerToken([turnBeat(1, 1_000, 566), turnTokens(1, 391)]);
    expect(ratio).toBe(DEFAULT_CHARS_PER_TOKEN);
  });

  // (#2886/#2885, real recorded shape — Splash bake-off run
  // `darkmux-coding-refresh-rotation-1790242191590`) Turn 2 was cut by a
  // reasoning checkpoint: 74,617 generated chars, only 91 billed tokens
  // (≈820 chars/token). Turn 3 was ordinary: 7,227 chars, 1,943 tokens
  // (≈3.72 chars/token, close to the real measured ratio). Blended together
  // the pair calibrates to ≈40 chars/token — this must calibrate from turn
  // 3 ALONE.
  const checkpoint = (turn: number): FlowRecord =>
    ({ ts: atSec(0), action: "dispatch.checkpoint", session_id: SID, payload: { turn_seq: turn, checkpoint: 1, verdict: "conclude" } }) as unknown as FlowRecord;

  it("excludes a checkpointed turn from calibration, keyed on the dispatch.checkpoint record (#2886)", () => {
    const ratio = measuredCharsPerToken([
      turnBeat(2, 1_000, 74_617),
      turnTokens(2, 91),
      checkpoint(2),
      turnBeat(3, 5_000, 7_227),
      turnTokens(3, 1_943),
    ]);
    expect(ratio).toBeCloseTo(7_227 / 1_943, 5);
  });

  it("falls back to DEFAULT_CHARS_PER_TOKEN when the ONLY turn with usage is checkpointed", () => {
    const ratio = measuredCharsPerToken([turnBeat(2, 1_000, 74_617), turnTokens(2, 91), checkpoint(2)]);
    expect(ratio).toBe(DEFAULT_CHARS_PER_TOKEN);
  });

  // (#2886 pass 4, MUST — fresh-reviewer finding 1) A `continue` checkpoint
  // did NOT cut the stream — the harness judged the turn mid-thought and let
  // it keep going, so its telemetry.tokens bills the WHOLE turn normally.
  // Only `verdict: "conclude"` (the harness forcing a hand-off) actually
  // truncates. Real shape, session
  // `darkmux-coding-refresh-rotation-1790243027020` turn 2: checkpointed
  // with `verdict: "continue"`, still billed normally.
  const continueCheckpoint = (turn: number): FlowRecord =>
    ({ ts: atSec(0), action: "dispatch.checkpoint", session_id: SID, payload: { turn_seq: turn, checkpoint: 1, verdict: "continue" } }) as unknown as FlowRecord;

  it("does NOT exclude a turn whose checkpoint verdict is 'continue' — it billed normally", () => {
    const ratio = measuredCharsPerToken([turnBeat(2, 1_000, 8_050), continueCheckpoint(2), turnTokens(2, 2_300)]);
    // 8,050 / 2,300 ≈ 3.5 chars/token, the real measured ratio — must NOT
    // fall back to DEFAULT_CHARS_PER_TOKEN as if this turn were excluded.
    expect(ratio).toBeCloseTo(8_050 / 2_300, 5);
  });

  it("a turn with BOTH a continue and a later conclude checkpoint is still excluded (the conclude cut it)", () => {
    const ratio = measuredCharsPerToken([
      turnBeat(2, 1_000, 74_617),
      continueCheckpoint(2),
      turnTokens(2, 91),
      checkpoint(2),
      turnBeat(3, 5_000, 7_227),
      turnTokens(3, 1_943),
    ]);
    expect(ratio).toBeCloseTo(7_227 / 1_943, 5);
  });
});

describe("currentTokenRate", () => {
  it("is null with fewer than two samples (a fresh execution)", () => {
    expect(currentTokenRate([beat(1_000, 40)])).toBeNull();
    expect(currentTokenRate([])).toBeNull();
  });

  it("converts the latest Δchars/Δms into tok/s using the measured ratio", () => {
    // 80 chars over 2s = 40 chars/sec; latest cumulative 120 chars / 30 tokens = 4 chars/token → 10 tok/s.
    const reading = currentTokenRate([beat(1_000, 40), beat(3_000, 120), tokensRecord(30)]);
    expect(reading).not.toBeNull();
    expect(reading!.tokensPerSec).toBeCloseTo(10, 5);
    expect(reading!.atMs).toBe(3_000);
    expect(reading!.estimate).toBe(true);
  });

  it("uses only the two most recent samples, not the whole history", () => {
    const a = currentTokenRate([beat(0, 0), beat(1_000, 1000), beat(3_000, 1080)]);
    // Last pair: 80 chars over 2000ms = 40 chars/sec / 4 default = 10 tok/s.
    expect(a!.tokensPerSec).toBeCloseTo(10, 5);
  });

  // (#2885, acceptance criterion) "previous turn measured, new turn with
  // one heartbeat gives a carried reading, not null."
  it("carries the last measured rate into a new turn's lone first heartbeat, marked carried", () => {
    const hbT = (atMs: number, chars: number, turnSeq: number): FlowRecord =>
      ({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } }) as unknown as FlowRecord;
    // Turn 1: opens at 0 (every turn does — finding 2), then 800 chars over
    // 2s, then another 800 over 2s = 400 chars/s -> 100 tok/s at the
    // default 4 chars/token. The carry must read the (800, 1600) pair, not
    // the opening (0, 800) one (finding 2). Turn 2's own first (and only)
    // sample follows 18s later.
    const reading = currentTokenRate([hbT(1_000, 0, 1), hbT(3_000, 800, 1), hbT(5_000, 1_600, 1), hbT(23_000, 50, 2)]);
    expect(reading).not.toBeNull();
    expect(reading!.tokensPerSec).toBeCloseTo(100, 5);
    expect(reading!.carried).toBe(true);
  });

  it("is null, not carried, with only ONE heartbeat ever (nothing to carry from)", () => {
    expect(currentTokenRate([beat(1_000, 40)])).toBeNull();
  });

  // (#2886 pass 4, MUST — fresh-reviewer finding 2) Every turn opens with a
  // heartbeat at `generated_chars: 0`, sent before the first token — a pair
  // whose EARLIER sample is 0 chars spans prompt reading, not generation,
  // and reads near-zero. Real shape, session
  // `darkmux-coding-refresh-rotation-1790243027020`: turn 3 went 0 -> 4
  // chars over 13s; turn 4 then showed a dimmed "0" on a flat ring. The
  // carry must skip that near-zero pair and reach further back for the
  // most recent pair with real progress.
  it("never carries a pair whose earlier sample is 0 chars — reaches further back for real progress", () => {
    const hbT = (atMs: number, chars: number, turnSeq: number): FlowRecord =>
      ({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } }) as unknown as FlowRecord;
    const records = [
      // Turn 2: opens at 0 (every turn does), then real progress: 800 chars
      // over 2s, then another 800 over 2s = 400 chars/s -> 100 tok/s.
      hbT(0, 0, 2),
      hbT(2_000, 800, 2),
      hbT(4_000, 1_600, 2),
      // Turn 3: near-zero — its earlier sample (0) must be skipped, not carried.
      hbT(10_000, 0, 3),
      hbT(23_000, 4, 3),
      // Turn 4: lone first heartbeat — nothing of its own to read from yet.
      hbT(30_000, 1, 4),
    ];
    const reading = currentTokenRate(records);
    expect(reading).not.toBeNull();
    expect(reading!.carried).toBe(true);
    expect(reading!.tokensPerSec).toBeCloseTo(100, 5);
  });

  it("returns null (not a near-zero carry) when the ONLY prior pair has a 0-chars earlier sample", () => {
    const hbT = (atMs: number, chars: number, turnSeq: number): FlowRecord =>
      ({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } }) as unknown as FlowRecord;
    const records = [hbT(0, 0, 3), hbT(13_000, 4, 3), hbT(30_000, 1, 4)];
    expect(currentTokenRate(records)).toBeNull();
  });

  // (#2886 pass 5, MUST — fresh-reviewer finding F1) A pair whose earlier
  // sample is 0 chars is not ALWAYS untrustworthy — a FAST one (about one
  // heartbeat interval) is real signal: the model produced its first chars
  // almost immediately. This is the DIRECT path (the pair IS the current
  // turn's own last two samples), so a trusted fast opener must read fresh
  // (not carried).
  it("trusts a FAST opener pair (<= ~one heartbeat interval) as a fresh, non-carried reading", () => {
    const hbT = (atMs: number, chars: number, turnSeq: number): FlowRecord =>
      ({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } }) as unknown as FlowRecord;
    // 40 chars over 2s = 20 chars/s / 4 default = 5 tok/s.
    const reading = currentTokenRate([hbT(0, 0, 5), hbT(2_000, 40, 5)]);
    expect(reading).not.toBeNull();
    expect(reading!.tokensPerSec).toBeCloseTo(5, 5);
    expect(reading!.carried).toBeUndefined();
  });

  // (#2886 pass 5, MUST — fresh-reviewer finding F1, "the naive skip made
  // nulls jump 41->148") A SLOW opener as the CURRENT pair must not be
  // trusted directly (it would read ~0.1 tok/s at full brightness — real
  // data: 0 -> 4 chars over 13s) — but the fallback search must still find
  // an EARLIER pair that IS a fast, trustworthy opener, rather than
  // rejecting every 0-chars pair outright (the over-correction this finding
  // also warns against). Turn 4's own opening pair (0 -> 800 over 2s) is
  // exactly that: a fast opener, one turn back.
  it("rejects a SLOW opener as the current reading but still finds an earlier FAST opener to carry", () => {
    const hbT = (atMs: number, chars: number, turnSeq: number): FlowRecord =>
      ({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } }) as unknown as FlowRecord;
    const records = [
      // Turn 4: a FAST opener — 800 chars over 2s = 400 chars/s -> 100 tok/s.
      hbT(0, 0, 4),
      hbT(2_000, 800, 4),
      // Turn 5 (current): a SLOW opener — 4 chars over 13s, not trustworthy
      // as a direct reading.
      hbT(10_000, 0, 5),
      hbT(23_000, 4, 5),
    ];
    const reading = currentTokenRate(records);
    expect(reading).not.toBeNull();
    expect(reading!.carried).toBe(true);
    expect(reading!.tokensPerSec).toBeCloseTo(100, 5);
  });

  // (#2886 pass 4, MUST — fresh-reviewer finding 3) After a checkpoint,
  // `generated_chars` restarts within the SAME turn_seq (real shapes:
  // 119,547 -> 1; 74,617 -> 3), so the current turn's own last two
  // heartbeats read a NEGATIVE Δchars and `charsPerSecond` returns null.
  // Before this fix `currentTokenRate` returned null right there without
  // ever trying the carried rate — must fall back instead.
  it("falls back to the carried rate when the current turn's own last pair has restarted (post-checkpoint) chars", () => {
    const hbT = (atMs: number, chars: number, turnSeq: number): FlowRecord =>
      ({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } }) as unknown as FlowRecord;
    const records = [
      // Turn 4: real progress to carry from. 800 chars/2s = 400 chars/s.
      hbT(0, 0, 4),
      hbT(2_000, 800, 4),
      hbT(4_000, 1_600, 4),
      // Turn 5 (current): a checkpoint reset the counter mid-turn — the
      // SAME turn_seq's own last pair goes backward (119,547 -> 1).
      hbT(6_000, 119_547, 5),
      hbT(8_000, 1, 5),
    ];
    const reading = currentTokenRate(records);
    expect(reading).not.toBeNull();
    expect(reading!.carried).toBe(true);
    expect(reading!.tokensPerSec).toBeCloseTo(100, 5);
  });
});

describe("isStalled", () => {
  it("is false with no heartbeats yet (not yet started is not the same as stalled)", () => {
    expect(isStalled([], 10_000)).toBe(false);
  });

  it("is false just after a heartbeat, true once STALL_AFTER_MS has passed with no new one", () => {
    const recs = [beat(1_000, 40)];
    expect(isStalled(recs, 1_000 + STALL_AFTER_MS - 1)).toBe(false);
    expect(isStalled(recs, 1_000 + STALL_AFTER_MS + 1)).toBe(true);
  });
});

describe("aggregateTokenRate", () => {
  it("sums current readings across running executions", () => {
    const exec1 = [beat(1_000, 40), beat(3_000, 120)]; // 40 chars/sec / 4 = 10 tok/s
    const exec2 = [beat(1_000, 40), beat(3_000, 200)]; // 80 chars/sec / 4 = 20 tok/s
    const reading = aggregateTokenRate([exec1, exec2], 3_500);
    expect(reading?.tokensPerSec).toBeCloseTo(30, 5);
    expect(reading?.carried).toBe(false);
  });

  it("treats a stalled/fresh execution as contributing 0, not dropping the machine's total", () => {
    const generating = [beat(1_000, 40), beat(3_000, 120)];
    const fresh: FlowRecord[] = [beat(5_000, 0)];
    expect(aggregateTokenRate([generating, fresh], 5_500)?.tokensPerSec).toBeCloseTo(10, 5);
  });

  it("is null only when NO execution has a reading at all", () => {
    expect(aggregateTokenRate([[beat(1_000, 0)], []], 1_500)).toBeNull();
  });

  // (#2885) A carried reading from any ONE contributing execution marks the
  // whole aggregate carried — the caller renders a single number, dimmed or
  // not, never a per-execution split.
  it("marks the aggregate carried when any contributing execution's own reading is carried", () => {
    const hbT = (atMs: number, chars: number, turnSeq: number): FlowRecord =>
      ({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } }) as unknown as FlowRecord;
    // exec1: turn 1 opens at 0, then two real-progress intervals, then
    // turn 2's lone first sample — carried (from turn 1's second interval,
    // not its 0-opening one — finding 2).
    const exec1 = [hbT(0, 0, 1), hbT(2_000, 400, 1), hbT(4_000, 800, 1), hbT(23_000, 50, 2)];
    // exec2: an ordinary fresh same-turn pair — not carried.
    const exec2 = [beat(23_000, 0), beat(25_000, 200)];
    const reading = aggregateTokenRate([exec1, exec2], 25_500);
    expect(reading).not.toBeNull();
    expect(reading!.carried).toBe(true);
  });
});

// (#2877 pass 2 — "is this resting? can't tell") The legible state a stopped
// tube reads while between heartbeats. Real record shapes from
// `~/.darkmux/flows/2026-09-24.jsonl`, session
// `darkmux-coding-refresh-rotation-1790224432369`:
//   dispatch.tool  {tool_seq, tool_name, ...}                — a tool call
//   dispatch.turn  {turn_seq, finish_reason, usage, ...}     — ENDS a turn
//   dispatch.rest  {ms, turn, rest_ms, rests, reason, state} — a completed
//                  thermal rest (`ms` present); the announce-only sibling
//                  `{reason, state, pause: false, delay_ms}` carries no `ms`
//                  and is a pacing nudge, not a rest (matches sessionRun.ts's
//                  existing `dispatch.rest`-with-`ms` filter).
const start = (atMs: number): FlowRecord =>
  ({ ts: new Date(atMs).toISOString(), action: "dispatch.start", session_id: SID, payload: {} }) as unknown as FlowRecord;
const turnEnd = (atMs: number, turnSeq: number): FlowRecord =>
  ({ ts: new Date(atMs).toISOString(), action: "dispatch.turn", session_id: SID, payload: { turn_seq: turnSeq } }) as unknown as FlowRecord;
const tool = (atMs: number): FlowRecord =>
  ({ ts: new Date(atMs).toISOString(), action: "dispatch.tool", session_id: SID, payload: { tool_name: "bash" } }) as unknown as FlowRecord;
const rest = (atMs: number, ms: number): FlowRecord =>
  ({ ts: new Date(atMs).toISOString(), action: "dispatch.rest", session_id: SID, payload: { ms, reason: "thermal-duty-cycle" } }) as unknown as FlowRecord;
const restAnnounceOnly = (atMs: number): FlowRecord =>
  ({ ts: new Date(atMs).toISOString(), action: "dispatch.rest", session_id: SID, payload: { reason: "thermal-duty-cycle", pause: false, delay_ms: 15_000 } }) as unknown as FlowRecord;

describe("deriveLiveState", () => {
  it("is generating while a heartbeat is fresh — same threshold currentTokenRate uses", () => {
    const recs = [beat(1_000, 40), beat(3_000, 120)];
    expect(deriveLiveState(recs, 3_000 + STALL_AFTER_MS - 1)).toEqual({ state: "generating" });
  });

  // (#2886 pass 4, CONSIDER-do-it — fresh-reviewer finding 4) A turn whose
  // only heartbeat(s) read `generated_chars: 0` is still reading the prompt
  // / thinking before its first token — GENERATING would light the lamp and
  // drive the wave over a stretch that hasn't produced anything yet.
  it("is prompt, not generating, while the only fresh heartbeat(s) read 0 chars", () => {
    const oneZero = [beat(1_000, 0)];
    expect(deriveLiveState(oneZero, 1_000 + STALL_AFTER_MS - 1)).toEqual({ state: "prompt" });
    const twoZero = [beat(1_000, 0), beat(3_000, 0)];
    expect(deriveLiveState(twoZero, 3_000 + STALL_AFTER_MS - 1)).toEqual({ state: "prompt" });
  });

  it("is generating once the fresh heartbeat shows real progress, even right after a 0-chars opener", () => {
    const recs = [beat(1_000, 0), beat(3_000, 40)];
    expect(deriveLiveState(recs, 3_000 + STALL_AFTER_MS - 1)).toEqual({ state: "generating" });
  });

  it("is prompt right after dispatch.start, before the first heartbeat", () => {
    expect(deriveLiveState([start(0)], 500)).toEqual({ state: "prompt" });
  });

  it("is prompt once a turn has ended and no heartbeat for the next one has landed yet", () => {
    // The turn-end marker lands well after the last heartbeat has gone
    // stale (past STALL_AFTER_MS) — without it this would read "stalled".
    const turnEndAt = 1_000 + STALL_AFTER_MS + 200;
    const recs = [beat(0, 10), beat(1_000, 200), turnEnd(turnEndAt, 1)];
    expect(deriveLiveState(recs, turnEndAt + 100)).toEqual({ state: "prompt" });
  });

  it("is tools while a dispatch.tool is the latest thing and no heartbeat has followed it", () => {
    const toolAt = 1_000 + STALL_AFTER_MS + 200;
    const recs = [beat(0, 10), beat(1_000, 200), turnEnd(toolAt, 1), tool(toolAt)];
    // (#2890) The completed tool's name rides along for the TOOLS icon.
    expect(deriveLiveState(recs, toolAt + 2_000)).toEqual({ state: "tools", toolName: "bash" });
  });

  // (Found via screenshot verification against the real corpus — a
  // dispatch.tool record whose own `ts` truncates to the SAME whole second
  // as the heartbeat immediately preceding it, e.g. heartbeat
  // sampled_at_ms=...636141 vs the tool record's ts="...:16Z" (=...636000).
  // The marker's truncated 636000 < the heartbeat's precise 636141, so the
  // naive `marker.atMs >= lastBeatAt` comparison read the tool call as
  // OLDER than the heartbeat it actually followed, falling through to
  // "stalled" instead of "tools".)
  it("does not mistake a same-second marker for one older than the heartbeat it followed (real-record whole-second ts vs ms-precise sampled_at_ms)", () => {
    const beatAtMs = 1_790_224_636_141;
    // Record `ts` truncates to the whole second BELOW the heartbeat's own
    // ms-precise sampled_at_ms — real wire behavior (`toISOString` would
    // keep the ms; a real flow record's `ts` does not).
    const toolTs = new Date(Math.floor(beatAtMs / 1000) * 1000).toISOString();
    const recs: FlowRecord[] = [
      beat(beatAtMs - 2_000, 4_483),
      beat(beatAtMs, 5_623),
      { ts: toolTs, action: "dispatch.tool", session_id: SID, payload: { tool_name: "edit" } } as unknown as FlowRecord,
    ];
    expect(deriveLiveState(recs, beatAtMs + STALL_AFTER_MS + 1_000)).toEqual({ state: "tools", toolName: "edit" });
  });

  it("is rest with a countdown while inside a reported rest's ms window, then falls to prompt once it elapses", () => {
    const recs = [tool(0), rest(1_000, 15_000)];
    // 1s into the 15s window → 14s left (ceil).
    expect(deriveLiveState(recs, 2_000)).toEqual({ state: "rest", restSecondsLeft: 14 });
    // Right at the boundary the window has fully elapsed.
    expect(deriveLiveState(recs, 1_000 + 15_000)).toEqual({ state: "prompt" });
    // Comfortably past it too.
    expect(deriveLiveState(recs, 20_000)).toEqual({ state: "prompt" });
  });

  it("ignores the announce-only rest record (no ms) — a pacing nudge, not a rest", () => {
    const recs = [tool(0), restAnnounceOnly(1_000)];
    expect(deriveLiveState(recs, 2_000)).toEqual({ state: "tools", toolName: "bash" });
  });

  it("is stalled once a heartbeat has gone stale with nothing after it to explain the gap", () => {
    const recs = [beat(0, 10), beat(1_000, 200)];
    expect(deriveLiveState(recs, 1_000 + STALL_AFTER_MS + 1)).toEqual({ state: "stalled" });
  });

  it("prefers a marker newer than the last stale heartbeat over calling it stalled", () => {
    const recs = [beat(0, 10), beat(1_000, 200), tool(1_000 + STALL_AFTER_MS + 500)];
    expect(deriveLiveState(recs, 1_000 + STALL_AFTER_MS + 600)).toEqual({ state: "tools", toolName: "bash" });
  });

  it("ignores records after the given clock — never reads the future", () => {
    // The rest record technically exists in the array, but its ts is after
    // `nowMs`; the state must read as if it had not happened yet.
    const recs = [tool(0), rest(5_000, 15_000)];
    expect(deriveLiveState(recs, 1_000)).toEqual({ state: "tools", toolName: "bash" });
  });

  it("mutation self-check: without the rest branch this would read prompt/tools instead", () => {
    // Documents the case the rest branch exists to catch — asserted for real
    // above; this is the paired "what breaks if the branch is removed" case
    // referenced in the report's Self-QA section.
    const recs = [tool(0), rest(1_000, 15_000)];
    const reading = deriveLiveState(recs, 2_000);
    expect(reading.state).toBe("rest");
    expect(reading.state).not.toBe("tools");
  });
});

describe("aggregateLiveState", () => {
  it("is generating when any execution is generating", () => {
    const generating = [beat(0, 10), beat(2_000, 90)];
    const resting = [tool(0), rest(1_000, 15_000)];
    expect(aggregateLiveState([generating, resting], 2_000)).toEqual({ state: "generating" });
  });

  it("surfaces rest over tools/prompt when nothing is generating", () => {
    const toolsOnly = [tool(0)];
    const resting = [tool(0), rest(1_000, 15_000)];
    expect(aggregateLiveState([toolsOnly, resting], 2_000)).toEqual({ state: "rest", restSecondsLeft: 14 });
  });

  it("falls back to stalled only when every execution is stalled", () => {
    const stalledExec = [beat(0, 10), beat(1_000, 200)];
    expect(aggregateLiveState([stalledExec], 1_000 + STALL_AFTER_MS + 1)).toEqual({ state: "stalled" });
  });

  it("is null when there are no executions at all", () => {
    expect(aggregateLiveState([], 1_000)).toBeNull();
  });
});

describe("averageGenerationRate", () => {
  // A finished run's TOK/S is the model's generation rate: billed completion
  // tokens over the time it spent generating (`generation_ms` per turn), not
  // over the wall clock, which includes rests, tools and prompt reading
  // (a real run read 45 over wall clock against ~80 over generation time).
  const turn = (sid: string, seq: number, genMs: number | undefined): FlowRecord =>
    ({ ts: atSec(seq), action: "dispatch.turn", session_id: sid, payload: genMs == null ? { turn_seq: seq } : { turn_seq: seq, generation_ms: genMs } }) as unknown as FlowRecord;
  const tok = (sid: string, seq: number, completion: number): FlowRecord =>
    ({ ts: atSec(seq), action: "telemetry.tokens", session_id: sid, payload: { turn_seq: seq, completion_tokens: completion } }) as unknown as FlowRecord;

  it("sums billed tokens over summed generation time, paired per turn", () => {
    const reading = averageGenerationRate([[turn("a", 1, 5_000), tok("a", 1, 400), turn("a", 2, 5_000), tok("a", 2, 600)]]);
    expect(reading?.tokensPerSec).toBeCloseTo(100, 5);
    expect(reading?.billedTurns).toBe(2);
    expect(reading?.totalTurns).toBe(2);
  });

  it("pairs by session as well as turn, across a mission's executions", () => {
    const reading = averageGenerationRate([
      [turn("a", 1, 4_000), tok("a", 1, 400)],
      [turn("b", 1, 6_000), tok("b", 1, 200)],
    ]);
    expect(reading?.tokensPerSec).toBeCloseTo(60, 5);
  });

  it("skips a turn with no generation_ms, and is null when no turn has one", () => {
    expect(averageGenerationRate([[turn("a", 1, 5_000), tok("a", 1, 500), turn("a", 2, undefined), tok("a", 2, 900)]])?.tokensPerSec).toBeCloseTo(
      100,
      5,
    );
    expect(averageGenerationRate([[turn("a", 1, undefined), tok("a", 1, 500)]])).toBeNull();
  });

  // (#2886) Real recorded shape: a checkpointed turn's billed tokens cover
  // only its final continuation while `generation_ms` spans the whole
  // chain — including it drags a real ~150 tok/s down to ~34.
  const checkpoint = (sid: string, seq: number): FlowRecord =>
    ({ ts: atSec(seq), action: "dispatch.checkpoint", session_id: sid, payload: { turn_seq: seq, checkpoint: 1, verdict: "conclude" } }) as unknown as FlowRecord;

  it("excludes a checkpointed turn from the average, labeling how many of the paired turns were billed", () => {
    const reading = averageGenerationRate([
      [
        turn("a", 1, 200), // billed, ordinary
        tok("a", 1, 20), // 100 tok/s
        turn("a", 2, 220_000), // checkpointed: huge generation_ms, tiny billed tokens
        tok("a", 2, 91),
        checkpoint("a", 2),
      ],
    ]);
    expect(reading).not.toBeNull();
    // Only turn 1 is billed: 20 tokens / 0.2s = 100 tok/s, not the ~0.5
    // tok/s a naive (20+91)/(200+220000)ms average would read.
    expect(reading!.tokensPerSec).toBeCloseTo(100, 5);
    expect(reading!.billedTurns).toBe(1);
    expect(reading!.totalTurns).toBe(2);
  });

  it("returns a null rate (not a fallback average) when EVERY paired turn is checkpointed", () => {
    const reading = averageGenerationRate([[turn("a", 1, 220_000), tok("a", 1, 91), checkpoint("a", 1)]]);
    expect(reading).toEqual({ tokensPerSec: null, billedTurns: 0, totalTurns: 1 });
  });

  // (#2886 pass 4, MUST — fresh-reviewer finding 1) Real shape, session
  // `darkmux-coding-refresh-rotation-1790243027020` turn 2: checkpointed
  // with `verdict: "continue"` — NOT cut, billed 32,000 + 1,803 = 33,803
  // completion tokens over 127,348 ms of generation (~265 tok/s). Excluding
  // it (treating `continue` the same as `conclude`) is the bug that made
  // this run read 102 tok/s "avg · 5 of 6 turns" instead of ~210.
  const continueCheckpoint = (sid: string, seq: number): FlowRecord =>
    ({ ts: atSec(seq), action: "dispatch.checkpoint", session_id: sid, payload: { turn_seq: seq, checkpoint: 1, verdict: "continue" } }) as unknown as FlowRecord;

  it("does NOT exclude a turn whose checkpoint verdict is 'continue' from the average", () => {
    const reading = averageGenerationRate([[turn("a", 2, 127_348), tok("a", 2, 33_803), continueCheckpoint("a", 2)]]);
    expect(reading).not.toBeNull();
    expect(reading!.billedTurns).toBe(1);
    expect(reading!.totalTurns).toBe(1);
    expect(reading!.tokensPerSec).toBeCloseTo(33_803 / (127_348 / 1000), 5);
  });
});

describe("liveStateLabel", () => {
  it("names the prompt wait as 'reading prompt', not the bare word the page's prompt disclosure also uses", () => {
    expect(liveStateLabel({ state: "prompt" } as never)).toBe("reading prompt");
  });
});

// (#2886 pass 3, "STALL while disconnected") A lost daemon connection must
// not let a false STALL claim through.
describe("liveStateWhileConnected", () => {
  it("downgrades a stalled reading to null (the shared 'no live execution' rendering) when disconnected", () => {
    expect(liveStateWhileConnected({ state: "stalled" }, false)).toBeNull();
  });

  it("leaves a stalled reading alone while connected", () => {
    expect(liveStateWhileConnected({ state: "stalled" }, true)).toEqual({ state: "stalled" });
  });

  it("leaves every non-stalled state alone even while disconnected — only STALL is a false claim", () => {
    expect(liveStateWhileConnected({ state: "generating" }, false)).toEqual({ state: "generating" });
    expect(liveStateWhileConnected({ state: "rest", restSecondsLeft: 5 }, false)).toEqual({ state: "rest", restSecondsLeft: 5 });
    expect(liveStateWhileConnected({ state: "tools" }, false)).toEqual({ state: "tools" });
    expect(liveStateWhileConnected({ state: "prompt" }, false)).toEqual({ state: "prompt" });
  });

  it("passes a null reading through unchanged regardless of connection", () => {
    expect(liveStateWhileConnected(null, false)).toBeNull();
    expect(liveStateWhileConnected(null, true)).toBeNull();
  });

  // (#2886 pass 4, do-it — fresh-reviewer finding 5, "half-open connection
  // race") STALL_AFTER_MS (30s) can fire before the header's own watchdog
  // notices a half-open connection (LIVE_CONTACT_TIMEOUT_MS, ~40s) — a
  // half-open connection delivers no visible `error` event, so `connected`
  // stays `true` while heartbeats have already gone silent. A stall claim
  // is trusted only when the daemon has answered (`lastContactMs`) AFTER
  // the point the stalled execution's own last heartbeat
  // (`lastHeartbeatMs`) plus STALL_AFTER_MS — i.e. after the deadline that
  // heartbeat missed.
  describe("the half-open connection race (finding 5)", () => {
    const lastBeat = 1_000_000;

    it("trusts a stalled reading when contact is confirmed AFTER the stall deadline", () => {
      const halfOpen = { lastContactMs: lastBeat + STALL_AFTER_MS + 1, lastHeartbeatMs: lastBeat };
      expect(liveStateWhileConnected({ state: "stalled" }, true, halfOpen)).toEqual({ state: "stalled" });
    });

    it("downgrades to no-signal when the last confirmed contact predates the stall deadline", () => {
      // Contact was confirmed, but BEFORE the point a stall claim needs one.
      const halfOpen = { lastContactMs: lastBeat + STALL_AFTER_MS - 1, lastHeartbeatMs: lastBeat };
      expect(liveStateWhileConnected({ state: "stalled" }, true, halfOpen)).toBeNull();
    });

    it("downgrades to no-signal when there is no contact evidence at all", () => {
      const halfOpen = { lastContactMs: null, lastHeartbeatMs: lastBeat };
      expect(liveStateWhileConnected({ state: "stalled" }, true, halfOpen)).toBeNull();
    });

    it("skips the half-open check entirely when omitted — old 2-arg behavior unchanged", () => {
      expect(liveStateWhileConnected({ state: "stalled" }, true)).toEqual({ state: "stalled" });
    });

    it("skips the half-open check when there is no heartbeat to compare against", () => {
      const halfOpen = { lastContactMs: null, lastHeartbeatMs: null };
      expect(liveStateWhileConnected({ state: "stalled" }, true, halfOpen)).toEqual({ state: "stalled" });
    });

    it("the plain disconnected check still wins outright, before the half-open check ever runs", () => {
      const halfOpen = { lastContactMs: lastBeat + STALL_AFTER_MS + 1, lastHeartbeatMs: lastBeat };
      expect(liveStateWhileConnected({ state: "stalled" }, false, halfOpen)).toBeNull();
    });

    it("never touches a non-stalled reading, even with failing half-open evidence", () => {
      const halfOpen = { lastContactMs: null, lastHeartbeatMs: lastBeat };
      expect(liveStateWhileConnected({ state: "generating" }, true, halfOpen)).toEqual({ state: "generating" });
    });
  });
});

describe("lastHeartbeatMs", () => {
  it("is null when no execution has ever produced a heartbeat", () => {
    expect(lastHeartbeatMs([[], []])).toBeNull();
  });

  it("is the MOST RECENT heartbeat across several executions", () => {
    const execA = [beat(1_000, 40), beat(3_000, 120)];
    const execB = [beat(2_000, 10), beat(5_000, 90)];
    expect(lastHeartbeatMs([execA, execB])).toBe(5_000);
  });

  it("ignores an execution with no heartbeats without throwing", () => {
    const execA = [beat(1_000, 40)];
    expect(lastHeartbeatMs([execA, []])).toBe(1_000);
  });
});

// (pre-PR review, 2026-09-24) The findings below were each PROVEN on real
// runs before these tests existed.
describe("which executions count: live ones only", () => {
  const rec = (sid: string, atMs: number, action: string, payload: Record<string, unknown> = {}, source?: string): FlowRecord =>
    ({ ts: new Date(atMs).toISOString(), action, session_id: sid, ...(source ? { source } : {}), payload }) as unknown as FlowRecord;
  const hb = (sid: string, atMs: number, chars: number, turn = 1) =>
    rec(sid, atMs, "dispatch.turn.heartbeat", { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turn });

  it("a FINISHED execution's last rate never adds to a live one's (a mission read 350 tok/s with one execution at ~40)", () => {
    const finished = [rec("a", 0, "dispatch.start"), hb("a", 1_000, 0), hb("a", 3_000, 800), rec("a", 3_500, "dispatch.complete")];
    const live = [rec("b", 10_000, "dispatch.start"), hb("b", 11_000, 0), hb("b", 13_000, 800)];
    // Both measured 400 chars/s -> 100 tok/s at the default 4 chars/token.
    expect(aggregateTokenRate([finished, live], 13_500)?.tokensPerSec).toBeCloseTo(100, 5);
  });

  it("a resting or tool-running execution adds nothing while another generates", () => {
    const resting = [rec("a", 0, "dispatch.start"), hb("a", 1_000, 0), hb("a", 3_000, 800), rec("a", 3_500, "dispatch.rest", { ms: 15_000 })];
    const live = [rec("b", 0, "dispatch.start"), hb("b", 3_000, 0), hb("b", 5_000, 800)];
    expect(aggregateTokenRate([resting, live], 5_500)?.tokensPerSec).toBeCloseTo(100, 5);
  });

  it("the mission's own run-grain session (a mission-sourced start) never reads as PROMPT over a stalled execution", () => {
    const runGrain = [rec("m", 0, "dispatch.start", {}, "mission")];
    const stalled = [rec("b", 0, "dispatch.start"), hb("b", 1_000, 0), hb("b", 3_000, 800)];
    expect(aggregateLiveState([runGrain, stalled], 3_000 + 60_000)?.state).toBe("stalled");
  });

  // (fix-pass verifier, PROVEN on a real crawl) The page's candidate sessions
  // include the mission's lifecycle session (`mission start`, `phase start`)
  // and every scheduler task session (`step start`/`step complete`). None is
  // an execution; each read as PROMPT and outranked a real stall.
  it("a mission's lifecycle and task sessions are not executions and never read as PROMPT over a stall", () => {
    const lifecycle = [rec("mission-m", 0, "mission start"), rec("mission-m", 0, "phase start")];
    const task = [rec("task-1-m", 500, "step start"), rec("task-1-m", 900, "step timing")];
    const stalled = [rec("b", 0, "dispatch.start"), hb("b", 1_000, 0), hb("b", 3_000, 800)];
    expect(aggregateLiveState([lifecycle, task, stalled], 3_000 + 60_000)?.state).toBe("stalled");
  });

  it("is null, not PROMPT, when no live execution exists (a mission between model steps)", () => {
    const runGrain = [rec("m", 0, "dispatch.start", {}, "mission")];
    const finished = [rec("a", 0, "dispatch.start"), hb("a", 1_000, 0), rec("a", 2_000, "dispatch.complete")];
    const lifecycle = [rec("mission-m", 0, "mission start")];
    expect(aggregateLiveState([runGrain, finished, lifecycle], 10_000)).toBeNull();
  });

  it("a finished execution never reads as PROMPT over a stalled one", () => {
    const finished = [rec("a", 0, "dispatch.start"), hb("a", 1_000, 0), rec("a", 2_000, "dispatch.turn", { turn_seq: 1 }), rec("a", 2_000, "dispatch.complete")];
    const stalled = [rec("b", 0, "dispatch.start"), hb("b", 1_000, 0), hb("b", 3_000, 800)];
    expect(aggregateLiveState([finished, stalled], 3_000 + 60_000)?.state).toBe("stalled");
  });
});

describe("tools vs reading prompt, from the tool COMPLETION records", () => {
  // `dispatch.tool` is emitted on `tool.completed`. A turn that ends with N
  // tool calls is TOOLS until N completions; after that the model is
  // reading the results: PROMPT. Before this, the next turn's prompt
  // processing read as TOOLS.
  const turn = (atMs: number, seq: number, calls: number): FlowRecord =>
    ({ ts: new Date(atMs).toISOString(), action: "dispatch.turn", session_id: SID, payload: { turn_seq: seq, tool_calls_count: calls } }) as unknown as FlowRecord;

  it("is TOOLS between the turn end and the last of its tool completions", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turn(4_000, 1, 2), tool(5_000)];
    expect(deriveLiveState(recs, 6_000).state).toBe("tools");
  });

  it("is PROMPT once every tool the turn called has completed", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turn(4_000, 1, 2), tool(5_000), tool(6_000)];
    expect(deriveLiveState(recs, 7_000).state).toBe("prompt");
  });
  // (#2890) The TOOLS center shows an icon for the tool. The runtime emits
  // `dispatch.tool` on COMPLETION, so the name the viewer has is the latest
  // completed call of THIS turn; a previous turn's tool must never leak in.
  const namedTool = (atMs: number, name: string): FlowRecord =>
    ({ ts: new Date(atMs).toISOString(), action: "dispatch.tool", session_id: SID, payload: { tool_name: name } }) as unknown as FlowRecord;

  it("names the latest completed tool of this turn while TOOLS", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turn(4_000, 1, 3), namedTool(5_000, "read"), namedTool(6_000, "edit")];
    expect(deriveLiveState(recs, 7_000)).toEqual({ state: "tools", toolName: "edit" });
  });

  it("names no tool before this turn's first completion", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turn(4_000, 1, 2)];
    expect(deriveLiveState(recs, 5_000)).toEqual({ state: "tools" });
  });

  it("never carries the previous turn's tool into this one", () => {
    const recs = [
      start(0),
      beat(1_000, 0),
      beat(3_000, 800),
      turn(4_000, 1, 1),
      namedTool(5_000, "bash"),
      beat(6_000, 0),
      beat(8_000, 900),
      turn(9_000, 2, 2),
    ];
    expect(deriveLiveState(recs, 10_000)).toEqual({ state: "tools" });
  });

  it("ignores a tool completion from after the clock (playback cut)", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turn(4_000, 1, 3), namedTool(5_000, "read"), namedTool(9_000, "edit")];
    expect(deriveLiveState(recs, 6_000)).toEqual({ state: "tools", toolName: "read" });
  });

  it("carries the name through executionTokenReading and aggregateLiveState", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turn(4_000, 1, 3), namedTool(5_000, "search")];
    expect(executionTokenReading(recs, 6_000).toolName).toBe("search");
    expect(aggregateLiveState([recs], 6_000)).toEqual({ state: "tools", toolName: "search" });
  });
});

describe("a turn's first reading never pairs with the previous turn", () => {
  // (#2885) Pre-#2885 this returned `null` outright — "no reading yet" for
  // several seconds every short turn, the exact tile-reads-dead defect the
  // issue reports. It now falls back to the CARRIED reading instead (see
  // `currentTokenRate` describe above): still never a near-zero rate spanning
  // the tool gap between turn 1's last sample and turn 2's first, but also
  // never a bare "—" while the run is plainly still generating.
  it("never pairs a new turn's first sample with the previous turn's last (no near-zero rate across the tool gap)", () => {
    const hbT = (atMs: number, chars: number, turnSeq: number): FlowRecord =>
      ({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } }) as unknown as FlowRecord;
    // Turn 1: opens at 0, then 800 chars over 2s, then another 800 over 2s
    // = 400 chars/s throughout. Turn 2's first sample is 18s later — if
    // this paired turn 1's last (1,600) against turn 2's first across that
    // 18s gap it would read a near-zero rate; instead it carries turn 1's
    // own (800, 1,600) interval — its 0-opening interval is skipped
    // (finding 2) even though it would have given the same number here.
    const reading = currentTokenRate([hbT(1_000, 0, 1), hbT(3_000, 800, 1), hbT(5_000, 1_600, 1), hbT(23_000, 900, 2)]);
    expect(reading).not.toBeNull();
    expect(reading!.carried).toBe(true);
    expect(reading!.tokensPerSec).toBeCloseTo(100, 5);
  });
});

// (#2881) The fleet card pager's per-execution data.
describe("executionRole", () => {
  const rec = (action: string, handle?: string): FlowRecord =>
    ({ ts: atSec(0), action, session_id: SID, ...(handle ? { handle } : {}), payload: {} }) as unknown as FlowRecord;

  it("strips the darkmux/ prefix and lowercases", () => {
    expect(executionRole([rec("dispatch.start", "darkmux/coder")])).toBe("coder");
  });

  it("lowercases a bare handle with no prefix", () => {
    expect(executionRole([rec("dispatch.start", "CODER")])).toBe("coder");
  });

  it("is empty when nothing in the set carries a handle", () => {
    expect(executionRole([rec("dispatch.turn.heartbeat")])).toBe("");
  });

  it("falls back to any record's handle when no dispatch.start carries one", () => {
    expect(executionRole([rec("dispatch.turn.heartbeat", "darkmux/reviewer")])).toBe("reviewer");
  });

  it("prefers the LATEST dispatch.start's handle over an earlier one", () => {
    const early = { ts: atSec(0), action: "dispatch.start", session_id: SID, handle: "darkmux/coder", payload: {} } as unknown as FlowRecord;
    const later = { ts: atSec(10), action: "dispatch.start", session_id: SID, handle: "darkmux/reviewer", payload: {} } as unknown as FlowRecord;
    expect(executionRole([early, later])).toBe("reviewer");
  });
});

describe("executionTokenReading", () => {
  const rec = (sec: number, action: string, payload: Record<string, unknown> = {}, handle?: string): FlowRecord =>
    ({ ts: atSec(sec), action, session_id: SID, ...(handle ? { handle } : {}), payload }) as unknown as FlowRecord;
  const hb = (sec: number, chars: number) => rec(sec, "dispatch.turn.heartbeat", { sampled_at_ms: Date.parse(atSec(sec)), generated_chars: chars, turn_seq: 1 });

  it("carries the session id, role, generating state and rate", () => {
    const records = [rec(0, "dispatch.start", {}, "darkmux/coder"), hb(0, 0), hb(2, 400)];
    const reading = executionTokenReading(records, Date.parse(atSec(2)));
    expect(reading.sessionId).toBe(SID);
    expect(reading.role).toBe("coder");
    expect(reading.state).toBe("generating");
    // 400 chars / 2s = 200 chars/s -> 50 tok/s at the default 4 chars/token.
    expect(reading.tokensPerSec).toBeCloseTo(50, 5);
    expect(reading.carried).toBe(false);
  });

  it("reports null tokensPerSec while generating with no same-turn pair yet — never 0", () => {
    // The start marker sits 5s before the heartbeat so its second-floor tie
    // rule (`deriveLiveState`'s own doc: a marker in the SAME second as the
    // last heartbeat wins) doesn't fire — this fixture is testing the fresh
    // "one heartbeat, no pair" case, not that tie.
    const records = [rec(-5, "dispatch.start", {}, "darkmux/coder"), hb(0, 40)];
    const reading = executionTokenReading(records, Date.parse(atSec(0)));
    expect(reading.state).toBe("generating");
    expect(reading.tokensPerSec).toBeNull();
  });

  it("reports the rest state with restSecondsLeft, and no rate", () => {
    const records = [
      rec(0, "dispatch.start", {}, "darkmux/coder"),
      hb(0, 0),
      hb(2, 400),
      rec(3, "dispatch.rest", { ms: 15_000 }),
    ];
    const reading = executionTokenReading(records, Date.parse(atSec(3)) + 5_000);
    expect(reading.state).toBe("rest");
    expect(reading.restSecondsLeft).toBe(10);
    expect(reading.tokensPerSec).toBeNull();
  });

  it("marks a carried reading — the previous turn's rate, into a fresh turn's lone first heartbeat", () => {
    // (rebase onto fix/2886-tokrate-checkpoints) `carriedTokenRate` now
    // skips a pair whose EARLIER sample has 0 chars, so turn 1's first
    // heartbeat starts at a nonzero count here — the 400-char delta over
    // the same 2s window is unchanged, so the expected 50 tok/s below still
    // holds.
    const records = [
      rec(0, "dispatch.start", {}, "darkmux/coder"),
      hb(0, 40),
      hb(2, 440),
      rec(20, "dispatch.turn.heartbeat", { sampled_at_ms: Date.parse(atSec(20)), generated_chars: 50, turn_seq: 2 }),
    ];
    const reading = executionTokenReading(records, Date.parse(atSec(20)));
    expect(reading.state).toBe("generating");
    expect(reading.carried).toBe(true);
    expect(reading.tokensPerSec).toBeCloseTo(50, 5);
  });

  // (#2886 pass 3 downgrade, applied per execution) A false STALL claim from
  // a lost connection must not survive at the PAGE grain either.
  it("downgrades a stalled reading to null (no signal) when disconnected, same rule as the aggregate", () => {
    const records = [rec(0, "dispatch.start", {}, "darkmux/coder"), hb(0, 0), hb(2, 400)];
    const nowMs = Date.parse(atSec(2)) + STALL_AFTER_MS + 5_000;
    const connected = executionTokenReading(records, nowMs, true);
    expect(connected.state).toBe("stalled");
    const disconnected = executionTokenReading(records, nowMs, false);
    expect(disconnected.state).toBeNull();
    expect(disconnected.restSecondsLeft).toBeUndefined();
  });
});

describe("liveStatePriority", () => {
  it("ranks generating best and stalled worst among real states", () => {
    expect(liveStatePriority("generating")).toBeLessThan(liveStatePriority("rest"));
    expect(liveStatePriority("rest")).toBeLessThan(liveStatePriority("tools"));
    expect(liveStatePriority("tools")).toBeLessThan(liveStatePriority("prompt"));
    expect(liveStatePriority("prompt")).toBeLessThan(liveStatePriority("stalled"));
  });

  it("ranks null (no signal) worse than every real state, including stalled", () => {
    expect(liveStatePriority(null)).toBeGreaterThan(liveStatePriority("stalled"));
  });
});
