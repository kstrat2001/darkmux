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
    expect(label(container)).toBe("Utility model: darkmux:util-4b\nresident · no signal");
    expect(container.querySelector(".mach-util")!.getAttribute("data-visual")).toBe("quiet");
  });

  it("a running job still shows while its other sources are unanswered", () => {
    const { container } = render(<UtilityGlyph strip={routing} noSignal />);
    expect(label(container)).toBe("Utility model: darkmux:util-4b\nresident · radio routing");
    expect(container.querySelector(".mach-util")!.getAttribute("data-visual")).toBe("radio");
  });

  it("answered, a quiet strip is idle", () => {
    const { container } = render(<UtilityGlyph strip={quiet} />);
    expect(label(container)).toBe("Utility model: darkmux:util-4b\nresident · idle");
  });
});

// (operator, 2026-10-01) The bare dot was not readable as "the utility
// model": it is drawn as a robot (Lucide `bot`, ISC, ui/vendor-licenses),
// and the tooltip leads with what it is and which model.
describe("UtilityGlyph is a robot that says what it is", () => {
  const strip = (over: Partial<UtilityStrip>): UtilityStrip => ({ model: "darkmux:util-4b", resident: true, job: null, ...over });

  it("draws the robot in every state", () => {
    for (const s of [strip({}), strip({ resident: false }), strip({ resident: null }), strip({ model: null })]) {
      const { container, unmount } = render(<UtilityGlyph strip={s} />);
      expect(container.querySelector(".mach-util__bot"), JSON.stringify(s)).not.toBeNull();
      expect(container.querySelector(".mach-util__dot")).toBeNull();
      unmount();
    }
  });

  it("the tooltip and the accessible name lead with the role and the model", () => {
    const { container } = render(<UtilityGlyph strip={strip({ resident: false })} />);
    const el = container.querySelector(".mach-util")!;
    expect(el.getAttribute("title")).toBe("Utility model: darkmux:util-4b\nnot loaded · idle");
    expect(el.getAttribute("aria-label")).toBe(el.getAttribute("title"));
  });

  it("with no utility model known to this viewer, it says unknown, not none", () => {
    const { container } = render(<UtilityGlyph strip={strip({ model: null, resident: null })} />);
    expect(container.querySelector(".mach-util")!.getAttribute("title")).toBe("Utility model: unknown\nidle");
  });
});
