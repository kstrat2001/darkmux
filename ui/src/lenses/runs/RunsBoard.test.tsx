import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RunsBoard } from "./RunsBoard";
import { todayUTC } from "../../lib/flow";
import { useHashRoute } from "../../lib/useHashRoute";

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
  // (#1900 QA nit) A shared belt-and-suspenders reset — several tests below
  // already reset `location.hash` themselves in a `finally`, but this makes
  // the "hash starts empty" assumption a property of the SUITE rather than
  // something each new hash-writing test has to remember to arrange for
  // itself.
  window.location.hash = "";
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
  /** The row's nav affordance. Measured on a real daemon: 29 of 489 rows
   *  (`kind==="mission"`, untracked, no `session_id` — an ACP-ephemeral or an
   *  aged-out review whose session records have left the flow window) resolve
   *  to `runDestination`'s `"none"` and swallow the click. Those rows are
   *  correctly inert already (no `role`, no handler), but they LOOKED
   *  identical to a live row, so a dead click read as a broken control.
   *
   *  The chevron is the fix rather than hover alone, because hover does not
   *  exist on a phone — and hover on every row would promise a destination
   *  6% of the time there is none. Its PRESENCE is the affordance.
   *
   *  Asserted via `data-nav` rather than a rendered element: the chevron is a
   *  CSS `::after` keyed on that attribute, because a text node would land in
   *  `#stage`'s extracted text and break the frozen parity goldens. CI caught
   *  exactly that — `next-parity-runs` reddened on two goldens while the
   *  console suite (the one run locally) stayed green. */
  it("a row that has a destination renders the nav chevron and is interactive", async () => {
    mockFetch(true, true, {}, [
      { id: "m-live", kind: "mission", status: "complete", tracked: true, updated_ts: 400 },
    ]);
    renderBoard();
    await waitFor(() => expect(screen.getByText("m-live")).toBeInTheDocument());
    const row = document.querySelector(".labrunrow")!;
    expect(row).toHaveAttribute("role", "button");
    expect(row).not.toHaveClass("flat");
    expect(row).toHaveAttribute("data-nav", "1");
  });

  it("a row with NO destination renders no chevron and stays inert", async () => {
    // Untracked mission with no session_id — `runDestination` -> "none".
    mockFetch(true, true, {}, [
      { id: "acp-ephemeral-x", kind: "mission", status: "abandoned", tracked: false, updated_ts: 400 },
    ]);
    renderBoard();
    await waitFor(() => expect(screen.getByText("acp-ephemeral-x")).toBeInTheDocument());
    const row = document.querySelector(".labrunrow")!;
    expect(row).not.toHaveAttribute("role");
    expect(row).toHaveClass("flat");
    expect(row).not.toHaveAttribute("data-nav");
  });

  it("renders the unparseable status badge with its own class and text", async () => {
    mockFetch(true, true, {}, [{ id: "m-broken", kind: "mission", status: "unparseable", tracked: true, updated_ts: 400 }]);
    renderBoard();
    await waitFor(() => expect(screen.getByText("m-broken")).toBeInTheDocument());
    const badge = screen.getByText("unparseable");
    expect(badge).toHaveClass("labbadge", "unparseable");
  });

  /** (#1907) The badge's CLASS stays keyed on `run.status` (so the dim
   *  `.labbadge.abandoned` styling is unchanged) but its TEXT now reads
   *  `abandoned_reason` — a deliberate `mission abort` renders "aborted";
   *  a run with no terminal record ever written (or an older server that
   *  didn't send the field at all) renders "no ending recorded". Both
   *  must be real, distinguishable text in the DOM, not the same word. */
  it("renders an abandoned row's badge text from abandoned_reason, not the bare status word", async () => {
    mockFetch(true, true, {}, [
      { id: "aborted-1", kind: "mission", status: "abandoned", tracked: true, updated_ts: 400, abandoned_reason: "aborted" },
      { id: "stale-1", kind: "dispatch", status: "abandoned", tracked: false, updated_ts: 300 },
    ]);
    renderBoard();
    await waitFor(() => expect(screen.getByText("aborted-1")).toBeInTheDocument());

    const abortedBadge = screen.getByText("aborted");
    expect(abortedBadge).toHaveClass("labbadge", "abandoned");

    const staleBadge = screen.getByText("no ending recorded");
    expect(staleBadge).toHaveClass("labbadge", "abandoned");

    expect(screen.queryByText("abandoned", { selector: ".labbadge" })).not.toBeInTheDocument();
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

  it("(#1900, session_id wiring #1915) a terminated, untracked dispatch row with flow records is interactive and activating it navigates to #session=<id>", async () => {
    // "ghost" is `kind: "dispatch", tracked: false` — server-side, EVERY
    // such row is synthesized only for a flow session that saw a real
    // `dispatch start` record (`ghost_runs`'s `has_start` gate in
    // `crates/darkmux-serve/src/runs.rs`), so it always has something to
    // show via `/flow-session/<id>` even with no mission graph behind it.
    // The "untracked" chip still shows (it's an honest label — no durable
    // run record backs this row) but it must no longer mean unopenable.
    // `session_id: "ghost"` matches the real wire shape: `ghost_runs`
    // populates it from the row's OWN id (#1915) — the client no longer
    // special-cases `kind === "dispatch"`, it reads `run.session_id`
    // uniformly, so this fixture has to carry it like a real server
    // response would.
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url === "/runs") {
          return Promise.resolve(
            new Response(
              JSON.stringify({
                runs: [{ id: "ghost", kind: "dispatch", status: "abandoned", tracked: false, session_id: "ghost", updated_ts: 1 }],
                generated_at_ms: 1,
              }),
              { status: 200 },
            ),
          );
        }
        return Promise.resolve(new Response(JSON.stringify({ configured: true, dir: null, exists: null, runs: [] }), { status: 200 }));
      }),
    );
    try {
      renderBoard();
      await waitFor(() => expect(screen.getByText("ghost")).toBeInTheDocument());
      expect(screen.getByText("untracked")).toBeInTheDocument();
      const row = screen.getByText("ghost").closest(".labrunrow")!;
      expect(row).not.toHaveClass("flat");
      expect(row).toHaveAttribute("role", "button");
      expect(row).toHaveAttribute("tabIndex", "0");

      fireEvent.click(row);
      expect(window.location.hash).toBe("#session=ghost");
      // No mission-graph gate applies here — `/flow-session/<id>` is a
      // plain daemon fetch, same precedent as `FleetLens.tsx`'s activity-
      // lane bars, which navigate to `#session=<sid>` ungated.
      expect(screen.queryByText(/needs a running daemon/i)).not.toBeInTheDocument();
    } finally {
      window.location.hash = "";
    }
  });

  it("(#1915) an untracked MISSION row that carries a session_id is interactive and activating it navigates to #session=<id>", async () => {
    // This is the #1915 defect itself: `kind: "mission", tracked: false`
    // is `flow_mission_to_run`'s shape (#1705 — a peer's mission this
    // daemon only sees via the fleet stream), and it USED to always read
    // as flat because the old `interactive`/`runDestination` logic only
    // ever special-cased `kind === "dispatch"`. But the server picks a
    // representative session for a mission row exactly like it does for
    // role/model/route, so a mission carrying `session_id` has just as
    // real a destination as the dispatch ghost above — same drill, same
    // `#session=<id>` hash, no mission-graph gate.
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url === "/runs") {
          return Promise.resolve(
            new Response(
              JSON.stringify({
                runs: [
                  {
                    id: "peer-mission-with-session",
                    kind: "mission",
                    status: "running",
                    tracked: false,
                    session_id: "peer-session-1",
                    updated_ts: 1,
                  },
                ],
                generated_at_ms: 1,
              }),
              { status: 200 },
            ),
          );
        }
        return Promise.resolve(new Response(JSON.stringify({ configured: true, dir: null, exists: null, runs: [] }), { status: 200 }));
      }),
    );
    try {
      renderBoard();
      await waitFor(() => expect(screen.getByText("peer-mission-with-session")).toBeInTheDocument());
      expect(screen.getByText("untracked")).toBeInTheDocument();
      const row = screen.getByText("peer-mission-with-session").closest(".labrunrow")!;
      expect(row).not.toHaveClass("flat");
      expect(row).toHaveAttribute("role", "button");

      fireEvent.click(row);
      expect(window.location.hash).toBe("#session=peer-session-1");
      expect(screen.queryByText(/needs a running daemon/i)).not.toBeInTheDocument();
    } finally {
      window.location.hash = "";
    }
  });

  it("(#1915) a row with genuinely nothing behind it — an untracked mission with no session_id at all — stays non-interactive", async () => {
    // `kind: "mission", tracked: false`, no `session_id`: a mission this
    // daemon knows only from a terminal record, with no dispatch session
    // ever joined to it. This is the ONLY untracked shape that still has
    // truly nothing to open — the case #1900's fix left behind and #1915
    // fixed everywhere else.
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url === "/runs") {
          return Promise.resolve(
            new Response(
              JSON.stringify({
                runs: [{ id: "peer-mission", kind: "mission", status: "running", tracked: false, updated_ts: 1 }],
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
    await waitFor(() => expect(screen.getByText("peer-mission")).toBeInTheDocument());
    expect(screen.getByText("untracked")).toBeInTheDocument();
    const row = screen.getByText("peer-mission").closest(".labrunrow")!;
    expect(row).toHaveClass("flat");
    expect(row).not.toHaveAttribute("role");
    fireEvent.click(row);
    expect(window.location.hash).toBe("");
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

  it("(#1900, session_id wiring #1915) an untracked dispatch ghost row also opens #session=<id> from a keyboard Enter activation, and never shows the mission-graph notice", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url === "/runs") {
          return Promise.resolve(
            new Response(
              JSON.stringify({
                runs: [{ id: "ghost2", kind: "dispatch", status: "abandoned", tracked: false, session_id: "ghost2", updated_ts: 1 }],
                generated_at_ms: 1,
              }),
              { status: 200 },
            ),
          );
        }
        return Promise.resolve(new Response(JSON.stringify({ configured: true, dir: null, exists: null, runs: [] }), { status: 200 }));
      }),
    );
    try {
      renderBoard();
      await waitFor(() => expect(screen.getByText("ghost2")).toBeInTheDocument());
      fireEvent.keyDown(screen.getByText("ghost2").closest(".labrunrow")!, { key: "Enter" });
      expect(window.location.hash).toBe("#session=ghost2");
      expect(screen.queryByText(/needs a running daemon/i)).not.toBeInTheDocument();
    } finally {
      window.location.hash = "";
    }
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

/**
 * (#1920) A harness that mirrors `App.tsx`'s ACTUAL `RunsBoard` wiring —
 * `initialKind`/`initialRun`/`initialMachineUid` re-derived from
 * `useHashRoute()` on every render (`App.tsx`'s `renderRoute`), not fixed
 * props handed to `RunsBoard` once at construction. Every other test in
 * this file uses `renderBoard()`, which constructs `RunsBoard` directly
 * with props that never change after mount — so `suppressResyncRef`'s
 * guard (in `RunsBoard.tsx`, against the deep-link resync effect's own
 * echo of `onLabRunUnresolvable`'s `writeHash` call) can never even be
 * exercised there: the race it guards against only exists when
 * `initialRun` is genuinely RE-DERIVED from the URL after mount, the way
 * `App.tsx` does and `renderBoard()` structurally cannot.
 */
function AppLikeRunsHarness() {
  const route = useHashRoute();
  if (route.kind !== "runs") return null;
  return <RunsBoard initialKind={route.runsKind} initialRun={route.run} initialMachineUid={route.machine} />;
}

describe("RunsBoard — deep-link wiring parity with App.tsx (#1920)", () => {
  afterEach(() => {
    window.location.hash = "";
    document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
    vi.unstubAllGlobals();
  });

  // (#1920) `RunsBoard.tsx`'s own `onLabRunUnresolvable` sets
  // `suppressResyncRef.current = true` before clearing `labRunDir`, so the
  // deep-link resync effect recognizes its OWN echo (the `writeHash` call
  // inside `onLabRunUnresolvable` changes `location.href` without firing a
  // real `hashchange`) rather than mistaking it for a fresh external
  // deep-link and wiping the "couldn't open run" notice back out via its
  // own `setRowClickNotice(null)`. `RunsBoard.test.tsx`'s direct-construction
  // tests can't reproduce this — `initialRun` is fixed for the component's
  // whole lifetime there, so the resync effect's guarded branch never runs
  // against a genuinely re-derived prop. This test drives a REAL deep link
  // through `useHashRoute()`, matching `App.tsx`'s own wiring, and forces
  // the exact re-render `App.tsx` would eventually get from some unrelated
  // cause (a poll, a refetch) by firing a `hashchange` after the notice
  // first appears — the same echo the guard exists to recognize.
  it("a deep link to an unresolvable lab run keeps its notice after the echoed re-render, not wiped back out", async () => {
    mockFetch(); // /runs, /lab/runs both ok; /lab/run/detail?dir=bad-dir falls through to this mock's 404 default
    window.location.hash = "#lens=runs&kind=lab&run=bad-dir";

    // No <meta name="darkmux-mode"> is injected by this test harness, so
    // `missionGraphReachable()` defaults false and the daemon-less-static
    // notice would render instead — inject it, matching a REAL `darkmux
    // serve`-served page (same pattern the mission-row test above uses),
    // so the branch under test is the "couldn't open run" one #1920 names.
    const meta = document.createElement("meta");
    meta.name = "darkmux-mode";
    meta.content = "live";
    document.head.appendChild(meta);

    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <AppLikeRunsHarness />
      </QueryClientProvider>,
    );

    const notice = /couldn't open run "bad-dir"/;
    await waitFor(() => expect(screen.getByText(notice)).toBeInTheDocument());

    // `onLabRunUnresolvable`'s own `writeHash` (a `replaceState`, per
    // `hashSync.ts`'s own doc) already moved `location.href` to `run=null`
    // without dispatching `hashchange`. Firing one now is the stand-in for
    // "the next unrelated App re-render" `RunsBoard.tsx`'s own comment
    // names as the real-world trigger — it forces `useHashRoute()` to
    // recompute and hand `RunsBoard` a fresh (now-null) `initialRun`,
    // which is exactly the echo `suppressResyncRef` exists to recognize.
    await act(async () => {
      window.dispatchEvent(new Event("hashchange"));
    });

    expect(screen.getByText(notice)).toBeInTheDocument();
  });
});
