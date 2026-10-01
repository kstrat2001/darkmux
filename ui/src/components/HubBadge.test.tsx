import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { HubBadge } from "./HubBadge";

describe("HubBadge", () => {
  it("says HUB in the shared chip look and names what it means", () => {
    render(<HubBadge declared />);
    const badge = screen.getByTestId("hub-badge");
    expect(badge.textContent).toBe("hub");
    expect(badge.classList.contains("chip")).toBe(true);
    expect(badge.getAttribute("title")).toContain("fleet.mode hub");
  });

  it("renders nothing for a machine that does not declare hub", () => {
    const { container } = render(<HubBadge declared={false} />);
    expect(container.firstChild).toBeNull();
  });
});
