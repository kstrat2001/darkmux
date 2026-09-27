import { describe, expect, it } from "vitest";
import { PEPPER_SID, pepperAt, pepperRecords } from "../testing/pepperGrinderRun";
import {
  DEFAULT_CHARS_PER_TOKEN,
  STALL_AFTER_MS,
  aggregateLiveState,
  aggregateTokenRate,
  charsPerSecond,
  currentTokenRate,
  deriveLiveState,
  restReasonLabel,
  restReasonWord,
  executionRole,
  executionTokenReading,
  promptTokensLabel,
  heartbeatSamples,
  isStalled,
  liveStatePriority,
  measuredCharsPerToken,
  averageGenerationRate,
  liveStateLabel,
  liveStateWhileConnected,
  lastHeartbeatMs,
  toolReadout,
  liveExecutions,
  reasonForLine,
} from "./tokenRate";
import type { NormRecord } from "./ingest";
import { norm } from "../testing/records";

const SID = "darkmux-coder-1790125784225";
const atSec = (sec: number) => new Date(Date.UTC(2026, 8, 23, 1, 9, 0) + sec * 1000).toISOString();

/** New-shape heartbeat: carries `sampled_at_ms` (ms) + `generated_chars`
 *  (content + reasoning), same fields dispatch_internal.rs::heartbeat_payload
 *  now forwards (#2877). */
const beat = (sampledAtMs: number, generatedChars: number, cumulativeChars = generatedChars): NormRecord =>
  norm({
    ts: new Date(sampledAtMs).toISOString(),
    action: "dispatch.turn.heartbeat",
    session_id: SID,
    payload: { sampled_at_ms: sampledAtMs, generated_chars: generatedChars, cumulative_chars: cumulativeChars },
  });

/** Old-shape heartbeat, exactly what a pre-#2877 runtime forwards: no
 *  `sampled_at_ms`, no `generated_chars` — only the whole-second flow `ts`
 *  and the answer-only `cumulative_chars`. */
const oldBeat = (sec: number, cumulativeChars: number): NormRecord =>
  norm({
    ts: atSec(sec),
    action: "dispatch.turn.heartbeat",
    session_id: SID,
    payload: { cumulative_chars: cumulativeChars },
  });

const tokensRecord = (completionTokens: number): NormRecord =>
  norm({
    ts: atSec(0),
    action: "telemetry.tokens",
    session_id: SID,
    payload: { completion_tokens: completionTokens },
  });

