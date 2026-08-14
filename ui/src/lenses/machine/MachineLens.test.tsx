import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MachineLens } from "./MachineLens";
import { todayUTC, prevDateUTC } from "../../lib/flow";

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

  it("an unrecognized/stale uid degrades gracefully — names the raw uid, links to its (empty) runs lens, never crashes", async () => {
    mockMachineFetch({ specs: { machine_id: "MacBook-Pro", cpu_brand: "M5 Max" } });
    renderMachine("totally-unknown-uid-nobody-has-ever-seen");
    await waitFor(() => expect(screen.getByText(/machine · totally-unknown-uid-nobody-has-ever-seen/)).toBeInTheDocument());
    // (#1809) The old "no runs recorded for this machine" hint text is gone
    // with the runs list itself — a stale uid still gets a real, honestly
    // zero-count link out (never a crash, never a stale-looking count).
    const link = screen.getByRole("link", { name: /runs on/i });
    expect(link).toHaveAttribute("href", "#lens=runs&machine=totally-unknown-uid-nobody-has-ever-seen");
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

function renderMachineLens() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <MachineLens uid={null} />
    </QueryClientProvider>,
  );
}

/** One flow record per session. */
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

// (#1809, finishing #1508 step 4) The old "RUNS ON <MACHINE>" list — and
// the "show all"/"show fewer" expand-collapse control the a11y packet
// (#1767) hardened keyboard access for — is gone; this section replaces
// those tests with coverage of what replaced it: a link out to the runs
// lens, pinned to this machine.
//
// The link carries NO COUNT, and these tests pin that absence deliberately.
// An earlier cut labeled it with `sessionsOn(...).length` and this suite
// asserted the number — which passed while the live page LIED: that counts
// distinct session ids in the 24h flow window, the destination lists /runs
// rows over a 14-day window unioning missions, lab runs and ghosts. Measured
// on a real daemon: the link read "0 runs" while the destination listed 282.
// The tests were green the whole time, because they seeded the flow window
// and asserted against the same window — never against what the destination
// would actually show. A test that pins a number to its own fixture cannot
// catch a number that means the wrong thing.
describe("MachineLens — the runs-lens link (#1809)", () => {
  it("names the machine and points at the pinned runs lens", async () => {
    mockMachineRunsFetch(3);
    renderMachineLens();
    const link = await screen.findByRole("link", { name: /runs on MacBook-Pro/i });
    expect(link).toHaveAttribute("href", "#lens=runs&machine=u1");
  });

  // The regression guard for the lie described above: whatever the flow
  // window happens to hold, the label must not claim a quantity. Seeded with
  // three sessions precisely because the old implementation would have
  // rendered "3" here.
  it("claims no count, whatever this window's session total happens to be", async () => {
    mockMachineRunsFetch(3);
    renderMachineLens();
    const link = await screen.findByRole("link", { name: /runs on MacBook-Pro/i });
    expect(link.textContent).toBe("runs on MacBook-Pro →");
    expect(link.textContent).not.toMatch(/\d/);
  });

  it("renders the same label with an EMPTY window — no count to be wrong, nothing hidden", async () => {
    mockMachineRunsFetch(0);
    renderMachineLens();
    const link = await screen.findByRole("link", { name: /runs on MacBook-Pro/i });
    expect(link.textContent).toBe("runs on MacBook-Pro →");
  });

  it("clicking the link navigates via a real hash write, not a page reload", async () => {
    mockMachineRunsFetch(3);
    renderMachineLens();
    const link = await screen.findByRole("link", { name: /runs on MacBook-Pro/i });
    fireEvent.click(link);
    expect(window.location.hash).toBe("#lens=runs&machine=u1");
  });

  // Red-provable regression guard: if the old list ever comes back, this
  // fails — proving the removal actually happened, not just that the link
  // exists alongside it.
  it("the old per-run list markup is gone — no '.machine-lens__run' rows, no 'RUNS ON' header", async () => {
    mockMachineRunsFetch(3);
    const { container } = renderMachineLens();
    await screen.findByRole("link", { name: /runs on MacBook-Pro/i });
    expect(container.querySelector(".machine-lens__run")).toBeNull();
    expect(screen.queryByText(/RUNS ON/)).not.toBeInTheDocument();
  });
});
