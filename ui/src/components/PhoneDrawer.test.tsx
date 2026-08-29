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

function record(i: number): FlowRecord {
  return {
    ts: `2026-01-01T00:0${i}:00Z`,
    category: "note",
    source: "operator",
    action: "note",
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
    expect(sheet.style.height).toBe("30vh");
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-events"]')!,
    );
    sheet = document.querySelector(
      '[data-act="phone-drawer"]',
    ) as HTMLElement;
    // (#2108, operator's core ask) Switching tabs while open must NEVER
    // change the sheet's height — one shared value, not a per-tab one.
    expect(sheet.style.height).toBe("30vh");
  });

  // (#2108, operator correction — reverted from the earlier "every row
  // expanded inline" attempt, which was unreadable) The Events tab is the
  // SAME list + detail-pane split the desktop events column already has.
  // Tapping a row shows that record in the pane, right here in the sheet
  // — no route change, no drill-in navigation, no separate card. The pane
  // is empty/placeholder until a row is tapped.
  it("the pane is empty/placeholder until a row is tapped (no records yet)", () => {
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
    expect(document.querySelector(".eventlog__none")).not.toBeNull();
  });

  it("tapping a row populates the detail pane inside the sheet — no per-row expanded payload in the list", () => {
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
    // `follow` mode (the desktop pane's own default) already shows the
    // newest record's detail — tapping the OLDER row is the real proof
    // that a tap changes what the pane shows, in the sheet.
    fireEvent.click(rows[1]);
    const pane = document.querySelector(".eventlog__detailbody")!;
    // `RecordView`'s own key rendering replaces underscores with spaces
    // (`Row`'s `.rv__key`) — asserting on ITS actual output, not a guess.
    expect(pane.textContent).toContain("tool name");
    expect(pane.textContent).toContain("grep");
    expect(pane.textContent).toContain("args chars");
    // Confirms the pane actually SWITCHED to the tapped (older) record,
    // not merely appended to whatever `follow` mode had shown before.
    expect(pane.textContent).not.toContain("rec-2");
    expect(pane.textContent).toContain("rec-1");
    // In the sheet — no navigation, no push screen, no hash change.
    expect(document.querySelector('[data-act="eventlog-pushed"]')).toBeNull();
    expect(window.location.hash).toBe("");
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
    const vh = parseFloat(sheet.style.height);
    expect(vh).toBeGreaterThanOrEqual(85);
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

  it("opening pins the body at its current scroll offset (position: fixed, negative top) and restores + scrolls back on close", () => {
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
    expect(document.body.style.position).toBe("fixed");
    expect(document.body.style.top).toBe("-240px");
    expect(document.body.style.width).toBe("100%");
    expect(document.body.style.overflow).toBe("hidden");

    // Closing (active-tab re-tap) restores the styles AND scrolls back to
    // the exact offset that was saved on open.
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
    // Starts at a fixed offset (clears the masthead), not `inset: 0`.
    expect(rule).toMatch(/top:\s*64px/);
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
    expect(rule).toMatch(/padding-bottom:\s*calc\(16px \+ env\(safe-area-inset-bottom, 0px\)\)/);
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