describe("heartbeatSamples", () => {
  it("reads new-shape sampled_at_ms + generated_chars", () => {
    const samples = heartbeatSamples([beat(1_000, 40), beat(3_000, 120)]);
    expect(samples).toEqual([
      // (#2890) `visible` (cumulative_chars) rides along when the record
      // also has generated_chars; the old-runtime case below has none.
      { atMs: 1_000, chars: 40, visible: 40 },
      { atMs: 3_000, chars: 120, visible: 120 },
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
    const weird = norm({ ts: atSec(0), action: "dispatch.turn.heartbeat", session_id: SID, payload: {} });
    const other = norm({ ts: atSec(0), action: "dispatch.turn", session_id: SID, payload: { cumulative_chars: 999 } });
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
  const turnBeat = (turn: number, ms: number, chars: number): NormRecord =>
    norm({ ...beat(ms, chars), payload: { sampled_at_ms: ms, generated_chars: chars, turn_seq: turn } });
  const turnTokens = (turn: number, completion: number): NormRecord =>
    norm({ ...tokensRecord(completion), payload: { completion_tokens: completion, turn_seq: turn } });

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
  const checkpoint = (turn: number): NormRecord =>
    norm({ ts: atSec(0), action: "dispatch.checkpoint", session_id: SID, payload: { turn_seq: turn, checkpoint: 1, verdict: "conclude" } });

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
  const continueCheckpoint = (turn: number): NormRecord =>
    norm({ ts: atSec(0), action: "dispatch.checkpoint", session_id: SID, payload: { turn_seq: turn, checkpoint: 1, verdict: "continue" } });

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
    const hbT = (atMs: number, chars: number, turnSeq: number): NormRecord =>
      norm({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } });
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
    const hbT = (atMs: number, chars: number, turnSeq: number): NormRecord =>
      norm({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } });
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
    const hbT = (atMs: number, chars: number, turnSeq: number): NormRecord =>
      norm({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } });
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
    const hbT = (atMs: number, chars: number, turnSeq: number): NormRecord =>
      norm({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } });
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
    const hbT = (atMs: number, chars: number, turnSeq: number): NormRecord =>
      norm({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } });
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
    const hbT = (atMs: number, chars: number, turnSeq: number): NormRecord =>
      norm({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } });
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
    const fresh: NormRecord[] = [beat(5_000, 0)];
    expect(aggregateTokenRate([generating, fresh], 5_500)?.tokensPerSec).toBeCloseTo(10, 5);
  });

  it("is null only when NO execution has a reading at all", () => {
    expect(aggregateTokenRate([[beat(1_000, 0)], []], 1_500)).toBeNull();
  });

  // (#2885) A carried reading from any ONE contributing execution marks the
  // whole aggregate carried — the caller renders a single number, dimmed or
  // not, never a per-execution split.
  it("marks the aggregate carried when any contributing execution's own reading is carried", () => {
    const hbT = (atMs: number, chars: number, turnSeq: number): NormRecord =>
      norm({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } });
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
const start = (atMs: number): NormRecord =>
  norm({ ts: new Date(atMs).toISOString(), action: "dispatch.start", session_id: SID, payload: {} });
const turnEnd = (atMs: number, turnSeq: number): NormRecord =>
  norm({ ts: new Date(atMs).toISOString(), action: "dispatch.turn", session_id: SID, payload: { turn_seq: turnSeq } });
const tool = (atMs: number): NormRecord =>
  norm({ ts: new Date(atMs).toISOString(), action: "dispatch.tool", session_id: SID, payload: { tool_name: "bash" } });
const rest = (atMs: number, ms: number): NormRecord =>
  norm({ ts: new Date(atMs).toISOString(), action: "dispatch.rest", session_id: SID, payload: { ms, reason: "thermal-duty-cycle" } });
const restAnnounceOnly = (atMs: number): NormRecord =>
  norm({ ts: new Date(atMs).toISOString(), action: "dispatch.rest", session_id: SID, payload: { reason: "thermal-duty-cycle", pause: false, delay_ms: 15_000 } });

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
    // (#2963) The completed call is not the one running (if any is), and
    // with no `tool_names` nothing says what is: the neutral TOOLS state.
    expect(deriveLiveState(recs, toolAt + 2_000)).toEqual({ state: "tools" });
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
    const recs: NormRecord[] = [
      beat(beatAtMs - 2_000, 4_483),
      beat(beatAtMs, 5_623),
      norm({ ts: toolTs, action: "dispatch.tool", session_id: SID, payload: { tool_name: "edit" } }),
    ];
    expect(deriveLiveState(recs, beatAtMs + STALL_AFTER_MS + 1_000)).toEqual({ state: "tools" });
  });

  // (#2902 step 5) A hosted call held by its endpoint budget: no
  // `dispatch.rest` is written, yet it reads REST "budget · <endpoint>",
  // counting to the announced resume, and ends at `budget.resume`/`stop`.
  it("reads a hosted budget wait as REST budget · <endpoint> until it resumes", () => {
    const at = (ms: number) => new Date(ms).toISOString();
    const wait = norm({ ts: at(1_000), action: "budget.wait", session_id: SID, payload: { endpoint_id: "azure", wait_seconds: 60 } });
    expect(deriveLiveState([wait], 11_000)).toEqual({
      state: "rest", restSecondsLeft: 50, restEndMs: 61_000, restReason: "budget · azure", restReasonWord: "budget",
    });
    const resumed = norm({ ts: at(20_000), action: "budget.resume", session_id: SID, payload: { endpoint_id: "azure" } });
    expect(deriveLiveState([wait, resumed], 21_000).state).toBe("prompt");
    expect(aggregateLiveState([[wait]], 11_000)?.state).toBe("rest");
    // (5th review C1) Stopped: the wait is over, and the execution is closed
    // (the call was never sent), never left reading PROMPT or live.
    const stopped = norm({ ts: at(20_000), action: "budget.stop", session_id: SID, payload: { endpoint_id: "azure" } });
    expect(deriveLiveState([wait, stopped], 21_000).state).toBe("prompt");
    expect(liveExecutions([[wait, stopped]], 21_000)).toEqual([]);
    // An agentic-remote run the pacer held, then stopped: it has other
    // evidence (its start), and the stop still closes it at once, before the
    // run's own terminal record lands.
    const start = norm({ ts: at(0), action: "dispatch.start", session_id: SID, payload: {} });
    expect(liveExecutions([[start, wait, stopped]], 21_000)).toEqual([]);
    expect(aggregateLiveState([[wait, stopped]], 21_000)).toBeNull();
  });

  // (5th review C1) A waiter that died mid-wait writes nothing more. Past its
  // resume time plus the grace it is not a live execution, and not REST.
  it("a budget wait silent past its resume time plus the grace is not live", () => {
    const at = (ms: number) => new Date(ms).toISOString();
    const wait = norm({ ts: at(1_000), action: "budget.wait", session_id: SID, payload: { endpoint_id: "azure", wait_seconds: 60 } });
    expect(liveExecutions([[wait]], 61_000 + 59_000)).toHaveLength(1);
    expect(liveExecutions([[wait]], 61_000 + 61_000)).toEqual([]);
    expect(aggregateLiveState([[wait]], 61_000 + 61_000)).toBeNull();
  });

  // (5th review C7) A day window's wait reads as a compact duration, and a
  // long endpoint id is trimmed so the line fits its slot.
  it("a long wait reads 23h 53m, then minutes, then seconds; a long endpoint id is trimmed", () => {
    const at = (ms: number) => new Date(ms).toISOString();
    const secs = 23 * 3600 + 53 * 60;
    const wait = norm({ ts: at(0), action: "budget.wait", session_id: SID, payload: { endpoint_id: "azure-openai-eastus2-prod", wait_seconds: secs } });
    const r = deriveLiveState([wait], 0);
    // (6th review) The full id where there is room (the lamp status, the
    // hover title): two endpoints sharing a prefix stay distinct. Trimmed
    // only for the one-line slots (the note line, the card's status line).
    expect(r.restReason).toBe("budget · azure-openai-eastus2-prod");
    expect(liveStateLabel(r)).toBe("rest 23h 53m · budget · azure-openai-eastus2-prod");
    expect(reasonForLine(r.restReason ?? "")).toBe("budget · azure-opena…");
    expect(reasonForLine("thermal · serious")).toBe("thermal · serious");
    expect(reasonForLine("battery")).toBe("battery");
    expect(liveStateLabel(deriveLiveState([wait], (secs - 12 * 60) * 1000))).toMatch(/^rest 12m · /);
    expect(liveStateLabel(deriveLiveState([wait], (secs - 45) * 1000))).toMatch(/^rest 45s · /);
    expect(restReasonLabel("thermal", "serious")).toBe("thermal · serious");
  });

  it("is rest with a countdown while inside a reported rest's ms window, then falls to prompt once it elapses", () => {
    const recs = [tool(0), rest(1_000, 15_000)];
    // 1s into the 15s window → 14s left (ceil).
    expect(deriveLiveState(recs, 2_000)).toEqual({ state: "rest", restSecondsLeft: 14, restEndMs: 16000, restReason: "thermal pacing", restReasonWord: "thermal pacing" });
    // Right at the boundary the window has fully elapsed.
    expect(deriveLiveState(recs, 1_000 + 15_000)).toEqual({ state: "prompt" });
    // Comfortably past it too.
    expect(deriveLiveState(recs, 20_000)).toEqual({ state: "prompt" });
  });

  it("ignores the announce-only rest record (no ms) — a pacing nudge, not a rest", () => {
    const recs = [tool(0), restAnnounceOnly(1_000)];
    expect(deriveLiveState(recs, 2_000)).toEqual({ state: "tools" });
  });

  it("is stalled once a heartbeat has gone stale with nothing after it to explain the gap", () => {
    const recs = [beat(0, 10), beat(1_000, 200)];
    expect(deriveLiveState(recs, 1_000 + STALL_AFTER_MS + 1)).toEqual({ state: "stalled" });
  });

  it("prefers a marker newer than the last stale heartbeat over calling it stalled", () => {
    const recs = [beat(0, 10), beat(1_000, 200), tool(1_000 + STALL_AFTER_MS + 500)];
    expect(deriveLiveState(recs, 1_000 + STALL_AFTER_MS + 600)).toEqual({ state: "tools" });
  });

  it("ignores records after the given clock — never reads the future", () => {
    // The rest record technically exists in the array, but its ts is after
    // `nowMs`; the state must read as if it had not happened yet.
    const recs = [tool(0), rest(5_000, 15_000)];
    expect(deriveLiveState(recs, 1_000)).toEqual({ state: "tools" });
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
    expect(aggregateLiveState([toolsOnly, resting], 2_000)).toEqual({ state: "rest", restSecondsLeft: 14, restEndMs: 16000, restReason: "thermal pacing", restReasonWord: "thermal pacing" });
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
  const turn = (sid: string, seq: number, genMs: number | undefined): NormRecord =>
    norm({ ts: atSec(seq), action: "dispatch.turn", session_id: sid, payload: genMs == null ? { turn_seq: seq } : { turn_seq: seq, generation_ms: genMs } });
  const tok = (sid: string, seq: number, completion: number): NormRecord =>
    norm({ ts: atSec(seq), action: "telemetry.tokens", session_id: sid, payload: { turn_seq: seq, completion_tokens: completion } });

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
  const checkpoint = (sid: string, seq: number): NormRecord =>
    norm({ ts: atSec(seq), action: "dispatch.checkpoint", session_id: sid, payload: { turn_seq: seq, checkpoint: 1, verdict: "conclude" } });

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
  const continueCheckpoint = (sid: string, seq: number): NormRecord =>
    norm({ ts: atSec(seq), action: "dispatch.checkpoint", session_id: sid, payload: { turn_seq: seq, checkpoint: 1, verdict: "continue" } });

  it("does NOT exclude a turn whose checkpoint verdict is 'continue' from the average", () => {
    const reading = averageGenerationRate([[turn("a", 2, 127_348), tok("a", 2, 33_803), continueCheckpoint("a", 2)]]);
    expect(reading).not.toBeNull();
    expect(reading!.billedTurns).toBe(1);
    expect(reading!.totalTurns).toBe(1);
    expect(reading!.tokensPerSec).toBeCloseTo(33_803 / (127_348 / 1000), 5);
  });
});

describe("liveStateLabel", () => {
  it("names the prompt wait as 'processing prompt', not the bare word the page's prompt disclosure also uses", () => {
    expect(liveStateLabel({ state: "prompt" } as never)).toBe("processing prompt");
  });
  // (#2950) REST says why, after the countdown, in TOOL GEN's " · " form.
  it("puts a rest's reason after its countdown, and nothing when there is none", () => {
    expect(liveStateLabel({ state: "rest", restSecondsLeft: 12, restReason: "thermal · serious" })).toBe("rest 12s · thermal · serious");
    expect(liveStateLabel({ state: "rest", restSecondsLeft: 12 })).toBe("rest 12s");
  });
});

