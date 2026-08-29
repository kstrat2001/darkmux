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
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={rolling} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} liveStatus="live" />);
    expect(screen.getByText(/GPU 68%/)).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("opens the dialog on pill click and shows all three meters", () => {
    const rolling = [proc("2026-01-01T00:19:00Z", 10, 68, 20)];
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={rolling} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} liveStatus="live" />);
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
  // those gestures are a PHONE-ONLY concept now, already covered by the
  // "phone chrome" describe block below. Likewise "remembers open state
  // across mounts via localStorage" is gone, not failing-and-ignored:
  // `dialogManager`'s store has never persisted across a page load (no
  // other dialog in this app does either), so a stats panel reopening
  // itself on every fresh load would be the one exception — dropped
  // deliberately, see `MachineDrawer.tsx`'s own module doc.
  it("closes on the close button", () => {
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} liveStatus="live" />);
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    fireEvent.click(document.querySelector("#imodalbg .dialog__close")!);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closes on backdrop click", () => {
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} liveStatus="live" />);
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    fireEvent.click(document.getElementById("imodalbg")!);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closes on Escape", () => {
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} liveStatus="live" />);
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("clicking the pill again while open closes it (toggle)", () => {
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} liveStatus="live" />);
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
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} liveMachines={new Map()} specs={null} liveStatus="live" nowMsOverride={NOW} />);
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    expect(document.querySelector(".machine-drawer__identity")).toBeNull();
  });

});

// ── (phone feedback, 2026-08-29) idle state + last-known ─────────────────

describe("MachineDrawer — idle state (no samples)", () => {
  it("shows an idle line and no meters when the rolling window has no samples at all", () => {
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} liveMachines={new Map()} specs={null} liveStatus="live" nowMsOverride={NOW} />);
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
      <MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[oldSample]} localUid={null} liveMachines={new Map()} specs={null} liveStatus="live" nowMsOverride={NOW} />,
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
        specs={null} liveStatus="live"
        nowMsOverride={NOW}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    expect(screen.queryByText(/idle ·/)).toBeNull();
    expect(document.querySelector(".meter-row")).not.toBeNull();
  });
});

// ── (phone feedback, 2026-08-29) the phone bottom bar + drag-to-expand ───

describe("MachineDrawer — phone chrome (isMobileOverride)", () => {
  it("renders the bottom bar instead of the pill on a phone, closed by default", () => {
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={[proc("2026-01-01T00:19:00Z", 10, 68, 20)]}
        localUid={null}
        liveMachines={new Map()}
        specs={null} liveStatus="live"
        nowMsOverride={NOW}
        isMobileOverride={true}
      />,
    );
    expect(document.querySelector('[data-act="machine-bottombar"]')).not.toBeNull();
    expect(document.querySelector('[data-act="machine-drawer-pill"]')).toBeNull();
    expect(screen.getByText(/GPU 68%/)).toBeInTheDocument();
  });

  it("tapping the bar opens the sheet, and the bar disappears while open", () => {
    render(
      <MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} liveMachines={new Map()} specs={null} liveStatus="live" nowMsOverride={NOW} isMobileOverride={true} />,
    );
    fireEvent.click(document.querySelector('[data-act="machine-bottombar"]')!);
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(document.querySelector('[data-act="machine-bottombar"]')).toBeNull();
  });

  it("a swipe up on the closed bar opens the sheet", () => {
    render(
      <MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} liveMachines={new Map()} specs={null} liveStatus="live" nowMsOverride={NOW} isMobileOverride={true} />,
    );
    const bar = document.querySelector('[data-act="machine-bottombar"]')!;
    fireEvent.touchStart(bar, { touches: [{ clientY: 200 }] });
    fireEvent.touchEnd(bar, { changedTouches: [{ clientY: 150 }] });
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("dragging the open sheet's handle up past the threshold snaps to the full-height class", () => {
    render(
      <MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} liveMachines={new Map()} specs={null} liveStatus="live" nowMsOverride={NOW} isMobileOverride={true} />,
    );
    fireEvent.click(document.querySelector('[data-act="machine-bottombar"]')!);
    const handle = document.querySelector('[data-act="machine-drawer-handle"]')!;
    fireEvent.touchStart(handle, { touches: [{ clientY: 300 }] });
    fireEvent.touchMove(handle, { touches: [{ clientY: 250 }] }); // 50px up
    expect(document.querySelector(".machine-drawer--full")).not.toBeNull();
  });

  it("closing and reopening resets the full-height snap", () => {
    render(
      <MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} liveMachines={new Map()} specs={null} liveStatus="live" nowMsOverride={NOW} isMobileOverride={true} />,
    );
    fireEvent.click(document.querySelector('[data-act="machine-bottombar"]')!);
    const handle = document.querySelector('[data-act="machine-drawer-handle"]')!;
    fireEvent.touchStart(handle, { touches: [{ clientY: 300 }] });
    fireEvent.touchMove(handle, { touches: [{ clientY: 250 }] });
    expect(document.querySelector(".machine-drawer--full")).not.toBeNull();
    fireEvent.click(screen.getByLabelText("Close"));
    fireEvent.click(document.querySelector('[data-act="machine-bottombar"]')!);
    expect(document.querySelector(".machine-drawer--full")).toBeNull();
  });
});
