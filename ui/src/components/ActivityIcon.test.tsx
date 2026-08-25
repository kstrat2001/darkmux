import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { ActivityIcon, ACT_ICON } from "./ActivityIcon";
import { activityOf } from "../lib/eventFilters";
import type { FlowRecord } from "../types/handwritten";

/** These assert on `data-act-icon`, never on text. An inline SVG contributes
 * nothing to `innerText` — the exact reason the icons could go missing from
 * the port while every text-extraction parity golden still matched (see the
 * component's module doc, and `MachineIcon`'s before it). A test that reads
 * rendered text here would pass with the glyphs deleted. */
function iconKeyFor(act: string): string | null {
  const { container } = render(<ActivityIcon act={act} />);
  return container.querySelector("[data-act-icon]")?.getAttribute("data-act-icon") ?? null;
}

describe("ActivityIcon", () => {
  it("gives a tool call the wrench and reasoning the brain", () => {
    expect(iconKeyFor("tool call")).toBe("tool");
    expect(iconKeyFor("reasoning")).toBe("brain");
  });

  it("renders an actual glyph, not just the wrapper", () => {
    const { container } = render(<ActivityIcon act="tool call" />);
    expect(container.querySelector("svg")).not.toBeNull();
    expect(container.querySelector("svg path")).not.toBeNull();
  });

  it("carries the activity as a class and a title so the glyph is identifiable", () => {
    const { container } = render(<ActivityIcon act="dispatch start" />);
    const el = container.querySelector(".aico");
    expect(el?.className).toContain("act-dispatch-start");
    expect(el?.getAttribute("title")).toBe("dispatch start");
  });

  it("renders nothing for an unmapped activity rather than an empty box", () => {
    const { container } = render(<ActivityIcon act="something new" />);
    expect(container.querySelector(".aico")).toBeNull();
  });

  /** The map is keyed by `activityOf`'s OUTPUT, so a typo in either one is a
   * silently icon-less row. Checking a real record end-to-end catches the
   * drift a hand-written key list cannot. */
  it("is keyed by what activityOf actually returns", () => {
    const cases: Array<[string, string]> = [
      ["dispatch.tool", "tool"],
      ["dispatch.reasoning", "brain"],
      ["dispatch.turn", "turn"],
      ["dispatch.checkpoint", "checkpoint"],
    ];
    for (const [action, expected] of cases) {
      const act = activityOf({ ts: "", action } as unknown as FlowRecord);
      expect(ACT_ICON[act] ?? "").toBe(expected);
    }
  });
});
