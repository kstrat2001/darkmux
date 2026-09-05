import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { render, screen, fireEvent } from "@testing-library/react";
import { PhoneDrawer } from "./PhoneDrawer";
import { EventLogColumn } from "./EventLogColumn";
import type { FlowRecord } from "../types/handwritten";

function readStylesheet(): string {
  return readFileSync(
    path.join(path.dirname(fileURLToPath(import.meta.url)), "../styles.css"),
    "utf-8",
  );
}

/** Extracts one `selector { ... }` rule's body — the FIRST match, source
 * order. jsdom performs no real layout, so reading the stylesheet's own
 * text is how several `#2108` tests verify a CSS claim (a token, a
 * threshold, a property) without needing a real browser. */
function ruleBody(css: string, selector: string): string {
  const escaped = selector.replace(/[.#]/g, "\\$&");
  const match = css.match(new RegExp(`${escaped}\\s*\\{([^}]*)\\}`));
  if (!match) throw new Error(`no rule found for ${selector}`);
  return match[1];
}

const NOOP_MACHINE_TAB = {
  body: <div data-act="stub-machine-body">stats</div>,
};

// (#2416) `dispatch.reasoning` — the default event-filter view now curates
// by activity (reasoning/checkpoint/tool call/turn/dispatch error); the
// previous `note` fixture no longer shows by default, and none of these
// tests are about filtering, just generic row/drawer mechanics.
function record(i: number): FlowRecord {
  return {
    ts: `2026-01-01T00:0${i}:00Z`,
    category: "work",
    source: "operator",
    action: "dispatch.reasoning",
    handle: `rec-${i}`,
  };
}

const NO_EVENTS = {
  records: [] as FlowRecord[],
  scopeLabel: "fleet",
  visible: true,
  loading: false,
  error: null,
  historical: false,
  serverTruncated: false,
};

beforeEach(() => {
  window.localStorage.clear();
});

function drag(handle: Element, startY: number, endY: number) {
  fireEvent.pointerDown(handle, { clientY: startY, pointerId: 1 });
  fireEvent.pointerMove(handle, { clientY: endY, pointerId: 1 });
  fireEvent.pointerUp(handle, { clientY: endY, pointerId: 1 });
}

describe("PhoneDrawer (#2107 tabbed-drawer packet)", () => {
  it("renders the bar with both tabs, closed by default — no dialog, both tab labels visible", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    expect(
      document.querySelector('[data-act="phone-drawer-bar"]'),
    ).not.toBeNull();
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByText("Machine info")).toBeInTheDocument();
    expect(screen.getByText(/Events · 0/)).toBeInTheDocument();
  });

  it("tapping the Machine tab opens the drawer to the Machine panel", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(
      document.querySelector('[data-act="phone-drawer-panel-machine"]'),
    ).not.toBeNull();
    expect(
      document.querySelector('[data-act="stub-machine-body"]'),
    ).not.toBeNull();
  });

  it("tapping the ACTIVE tab again closes the drawer", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    const machineTab = document.querySelector(
      '[data-act="phone-drawer-tab-machine"]',
    )!;
    fireEvent.click(machineTab);
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    fireEvent.click(machineTab);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("tapping the OTHER tab while open switches tabs without closing", () => {
    const events = { ...NO_EVENTS, records: [record(1), record(2)] };
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={events}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(
      document.querySelector('[data-act="phone-drawer-panel-machine"]'),
    ).not.toBeNull();
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-events"]')!,
    );
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(
      document.querySelector('[data-act="phone-drawer-panel-machine"]'),
    ).toBeNull();
    expect(document.querySelector(".eventlog")).not.toBeNull();
    // (#2108, operator correction) The Events tab is the plain default
    // EventLogColumn — the row-list, same shape as the desktop column.
    expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(2);
  });

  it("tapping the handle closes the open drawer", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    const handle = document.querySelector('[data-act="phone-drawer-handle"]')!;
    fireEvent.pointerDown(handle, { clientY: 400, pointerId: 1 });
    fireEvent.pointerUp(handle, { clientY: 400, pointerId: 1 }); // no movement — a tap
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("Escape closes the open drawer", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("dragging the handle up resizes the open sheet and persists the SHARED height (not per-tab)", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    const handle = document.querySelector('[data-act="phone-drawer-handle"]')!;
    drag(handle, 600, 200); // drag up 400px — well past the tap threshold
    // (#2108, "one card" packet) The animated height now lives on the
    // OUTER sheet (`[data-act="phone-drawer"]`), not a separate body
    // element — the whole card grows/shrinks as one unit.
    const sheet = document.querySelector(
      '[data-act="phone-drawer"]',
    ) as HTMLElement;
    expect(sheet.style.height).not.toBe("88vh"); // moved off the default
    // (#2108, operator correction) ONE shared key now, not per-tab.
    expect(
      window.localStorage.getItem("dmux.phone-drawer.height"),
    ).not.toBeNull();
  });

  it("a drag that ends near the closed position snaps the drawer shut instead of leaving a sliver", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    const handle = document.querySelector('[data-act="phone-drawer-handle"]')!;
    // Open at the default 88vh, then drag DOWN past the close-snap floor.
    drag(handle, 200, 900);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("a stored height outside the drag range is clamped on load instead of trusted", () => {
    // (operator finding on a real phone, 2026-09-05) A persisted value
    // the drag logic could never have produced (a stale build's per-tab
    // key, a save taken against a keyboard-shrunk viewport, a hand
    // edit) opened the sheet at a height the layout wasn't designed
    // for — the Machine tab's content sat ~180px below the tab row and
    // the sheet read as "won't open". `openPct` is clamped through the
    // same `clampPct` the drag uses, so a stored 300 opens at
    // MAX_OPEN_PCT (90): `openHeightPx(90, 768, 64)` = min(691.2, 696)
    // = 691.2, and a stored 3 opens at MIN_OPEN_PCT (14): 0.14*768 =
    // 107.52.
    window.localStorage.setItem("dmux.phone-drawer.height", "300");
    const { unmount } = render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    let sheet = document.querySelector(
      '[data-act="phone-drawer"]',
    ) as HTMLElement;
    expect(parseFloat(sheet.style.height)).toBeCloseTo(691.2, 5);
    unmount();
    window.localStorage.setItem("dmux.phone-drawer.height", "3");
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    sheet = document.querySelector(
      '[data-act="phone-drawer"]',
    ) as HTMLElement;
    expect(parseFloat(sheet.style.height)).toBeCloseTo(107.52, 5);
  });

  /** Drives the handle with CONTROLLED clock samples so the drag's
   * velocity estimate is deterministic: `steps` are [clientY, ms] pairs
   * after the pointerdown at (startY, 0ms). */
  function dragTimed(handle: Element, startY: number, steps: Array<[number, number]>) {
    // A stepped clock, not a queue: React's scheduler reads
    // `performance.now()` too, so a shift-per-call queue drains out of
    // order under it and every sample collapses to dt=1ms.
    let now = 0;
    const spy = vi.spyOn(performance, "now").mockImplementation(() => now);
    try {
      fireEvent.pointerDown(handle, { clientY: startY, pointerId: 1 });
      for (const [y, t] of steps) {
        now = t;
        fireEvent.pointerMove(handle, { clientY: y, pointerId: 1 });
      }
      const last = steps[steps.length - 1][0];
      fireEvent.pointerUp(handle, { clientY: last, pointerId: 1 });
    } finally {
      spy.mockRestore();
    }
  }

  it("a fast upward flick snaps the sheet to its max height instead of stopping where the finger lifted", () => {
    // (operator finding on a real iPhone, 2026-09-05: "fast swipe doesn't
    // send to max") Stored 30%, then a flick that travels only 120px in
    // 60ms (2 px/ms, far over FLING_PX_PER_MS) — a drag-and-place would
    // land at 30 + 120/768*100 ≈ 45.6%; the flick lands at MAX (90%):
    // openHeightPx(90, 768, 64) = min(691.2, 696) = 691.2.
    window.localStorage.setItem("dmux.phone-drawer.height", "30");
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    const handle = document.querySelector('[data-act="phone-drawer-handle"]')!;
    dragTimed(handle, 600, [[560, 20], [520, 40], [480, 60]]);
    const sheet = document.querySelector(
      '[data-act="phone-drawer"]',
    ) as HTMLElement;
    expect(parseFloat(sheet.style.height)).toBeCloseTo(691.2, 5);
    expect(window.localStorage.getItem("dmux.phone-drawer.height")).toBe("90");
  });

  it("a slow drag of the same distance stays where the finger lifted (no flick)", () => {
    window.localStorage.setItem("dmux.phone-drawer.height", "30");
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    const handle = document.querySelector('[data-act="phone-drawer-handle"]')!;
    // 120px over 1200ms = 0.1 px/ms, well under the flick threshold.
    dragTimed(handle, 600, [[560, 400], [520, 800], [480, 1200]]);
    const sheet = document.querySelector(
      '[data-act="phone-drawer"]',
    ) as HTMLElement;
    // 30% + 120/768*100 = 45.625% of 768 = 350.4px
    expect(parseFloat(sheet.style.height)).toBeCloseTo(350.4, 5);
  });

  it("a fast downward flick closes the sheet even when the finger lifts above the close-snap floor", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    const handle = document.querySelector('[data-act="phone-drawer-handle"]')!;
    // Open at 88%; flick DOWN 120px in 60ms — still ~72% tall at release,
    // far above CLOSE_SNAP_PCT, but the flick closes it.
    dragTimed(handle, 200, [[240, 20], [280, 40], [320, 60]]);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("a pointercancel mid-drag finishes the drag instead of leaving the sheet frozen in its dragging state", () => {
    // (operator finding, real iPhone: "it loses control") iOS Safari
    // cancels the pointer when it claims a fast touch as a pan; the sheet
    // must settle (transition re-enabled, height committed), not hang.
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    const handle = document.querySelector('[data-act="phone-drawer-handle"]')!;
    fireEvent.pointerDown(handle, { clientY: 600, pointerId: 1 });
    fireEvent.pointerMove(handle, { clientY: 500, pointerId: 1 });
    let sheet = document.querySelector('[data-act="phone-drawer"]') as HTMLElement;
    expect(sheet.className).toContain("phone-drawer--dragging");
    fireEvent.pointerCancel(handle, { clientY: 500, pointerId: 1 });
    sheet = document.querySelector('[data-act="phone-drawer"]') as HTMLElement;
    expect(sheet.className).not.toContain("phone-drawer--dragging");
    expect(screen.queryByRole("dialog")).not.toBeNull();
    // An un-moved cancel is NOT a tap: it must not toggle the sheet.
    fireEvent.pointerDown(handle, { clientY: 600, pointerId: 2 });
    fireEvent.pointerCancel(handle, { clientY: 600, pointerId: 2 });
    expect(screen.queryByRole("dialog")).not.toBeNull();
  });

  it("re-opening EITHER tab restores the ONE shared persisted height", () => {
    window.localStorage.setItem("dmux.phone-drawer.height", "30");
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    // (#2108, "one card" packet) Same outer-sheet height as above.
    let sheet = document.querySelector(
      '[data-act="phone-drawer"]',
    ) as HTMLElement;
    // (#2108, operator finding — real device, round 2) The style is now
    // a REAL PIXEL height computed in JS against jsdom's default
    // 768px `innerHeight` (no `visualViewport` there) and the 64px
    // masthead-height fallback (no `--masthead-h` set in this test) —
    // `openHeightPx(30, 768, 64)` = min(0.30*768, 768-64-8) =
    // min(230.4, 696) = 230.4. `toBeCloseTo` (not `toBe`) because
    // `0.30 * 768` is `230.39999999999998` in IEEE-754 floating point —
    // a real value, not a bug, and exact string equality on it is
    // fragile in a way the underlying claim ("~230.4px") isn't.
    expect(parseFloat(sheet.style.height)).toBeCloseTo(230.4, 5);
    expect(sheet.style.height.endsWith("px")).toBe(true);
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-events"]')!,
    );
    sheet = document.querySelector(
      '[data-act="phone-drawer"]',
    ) as HTMLElement;
    // (#2108, operator's core ask) Switching tabs while open must NEVER
    // change the sheet's height — one shared value, not a per-tab one.
    // (#2108, operator finding — real device, round 2) The style is now
    // a REAL PIXEL height computed in JS against jsdom's default
    // 768px `innerHeight` (no `visualViewport` there) and the 64px
    // masthead-height fallback (no `--masthead-h` set in this test) —
    // `openHeightPx(30, 768, 64)` = min(0.30*768, 768-64-8) =
    // min(230.4, 696) = 230.4. `toBeCloseTo` (not `toBe`) because
    // `0.30 * 768` is `230.39999999999998` in IEEE-754 floating point —
    // a real value, not a bug, and exact string equality on it is
    // fragile in a way the underlying claim ("~230.4px") isn't.
    expect(parseFloat(sheet.style.height)).toBeCloseTo(230.4, 5);
    expect(sheet.style.height.endsWith("px")).toBe(true);
  });

  // (#2108, operator correction — reverted from the earlier "every row
  // expanded inline" attempt, which was unreadable) The Events tab is the
  // SAME list + detail-pane split the desktop events column already has.
  // Tapping a row shows that record in the pane, right here in the sheet
  // — no route change, no drill-in navigation, no separate card. The pane
  // is empty/placeholder until a row is tapped.
  // (#2108, operator design change, round 4 — REWRITTEN) The phone Events
  // tab dropped the always-present split pane in favor of `pushDetail`
  // (`EventLogColumn.tsx`'s own doc on the switch, `PhoneDrawer.tsx`'s
  // call site): with no records, the LIST shows its own "no events yet"
  // placeholder — there is no separate pane to be empty any more, since
  // no pane renders at all until a row is tapped and pushed.
  it("with no records, the list shows its own empty placeholder — no pane, no pushed screen", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-events"]')!,
    );
    const empty = document.querySelector(".eventlog__empty");
    expect(empty).not.toBeNull();
    expect(empty!.textContent).toBe("no events yet");
    expect(document.querySelector('[data-act="eventlog-pushed"]')).toBeNull();
  });

  it("tapping a row PUSHES a full detail screen — the strip names the record, the back control returns to the list", () => {
    const older: FlowRecord = { ...record(1), payload: { tool_name: "grep", args_chars: 42 } } as FlowRecord;
    const newer: FlowRecord = { ...record(2), payload: { tool_name: "ls", args_chars: 3 } } as FlowRecord;
    const events = { ...NO_EVENTS, records: [older, newer] };
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={events}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-events"]')!,
    );
    // No pushed screen yet — the list is what's showing.
    expect(document.querySelector('[data-act="eventlog-pushed"]')).toBeNull();
    const rows = document.querySelectorAll('[data-act="rec"]');
    // Newest-first: `newer` (handle "rec-2", the row's own `title`
    // attribute — hover provenance, not visible row text) renders first.
    // Neither row carries its expanded payload — only the row's own short
    // preview line, exactly like the desktop column's rows.
    expect(rows[0].getAttribute("title")).toBe("rec-2");
    expect(rows[1].getAttribute("title")).toBe("rec-1");
    rows.forEach((row) => {
      expect(row.textContent).not.toContain("tool name");
    });
    fireEvent.click(rows[1]);
    // The list is REPLACED by the pushed screen (mutually exclusive
    // branches — see `EventLogColumn.tsx`'s own doc), not shown beside it.
    const pushed = document.querySelector('[data-act="eventlog-pushed"]');
    expect(pushed).not.toBeNull();
    expect(document.querySelector('[data-act="rec"]')).toBeNull();
    // The one-row strip at the top names WHICH record this is — and IS
    // the back control (round 5, operator finding: a separate bar
    // "wastes a row"; removed). Real, ≥44px control — checked here on
    // role/label; its painted height is a stylesheet-body concern,
    // covered by this file's own stylesheet-check tests below.
    const strip = document.querySelector('[data-act="rec-strip"]')!;
    expect(strip.textContent).toContain("reasoning"); // (#2416) fixture activity renamed from "note"
    expect(strip.getAttribute("role")).toBe("button");
    expect(strip.getAttribute("aria-label")).toBe("Back to list");
    expect(document.querySelector('[data-act="eventlog-back"]')).toBeNull();
    // `RecordView`'s own key rendering replaces underscores with spaces
    // (`Row`'s `.rv__key`) — asserting on ITS actual output, not a guess.
    const pane = document.querySelector(".eventlog__detailbody--pushed")!;
    expect(pane.textContent).toContain("tool name");
    expect(pane.textContent).toContain("grep");
    expect(pane.textContent).toContain("args chars");
    expect(pane.textContent).not.toContain("rec-2");
    expect(pane.textContent).toContain("rec-1");
    // No navigation, no hash change — this stays ON the sheet.
    expect(window.location.hash).toBe("");
    // Tapping the strip returns to the list, and the pushed screen is gone.
    fireEvent.click(strip);
    expect(document.querySelector('[data-act="eventlog-pushed"]')).toBeNull();
    expect(document.querySelectorAll('[data-act="rec"]').length).toBe(2);
  });

  // (#2108, operator correction — round 4) "follow" stays a real toggle on
  // the phone, not a one-shot "go to latest" action (the operator's own
  // reversal of an earlier ask on the same thread) — matching desktop's
  // own `selectRecord`, tapping a row clears it.
  it("tapping a row while follow is ON pushes detail and clears the armed follow state", () => {
    const events = { ...NO_EVENTS, records: [record(1), record(2), record(3)] };
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={events}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-events"]')!,
    );
    // `follow` defaults ON.
    expect(document.getElementById("follow")!.className).toContain(" on");
    fireEvent.click(document.querySelectorAll('[data-act="rec"]')[1]);
    expect(document.querySelector('[data-act="eventlog-pushed"]')).not.toBeNull();
    // The follow control isn't even in the DOM on the pushed screen (see
    // the test below) — tapping the strip BACK to the list is what
    // proves it was actually cleared, not just hidden.
    fireEvent.click(document.querySelector('[data-act="rec-strip"]')!);
    expect(document.getElementById("follow")!.className).not.toContain(" on");
  });

  // (#2108, operator's final form — round 4) The invariant is "in follow
  // mode you are always in the list state", enforced STRUCTURALLY: the
  // follow control lives only in the list's own header, never on the
  // pushed-detail screen — so follow can only ever be toggled from the
  // list, and there is no "pop the detail when follow turns on" code
  // path to test (an earlier draft of this had one; removed per the
  // operator's own correction).
  it("the pushed detail screen has NO follow control; back returns to the list with follow still off", () => {
    const events = { ...NO_EVENTS, records: [record(1), record(2), record(3)] };
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={events}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-events"]')!,
    );
    fireEvent.click(document.querySelectorAll('[data-act="rec"]')[1]);
    expect(document.querySelector('[data-act="eventlog-pushed"]')).not.toBeNull();
    // No follow control, no filter row — just the strip (itself the back
    // control, round 5) and the body.
    expect(document.getElementById("follow")).toBeNull();
    expect(document.querySelector(".eventlog__search")).toBeNull();
    fireEvent.click(document.querySelector('[data-act="rec-strip"]')!);
    expect(document.querySelector('[data-act="eventlog-pushed"]')).toBeNull();
    // Tapping a row turned follow off (matching desktop's own
    // `selectRecord`); going back doesn't turn it back on.
    expect(document.getElementById("follow")!.className).not.toContain(" on");
  });

  it("toggling follow ON from the list pins to the newest row and scrolls the list to the top", () => {
    const events = { ...NO_EVENTS, records: [record(1), record(2), record(3)] };
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={events}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-events"]')!,
    );
    // Tap an older row first: turns follow off (per the test above) and
    // scrolls the list, simulating a mid-read position.
    fireEvent.click(document.querySelectorAll('[data-act="rec"]')[1]);
    fireEvent.click(document.querySelector('[data-act="rec-strip"]')!);
    (document.getElementById("logbody") as HTMLElement).scrollTop = 120;
    expect(document.getElementById("follow")!.className).not.toContain(" on");
    fireEvent.click(document.getElementById("follow")!);
    expect(document.getElementById("follow")!.className).toContain(" on");
    expect((document.getElementById("logbody") as HTMLElement).scrollTop).toBe(0);
    // Newest row (rec-3, first — newest-first order) is the one pinned/
    // highlighted.
    const rows = document.querySelectorAll('[data-act="rec"]');
    expect(rows[0].getAttribute("title")).toBe("rec-3");
    expect(rows[0].className).toContain(" sel");
  });

  it("the list rows render identically to the desktop column's rows (same classes, same content)", () => {
    const recs = [record(1), record(2)];
    const events = { ...NO_EVENTS, records: recs };
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={events}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-events"]')!,
    );
    const sheetRows = [...document.querySelectorAll('[data-act="rec"]')];

    // The SAME `EventLogColumn`, mounted plainly (App.tsx's own desktop
    // call shape — no `pushDetail`, no phone-specific prop at all).
    const desktop = render(
      <EventLogColumn scopeLabel="fleet" records={recs} visible />,
    );
    const desktopRows = [
      ...desktop.container.querySelectorAll('[data-act="rec"]'),
    ];

    expect(sheetRows.length).toBe(2);
    expect(sheetRows.length).toBe(desktopRows.length);
    sheetRows.forEach((row, i) => {
      expect(row.className).toBe(desktopRows[i].className);
      expect(row.textContent).toBe(desktopRows[i].textContent);
    });
  });

  it("the sheet's open height is at least 85% of the viewport on first open (844px viewport)", () => {
    Object.defineProperty(window, "innerHeight", {
      configurable: true,
      value: 844,
    });
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    const sheet = document.querySelector(
      '[data-act="phone-drawer"]',
    ) as HTMLElement;
    // (#2108, operator finding — real device, round 2) The style is now
    // a real PIXEL height (`openHeightPx`'s own doc), not a percentage
    // string — recompute the percentage of the 844px viewport this test
    // set, rather than parsing a percent straight off the style (there
    // isn't one to parse any more).
    const px = parseFloat(sheet.style.height);
    const pct = (px / 844) * 100;
    expect(pct).toBeGreaterThanOrEqual(85);
  });

  it("the Events tab's connection dot reflects a live route's live status", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="reconnecting"
        route={{ kind: "fleet" }}
      />,
    );
    expect(
      document.querySelector(".phone-drawer__dot--reconnecting"),
    ).not.toBeNull();
  });

  it("the Events tab's connection dot reads as a replay on a non-live route", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "playback", date: "2026-01-01" }}
      />,
    );
    expect(document.querySelector(".phone-drawer__dot--replay")).not.toBeNull();
  });
});

