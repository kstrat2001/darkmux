import { describe, expect, it } from "vitest";
import { engineLabel, engineOf, engineRow, engineSuffix, residentsUnknownNote } from "./machineEngine";

const mlx = { name: "local-8080", reports_residency: false };
const lms = { name: "lmstudio", reports_residency: true };

describe("machineEngine (#316 stage 4.5)", () => {
  it("reads a card with no engine as LM Studio, the only engine before 4.5", () => {
    expect(engineOf({ engine: null })).toEqual(lms);
    expect(engineLabel(engineOf({ engine: null }))).toBe("LM Studio");
  });

  it("names an engine that does not report residency as such, never as an empty machine", () => {
    expect(residentsUnknownNote({ engine: mlx, lms_unreachable: false })).toBe(
      "local-8080 does not report which models it has loaded.",
    );
    expect(engineRow({ engine: mlx })).toBe("local-8080 · does not report what it holds");
  });

  it("says an unreachable engine is unknown, and only an answering one may read as empty", () => {
    expect(residentsUnknownNote({ engine: lms, lms_unreachable: true })).toMatch(/LM Studio could not be asked/);
    expect(residentsUnknownNote({ engine: { name: "nope", reports_residency: false }, lms_unreachable: true })).toMatch(
      /`nope` cannot be used/,
    );
    expect(residentsUnknownNote({ engine: lms, lms_unreachable: false })).toBeNull();
    expect(engineRow({ engine: lms })).toBe("LM Studio");
  });

  it("adds the engine to the hardware line only when it is not LM Studio", () => {
    expect(engineSuffix(mlx)).toBe(" · local-8080");
    expect(engineSuffix(lms)).toBe("");
  });
});
