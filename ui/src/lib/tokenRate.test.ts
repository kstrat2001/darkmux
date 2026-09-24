import { describe, expect, it } from "vitest";
import type { FlowRecord } from "../types/handwritten";
import {
  DEFAULT_CHARS_PER_TOKEN,
  STALL_AFTER_MS,
  aggregateTokenRate,
  charsPerSecond,
  currentTokenRate,
  heartbeatSamples,
  isStalled,
  measuredCharsPerToken,
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
    expect(aggregateTokenRate([exec1, exec2])).toBeCloseTo(30, 5);
  });

  it("treats a stalled/fresh execution as contributing 0, not dropping the machine's total", () => {
    const generating = [beat(1_000, 40), beat(3_000, 120)];
    const fresh: FlowRecord[] = [beat(5_000, 0)];
    expect(aggregateTokenRate([generating, fresh])).toBeCloseTo(10, 5);
  });

  it("is null only when NO execution has a reading at all", () => {
    expect(aggregateTokenRate([[beat(1_000, 0)], []])).toBeNull();
  });
});