// ── (operator finding) the Machine tab reads a static label, never a live
//    number — both closed and open. See `machineStatsContent.tsx`'s doc.
describe("PhoneDrawer — Machine tab label (#2107, #1833)", () => {
  it("reads 'Machine info' both closed and open, never a live CPU/GPU/MEM line", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    expect(screen.getByText("Machine info")).toBeInTheDocument();
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(screen.getByText("Machine info")).toBeInTheDocument();
    expect(screen.queryByText(/CPU \d/)).toBeNull();
    expect(screen.queryByText(/GPU \d/)).toBeNull();
  });
});

// ── (operator finding) the always-mounted body + its open/dragging classes,
//    the mechanism the slide-animation CSS keys off. `styles.css`'s own
//    `.phone-drawer--open .phone-drawer__body` / `.phone-drawer--dragging
//    .phone-drawer__body` rules are what actually animate — jsdom doesn't
//    apply real CSS, so these tests prove the CLASS TOGGLING is correct
//    (the mechanism), not the rendered motion itself.
describe("PhoneDrawer — slide transition class toggling (#2107, #1833)", () => {
  it("the body is ALWAYS mounted in the DOM, closed or open — only its role/content differ", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    const body = document.querySelector('[data-act="phone-drawer-body"]');
    expect(body).not.toBeNull();
    expect(body!.getAttribute("role")).toBeNull();
    expect(body!.getAttribute("aria-hidden")).toBe("true");

    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    const sameBody = document.querySelector('[data-act="phone-drawer-body"]');
    expect(sameBody).toBe(body); // same node — never unmounted/remounted
    expect(sameBody!.getAttribute("role")).toBe("dialog");
    expect(sameBody!.getAttribute("aria-hidden")).toBe("false");
  });

  it("the outer container carries `phone-drawer--open` only while open", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    const outer = document.querySelector('[data-act="phone-drawer"]')!;
    expect(outer.className).not.toMatch(/phone-drawer--open/);

    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(outer.className).toMatch(/phone-drawer--open/);

    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    ); // active tab tap closes
    expect(outer.className).not.toMatch(/phone-drawer--open/);
  });

  it("the outer container carries `phone-drawer--dragging` only WHILE a drag is in progress, so the transition disables live and re-enables on release", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    const outer = document.querySelector('[data-act="phone-drawer"]')!;
    const handle = document.querySelector('[data-act="phone-drawer-handle"]')!;
    expect(outer.className).not.toMatch(/phone-drawer--dragging/);

    fireEvent.pointerDown(handle, { clientY: 600, pointerId: 1 });
    fireEvent.pointerMove(handle, { clientY: 200, pointerId: 1 }); // real movement, past TAP_SLOP_PX
    expect(outer.className).toMatch(/phone-drawer--dragging/);

    fireEvent.pointerUp(handle, { clientY: 200, pointerId: 1 });
    expect(outer.className).not.toMatch(/phone-drawer--dragging/);
  });
});

