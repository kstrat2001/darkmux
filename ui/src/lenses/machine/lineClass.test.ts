/**
 * The machine lens classifies health lines by content pattern, and the
 * classification carries SEMANTIC COLOR — so a wrong rule doesn't throw, it
 * quietly tells the operator the wrong thing about their machine.
 *
 * This suite exists because the first cut of `lineClass` did exactly that.
 * It assumed an OK/RED state vocabulary; the daemon actually reports
 * green/amber/red. Every healthy model therefore matched the fallthrough
 * branch and rendered in the WARNING color, and `DARKMUX` — an owner tag,
 * not a health state — rendered as a warning too. Nothing failed. 330
 * component tests stayed green, `tsc` was clean, and the parity goldens
 * couldn't see it either, because a class name contributes no `innerText`.
 * It was caught by looking at a screenshot.
 *
 * So the assertions below pin the VOCABULARY against the live shape of
 * `/machine/resources`, and — per the inverted-case rule — assert the cases
 * where a state class must NOT be applied, since "everything is a warning"
 * is precisely the bug a happy-path-only test would have passed.
 */
import { describe, it, expect } from "vitest";
import { lineClass } from "./MachineLens";

describe("lineClass — state vocabulary", () => {
  // Verbatim from `/machine/resources` (`machine.state`, `models[].state`),
  // uppercased for display by `memoryLedgerLines.ts`.
  it("maps green to the OK severity, not the warning fallthrough", () => {
    expect(lineClass("GREEN")).toBe("mline--state is-ok");
  });

  it("maps amber to the warning severity", () => {
    expect(lineClass("AMBER")).toBe("mline--state is-warn");
  });

  it("maps red to the bad severity", () => {
    expect(lineClass("RED")).toBe("mline--state is-bad");
  });

  it("gives unknown a state chip with no severity color", () => {
    expect(lineClass("UNKNOWN")).toBe("mline--state");
  });
});

describe("lineClass — the inverted cases", () => {
  it("does NOT treat the owner tag as a health state", () => {
    // The original defect: uppercase-shaped matching made this amber.
    expect(lineClass("DARKMUX")).toBe("mline--owner");
    expect(lineClass("USER")).toBe("mline--owner");
  });

  it("does not award a state class to an arbitrary uppercase string", () => {
    // `?? ""` because the correct answer here is "no class at all" — asserting
    // `.not.toContain` directly on `undefined` is an invalid matcher call that
    // ERRORS rather than passing, which would have made this look like a
    // caught defect instead of a broken assertion.
    expect(lineClass("SOME OTHER TEXT") ?? "").not.toContain("mline--state");
  });

  it("leaves a model identifier unclassified rather than guessing", () => {
    expect(lineClass("darkmux:qwen3-4b-instruct-2507")).toBeUndefined();
  });
});

describe("lineClass — the authored markers", () => {
  it("recognizes the hint prefix", () => {
    expect(lineClass("↳ unload the 35B to free 18 GB")).toBe("mline--hint");
  });

  it("recognizes the warning prefix", () => {
    expect(lineClass("⚠ daemon unreachable — showing the last snapshot")).toBe("mline--warn");
  });

  it("recognizes a middot-joined meta line", () => {
    expect(lineClass("ctx 262144 · weights 18.45 GB · potential 24.57 GB")).toBe("mline--meta");
  });

  it("recognizes a bare lowercase label", () => {
    expect(lineClass("swap used")).toBe("mline--label");
    expect(lineClass("memory free")).toBe("mline--label");
  });

  it("does not mistake a long lowercase sentence for a label", () => {
    expect(lineClass("no runs recorded for this machine at this point in the timeline")).toBeUndefined();
  });
});
