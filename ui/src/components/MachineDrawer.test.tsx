import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { MachineDrawer } from "./MachineDrawer";
import { closeOpenModal } from "../lib/dialogManager";
import type { FlowRecord } from "../types/handwritten";

const proc = (ts: string, cpu: number, gpu: number, mem: number): FlowRecord => ({
  ts,
  category: "telemetry",
  source: "process",
  action: "telemetry.process",
  payload: { cpu, gpu, mem },
});

const NOW = Date.parse("2026-01-01T00:20:00Z");

/** (#2107 tabbed-drawer packet) `MachineDrawer` now also carries the
 * Events tab's props through to the phone drawer — irrelevant to every
 * desktop-only test in this file, so a shared empty default keeps those
 * unchanged rather than repeating five extra props at every call site. */
const EMPTY_EVENTLOG = {
  eventLogRecords: [] as FlowRecord[],
  eventLogScopeLabel: "fleet",
  eventLogVisible: true,
  eventLogLoading: false,
  eventLogError: null,
  eventLogHistorical: false,
};

beforeEach(() => {
  window.localStorage.clear();
});

afterEach(() => {
  // A test that injects a `darkmux-version` meta and throws before its own
  // cleanup line would otherwise leak that meta into every later test in
  // this file — belt-and-braces alongside each test's own removal.
  document.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
  // dialogManager's open/close state outlives `render()`/unmount (same
  // reason `Masthead.test.tsx` resets it) — this file's desktop tests now
  // drive the SAME shared store `Masthead.tsx`'s ⓘ does.
  closeOpenModal({ restore: false });
});