// ── (operator finding) modal-while-open: scroll lock + tap-outside-closes ──
describe("PhoneDrawer — modal behavior while open (#2107, #1833)", () => {
  it("locks page scroll for the drawer's ENTIRE open lifetime, at any height — not gated on a height threshold", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    expect(document.body.style.overflow).not.toBe("hidden");
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(document.body.style.overflow).toBe("hidden");
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    ); // closes
    expect(document.body.style.overflow).not.toBe("hidden");
  });

  it("a click on the backdrop closes the open drawer, and the click does not reach an element behind it", () => {
    const behindClick = vi.fn();
    render(
      <>
        <button type="button" data-act="page-content" onClick={behindClick}>
          Page button
        </button>
        <PhoneDrawer
          machineTab={NOOP_MACHINE_TAB}
          events={NO_EVENTS}
          liveStatus="live"
          route={{ kind: "fleet" }}
        />
      </>,
    );
    expect(
      document.querySelector('[data-act="phone-drawer-backdrop"]'),
    ).toBeNull(); // no backdrop while closed

    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    const backdrop = document.querySelector(
      '[data-act="phone-drawer-backdrop"]',
    );
    expect(backdrop).not.toBeNull();

    fireEvent.click(backdrop!);
    expect(screen.queryByRole("dialog")).toBeNull();
    // The page's own button never received a click — the backdrop is a
    // SEPARATE element from the page content, so clicking it structurally
    // cannot dispatch to a different, unrelated node.
    expect(behindClick).not.toHaveBeenCalled();
  });
});

