import { describe, it, expect } from "vitest";
import { PANEL_IDS } from "../../lib/route";
import { PANELS, isManualPanel, panelCols } from "./panels";

describe("PANELS", () => {
  it("covers exactly the routing allowlist, same drift guard as panel.rs's own PANEL_IDS test", () => {
    expect(PANELS.map((p) => p.id).sort()).toEqual([...PANEL_IDS].sort());
  });

  // (#1904) `DEFAULT_PANEL_ID`/"mission-status is the default panel" is
  // retired along with this test — the console's landing default is no
  // longer any CLI panel, it's the client-rendered "activity" view
  // (`ConsolePanel.tsx`'s own `ConsoleSelection` union, outside this
  // module's PANELS entirely). `ConsolePanel.test.tsx` pins the real landing
  // behavior now; `mission-status` stays a normal, selectable entry in this
  // list, covered by the drift-guard test above.

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
