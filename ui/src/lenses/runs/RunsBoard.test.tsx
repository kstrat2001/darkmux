import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RunsBoard } from "./RunsBoard";
import { todayUTC } from "../../lib/flow";

function renderBoard(
  initialKind: "all" | "mission" | "dispatch" | "lab" = "all",
  initialRun: string | null = null,
  initialMachineUid: string | null = null,
) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <RunsBoard initialKind={initialKind} initialRun={initialRun} initialMachineUid={initialMachineUid} />
    </QueryClientProvider>,
  );
}

// The `location.href` cross-document-navigation stub this file used to need
// for the mission-row test (`withHrefStub`) is gone (#1868) — that row now
// navigates in-app via `location.hash`, which jsdom handles natively, same
// as every other hash-driven test in this file.

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

  /** (#1881, QA-caught) `RunStatus` gained a sixth value (`unparseable`,
   *  for an envelope this binary couldn't resolve a verdict for) and
   *  nothing in this file exercised it — the badge path is fully generic
   *  (`labbadge ${run.status}`), so the risk was low, but the styling was
   *  asserted by nothing. */
  it("renders the unparseable status badge with its own class and text", async () => {
    mockFetch(true, true, {}, [{ id: "m-broken", kind: "mission", status: "unparseable", tracked: true, updated_ts: 400 }]);
    renderBoard();
    await waitFor(() => expect(screen.getByText("m-broken")).toBeInTheDocument());
    const badge = screen.getByText("unparseable");
    expect(badge).toHaveClass("labbadge", "unparseable");
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

  it("clicking a tracked mission row navigates in-app to #mission=<id> when a real daemon is behind the page (#1868)", async () => {
    mockFetch();
    // No <meta name="darkmux-mode"> is injected by this test harness, so
    // `missionGraphReachable()` defaults false — inject it, matching what a
    // REAL `darkmux serve`-served page does (see `Masthead.tsx`'s own
    // `injectedMeta` doc). Unlike the pre-#1868 version of this test, no
    // `location.href` stub is needed: the destination is now a real
    // in-app `location.hash` write (a `hashchange`-firing navigation, not a
    // cross-document one), which jsdom handles natively.
    const meta = document.createElement("meta");
    meta.name = "darkmux-mode";
    meta.content = "live";
    document.head.appendChild(meta);
    try {
      renderBoard();
      await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
      fireEvent.click(screen.getByText("m1").closest(".labrunrow")!);
      expect(window.location.hash).toBe("#mission=m1");
    } finally {
      meta.remove();
      window.location.hash = "";
    }
  });

  it("clicking a tracked mission row with NO daemon behind the page surfaces a visible, honest notice — not a silent no-op or a broken nav", async () => {
    mockFetch();
    renderBoard();
    // "m1" (mission, tracked:true) is interactive — a real `data-act` target
    // in legacy (`gomission`). No <meta name="darkmux-mode"> is injected
    // here (matching every automated harness — see `injectedMeta`'s doc),
    // so there is genuinely no daemon behind this page to navigate to.
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    expect(screen.queryByText(/needs a running daemon/i)).not.toBeInTheDocument();

    fireEvent.click(screen.getByText("m1").closest(".labrunrow")!);

    const notice = screen.getByText(/needs a running daemon/i);
    expect(notice).toBeInTheDocument();
    expect(notice).toHaveAttribute("role", "status");
    expect(notice.textContent).toMatch(/this static build has no mission graph data to show/i);
  });

  it("the daemon-less notice also fires from a keyboard Enter activation", async () => {
    mockFetch();
    renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    fireEvent.keyDown(screen.getByText("m1").closest(".labrunrow")!, { key: "Enter" });
    expect(screen.getByText(/needs a running daemon/i)).toBeInTheDocument();
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
    expect(screen.queryByText(/needs a running daemon/i)).not.toBeInTheDocument();
  });

  it("switching kind chips clears a lingering row-click notice", async () => {
    mockFetch();
    const { container } = renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    fireEvent.click(screen.getByText("m1").closest(".labrunrow")!);
    expect(screen.getByText(/needs a running daemon/i)).toBeInTheDocument();

    fireEvent.click(container.querySelector('[data-arg="dispatch"]')!);
    expect(screen.queryByText(/needs a running daemon/i)).not.toBeInTheDocument();
  });

  it("clicking a lab row opens the lab-run detail pane, and its back link returns to the list", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url === "/runs") return Promise.resolve(new Response(JSON.stringify({ runs: RUNS, generated_at_ms: 1 }), { status: 200 }));
        if (url === "/lab/runs")
          return Promise.resolve(new Response(JSON.stringify({ configured: true, dir: "/lab", exists: true, runs: [] }), { status: 200 }));
        if (url.startsWith("/lab/run/detail")) return Promise.resolve(new Response(JSON.stringify({ dir: "l1", funnels: [], scores: null }), { status: 200 }));
        if (url.startsWith("/lab/run/events")) return Promise.resolve(new Response(JSON.stringify({ lines: [], next_offset: 0, finished: false }), { status: 200 }));
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
    const { container } = renderBoard();
    await waitFor(() => expect(screen.getByText("l1")).toBeInTheDocument());

    fireEvent.click(screen.getByText("l1").closest(".labrunrow")!);
    await waitFor(() => expect(screen.getByText("‹ runs")).toBeInTheDocument());
    expect(screen.getByText(/· l1/)).toBeInTheDocument();
    expect(container.querySelector('[data-arg="all"]')).toBeNull(); // the kind chips are gone in this view

    fireEvent.click(screen.getByText("‹ runs"));
    await waitFor(() => expect(screen.getByText("l1")).toBeInTheDocument());
    expect(screen.queryByText("‹ runs")).not.toBeInTheDocument();
  });

  it("a deep-link into kind=lab with a run= param opens the lab-run detail pane directly, on first render", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url.startsWith("/lab/run/detail")) return Promise.resolve(new Response(JSON.stringify({ dir: "live/gate-1", funnels: [], scores: null }), { status: 200 }));
        if (url.startsWith("/lab/run/events")) return Promise.resolve(new Response(JSON.stringify({ lines: [], next_offset: 0, finished: false }), { status: 200 }));
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
    renderBoard("lab", "live/gate-1");
    await waitFor(() => expect(screen.getByText(/· live\/gate-1/)).toBeInTheDocument());
    // Never fetched /runs or /lab/runs's list-only data path before landing
    // here — the detail pane doesn't wait on it (matches legacy: drillLabRun
    // never blocks on loadRuns()/loadLabRuns()).
    expect(screen.getByText("‹ runs")).toBeInTheDocument();
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