// ── (operator finding, iOS Safari Home Screen install) `overflow: hidden`
//    on <body> is ignored by iOS Safari — a drag on the open drawer still
//    scrolled the page behind it. The iOS-proof form pins <body> via
//    `position: fixed` at its current scroll offset and restores +
//    re-scrolls to that exact offset on close, covering every exit path
//    (tab close, unmount-while-open) through one effect keyed on `open`.
describe("PhoneDrawer — iOS scroll lock (operator finding)", () => {
  afterEach(() => {
    // Belt-and-braces: a failing assertion mid-test must not leave a real
    // fixed body bleeding into a LATER test's `document.body`.
    document.body.style.position = "";
    document.body.style.top = "";
    document.body.style.left = "";
    document.body.style.right = "";
    document.body.style.width = "";
    document.body.style.overflow = "";
  });

  // (#2108 review finding 4, WebKit-proven) Rewritten: opening now scrolls
  // to the TOP first, THEN pins at `top: 0` — no longer at the pre-open
  // scroll offset (`-240px`). A negative-offset pin broke
  // `.app-shell__sticky`'s `position: sticky` (sticky's "am I stuck" test
  // needs a genuinely scrolling ancestor, and `<body>` becoming
  // `position: fixed` removes that), which could land the sticky nav/tab
  // row inside the backdrop's masthead-clearance band — a real nav tap
  // while the drawer was supposed to be modal. Scrolling to 0 before
  // pinning means there is never a nonzero offset for sticky to lose. The
  // pre-open offset is still saved and restored via `window.scrollTo` on
  // close, same as before this fix — only the OPEN-time pin position
  // changed, not the restore contract.
  it("opening scrolls to the top and pins the body there (position: fixed, top: 0), then restores + scrolls back to the pre-open offset on close", () => {
    const scrollToSpy = vi.spyOn(window, "scrollTo").mockImplementation(() => {});
    Object.defineProperty(window, "scrollY", {
      configurable: true,
      value: 240,
    });

    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    expect(document.body.style.position).not.toBe("fixed");

    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(scrollToSpy).toHaveBeenCalledWith(0, 0);
    expect(document.body.style.position).toBe("fixed");
    expect(document.body.style.top).toBe("0px");
    expect(document.body.style.width).toBe("100%");
    expect(document.body.style.overflow).toBe("hidden");

    // Closing (active-tab re-tap) restores the styles AND scrolls back to
    // the exact offset that was saved BEFORE the drawer opened.
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(document.body.style.position).not.toBe("fixed");
    expect(document.body.style.top).toBe("");
    expect(scrollToSpy).toHaveBeenCalledWith(0, 240);

    scrollToSpy.mockRestore();
  });

  it("unmounting WHILE open still restores the body — no path leaves it stuck fixed", () => {
    const scrollToSpy = vi.spyOn(window, "scrollTo").mockImplementation(() => {});
    Object.defineProperty(window, "scrollY", {
      configurable: true,
      value: 80,
    });

    const { unmount } = render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(document.body.style.position).toBe("fixed");

    unmount();
    expect(document.body.style.position).not.toBe("fixed");
    expect(scrollToSpy).toHaveBeenCalledWith(0, 80);

    scrollToSpy.mockRestore();
  });

  // (Self-QA gate mutation self-check, per the task brief) Removing the
  // restore half of the effect's cleanup — mirrored here by asserting
  // against a build that never runs it — must fail this test; see the
  // PR's own report for the mutate/observe-fail/restore transcript. This
  // test is the one that would catch that regression.
  it("does not leave the body permanently fixed after close (regression guard)", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(document.body.style.position).toBe("");
  });

  it("the backdrop blocks a touchmove from reaching the page (non-passive preventDefault)", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    const backdrop = document.querySelector(
      '[data-act="phone-drawer-backdrop"]',
    )!;
    const event = new Event("touchmove", { cancelable: true, bubbles: true });
    backdrop.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(true);
  });
});

