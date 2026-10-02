import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { render } from "@testing-library/react";
import { UtilityGlyph } from "./UtilityGlyph";
import { UtilityResidency, type UtilityStrip } from "../lib/utilityJobs";

// (#2958) `noSignal` changes only what a QUIET strip says: a job already
// read (the live channel can carry one before the flow window answers) is a
// positive reading and shows.
describe("UtilityGlyph checking… (#2958)", () => {
  const quiet: UtilityStrip = { model: "darkmux:util-4b", residency: UtilityResidency.Resident, job: null };
  const routing = { ...quiet, job: { job: "radio_routing", visual: "radio", word: "radio routing", stalled: false, sinceMs: 0 } } as unknown as UtilityStrip;
  const label = (c: HTMLElement) => c.querySelector(".mach-util")!.getAttribute("aria-label");

  it("a quiet strip says checking…, not idle, and draws quiet", () => {
    const { container } = render(<UtilityGlyph strip={quiet} noSignal />);
    expect(label(container)).toBe("Utility model: darkmux:util-4b\nresident · checking…");
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
  const strip = (over: Partial<UtilityStrip>): UtilityStrip => ({ model: "darkmux:util-4b", residency: UtilityResidency.Resident, job: null, ...over });

  it("draws the robot in every state", () => {
    for (const s of [strip({}), strip({ residency: UtilityResidency.NotLoaded }), strip({ residency: UtilityResidency.Unknown, model: null }), strip({ model: null, residency: UtilityResidency.Unknown })]) {
      const { container, unmount } = render(<UtilityGlyph strip={s} />);
      expect(container.querySelector(".mach-util__bot"), JSON.stringify(s)).not.toBeNull();
      expect(container.querySelector(".mach-util__dot")).toBeNull();
      unmount();
    }
  });

  it("the tooltip and the accessible name lead with the role and the model", () => {
    const { container } = render(<UtilityGlyph strip={strip({ residency: UtilityResidency.NotLoaded })} />);
    const el = container.querySelector(".mach-util")!;
    expect(el.getAttribute("title")).toBe("Utility model: darkmux:util-4b\nnot loaded · idle");
    expect(el.getAttribute("aria-label")).toBe(el.getAttribute("title"));
  });

  it("with no utility model known to this viewer, it says unknown, not none", () => {
    const { container } = render(<UtilityGlyph strip={strip({ model: null, residency: UtilityResidency.Unknown })} />);
    expect(container.querySelector(".mach-util")!.getAttribute("title")).toBe("Utility model: unknown\nidle");
  });
});

// (5.0) Four residency states, drawn per the Machine Card v5 spec: solid and
// green when resident, faded gray when not loaded, dashed gray when unknown,
// and NO robot (its slot kept) when the machine registers none.
describe("UtilityGlyph residency states", () => {
  const strip = (residency: UtilityResidency, model: string | null = "darkmux:util-4b"): UtilityStrip => ({ model, residency, job: null });
  const el = (c: HTMLElement) => c.querySelector<HTMLElement>(".mach-util")!;

  it.each([
    [UtilityResidency.Resident, "filled"],
    [UtilityResidency.NotLoaded, "hollow"],
    [UtilityResidency.Unknown, "unknown"],
  ])("%s draws the robot with data-dot %s", (residency, dot) => {
    const { container } = render(<UtilityGlyph strip={strip(residency)} />);
    expect(el(container).getAttribute("data-dot")).toBe(dot);
    expect(el(container).getAttribute("data-residency")).toBe(residency);
    expect(container.querySelector(".mach-util__bot")).not.toBeNull();
  });

  it("only a resident robot takes the healthy color class", () => {
    const resident = render(<UtilityGlyph strip={strip(UtilityResidency.Resident)} />);
    expect(el(resident.container).classList.contains("mach-util--resident")).toBe(true);
    resident.unmount();
    for (const r of [UtilityResidency.NotLoaded, UtilityResidency.Unknown]) {
      const { container, unmount } = render(<UtilityGlyph strip={strip(r)} />);
      expect(el(container).classList.contains("mach-util--resident")).toBe(false);
      unmount();
    }
  });

  it("the resident color is the tube's phosphor token; the other states keep the identity gray", () => {
    const css = readFileSync("src/styles.css", "utf8");
    expect(css).toMatch(/\.mach-util--resident\s*\{\s*color:\s*var\(--scope-phosphor\);/);
    expect(css).toMatch(/\.mach-util\s*\{[^}]*color:\s*var\(--scope-utility\);/);
  });

  it("none: the slot stays, with no robot, no tooltip and nothing announced", () => {
    const { container } = render(<UtilityGlyph strip={strip(UtilityResidency.None, null)} />);
    const slot = el(container);
    expect(slot).not.toBeNull();
    expect(slot.getAttribute("data-residency")).toBe("none");
    expect(slot.querySelector("svg")).toBeNull();
    expect(slot.getAttribute("title")).toBeNull();
    expect(slot.getAttribute("aria-label")).toBeNull();
    expect(slot.getAttribute("aria-hidden")).toBe("true");
  });

  it("unknown with a model seen in the records still names it", () => {
    const { container } = render(<UtilityGlyph strip={strip(UtilityResidency.Unknown)} />);
    expect(el(container).getAttribute("title")).toBe("Utility model: darkmux:util-4b\nresidency unknown · idle");
  });
});
