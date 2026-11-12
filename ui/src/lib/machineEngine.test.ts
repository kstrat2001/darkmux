import { describe, expect, it } from "vitest";
import { darkmuxResidents, engineLabel, engineOf, engineRow, engineSuffix, residentsKnown, residentsUnknownNote } from "./machineEngine";

const mlx = { name: "local-8080", reports_residency: false, usable: true };
const lms = { name: "lmstudio", reports_residency: true, usable: true };
const bogus = { name: "nope", reports_residency: false, usable: false };

describe("machineEngine (#316 stage 4.5)", () => {
  it("reads a card with no engine as LM Studio, the only engine before 4.5", () => {
    expect(engineOf({ engine: null })).toEqual(lms);
    expect(engineLabel(engineOf({ engine: null }))).toBe("LM Studio");
  });

  it("names an engine that does not report residency as such, never as an empty machine", () => {
    expect(residentsUnknownNote({ engine: mlx, lms_unreachable: true })).toBe("local-8080 does not report which models it has loaded.");
    expect(engineRow({ engine: mlx })).toBe("local-8080 · does not report what it holds");
  });

  it("says an unasked LM Studio is unknown, and only a known list may read as empty", () => {
    expect(residentsUnknownNote({ engine: lms, lms_unreachable: true })).toMatch(/LM Studio could not be asked/);
    expect(residentsUnknownNote({ engine: null, lms_unreachable: true })).toMatch(/LM Studio could not be asked/);
    expect(residentsUnknownNote({ engine: lms, lms_unreachable: false })).toBeNull();
    expect(residentsUnknownNote(null)).toBeNull();
    // A failed read of another engine is that engine, never "does not report".
    expect(residentsUnknownNote({ engine: { ...mlx, reports_residency: true }, lms_unreachable: true })).toBe(
      "local-8080 could not be asked; what it holds is unknown.",
    );
    expect(residentsKnown({ engine: mlx, lms_unreachable: true })).toBe(false);
    expect(residentsKnown(undefined)).toBe(true);
    expect(engineRow({ engine: lms })).toBe("LM Studio");
  });

  it("says a setting that names no usable engine cannot be used, never that it does not report", () => {
    expect(residentsUnknownNote({ engine: bogus, lms_unreachable: true })).toMatch(/`nope` cannot be used/);
    expect(engineRow({ engine: bogus })).toBe("nope · cannot be used (darkmux doctor names why)");
  });

  // (#3554) The fleet card's idle count line: what the machine holds for
  // darkmux, never a model another app loaded (contract 4), and never a
  // count when the list is not the engine's answer.
  describe("darkmuxResidents", () => {
    const model = (identifier: string) => ({ identifier, model: identifier.replace(/^darkmux:/, ""), status: "idle", size: "1 GB", context: 4096 });
    it("counts the residents in darkmux's namespace only", () => {
      const specs = { engine: lms, lms_unreachable: false, loaded_models: [model("darkmux:qwen"), model("darkmux:util-4b"), model("user-loaded")] };
      expect(darkmuxResidents(specs)).toEqual({ known: true, count: 2 });
    });
    it("counts zero for an engine that answered with nothing of darkmux's", () => {
      expect(darkmuxResidents({ engine: lms, lms_unreachable: false, loaded_models: [model("user-loaded")] })).toEqual({ known: true, count: 0 });
    });
    it("is unknown, with the reason, when the list is not the engine's answer", () => {
      expect(darkmuxResidents({ engine: mlx, lms_unreachable: true, loaded_models: [] })).toEqual({
        known: false,
        why: "local-8080 does not report which models it has loaded.",
      });
    });
    it("claims nothing when no card was read", () => {
      expect(darkmuxResidents(null)).toBeNull();
    });
    it("claims nothing for a card that leaves the list out", () => {
      expect(darkmuxResidents({ engine: lms, lms_unreachable: false } as never)).toBeNull();
    });
  });

  it("adds the engine to the hardware line only when it is not LM Studio", () => {
    expect(engineSuffix(mlx)).toBe(" · local-8080");
    expect(engineSuffix(lms)).toBe("");
  });
});
