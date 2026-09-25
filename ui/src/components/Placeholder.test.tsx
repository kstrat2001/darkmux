import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { Shimmer } from "./Placeholder";

// (#2862) `Shimmer` is the ONE loading-placeholder implementation, lifted out
// of the fleet hero's bespoke `.savnum`/`.scv` overlay (#2817). These tests
// pin the contract every call site depends on: no leaked text (#2830's
// defect class), decorative to assistive tech (the CONTAINER carries
// `role="status"`/`aria-label`, not each shimmered value), and a caller-
// supplied box (`minHeight`/`minWidth`) so geometry never depends on content.
describe("Shimmer", () => {
  it("renders no text content", () => {
    const { container } = render(<Shimmer className="mv" minHeight="1.2em" />);
    const el = container.firstElementChild!;
    expect((el.textContent ?? "").trim()).toBe("");
  });

  it("is decorative — aria-hidden, so a screen reader does not announce it separately from the pending container", () => {
    const { container } = render(<Shimmer />);
    const el = container.firstElementChild!;
    expect(el.getAttribute("aria-hidden")).toBe("true");
  });

  it("carries the shimmer class plus any caller class, so it composes with existing layout CSS", () => {
    const { container } = render(<Shimmer className="savnum" />);
    const el = container.firstElementChild!;
    expect(el.className).toContain("ph-shimmer");
    expect(el.className).toContain("savnum");
  });

  it("renders as the caller's own element type, so a block value keeps its block box", () => {
    const { container } = render(<Shimmer as="span" minWidth="4em" />);
    expect(container.firstElementChild!.tagName).toBe("SPAN");
  });

  it("floors its own height so it cannot collapse to zero with no text (#2830's defect class)", () => {
    const { container } = render(<Shimmer minHeight="1.4em" />);
    const el = container.firstElementChild as HTMLElement;
    expect(el.style.minHeight).toBe("1.4em");
  });
});
