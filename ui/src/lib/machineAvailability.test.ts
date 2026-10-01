import { describe, it, expect } from "vitest";
import { availabilityWarning, machineAvailability } from "./machineAvailability";

describe("machineAvailability (#3012)", () => {
  it("self is always known, even with nothing in the window", () => {
    expect(machineAvailability({ self: true, seen: false, standing: "unknown" })).toBe("known");
  });
  it("a peer nothing reaches this viewer from is not streamed, whatever its card says", () => {
    expect(machineAvailability({ self: false, seen: false, standing: "online" })).toBe("not_streamed");
  });
  it("a peer that is offline is not reporting, never not streamed, seen or not", () => {
    expect(machineAvailability({ self: false, seen: false, standing: "offline" })).toBe("not_reporting");
  });
  it("a peer seen and now offline is not reporting", () => {
    expect(machineAvailability({ self: false, seen: true, standing: "offline" })).toBe("not_reporting");
  });
  it("a peer seen and up or unsure is known", () => {
    expect(machineAvailability({ self: false, seen: true, standing: "online" })).toBe("known");
    expect(machineAvailability({ self: false, seen: true, standing: "unknown" })).toBe("known");
  });
  it("only the unknown states warn", () => {
    expect(availabilityWarning("known")).toBeNull();
    expect(availabilityWarning("not_streamed")).toMatch(/do not reach this viewer/);
    expect(availabilityWarning("not_reporting")).toMatch(/stopped reporting/);
  });
});