// (#2950) Why a runtime rested, in plain words, from the rest record's own
// `reason` and `state`: every reason a producer writes today, an unknown
// one verbatim, and nothing for a record that names none.
describe("restReasonLabel", () => {
  it.each([
    ["turn_delay", undefined, "turn delay (config)"],
    ["thermal-duty-cycle", "fair", "thermal pacing · fair"],
    ["thermal-duty-cycle", undefined, "thermal pacing"],
    ["thermal", "serious", "thermal · serious"],
    ["thermal-critical", "critical", "thermal breaker · critical"],
    ["thermal-episode-limit", "serious", "thermal hold · serious"],
    ["battery", "18%", "battery · 18%"],
    ["budget", "azure", "budget · azure"],
    ["paused", undefined, "paused"],
    ["solar-flare", "x9", "solar-flare · x9"],
    ["toString", undefined, "toString"],
  ])("%s / %s -> %s", (reason, state, want) => {
    expect(restReasonLabel(reason, state)).toBe(want);
    // (#2950, phone card) The same words without the state.
    expect(restReasonWord(reason)).toBe(want.split(" · ")[0]);
  });
  it("says nothing when the record names no reason, whatever its state", () => {
    expect(restReasonLabel(undefined, undefined)).toBeNull();
    expect(restReasonLabel("", "serious")).toBeNull();
    expect(restReasonLabel(null, "serious")).toBeNull();
    expect(restReasonLabel(42, undefined)).toBeNull();
    expect(restReasonWord(undefined)).toBeNull();
    expect(restReasonWord("")).toBeNull();
  });
});

// (#2950 review, CONSIDER 1) Two executions resting at once for different
// reasons: the aggregate's reason and its countdown come from the SAME
// execution, never one from each.
describe("aggregateLiveState with two resting executions", () => {
  const restOf = (sid: string, atMs: number, payload: Record<string, unknown>): NormRecord =>
    norm({ ts: new Date(atMs).toISOString(), action: "dispatch.rest", session_id: sid, payload });
  const toolOf = (sid: string): NormRecord =>
    norm({ ts: new Date(0).toISOString(), action: "dispatch.tool", session_id: sid, payload: { tool_name: "bash" } });
  const thermal = [toolOf("a"), restOf("a", 1_000, { ms: 15_000, reason: "thermal", state: "serious" })];
  const battery = [toolOf("b"), restOf("b", 1_000, { ms: 5_000, reason: "battery", state: "18%" })];
  it("keeps the winning execution's reason with its own countdown, in either order", () => {
    expect(aggregateLiveState([thermal, battery], 2_000)).toEqual({ state: "rest", restSecondsLeft: 14, restEndMs: 16000, restReason: "thermal · serious", restReasonWord: "thermal" });
    expect(aggregateLiveState([battery, thermal], 2_000)).toEqual({ state: "rest", restSecondsLeft: 4, restEndMs: 6000, restReason: "battery · 18%", restReasonWord: "battery" });
  });
});

