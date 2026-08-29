import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  render as rtlRender,
  screen,
  fireEvent,
  waitFor,
} from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MachineDrawer } from "./MachineDrawer";
import { closeOpenModal } from "../lib/dialogManager";
import type { FlowRecord } from "../types/handwritten";

const proc = (
  ts: string,
  cpu: number,
  gpu: number,
  mem: number,
): FlowRecord => ({
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

/** The pill/tab no longer carries a live label (operator finding: "looks
 * too busy") — both surfaces read this static string at rest AND while
 * open; see `MachineDrawer.tsx`/`PhoneDrawer.tsx`'s own doc. */
const MACHINE_INFO_LABEL = "Machine info";

/** (#2107, #1833) `useMachineStatsContent` now calls `useDaemonLoad`
 * (`hooks/useDaemonLoad.ts`), which needs a `QueryClientProvider` in the
 * tree — every pre-existing `render(<MachineDrawer .../>)` call in this
 * file predates that and would otherwise throw "No QueryClient set".
 * Shadowing `render` here (rather than touching all 17 call sites) keeps
 * every existing test's body unchanged; only the import binding differs
 * (`rtlRender` for the real one). Mirrors `MachineLens.test.tsx`'s own
 * `renderMachine` helper in spirit, just named to slot in as a drop-in
 * replacement instead of a differently-named wrapper function. */
function render(ui: React.ReactElement) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return rtlRender(
    <QueryClientProvider client={queryClient}>{ui}</QueryClientProvider>,
  );
}

beforeEach(() => {
  window.localStorage.clear();
  // No daemon to poll in these tests — `useDaemonLoad`'s live query would
  // otherwise attempt a real `fetch("/machine/resources")` against jsdom's
  // no-origin environment on every render. Stubbed to fail fast and
  // deterministically (mirrors the pre-#2107 "no daemon load" baseline
  // every existing test in this file was written against — polling is
  // gated on `isOpen` now, but a test that DOES open the surface would
  // otherwise still race a real network attempt), rather than relying on
  // jsdom's URL-parse failure shape. The dedicated "daemon load" describe
  // block below overrides this per-test with real payloads.
  vi.stubGlobal("fetch", () =>
    Promise.reject(new Error("fetch disabled in MachineDrawer tests")),
  );
});

afterEach(() => {
  vi.unstubAllGlobals();
  // A test that injects a `darkmux-version` meta and throws before its own
  // cleanup line would otherwise leak that meta into every later test in
  // this file — belt-and-braces alongside each test's own removal.
  document
    .querySelectorAll('meta[name^="darkmux-"]')
    .forEach((el) => el.remove());
  // dialogManager's open/close state outlives `render()`/unmount (same
  // reason `Masthead.test.tsx` resets it) — this file's desktop tests now
  // drive the SAME shared store `Masthead.tsx`'s ⓘ does.
  closeOpenModal({ restore: false });
});

function pill() {
  return screen.getByRole("button", { name: MACHINE_INFO_LABEL });
}

describe("MachineDrawer (#2107)", () => {
  it("renders the pill as a static 'Machine info' label, closed by default — no live number even with samples present", () => {
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
    expect(pill()).toBeInTheDocument();
    expect(screen.queryByText(/GPU 68%/)).toBeNull();
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
    fireEvent.click(pill());
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(screen.getByText("CPU")).toBeInTheDocument();
    expect(screen.getByText("GPU")).toBeInTheDocument();
    expect(screen.getByText("MEM")).toBeInTheDocument();
    expect(screen.getByText("last 10 min")).toBeInTheDocument();
    // The rolling sample's own GPU reading, from the dispatch-derived
    // fallback aggregate (no daemon `load` reachable in this file's
    // default fetch stub). The Meter renders `now` as its own text node
    // inside a `[data-meter="gpu"]` wrapper — not a combined "GPU 68%"
    // string — so the scoped query below reads the SAME DOM the operator
    // actually sees.
    expect(
      document.querySelector('[data-meter="gpu"] .meter-now')?.textContent,
    ).toBe("68%");
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
    fireEvent.click(pill());
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
    fireEvent.click(pill());
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
    fireEvent.click(pill());
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
    const p = pill();
    fireEvent.click(p);
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    fireEvent.click(p);
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
    fireEvent.click(pill());
    expect(screen.getByText("this mission")).toBeInTheDocument();
    expect(
      document.querySelector('[data-meter="gpu"] .meter-now')?.textContent,
    ).toBe("55%");
  });

  it("(#2107) the header line carries machine name · hardware · darkmux version — the phone's only route to that info", () => {
    const meta = document.createElement("meta");
    meta.name = "darkmux-version";
    meta.content = "3.3.0 (abc1234)";
    document.head.appendChild(meta);
    const flowWindow = [
      {
        ts: "2026-01-01T00:00:00Z",
        machine_uid: "self-uid",
        machine_id: "MacBook-Pro",
      },
    ];
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
    fireEvent.click(pill());
    // Scoped to the identity line specifically — "MacBook-Pro" and
    // "M5 Max" also appear in the about section's own machine/hardware
    // rows below it, which would make an unscoped `getByText` ambiguous.
    const identity = document.querySelector(".machine-drawer__identity")!;
    expect(identity.textContent).toBe(
      "MacBook-Pro · M5 Max · 128 GB · darkmux 3.3.0 (abc1234)",
    );
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
    fireEvent.click(pill());
    expect(document.querySelector(".machine-drawer__identity")).toBeNull();
  });
});

// ── (phone feedback, 2026-08-29) idle state + last-known ─────────────────

describe("MachineDrawer — idle state (no samples)", () => {
  // (#2107, #1833) With no daemon `load` reachable (stubbed off for this
  // whole file), a non-mission/non-dispatch route's idle wording now says
  // so explicitly rather than the old generic "idle · no samples" line —
  // see `machineStatsContent.tsx`'s own doc on `idleLine`.
  it("shows the daemon-not-sampling idle line and no meters when the rolling window has no samples at all", () => {
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
    fireEvent.click(pill());
    expect(screen.getByText("daemon does not sample yet")).toBeInTheDocument();
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
    fireEvent.click(pill());
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
    fireEvent.click(pill());
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
  it("renders the phone drawer's bar (not the desktop pill) with the static label, not a live number", () => {
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
    expect(
      document.querySelector('[data-act="phone-drawer-bar"]'),
    ).not.toBeNull();
    expect(
      document.querySelector('[data-act="machine-drawer-pill"]'),
    ).toBeNull();
    expect(screen.getByText(MACHINE_INFO_LABEL)).toBeInTheDocument();
    expect(screen.queryByText(/GPU 68%/)).toBeNull();
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
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(screen.getByText("CPU")).toBeInTheDocument();
    expect(screen.getByText("last 10 min")).toBeInTheDocument();
  });

  it("tapping the Events tab mounts the EventLogColumn with the records handed down from App", () => {
    const records: FlowRecord[] = [
      {
        ts: "2026-01-01T00:00:00Z",
        category: "note",
        source: "operator",
        action: "note",
        handle: "hello",
      },
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
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-events"]')!,
    );
    expect(document.querySelector(".eventlog")).not.toBeNull();
    expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(1);
    // Only ONE events pane exists — `MachineDrawer` never ALSO renders the
    // desktop pill/dialog while in the phone skin.
    expect(
      document.querySelector('[data-act="machine-drawer-pill"]'),
    ).toBeNull();
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
    expect(
      document.querySelector('[data-act="machine-drawer-pill"]'),
    ).not.toBeNull();
  });
});

// ── (#2107, #1833) the daemon's continuous host sampler ──
//
// `useDaemonLoad` polls `/machine/resources`' `load` block, gated on
// whether the surface is OPEN — see `hooks/useDaemonLoad.test.tsx` for the
// dedicated closed/open/close polling-cadence proof. This block proves the
// INTEGRATION: opening the dialog actually renders what the daemon reports,
// on the very FIRST resolved poll (the warm-up finding — the viewer never
// accumulates avg/max itself across polls; `load.window` is used verbatim).
describe("MachineDrawer — daemon load block (#2107, #1833)", () => {
  const RESOURCES_WITH_LOAD = {
    schema_version: "1",
    generated_at_ms: 1,
    gather_ms: 1,
    limit_bytes: 1,
    limit_source: "test",
    pool: {
      capacity_bytes: 1,
      used_bytes: 1,
      available_bytes: 1,
      free_bytes: 1,
    },
    pressure: {
      swap_used_bytes: 0,
      compressor_bytes: 0,
      margin_percent: 90,
      red: false,
    },
    models: [],
    machine: {
      potential_bytes: 1,
      unpriced_models: 0,
      current_bytes: 1,
      state: "green",
    },
    attribution: "test",
    messages: [],
    cache_ttl_ms: 2000,
    load: {
      now: { cpu_pct: 12, mem_pct: 34, gpu_pct: 56, sampled_at_ms: 4000 },
      window: {
        cpu: { mean_pct: 10, p95_pct: 15, max_pct: 20 },
        mem: { mean_pct: 30, p95_pct: 35, max_pct: 40 },
        gpu: { mean_pct: 50, p95_pct: 55, max_pct: 60 },
        samples: 3,
        interval_ms: 2000,
        span_ms: 90_000, // 90s — under 10 min, so the label must say "2 min", not "10 min"
      },
      sampler_cost_ms_mean: 4.2,
    },
  };

  function stubDaemonFetch() {
    const fetchMock = vi.fn((url: string) => {
      if (String(url) === "/machine/resources") {
        return Promise.resolve(
          new Response(JSON.stringify(RESOURCES_WITH_LOAD), { status: 200 }),
        );
      }
      return Promise.reject(new Error(`unexpected fetch in this test: ${url}`));
    });
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
  }

  it("closed: zero fetches — the pill shows the static label with no network activity", () => {
    const fetchMock = stubDaemonFetch();
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
    expect(pill()).toBeInTheDocument();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  // (operator warm-up finding) The daemon's ring samples continuously
  // regardless of whether any viewer is watching, so the very FIRST poll
  // after opening already carries the full window reduction — this must
  // NOT require a second poll to show avg/max.
  it("opening triggers exactly one fetch, and avg/max/now all appear on that FIRST resolved poll", async () => {
    const fetchMock = stubDaemonFetch();
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
    fireEvent.click(pill());
    expect(
      screen.queryByText("daemon does not sample yet"),
    ).toBeInTheDocument(); // pre-resolve

    // GPU's own window: now=56, mean(avg)=50, max=60 — all present from
    // the SAME single resolved fetch, no second poll needed.
    await waitFor(() =>
      expect(screen.getByText(/50% avg/)).toBeInTheDocument(),
    );
    expect(screen.getByText("56%")).toBeInTheDocument(); // now (meter-now)
    expect(screen.getByText(/60% max/)).toBeInTheDocument();
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("the scope label reflects the daemon's ACTUAL window span, not a hardcoded 'last 10 min'", async () => {
    stubDaemonFetch();
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
    fireEvent.click(pill());
    // 90_000ms span → 2 minutes, not the ring's 10-minute ceiling.
    await waitFor(() =>
      expect(
        screen.getByText("last 2 min · daemon sampler"),
      ).toBeInTheDocument(),
    );
    expect(screen.getByText("sampler cost 4.2 ms/sample")).toBeInTheDocument();
    expect(screen.queryByText("daemon does not sample yet")).toBeNull();
    expect(screen.queryByText("last 10 min · daemon sampler")).toBeNull();
    expect(document.querySelector(".meter-row")).not.toBeNull();
  });

  it("on a mission route, `now` still comes from the daemon while avg/max keep the mission's own scope label", async () => {
    stubDaemonFetch();
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
    fireEvent.click(pill());

    // The body's `now` reading is overridden to the daemon's 56%, even
    // though the mission's own last sample was GPU 55%.
    await waitFor(() => expect(screen.getByText("56%")).toBeInTheDocument());

    // Scope label stays the mission's own — the daemon suffix/cost line is
    // a non-mission/non-dispatch-only affordance.
    expect(screen.getByText("this mission")).toBeInTheDocument();
    expect(screen.queryByText(/daemon sampler/)).toBeNull();
    expect(screen.queryByText(/sampler cost/)).toBeNull();
    // avg/max stay the mission's OWN dispatch-derived numbers (55, the
    // single sample's own value), not the daemon's window (50/60).
    expect(screen.getByText(/55% avg/)).toBeInTheDocument();
  });
});
