import { describe, it, expect } from "vitest";
import { PANEL_IDS } from "../../lib/route";
import { PANELS, DEFAULT_PANEL_ID, isManualPanel, panelCols } from "./panels";

describe("PANELS", () => {
  it("covers exactly the routing allowlist, same drift guard as panel.rs's own PANEL_IDS test", () => {
    expect(PANELS.map((p) => p.id).sort()).toEqual([...PANEL_IDS].sort());
  });

  it("mission-status is the default panel, matching viewer.html's state.panelId initial value", () => {
    expect(DEFAULT_PANEL_ID).toBe("mission-status");
    expect(PANELS.some((p) => p.id === DEFAULT_PANEL_ID)).toBe(true);
  });

  it("only doctor is manual-only", () => {
    expect(isManualPanel("doctor")).toBe(true);
    for (const p of PANELS) {
      if (p.id !== "doctor") expect(isManualPanel(p.id)).toBe(false);
    }
  });
});

describe("panelCols", () => {
  it("clamps to the floor at a narrow width", () => {
    expect(panelCols({ clientWidth: 40 } as Element)).toBe(36);
  });

  it("clamps to the ceiling at a very wide width", () => {
    expect(panelCols({ clientWidth: 5000 } as Element)).toBe(200);
  });

  it("a phone's real width survives the clamp unchanged (#1613 parity with the daemon's own floor)", () => {
    // 390px viewport, minus the panel's own chrome, matches the daemon-side
    // fixture in `crates/darkmux-serve/src/panel.rs`'s `cols_clamped_hard`
    // test asserting 52 survives unclamped.
    expect(panelCols({ clientWidth: 399 } as Element)).toBe(52);
  });

  it("falls back to window.innerWidth when no element is passed", () => {
    const got = panelCols(null);
    expect(got).toBeGreaterThanOrEqual(36);
    expect(got).toBeLessThanOrEqual(200);
  });
});
