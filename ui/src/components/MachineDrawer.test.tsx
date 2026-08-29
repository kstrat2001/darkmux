import { describe, it, expect, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { MachineDrawer } from "./MachineDrawer";
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

describe("MachineDrawer (#2107)", () => {
  it("renders the pill with the live GPU value, closed by default", () => {
    const rolling = [proc("2026-01-01T00:19:00Z", 10, 68, 20)];
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={rolling} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} />);
    expect(screen.getByText(/GPU 68%/)).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("opens the dialog on pill click and shows all three meters", () => {
    const rolling = [proc("2026-01-01T00:19:00Z", 10, 68, 20)];
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={rolling} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} />);
    fireEvent.click(screen.getByText(/GPU 68%/));
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(screen.getByText("CPU")).toBeInTheDocument();
    expect(screen.getByText("GPU")).toBeInTheDocument();
    expect(screen.getByText("MEM")).toBeInTheDocument();
    expect(screen.getByText("last 10 min")).toBeInTheDocument();
  });

  it("closes on the close button", () => {
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} />);
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    fireEvent.click(screen.getByLabelText("Close"));
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closes on backdrop click", () => {
    const { container } = render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} />);
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    fireEvent.click(container.querySelector(".machine-drawer__backdrop")!);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closes on Escape", () => {
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} />);
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closes on a downward swipe of the handle", () => {
    const { container } = render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} />);
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    const handle = container.querySelector('[data-act="machine-drawer-handle"]')!;
    fireEvent.touchStart(handle, { touches: [{ clientY: 100 }] });
    fireEvent.touchEnd(handle, { changedTouches: [{ clientY: 200 }] });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("an upward or negligible swipe does not close the sheet", () => {
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} />);
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    const handle = document.querySelector('[data-act="machine-drawer-handle"]')!;
    fireEvent.touchStart(handle, { touches: [{ clientY: 200 }] });
    fireEvent.touchEnd(handle, { changedTouches: [{ clientY: 190 }] });
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("remembers open state across mounts via localStorage", () => {
    const { unmount } = render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} />);
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    unmount();
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} nowMsOverride={NOW} liveMachines={new Map()} specs={null} />);
    expect(screen.getByRole("dialog")).toBeInTheDocument();
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
        nowMsOverride={NOW}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    expect(screen.getByText(/MacBook-Pro/)).toBeInTheDocument();
    expect(screen.getByText(/M5 Max/)).toBeInTheDocument();
    expect(screen.getByText(/darkmux 3\.3\.0 \(abc1234\)/)).toBeInTheDocument();
    document.head.removeChild(meta);
  });

  it("(#2107) omits the header line entirely when nothing is known yet, rather than rendering an empty row", () => {
    render(<MachineDrawer route={{ kind: "fleet" }} routeRecords={[]} flowWindow={[]} localUid={null} liveMachines={new Map()} specs={null} nowMsOverride={NOW} />);
    fireEvent.click(screen.getByRole("button", { name: /machine ·/ }));
    expect(document.querySelector(".machine-drawer__identity")).toBeNull();
  });
});
