import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { UtilityGlyph } from "./UtilityGlyph";
import type { UtilityStrip } from "../lib/utilityJobs";

// (#2958) `noSignal` changes only what a QUIET strip says: a job already
// read (the live channel can carry one before the flow window answers) is a
// positive reading and shows.
describe("UtilityGlyph no signal (#2958)", () => {
  const quiet: UtilityStrip = { model: "darkmux:util-4b", resident: true, job: null };
  const routing = { ...quiet, job: { job: "radio_routing", visual: "radio", word: "radio routing", stalled: false, sinceMs: 0 } } as unknown as UtilityStrip;
  const label = (c: HTMLElement) => c.querySelector(".mach-util")!.getAttribute("aria-label");

  it("a quiet strip says no signal, not idle, and draws quiet", () => {
    const { container } = render(<UtilityGlyph strip={quiet} noSignal />);
    expect(label(container)).toBe("utility model darkmux:util-4b · resident · no signal");
    expect(container.querySelector(".mach-util")!.getAttribute("data-visual")).toBe("quiet");
  });

  it("a running job still shows while its other sources are unanswered", () => {
    const { container } = render(<UtilityGlyph strip={routing} noSignal />);
    expect(label(container)).toBe("utility model darkmux:util-4b · resident · radio routing");
    expect(container.querySelector(".mach-util")!.getAttribute("data-visual")).toBe("radio");
  });

  it("answered, a quiet strip is idle", () => {
    const { container } = render(<UtilityGlyph strip={quiet} />);
    expect(label(container)).toBe("utility model darkmux:util-4b · resident · idle");
  });
});
