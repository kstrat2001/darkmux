import { FLEET_UID, lower } from "../../testing/machineFleet";
import { machineKeyHash } from "../../lib/machineKey";
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RunsBoard } from "./RunsBoard";
import { todayUTC } from "../../lib/flow";
import { useHashRoute } from "../../lib/useHashRoute";

function renderBoard(
  initialKind: "all" | "mission" | "dispatch" | "lab" = "all",
  initialRun: string | null = null,
  initialMachineKey: string | null = null,
) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <RunsBoard initialKind={initialKind} initialLab={initialRun} initialMachineKey={initialMachineKey} />
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

function mockFetch(runsOk = true, labRunsOk = true, labSource: Record<string, unknown> = {}, runs: unknown[] = RUNS, fleetView: unknown[] | null = null, liveSessions: string[] = []) {
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      if (url === "/fleet/dispatches/live") return Promise.resolve(new Response(JSON.stringify({ dispatches: liveSessions.map((session_id) => ({ session_id })), meta: { sources: { fleet: { state: "ok" } }, complete: true } }), { status: 200 }));
      if (url === "/fleet/view" && fleetView) return Promise.resolve(new Response(JSON.stringify({ machines: fleetView }), { status: 200 }));
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
   *  (`kind==="mission"`, untracked, no `dispatch_id`, an ACP-ephemeral or an
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

  // (operator, 2026-10-01) A lab row's verify word carries its outcome's
  // color: "pass" green, "FAIL" red. "—" (no verify result was recorded) is
  // neither good nor bad news and stays dim. The row's text is unchanged, so
  // the parity goldens, which read text, do not move.
  it("colors a lab row's verify word: pass green, FAIL red, the dash left dim", async () => {
    mockFetch(true, true, {}, [
      { id: "lab-pass", kind: "lab", status: "complete", tracked: true, updated_ts: 403, workload: "pepper-grinder", verify_passed: true },
      { id: "lab-fail", kind: "lab", status: "error", tracked: true, updated_ts: 402, workload: "pepper-grinder", verify_passed: false },
      { id: "lab-none", kind: "lab", status: "abandoned", tracked: true, updated_ts: 401, workload: "pepper-grinder" },
    ]);
    renderBoard();
    await waitFor(() => expect(screen.getByText("lab-none")).toBeInTheDocument());
    const rowOf = (id: string) => screen.getByText(id).closest(".labrunrow")!;
    const word = (id: string) => rowOf(id).querySelector("[data-verify]");
    expect(word("lab-pass")).toHaveAttribute("data-verify", "pass");
    expect(word("lab-pass")).toHaveTextContent(/^pass$/);
    expect(word("lab-fail")).toHaveAttribute("data-verify", "fail");
    expect(word("lab-fail")).toHaveTextContent(/^FAIL$/);
    expect(word("lab-none")).toBeNull();
    expect(rowOf("lab-fail").textContent).toContain("pepper-grinder · verify FAIL");
    expect(rowOf("lab-pass").textContent).toContain("pepper-grinder · verify pass");
    expect(rowOf("lab-none").textContent).toContain("pepper-grinder · verify \u2014");
  });

  it("a row with NO destination renders no chevron and stays inert", async () => {
    // Untracked mission with no dispatch_id, `runDestination` -> "none".
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
    expect(badge).toHaveClass("wstatus", "is-idle", "s-unparseable");
  });

  // (5.0 R3) A run recorded as running on a peer that is down has no live
  // evidence: its badge and its status filter both read "unknown".
  describe("a running run on a machine that is not reporting", () => {
    const peerRow = (liveness: "no_beat" | "live") => ({
      entry: { id: "studio", address: "a:1", added_unix_ms: 1 },
      is_this_machine: false,
      machine_uid: "u-studio",
      liveness,
      card: { state: "unreachable", reason: "listener_off", detail: null },
    });
    const RUN = [{ id: "peer-run", kind: "dispatch", status: "running", tracked: true, updated_ts: 400, machine: "studio", machine_uid: "u-studio" }];

    it("reads unknown, not running, and is filed under unknown", async () => {
      mockFetch(true, true, {}, RUN, [peerRow("no_beat")]);
      renderBoard();
      await waitFor(() => expect(screen.getByText("unknown", { selector: ".labbadge" })).toBeInTheDocument());
      const badge = screen.getByText("unknown", { selector: ".labbadge" });
      expect(badge).toHaveClass("wstatus", "is-idle", "s-unknown");
      expect(badge).not.toHaveAttribute("data-live");
      expect(badge.getAttribute("title")).toMatch(/not reporting/i);
      expect(screen.queryByText("running", { selector: ".labbadge" })).not.toBeInTheDocument();
    });

    const asked = (u: string) => (fetch as unknown as { mock: { calls: string[][] } }).mock.calls.some((c) => c[0] === u);

    it("still reads running while its machine is reporting", async () => {
      mockFetch(true, true, {}, RUN, [peerRow("live")]);
      renderBoard();
      await waitFor(() => expect(asked("/fleet/view") && asked("/fleet/dispatches/live")).toBe(true));
      await waitFor(() => expect(screen.getByText("running", { selector: ".labbadge" })).toBeInTheDocument());
    });

    it("a run whose session is live is never unknown, even when its machine's row reads down", async () => {
      mockFetch(true, true, {}, RUN, [peerRow("no_beat")], ["peer-run"]);
      renderBoard();
      await waitFor(() => expect(asked("/fleet/view") && asked("/fleet/dispatches/live")).toBe(true));
      await new Promise((r) => setTimeout(r, 100));
      expect(screen.getByText("running", { selector: ".labbadge" })).toBeInTheDocument();
    });
  });

  /** (F10/F11) A cut-off or partial run is `degraded`, never `complete`: its
   *  badge says so, in the same caution color `stopped` and `escalated` use. */
  it("renders the degraded status badge in the caution color, with its own word", async () => {
    mockFetch(true, true, {}, [{ id: "m-cut", kind: "mission", status: "degraded", tracked: true, updated_ts: 400 }]);
    renderBoard();
    await waitFor(() => expect(screen.getByText("m-cut")).toBeInTheDocument());
    const badge = screen.getByText("degraded");
    expect(badge).toHaveClass("wstatus", "is-degraded", "s-degraded");
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
    expect(abortedBadge).toHaveClass("wstatus", "is-stopped", "s-abandoned");

    const staleBadge = screen.getByText("no ending recorded");
    expect(staleBadge).toHaveClass("wstatus", "is-stopped", "s-abandoned");

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

  it("(#1900, dispatch_id wiring #1915) a terminated, untracked dispatch row with flow records is interactive and activating it navigates to #dispatch=<id>", async () => {
    // "ghost" is `kind: "dispatch", tracked: false` — server-side, EVERY
    // such row is synthesized only for a flow session that saw a real
    // `dispatch start` record (`ghost_runs`'s `has_start` gate in
    // `crates/darkmux-serve/src/runs.rs`), so it always has something to
    // show via `/flow-dispatch/<id>` even with no mission graph behind it.
    // The "untracked" chip still shows (it's an honest label — no durable
    // run record backs this row) but it must no longer mean unopenable.
    // `dispatch_id: "ghost"` matches the real wire shape: `ghost_runs`
    // populates it from the row's OWN id (#1915) — the client no longer
    // special-cases `kind === "dispatch"`, it reads `run.dispatch_id`
    // uniformly, so this fixture has to carry it like a real server
    // response would.
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url === "/runs") {
          return Promise.resolve(
            new Response(
              JSON.stringify({
                runs: [{ id: "ghost", kind: "dispatch", status: "abandoned", tracked: false, dispatch_id: "ghost", updated_ts: 1 }],
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
      expect(window.location.hash).toBe("#dispatch=ghost");
      // No mission-graph gate applies here — `/flow-dispatch/<id>` is a
      // plain daemon fetch, same precedent as `FleetLens.tsx`'s activity-
      // lane bars, which navigate to `#dispatch=<sid>` ungated.
      expect(screen.queryByText(/needs a running daemon/i)).not.toBeInTheDocument();
    } finally {
      window.location.hash = "";
    }
  });

  it("(#1915) an untracked MISSION row that carries a dispatch_id is interactive and activating it navigates to #dispatch=<id>", async () => {
    // This is the #1915 defect itself: `kind: "mission", tracked: false`
    // is `flow_mission_to_run`'s shape (#1705 — a peer's mission this
    // daemon only sees via the fleet stream), and it USED to always read
    // as flat because the old `interactive`/`runDestination` logic only
    // ever special-cased `kind === "dispatch"`. But the server picks a
    // representative session for a mission row exactly like it does for
    // role/model/route, so a mission carrying `dispatch_id` has just as
    // real a destination as the dispatch ghost above — same drill, same
    // `#dispatch=<id>` hash, no mission-graph gate.
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
                    dispatch_id: "peer-session-1",
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
      expect(window.location.hash).toBe("#dispatch=peer-session-1&dispatch.mission=peer-mission-with-session");
      expect(screen.queryByText(/needs a running daemon/i)).not.toBeInTheDocument();
    } finally {
      window.location.hash = "";
    }
  });

  it("(#1915) a row with genuinely nothing behind it, an untracked mission with no dispatch_id at all, stays non-interactive", async () => {
    // `kind: "mission", tracked: false`, no `dispatch_id`: a mission this
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

  it("a click that ends a text selection inside a run row does not navigate (select-not-click)", async () => {
    mockFetch();
    const meta = document.createElement("meta");
    meta.name = "darkmux-mode";
    meta.content = "live";
    document.head.appendChild(meta);
    try {
      renderBoard();
      await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
      const label = screen.getByText("m1");
      const row = label.closest(".labrunrow")!;
      const range = document.createRange();
      range.selectNodeContents(label);
      window.getSelection()!.removeAllRanges();
      window.getSelection()!.addRange(range);
      fireEvent.click(row);
      expect(window.location.hash).toBe("");
      window.getSelection()!.removeAllRanges();
      fireEvent.click(row);
      expect(window.location.hash).toBe("#mission=m1");
    } finally {
      meta.remove();
      window.getSelection()?.removeAllRanges();
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

  it("(#2065) a static build that ships a graphs file opens the mission's MAP lens, not the daemon-less notice", async () => {
    mockFetch();
    for (const [name, content] of [
      ["darkmux-flow-src", "./demo-flow.jsonl"],
      ["darkmux-graphs-src", "./demo-graphs.json"],
    ]) {
      const meta = document.createElement("meta");
      meta.name = name;
      meta.content = content;
      document.head.appendChild(meta);
    }
    try {
      renderBoard();
      await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
      fireEvent.click(screen.getByText("m1").closest(".labrunrow")!);
      expect(window.location.hash).toBe("#mission=m1");
      expect(screen.queryByText(/needs a running daemon/i)).not.toBeInTheDocument();
    } finally {
      document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((m) => m.remove());
      window.location.hash = "";
    }
  });

  it("the daemon-less notice also fires from a keyboard Enter activation", async () => {
    mockFetch();
    renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    fireEvent.keyDown(screen.getByText("m1").closest(".labrunrow")!, { key: "Enter" });
    expect(screen.getByText(/needs a running daemon/i)).toBeInTheDocument();
  });

  it("(#1900, dispatch_id wiring #1915) an untracked dispatch ghost row also opens #dispatch=<id> from a keyboard Enter activation, and never shows the mission-graph notice", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url === "/runs") {
          return Promise.resolve(
            new Response(
              JSON.stringify({
                runs: [{ id: "ghost2", kind: "dispatch", status: "abandoned", tracked: false, dispatch_id: "ghost2", updated_ts: 1 }],
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
      expect(window.location.hash).toBe("#dispatch=ghost2");
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
        if (url.startsWith("/lab/run/detail")) return Promise.resolve(new Response(JSON.stringify({ dir: "l1", reviews: [], scores: null }), { status: 200 }));
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

  // (#2860 review F2) A `lab=<dir>` deep link used to open `LabRunDetail`
  // directly, skipping the rule list rows follow. A lab run with a
  // representative session opens the shared session view from EVERY entry
  // point; only a run without one (a bench run) keeps its own record page.
  const CODING_RUN = { id: "coding-1", kind: "lab", status: "complete", tracked: true, dispatch_id: "sess-c1", updated_ts: 50 };
  const CODING_LAB_RUN = {
    dir: "coding-1", mtime_ms: 50, case_ids: [], bundles: 0, raw_flags: 0, deduped_flags: 0,
    confirmed: 0, needs_check: 0, archived: 0, degenerate: false, finished: false,
  };
  function mockLabBoard() {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url === "/runs") return Promise.resolve(new Response(JSON.stringify({ runs: [CODING_RUN], generated_at_ms: 1 }), { status: 200 }));
        if (url === "/lab/runs")
          return Promise.resolve(new Response(JSON.stringify({ configured: true, dir: "/lab", exists: true, runs: [CODING_LAB_RUN] }), { status: 200 }));
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
  }

  it("(#2860) a plain list row for a lab run with a session opens the shared session view, not the funnel page", async () => {
    mockLabBoard();
    renderBoard("lab");
    await waitFor(() => expect(screen.getByText("coding-1")).toBeInTheDocument());
    const before = history.length;
    fireEvent.click(screen.getByText("coding-1").closest(".labrunrow")!);
    expect(window.location.hash).toBe("#dispatch=sess-c1");
    // A real navigation, not a replace: Back returns to the lab list.
    // Opening the record page and redirecting from there also lands on the
    // session view, but REPLACES the list's history entry on the way.
    expect(history.length).toBe(before + 1);
    expect(screen.queryByText("‹ runs")).not.toBeInTheDocument();
  });

  it("(#2860) a lab= deep link to a run with a session redirects to the shared session view", async () => {
    mockLabBoard();
    renderBoard("lab", "coding-1");
    await waitFor(() => expect(window.location.hash).toBe("#dispatch=sess-c1"));
    expect(screen.queryByText("‹ runs")).not.toBeInTheDocument();
    // The funnel page never mounts on the way: it would start its own detail
    // fetch in the same commit as the redirect.
    const calls = (fetch as unknown as { mock: { calls: [string][] } }).mock.calls.map((c) => c[0]);
    expect(calls.some((u) => u.startsWith("/lab/run/detail"))).toBe(false);
  });

  it("a deep-link into kind=lab with a lab= param opens the lab-run detail pane directly, on first render", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url.startsWith("/lab/run/detail")) return Promise.resolve(new Response(JSON.stringify({ dir: "live/gate-1", reviews: [], scores: null }), { status: 200 }));
        if (url.startsWith("/lab/run/events")) return Promise.resolve(new Response(JSON.stringify({ lines: [], next_offset: 0, finished: false }), { status: 200 }));
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
    renderBoard("lab", "live/gate-1");
    await waitFor(() => expect(screen.getByText(/· live\/gate-1/)).toBeInTheDocument());
    // (#2860) The deep link now waits for `/runs` to apply the same routing
    // rule as a list row. `/runs` 404s here, so there is no row to route by
    // and the link falls back to the run's own record page, as before.
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

  // (4.0) Runs recorded before the lab dir moved sit in the old dir and the
  // list is empty until the operator moves them: the empty state names the
  // command instead of claiming there are no runs.
  it("a pending lab-dir move names the command in the empty state", async () => {
    mockFetch(
      true,
      true,
      { pending_move: { from: "/h/runs", to: "/h/lab", command: "mv /h/runs /h/lab" }, dir: "/h/lab", exists: false },
      NO_LAB_RUNS,
    );
    const { container } = renderBoard();
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    fireEvent.click(container.querySelector('[data-arg="lab"]')!);
    await waitFor(() => expect(screen.getByText(/mv \/h\/runs \/h\/lab/)).toBeInTheDocument());
    expect(screen.getByText(/still in \/h\/runs/i)).toBeInTheDocument();
    expect(screen.queryByText(/does not exist yet/i)).toBeNull();
    expect(screen.queryByText(/no lab runs found/i)).toBeNull();
  });

  // Keyboard-accessibility structure — the runs-lens `role="button"` chips
  // (RunsBar's kind filter) and the `.runmore` "show all"
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
    /** (#2921 follow-up) Replace the default `u1 -> MacBook-Pro` record. */
    flowToday?: unknown[];
    specs?: unknown;
    roster?: unknown[];
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
              JSON.stringify(opts.flowToday ?? [{ ts: `${today}T00:00:00Z`, machine_uid: "u1", machine_id: "MacBook-Pro" }, ...(opts.extraFlowToday ?? [])]),
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
        if (path === "/machine/specs" && opts.specs) return Promise.resolve(new Response(JSON.stringify(opts.specs), { status: 200 }));
        if (path === "/fleet/roster" && opts.roster) {
          return Promise.resolve(new Response(JSON.stringify({ machines: opts.roster, error: null }), { status: 200 }));
        }
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
  }

  // (#2921 follow-up) A pinned machine the window knows only by uid is named
  // the way its fleet card is: this daemon's specs name, else its roster id.
  const FAKE_UID = "00000000-0000-4000-8000-ABCDEF000001";
  const uidOnlyToday = () => [{ ts: `${todayUTC()}T00:00:00Z`, machine_uid: FAKE_UID }];
  it("(#2921) a uid-only pinned machine that is THIS daemon reads its specs name", async () => {
    mockPinnedFetch({ flowToday: uidOnlyToday(), specs: { machine_id: "scratch-box", machine_uid: FAKE_UID } });
    renderBoard("all", null, FAKE_UID);
    await waitFor(() => expect(screen.getByText(/Machine scratch-box/)).toBeInTheDocument());
  });
  it("(#2921) a uid-only pinned remote machine reads its roster id", async () => {
    mockPinnedFetch({ flowToday: uidOnlyToday(), roster: [{ id: "studio", address: "100.64.1.2:8765", added_unix_ms: 1, machine_uid: FAKE_UID }] });
    renderBoard("all", null, FAKE_UID);
    await waitFor(() => expect(screen.getByText(/Machine studio/)).toBeInTheDocument());
    expect(document.body.textContent).not.toMatch(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i);
  });

  // A pin shows the runs of ONE machine, decided by uid (`Run.machine_uid`):
  // a renamed or `.local` spelling neither hides a run nor splits the
  // machine, and two machines sharing a display name never share a pin.
  describe("by uid (machine identity)", () => {
    const at = (h: number) => `${todayUTC()}T0${h}:00:00Z`;
    const run = (id: string, machine: string, machine_uid?: string) => ({ id, kind: "dispatch", status: "complete", tracked: false, updated_ts: 100, machine, machine_uid });

    it("this machine's pin shows its runs filed under another spelling of its name, and its uid in the other case", async () => {
      mockPinnedFetch({
        flowToday: [{ ts: at(0), machine_uid: FLEET_UID.mbp, machine_id: "MacBook-Pro.local" }],
        specs: { machine_id: "MacBook-Pro", machine_uid: lower(FLEET_UID.mbp) },
        runs: [run("by-uid", "MacBook-Pro", lower(FLEET_UID.mbp)), run("by-name", "MacBook-Pro"), run("other", "studio", FLEET_UID.studio)],
      });
      renderBoard("all", null, "MacBook-Pro.local");
      await waitFor(() => expect(screen.getByText("by-uid")).toBeInTheDocument());
      expect(screen.getByText("by-name")).toBeInTheDocument();
      expect(screen.queryByText("other")).not.toBeInTheDocument();
    });

    it("a roster-only peer's pin shows its runs", async () => {
      mockPinnedFetch({
        flowToday: [{ ts: at(0), machine_uid: FLEET_UID.mbp, machine_id: "MacBook-Pro" }],
        roster: [{ id: "darkbook", address: "100.64.1.2:8765", added_unix_ms: 1, machine_uid: lower(FLEET_UID.darkbook) }],
        runs: [run("db-run", "Darkbook", FLEET_UID.darkbook), run("mbp-run", "MacBook-Pro", FLEET_UID.mbp)],
      });
      renderBoard("all", null, "darkbook");
      await waitFor(() => expect(screen.getByText("db-run")).toBeInTheDocument());
      expect(screen.queryByText("mbp-run")).not.toBeInTheDocument();
    });

    it("two machines with one display name each pin only their own runs", async () => {
      mockPinnedFetch({
        flowToday: [
          { ts: at(0), machine_uid: FLEET_UID.macA, machine_id: "Mac" },
          { ts: at(1), machine_uid: FLEET_UID.macB, machine_id: "Mac" },
        ],
        runs: [run("run-a", "Mac", FLEET_UID.macA), run("run-b", "Mac", FLEET_UID.macB)],
      });
      renderBoard("all", null, `Mac_${machineKeyHash(FLEET_UID.macB).slice(0, 6)}`);
      await waitFor(() => expect(screen.getByText("run-b")).toBeInTheDocument());
      expect(screen.queryByText("run-a")).not.toBeInTheDocument();
    });

    it("one machine under two spellings adds no machine column to the unpinned board", async () => {
      mockPinnedFetch({ runs: [run("x1", "MacBook-Pro", FLEET_UID.mbp), run("x2", "MacBook-Pro.local", lower(FLEET_UID.mbp))] });
      renderBoard("all", null, null);
      await waitFor(() => expect(screen.getByText("x1")).toBeInTheDocument());
      expect(document.body.textContent).not.toContain("MacBook-Pro.local");
    });

    it("two machines with one display name are told apart on the unpinned board", async () => {
      mockPinnedFetch({ runs: [{ ...run("y1", "Mac", FLEET_UID.macA), updated_ts: 9 }, { ...run("y2", "Mac", FLEET_UID.macB), updated_ts: 8 }] });
      renderBoard("all", null, null);
      await waitFor(() => expect(screen.getByText("y1")).toBeInTheDocument());
      const metas = [...document.querySelectorAll(".labrunmeta")].map((e) => e.textContent);
      expect(metas.sort()).toEqual(["Mac", "Mac 2"]);
    });
  });

  // (#2929) The pin rides in the address bar as a machine KEY, never the uid.
  const UUID_RE = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i;
  it("(#2929) an old link carrying the uid still pins the machine, and the hash is rewritten to its key without a history entry", async () => {
    mockPinnedFetch({ flowToday: uidOnlyToday(), specs: { machine_id: "scratch-box", machine_uid: FAKE_UID } });
    window.location.hash = `#lens=runs&machine=${FAKE_UID.toLowerCase()}`;
    const before = window.history.length;
    renderBoard("all", null, FAKE_UID.toLowerCase());
    await waitFor(() => expect(screen.getByText(/Machine scratch-box/)).toBeInTheDocument());
    await waitFor(() => expect(window.location.hash).toBe("#lens=runs&machine=scratch-box"));
    expect(UUID_RE.test(window.location.hash)).toBe(false);
    expect(window.history.length).toBe(before);
  });

  it("(#2929) an old uid link is not rewritten until the roster has landed: a roster name renumbers the unnamed", async () => {
    // The roster answers late. Before it lands this uid-only machine reads as
    // "unnamed machine" (key `unnamed-1`); after, it is "studio". Rewriting
    // on the early answer would leave a key that stops resolving.
    let releaseRoster: () => void = () => {};
    const rosterGate = new Promise<void>((r) => {
      releaseRoster = r;
    });
    mockPinnedFetch({ flowToday: uidOnlyToday() });
    const base = vi.mocked(fetch).getMockImplementation()!;
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) =>
        String(url) === "/fleet/roster"
          ? rosterGate.then(
              () =>
                new Response(JSON.stringify({ machines: [{ id: "studio", address: "a:1", added_unix_ms: 1, machine_uid: FAKE_UID }], error: null }), {
                  status: 200,
                }),
            )
          : base(url),
      ),
    );
    window.location.hash = `#lens=runs&machine=${FAKE_UID}`;
    renderBoard("all", null, FAKE_UID);
    await waitFor(() => expect(screen.getByText(/Machine unnamed machine/)).toBeInTheDocument());
    await new Promise((r) => setTimeout(r, 200));
    expect(window.location.hash, "held until the roster settles").toBe(`#lens=runs&machine=${FAKE_UID}`);
    releaseRoster();
    await waitFor(() => expect(window.location.hash).toBe("#lens=runs&machine=studio"));
    expect(screen.getByText(/Machine studio/)).toBeInTheDocument();
  });

  it("(#2929) a link shared with the earlier `~` separator opens its machine and is rewritten to the `_` form", async () => {
    const TWIN = "00000000-0000-4000-8000-ABCDEF000002";
    mockPinnedFetch({
      flowToday: [
        { ts: `${todayUTC()}T00:00:00Z`, machine_uid: FAKE_UID, machine_id: "MacBook-Pro" },
        { ts: `${todayUTC()}T01:00:00Z`, machine_uid: TWIN, machine_id: "MacBook-Pro" },
      ],
    });
    const hx = machineKeyHash(TWIN).slice(0, 6);
    window.location.hash = `#lens=runs&machine=MacBook-Pro%7E${hx}`;
    renderBoard("all", null, `MacBook-Pro~${hx}`);
    await waitFor(() => expect(window.location.hash).toBe(`#lens=runs&machine=MacBook-Pro_${hx}`));
    expect(screen.getByText(/Machine MacBook-Pro/)).toBeInTheDocument();
  });

  it("(#2929) a name key pins the machine it names", async () => {
    mockPinnedFetch();
    renderBoard("all", null, "MacBook-Pro");
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    expect(screen.getByText(/Machine MacBook-Pro/)).toBeInTheDocument();
    expect(screen.queryByText("m2")).not.toBeInTheDocument();
    // A key link is already canonical: nothing rewrites it.
    fireEvent.click(document.querySelector('[data-arg="mission"]')!);
    expect(window.location.hash).toBe("#lens=runs&kind=mission&machine=MacBook-Pro");
  });

  it("(#2929) two unnamed machines: the second's key pins the second one, not the first", async () => {
    const OTHER = "00000000-0000-4000-8000-ABCDEF000002";
    mockPinnedFetch({
      flowToday: [
        { ts: `${todayUTC()}T00:00:00Z`, machine_uid: FAKE_UID },
        { ts: `${todayUTC()}T01:00:00Z`, machine_uid: OTHER },
      ],
    });
    renderBoard("all", null, `unnamed-${machineKeyHash(OTHER).slice(0, 6)}`);
    await waitFor(() => expect(screen.getByText(/Machine unnamed machine 2/)).toBeInTheDocument());
    expect(window.location.hash).not.toMatch(UUID_RE);
  });

  it("(#2929) a key nothing resolves says 'machine not found' in the chip's slot, with no rows", async () => {
    mockPinnedFetch();
    const { container } = renderBoard("all", null, "no-such-machine");
    await waitFor(() => expect(screen.getByText(/no runs match these filters/)).toBeInTheDocument());
    const chip = container.querySelector('[data-act="clearmachine"]');
    expect(chip?.textContent).toBe("machine not found ✕");
    expect(container.textContent).not.toMatch(/unnamed machine/);
    expect(screen.queryByText("m1")).not.toBeInTheDocument();
    // A plain unknown key is left as typed: it identifies nothing.
    expect(window.location.hash).not.toContain("not-found");
  });

  it("(#2929 C4) an old uid link that resolves to nothing is rewritten to the not-found marker once settled", async () => {
    mockPinnedFetch();
    const gone = "00000000-0000-4000-8000-ABCDEF000007".toLowerCase();
    window.location.hash = `#lens=runs&machine=${gone}`;
    const { container } = renderBoard("all", null, gone);
    await waitFor(() => expect(window.location.hash).toBe("#lens=runs&machine=not-found"));
    await waitFor(() => expect(container.querySelector('[data-act="clearmachine"]')?.textContent).toBe("machine not found ✕"));
    expect(UUID_RE.test(window.location.hash)).toBe(false);
  });

  it("(#2929 C4) an old uid link to a roster-declared machine never seen opens its roster card by name", async () => {
    const DECLARED = "00000000-0000-4000-8000-ABCDEF000008";
    mockPinnedFetch({ roster: [{ id: "garage-mac", address: "a:1", added_unix_ms: 1, machine_uid: DECLARED }] });
    window.location.hash = `#lens=runs&machine=${DECLARED}`;
    const { container } = renderBoard("all", null, DECLARED);
    await waitFor(() => expect(window.location.hash).toBe("#lens=runs&machine=garage-mac"));
    // (C3) A roster-only card is labeled with its roster id.
    await waitFor(() => expect(container.querySelector('[data-act="clearmachine"]')?.textContent).toBe("Machine garage-mac ✕"));
  });

  it("(#2929 C5) while the inputs are still landing, an unresolved key shows the loading rows, not 'machine not found'", async () => {
    let releaseRoster: () => void = () => {};
    const rosterGate = new Promise<void>((r) => {
      releaseRoster = r;
    });
    mockPinnedFetch({ flowToday: uidOnlyToday() });
    const base = vi.mocked(fetch).getMockImplementation()!;
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) =>
        String(url) === "/fleet/roster"
          ? rosterGate.then(
              () =>
                new Response(JSON.stringify({ machines: [{ id: "studio", address: "a:1", added_unix_ms: 1, machine_uid: FAKE_UID }], error: null }), {
                  status: 200,
                }),
            )
          : base(url),
      ),
    );
    const { container } = renderBoard("all", null, "studio");
    await new Promise((r) => setTimeout(r, 300));
    expect(container.querySelector('[data-state="pending"]')).not.toBeNull();
    expect(container.textContent).not.toMatch(/not found/);
    releaseRoster();
    await waitFor(() => expect(container.querySelector('[data-act="clearmachine"]')?.textContent).toBe("Machine studio ✕"));
  });

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
    expect(screen.getByText(/Machine MacBook-Pro/)).toBeInTheDocument();
  });

  it("clicking the chip clears the pin — back to every machine, via a real hash write", async () => {
    mockPinnedFetch();
    const { container } = renderBoard("all", null, "u1");
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    expect(screen.queryByText("m2")).not.toBeInTheDocument();

    fireEvent.click(container.querySelector('[data-act="clearmachine"]')!);

    await waitFor(() => expect(screen.getByText("m2")).toBeInTheDocument());
    expect(screen.queryByText(/Machine MacBook-Pro/)).not.toBeInTheDocument();
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
    // (#2929) The old uid link was rewritten to the machine's key.
    expect(window.location.hash).toBe("#lens=runs&kind=dispatch&machine=MacBook-Pro");
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
});

/**
 * (#1920) A harness that mirrors `App.tsx`'s ACTUAL `RunsBoard` wiring —
 * `initialKind`/`initialRun`/`initialMachineKey` re-derived from
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
  return <RunsBoard initialKind={route.runsKind} initialLab={route.lab} initialMachineKey={route.machine} />;
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
  it("(#2065) on a static build that ships mission graphs, an unresolvable lab run still gets the daemon-less notice, not a 'removed or stale' claim", async () => {
    mockFetch(); // /lab/run/detail?dir=bad-dir 404s here exactly as a static host would
    window.location.hash = "#lens=runs&kind=lab&lab=bad-dir";
    for (const [name, content] of [
      ["darkmux-flow-src", "./demo-flow.jsonl"],
      ["darkmux-graphs-src", "./demo-graphs.json"],
    ]) {
      const meta = document.createElement("meta");
      meta.name = name;
      meta.content = content;
      document.head.appendChild(meta);
    }
    try {
      const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
      render(
        <QueryClientProvider client={queryClient}>
          <AppLikeRunsHarness />
        </QueryClientProvider>,
      );
      await waitFor(() => expect(screen.getByText(/needs a running daemon/)).toBeInTheDocument());
      expect(screen.queryByText(/couldn't open run/)).not.toBeInTheDocument();
    } finally {
      document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((m) => m.remove());
      window.location.hash = "";
    }
  });

  it("a deep link to an unresolvable lab run keeps its notice after the echoed re-render, not wiped back out", async () => {
    mockFetch(); // /runs, /lab/runs both ok; /lab/run/detail?dir=bad-dir falls through to this mock's 404 default
    window.location.hash = "#lens=runs&kind=lab&lab=bad-dir";

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
    // `hashSync.ts`'s own doc) already moved `location.href` to `lab=null`
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

/**
 * (#1966) The board fetched its run list ONCE on mount, so a run that was
 * `running` at load time rendered `running` forever and only a page reload
 * corrected it. The operator-visible tell was one row disagreeing with itself:
 * a frozen status badge beside a replay control derived from state that does
 * update.
 *
 * These assert against a CHANGED server response after the poll interval, not
 * against the first render. The pre-existing tests all pass with the bug
 * present because they only ever exercise the initial fetch — which is the one
 * state that was always correct.
 */
describe("RunsBoard — the run list keeps up with the server", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  function mockRuns(status: () => string) {
    return vi.fn(async (url: string) => {
      const u = String(url);
      const body = u.includes("lab")
        ? { runs: [] }
        : { runs: [{ id: "r-1", kind: "dispatch", status: status(), tracked: true, ts: todayUTC() }] };
      return { ok: true, status: 200, json: async () => body };
    });
  }

  it("re-renders a run as finished once the server says so, with no remount", async () => {
    let status = "running";
    vi.stubGlobal("fetch", mockRuns(() => status));

    renderBoard("dispatch");
    await waitFor(() => expect(screen.getByText(/running/i)).toBeTruthy());

    // The dispatch ends. Nothing about the page changes; only the server does.
    status = "complete";

    await waitFor(() => expect(screen.queryByText(/running/i)).toBeNull(), { timeout: 15_000 });
    expect(screen.getByText(/complete/i)).toBeTruthy();
  }, 20_000);
});

/**
 * (#2063) The machine pin on a DAEMON-LESS static build (darkmux.com/demo).
 * There is no `/flow/<date>` and no `/fleet/machines/live` to resolve a uid
 * through — `useFlowWindow`/`useLiveMachines` are gated off on purpose
 * (#1801) — so the pin's alias set has to come from the committed flow file
 * (`darkmux-flow-src`), the same records the playback lens already reads.
 * Before this test, the fleet card's drill-in landed on "no runs recorded
 * yet" for every machine on the demo while the unpinned board listed them.
 */
describe("RunsBoard — the machine pin on a static build (#2063)", () => {
  function injectMeta(name: string, content: string) {
    const el = document.createElement("meta");
    el.setAttribute("name", name);
    el.setAttribute("content", content);
    document.head.appendChild(el);
  }
  afterEach(() => {
    window.location.hash = "";
    document.querySelectorAll('meta[name^="darkmux-"]').forEach((m) => m.remove());
  });

  const STATIC_RUNS = [
    { id: "m1", kind: "mission", status: "complete", tracked: true, updated_ts: 300, machine: "m5-ultra-256gb" },
    { id: "d1", kind: "dispatch", status: "running", tracked: true, role: "coder", updated_ts: 200, machine: "m5-ultra-256gb" },
    // Another machine — must stay out of the u1 pin.
    { id: "m2", kind: "mission", status: "complete", tracked: true, updated_ts: 250, machine: "m1-max-32gb-studio" },
  ];
  // The committed flow file, as the demo's export writes it: uid + name on
  // every record, spread across DAYS (a static fixture's timestamps are
  // frozen at export time — a viewer-clock window must not be what resolves
  // the pin, or it decays empty as the deploy ages).
  const STATIC_FLOW = [
    { ts: "2026-08-26T01:00:00Z", machine_uid: "u1", machine_id: "m5-ultra-256gb" },
    { ts: "2026-08-27T01:00:00Z", machine_uid: "u2", machine_id: "m1-max-32gb-studio" },
  ]
    .map((r) => JSON.stringify(r))
    .join("\n");

  function mockStaticFetch() {
    const seen: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        const path = String(url);
        seen.push(path);
        if (path === "/runs") {
          return Promise.resolve(new Response(JSON.stringify({ runs: STATIC_RUNS, generated_at_ms: 1 }), { status: 200 }));
        }
        if (path === "/lab/runs") {
          return Promise.resolve(new Response(JSON.stringify({ configured: false, dir: null, exists: false, runs: [] }), { status: 200 }));
        }
        if (path === "./demo-flow.jsonl") {
          return Promise.resolve(new Response(STATIC_FLOW, { status: 200 }));
        }
        // A static host has NO daemon routes: `/flow/<date>`,
        // `/fleet/machines/live` and everything else 404, as on darkmux.com.
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
    return seen;
  }

  it("resolves the pin's alias set from the committed flow file, never from a daemon route", async () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    const seen = mockStaticFetch();
    renderBoard("all", null, "u1");
    await waitFor(() => expect(screen.getByText("m1")).toBeInTheDocument());
    expect(screen.getByText("d1")).toBeInTheDocument();
    expect(screen.queryByText("m2")).not.toBeInTheDocument();
    // The chip names the machine, not the raw uid.
    expect(screen.getAllByText(/m5-ultra-256gb/).length).toBeGreaterThan(0);
    expect(seen.filter((p) => p.startsWith("/flow/") || p === "/fleet/machines/live")).toEqual([]);
  });

  it("shows the loading state, not an empty pin, while the flow file is still downloading", async () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        const path = String(url);
        if (path === "/runs") {
          return Promise.resolve(new Response(JSON.stringify({ runs: STATIC_RUNS, generated_at_ms: 1 }), { status: 200 }));
        }
        if (path === "/lab/runs") {
          return Promise.resolve(new Response(JSON.stringify({ configured: false, dir: null, exists: false, runs: [] }), { status: 200 }));
        }
        // The multi-megabyte demo file, still in flight: never resolves.
        if (path === "./demo-flow.jsonl") return new Promise<Response>(() => {});
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
    renderBoard("all", null, "u1");
    // Give the runs + lab queries every chance to settle first.
    await waitFor(() => expect(vi.mocked(fetch).mock.calls.length).toBeGreaterThanOrEqual(3));
    await act(async () => {
      await new Promise((r) => setTimeout(r, 50));
    });
    expect(screen.getByRole("status", { name: "Loading runs" })).toBeInTheDocument();
    expect(screen.queryByText(/no runs recorded yet/)).not.toBeInTheDocument();
  });

  // (#2086) "never downloads the flow file for an unpinned board" retired:
  // the shell loads the day on every static route for the transport, and
  // this board reads that same cache slot, so the file is one download the
  // board neither triggers nor avoids.
});

// (#2862) The runs list drew a bare "loading…" line while `/runs` and
// `/lab/runs` were in flight — no header, no sense of what was coming. The
// fix draws the real header immediately and fills placeholder rows shaped
// like real ones (status badge, run id, meta line) rather than nothing.
describe("RunsBoard — the runs-list pending state draws the page, not a bare line (#2862)", () => {
  it("shows the real header and shape-of-real-rows placeholders while /runs and /lab/runs are in flight", async () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));

    renderBoard();

    const pending = await waitFor(() => {
      const el = screen.getByRole("status", { name: "Loading runs" });
      expect(el).toBeInTheDocument();
      return el;
    });

    // The header draws immediately — this is the whole point: the operator
    // sees the page, not a blank screen with one word on it.
    expect(pending.querySelector(".stagehdr")?.textContent).toMatch(/runs/);

    // No bare "loading…" text anywhere in the pending state.
    expect(pending.textContent).not.toMatch(/loading…/);

    // Placeholder rows shaped like `RunRow` — badge, kind, id, meta line —
    // each with a shimmer standing in for the not-yet-known value.
    //
    // `.labrunrow-ph`, deliberately NOT `.labrunrow`: several e2e specs
    // assert exact `.labrunrow` counts as their "real data has landed"
    // signal, so the skeleton must never carry that token (see
    // `.labrunrow-ph`'s own doc in styles.css). This is the mutation-tested
    // guard — see the self-QA section of the PR report.
    const rows = pending.querySelectorAll(".labrunrow-ph");
    expect(rows.length, "expected at least one placeholder row").toBeGreaterThan(0);
    expect(pending.querySelectorAll(".labrunrow").length, "a skeleton row must never carry the real .labrunrow token").toBe(0);
    for (const row of rows) {
      expect(row.querySelector(".ph-shimmer"), "each placeholder row needs a shimmered value").toBeTruthy();
      // Guard against a vacuous placeholder: no row may leak real text.
      expect((row.textContent ?? "").trim()).toBe("");
    }
  });
});
