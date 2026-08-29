import { describe, it, expect, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { PhoneDrawer } from "./PhoneDrawer";
import type { FlowRecord } from "../types/handwritten";

const NOOP_MACHINE_TAB = { compactLine: "CPU 34% · GPU 68% · MEM 62%", body: <div data-act="stub-machine-body">stats</div> };

function record(i: number): FlowRecord {
  return { ts: `2026-01-01T00:0${i}:00Z`, category: "note", source: "operator", action: "note", handle: `rec-${i}` };
}

const NO_EVENTS = { records: [] as FlowRecord[], scopeLabel: "fleet", visible: true, loading: false, error: null, historical: false, serverTruncated: false };

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
    render(<PhoneDrawer machineTab={NOOP_MACHINE_TAB} events={NO_EVENTS} liveStatus="live" route={{ kind: "fleet" }} />);
    expect(document.querySelector('[data-act="phone-drawer-bar"]')).not.toBeNull();
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByText(/CPU 34% · GPU 68% · MEM 62%/)).toBeInTheDocument();
    expect(screen.getByText(/Events · 0/)).toBeInTheDocument();
  });

  it("tapping the Machine tab opens the drawer to the Machine panel", () => {
    render(<PhoneDrawer machineTab={NOOP_MACHINE_TAB} events={NO_EVENTS} liveStatus="live" route={{ kind: "fleet" }} />);
    fireEvent.click(document.querySelector('[data-act="phone-drawer-tab-machine"]')!);
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(document.querySelector('[data-act="phone-drawer-panel-machine"]')).not.toBeNull();
    expect(document.querySelector('[data-act="stub-machine-body"]')).not.toBeNull();
  });

  it("tapping the ACTIVE tab again closes the drawer", () => {
    render(<PhoneDrawer machineTab={NOOP_MACHINE_TAB} events={NO_EVENTS} liveStatus="live" route={{ kind: "fleet" }} />);
    const machineTab = document.querySelector('[data-act="phone-drawer-tab-machine"]')!;
    fireEvent.click(machineTab);
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    fireEvent.click(machineTab);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("tapping the OTHER tab while open switches tabs without closing", () => {
    const events = { ...NO_EVENTS, records: [record(1), record(2)] };
    render(<PhoneDrawer machineTab={NOOP_MACHINE_TAB} events={events} liveStatus="live" route={{ kind: "fleet" }} />);
    fireEvent.click(document.querySelector('[data-act="phone-drawer-tab-machine"]')!);
    expect(document.querySelector('[data-act="phone-drawer-panel-machine"]')).not.toBeNull();
    fireEvent.click(document.querySelector('[data-act="phone-drawer-tab-events"]')!);
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(document.querySelector('[data-act="phone-drawer-panel-machine"]')).toBeNull();
    expect(document.querySelector(".eventlog")).not.toBeNull();
    expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(2);
  });

  it("tapping the handle closes the open drawer", () => {
    render(<PhoneDrawer machineTab={NOOP_MACHINE_TAB} events={NO_EVENTS} liveStatus="live" route={{ kind: "fleet" }} />);
    fireEvent.click(document.querySelector('[data-act="phone-drawer-tab-machine"]')!);
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    const handle = document.querySelector('[data-act="phone-drawer-handle"]')!;
    fireEvent.pointerDown(handle, { clientY: 400, pointerId: 1 });
    fireEvent.pointerUp(handle, { clientY: 400, pointerId: 1 }); // no movement — a tap
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("Escape closes the open drawer", () => {
    render(<PhoneDrawer machineTab={NOOP_MACHINE_TAB} events={NO_EVENTS} liveStatus="live" route={{ kind: "fleet" }} />);
    fireEvent.click(document.querySelector('[data-act="phone-drawer-tab-machine"]')!);
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("dragging the handle up resizes the open sheet and persists the height for the ACTIVE tab only", () => {
    render(<PhoneDrawer machineTab={NOOP_MACHINE_TAB} events={NO_EVENTS} liveStatus="live" route={{ kind: "fleet" }} />);
    fireEvent.click(document.querySelector('[data-act="phone-drawer-tab-machine"]')!);
    const handle = document.querySelector('[data-act="phone-drawer-handle"]')!;
    drag(handle, 600, 200); // drag up 400px — well past the tap threshold
    const body = document.querySelector('[data-act="phone-drawer-body"]') as HTMLElement;
    expect(body.style.height).not.toBe("50vh"); // moved off the default
    expect(window.localStorage.getItem("dmux.phone-drawer.height.machine")).not.toBeNull();
    expect(window.localStorage.getItem("dmux.phone-drawer.height.events")).toBeNull();
  });

  it("a drag that ends near the closed position snaps the drawer shut instead of leaving a sliver", () => {
    render(<PhoneDrawer machineTab={NOOP_MACHINE_TAB} events={NO_EVENTS} liveStatus="live" route={{ kind: "fleet" }} />);
    fireEvent.click(document.querySelector('[data-act="phone-drawer-tab-machine"]')!);
    const handle = document.querySelector('[data-act="phone-drawer-handle"]')!;
    // Open at the default 50vh, then drag DOWN past the close-snap floor.
    drag(handle, 200, 900);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("re-opening a tab restores its own previously persisted height, independent of the other tab", () => {
    window.localStorage.setItem("dmux.phone-drawer.height.machine", "30");
    window.localStorage.setItem("dmux.phone-drawer.height.events", "80");
    render(<PhoneDrawer machineTab={NOOP_MACHINE_TAB} events={NO_EVENTS} liveStatus="live" route={{ kind: "fleet" }} />);
    fireEvent.click(document.querySelector('[data-act="phone-drawer-tab-machine"]')!);
    let body = document.querySelector('[data-act="phone-drawer-body"]') as HTMLElement;
    expect(body.style.height).toBe("30vh");
    fireEvent.click(document.querySelector('[data-act="phone-drawer-tab-events"]')!);
    body = document.querySelector('[data-act="phone-drawer-body"]') as HTMLElement;
    expect(body.style.height).toBe("80vh");
  });

  it("Events tab mounts EventLogColumn in pushDetail mode — selecting a record pushes a back-button detail screen", () => {
    const events = { ...NO_EVENTS, records: [record(1)] };
    render(<PhoneDrawer machineTab={NOOP_MACHINE_TAB} events={events} liveStatus="live" route={{ kind: "fleet" }} />);
    fireEvent.click(document.querySelector('[data-act="phone-drawer-tab-events"]')!);
    fireEvent.click(document.querySelector('[data-act="rec"]')!);
    expect(document.querySelector('[data-act="eventlog-pushed"]')).not.toBeNull();
    expect(document.querySelector('[data-act="eventlog-back"]')).not.toBeNull();
    fireEvent.click(document.querySelector('[data-act="eventlog-back"]')!);
    expect(document.querySelector('[data-act="eventlog-pushed"]')).toBeNull();
    expect(document.querySelector('[data-act="rec"]')).not.toBeNull();
  });

  it("the Events tab's connection dot reflects a live route's live status", () => {
    render(<PhoneDrawer machineTab={NOOP_MACHINE_TAB} events={NO_EVENTS} liveStatus="reconnecting" route={{ kind: "fleet" }} />);
    expect(document.querySelector(".phone-drawer__dot--reconnecting")).not.toBeNull();
  });

  it("the Events tab's connection dot reads as a replay on a non-live route", () => {
    render(<PhoneDrawer machineTab={NOOP_MACHINE_TAB} events={NO_EVENTS} liveStatus="live" route={{ kind: "playback", date: "2026-01-01" }} />);
    expect(document.querySelector(".phone-drawer__dot--replay")).not.toBeNull();
  });
});
