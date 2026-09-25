import { describe, it, expect } from "vitest";
import { scopeCenter } from "./scopeCenter";

// (#2890) One center for every scope in the app (run page and fleet card).
describe("scopeCenter", () => {
  it("GEN: the rounded rate over tok/s, thinking or not", () => {
    expect(scopeCenter({ state: "generating", tokensPerSec: 61.6 })).toEqual({ centerLabel: "62", centerUnit: "tok/s", centerCarried: false });
    expect(scopeCenter({ state: "generating", tokensPerSec: 61.6, thinking: true }).centerUnit).toBe("tok/s");
    expect(scopeCenter({ state: "generating", tokensPerSec: null }).centerLabel).toBe("—");
    expect(scopeCenter({ state: "generating", tokensPerSec: 50, carried: true }).centerCarried).toBe(true);
  });
  it("REST: the countdown over resting", () => {
    expect(scopeCenter({ state: "rest", tokensPerSec: 0, restSecondsLeft: 8 })).toEqual({ centerLabel: "8s", centerUnit: "resting", centerCarried: false });
  });
  it("TOOLS: \"writing\", without the seconds, only while writing", () => {
    expect(scopeCenter({ state: "tools", tokensPerSec: 0, writing: true, writingSeconds: 10 })).toMatchObject({ centerLabel: null, centerUnit: "writing" });
    expect(scopeCenter({ state: "tools", tokensPerSec: 0 })).toMatchObject({ centerLabel: null, centerUnit: null });
  });
  it("PROMPT: the size over reading when known; nothing otherwise (the brain)", () => {
    expect(scopeCenter({ state: "prompt", tokensPerSec: 0, promptLabel: "~18k" })).toMatchObject({ centerLabel: "~18k", centerUnit: "processing" });
    expect(scopeCenter({ state: "prompt", tokensPerSec: 0 })).toMatchObject({ centerLabel: null, centerUnit: null });
  });
  it("IDLE says idle, on its own", () => {
    expect(scopeCenter({ state: "idle", tokensPerSec: 0 })).toEqual({ centerLabel: null, centerUnit: "idle", centerCarried: false });
  });
  it("a state's extras never leak into another state", () => {
    expect(scopeCenter({ state: "idle", tokensPerSec: 0, restSecondsLeft: 5, promptLabel: "~9k", writing: true })).toEqual({ centerLabel: null, centerUnit: "idle", centerCarried: false });
    expect(scopeCenter({ state: "stalled", tokensPerSec: 0, restSecondsLeft: 5, promptLabel: "~9k", writing: true })).toEqual({ centerLabel: null, centerUnit: null, centerCarried: false });
  });
});
