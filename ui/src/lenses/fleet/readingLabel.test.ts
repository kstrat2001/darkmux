import { describe, expect, it } from "vitest";
import { readingLabel, readingTitle } from "./readingLabel";
import type { ExecutionTokenReading } from "../../lib/tokenRate";

const exec = (over: Partial<ExecutionTokenReading>): ExecutionTokenReading =>
  ({ sessionId: "s1", role: "coder", state: "generating", tokensPerSec: null, carried: false, ...over }) as ExecutionTokenReading;

describe("readingLabel: the status line while an execution runs", () => {
  it("generating says the measured rate", () => {
    expect(readingLabel(exec({ tokensPerSec: 41.6 }), "generating")).toEqual({ kind: "text", text: "42 tok/s" });
  });
  it("generating with no reading yet is not yet measured, never a confident zero", () => {
    expect(readingLabel(exec({ tokensPerSec: null }), "generating")).toEqual({ kind: "text", text: "— tok/s" });
  });
  it("thinking keeps its word, measured or not", () => {
    expect(readingLabel(exec({ tokensPerSec: 12, thinking: true }), "generating")).toEqual({ kind: "text", text: "12 think tok/s" });
    expect(readingLabel(exec({ tokensPerSec: null, thinking: true }), "generating")).toEqual({ kind: "text", text: "— think tok/s" });
  });
  it("prompt processing names the estimated size when it is known", () => {
    expect(readingLabel(exec({ state: "prompt", promptLabel: "~36k" }), "prompt")).toEqual({ kind: "text", text: "processing ~36k" });
    expect(readingLabel(exec({ state: "prompt" }), "prompt")).toEqual({ kind: "text", text: "processing prompt" });
  });
  it("(#3210) prompt processing with the engine's progress says how far, and how long is left", () => {
    const promptProgress = { processed: 1_024, total: 2_516, percent: 40, secondsLeft: 23 };
    // (live check on a 1280px desktop) "processing 40% · ~23s l…" ellipsized
    // the time left on a three-card row: with a time left, the line is the
    // percentage and the time alone (the brain and its bar say "processing").
    expect(readingLabel(exec({ state: "prompt", promptLabel: "~36k", promptProgress }), "prompt")).toEqual({ kind: "text", text: "40% · ~23s left" });
    // Before there is a rate, the word keeps the line readable.
    const first = { processed: 512, total: 2_516, percent: 20 };
    expect(readingLabel(exec({ state: "prompt", promptProgress: first }), "prompt")).toEqual({ kind: "text", text: "processing 20%" });
    expect(readingTitle(exec({ state: "prompt", promptProgress }), "prompt")).toBe("uncached prompt processed: 1,024 of 2,516 tokens, about 23s left");
  });
  it("compacting counts its seconds", () => {
    expect(readingLabel(exec({ state: "prompt", compacting: true, compactingSeconds: 12 }), "prompt")).toEqual({ kind: "text", text: "compacting · 12s" });
  });
  it("a rest with a reason carries both forms of it, for the card to pick by width", () => {
    const label = readingLabel(exec({ state: "rest", restReason: "thermal · serious", restReasonWord: "thermal" }), "rest");
    expect(label).toMatchObject({ kind: "byWidth", word: "thermal" });
  });
  it("a rest without a reason counts down", () => {
    expect(readingLabel(exec({ state: "rest", restSecondsLeft: 12 }), "rest")).toEqual({ kind: "text", text: "rest 12s" });
  });
  it("stalled and tool generation use the shared state words", () => {
    expect(readingLabel(exec({ state: "stalled" }), "stalled")).toEqual({ kind: "text", text: "stalled" });
    expect(readingLabel(exec({ state: "tools", writing: true, toolName: "write", writingSeconds: 18 }), "tools")).toEqual({ kind: "text", text: "tool gen · write · 18s" });
  });
});

describe("readingTitle", () => {
  it("repeats the prompt size and the whole rest reason on hover", () => {
    expect(readingTitle(exec({ promptLabel: "~36k" }), "prompt")).toBe("estimated prompt size: ~36k tokens");
    expect(readingTitle(exec({ restReason: "thermal · serious" }), "rest")).toBe("resting: thermal · serious");
    expect(readingTitle(exec({}), "generating")).toBeUndefined();
  });
});
