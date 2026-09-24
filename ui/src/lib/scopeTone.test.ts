import { describe, it, expect } from "vitest";
import { SCOPE_TONE_TOKEN, parseHexRgb, lighten } from "./scopeTone";

describe("scope tone: the trace takes the lit lamp's color", () => {
  it("maps each state to the SAME token its lamp uses, and no state to the phosphor green", () => {
    expect(SCOPE_TONE_TOKEN).toEqual({
      generating: "--scope-phosphor",
      prompt: "--lamp-prompt",
      tools: "--lamp-tools",
      rest: "--lamp-rest",
      stalled: "--lamp-stall",
      none: "--scope-phosphor",
      nosignal: "--scope-nosignal",
    });
  });

  it("parses a token's hex value, tolerating the whitespace getComputedStyle returns", () => {
    expect(parseHexRgb(" #fbbf24")).toEqual([251, 191, 36]);
    expect(parseHexRgb("#7dffa0")).toEqual([125, 255, 160]);
    expect(parseHexRgb("not a color")).toBeNull();
  });

  it("lightens toward white for the sweep dot's hot core", () => {
    expect(lighten([0, 100, 200], 0.5)).toEqual([128, 178, 228]);
  });
});
