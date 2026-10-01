import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import { render, screen, waitFor, fireEvent, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RunsBoard } from "./RunsBoard";
import { FILTER_DIMS, emptyFilterSel, type FilterSel } from "../../lib/runsFilterQuery";

// (#2925) The dimension filters, through the rendered board. The clock is
// frozen so the time-window assertions never depend on when the suite runs.
const NOW = Date.UTC(2026, 9, 1, 12, 0, 0);
const NOW_S = NOW / 1000;

const RUNS = [
  { id: "a1", kind: "dispatch", status: "complete", tracked: true, updated_ts: NOW_S - 600, role: "coder", model: "darkmux:qwen", machine: "studio", machine_uid: "AA" },
  { id: "a2", kind: "dispatch", status: "complete", tracked: true, updated_ts: NOW_S - 7200, role: "coder", model: "qwen", machine: "studio-renamed", machine_uid: "aa" },
  { id: "b1", kind: "mission", status: "running", tracked: true, updated_ts: NOW_S - 100, role: "reviewer", model: "llama", machine: "laptop", machine_uid: "BB" },
  { id: "l1", kind: "lab", status: "complete", tracked: true, updated_ts: NOW_S - 3 * 86400, workload: "w1", verify_passed: false, machine: "laptop", machine_uid: "BB" },
];

function mockFetch(runs: unknown[] = RUNS) {
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      if (url === "/runs") return Promise.resolve(new Response(JSON.stringify({ runs, generated_at_ms: 1 }), { status: 200 }));
      if (url === "/lab/runs") return Promise.resolve(new Response(JSON.stringify({ configured: true, dir: "/lab", exists: true, runs: [] }), { status: 200 }));
      return Promise.resolve(new Response("[]", { status: 200 }));
    }),
  );
}

function renderBoard(filters?: FilterSel, kind: "all" | "mission" | "dispatch" | "lab" = "all") {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <RunsBoard initialKind={kind} initialRun={null} initialMachineKey={null} initialFilters={filters} />
    </QueryClientProvider>,
  );
}

const sel = (o: Partial<FilterSel>): FilterSel => ({ ...emptyFilterSel(), ...o });
const pill = (dim: string) => document.querySelector<HTMLElement>(`.fpill[data-dim="${dim}"]`) as HTMLElement;
const pillOrder = () => [...document.querySelectorAll<HTMLElement>(".fpill")].map((p) => p.dataset.dim);
const rowIds = () => [...document.querySelectorAll(".labrunrow .labruncrew")].map((e) => e.textContent);

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["Date"] });
  vi.setSystemTime(NOW);
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  window.location.hash = "";
});

async function ready() {
  await waitFor(() => expect(document.querySelector(".fbar")).not.toBeNull());
}

describe("RunsBoard dimension filters (#2925)", () => {
  it("draws one pill per dimension, in bar order, and keeps them all while values are chosen", async () => {
    mockFetch();
    renderBoard();
    await ready();
    expect(pillOrder()).toEqual([...FILTER_DIMS]);
    fireEvent.click(pill("role"));
    fireEvent.click(screen.getByLabelText(/coder/));
    // Choosing a role leaves every pill in place, even ones that now cannot narrow.
    expect(pillOrder()).toEqual([...FILTER_DIMS]);
  });

  it("choosing a value filters the rows, counts the kind tabs under it, and writes the hash", async () => {
    mockFetch();
    renderBoard();
    await ready();
    fireEvent.click(pill("role"));
    fireEvent.click(screen.getByLabelText(/coder/));
    expect(rowIds()).toEqual(["a1", "a2"]);
    expect(screen.getByText("Showing 2 of 4 runs")).toBeInTheDocument();
    const tab = (k: string) => document.querySelector(`.runchip[data-arg="${k}"] .runchipn`)?.textContent?.trim();
    expect([tab("all"), tab("dispatch"), tab("mission"), tab("lab")]).toEqual(["2", "2", "0", "0"]);
    expect(new URLSearchParams(window.location.hash.slice(1)).getAll("role")).toEqual(["coder"]);
    expect(screen.getByText("Role coder ✕")).toBeInTheDocument();
  });

  it("one machine under two names is one choice, labeled with its current name", async () => {
    mockFetch();
    renderBoard();
    await ready();
    fireEvent.click(pill("machine"));
    const pop = screen.getByRole("dialog");
    const labels = within(pop).getAllByRole("checkbox").map((c) => c.closest("label")?.textContent);
    // uid AA/aa is one choice under the name on its most recent run (`studio`,
    // a1 beats the older `studio-renamed`); `laptop` is the other machine.
    expect(labels).toEqual(["studio2", "laptop2"]);
  });

  it("a dimension whose runs in view all share one value dims, says why, and still opens", async () => {
    mockFetch();
    renderBoard(undefined, "lab");
    await ready();
    expect(pill("route").className).toContain("idle");
    expect(pill("route").title).toBe("every run in view has the same route");
    expect(pill("tracked").className).toContain("idle");
    expect(pill("status").className).toContain("idle");
    fireEvent.click(pill("route"));
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("Escape closes the popover and returns focus to its pill", async () => {
    mockFetch();
    renderBoard();
    await ready();
    fireEvent.click(pill("status"));
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(document.activeElement).toBe(pill("status"));
  });

  it("'only' selects just that value; Clear all empties every dimension", async () => {
    mockFetch();
    renderBoard(sel({ role: ["coder", "reviewer"], time: ["24h"] }));
    await ready();
    expect(screen.getByText("2 selected")).toBeInTheDocument();
    fireEvent.click(pill("role"));
    fireEvent.click(within(screen.getByRole("dialog")).getAllByText("only")[0]);
    expect(rowIds()).toEqual(["a1", "a2"]);
    fireEvent.click(screen.getByText("Clear all"));
    expect(rowIds().length).toBe(4);
    expect(screen.queryByText("Clear all")).toBeNull();
  });

  it("a filter that matches nothing says how to get runs back, and keeps the selected value to untick", async () => {
    mockFetch();
    renderBoard(sel({ role: ["coder"], status: ["running"] }));
    await ready();
    expect(screen.getByText(/no runs match these filters/)).toBeInTheDocument();
    fireEvent.click(pill("role"));
    const zero = within(screen.getByRole("dialog")).getByLabelText(/coder/);
    expect((zero as HTMLInputElement).checked).toBe(true);
  });

  it("time is one window by latest activity, against the frozen clock", async () => {
    mockFetch();
    renderBoard(sel({ time: ["24h"] }));
    await ready();
    expect(rowIds()).toEqual(["b1", "a1", "a2"]);
  });

  it("a deep link with filters seeds the board", async () => {
    mockFetch();
    renderBoard(sel({ status: ["running"] }));
    await ready();
    expect(rowIds()).toEqual(["b1"]);
  });
});