// ── (#2107, #1833) reporting the Machine tab's open state up to the parent
//    (`MachineDrawer.tsx` gates the daemon-load poll on this) ──
describe("PhoneDrawer — onMachineOpenChange (#2107, #1833)", () => {
  it("fires false on mount, true when the Machine tab opens, false again when it closes", () => {
    const onMachineOpenChange = vi.fn();
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
        onMachineOpenChange={onMachineOpenChange}
      />,
    );
    expect(onMachineOpenChange).toHaveBeenLastCalledWith(false);

    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(onMachineOpenChange).toHaveBeenLastCalledWith(true);

    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    ); // closes
    expect(onMachineOpenChange).toHaveBeenLastCalledWith(false);
  });

  it('fires false when the Events tab is open — the Machine tab specifically must be active, not just "the drawer"', () => {
    const onMachineOpenChange = vi.fn();
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
        onMachineOpenChange={onMachineOpenChange}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(onMachineOpenChange).toHaveBeenLastCalledWith(true);

    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-events"]')!,
    ); // switches, stays open
    expect(onMachineOpenChange).toHaveBeenLastCalledWith(false);
  });

  it("omitting the callback entirely is safe (optional prop)", () => {
    render(
      <PhoneDrawer
        machineTab={NOOP_MACHINE_TAB}
        events={NO_EVENTS}
        liveStatus="live"
        route={{ kind: "fleet" }}
      />,
    );
    expect(() =>
      fireEvent.click(
        document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
      ),
    ).not.toThrow();
  });
});

