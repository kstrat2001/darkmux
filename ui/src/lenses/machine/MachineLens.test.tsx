import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MachineLens } from "./MachineLens";
import { todayUTC, prevDateUTC, RECENT_CAP } from "../../lib/flow";

function renderMachine(uid: string | null) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <MachineLens uid={uid} />
    </QueryClientProvider>,
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
  window.location.hash = "";
});

const RESOURCES = {
  schema_version: "1",
  generated_at_ms: 1,
  gather_ms: 1,
  limit_bytes: 1000,
  limit_source: "budget",
  pool: { capacity_bytes: 2000, available_bytes: 1000 },
  pressure: { swap_used_bytes: 0, compressor_bytes: 0, memory_free_percent: 90, red: false },
  models: [],
  machine: { potential_bytes: 500, unpriced_models: 0, current_bytes: 100, state: "green" },
  attribution: "test",
  warnings: [],
  cache_ttl_ms: 2000,
};

/** Routes every endpoint the machine lens reads. `resourcesCalled` records
 * whether `/machine/resources` was EVER requested — the load-bearing
 * assertion for the local-only-probe gate (`enabled: isLocalMach`). */
function mockMachineFetch(opts: {
  specs?: unknown;
  flowToday?: unknown[];
  flowYesterday?: unknown[];
  liveMachines?: unknown[];
} = {}) {
  const today = todayUTC();
  const yesterday = prevDateUTC(today);
  const resourcesCalled = { value: false };
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      const path = String(url);
      if (path === "/machine/specs") {
        return Promise.resolve(new Response(JSON.stringify(opts.specs ?? {}), { status: opts.specs === null ? 404 : 200 }));
      }
      if (path === "/machine/resources") {
        resourcesCalled.value = true;
        return Promise.resolve(new Response(JSON.stringify(RESOURCES), { status: 200 }));
      }
      if (path === `/flow/${today}`) return Promise.resolve(new Response(JSON.stringify(opts.flowToday ?? []), { status: 200 }));
      if (path === `/flow/${yesterday}`) return Promise.resolve(new Response(JSON.stringify(opts.flowYesterday ?? []), { status: 200 }));
      if (path === "/fleet/machines/live") {
        return Promise.resolve(
          new Response(
            JSON.stringify({ machines: opts.liveMachines ?? [], meta: { sources: { fleet: { state: "off" } }, complete: true } }),
            { status: 200 },
          ),
        );
      }
      if (path === "/fleet/sessions/live") {
        return Promise.resolve(new Response(JSON.stringify({ sessions: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }));
      }
      return Promise.resolve(new Response("not recorded\n", { status: 404 }));
    }),
  );
  return resourcesCalled;
}