describe("the rest reading carries the rest record's own reason", () => {
  const restWith = (atMs: number, payload: Record<string, unknown>): NormRecord =>
    norm({ ts: new Date(atMs).toISOString(), action: "dispatch.rest", session_id: SID, payload });
  it("a thermal pause names its state", () => {
    const r = deriveLiveState([tool(0), restWith(1_000, { ms: 2_000, reason: "thermal", state: "serious", turn: 3 })], 1_500);
    expect(r).toEqual({ state: "rest", restSecondsLeft: 2, restEndMs: 3000, restReason: "thermal · serious", restReasonWord: "thermal" });
  });
  it("the latest rest wins: a turn delay after a thermal pause reads as the turn delay", () => {
    const recs = [tool(0), restWith(1_000, { ms: 2_000, reason: "thermal", state: "serious" }), restWith(3_000, { ms: 5_000, reason: "turn_delay" })];
    expect(deriveLiveState(recs, 3_500)).toEqual({ state: "rest", restSecondsLeft: 5, restEndMs: 8000, restReason: "turn delay (config)", restReasonWord: "turn delay (config)" });
  });
  it("a rest record with no reason carries none", () => {
    expect(deriveLiveState([tool(0), restWith(1_000, { ms: 15_000 })], 2_000)).toEqual({ state: "rest", restSecondsLeft: 14, restEndMs: 16000 });
  });
  it("a pacing announcement (no ms) never lends its reason to the rest", () => {
    const recs = [tool(0), restWith(1_000, { ms: 15_000, reason: "turn_delay" }), restWith(1_500, { reason: "thermal-duty-cycle", state: "fair", pause: false, delay_ms: 15_000 })];
    expect(deriveLiveState(recs, 2_000)).toEqual({ state: "rest", restSecondsLeft: 14, restEndMs: 16000, restReason: "turn delay (config)", restReasonWord: "turn delay (config)" });
  });
  it("the per-execution reading passes it on, and only while resting", () => {
    const recs = [tool(0), restWith(1_000, { ms: 15_000, reason: "battery", state: "18%" })];
    expect(executionTokenReading(recs, 2_000)).toMatchObject({ restReason: "battery · 18%", restReasonWord: "battery", restEndMs: 16_000 });
    expect(executionTokenReading(recs, 20_000).restReason).toBeUndefined();
    // (#2961) The end time too, only while resting.
    expect(executionTokenReading(recs, 20_000).restEndMs).toBeUndefined();
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
  const rec = (sid: string, atMs: number, action: string, payload: Record<string, unknown> = {}, source?: string): NormRecord =>
    norm({ ts: new Date(atMs).toISOString(), action, session_id: sid, ...(source ? { source } : {}), payload });
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
    const lifecycle = [rec("mission-m", 0, "mission.start"), rec("mission-m", 0, "phase.start")];
    const task = [rec("task-1-m", 500, "step.start"), rec("task-1-m", 900, "step.timing")];
    const stalled = [rec("b", 0, "dispatch.start"), hb("b", 1_000, 0), hb("b", 3_000, 800)];
    expect(aggregateLiveState([lifecycle, task, stalled], 3_000 + 60_000)?.state).toBe("stalled");
  });

  it("is null, not PROMPT, when no live execution exists (a mission between model steps)", () => {
    const runGrain = [rec("m", 0, "dispatch.start", {}, "mission")];
    const finished = [rec("a", 0, "dispatch.start"), hb("a", 1_000, 0), rec("a", 2_000, "dispatch.complete")];
    const lifecycle = [rec("mission-m", 0, "mission.start")];
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
  const turn = (atMs: number, seq: number, calls: number): NormRecord =>
    norm({ ts: new Date(atMs).toISOString(), action: "dispatch.turn", session_id: SID, payload: { turn_seq: seq, tool_calls_count: calls } });
  // (#2963) A 1.64.0 turn record: each call's name and path, the paths key
  // left out when no call has one (as the host writes it).
  const turnWithCalls = (atMs: number, seq: number, calls: [string, string | null][]): NormRecord =>
    norm({
      ts: new Date(atMs).toISOString(),
      action: "dispatch.turn",
      session_id: SID,
      payload: {
        turn_seq: seq,
        tool_calls_count: calls.length,
        tool_names: calls.map(([n]) => n),
        ...(calls.some(([, p]) => p !== null) ? { tool_paths: calls.map(([, p]) => p) } : {}),
      },
    });

  it("is TOOLS between the turn end and the last of its tool completions", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turn(4_000, 1, 2), tool(5_000)];
    expect(deriveLiveState(recs, 6_000).state).toBe("tools");
  });

  it("is PROMPT once every tool the turn called has completed", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turn(4_000, 1, 2), tool(5_000), tool(6_000)];
    expect(deriveLiveState(recs, 7_000).state).toBe("prompt");
  });
  // (#2890) The TOOLS center shows an icon for the tool. (#2963) It is the
  // RUNNING call's tool: `tool_names[k]` from the turn record. Without that
  // list, a completed call's name is never shown (it is not the one
  // running), and a previous turn's tool must never leak in.
  const namedTool = (atMs: number, name: string): NormRecord =>
    norm({ ts: new Date(atMs).toISOString(), action: "dispatch.tool", session_id: SID, payload: { tool_name: name } });

  it("(#2963) with no tool_names, a completed call's name is never the running call's", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turn(4_000, 1, 3), namedTool(5_000, "read"), namedTool(6_000, "edit")];
    expect(deriveLiveState(recs, 7_000)).toEqual({ state: "tools" });
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
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turnWithCalls(4_000, 1, [["read", null], ["edit", null], ["edit", null]]), namedTool(5_000, "read"), namedTool(9_000, "edit")];
    expect(deriveLiveState(recs, 6_000)).toEqual({ state: "tools", toolName: "edit" });
  });

  // (#2963) The run page's readout and the TOOLS icon name the call RUNNING
  // NOW. `dispatch.turn` carries `tool_names` and `tool_paths` (FLOW
  // 1.64.0), one entry per call of the turn in the model's order; while the
  // turn's k-th call runs (k = calls completed so far in this turn) the
  // tool is `tool_names[k]` and the file `tool_paths[k]`. A completed call's
  // own name or file never stands in for a later call's.
  const pathTool = (atMs: number, name: string, path: string): NormRecord =>
    norm({ ts: new Date(atMs).toISOString(), action: "dispatch.tool", session_id: SID, payload: { tool_name: name, args: JSON.stringify({ path, content: "x" }) } });
  const writingBeat = (atMs: number, name: string): NormRecord =>
    norm({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: 900, turn_seq: 2, phase: "writing_tool_call", tool_name: name } });
  const turnPathsOnly = (atMs: number, seq: number, paths: (string | null)[]): NormRecord =>
    norm({ ts: new Date(atMs).toISOString(), action: "dispatch.turn", session_id: SID, payload: { turn_seq: seq, tool_calls_count: paths.length, tool_paths: paths } });

  it("(#2963) a turn that writes a.ts then reads b.ts: `read · b.ts` while b.ts is read", () => {
    const base = [start(0), beat(1_000, 0), beat(3_000, 800), turnWithCalls(4_000, 1, [["write", "/workspace/src/a.ts"], ["read", "/workspace/src/b.ts"]])];
    expect(deriveLiveState(base, 4_500)).toEqual({ state: "tools", toolName: "write", toolPath: "src/a.ts" });
    const after = [...base, pathTool(5_000, "write", "/workspace/src/a.ts")];
    expect(deriveLiveState(after, 5_500)).toEqual({ state: "tools", toolName: "read", toolPath: "src/b.ts" });
  });

  // (#2963 review, MUST FIX 1) The lists hold only the calls that RUN: a
  // call the runtime refused (ungranted, not a tool, cut off) is not in
  // them, while `tool_calls_count` still counts it. The list's length is
  // how many will complete.
  const turnRunning = (atMs: number, seq: number, count: number, calls: [string, string | null][]): NormRecord =>
    norm({
      ts: new Date(atMs).toISOString(),
      action: "dispatch.turn",
      session_id: SID,
      payload: { turn_seq: seq, tool_calls_count: count, tool_names: calls.map(([n]) => n), tool_paths: calls.map(([, p]) => p) },
    });

  it("(#2963) after an ungranted call, the readout shows the running `read · y.rs`", () => {
    // The model asked for [write x.rs (ungranted), read y.rs]; only the read runs.
    const base = [start(0), beat(1_000, 0), beat(3_000, 800), turnRunning(4_000, 1, 2, [["read", "/workspace/src/y.rs"]])];
    expect(deriveLiveState(base, 4_500)).toEqual({ state: "tools", toolName: "read", toolPath: "src/y.rs" });
  });

  it("(#2963) once every call that runs has completed, it is PROMPT, whatever the raw count says", () => {
    // [read a, write b (ungranted)]: the read completes and nothing else will.
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turnRunning(4_000, 1, 2, [["read", "src/a.ts"]]), pathTool(5_000, "read", "src/a.ts")];
    expect(deriveLiveState(recs, 6_000).state).toBe("prompt");
  });

  it("(#2963) a turn none of whose calls run is PROMPT at once", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turnRunning(4_000, 1, 1, [])];
    expect(deriveLiveState(recs, 4_500).state).toBe("prompt");
  });

  it("(#2963 review, CONSIDER 4) a stray `path` on a tool that takes none does not put the lists out of step", () => {
    const bash = norm({ ts: new Date(5_000).toISOString(), action: "dispatch.tool", session_id: SID, payload: { tool_name: "bash", args: JSON.stringify({ command: "ls", path: "elsewhere" }) } });
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turnRunning(4_000, 1, 2, [["bash", null], ["read", "src/b.ts"]]), bash];
    expect(deriveLiveState(recs, 6_000)).toEqual({ state: "tools", toolName: "read", toolPath: "src/b.ts" });
  });

  it("(#2963 review, CONSIDER 4) a write whose path was cut off matches the listed path through its result, `./` or not", () => {
    // The capped args lost the path; the viewer falls back to the result's
    // resolved path, which must equal the listed `./src/a.ts`.
    const args = JSON.stringify({ content: "x".repeat(600), path: "./src/a.ts" }).slice(0, 512);
    const write = norm({ ts: new Date(5_000).toISOString(), action: "dispatch.tool", session_id: SID, payload: { tool_name: "write", args, result: "Wrote 600 bytes to /workspace/src/a.ts" } });
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turnRunning(4_000, 1, 2, [["write", "./src/a.ts"], ["read", "./src/b.ts"]]), write];
    expect(deriveLiveState(recs, 6_000)).toEqual({ state: "tools", toolName: "read", toolPath: "src/b.ts" });
  });

  it("(#2963 review, CONSIDER 5) with no lists, the written tool's name goes once its call completes", () => {
    // An older record: no count and no lists. The heartbeat named the call
    // being written; once a completion arrives, that call is done, and the
    // record set says nothing about what (if anything) runs next.
    const recs = [start(0), beat(1_000, 0), writingBeat(2_000, "write"), turnEnd(3_000, 1), namedTool(4_000, "write")];
    expect(deriveLiveState(recs, 4_500)).toEqual({ state: "tools" });
  });

  it("(#2963) never shows a previous call's word or file while a later call runs (no lists)", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turn(4_000, 1, 2), pathTool(5_000, "read", "/workspace/src/a.ts")];
    expect(deriveLiveState(recs, 6_000)).toEqual({ state: "tools" });
  });

  it("(#2963) names with no paths (calls that take none) still name the running call", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turnWithCalls(4_000, 1, [["bash", null], ["read", null]]), namedTool(5_000, "bash")];
    expect(deriveLiveState(recs, 6_000)).toEqual({ state: "tools", toolName: "read" });
  });

  it("(#2963) paths with no names (a partial record): the file, with no word", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turnPathsOnly(4_000, 1, ["src/a.ts", "src/b.ts"]), pathTool(5_000, "read", "src/a.ts")];
    expect(deriveLiveState(recs, 6_000)).toEqual({ state: "tools", toolPath: "src/b.ts" });
  });

  it("(#2963) the running call's tool and file advance with each completion", () => {
    const base = [start(0), beat(1_000, 0), beat(3_000, 800), turnWithCalls(4_000, 1, [["read", "/workspace/src/a.ts"], ["edit", "/workspace/src/b.ts"], ["write", "src/c.ts"]])];
    const one = [...base, pathTool(5_000, "read", "/workspace/src/a.ts")];
    expect(deriveLiveState(one, 5_500)).toEqual({ state: "tools", toolName: "edit", toolPath: "src/b.ts" });
    const two = [...one, pathTool(6_000, "edit", "/workspace/src/b.ts")];
    expect(deriveLiveState(two, 6_500)).toEqual({ state: "tools", toolName: "write", toolPath: "src/c.ts" });
  });

  it("(#2963) a call with no path in the list has no file, and keeps its word", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turnWithCalls(4_000, 1, [["read", "src/a.ts"], ["write", null]]), pathTool(5_000, "read", "src/a.ts")];
    expect(deriveLiveState(recs, 6_000)).toEqual({ state: "tools", toolName: "write" });
  });

  it("(#2963) a completion whose FILE disagrees with the list stops both lists for the rest of the turn", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turnWithCalls(4_000, 1, [["read", "src/a.ts"], ["read", "src/b.ts"], ["read", "src/c.ts"]]), pathTool(5_000, "read", "src/b.ts")];
    expect(deriveLiveState(recs, 6_000)).toEqual({ state: "tools" });
    const later = [...recs, pathTool(6_000, "read", "src/b.ts")];
    expect(deriveLiveState(later, 6_500)).toEqual({ state: "tools" });
  });

  it("(#2963) a completion whose NAME disagrees with the list stops both lists for the rest of the turn", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turnWithCalls(4_000, 1, [["write", null], ["read", "src/b.ts"]]), namedTool(5_000, "bash")];
    expect(deriveLiveState(recs, 6_000)).toEqual({ state: "tools" });
  });

  it("(#2963) a turn out of step does not stop the next turn's lists", () => {
    const recs = [
      start(0), beat(1_000, 0), beat(3_000, 800),
      turnWithCalls(4_000, 1, [["read", "src/a.ts"]]), pathTool(5_000, "read", "src/other.ts"),
      beat(6_000, 0), beat(8_000, 900),
      turnWithCalls(9_000, 2, [["read", "src/next.ts"], ["edit", "src/last.ts"]]), pathTool(10_000, "read", "src/next.ts"),
    ];
    expect(deriveLiveState(recs, 11_000)).toEqual({ state: "tools", toolName: "edit", toolPath: "src/last.ts" });
  });

  it("(#2963) a paths list shorter than the names: no file past its end", () => {
    const short = norm({ ts: new Date(4_000).toISOString(), action: "dispatch.turn", session_id: SID, payload: { turn_seq: 1, tool_calls_count: 3, tool_paths: ["src/a.ts"], tool_names: ["read", "read", "read"] } });
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), short, pathTool(5_000, "read", "src/a.ts")];
    expect(deriveLiveState(recs, 6_000)).toEqual({ state: "tools", toolName: "read" });
  });

  it("(#2963) never carries the previous turn's lists into this one", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turnWithCalls(4_000, 1, [["read", "src/a.ts"]]), pathTool(5_000, "read", "src/a.ts"), beat(6_000, 0), beat(8_000, 900), turn(9_000, 2, 2)];
    expect(deriveLiveState(recs, 10_000)).toEqual({ state: "tools" });
  });

  it("(#2963) no lists, one call: the writing heartbeat's name is that call's", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turn(4_000, 1, 1), pathTool(5_000, "read", "src/a.ts"), beat(6_000, 0), writingBeat(8_000, "write"), turn(9_000, 2, 1)];
    expect(deriveLiveState(recs, 10_000)).toEqual({ state: "tools", toolName: "write" });
  });

  it("(#2963) no lists, several calls: the writing heartbeat names the LAST call written, not the first to run", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), writingBeat(3_500, "read"), turn(4_000, 1, 2)];
    expect(deriveLiveState(recs, 4_500)).toEqual({ state: "tools" });
  });

  it("(#2963) the list wins over the writing heartbeat, even for a turn of one call", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), writingBeat(3_500, "read"), turnWithCalls(4_000, 1, [["write", "src/new.ts"]])];
    expect(deriveLiveState(recs, 4_500)).toEqual({ state: "tools", toolName: "write", toolPath: "src/new.ts" });
  });

  it("(#2963) the list wins over the writing heartbeat", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), writingBeat(3_500, "read"), turnWithCalls(4_000, 1, [["write", "src/new.ts"], ["read", "src/r.ts"]])];
    expect(deriveLiveState(recs, 4_500)).toEqual({ state: "tools", toolName: "write", toolPath: "src/new.ts" });
  });

  it("(#2963) escapes control characters in a listed path", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turnWithCalls(4_000, 1, [["write", "src/a\u202Eb.ts"]])];
    expect(deriveLiveState(recs, 5_000)).toEqual({ state: "tools", toolName: "write", toolPath: "src/a⟨U+202E⟩b.ts" });
  });

  it("(#2963) a completion after the playback cut does not advance the index", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turnWithCalls(4_000, 1, [["read", "src/a.ts"], ["edit", "src/b.ts"]]), pathTool(9_000, "read", "src/a.ts")];
    expect(deriveLiveState(recs, 6_000)).toEqual({ state: "tools", toolName: "read", toolPath: "src/a.ts" });
  });

  it("carries the name through executionTokenReading and aggregateLiveState", () => {
    const recs = [start(0), beat(1_000, 0), beat(3_000, 800), turnWithCalls(4_000, 1, [["read", null], ["search", "src"], ["bash", null]]), namedTool(5_000, "read")];
    expect(executionTokenReading(recs, 6_000).toolName).toBe("search");
    expect(aggregateLiveState([recs], 6_000)).toEqual({ state: "tools", toolName: "search", toolPath: "src" });
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
    const hbT = (atMs: number, chars: number, turnSeq: number): NormRecord =>
      norm({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: turnSeq } });
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
  const rec = (action: string, handle?: string): NormRecord =>
    norm({ ts: atSec(0), action, session_id: SID, ...(handle ? { handle } : {}), payload: {} });

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
    const early = norm({ ts: atSec(0), action: "dispatch.start", session_id: SID, handle: "darkmux/coder", payload: {} });
    const later = norm({ ts: atSec(10), action: "dispatch.start", session_id: SID, handle: "darkmux/reviewer", payload: {} });
    expect(executionRole([early, later])).toBe("reviewer");
  });
});

