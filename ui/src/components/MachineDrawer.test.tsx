import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  render as rtlRender,
  screen,
  fireEvent,
  waitFor,
  act,
} from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MachineDrawer } from "./MachineDrawer";
import { closeOpenModal, openModalEl } from "../lib/dialogManager";
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

/** (#2108, operator finding) There is no in-component desktop trigger any
 * more — `MachineDrawer.tsx` on desktop now renders ONLY `<Dialog
 * id="imodalbg">`, no button of its own. Every desktop test in this file
 * that used to `fireEvent.click(pill())` now opens the SAME shared
 * `dialogManager` store directly, the way the masthead's own ⓘ
 * (`Masthead.tsx`, not rendered by these component-level tests) actually
 * does it in the real app. Wrapped in `act` since this mutates external
 * store state outside any React event handler React itself dispatched. */
function openDesktop() {
  act(() => {
    openModalEl("imodalbg");
  });
}

describe("MachineDrawer (#2107)", () => {
  it("(#2108) renders no floating trigger of its own on desktop — the masthead ⓘ is the only affordance", () => {
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
    expect(
      screen.queryByRole("button", { name: MACHINE_INFO_LABEL }),
    ).toBeNull();
    expect(document.querySelector('[data-act="machine-drawer-pill"]')).toBeNull();
    expect(screen.queryByText(/GPU 68%/)).toBeNull();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("opens on the shared dialog store and shows all three meters", () => {
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
    openDesktop();
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
    openDesktop();
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
    openDesktop();
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
    openDesktop();
    fireEvent.keyDown(document, { key: "Escape" });
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
    openDesktop();
    expect(screen.getByText("this mission")).toBeInTheDocument();
    expect(
      document.querySelector('[data-meter="gpu"] .meter-now')?.textContent,
    ).toBe("55%");
  });

  // (#2120) The `playback` kv row — everything the sticky row's folded
  // `#meta` summary used to carry that the transport doesn't already say:
  // the flow day, the recorded time span, the day's record/machine census,
  // and the raw mission id (the transport itself shows only a human label,
  // or nothing — `App.tsx`'s own `playbackMissionLabel` doc). `routeRecords`
  // here stands in for what `App.tsx` hands down on a playback route — the
  // WHOLE loaded day, not a rolling window.
  it("(#2120) shows the `playback` row — day, span, census, and the raw mission id — on a playback route", () => {
    const dayRecords: FlowRecord[] = [
      { ts: "2026-08-26T01:08:17.000Z", machine_uid: "m1", machine_id: "MacBook-Pro", mission_id: "demo-review-nameof-recency", session_id: "s1", action: "dispatch.start" } as FlowRecord,
      { ts: "2026-08-26T14:13:01.000Z", machine_uid: "m1", machine_id: "MacBook-Pro", mission_id: "demo-review-nameof-recency", session_id: "s1", action: "dispatch.complete" } as FlowRecord,
    ];
    render(
      <MachineDrawer
        route={{ kind: "playback", date: "2026-08-26" }}
        routeRecords={dayRecords}
        flowWindow={[]}
        localUid={null}
        nowMsOverride={NOW}
        liveMachines={new Map()}
        specs={null}
        liveStatus="live"
        {...EMPTY_EVENTLOG}
      />,
    );
    openDesktop();
    expect(screen.getByText("playback")).toBeInTheDocument();
    const row = screen.getByText("playback").closest(".dialog__kv")!;
    const value = row.querySelector("span")!.textContent ?? "";
    expect(value).toContain("flow 2026-08-26");
    expect(value).toContain("2 records · 1 machines");
    expect(value).toContain("mission demo-review-nameof-recency");
  });

  it("(#2120) renders no `playback` row on a live route — nothing to replay", () => {
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
    openDesktop();
    expect(screen.queryByText("playback")).toBeNull();
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
          flow_schema_version: "1.28.0",
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
    openDesktop();
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
    openDesktop();
    expect(document.querySelector(".machine-drawer__identity")).toBeNull();
  });

  // (#2108, operator finding) The external links row (github/guide/
  // articles/home) is REMOVED from the about section — neither surface
  // shows it any more.
  it("(#2108) does not render the external links row", () => {
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
    openDesktop();
    expect(screen.queryByText("links")).toBeNull();
    expect(
      document.querySelector('a[href="https://github.com/kstrat2001/darkmux"]'),
    ).toBeNull();
    expect(
      document.querySelector('a[href="https://darkmux.com/guide/"]'),
    ).toBeNull();
    expect(
      document.querySelector('a[href="https://darklyenergized.substack.com"]'),
    ).toBeNull();
    expect(
      document.querySelector('a[href="https://darkmux.com/"]'),
    ).toBeNull();
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
    openDesktop();
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
    openDesktop();
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
    openDesktop();
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
    // (#2108) The phone drawer's Events tab is the plain default
    // EventLogColumn (list + detail pane, tap-to-select) — same row
    // rendering as the desktop column.
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
    // (#2108, operator finding) No floating trigger on desktop any more —
    // just the dialog shell (openable via `dialogManager`, same as the
    // masthead's own ⓘ), never the phone bar.
    expect(
      document.querySelector('[data-act="machine-drawer-pill"]'),
    ).toBeNull();
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
      now: {
        sampled_at_ms: 4000,
        sampler_cost_ms: 4.2,
        cpu_pct: 12,
        cpu_clusters: null,
        mem_pct: 34,
        gpu_pct: 56,
        gpu_mhz: null,
        gpu_mem_bytes: null,
        thermal: null,
        power_mw: null,
      },
      window: {
        samples: 3,
        interval_ms: 2000,
        span_ms: 90_000, // 90s — under 10 min, so the label must say "2 min", not "10 min"
        cpu_pct: { mean: 10, p95: 15, max: 20 },
        mem_pct: { mean: 30, p95: 35, max: 40 },
        gpu_pct: { mean: 50, p95: 55, max: 60 },
        power_mw: null,
        thermal: null,
        energy_mwh: null,
      },
    },
  };

  function stubDaemonFetch(payload: unknown = RESOURCES_WITH_LOAD) {
    const fetchMock = vi.fn((url: string) => {
      if (String(url) === "/machine/resources") {
        return Promise.resolve(
          new Response(JSON.stringify(payload), { status: 200 }),
        );
      }
      return Promise.reject(new Error(`unexpected fetch in this test: ${url}`));
    });
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
  }

  it("closed: zero fetches — no dialog open, no network activity", () => {
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
    expect(screen.queryByRole("dialog")).toBeNull();
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
    openDesktop();
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
    openDesktop();
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
    openDesktop();

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

// ── (#2108, host-sample-shape v2) thermal/power/CPU-cluster rows ──────────
//
// `HostExtras` (`machineStatsContent.tsx`) reads `daemonLoad.now`/`.window`
// directly — independent of `agg`/`isIdle` — so these rows render off the
// SAME single resolved `/machine/resources` fetch the CPU/GPU/MEM meters
// already use above. Exercised through BOTH the desktop dialog and the
// phone drawer's Machine tab, since both render the exact same
// `useMachineStatsContent` body (see that hook's own doc).
describe("MachineDrawer — host extras: thermal/power/CPU clusters (#2108)", () => {
  const FULL_LOAD = {
    schema_version: "1",
    generated_at_ms: 1,
    gather_ms: 1,
    limit_bytes: 1,
    limit_source: "test",
    pool: { capacity_bytes: 1, used_bytes: 1, available_bytes: 1, free_bytes: 1 },
    pressure: { swap_used_bytes: 0, compressor_bytes: 0, margin_percent: 90, red: false },
    models: [],
    machine: { potential_bytes: 1, unpriced_models: 0, current_bytes: 1, state: "green" },
    attribution: "test",
    messages: [],
    cache_ttl_ms: 2000,
    load: {
      now: {
        sampled_at_ms: 4000,
        sampler_cost_ms: 4.2,
        cpu_pct: 12,
        cpu_clusters: [
          { name: "Super", cores: 6, pct: 46, mhz: 4400 },
          { name: "Performance", cores: 12, pct: 22, mhz: 3400 },
          { name: "Efficiency", cores: 4, pct: 9, mhz: 2100 },
        ],
        mem_pct: 34,
        gpu_pct: 56,
        gpu_mhz: 1296,
        gpu_mem_bytes: 912_000_000,
        thermal: { state: "fair", cpu_speed_limit_pct: 87 },
        power_mw: { cpu: 5200, gpu: 3400, ane: 400, total: 9000 },
      },
      window: {
        samples: 3,
        interval_ms: 2000,
        span_ms: 90_000,
        cpu_pct: { mean: 10, p95: 15, max: 20 },
        mem_pct: { mean: 30, p95: 35, max: 40 },
        gpu_pct: { mean: 50, p95: 55, max: 60 },
        power_mw: {
          total: { mean: 7800, p95: 9200, max: 11000 },
          gpu: { mean: 2600, p95: 3600, max: 4200 },
          cpu: { mean: 4700, p95: 5300, max: 6200 },
        },
        thermal: {
          worst_state: "serious",
          above_nominal_ms: 45_000,
          min_cpu_speed_limit_pct: 80,
        },
        energy_mwh: 1289,
      },
    },
  };

  const NULL_LOAD = {
    ...FULL_LOAD,
    load: {
      now: {
        sampled_at_ms: 4000,
        sampler_cost_ms: 4.2,
        cpu_pct: 12,
        cpu_clusters: null,
        mem_pct: 34,
        gpu_pct: 56,
        gpu_mhz: null,
        gpu_mem_bytes: null,
        thermal: null,
        power_mw: null,
      },
      window: {
        samples: 3,
        interval_ms: 2000,
        span_ms: 90_000,
        cpu_pct: { mean: 10, p95: 15, max: 20 },
        mem_pct: { mean: 30, p95: 35, max: 40 },
        gpu_pct: { mean: 50, p95: 55, max: 60 },
        power_mw: null,
        thermal: null,
        energy_mwh: null,
      },
    },
  };

  function stubFetch(payload: unknown) {
    const fetchMock = vi.fn((url: string) => {
      if (String(url) === "/machine/resources") {
        return Promise.resolve(
          new Response(JSON.stringify(payload), { status: 200 }),
        );
      }
      return Promise.reject(new Error(`unexpected fetch in this test: ${url}`));
    });
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
  }

  it("renders every new row with correct formatting when every field is present (desktop dialog)", async () => {
    stubFetch(FULL_LOAD);
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
    openDesktop();

    // GPU row extras — MHz + in-use memory, joined onto the GPU reading.
    // Scoped container query, not `getByText` with the full string: the
    // text is split between a bare "GPU " text node and the nested
    // `<InlineOrCells>` span (desktop), which `getByText` won't bridge.
    await waitFor(() =>
      expect(
        document.querySelector(".machine-drawer__gpu-extra")?.textContent,
      ).toBe("GPU 1296 MHz · 912.0 MB"),
    );

    // Thermal: state pill (title-cased) + speed limit (< 100%) + window
    // worst-state/above-nominal line.
    expect(screen.getByText("Fair")).toBeInTheDocument();
    expect(screen.getByText("CPU speed limit 87%")).toBeInTheDocument();
    expect(
      screen.getByText("worst Serious · 45s above nominal"),
    ).toBeInTheDocument();

    // Power: total now/avg/p95/max in W (≥1000 mW), per-channel row, and
    // the window energy total in Wh (energy_mwh ≥ 1000).
    expect(
      screen.getByText("9.0 W now · 7.8 W avg · 9.2 W p95 · 11.0 W max"),
    ).toBeInTheDocument();
    expect(
      screen.getByText("CPU 5.2 W · GPU 3.4 W · ANE 400 mW"),
    ).toBeInTheDocument();
    expect(screen.getByText("1.29 Wh")).toBeInTheDocument();

    // CPU clusters: one row per cluster — name, core count, pct, MHz.
    expect(screen.getByText("Super")).toBeInTheDocument();
    expect(screen.getByText("Performance")).toBeInTheDocument();
    expect(screen.getByText("Efficiency")).toBeInTheDocument();
    expect(screen.getByText(/6 cores/)).toBeInTheDocument();
    expect(screen.getByText(/4400 MHz/)).toBeInTheDocument();
  });

  /** (operator finding — "GPU memory is wrapping") A host with no IOReport
   * GPU perf-state (`gpu_mhz: null`, common on this hardware) leaves the
   * GPU-extra line with exactly ONE item (memory). `InlineOrCells`' mobile
   * cell-grid mode stacks a label above its value regardless of item
   * count, which read as three floating lines ("GPU" / "MEMORY" /
   * "23.8 MB") on a phone — indistinguishable from real section headers.
   * The fix bypasses `InlineOrCells` for a single item; this fixture is
   * the exact case that was broken, on BOTH surfaces. */
  const SINGLE_GPU_ITEM_LOAD = {
    ...FULL_LOAD,
    load: {
      ...FULL_LOAD.load,
      now: { ...FULL_LOAD.load.now, gpu_mhz: null, gpu_mem_bytes: 23_800_000 },
    },
  };

  it("a single GPU-extra item (no MHz reading) renders as one inline line, not a stacked label/value (desktop dialog)", async () => {
    stubFetch(SINGLE_GPU_ITEM_LOAD);
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
    openDesktop();
    await waitFor(() =>
      expect(
        document.querySelector(".machine-drawer__gpu-extra")?.textContent,
      ).toBe("GPU 23.8 MB"),
    );
    expect(
      document.querySelector('.machine-drawer__gpu-extra [data-act="inline-or-cells"]'),
    ).toBeNull();
  });

  it("a single GPU-extra item (no MHz reading) renders as one inline line on mobile too — the exact case that used to wrap into 3 lines", async () => {
    stubFetch(SINGLE_GPU_ITEM_LOAD);
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
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    await waitFor(() =>
      expect(
        document.querySelector(".machine-drawer__gpu-extra")?.textContent,
      ).toBe("GPU 23.8 MB"),
    );
    // The bug: on mobile, a single item still went through `InlineOrCells`'
    // cell-grid, producing a "MEMORY" cell label styled like a section
    // header sitting above its value. No cell-grid at all for one item now.
    expect(
      document.querySelector('.machine-drawer__gpu-extra [data-act="inline-or-cells"]'),
    ).toBeNull();
  });

  it("hides every new row when every new field is null — no layout the operator has to scroll past", async () => {
    const fetchMock = stubFetch(NULL_LOAD);
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
    openDesktop();
    // Wait for the resolved poll (the existing CPU/GPU/MEM meters still
    // render off it) before asserting on what it did NOT add.
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    await waitFor(() =>
      expect(document.querySelector(".meter-row")).not.toBeNull(),
    );
    expect(document.querySelector(".thermal-row")).toBeNull();
    expect(document.querySelector(".power-block")).toBeNull();
    expect(document.querySelector(".cluster-block")).toBeNull();
    expect(document.querySelector(".machine-drawer__gpu-extra")).toBeNull();
  });

  // (#2108 review finding 7) A pre-#2108 v1-shaped daemon reply — `window`
  // present but keyed the OLD way (`window.cpu.mean_pct`, no `cpu_pct`/
  // `gpu_pct`/`mem_pct`/`thermal`/`power_mw`/`energy_mwh` at all — the
  // exact mismatch the sibling Rust-side finding traced to
  // `scripts/demo-env/build.py` emitting v1 against a v2-shaped committed
  // fixture). `effectiveHostAggregate`/`HostExtras`'s `?.`/`?? null` guards
  // (`machineStatsContent.tsx`) are what stand between this payload and a
  // thrown `TypeError: Cannot read properties of undefined` that would
  // have taken the whole dialog down — this is the render-level proof, not
  // just the pure-function unit test in `machineStatsContent.test.ts`.
  it("a v1-shaped daemon window (old field names, none of the v2 keys) renders the degraded meters instead of throwing", async () => {
    const v1Load = {
      ...FULL_LOAD,
      load: {
        now: {
          sampled_at_ms: 4000,
          sampler_cost_ms: 4.2,
          cpu_pct: 12,
          mem_pct: 34,
          gpu_pct: 56,
          // v1 never had cpu_clusters/gpu_mhz/gpu_mem_bytes/thermal/power_mw.
        },
        window: {
          samples: 3,
          span_ms: 90_000,
          // v1's OWN naming — nested `mean_pct`/`peak_pct`/`p95_pct` under
          // `cpu`/`gpu`/`mem`, never the v2 `cpu_pct: { mean, max, p95 }`
          // shape `MachineLoad`'s type declares as always-present.
          cpu: { mean_pct: 10, peak_pct: 20, p95_pct: 15 },
          mem: { mean_pct: 30, peak_pct: 40, p95_pct: 35 },
          gpu: { mean_pct: 50, peak_pct: 60, p95_pct: 55 },
        },
      },
    };
    const fetchMock = stubFetch(v1Load);
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
    openDesktop();
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    // The meters still render (this doesn't throw) — `now` is present on
    // this v1 shape, so it renders; avg/max/p95 degrade to "—" since the
    // v2 keys they'd read from are entirely absent.
    await waitFor(() =>
      expect(document.querySelector(".meter-row")).not.toBeNull(),
    );
    expect(screen.getByText("56%")).toBeInTheDocument(); // GPU `now`
    // avg/max degrade to the "—" placeholder (`Meter.tsx`'s own
    // null-renders-as-"—" convention) — every meter's avg/max row reads
    // it, not just one exact-text match, since "—" sits alongside " avg "/
    // " max " text nodes rather than in its own wrapping element.
    const avgmaxRows = document.querySelectorAll(".meter-avgmax");
    expect(avgmaxRows.length).toBeGreaterThan(0);
    avgmaxRows.forEach((row) => expect(row.textContent).toContain("—"));
    // GPU extras (MHz/memory) never appeared either — v1's `now` lacks
    // `gpu_mhz`/`gpu_mem_bytes` entirely, not just as explicit nulls.
    expect(document.querySelector(".machine-drawer__gpu-extra")).toBeNull();
    // None of the v2-only rows render — their source data is entirely
    // absent on this shape, same degraded-hides-the-row contract as the
    // explicit-null case above.
    expect(document.querySelector(".thermal-row")).toBeNull();
    expect(document.querySelector(".power-block")).toBeNull();
    expect(document.querySelector(".cluster-block")).toBeNull();
    expect(document.querySelector(".machine-drawer__gpu-extra")).toBeNull();
  });

  it("the phone drawer's Machine tab renders the SAME host-extras rows as the desktop dialog", async () => {
    stubFetch(FULL_LOAD);
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
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    await waitFor(() => expect(screen.getByText("Fair")).toBeInTheDocument());
    expect(screen.getByText("Super")).toBeInTheDocument();
    // Phone chrome renders the CELL GRID (InlineOrCells' mobile branch),
    // not the desktop's joined dotted text — see the dedicated
    // "wrap fix" describe block below for the row-level proof; this test
    // only needs to confirm the SAME facts reached this surface too.
    const channelsGrid = [
      ...document.querySelectorAll(".dialog__kv"),
    ].find((kv) => kv.querySelector("b")?.textContent === "Channels");
    expect(channelsGrid).toBeTruthy();
    expect(
      channelsGrid!.querySelector('[data-act="inline-or-cells"]'),
    ).not.toBeNull();
    expect(channelsGrid!.textContent).toContain("CPU");
    expect(channelsGrid!.textContent).toContain("5.2 W");
    expect(channelsGrid!.textContent).toContain("ANE");
  });

  // ── (#2108, operator finding — wrap fix) dotted lists that become cell
  //    grids on narrow viewports. Real-phone review found several rows in
  //    `machineStatsContent.tsx` wrapping MID-ITEM at ~390px: the power
  //    total ("15.0 W / p95" split across two lines), the channels row
  //    ("ANE" pushed onto its own broken line), and the identity header
  //    (wrapping after "128 GB ·"). `InlineOrCells.test.tsx` proves the
  //    shared component itself; these prove the WIRING — that the real
  //    hook/prop thread actually switches these specific rows at the real
  //    breakpoint, reusing THIS describe block's own `FULL_LOAD`/
  //    `stubFetch`. ──

  it("mobile: the identity header stacks TWO lines with no separators and drops the version", () => {
    const meta = document.createElement("meta");
    meta.name = "darkmux-version";
    meta.content = "3.3.0 (ea3caf27)";
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
          darkmux_version: "3.3.0 (ea3caf27)",
          flow_schema_version: "1.28.0",
          machine_id: "MacBook-Pro",
          os: "macOS",
          ram_total_bytes: 137438953472,
          ram_free_for_ai_bytes: null,
          cpu_brand: "Apple M5 Max",
          loaded_models: [],
          lms_unreachable: false,
          utility_model: null,
          redis_url_redacted: null,
          generated_at_ms: NOW,
        }}
        liveStatus="live"
        nowMsOverride={NOW}
        isMobileOverride={true}
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    const identity = document.querySelector(
      ".machine-drawer__identity--mobile",
    )!;
    expect(identity).not.toBeNull();
    const lines = identity.querySelectorAll(
      ".machine-drawer__identity-line",
    );
    expect(lines.length).toBe(2);
    expect(lines[0].textContent).toBe("MacBook-Pro");
    expect(lines[1].textContent).toBe("Apple M5 Max · 128 GB");
    // The version is dropped from this form — it's already the `build`
    // row in the about kv block below, nothing is lost.
    expect(identity.textContent).not.toContain("3.3.0");

    // Desktop keeps the SINGLE dotted line, version included — unchanged
    // from before this packet.
    document.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
  });

  it("desktop: the identity header stays the single dotted line with the version, no --mobile modifier", () => {
    const meta = document.createElement("meta");
    meta.name = "darkmux-version";
    meta.content = "3.3.0 (ea3caf27)";
    document.head.appendChild(meta);
    const flowWindow = [
      { ts: "2026-01-01T00:00:00Z", machine_uid: "self-uid", machine_id: "MacBook-Pro" },
    ];
    render(
      <MachineDrawer
        route={{ kind: "fleet" }}
        routeRecords={[]}
        flowWindow={flowWindow}
        localUid="self-uid"
        liveMachines={new Map()}
        specs={{
          darkmux_version: "3.3.0 (ea3caf27)",
          flow_schema_version: "1.28.0",
          machine_id: "MacBook-Pro",
          os: "macOS",
          ram_total_bytes: 137438953472,
          ram_free_for_ai_bytes: null,
          cpu_brand: "Apple M5 Max",
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
    openDesktop();
    expect(
      document.querySelector(".machine-drawer__identity--mobile"),
    ).toBeNull();
    const identity = document.querySelector(".machine-drawer__identity")!;
    expect(identity.textContent).toBe(
      "MacBook-Pro · Apple M5 Max · 128 GB · darkmux 3.3.0 (ea3caf27)",
    );
  });

  it("mobile: power total, channels, thermal window, and GPU lines each render as a nowrap cell grid", async () => {
    stubFetch(FULL_LOAD);
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
        {...EMPTY_EVENTLOG}
      />,
    );
    fireEvent.click(
      document.querySelector('[data-act="phone-drawer-tab-machine"]')!,
    );
    await waitFor(() =>
      expect(
        document.querySelectorAll('[data-act="inline-or-cells"]').length,
      ).toBeGreaterThan(0),
    );
    // power total (4 cells: now/avg/p95/max), channels (3: CPU/GPU/ANE),
    // thermal window (2: worst/above nominal), GPU MHz+memory (2).
    const grids = document.querySelectorAll('[data-act="inline-or-cells"]');
    expect(grids.length).toBe(4);
    grids.forEach((grid) => {
      const values = grid.querySelectorAll(".inline-or-cells__cell-value");
      expect(values.length).toBeGreaterThan(0);
      values.forEach((v) => {
        expect(v.className).toContain("inline-or-cells__cell-value");
      });
    });
    // No dotted-list joined string anywhere on this surface any more.
    expect(
      screen.queryByText("9.0 W now · 7.8 W avg · 9.2 W p95 · 11.0 W max"),
    ).toBeNull();
    expect(screen.queryByText("worst Serious · 45s above nominal")).toBeNull();
  });
});
