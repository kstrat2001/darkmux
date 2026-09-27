import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { ToolIcon } from "./ToolIcon";

const paths = (kind: Parameters<typeof ToolIcon>[0]["kind"]) =>
  [...render(<ToolIcon kind={kind} />).container.querySelectorAll("path")].map((p) => p.getAttribute("d"));

// (#2963, operator 2026-09-27) `write` REPLACES a whole file, so its page
// with a "+" read as "adds lines only". It is a plain file now: the page
// outline and its folded corner, no mark. The words carry the detail.
describe("ToolIcon (#2963)", () => {
  it("write is a plain file: the page outline and its folded corner, nothing else", () => {
    expect(paths("write")).toEqual(["M6 3h8l4 4v14H6z", "M14 3v4h4"]);
  });

  it("edit keeps its pencil", () => {
    expect(paths("edit")).toEqual(["M15.5 4.5l4 4L9 19H5v-4L15.5 4.5z", "M13.5 6.5l4 4"]);
  });
});