describe("executionTokenReading", () => {
  const rec = (sec: number, action: string, payload: Record<string, unknown> = {}, handle?: string): NormRecord =>
    norm({ ts: atSec(sec), action, session_id: SID, ...(handle ? { handle } : {}), payload });
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

// (#2889) The model writing a tool call. LM Studio names the call at once,
// then generates its arguments without sending anything; the runtime ticks
// through that silence and the host forwards each tick as a heartbeat with
// `phase: "writing_tool_call"`, `tool_name`, and `generated_chars` UNCHANGED.
describe("(#2889) writing a tool call", () => {
  const rec = (sec: number, action: string, payload: Record<string, unknown> = {}): NormRecord =>
    norm({ ts: atSec(sec), action, session_id: SID, payload });
  const hb = (sec: number, chars: number, extra: Record<string, unknown> = {}) =>
    rec(sec, "dispatch.turn.heartbeat", { sampled_at_ms: Date.parse(atSec(sec)), generated_chars: chars, turn_seq: 1, ...extra });
  const writing = (sec: number, chars: number, tool = "write") => hb(sec, chars, { phase: "writing_tool_call", tool_name: tool });

  /** The probe's shape, stretched: 2s of reasoning, then the name at 4s,
   *  then 36s of ticks at the same count, well past `STALL_AFTER_MS`. */
  const probe = (): NormRecord[] => {
    const out = [rec(-5, "dispatch.start"), hb(0, 0, { prompt_chars: 144_000 }), hb(1, 400), hb(2, 800), writing(4, 812)];
    for (let s = 6; s <= 40; s += 2) out.push(writing(s, 812));
    return out;
  };

  it("derives TOOLS, writing, with the tool name — never STALL, PROMPT or GEN — through a gap longer than the stall threshold", () => {
    const nowMs = Date.parse(atSec(41));
    expect(nowMs - Date.parse(atSec(4))).toBeGreaterThan(STALL_AFTER_MS);
    expect(deriveLiveState(probe(), nowMs)).toEqual({ state: "tools", toolName: "write", writing: true, writingSeconds: 37 });
  });

  it("the writing stretch counts from the first writing heartbeat of the current run of them", () => {
    expect(deriveLiveState(probe(), Date.parse(atSec(10)))).toMatchObject({ state: "tools", writing: true, writingSeconds: 6 });
  });

  it("a writing heartbeat gone stale is still a stall — the tick stopped, so nothing is writing", () => {
    expect(deriveLiveState(probe(), Date.parse(atSec(40)) + STALL_AFTER_MS + 1).state).toBe("stalled");
  });

  it("once the turn ends, the TOOLS reading names the written tool until a completion names one", () => {
    const records = [...probe(), writing(41, 4_267), rec(42, "dispatch.turn", { turn_seq: 1, tool_calls_count: 1 })];
    expect(deriveLiveState(records, Date.parse(atSec(43)))).toEqual({ state: "tools", toolName: "write" });
  });

  it("a writing sample never pairs into a rate: no zero dragging the live rate, no spike when the arguments land", () => {
    // After the arguments land (812 -> 4,267 chars in 1s), the reading must
    // be the last GENUINE pair (1s -> 2s: 400 chars/s = 100 tok/s), carried.
    const records = [...probe(), writing(41, 4_267)];
    const reading = currentTokenRate(records);
    expect(reading).not.toBeNull();
    expect(reading!.tokensPerSec).toBeCloseTo(100, 5);
    expect(reading!.carried).toBe(true);
  });

  it("writing ticks do not move the chars-per-token calibration", () => {
    const tokens = rec(43, "telemetry.tokens", { turn_seq: 1, completion_tokens: 1_000 });
    const withTicks = [...probe(), writing(41, 4_267), tokens];
    const withoutTicks = [hb(0, 0), hb(2, 800), writing(41, 4_267), tokens];
    expect(measuredCharsPerToken(withTicks)).toBeCloseTo(measuredCharsPerToken(withoutTicks), 10);
  });

  it("the stretch never reaches back into an earlier turn's writing (an older host sends no opener between them)", () => {
    const t1 = (sec: number) => rec(sec, "dispatch.turn.heartbeat", { sampled_at_ms: Date.parse(atSec(sec)), generated_chars: 500, turn_seq: 1, phase: "writing_tool_call", tool_name: "read" });
    const t2 = (sec: number) => rec(sec, "dispatch.turn.heartbeat", { sampled_at_ms: Date.parse(atSec(sec)), generated_chars: 90, turn_seq: 2, phase: "writing_tool_call", tool_name: "edit" });
    const records = [rec(-5, "dispatch.start"), t1(0), t1(2), t2(20), t2(22)];
    expect(deriveLiveState(records, Date.parse(atSec(25)))).toMatchObject({ state: "tools", toolName: "edit", writingSeconds: 5 });
  });

  it("labels the writing stretch with its elapsed seconds; running a tool keeps the old word", () => {
    // (#2926) With the tool being written, and just "tool gen" when the
    // heartbeat names none; the seconds stay the honest progress signal.
    expect(liveStateLabel({ state: "tools", toolName: "edit", writing: true, writingSeconds: 70 })).toBe("tool gen · edit · 70s");
    expect(liveStateLabel({ state: "tools", writing: true, writingSeconds: 3 })).toBe("tool gen · 3s");
    expect(liveStateLabel({ state: "tools", toolName: "edit" })).toBe("tools");
  });

  it("executionTokenReading carries the writing flag and seconds for the fleet card", () => {
    const reading = executionTokenReading(probe(), Date.parse(atSec(41)));
    expect(reading).toMatchObject({ state: "tools", toolName: "write", writing: true, writingSeconds: 37, tokensPerSec: null });
  });
});

describe("(#2889) the prompt size on the opening heartbeat", () => {
  const rec = (sec: number, action: string, payload: Record<string, unknown> = {}): NormRecord =>
    norm({ ts: atSec(sec), action, session_id: SID, payload });
  const hb = (sec: number, chars: number, extra: Record<string, unknown> = {}) =>
    rec(sec, "dispatch.turn.heartbeat", { sampled_at_ms: Date.parse(atSec(sec)), generated_chars: chars, turn_seq: 2, ...extra });

  it("a fresh opener carrying prompt_chars reads PROMPT with that size", () => {
    const records = [rec(-5, "dispatch.start"), hb(0, 0, { prompt_chars: 144_000 })];
    expect(deriveLiveState(records, Date.parse(atSec(3)))).toEqual({ state: "prompt", promptChars: 144_000 });
  });

  it("an opener from an older host (no prompt_chars) reads PROMPT with no size", () => {
    const records = [rec(-5, "dispatch.start"), hb(0, 0)];
    expect(deriveLiveState(records, Date.parse(atSec(3)))).toEqual({ state: "prompt" });
  });

  it("(#2890) executionTokenReading estimates the prompt's size with the execution's own calibration", () => {
    const tokens = rec(-8, "telemetry.tokens", { turn_seq: 1, completion_tokens: 1_000 });
    const turn1 = [hb(-12, 0), hb(-10, 3_000)].map((r) => norm({ ...r, payload: { ...(r as unknown as { payload: object }).payload, turn_seq: 1 } }));
    const records = [rec(-15, "dispatch.start"), ...turn1, tokens, hb(0, 0, { prompt_chars: 144_000 })];
    // 3,000 chars over 1,000 billed tokens = 3 chars/token -> 48k, not the default 4's 36k.
    expect(executionTokenReading(records, Date.parse(atSec(3))).promptLabel).toBe("~48k");
    expect(executionTokenReading([rec(-5, "dispatch.start"), hb(0, 0)], Date.parse(atSec(3))).promptLabel).toBeUndefined();
  });

  it("(#2890) the size converts to an estimated token count", () => {
    expect(promptTokensLabel(144_000, 4)).toBe("~36k");
    expect(promptTokensLabel(144_000, 3)).toBe("~48k");
    expect(promptTokensLabel(3_200, 4)).toBe("~800");
    expect(promptTokensLabel(0, 4)).toBeNull();
  });

  it("(#2919) each arm of the label hands over where the arm below would round to its unit", () => {
    // charsPerToken = 1, so the char count IS the token estimate.
    expect(promptTokensLabel(999.49, 1)).toBe("~999");
    expect(promptTokensLabel(999.5, 1)).toBe("~1k"); // never "~1000"
    expect(promptTokensLabel(1_000, 1)).toBe("~1k");
    expect(promptTokensLabel(999_499, 1)).toBe("~999k");
    expect(promptTokensLabel(999_500, 1)).toBe("~1.0M"); // never "~1000k"
    expect(promptTokensLabel(1_000_000, 1)).toBe("~1.0M");
    expect(promptTokensLabel(1_234_567, 1)).toBe("~1.2M");
  });

  // (#2889 review, M3) The wire as it really lands: record `ts` is
  // WHOLE-SECOND, and the heartbeat's own `sampled_at_ms` is ms-precise. In
  // 9 of 12 real openers the marker (`dispatch.start`, or the turn's last
  // `dispatch.tool`) shares a second with the 0-char opener, so the marker
  // wins the tie. The size must still ride along for the whole PROMPT phase.
  const wholeSec = (ms: number) => new Date(Math.floor(ms / 1000) * 1000).toISOString().replace(".000Z", "Z");
  const base = Date.parse(atSec(0));
  const wire = (atMs: number, action: string, payload: Record<string, unknown> = {}): NormRecord =>
    norm({ ts: wholeSec(atMs), action, session_id: SID, payload });

  it("a dispatch.start in the same second as the opener still reads PROMPT with the opener's size", () => {
    const records = [
      wire(base + 100, "dispatch.start"),
      wire(base + 300, "dispatch.turn.heartbeat", { sampled_at_ms: base + 300, generated_chars: 0, turn_seq: 1, prompt_chars: 144_000 }),
    ];
    expect(deriveLiveState(records, base + 1_500)).toEqual({ state: "prompt", promptChars: 144_000 });
    // ...and for the whole prompt phase, past the stall threshold.
    expect(deriveLiveState(records, base + 40_000)).toEqual({ state: "prompt", promptChars: 144_000 });
  });

  it("the turn's last dispatch.tool in the same second as the next opener still carries the size", () => {
    const records = [
      wire(base, "dispatch.start"),
      wire(base + 200, "dispatch.turn.heartbeat", { sampled_at_ms: base + 200, generated_chars: 0, turn_seq: 1, prompt_chars: 90_000 }),
      wire(base + 2_000, "dispatch.turn.heartbeat", { sampled_at_ms: base + 2_000, generated_chars: 400, turn_seq: 1 }),
      wire(base + 3_000, "dispatch.turn", { turn_seq: 1, tool_calls_count: 1 }),
      wire(base + 6_100, "dispatch.tool", { tool_name: "read" }),
      wire(base + 6_400, "dispatch.turn.heartbeat", { sampled_at_ms: base + 6_400, generated_chars: 0, turn_seq: 2, prompt_chars: 96_000 }),
    ];
    expect(deriveLiveState(records, base + 7_000)).toEqual({ state: "prompt", promptChars: 96_000 });
  });

  it("an opener from BEFORE the winning marker's second never lends its size to a later PROMPT", () => {
    // Turn 1's opener at :00.3, then its turn record at :03 says no tools:
    // PROMPT, but the size belonged to a request already answered.
    const records = [
      wire(base + 100, "dispatch.start"),
      wire(base + 300, "dispatch.turn.heartbeat", { sampled_at_ms: base + 300, generated_chars: 0, turn_seq: 1, prompt_chars: 144_000 }),
      wire(base + 3_000, "dispatch.turn", { turn_seq: 1, tool_calls_count: 0 }),
    ];
    expect(deriveLiveState(records, base + 3_500)).toEqual({ state: "prompt" });
  });

  it("an elapsed rest in the same second as the next opener reads PROMPT with its size", () => {
    const records = [
      wire(base + 100, "dispatch.rest", { ms: 400 }),
      wire(base + 700, "dispatch.turn.heartbeat", { sampled_at_ms: base + 700, generated_chars: 0, turn_seq: 2, prompt_chars: 60_000 }),
    ];
    expect(deriveLiveState(records, base + 1_500)).toEqual({ state: "prompt", promptChars: 60_000 });
  });

});

describe("(#2890) thinking vs visible text while generating", () => {
  it("only reasoning grew since the last sample: generating, thinking", () => {
    const recs = [beat(1_000, 400, 0), beat(3_000, 1_200, 0)];
    expect(deriveLiveState(recs, 3_500)).toEqual({ state: "generating", thinking: true });
  });
  it("visible text grew: generating, not thinking", () => {
    const recs = [beat(1_000, 1_200, 0), beat(3_000, 1_900, 700)];
    expect(deriveLiveState(recs, 3_500)).toEqual({ state: "generating" });
  });
  it("a turn's first chunk with no visible text yet reads as thinking", () => {
    expect(deriveLiveState([beat(1_000, 300, 0)], 1_500)).toEqual({ state: "generating", thinking: true });
  });
  it("an old runtime (no generated_chars) never claims thinking", () => {
    const recs = [oldBeat(1, 100), oldBeat(3, 500)];
    expect(deriveLiveState(recs, Date.parse(atSec(3)) + 500)).toEqual({ state: "generating" });
  });
});

describe("(#2890) executionTokenReading carries thinking", () => {
  it("present only while generating and thinking", () => {
    const thinking = executionTokenReading([beat(1_000, 400, 0), beat(3_000, 1_200, 0)], 3_500, true);
    expect(thinking.state).toBe("generating");
    expect(thinking.thinking).toBe(true);
    const text = executionTokenReading([beat(1_000, 400, 0), beat(3_000, 1_200, 800)], 3_500, true);
    expect(text.thinking).toBeUndefined();
  });
});


// (#2926) Every turn's stream opens with a tiny chunk: `generated_chars` 0 ->
// 2..3 about 0.5-3 s after the turn's first heartbeat. Measured across every
// heartbeat day on the operator's machine (2026-09-12..26, 173 opener pairs):
// the second sample of an opener held 1-5 chars 145 times and at least 235
// chars the other 28 times, nothing in between. The fast ones landed inside
// the 2.5 s opener trust window, so 3 chars over 577 ms read as ~1 tok/s
// under a lit THINK lamp until the next heartbeat.
describe("(#2926) the stream-open chunk is not a rate", () => {
  const cut = (records: NormRecord[], nowMs: number) => records.filter((r) => Date.parse(r.ts) <= nowMs);

  it("turn 7's opener (0 -> 2 chars in 970 ms) carries the last real rate instead of reading ~1 tok/s", () => {
    // Turn 7's 2-char sample (ts 10:51:32) is the latest; the next lands at :34.
    const now = pepperAt("10:51:33");
    const records = cut(pepperRecords(), now);
    expect(deriveLiveState(records, now)).toEqual({ state: "generating", thinking: true });
    const reading = currentTokenRate(records);
    expect(reading).not.toBeNull();
    expect(reading!.carried).toBe(true);
    // Turn 6's last pair (764 -> 1,530 chars over 2.3 s), not an opener.
    expect(reading!.tokensPerSec).toBeGreaterThan(50);
  });

  it("the same opener as a session's FIRST turn has nothing to carry: no figure at all", () => {
    const now = pepperAt("10:51:33");
    const records = cut(pepperRecords({ minTurn: 7 }), now);
    expect(currentTokenRate(records)).toBeNull();
    expect(executionTokenReading(records, now)).toMatchObject({ state: "generating", thinking: true, tokensPerSec: null });
  });

  it("the first pair past the opener measures real reasoning, fresh (turn 2: 3 -> 887 chars, ~100 tok/s)", () => {
    const now = pepperAt("10:51:13");
    const records = cut(pepperRecords(), now);
    const reading = currentTokenRate(records);
    expect(reading!.carried).toBeUndefined();
    // (887 - 3) chars over 1,908 ms at the session's measured ~3.8 chars/token.
    expect(reading!.tokensPerSec).toBeGreaterThan(90);
    expect(reading!.tokensPerSec).toBeLessThan(140);
  });

  it("no moment of the real run reads a stream-open figure: a generating reading is the real rate or none", () => {
    // Just after every heartbeat of the two minutes (their own `ts` is whole
    // seconds, so step the page clock to the next second).
    const all = pepperRecords();
    for (const b of heartbeatSamples(all)) {
      const nowMs = Math.ceil(b.atMs / 1000) * 1000 + 500;
      const reading = executionTokenReading(cut(all, nowMs), nowMs);
      if (reading.state === "generating" && reading.tokensPerSec != null) {
        expect(reading.tokensPerSec, `at ${new Date(nowMs).toISOString()}`).toBeGreaterThan(20);
      }
    }
  });

  it("a fast opener with real output is still trusted (the #2886 fast-opener rule stands)", () => {
    const hbT = (atMs: number, chars: number): NormRecord =>
      norm({ ts: new Date(atMs).toISOString(), action: "dispatch.turn.heartbeat", session_id: PEPPER_SID, payload: { sampled_at_ms: atMs, generated_chars: chars, turn_seq: 1 } });
    // 17 chars is the smallest opener chunk that counts; 16 is still stream-open.
    expect(currentTokenRate([hbT(0, 0), hbT(1_000, 17)])).not.toBeNull();
    expect(currentTokenRate([hbT(0, 0), hbT(1_000, 16)])).toBeNull();
  });
});

// (#2915) While the work model's execution is compacting, the PROMPT lamp
// stays lit (operator, 2026-09-26) and the reading says "compacting", counting
// like REST; the compaction's own usage record (or the execution moving on)
// ends it; a compaction that never ends reads STALL after its own bound.
describe("(#2915) compacting", () => {
  const compactStart = (atMs: number, stallAfterSeconds?: number): NormRecord =>
    norm({
      ts: new Date(atMs).toISOString(),
      action: "utility.start",
      category: "telemetry",
      source: "utility",
      session_id: SID,
      handle: "compactor",
      payload: { job: "compaction", model: "u4b", serves: SID, ...(stallAfterSeconds != null ? { stall_after_seconds: stallAfterSeconds } : {}) },
    });
  const compactUsage = (atMs: number, job: string | null = "compaction"): NormRecord =>
    norm({
      ts: new Date(atMs).toISOString(),
      action: "telemetry.tokens",
      category: "telemetry",
      source: "tokens",
      session_id: SID,
      handle: "compactor",
      payload: { call_kind: "compaction", purpose: "utility", ...(job ? { job } : {}), total_tokens: 100 },
    });
  const routingStart = (atMs: number): NormRecord =>
    norm({ ...compactStart(atMs), session_id: undefined, payload: { job: "radio_routing", model: "u4b", stall_after_seconds: 30 } });

  const toolAt = 1_000 + STALL_AFTER_MS + 200;
  const before = [beat(0, 10), beat(1_000, 200), turnEnd(toolAt, 1), tool(toolAt)];

  it("reads PROMPT with compacting and its seconds, from the start", () => {
    const at = toolAt + 1_000;
    expect(deriveLiveState([...before, compactStart(at, 600)], at + 12_400)).toEqual({ state: "prompt", compacting: true, compactingSeconds: 12 });
  });

  it("the compaction's usage record ends it: back to plain PROMPT", () => {
    const at = toolAt + 1_000;
    expect(deriveLiveState([...before, compactStart(at, 600), compactUsage(at + 9_000)], at + 12_000)).toEqual({ state: "prompt" });
  });

  it("a legacy compaction usage record (no job) also ends it", () => {
    const at = toolAt + 1_000;
    expect(deriveLiveState([...before, compactStart(at, 600), compactUsage(at + 9_000, null)], at + 12_000)).toEqual({ state: "prompt" });
  });

  it("the next turn's heartbeat ends it (a compaction whose calls all failed leaves no usage record)", () => {
    const at = toolAt + 1_000;
    const recs = [...before, compactStart(at, 600), beat(at + 3_000, 0), beat(at + 5_000, 40)];
    expect(deriveLiveState(recs, at + 5_500)).toEqual({ state: "generating" });
  });

  it("a compaction usage record with no open compaction changes nothing (legacy runs keep their old reading)", () => {
    const at = toolAt + 1_000;
    expect(deriveLiveState([...before, compactUsage(at)], at + 2_000)).toEqual(deriveLiveState(before, at + 2_000));
  });

  it("reads STALL past its own bound with no end", () => {
    const at = toolAt + 1_000;
    expect(deriveLiveState([...before, compactStart(at, 60)], at + 59_000)).toMatchObject({ state: "prompt", compacting: true });
    expect(deriveLiveState([...before, compactStart(at, 60)], at + 61_000)).toEqual({ state: "stalled" });
  });

  it("with no bound on the record, the default inactivity window applies", () => {
    const at = toolAt + 1_000;
    expect(deriveLiveState([...before, compactStart(at)], at + 599_000)).toMatchObject({ compacting: true });
    expect(deriveLiveState([...before, compactStart(at)], at + 601_000)).toEqual({ state: "stalled" });
  });

  it("a routing job is not this execution's work: it never reads compacting", () => {
    const at = toolAt + 1_000;
    expect(deriveLiveState([...before, routingStart(at)], at + 2_000)).toEqual(deriveLiveState(before, at + 2_000));
  });

  it("(#2915 review, C4) counts from the start's own ms time, not its whole-second ts", () => {
    const at = toolAt + 1_000;
    const s = compactStart(at, 600);
    (s as unknown as { payload: Record<string, unknown> }).payload.started_at_ms = at + 800;
    expect(deriveLiveState([...before, s], at + 2_700)).toMatchObject({ compacting: true, compactingSeconds: 1 });
  });

  it("(#2915 review, C4) a sub-second compaction (start and end in one whole second) ends", () => {
    const at = Math.floor((toolAt + 1_000) / 1000) * 1000;
    const s = compactStart(at, 600);
    (s as unknown as { payload: Record<string, unknown> }).payload.started_at_ms = at + 200;
    const e = compactUsage(at);
    (e as unknown as { payload: Record<string, unknown> }).payload.ended_at_ms = at + 900;
    expect(deriveLiveState([...before, s, e], at + 1_500)).toEqual({ state: "prompt" });
  });

  it("labels as `compacting · Ns`, never `processing prompt`", () => {
    expect(liveStateLabel({ state: "prompt", compacting: true, compactingSeconds: 7 })).toBe("compacting · 7s");
    expect(liveStateLabel({ state: "prompt" })).toBe("processing prompt");
  });

  it("an execution's reading carries it, with no prompt size", () => {
    const at = toolAt + 1_000;
    const r = executionTokenReading([...before, compactStart(at, 600)], at + 4_000);
    expect(r).toMatchObject({ state: "prompt", compacting: true, compactingSeconds: 4 });
    expect(r.promptLabel).toBeUndefined();
  });
});

// (#2963) The run page's readout line for a tool that takes a file: the
// action and the file ("write · src/lib/tokenRate.ts"), for read, write and
// edit only. Anything else, or no file, has no line.
describe("toolReadout (#2963)", () => {
  it("names the action and the running call's file for read, write and edit", () => {
    expect(toolReadout({ state: "tools", toolName: "write", toolPath: "src/lib/tokenRate.ts" })).toEqual({ action: "write", path: "src/lib/tokenRate.ts" });
    expect(toolReadout({ state: "tools", toolName: "read", toolPath: "README.md" })).toEqual({ action: "read", path: "README.md" });
    expect(toolReadout({ state: "tools", toolName: "edit", toolPath: "a/b.rs" })).toEqual({ action: "edit", path: "a/b.rs" });
  });

  it("the action word alone when the running call's file is unknown", () => {
    expect(toolReadout({ state: "tools", toolName: "write" })).toEqual({ action: "write" });
  });

  it("has no line for another tool, no tool name, or while the call is generated", () => {
    expect(toolReadout({ state: "tools", toolName: "search", toolPath: "src" })).toBeNull();
    expect(toolReadout({ state: "tools", toolName: "bash", toolPath: "src/a.ts" })).toBeNull();
    expect(toolReadout({ state: "tools", toolPath: "src/a.ts" })).toBeNull();
    expect(toolReadout({ state: "tools", toolName: "write", toolPath: "src/a.ts", writing: true, writingSeconds: 3 })).toBeNull();
    expect(toolReadout({ state: "prompt", toolName: "write", toolPath: "src/a.ts" })).toBeNull();
  });
});