describe("MachineLens", () => {
  it("uid: null (nav-tab/deep-link) is always the local machine — resources loads with real figures", async () => {
    const resourcesCalled = mockMachineFetch({ specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max", ram_total_bytes: 137438953472 } });
    renderMachine(null);
    await waitFor(() => expect(screen.getByText(/machine total/i)).toBeInTheDocument());
    expect(resourcesCalled.value).toBe(true);
    expect(screen.queryByText(/not reported from here/i)).not.toBeInTheDocument();
  });

  it("a fleet-card drill into a REMOTE uid never fetches /machine/resources, and shows the honest not-reported line", async () => {
    const resourcesCalled = mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max" },
      liveMachines: [{ machine_uid: "remote-uid", display_name: "studio", schema_version: "1", beat_ts_ms: 1, specs: "M1 Max · 32 GB" }],
    });
    renderMachine("remote-uid");
    // `label` is interpolated as a bare text node beside its `<button>`/text
    // siblings in `.machine-lens__hdr` (no wrapping element of its own), so
    // an EXACT-match query can't isolate it — a substring regex against the
    // header's combined text is the reliable form here.
    await waitFor(() => expect(screen.getByText(/machine · studio/)).toBeInTheDocument());
    expect(screen.getByText(/residency \/ RAM not reported from here — local-probe only/i)).toBeInTheDocument();
    expect(screen.getByText(/View the machine page on studio directly/i)).toBeInTheDocument();
    expect(screen.queryByText(/machine total/i)).not.toBeInTheDocument();
    // The whole point of the gate — never even ISSUE the local probe request
    // for a page that can't honestly show its answer.
    expect(resourcesCalled.value).toBe(false);
    // A remote machine's own presence-beat specs string renders in the header.
    expect(screen.getByText(/M1 Max · 32 GB/)).toBeInTheDocument();
  });

  it("self-corrects to local figures when the drilled uid turns out to BE this machine (the OR-gate)", async () => {
    const resourcesCalled = mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max", ram_total_bytes: 137438953472 },
      flowToday: [{ ts: `${todayUTC()}T00:00:00Z`, machine_uid: "self-uid", machine_id: "MacBook-Pro" }],
    });
    // A fleet-card drill (uid explicit, machineIsLocal=false) into the uid
    // that resolves to THIS daemon's own specs.machine_id.
    renderMachine("self-uid");
    await waitFor(() => expect(screen.getByText(/machine total/i)).toBeInTheDocument());
    expect(resourcesCalled.value).toBe(true);
    expect(screen.queryByText(/not reported from here/i)).not.toBeInTheDocument();
  });

  it("an unrecognized/stale uid degrades gracefully — names the raw uid, shows no runs, never crashes", async () => {
    mockMachineFetch({ specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max" } });
    renderMachine("totally-unknown-uid-nobody-has-ever-seen");
    await waitFor(() => expect(screen.getByText(/machine · totally-unknown-uid-nobody-has-ever-seen/)).toBeInTheDocument());
    expect(screen.getByText(/no runs recorded for this machine/i)).toBeInTheDocument();
    expect(screen.getByText(/not reported from here/i)).toBeInTheDocument();
  });

  /**
   * The merge-gate CONSIDER 5 finding: `data-state` is documented (see
   * `MachineLens.tsx`'s own comment above the health region) as the parity
   * harness's post-fetch SETTLED signal — but on a remote page `resources`
   * never fetches at all (`enabled: isLocalMach` gates the query off), so
   * the marker sat at "loading" forever even though the not-reported
   * placeholder had already rendered correctly. A future remote parity
   * test waiting on "loaded"/"error" would hang. A remote page must settle
   * on its own distinct value instead.
   */
  it("a remote machine page settles on data-state=\"remote\", never stuck at \"loading\"", async () => {
    mockMachineFetch({
      specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max" },
      liveMachines: [{ machine_uid: "remote-uid", display_name: "studio", schema_version: "1", beat_ts_ms: 1, specs: "M1 Max · 32 GB" }],
    });
    renderMachine("remote-uid");
    await waitFor(() => expect(screen.getByText(/residency \/ RAM not reported from here/i)).toBeInTheDocument());
    expect(document.querySelector(".machine-lens__health")).toHaveAttribute("data-state", "remote");
  });

  it("the local machine page still settles on data-state=\"loaded\" once /machine/resources resolves", async () => {
    mockMachineFetch({ specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max", ram_total_bytes: 137438953472 } });
    renderMachine(null);
    await waitFor(() => expect(screen.getByText(/machine total/i)).toBeInTheDocument());
    expect(document.querySelector(".machine-lens__health")).toHaveAttribute("data-state", "loaded");
  });

  it("the 'fleet' back-link writes an empty hash", async () => {
    mockMachineFetch({ specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max" } });
    renderMachine(null);
    await waitFor(() => expect(screen.getByText(/machine total/i)).toBeInTheDocument());
    window.location.hash = "#lens=machine";
    screen.getByRole("button", { name: "fleet" }).click();
    expect(window.location.hash).toBe("");
  });
});

/* ── Merged from two branches that independently created this file ──────────
   The drill-in packet added the route/uid cases above; the styling+a11y
   packet (#1767) added the "show all" keyboard-operability cases below.
   They are disjoint and both are kept.

   One adjustment was required: #1767's helper rendered `<MachineLens />`
   with no props, which no longer typechecks now that the component takes a
   `uid`. It passes `uid={null}` — the nav-tab/local-machine case its tests
   were always exercising. */

function renderMachineLens() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <MachineLens uid={null} />
    </QueryClientProvider>,
  );
}

/** One flow record is enough for `sessionsOn`/`buildMachineRuns` to surface
 *  a run row — `buildMachineRuns` falls back to "no start" when there's no
 *  `dispatch.start`, reading handle/model off the first record on the
 *  session (see `lib/flow.ts`'s own doc). */
function machineFlowRecord(sessionId: string) {
  return {
    ts: `${todayUTC()}T10:00:00.000Z`,
    machine_uid: "u1",
    machine_id: "MacBook-Pro",
    session_id: sessionId,
    handle: "coder",
    model: "qwen3.6-35b-a3b",
  };
}

function mockMachineRunsFetch(runCount: number) {
  const today = todayUTC();
  const yesterday = prevDateUTC(today);
  const records = Array.from({ length: runCount }, (_, i) => machineFlowRecord(`s${i}`));
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      const path = String(url);
      if (path === `/flow/${today}`) return Promise.resolve(new Response(JSON.stringify(records), { status: 200 }));
      if (path === `/flow/${yesterday}`) return Promise.resolve(new Response(JSON.stringify([]), { status: 200 }));
      if (path === "/fleet/machines/live") {
        return Promise.resolve(
          new Response(JSON.stringify({ machines: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }),
        );
      }
      if (path === "/fleet/sessions/live") {
        return Promise.resolve(
          new Response(JSON.stringify({ sessions: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }),
        );
      }
      if (path === "/machine/specs") {
        return Promise.resolve(new Response(JSON.stringify({ machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" }), { status: 200 }));
      }
      if (path === "/machine/resources") {
        return Promise.resolve(
          new Response(
            JSON.stringify({
              schema_version: "1.0",
              generated_at_ms: 1,
              gather_ms: 1,
              limit_bytes: 1000,
              limit_source: "test",
              pool: { capacity_bytes: 1000, available_bytes: 500 },
              pressure: { swap_used_bytes: 0, compressor_bytes: 0, memory_free_percent: 50, red: false },
              models: [],
              machine: { potential_bytes: 0, unpriced_models: 0, current_bytes: 0, state: "green" },
            }),
            { status: 200 },
          ),
        );
      }
      return Promise.resolve(new Response("not recorded\n", { status: 404 }));
    }),
  );
}

describe("MachineLens — keyboard operability of '.machine-lens__runsmore'", () => {
  it("does not render the 'show all' control when the run count is at or under the cap", async () => {
    mockMachineRunsFetch(RECENT_CAP);
    const { container } = renderMachineLens();
    await waitFor(() => expect(screen.getByText(/RUNS ON/)).toBeInTheDocument());
    expect(container.querySelector(".machine-lens__runsmore")).toBeNull();
  });

  it("is a real role=button with tabIndex once the run count exceeds the cap", async () => {
    mockMachineRunsFetch(RECENT_CAP + 5);
    const { container } = renderMachineLens();
    await waitFor(() => expect(container.querySelector(".machine-lens__runsmore")).toBeInTheDocument());
    const more = container.querySelector(".machine-lens__runsmore")!;
    expect(more).toHaveAttribute("role", "button");
    expect(more).toHaveAttribute("tabIndex", "0");
    expect(more.textContent).toMatch(new RegExp(`show all ${RECENT_CAP + 5}`));
  });

  it("expands to the full list on a click", async () => {
    mockMachineRunsFetch(RECENT_CAP + 5);
    const { container } = renderMachineLens();
    await waitFor(() => expect(container.querySelector(".machine-lens__runsmore")).toBeInTheDocument());
    fireEvent.click(container.querySelector(".machine-lens__runsmore")!);
    await waitFor(() => expect(container.querySelectorAll(".machine-lens__run").length).toBe(RECENT_CAP + 5));
    expect(screen.getByText("show fewer")).toBeInTheDocument();
  });

  it("expands to the full list on a keyboard Enter — the fix under test", async () => {
    mockMachineRunsFetch(RECENT_CAP + 5);
    const { container } = renderMachineLens();
    await waitFor(() => expect(container.querySelector(".machine-lens__runsmore")).toBeInTheDocument());
    fireEvent.keyDown(container.querySelector(".machine-lens__runsmore")!, { key: "Enter" });
    await waitFor(() => expect(container.querySelectorAll(".machine-lens__run").length).toBe(RECENT_CAP + 5));
  });

  it("toggles back on a keyboard Space", async () => {
    mockMachineRunsFetch(RECENT_CAP + 5);
    const { container } = renderMachineLens();
    await waitFor(() => expect(container.querySelector(".machine-lens__runsmore")).toBeInTheDocument());
    fireEvent.keyDown(container.querySelector(".machine-lens__runsmore")!, { key: " " });
    await waitFor(() => expect(screen.getByText("show fewer")).toBeInTheDocument());
    fireEvent.keyDown(container.querySelector(".machine-lens__runsmore")!, { key: " " });
    await waitFor(() => expect(container.querySelectorAll(".machine-lens__run").length).toBe(RECENT_CAP));
  });
});