/**
 * (#1801) `darkmux-runs-src`/`darkmux-lab-runs-src` — the static demo's
 * committed fixture files, read instead of `GET /runs`/`GET /lab/runs`
 * (there is no daemon behind the static demo to serve either). Via
 * `staticSource.ts`'s `resolveRunsSrc()`/`resolveLabRunsSrc()`.
 */
describe("RunsBoard — the static-demo runs-src override (#1801)", () => {
  function injectMeta(name: string, content: string) {
    const el = document.createElement("meta");
    el.setAttribute("name", name);
    el.setAttribute("content", content);
    document.head.appendChild(el);
  }

  afterEach(() => {
    document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
  });

  function mockStaticSrc() {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url === "./demo-runs.json") {
          return Promise.resolve(new Response(JSON.stringify({ runs: RUNS, generated_at_ms: 1 }), { status: 200 }));
        }
        if (url === "./demo-lab-runs.json") {
          return Promise.resolve(
            new Response(JSON.stringify({ configured: true, dir: "/lab", exists: true, runs: [] }), { status: 200 }),
          );
        }
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
  }

  it("fetches the injected runs-src / lab-runs-src, never /runs or /lab/runs", async () => {
    injectMeta("darkmux-runs-src", "./demo-runs.json");
    injectMeta("darkmux-lab-runs-src", "./demo-lab-runs.json");
    mockStaticSrc();
    renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());

    const calls = (globalThis.fetch as unknown as { mock: { calls: unknown[][] } }).mock.calls.map((c) => String(c[0]));
    expect(calls).toContain("./demo-runs.json");
    expect(calls).toContain("./demo-lab-runs.json");
    expect(calls).not.toContain("/runs");
    expect(calls).not.toContain("/lab/runs");
  });

  // Inverted case: without the metas, the board keeps hitting the literal
  // daemon paths exactly as every other test in this file already proves —
  // restated here as its own assertion so this describe block doesn't rely
  // on file ordering to make the point.
  it("without the metas, it still fetches the literal /runs and /lab/runs paths", async () => {
    mockFetch();
    renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    const calls = (globalThis.fetch as unknown as { mock: { calls: unknown[][] } }).mock.calls.map((c) => String(c[0]));
    expect(calls).toContain("/runs");
    expect(calls).toContain("/lab/runs");
  });
});