// ── (#2108, operator finding — real device screenshot) stylesheet-content
//    tests. jsdom performs no real layout, so these read the actual CSS
//    source rather than a jsdom-computed style — the genuinely verifiable
//    claim for each of these five phone-sheet fixes. ──────────────────────
describe("PhoneDrawer — stylesheet checks (#2108)", () => {
  it("the backdrop DIMS the page (a real scrim, not transparent) and starts below the masthead", () => {
    const css = readStylesheet();
    const rule = ruleBody(css, ".phone-drawer__backdrop");
    expect(rule).not.toMatch(/background:\s*transparent/);
    expect(rule).toMatch(/background:\s*rgba\(0,\s*0,\s*0,\s*0\.45\)/);
    // Starts at a live-measured offset (clears the masthead), with `64px`
    // as its fallback for a pre-measurement first paint — not `inset: 0`,
    // and no longer a bare hardcoded `64px` (#2108 review finding 4/nit
    // 13(f): the real masthead measures ~61px via `--masthead-h`,
    // App.tsx's own `ResizeObserver`-fed custom property).
    expect(rule).toMatch(/top:\s*var\(--masthead-h,\s*64px\)/);
    expect(rule).not.toMatch(/inset:\s*0/);
  });

  it("the backdrop fades in via a keyframe animation (plays on mount, unlike a transition)", () => {
    const css = readStylesheet();
    const rule = ruleBody(css, ".phone-drawer__backdrop");
    expect(rule).toMatch(/animation:\s*phone-drawer-backdrop-in/);
    expect(css).toMatch(/@keyframes phone-drawer-backdrop-in/);
  });

  it("the sheet body reserves padding for the iOS home-indicator safe area", () => {
    const css = readStylesheet();
    const rule = ruleBody(css, ".phone-drawer__body");
    // (#2108, operator finding — round 4, item 4) 16px -> 12px, the
    // operator's own literal figure ("pane content padding-bottom =
    // env(safe-area-inset-bottom) + 12px").
    expect(rule).toMatch(/padding-bottom:\s*calc\(12px \+ env\(safe-area-inset-bottom, 0px\)\)/);
  });

  it("the detail pane's font-size is bumped to >= 14px on the narrow render, not the app's smaller default", () => {
    const css = readStylesheet();
    const match = css.match(/@media \(max-width: 768px\) \{\s*\.rv \{\s*font-size:\s*(\d+)px;\s*line-height:\s*([\d.]+);/);
    expect(match, "the phone-width .rv font-size override must exist").not.toBeNull();
    expect(Number(match![1])).toBeGreaterThanOrEqual(14);
    expect(Number(match![2])).toBeGreaterThanOrEqual(1.4);
  });

  it("the sheet root and its tab bar resolve the SAME background token, distinct from the page background token", () => {
    const css = readStylesheet();
    const sheetRule = ruleBody(css, ".phone-drawer");
    expect(sheetRule).toMatch(/background:\s*var\(--surface\)/);
    // `.phone-drawer__bar` carries no background of its OWN any more — it
    // shows the sheet's `--surface` through it, which is the actual fix
    // (one elevated surface for the whole card, not two competing ones).
    const barMatch = css.match(/\.phone-drawer__bar\s*\{([^}]*)\}/);
    expect(barMatch, "the phone-drawer__bar rule must exist").not.toBeNull();
    expect(barMatch![1]).not.toMatch(/background:/);
    // `--surface` is genuinely a DIFFERENT token from `--bg` (the page
    // background), not an alias — the whole point of the fix.
    expect(css).toMatch(/--surface:\s*#[0-9a-fA-F]+;/);
    const bgMatch = css.match(/--bg:\s*(#[0-9a-fA-F]+);/);
    const surfaceMatch = css.match(/--surface:\s*(#[0-9a-fA-F]+);/);
    expect(bgMatch![1]).not.toBe(surfaceMatch![1]);
  });
});
