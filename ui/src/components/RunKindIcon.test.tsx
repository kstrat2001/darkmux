import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { RunKindIcon } from "./RunKindIcon";
import type { RunKind } from "../types/generated/RunKind";

// (operator, 2026-10-07) A run's KIND used to render as a colored chip in the
// status chip's own style, so a green "LAB" read as the same class of fact as
// a status. It is an icon now, monochrome, with the kind in its accessible
// name and tooltip. An inline SVG adds no text, so a text assertion cannot see
// it (see ActivityIcon.tsx's module doc): these assert on `data-run-kind`, the
// role and the label.
describe("<RunKindIcon>", () => {
  it.each([
    ["lab", "lab run", 3],
    ["mission", "mission run", 4],
    ["dispatch", "dispatch run", 2],
  ] as const)("%s renders its own glyph, named %j in its label and tooltip", (kind, label, paths) => {
    const el = render(<RunKindIcon kind={kind as RunKind} />).container.firstElementChild!;
    expect(el).toHaveAttribute("data-run-kind", kind);
    expect(el).toHaveAttribute("role", "img");
    expect(el).toHaveAttribute("aria-label", label);
    expect(el).toHaveAttribute("title", label);
    // The glyph is the lucide icon's own path set, not a shared placeholder.
    const svg = el.querySelector("svg")!;
    expect(svg).toHaveAttribute("aria-hidden", "true");
    expect(svg.querySelectorAll("path")).toHaveLength(paths);
    // No visible text: the kind is not a word on the page any more.
    expect(el.textContent).toBe("");
  });

  it("the three kinds draw three different glyphs", () => {
    const glyph = (k: RunKind) => render(<RunKindIcon kind={k} />).container.querySelector("svg")!.innerHTML;
    expect(new Set([glyph("lab"), glyph("mission"), glyph("dispatch")]).size).toBe(3);
  });

  it("a kind this build has no glyph for shows its word in the same slot, never nothing", () => {
    const el = render(<RunKindIcon kind={"benchmark" as RunKind} />).container.firstElementChild!;
    expect(el).toHaveAttribute("data-run-kind", "benchmark");
    expect(el).toHaveAttribute("aria-label", "benchmark run");
    expect(el.querySelector("svg")).toBeNull();
    expect(el.textContent).toBe("benchmark");
  });
});
