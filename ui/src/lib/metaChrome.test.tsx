import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { ReadyHeadline } from "../components/ReadyHeadline";

// (operator: "it's the tests that don't force the proper result yet")
//
// The goldens CANNOT catch what these assert. `#meta` is an extracted parity
// region, but extraction compares `innerText` — and an inline SVG contributes
// no text, while a CSS class contributes none either. So the port shipped
// this headline as a flat string: the status dot lost its `ok` class (no
// green) and the machine icon was dropped entirely, and all 22 goldens
// matched byte-for-byte the whole time.
//
// The rule these encode: for anything PURELY VISUAL, assert STRUCTURE —
// element present, class applied — because text extraction is blind to it.

describe("ReadyHeadline", () => {
  it("renders the status dot with the class that colours it green", () => {
    const { container } = render(<ReadyHeadline n={2} ago="10h ago" />);
    const dot = container.querySelector(".rdot");
    expect(dot).toBeTruthy();
    expect(dot!.className).toContain("ok");
  });

  it("renders the machine ICON beside the count, not just the number", () => {
    const { container } = render(<ReadyHeadline n={2} ago="10h ago" />);
    const mco = container.querySelector(".mco");
    expect(mco).toBeTruthy();
    expect(mco!.querySelector("svg")).toBeTruthy();
  });

  it("keeps the count reachable to a screen reader via the wrapper's title", () => {
    const { container } = render(<ReadyHeadline n={3} ago="" />);
    expect(container.querySelector(".mco")!.getAttribute("title")).toBe("3 machines online");
  });

  it("says machine, singular, for one", () => {
    const { container } = render(<ReadyHeadline n={1} ago="" />);
    expect(container.querySelector(".mco")!.getAttribute("title")).toBe("1 machine online");
  });

  it("reproduces legacy's exact TEXT, so the parity goldens stay honest", () => {
    const { container } = render(<ReadyHeadline n={2} ago="10h ago" />);
    expect(container.textContent).toBe("● ready · 2  · last run 10h ago");
  });

  it("omits the last-run clause when no run is known", () => {
    const { container } = render(<ReadyHeadline n={2} ago="" />);
    expect(container.textContent).toBe("● ready · 2 ");
  });
});