/**
 * (#1809, finishing #1508 step 4) The machine pin — `#lens=runs&machine=<uid>`.
 *
 * `Run.machine` is a NAME (`machine_id`), not a uid (see `format.ts`'s
 * `runsForMachine` doc) — so pinning by uid needs a live `/flow/<date>` +
 * `/fleet/machines/live` window to resolve which name(s) that uid has
 * appeared under (`lib/flow.ts::machineNames`). `mockPinnedFetch` below
 * serves both, unlike this file's plain `mockFetch` (which 404s them,
 * fine for every OTHER test here — an unpinned board never resolves a
 * uid at all).
 */
describe("RunsBoard — the machine pin (#1809)", () => {
  afterEach(() => {
    window.location.hash = "";
  });

  const PINNED_RUNS = [
    { id: "m1", kind: "mission", status: "complete", tracked: true, updated_ts: 300, machine: "MacBook-Pro" },
    { id: "d1", kind: "dispatch", status: "running", tracked: true, role: "coder", updated_ts: 200, machine: "MacBook-Pro" },
    { id: "l1", kind: "lab", status: "abandoned", tracked: true, updated_ts: 150, machine: "MacBook-Pro" },
    // A different machine — must never appear under the u1 pin.
    { id: "m2", kind: "mission", status: "complete", tracked: true, updated_ts: 250, machine: "studio" },
    // Real tracked work with no recorded machine attribution at all — must
    // never appear under ANY pin (see `runsForMachine`'s own doc for why
    // this is the honest call, not a bug).
    { id: "g1", kind: "dispatch", status: "complete", tracked: true, updated_ts: 50 },
  ];

  const LAB_RUNS_FIXTURE = [{ dir: "l1", mtime_ms: 1, case_ids: [], bundles: 1, raw_flags: 0, deduped_flags: 0, confirmed: 0, needs_check: 0, archived: 0, degenerate: false, finished: true }];

  function mockPinnedFetch(opts: {
    runs?: unknown[];
    /** Extra flow records beyond the default single `u1 -> MacBook-Pro`
     * mapping — used by the multi-alias test to add a SECOND name for the
     * same uid. */
    extraFlowToday?: unknown[];
  } = {}) {
    const today = todayUTC();
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        const path = String(url);
        if (path === "/runs") {
          return Promise.resolve(new Response(JSON.stringify({ runs: opts.runs ?? PINNED_RUNS, generated_at_ms: 1 }), { status: 200 }));
        }
        if (path === "/lab/runs") {
          return Promise.resolve(
            new Response(JSON.stringify({ configured: true, dir: "/lab", exists: true, runs: LAB_RUNS_FIXTURE }), { status: 200 }),
          );
        }
        if (path === `/flow/${today}`) {
          return Promise.resolve(
            new Response(
              JSON.stringify([{ ts: `${today}T00:00:00Z`, machine_uid: "u1", machine_id: "MacBook-Pro" }, ...(opts.extraFlowToday ?? [])]),
              { status: 200 },
            ),
          );
        }
        if (path.startsWith("/flow/")) return Promise.resolve(new Response(JSON.stringify([]), { status: 200 }));
        if (path === "/fleet/machines/live") {
          return Promise.resolve(
            new Response(JSON.stringify({ machines: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }),
          );
        }
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
  }

  it("filters the flat row list to the pinned machine's alias set", async () => {
    mockPinnedFetch();
    renderBoard("all", null, "u1");
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    expect(screen.getByText("d1")).toBeInTheDocument();
    // The other machine and the unattributed row are both absent.
    expect(screen.queryByText("m2")).not.toBeInTheDocument();
    expect(screen.queryByText("g1")).not.toBeInTheDocument();
  });

  it("scopes the kind-chip counts to the pinned machine, not the whole fleet", async () => {
    mockPinnedFetch();
    const { container } = renderBoard("all", null, "u1");
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    // 3 rows carry machine "MacBook-Pro" (m1, d1, l1) — m2 (studio) and g1
    // (unattributed) are excluded from the "all" count under the pin.
    expect(container.querySelector('[data-arg="all"]')?.textContent).toContain("3");
    expect(container.querySelector('[data-arg="mission"]')?.textContent).toContain("1");
  });

  it("names the pinned machine in a visible, clickable chip", async () => {
    mockPinnedFetch();
    renderBoard("all", null, "u1");
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    expect(screen.getByText(/machine: MacBook-Pro/)).toBeInTheDocument();
  });

  it("clicking the chip clears the pin — back to every machine, via a real hash write", async () => {
    mockPinnedFetch();
    const { container } = renderBoard("all", null, "u1");
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    expect(screen.queryByText("m2")).not.toBeInTheDocument();

    fireEvent.click(container.querySelector('[data-act="clearmachine"]')!);

    await waitFor(() => expect(screen.getByText("m2")).toBeInTheDocument());
    expect(screen.queryByText(/machine: MacBook-Pro/)).not.toBeInTheDocument();
    expect(window.location.hash).toBe("#lens=runs");
  });

  // Inverted case: the UNPINNED board is untouched by any of the above —
  // every machine's rows show, and no chip renders at all. Guards against
  // the machine-pin feature accidentally narrowing the default view.
  it("an unpinned board still shows every machine, with no machine chip", async () => {
    mockPinnedFetch();
    renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    expect(screen.getByText("m2")).toBeInTheDocument();
    expect(screen.getByText("g1")).toBeInTheDocument();
    expect(screen.queryByText(/^machine:/)).not.toBeInTheDocument();
  });

  it("switching kind chips while pinned preserves the pin in the address bar", async () => {
    mockPinnedFetch();
    renderBoard("all", null, "u1");
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    fireEvent.click(document.querySelector('[data-arg="dispatch"]')!);
    await waitFor(() => expect(screen.queryByText("m1")).not.toBeInTheDocument());
    expect(screen.getByText("d1")).toBeInTheDocument();
    expect(window.location.hash).toBe("#lens=runs&kind=dispatch&machine=u1");
  });

  // The regression this whole feature exists to avoid shipping: matching
  // by a single resolved label instead of the full alias set. u1 here has
  // appeared under TWO names in the window (`MacBook-Pro` and
  // `MacBook-Pro.local`), and rows are split across both — a pin that only
  // matched `nameOf(uid)`'s first-found alias would silently drop half of
  // these.
  it("matches rows filed under EITHER of a uid's known aliases", async () => {
    mockPinnedFetch({
      runs: [
        { id: "old-alias", kind: "mission", status: "complete", tracked: true, updated_ts: 300, machine: "MacBook-Pro" },
        { id: "new-alias", kind: "mission", status: "complete", tracked: true, updated_ts: 200, machine: "MacBook-Pro.local" },
      ],
      extraFlowToday: [{ ts: `${todayUTC()}T01:00:00Z`, machine_uid: "u1", machine_id: "MacBook-Pro.local" }],
    });
    renderBoard("all", null, "u1");
    await waitFor(() => expect(screen.getByText("old-alias")).toBeInTheDocument());
    expect(screen.getByText("new-alias")).toBeInTheDocument();
  });

  it("the lab series view (kind=lab, ◧ series) is ALSO scoped to the pin, bridged via the shared dir/id", async () => {
    mockPinnedFetch();
    const { container } = renderBoard("lab", null, "u1");
    await waitFor(() => expect(screen.getByText("l1")).toBeInTheDocument());
    fireEvent.click(screen.getByText("◧ series"));
    await waitFor(() => expect(screen.getByText(/lab series/)).toBeInTheDocument());
    // l1 is the pinned machine's only lab run and has a recorded corpus
    // (`LAB_RUNS_FIXTURE`) — the series card renders for it.
    expect(container.querySelector(".labtaskcard")).toBeInTheDocument();
  });
});
