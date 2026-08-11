import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RunsBoard } from "./RunsBoard";

function renderBoard(initialKind: "all" | "mission" | "dispatch" | "lab" = "all") {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <RunsBoard initialKind={initialKind} />
    </QueryClientProvider>,
  );
}

const RUNS = [
  { id: "m1", kind: "mission", status: "complete", tracked: true, updated_ts: 300, machine: "MacBook-Pro" },
  { id: "d1", kind: "dispatch", status: "running", tracked: true, role: "coder", updated_ts: 200, machine: "MacBook-Pro" },
  { id: "l1", kind: "lab", status: "abandoned", tracked: true, updated_ts: 100, machine: "MacBook-Pro" },
];

/** The board's own fixture carries a lab row, and the lab-source notice
 *  only renders when there are NO lab runs — so a three-state test must
 *  start from a runs set without one, or it silently asserts nothing. */
const NO_LAB_RUNS = RUNS.filter((r) => r.kind !== "lab");

function mockFetch(runsOk = true, labRunsOk = true, labSource: Record<string, unknown> = {}, runs: unknown[] = RUNS) {
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      if (url === "/runs") {
        return Promise.resolve(
          runsOk
            ? new Response(JSON.stringify({ runs, generated_at_ms: 1 }), { status: 200 })
            : new Response("boom", { status: 500 }),
        );
      }
      if (url === "/lab/runs") {
        return Promise.resolve(
          labRunsOk
            ? new Response(JSON.stringify({ configured: true, dir: "/lab", exists: true, runs: [], ...labSource }), { status: 200 })
            : new Response("boom", { status: 500 }),
        );
      }
      return Promise.resolve(new Response("not found", { status: 404 }));
    }),
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("RunsBoard", () => {
  it("renders the pending state before both fetches resolve", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    renderBoard();
    expect(screen.getByRole("status", { name: /loading runs/i })).toBeInTheDocument();
  });

  it("renders one row per run, newest-activity-first, once both fetches resolve", async () => {
    mockFetch();
    renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    const rows = screen.getAllByText(/^(m1|d1|l1)$/).map((el) => el.textContent);
    expect(rows).toEqual(["m1", "d1", "l1"]); // updated_ts 300 > 200 > 100
  });

  it("shows the kind counts in the filter bar", async () => {
    mockFetch();
    const { container } = renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    expect(container.querySelector('[data-arg="all"]')?.textContent).toContain("3");
    expect(container.querySelector('[data-arg="mission"]')?.textContent).toContain("1");
  });

  it("clicking a kind chip re-filters the already-loaded list (no new fetch)", async () => {
    mockFetch();
    const { container } = renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    const fetchCallsBefore = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls.length;

    fireEvent.click(container.querySelector('[data-arg="dispatch"]')!);
    await waitFor(() => expect(screen.queryByText("m1")).not.toBeInTheDocument());
    expect(screen.getByText("d1")).toBeInTheDocument();
    expect(screen.queryByText("l1")).not.toBeInTheDocument();

    expect((globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls.length).toBe(fetchCallsBefore);
  });

  it("the ◧ series toggle only appears under kind=lab, and switches to the grouped view", async () => {
    mockFetch();
    renderBoard("lab");
    await waitFor(() => expect(screen.getByText("l1")).toBeInTheDocument());
    const seriesToggle = screen.getByText("◧ series");
    expect(seriesToggle).toBeInTheDocument();

    fireEvent.click(seriesToggle);
    await waitFor(() => expect(screen.getByText(/lab series/)).toBeInTheDocument());
  });

  it("an untracked ghost row is rendered as non-interactive with an 'untracked' marker", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url === "/runs") {
          return Promise.resolve(
            new Response(
              JSON.stringify({
                runs: [{ id: "ghost", kind: "dispatch", status: "abandoned", tracked: false, updated_ts: 1 }],
                generated_at_ms: 1,
              }),
              { status: 200 },
            ),
          );
        }
        return Promise.resolve(new Response(JSON.stringify({ configured: true, dir: null, exists: null, runs: [] }), { status: 200 }));
      }),
    );
    renderBoard();
    await waitFor(() => expect(screen.getByText("ghost")).toBeInTheDocument());
    expect(screen.getByText("untracked")).toBeInTheDocument();
    expect(screen.getByText("ghost").closest(".labrunrow")).toHaveClass("flat");
  });

  it("degrades a /runs fetch failure to the empty-runs render (matches legacy's silent catch)", async () => {
    mockFetch(false, true);
    renderBoard();
    await waitFor(() => expect(screen.getByText(/no runs recorded yet/i)).toBeInTheDocument());
  });

  it("clicking a still-interactive row surfaces a visible 'not ported yet' notice, not a silent no-op", async () => {
    mockFetch();
    renderBoard();
    // "m1" (mission, tracked:true) is interactive — a real `data-act` target
    // in legacy (`gomission`). Its detail page doesn't exist in `/next` yet.
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    expect(screen.queryByText(/isn't in \/next yet/i)).not.toBeInTheDocument();

    fireEvent.click(screen.getByText("m1").closest(".labrunrow")!);

    const notice = screen.getByText(/isn't in \/next yet/i);
    expect(notice).toBeInTheDocument();
    expect(notice).toHaveAttribute("role", "status");
    expect(notice.textContent).toMatch(/open it in the classic viewer at \//i);
  });

  it("the not-ported notice also fires from a keyboard Enter activation", async () => {
    mockFetch();
    renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    fireEvent.keyDown(screen.getByText("m1").closest(".labrunrow")!, { key: "Enter" });
    expect(screen.getByText(/isn't in \/next yet/i)).toBeInTheDocument();
  });

  it("an untracked ghost row has no click affordance and never shows the notice", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url === "/runs") {
          return Promise.resolve(
            new Response(
              JSON.stringify({
                runs: [{ id: "ghost2", kind: "dispatch", status: "abandoned", tracked: false, updated_ts: 1 }],
                generated_at_ms: 1,
              }),
              { status: 200 },
            ),
          );
        }
        return Promise.resolve(new Response(JSON.stringify({ configured: true, dir: null, exists: null, runs: [] }), { status: 200 }));
      }),
    );
    renderBoard();
    await waitFor(() => expect(screen.getByText("ghost2")).toBeInTheDocument());
    fireEvent.click(screen.getByText("ghost2").closest(".labrunrow")!);
    expect(screen.queryByText(/isn't in \/next yet/i)).not.toBeInTheDocument();
  });

  it("switching kind chips clears a lingering not-ported notice", async () => {
    mockFetch();
    const { container } = renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    fireEvent.click(screen.getByText("m1").closest(".labrunrow")!);
    expect(screen.getByText(/isn't in \/next yet/i)).toBeInTheDocument();

    fireEvent.click(container.querySelector('[data-arg="dispatch"]')!);
    expect(screen.queryByText(/isn't in \/next yet/i)).not.toBeInTheDocument();
  });

  // (#1585's bug class) `/lab/runs` distinguishes THREE reasons the lab tab
  // can be empty, and the operator acts differently on each. They were
  // rendered correctly but pinned by nothing — no unit mock set
  // `configured:false` or `exists:false`, and the recorded corpus is
  // `configured:true, exists:true` with 133 runs. A regression collapsing
  // them into one reassuring "no runs" line would have shipped green, which
  // is exactly how 247 real runs once read as an empty tab.
  it("no lab source wired reads as UNWIRED, not as an empty lab", async () => {
    mockFetch(true, true, { configured: false, dir: null, exists: null }, NO_LAB_RUNS);
    const { container } = renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    fireEvent.click(container.querySelector('[data-arg="lab"]')!);
    await waitFor(() => expect(screen.getByText(/no lab-run source wired/i)).toBeInTheDocument());
    expect(screen.queryByText(/no lab runs found/i)).toBeNull();
  });

  it("a configured-but-missing lab dir names the dir, rather than claiming no runs", async () => {
    mockFetch(true, true, { configured: true, dir: "/nope", exists: false }, NO_LAB_RUNS);
    const { container } = renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    fireEvent.click(container.querySelector('[data-arg="lab"]')!);
    await waitFor(() => expect(screen.getByText(/does not exist yet \(\/nope\)/i)).toBeInTheDocument());
    expect(screen.queryByText(/no lab runs found/i)).toBeNull();
  });

  it("a healthy but empty lab dir DOES read as no runs — the inverted case", async () => {
    // Guards the fix from over-firing: a wired, existing, genuinely empty
    // lab must not be reported as a configuration problem.
    mockFetch(true, true, { configured: true, dir: "/lab", exists: true }, NO_LAB_RUNS);
    const { container } = renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    fireEvent.click(container.querySelector('[data-arg="lab"]')!);
    await waitFor(() => expect(screen.getByText(/no lab runs found under the configured lab dir/i)).toBeInTheDocument());
    expect(screen.queryByText(/no lab-run source wired/i)).toBeNull();
  });

  // Keyboard-accessibility structure — the runs-lens `role="button"` chips
  // (RunsBar's kind filter + series toggle) and the `.runmore` "show all"
  // row are click-only divs/spans with tabIndex but no key handler prior to
  // this fix: reachable by Tab, unactivatable by keyboard. Text-only
  // assertions can't see either defect (the click handler still exists and
  // still produces the right text) — these assert on the STRUCTURE (the
  // attributes) and on Enter/Space actually firing the handler.
  describe("keyboard operability", () => {
    it("every runchip in the filter bar is a real role=button with tabIndex", async () => {
      mockFetch();
      const { container } = renderBoard();
      await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
      const chips = container.querySelectorAll(".runchip");
      expect(chips.length).toBeGreaterThan(0);
      chips.forEach((chip) => {
        expect(chip).toHaveAttribute("role", "button");
        expect(chip).toHaveAttribute("tabIndex", "0");
      });
    });

    it("a kind chip re-filters on Enter, the same as a click", async () => {
      mockFetch();
      const { container } = renderBoard();
      await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
      fireEvent.keyDown(container.querySelector('[data-arg="dispatch"]')!, { key: "Enter" });
      await waitFor(() => expect(screen.queryByText("m1")).not.toBeInTheDocument());
      expect(screen.getByText("d1")).toBeInTheDocument();
    });

    it("a kind chip re-filters on Space, the same as a click", async () => {
      mockFetch();
      const { container } = renderBoard();
      await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
      fireEvent.keyDown(container.querySelector('[data-arg="dispatch"]')!, { key: " " });
      await waitFor(() => expect(screen.queryByText("m1")).not.toBeInTheDocument());
      expect(screen.getByText("d1")).toBeInTheDocument();
    });

    it("the ◧ series toggle switches to the grouped view on Enter", async () => {
      mockFetch();
      renderBoard("lab");
      await waitFor(() => expect(screen.getByText("l1")).toBeInTheDocument());
      fireEvent.keyDown(screen.getByText("◧ series"), { key: "Enter" });
      await waitFor(() => expect(screen.getByText(/lab series/)).toBeInTheDocument());
    });

    it("'show all N more' is a real role=button and expands on Enter/Space", async () => {
      const manyRuns = Array.from({ length: 30 }, (_, i) => ({
        id: `r${i}`,
        kind: "dispatch",
        status: "complete",
        tracked: true,
        updated_ts: 30 - i,
      }));
      mockFetch(true, true, {}, manyRuns);
      const { container } = renderBoard();
      await waitFor(() => expect(screen.getByText("r0")).toBeInTheDocument());

      const more = container.querySelector(".runmore")!;
      expect(more).toHaveAttribute("role", "button");
      expect(more).toHaveAttribute("tabIndex", "0");
      expect(more.textContent).toMatch(/show all 30/);

      fireEvent.keyDown(more, { key: " " });
      await waitFor(() => expect(container.querySelector(".runmore")).not.toBeInTheDocument());
      expect(screen.getByText("r29")).toBeInTheDocument();
    });
  });
});