describe("MachineDrawer (#2107)", () => {
  it("renders the pill with the live GPU value, closed by default", () => {
    const rolling = [proc("2026-01-01T00:19:00Z", 10, 68, 20)];
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={rolling}
        localUid={null}
        nowMsOverride={NOW}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        {...EMPTY_EVENTLOG}
      />,
    );
    expect(screen.getByText(/GPU 68%/)).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("opens the dialog on pill click and shows all three meters", () => {
    const rolling = [proc("2026-01-01T00:19:00Z", 10, 68, 20)];
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={rolling}
        localUid={null}
        nowMsOverride={NOW}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(screen.getByText(/GPU 68%/));
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(screen.getByText("CPU")).toBeInTheDocument();
    expect(screen.getByText("GPU")).toBeInTheDocument();
    expect(screen.getByText("MEM")).toBeInTheDocument();
    expect(screen.getByText("last 10 min")).toBeInTheDocument();
  });

  // (#2107 "one modal" packet) Desktop's open/closed now lives entirely in
  // `dialogManager` — the SAME shared `<Dialog id="imodalbg">` shell
  // Filters/Notes use, not this component's own bespoke backdrop/handle.
  // That is why "closes on a downward swipe of the handle" and "an upward
  // or negligible swipe does not close the sheet" (desktop versions) are
  // GONE rather than updated: desktop has no handle to swipe any more —
  // those gestures are a PHONE-ONLY concept now, covered by
  // `PhoneDrawer.test.tsx` (#2107 tabbed-drawer packet). Likewise
  // "remembers open state across mounts via localStorage" is gone, not
  // failing-and-ignored: `dialogManager`'s store has never persisted
  // across a page load (no other dialog in this app does either), so a
  // stats panel reopening itself on every fresh load would be the one
  // exception — dropped deliberately, see `MachineDrawer.tsx`'s own
  // module doc.
  it("closes on the close button", () => {
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={[]}
        localUid={null}
        nowMsOverride={NOW}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    fireEvent.click(document.querySelector("#imodalbg .dialog__close")!);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closes on backdrop click", () => {
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={[]}
        localUid={null}
        nowMsOverride={NOW}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    fireEvent.click(document.getElementById("imodalbg")!);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closes on Escape", () => {
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={[]}
        localUid={null}
        nowMsOverride={NOW}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("clicking the pill again while open closes it (toggle)", () => {
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={[]}
        localUid={null}
        nowMsOverride={NOW}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        {...EMPTY_EVENTLOG}
      />,
    );
    const pill = screen.getByRole("button", { name: /machine ·/ });
    fireEvent.click(pill);
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    fireEvent.click(pill);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("scopes to the mission's own samples and label when on a mission route", () => {
    const missionRecords = [proc("2026-01-01T00:00:00Z", 30, 55, 40)];
    render(
      <MachineDrawer
        route={{ kind: "mission", missionId: "m1" }}
        routeRecords={missionRecords}
        flowWindow={[]}
        localUid={null}
        nowMsOverride={NOW}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    expect(screen.getByText("this mission")).toBeInTheDocument();
    expect(screen.getByText(/GPU 55%/)).toBeInTheDocument();
  });

  it("(#2107) the header line carries machine name · hardware · darkmux version — the phone's only route to that info", () => {
    const meta = document.createElement("meta");
    meta.name = "darkmux-version";
    meta.content = "3.3.0 (abc1234)";
    document.head.appendChild(meta);
    const flowWindow = [{ ts: "2026-01-01T00:00:00Z", machine_uid: "self-uid", machine_id: "MacBook-Pro" }];
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={flowWindow}
        localUid="self-uid"
        liveMachines={new Map()}
        specs={{
          darkmux_version: "3.3.0 (abc1234)",
          flow_schema_version: "1.27.0",
          machine_id: "MacBook-Pro",
          os: "macOS",
          ram_total_bytes: 137438953472,
          ram_free_for_ai_bytes: null,
          cpu_brand: "M5 Max",
          loaded_models: [],
          lms_unreachable: false,
          utility_model: null,
          redis_url_redacted: null,
          generated_at_ms: NOW,
        }}
        liveStatus="live"
        nowMsOverride={NOW}
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    // Scoped to the identity line specifically — "MacBook-Pro" and
    // "M5 Max" also appear in the about section's own machine/hardware
    // rows below it, which would make an unscoped `getByText` ambiguous.
    const identity = document.querySelector(".machine-drawer__identity")!;
    expect(identity.textContent).toBe("MacBook-Pro · M5 Max · 128 GB · darkmux 3.3.0 (abc1234)");
  });

  it("(#2107) omits the header line entirely when nothing is known yet, rather than rendering an empty row", () => {
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={[]}
        localUid={null}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        nowMsOverride={NOW}
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    expect(document.querySelector(".machine-drawer__identity")).toBeNull();
  });
});

// ── (phone feedback, 2026-08-29) idle state + last-known ─────────────────

describe("MachineDrawer — idle state (no samples)", () => {
  it("shows an idle line and no meters when the rolling window has no samples at all", () => {
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={[]}
        localUid={null}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        nowMsOverride={NOW}
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    expect(screen.getByText("idle · no samples in the last 10 min")).toBeInTheDocument();
    expect(document.querySelector(".meter-row")).toBeNull();
  });

  it("shows the last known reading and its age when the window is empty but something was seen earlier", () => {
    const oldSample: FlowRecord = {
      ts: new Date(NOW - 60 * 60_000).toISOString(), // 1h before NOW
      category: "telemetry",
      source: "process",
      action: "telemetry.process",
      payload: { cpu: 40, gpu: 55, mem: 30 },
    };
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={[oldSample]}
        localUid={null}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        nowMsOverride={NOW}
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    expect(screen.getByText(/last sample 1h ago/)).toBeInTheDocument();
    expect(screen.getByText(/CPU 40%/)).toBeInTheDocument();
    expect(screen.getByText(/GPU 55%/)).toBeInTheDocument();
    expect(screen.getByText(/MEM 30%/)).toBeInTheDocument();
  });

  it("a mission with real samples never shows the idle line", () => {
    const missionRecords = [proc("2026-01-01T00:00:00Z", 30, 55, 40)];
    render(
      <MachineDrawer
        route={{ kind: "mission", missionId: "m1" }}
        routeRecords={missionRecords}
        flowWindow={[]}
        localUid={null}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        nowMsOverride={NOW}
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    expect(screen.queryByText(/idle ·/)).toBeNull();
    expect(document.querySelector(".meter-row")).not.toBeNull();
  });
});

// ── (#2107 tabbed-drawer packet) the phone skin delegates to PhoneDrawer ──
//
// The tab/drag/height mechanics themselves are `PhoneDrawer.test.tsx`'s job
// (that component is decoupled from `MachineDrawer`'s data-fetching, so it
// is tested directly with plain pre-built props). What belongs HERE is the
// wiring: on a phone, `MachineDrawer` renders `PhoneDrawer` instead of the
// pill/dialog, and hands it the SAME machine-stats content the desktop
// dialog would have shown, plus the `eventLog*` props threaded through.

describe("MachineDrawer — phone skin delegates to PhoneDrawer (isMobileOverride)", () => {
  it("renders the phone drawer's bar (not the desktop pill) with the live compact numbers", () => {
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={[proc("2026-01-01T00:19:00Z", 10, 68, 20)]}
        localUid={null}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        nowMsOverride={NOW}
        isMobileOverride={true}
        {...EMPTY_EVENTLOG}
      />,
    );
    expect(document.querySelector('[data-act="phone-drawer-bar"]')).not.toBeNull();
    expect(document.querySelector('[data-act="machine-drawer-pill"]')).toBeNull();
    expect(screen.getByText(/GPU 68%/)).toBeInTheDocument();
  });

  it("tapping the Machine tab opens the drawer to the SAME stats content the desktop dialog renders", () => {
    const rolling = [proc("2026-01-01T00:19:00Z", 10, 68, 20)];
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={rolling}
        localUid={null}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        nowMsOverride={NOW}
        isMobileOverride={true}
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(document.querySelector('[data-act="phone-drawer-tab-machine"]')!);
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(screen.getByText("CPU")).toBeInTheDocument();
    expect(screen.getByText("last 10 min")).toBeInTheDocument();
  });

  it("tapping the Events tab mounts the EventLogColumn with the records handed down from App", () => {
    const records: FlowRecord[] = [
      { ts: "2026-01-01T00:00:00Z", category: "note", source: "operator", action: "note", handle: "hello" },
    ];
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={[]}
        localUid={null}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        nowMsOverride={NOW}
        isMobileOverride={true}
        eventLogRecords={records}
        eventLogScopeLabel="fleet"
        eventLogVisible={true}
        eventLogLoading={false}
        eventLogError={null}
        eventLogHistorical={false}
      />,
    );
    fireEvent.click(document.querySelector('[data-act="phone-drawer-tab-events"]')!);
    expect(document.querySelector(".eventlog")).not.toBeNull();
    expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(1);
    // Only ONE events pane exists — `MachineDrawer` never ALSO renders the
    // desktop pill/dialog while in the phone skin.
    expect(document.querySelector('[data-act="machine-drawer-pill"]')).toBeNull();
  });

  it("desktop skin never mounts the phone drawer bar", () => {
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={[]}
        localUid={null}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        nowMsOverride={NOW}
        isMobileOverride={false}
        {...EMPTY_EVENTLOG}
      />,
    );
    expect(document.querySelector('[data-act="phone-drawer-bar"]')).toBeNull();
    expect(document.querySelector('[data-act="machine-drawer-pill"]')).not.toBeNull();
  });
});
