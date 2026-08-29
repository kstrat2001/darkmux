import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { PhoneDrawer } from "./PhoneDrawer";
import type { FlowRecord } from "../types/handwritten";

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

  it("dragging the handle up resizes the open sheet and persists the height for the ACTIVE tab only", () => {
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
    const body = document.querySelector(
      '[data-act="phone-drawer-body"]',
    ) as HTMLElement;
    expect(body.style.height).not.toBe("50vh"); // moved off the default
    expect(
      window.localStorage.getItem("dmux.phone-drawer.height.machine"),
    ).not.toBeNull();
    expect(
      window.localStorage.getItem("dmux.phone-drawer.height.events"),
    ).toBeNull();
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
    // Open at the default 50vh, then drag DOWN past the close-snap floor.
    drag(handle, 200, 900);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("re-opening a tab restores its own previously persisted height, independent of the other tab", () => {
    window.localStorage.setItem("dmux.phone-drawer.height.machine", "30");
    window.localStorage.setItem("dmux.phone-drawer.height.events", "80");
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
    let body = document.querySelector(
      '[data-act="phone-drawer-body"]',
    ) as HTMLElement;
    expect(body.style.height).toBe("30vh");
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-events"]')!,
    );
    body = document.querySelector(
      '[data-act="phone-drawer-body"]',
    ) as HTMLElement;
    expect(body.style.height).toBe("80vh");
  });

  it("Events tab mounts EventLogColumn in pushDetail mode — selecting a record pushes a back-button detail screen", () => {
    const events = { ...NO_EVENTS, records: [record(1)] };
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
    fireEvent.click(document.querySelector('[data-act="rec"]')!);
    expect(
      document.querySelector('[data-act="eventlog-pushed"]'),
    ).not.toBeNull();
    expect(document.querySelector('[data-act="eventlog-back"]')).not.toBeNull();
    fireEvent.click(document.querySelector('[data-act="eventlog-back"]')!);
    expect(document.querySelector('[data-act="eventlog-pushed"]')).toBeNull();
    expect(document.querySelector('[data-act="rec"]')).not.toBeNull();
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
