import { describe, it, expect, afterEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { NavChrome } from "./NavChrome";
import type { Route } from "../lib/route";

afterEach(() => {
  window.location.hash = "";
});

describe("NavChrome", () => {
  it("renders all four tabs in legacy DOM order (viewer.html:816: fleet, console, runs, machine)", () => {
    render(<NavChrome route={{ kind: "fleet" }} />);
    const tabs = screen.getAllByRole("link");
    expect(tabs.map((t) => t.textContent)).toEqual(["fleet", "console", "runs", "machine"]);
    expect(tabs.map((t) => t.id)).toEqual(["lens-fleet", "lens-console", "lens-runs", "lens-machine"]);
  });

  it.each<[Route, string]>([
    [{ kind: "fleet" }, "lens-fleet"],
    [{ kind: "runs", runsKind: "all", run: null, machine: null }, "lens-runs"],
    [{ kind: "machine", uid: null }, "lens-machine"],
    [{ kind: "console", panelId: "", opts: {} }, "lens-console"],
    // Legacy: `state.level==="subsystem"` (a session drill-in) leaves the
    // fleet tab lit — see `NavChrome.tsx`'s own `isActive` doc.
    [{ kind: "dispatch", dispatchId: "abc-123" }, "lens-fleet"],
    // QA correction (2026-08-09, pre-#1868): the mission route lights
    // fleet, not console — see `NavChrome.tsx`'s own `isActive` doc for the
    // full measurement, and its #1868 note for why this still holds now
    // that the route renders `MissionGraphLens` for real.
    [{ kind: "mission", missionId: "m1" }, "lens-fleet"],
    // (#1809) A fleet-card drill (uid set) is the SAME shape as the session
    // drill above — arriving IN from fleet, not from a lens tab — so it
    // keeps FLEET lit, not MACHINE. The inverted case (uid: null) is
    // already covered two rows up.
    [{ kind: "machine", uid: "remote-uid" }, "lens-fleet"],
  ])("highlights exactly the tab matching %o -> %s", (route, expectedOnId) => {
    render(<NavChrome route={route} />);
    const tabs = screen.getAllByRole("link");
    for (const tab of tabs) {
      if (tab.id === expectedOnId) {
        expect(tab.className).toMatch(/\bon\b/);
        expect(tab.getAttribute("aria-current")).toBe("page");
      } else {
        expect(tab.className).not.toMatch(/\bon\b/);
        expect(tab.getAttribute("aria-current")).toBeNull();
      }
    }
  });

  it("an unrecognized route lights no tab at all", () => {
    render(<NavChrome route={{ kind: "unknown", hash: "lens=bogus" }} />);
    for (const tab of screen.getAllByRole("link")) {
      expect(tab.className).not.toMatch(/\bon\b/);
    }
  });

  it("clicking a tab writes the corresponding hash and prevents the default navigation", () => {
    render(<NavChrome route={{ kind: "fleet" }} />);
    fireEvent.click(screen.getByRole("link", { name: "machine" }));
    expect(window.location.hash).toBe("#lens=machine");
  });

  it("clicking the fleet tab from elsewhere clears the hash", () => {
    window.location.hash = "#lens=machine";
    render(<NavChrome route={{ kind: "machine", uid: null }} />);
    fireEvent.click(screen.getByRole("link", { name: "fleet" }));
    expect(window.location.hash).toBe("");
  });
});
