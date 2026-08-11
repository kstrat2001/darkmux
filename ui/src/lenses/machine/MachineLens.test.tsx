import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MachineLens } from "./MachineLens";
import { todayUTC, prevDateUTC, RECENT_CAP } from "../../lib/flow";

afterEach(() => {
  vi.unstubAllGlobals();
});

function renderMachineLens() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <MachineLens />
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

function mockMachineFetch(runCount: number) {
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
    mockMachineFetch(RECENT_CAP);
    const { container } = renderMachineLens();
    await waitFor(() => expect(screen.getByText(/RUNS ON/)).toBeInTheDocument());
    expect(container.querySelector(".machine-lens__runsmore")).toBeNull();
  });

  it("is a real role=button with tabIndex once the run count exceeds the cap", async () => {
    mockMachineFetch(RECENT_CAP + 5);
    const { container } = renderMachineLens();
    await waitFor(() => expect(container.querySelector(".machine-lens__runsmore")).toBeInTheDocument());
    const more = container.querySelector(".machine-lens__runsmore")!;
    expect(more).toHaveAttribute("role", "button");
    expect(more).toHaveAttribute("tabIndex", "0");
    expect(more.textContent).toMatch(new RegExp(`show all ${RECENT_CAP + 5}`));
  });

  it("expands to the full list on a click", async () => {
    mockMachineFetch(RECENT_CAP + 5);
    const { container } = renderMachineLens();
    await waitFor(() => expect(container.querySelector(".machine-lens__runsmore")).toBeInTheDocument());
    fireEvent.click(container.querySelector(".machine-lens__runsmore")!);
    await waitFor(() => expect(container.querySelectorAll(".machine-lens__run").length).toBe(RECENT_CAP + 5));
    expect(screen.getByText("show fewer")).toBeInTheDocument();
  });

  it("expands to the full list on a keyboard Enter — the fix under test", async () => {
    mockMachineFetch(RECENT_CAP + 5);
    const { container } = renderMachineLens();
    await waitFor(() => expect(container.querySelector(".machine-lens__runsmore")).toBeInTheDocument());
    fireEvent.keyDown(container.querySelector(".machine-lens__runsmore")!, { key: "Enter" });
    await waitFor(() => expect(container.querySelectorAll(".machine-lens__run").length).toBe(RECENT_CAP + 5));
  });

  it("toggles back on a keyboard Space", async () => {
    mockMachineFetch(RECENT_CAP + 5);
    const { container } = renderMachineLens();
    await waitFor(() => expect(container.querySelector(".machine-lens__runsmore")).toBeInTheDocument());
    fireEvent.keyDown(container.querySelector(".machine-lens__runsmore")!, { key: " " });
    await waitFor(() => expect(screen.getByText("show fewer")).toBeInTheDocument());
    fireEvent.keyDown(container.querySelector(".machine-lens__runsmore")!, { key: " " });
    await waitFor(() => expect(container.querySelectorAll(".machine-lens__run").length).toBe(RECENT_CAP));
  });
});
