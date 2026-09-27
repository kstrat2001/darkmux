import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { FlowReadNotice } from "./FlowReadNotice";

// (#2965) The notice names which day's read failed: "recent activity" is
// wrong when only yesterday's did, since today's records are all there.
describe("FlowReadNotice (#2965)", () => {
  const text = (today: boolean, yesterday: boolean) =>
    render(<FlowReadNotice failure={{ status: 500, message: "500 Internal Server Error", today, yesterday }} />).container.textContent ?? "";

  it("renders nothing without a failure", () => {
    expect(render(<FlowReadNotice failure={null} />).container.innerHTML).toBe("");
  });
  it("names today's read when only today failed", () => {
    expect(text(true, false)).toContain("Today's flow records failed to load (500 Internal Server Error)");
  });
  it("names yesterday's read when only yesterday failed", () => {
    const t = text(false, true);
    expect(t).toContain("Yesterday's flow records failed to load (500 Internal Server Error)");
    expect(t).not.toMatch(/recent|today/i);
  });
  it("names both when both failed", () => {
    expect(text(true, true)).toContain("Flow records for today and yesterday failed to load");
  });
  it("uses no em-dash", () => {
    for (const [a, b] of [[true, false], [false, true], [true, true]] as const) expect(text(a, b)).not.toContain("—");
  });
});
