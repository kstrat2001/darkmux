import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClientProvider, QueryClient } from "@tanstack/react-query";
import { FleetLens } from "./FleetLens";
import type { FlowRecord } from "../../types/handwritten";
import { todayUTC, prevDateUTC, FLOW_LIVE_TTL_MS } from "../../lib/flow";
import { closeOpenModal } from "../../lib/dialogManager";

// (#1913) Every fixture below anchors its records at "T10:00" of `today`
// (`todayUTC()`), and liveness (`flowLiveSessions`, `FLOW_LIVE_TTL_MS`) is
// judged against REAL wall-clock now. Left alone, that means the suite's
// pass/fail depended on what time of day it happened to run: before 10:00
// UTC the fixture sits in the future (trivially "fresh"), and after
// 10:05 UTC it's stale and every "N running" assertion goes red — a test
// that fails on a schedule, not intermittently. Freezing `Date` to a fixed
// instant makes the fixture-to-now distance an ASSERTED PARAMETER instead of
// something inherited from the clock. `toFake: ["Date"]` leaves
// setTimeout/setInterval alone, so `waitFor()`'s real-timer polling still
// works.
const FROZEN_NOW = "2026-06-15T10:02:00.000Z"; // 2 minutes after the T10:00 anchor, well inside FLOW_LIVE_TTL_MS

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["Date"] });
  vi.setSystemTime(new Date(FROZEN_NOW));
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  window.location.hash = "";
  // See EventLogColumn.test.tsx's own comment: dialogManager's open/close
  // state is a module-level singleton that outlives `render()`/unmount.
  closeOpenModal({ restore: false });
});

function renderFleetLens(
  props: Parameters<typeof FleetLens>[0] = {},
  /** Share one client across two renders when a test needs the SECOND
   * render to start from an already-settled query cache — see the replay
   * gate's test for why a fresh client makes that assertion vacuous. */
  queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } }),
) {
  return render(
    <QueryClientProvider client={queryClient}>
      <FleetLens {...props} />
    </QueryClientProvider>,
  );
}

/** Routes `fetchJson` calls the same way the real daemon's endpoint set
 * does, keyed on the URL. `flowToday`/`flowYesterday` default to an empty
 * window so a test only has to name the records it actually cares about. */
function mockFleetFetch(opts: {
  flowToday?: unknown[];
  flowYesterday?: unknown[];
  machines?: unknown[];
  /** (#1809) This daemon's OWN `/machine/specs` — the confirmed-local
   * signal `localMachineUid` resolves against. Omitted (the default) keeps
   * the pre-existing 404 (no daemon has confirmed ANY card as local), so
   * every pre-#1809 test in this file is unaffected by this field's
   * addition. Set it to make ONE uid resolve as local — see the two
   * locality-split tests below for why that distinction now matters. */
  specs?: unknown;
  /** (#1923) `GET /runs` rows — omitted (the default) keeps the pre-existing
   * 404 (every pre-#1923 test in this file is unaffected), same pattern as
   * `specs` above. */
  runs?: unknown[];
  /** (#1855) `GET /fleet/roster` entries — the operator's DECLARED
   * topology. Omitted defaults to an empty roster (`error: null`), so
   * every pre-#1855 test in this file is unaffected. */
  roster?: unknown[];
} = {}) {
  const today = todayUTC();
  const yesterday = prevDateUTC(today);
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      const path = String(url);
      if (path === `/flow/${today}`) return Promise.resolve(new Response(JSON.stringify(opts.flowToday ?? []), { status: 200 }));
      if (path === `/flow/${yesterday}`) return Promise.resolve(new Response(JSON.stringify(opts.flowYesterday ?? []), { status: 200 }));
      if (path === "/fleet/machines/live") {
        return Promise.resolve(
          new Response(
            JSON.stringify({
              machines: opts.machines ?? [],
              meta: { sources: { fleet: { state: "off" } }, complete: true },
            }),
            { status: 200 },
          ),
        );
      }
      if (path === "/fleet/sessions/live") {
        return Promise.resolve(
          new Response(JSON.stringify({ sessions: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }),
        );
      }
      if (path === "/machine/specs") {
        if (opts.specs === undefined) return Promise.resolve(new Response("{}", { status: 404 }));
        return Promise.resolve(new Response(JSON.stringify(opts.specs), { status: 200 }));
      }
      if (path === "/runs") {
        if (opts.runs === undefined) return Promise.resolve(new Response("not recorded\n", { status: 404 }));
        return Promise.resolve(new Response(JSON.stringify({ runs: opts.runs, generated_at_ms: 1 }), { status: 200 }));
      }
      if (path === "/fleet/roster") {
        return Promise.resolve(new Response(JSON.stringify({ machines: opts.roster ?? [], error: null }), { status: 200 }));
      }
      return Promise.resolve(new Response("not recorded\n", { status: 404 }));
    }),
  );
}

describe("FleetLens", () => {
  it("always renders the hero, even at zero — never hides it while there's no data yet", async () => {
    mockFleetFetch();
    renderFleetLens();
    await waitFor(() => expect(screen.getByText(/tokens · last/i)).toBeInTheDocument());
    // Two "0" values (local + cloud tokens) render rather than the card
    // disappearing — the "hides late, pops in" defect this port guards
    // against (see `SavingsHero`'s own doc).
    expect(screen.getByText("local tokens")).toBeInTheDocument();
    expect(screen.getByText("cloud tokens")).toBeInTheDocument();
  });

  it("sums a locally-run session's telemetry into local tokens, and renders its machine card", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        {
          ts: `${today}T10:00:05.000Z`,
          machine_uid: "u1",
          session_id: "s1",
          category: "telemetry",
          source: "tokens",
          payload: { turn_seq: 1, prompt_tokens: 500, completion_tokens: 100, total_tokens: 600 },
        },
        { ts: `${today}T10:01:00.000Z`, machine_uid: "u1", session_id: "s1", action: "dispatch.complete", payload: { total_tokens: 600 } },
      ],
    });
    renderFleetLens();
    await waitFor(() => expect(screen.getByText("600")).toBeInTheDocument()); // fmtN(600) = "600" local tokens
    // Renders twice: the machine card AND the activity-timeline lane label.
    expect(screen.getAllByText("MacBook-Pro").length).toBeGreaterThanOrEqual(2);
    expect(screen.getByText("idle")).toBeInTheDocument(); // no live session -> idle, not "dispatch in flight"
  });

  // (#2060) The rendered-DOM twin of `cards.test.ts`'s pure-function coverage
  // — a mission's own top-level session (`session_id === mission_id`) and
  // its one live seat dispatch (same `mission_id`, its own `session_id`)
  // must read as ONE running run on the actual card, not two.
  it("(#2060) a running mission with one live seat renders '1 running', not '2 running'", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "mission-1", mission_id: "mission-1", action: "dispatch.start" },
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "seat-1", mission_id: "mission-1", action: "dispatch.start" },
      ],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("1 running");
    expect(card.textContent).not.toContain("2 running");
  });

  // (#1923) A machine whose lab run is BETWEEN dispatches — the COW clone,
  // the baseline hash, the verify command, scoring — has no dispatch in
  // flight, so no contract-2 bookends and no presence key: flow sees nothing
  // and `machActive`/`sessionsOn` have nothing to read. The `/runs` lab row
  // (written at start, RAII-guarded) is the only source that stays "running"
  // across that whole span, and without it the card reads "idle" /
  // "0 running" while the run is very much live.
  it("(#1923) a running lab run with zero flow activity still renders 'dispatch in flight'", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.now() }],
      runs: [{ id: "lab-1", kind: "lab", status: "running", machine: "MacBook-Pro", tracked: true }],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("dispatch in flight");
    expect(card.textContent).toContain("1 running");
    expect(card.textContent).not.toContain("idle");
  });

  // (#1923 review) The rendered-DOM twin of `cards.test.ts`'s double-count
  // case. A lab run's DISPATCH phase DOES ride the flow stream — the
  // provider calls `darkmux_crew::dispatch::dispatch`, which emits the
  // contract-2 bookends and spawns the session-presence emitter — so both
  // sources see the same one run. The card must say "1 running".
  it("(#1923) a lab run whose dispatch is live reads '1 running', not '2 running'", async () => {
    const today = todayUTC();
    const labSession = "darkmux-coding-long-agentic-1756000000000";
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.now() }],
      flowToday: [
        {
          ts: `${today}T10:00:00.000Z`,
          machine_uid: "u1",
          machine_id: "MacBook-Pro",
          session_id: labSession,
          action: "dispatch.start",
          handle: "coder",
        },
      ],
      runs: [{ id: "long-agentic-balanced-1756000000-1", kind: "lab", status: "running", machine: "MacBook-Pro", tracked: true }],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    await waitFor(() => expect(card.textContent).toContain("running"));
    expect(card.textContent).toContain("1 running");
    expect(card.textContent).not.toContain("2 running");
  });

  // (#1923 review) A `/runs` that cannot be read yields the SAME empty list
  // a healthy idle machine does, so the cards silently return to the exact
  // "idle while a lab run is live" reading #1923 removed. The lens has to
  // name the failure rather than render the absence as data.
  it("(#1923) says so when the /runs read fails, instead of asserting 'no lab runs'", async () => {
    // `runs` omitted -> the mock's 404, which is what a daemon whose `/runs`
    // 500s or times out looks like to `fetchJson` (`ok: false`).
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.now() }],
    });
    const { container } = renderFleetLens();
    await waitFor(() => expect(container.querySelector('.fleetcov[data-state="runs-unreadable"]')).toBeTruthy());
    expect(screen.getByText(/Run records are unavailable/i)).toBeInTheDocument();
    // The count still renders (the `?? []` fallback keeps the lens alive) —
    // the notice is what stops it being read as a confident zero.
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(document.querySelector(".mach")!.textContent).toContain("0 running");
  });

  // The INVERTED case, and the one that proves the notice is conditional: a
  // `/runs` that answers cleanly must stay silent. Without it, a notice
  // hardwired on would pass the test above.
  it("(#1923) stays silent when /runs answers cleanly, even with no runs at all", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.now() }],
      runs: [],
    });
    const { container } = renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(container.querySelector('.fleetcov[data-state="runs-unreadable"]')).toBeNull();
  });

  // The second inverted case: a REPLAY never reads `machineRuns` at all
  // (`buildFleetCard` gates the lab count on `liveMode`), so a failed
  // `/runs` costs a replayed day nothing and warning about it would be the
  // bug — the same historical gate `FleetCoverageNotice` already carries.
  //
  // Both halves render against ONE `QueryClient` on purpose. A fresh client
  // makes the replay assertion vacuous: the absence is satisfied by the
  // `/runs` query simply not having settled yet, so the test passes with
  // the gate deleted. Rendering the LIVE lens first and waiting for its
  // notice proves the failed read is already in the cache; the replay
  // render then starts from that settled failure, and its silence is the
  // gate's doing rather than a race.
  it("(#1923) stays silent on a replay, where the lab count is never read", async () => {
    const today = todayUTC();
    // `runs` omitted -> the same 404 the live case above warns about.
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.now() }],
    });
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const live = renderFleetLens({}, qc);
    await waitFor(() => expect(live.container.querySelector('.fleetcov[data-state="runs-unreadable"]')).toBeTruthy());
    live.unmount();

    const records: FlowRecord[] = [
      { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      { ts: `${today}T10:01:00.000Z`, machine_uid: "u1", session_id: "s1", action: "dispatch.complete" },
    ];
    const { container } = renderFleetLens(
      {
        records,
        tMax: Date.parse(`${today}T10:01:00.000Z`),
        tMin: Date.parse(`${today}T10:00:00.000Z`),
        historical: true,
      },
      qc,
    );
    await waitFor(() => expect(container.querySelector(".mach")).not.toBeNull());
    expect(container.querySelector('.fleetcov[data-state="runs-unreadable"]')).toBeNull();
  });

  it("renders a real 'history →' link (#1640) when notes history exists, and it opens the notes dialog", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T09:00:00.000Z`, action: "note", source: "orchestrator", handle: "shipped the thing" },
        { ts: `${today}T10:00:00.000Z`, action: "note", source: "orchestrator", handle: "shipped another thing" },
      ],
    });
    const { container } = renderFleetLens();
    await waitFor(() => expect(screen.getByText(/shipped another thing/)).toBeInTheDocument());
    const link = container.querySelector('[data-act="notes"]');
    expect(link).toBeInTheDocument();
    expect(link!.textContent).toMatch(/history/i);

    expect(document.getElementById("nmodalbg")!.style.display).toBe("none");
    fireEvent.click(link!);
    expect(document.getElementById("nmodalbg")!.style.display).toBe("flex");
    // Both notes render, newest first (`openNotes()`'s `.reverse()`).
    const rows = document.querySelectorAll(".dialog__nrow");
    expect(rows.length).toBe(2);
    expect(rows[0].textContent).toContain("shipped another thing");
    expect(rows[1].textContent).toContain("shipped the thing");
  });

  it("no history link when there are no orchestrator notes", async () => {
    mockFleetFetch({});
    const { container } = renderFleetLens();
    await waitFor(() => expect(screen.getByText(/going hybrid takes nerve/i)).toBeInTheDocument());
    expect(container.querySelector('[data-act="notes"]')).not.toBeInTheDocument();
  });

  it("(#2068) the unattributed tile is ALWAYS in the hero, dimmed at zero, so a dispatch starting or finishing never re-flows the page", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        {
          ts: `${today}T10:00:05.000Z`,
          machine_uid: "u1",
          session_id: "s-local",
          action: "dispatch.start",
          category: "work",
          source: "crew",
          payload: { role: "coder", endpoint: "local" },
        },
      ],
    });
    renderFleetLens();
    await waitFor(() => expect(screen.getByText("local tokens")).toBeInTheDocument());
    const label = screen.getByText("unattributed");
    const tile = label.closest(".savlead")!;
    expect(tile.className).toMatch(/\bunknown\b/);
    expect(tile.className).toMatch(/\bzero\b/);
    expect(tile.querySelector(".savnum")!.textContent).toBe("0");
  });

  /** (U5-1) The gap the `historical` test below could not see: `App.tsx`
   * renders `<FleetLens />` with NO props, so `historical` sits at its
   * default `false` and the three live-only endpoints fired on the STATIC
   * demo — measured on the served build, `#lens=fleet` produced 404s for
   * `/fleet/machines/live`, `/fleet/sessions/live` and `/machine/specs`
   * plus their console errors. The prop describes the CALLER's intent (a
   * replay); only the BUILD can answer "is there a daemon at all" — the
   * #1801 rule `MachineLens`/`useFlowWindow`/`route.ts::isLiveRoute`
   * already follow. Rendered here exactly as `App.tsx` renders it: propless. */
  it("(U5-1) on a static build the propless FleetLens App renders never calls a live-only endpoint", async () => {
    const meta = document.createElement("meta");
    meta.name = "darkmux-flow-src";
    meta.content = "./demo-flow.jsonl";
    document.head.appendChild(meta);
    const seen: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        seen.push(String(url));
        return Promise.resolve(new Response("[]", { status: 200 }));
      }),
    );
    try {
      const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
      render(
        <QueryClientProvider client={queryClient}>
          <FleetLens />
        </QueryClientProvider>,
      );
      await waitFor(() => expect(document.querySelector(".savrow")).not.toBeNull());
      // Give every gated query a chance to fire before asserting none did
      // (a real timer — `vi.useFakeTimers` above fakes `Date` only).
      await new Promise((r) => setTimeout(r, 50));
      expect(seen.filter((p) => p === "/fleet/machines/live" || p === "/fleet/sessions/live" || p === "/machine/specs")).toEqual([]);
    } finally {
      document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((m) => m.remove());
    }
  });

  it("(#2067) on a static build the card's hardware line comes from the committed fleet snapshot, never from a daemon route", async () => {
    for (const [name, content] of [
      ["darkmux-flow-src", "./demo-flow.jsonl"],
      ["darkmux-fleet-src", "./demo-fleet.json"],
    ]) {
      const meta = document.createElement("meta");
      meta.name = name;
      meta.content = content;
      document.head.appendChild(meta);
    }
    const seen: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        const path = String(url);
        seen.push(path);
        if (path === "./demo-fleet.json") {
          return Promise.resolve(
            new Response(
              JSON.stringify({
                machines: [{ machine_uid: "u1", display_name: "m5-ultra-256gb", specs: "Apple M5 Ultra · 256 GB", beat_ts_ms: 1 }],
                meta: { sources: { fleet: { state: "ok" } }, complete: true },
              }),
              { status: 200 },
            ),
          );
        }
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
    try {
      const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
      const records = [
        { ts: "2026-08-26T10:00:00.000Z", machine_uid: "u1", machine_id: "m5-ultra-256gb", action: "machine.online", source: "presence-reconciler" },
      ] as unknown as FlowRecord[];
      render(
        <QueryClientProvider client={queryClient}>
          <FleetLens records={records} tMax={Date.parse("2026-08-26T10:00:00.000Z")} historical />
        </QueryClientProvider>,
      );
      await waitFor(() => expect(screen.getByText("Apple M5 Ultra · 256 GB")).toBeInTheDocument());
      expect(screen.queryByText("hardware not reported")).not.toBeInTheDocument();
      expect(seen.filter((p) => p === "/fleet/machines/live" || p === "/machine/specs")).toEqual([]);
    } finally {
      document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((m) => m.remove());
    }
  });

  it("honesty about incomplete data: a session with no dispatch bookend renders as 'unattributed', not silently local", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        {
          ts: `${today}T10:00:05.000Z`,
          machine_uid: "u1",
          session_id: "s-orphan",
          category: "telemetry",
          source: "tokens",
          payload: { turn_seq: 1, prompt_tokens: 900, completion_tokens: 100, total_tokens: 1000 },
        },
      ],
    });
    renderFleetLens();
    // (#2068) The label is always mounted now; the FIGURE is what proves the
    // tokens landed as unattributed rather than local.
    await waitFor(() => expect(screen.getByText("1,000")).toBeInTheDocument());
    expect(screen.getByText("1,000").parentElement!.className).toMatch(/\bunknown\b/);
    expect(screen.getByText("1,000").parentElement!.className).not.toMatch(/\bzero\b/);
    // Local tokens must NOT have absorbed it — it's excluded, not credited.
    const localBlock = screen.getByText("local tokens").previousSibling;
    expect(localBlock?.textContent).toBe("0");
  });

  // (drill-in packet, split by locality in #1809) The fleet-card click —
  // `data-act="machine" data-arg` in legacy (`ACTIONS.machine`,
  // `drillMachine(uid)`) — was previously a plain, non-interactive `<div>`,
  // and every card drilled to the SAME destination. #1809 splits that
  // destination by locality (see `FleetLens.tsx`'s own `machineDrillHash`
  // doc): the LOCAL card (confirmed against this daemon's OWN
  // `/machine/specs`) still reaches the residency room; anything this
  // daemon can't confirm as itself — including, but not limited to, a
  // genuinely remote machine — goes to the runs lens instead, pinned to
  // that machine. `MachineLens.test.tsx` covers what the residency-room
  // destination does with a REMOTE uid reached by a DIRECT deep-link
  // (still a real, supported route — see that file's own doc); these two
  // tests below are the LOCAL half. The REMOTE half — what a click on a
  // card this daemon can't confirm as itself actually does — is its own
  // test further down, the inverted case.
  it("clicking the LOCAL machine card navigates to the residency room via a real hash write", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      // This daemon's own /machine/specs identifies it AS the u1 machine —
      // the confirmed-local signal `localMachineUid` resolves against.
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    renderFleetLens();
    // "MacBook-Pro" renders TWICE — the machine card AND the activity-
    // timeline lane label (see the earlier test's own comment) — find the
    // CARD specifically via its ancestor class, not the singular query.
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("MacBook-Pro");
    expect(card).toHaveAttribute("role", "button");
    fireEvent.click(card);
    expect(window.location.hash).toBe("#lens=runs&machine=u1");
  });

  it("Enter/Space also activates the LOCAL fleet-card drill-in (keyboard parity with the click)", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    renderFleetLens();
    // "MacBook-Pro" renders TWICE — the machine card AND the activity-
    // timeline lane label (see the earlier test's own comment) — find the
    // CARD specifically via its ancestor class, not the singular query.
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("MacBook-Pro");
    fireEvent.keyDown(card, { key: "Enter" });
    expect(window.location.hash).toBe("#lens=runs&machine=u1");
  });

  // (#1809, merge-gate fix) The runs lens is for a machine POSITIVELY known
  // to be remote — nothing else. An earlier cut sent every unconfirmed card
  // there, which measured wrong on the first paint: `localUid` is null until
  // `/machine/specs` resolves, so the LOCAL card was clickable-and-wrong for
  // one frame (+0ms → runs, +100ms → machine). These two tests pin both
  // sides of the corrected rule, and the second is what stops the first from
  // being vacuous — without a confirmed-remote case, `machineDrillHash`
  // could always return the machine hash and every other test here would
  // still pass.
  it("an UNCONFIRMED card goes to the residency room — the destination that admits it is guessing", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      // No `specs` — locality unresolved, exactly the first-paint state.
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    fireEvent.click(document.querySelector(".mach")!);
    // The point of the single destination: an UNCONFIRMED card and a
    // confirmed one now agree, so there is no frame in which this card points
    // somewhere it will not point once `/machine/specs` resolves. That
    // flicker is what the old locality branch was working around.
    expect(window.location.hash).toBe("#lens=runs&machine=u1");
  });

  it("a CONFIRMED-REMOTE card goes to the runs lens, pinned to that machine", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        { ts: `${today}T10:00:01.000Z`, machine_uid: "u2", machine_id: "studio", session_id: "s2", action: "dispatch.start", handle: "coder" },
      ],
      // specs names THIS daemon as MacBook-Pro, so u2 is positively remote.
      specs: { machine_id: "MacBook-Pro" },
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelectorAll(".mach").length).toBe(2));
    const studio = [...document.querySelectorAll(".mach")].find((c) => c.textContent?.includes("studio"))!;
    fireEvent.click(studio);
    expect(window.location.hash).toBe("#lens=runs&machine=u2");
  });

  it("the savings hero renders tokens-only — no currency symbol or rate figure, even with non-zero savings (#803 regression coverage, restored post-#1806)", async () => {
    // Legacy's equivalent coverage
    // (`savings_hero_breakdown_is_classed_and_currency_free`, a source-text
    // scan of `viewer.html`'s `hybridNote`..`renderFleet` region) retired
    // with that file. This exercises the RENDERED hero instead — a fixture
    // with real, non-zero local tokens, so the assertion isn't vacuously
    // true against an empty "0" hero.
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        {
          ts: `${today}T10:00:05.000Z`,
          machine_uid: "u1",
          session_id: "s1",
          category: "telemetry",
          source: "tokens",
          payload: { turn_seq: 1, prompt_tokens: 500, completion_tokens: 100, total_tokens: 600 },
        },
        { ts: `${today}T10:01:00.000Z`, machine_uid: "u1", session_id: "s1", action: "dispatch.complete", payload: { total_tokens: 600 } },
      ],
    });
    const { container } = renderFleetLens();
    await waitFor(() => expect(screen.getByText("600")).toBeInTheDocument());
    const hero = container.querySelector(".savings");
    expect(hero).not.toBeNull();
    expect(hero!.textContent).not.toMatch(/[$€£]|USD|per million|\/M\b/);
  });

  // (#1869) The playback transport (`Scrubber`, rewind/play/speed/range)
  // only ever mounts inside `PlaybackLens`, which composes THIS component
  // rather than the other way around — `FleetLens` itself never renders it,
  // on any route. This is the live (default, no-hash) route's own coverage
  // of that: no `records`/`tMax`/`tMin`/`historical` props at all, the same
  // call every OTHER test in this file already makes.
  it("never renders a playback transport on the live route — the scrubber is PlaybackLens-only", async () => {
    mockFleetFetch();
    renderFleetLens();
    await waitFor(() => expect(screen.getByText(/tokens · last/i)).toBeInTheDocument());
    expect(document.querySelector(".scrub")).toBeNull();
    expect(screen.queryByRole("slider")).not.toBeInTheDocument();
  });

  // (#1869, QA gate — caught against a real daemon, not by any prior test)
  // `tMax` (the day's FIXED ceiling, used for the activity axis span) and
  // `playhead` (the scrub position) are two separate props now. A first
  // cut passed only one number for both, which was invisible in every unit
  // test above because none of them separate the two — this is the
  // integration-level regression test that does. Scrubbing all the way
  // back to `tMin` must NOT collapse the "ACTIVITY" axis header down to a
  // single repeated instant; the day's whole recorded span stays on
  // screen, with only the playhead marker (and the token hero, and the bar
  // classes) moving.
  // (#1903) The running COUNT is what the operator actually reads on a
  // fleet card ("N running"), and until now it carried no tap target of its
  // own — a click anywhere on the card, count included, fell through to
  // `machineDrillHash` and landed on the residency room. These three tests
  // pin the count's own destination, distinct from the card body's, without
  // touching `machineDrillHash` itself (covered above, unchanged).
  it("(#1903) tapping the running count with 2 live sessions navigates to the runs lens pinned to the machine; the card body still goes to the residency room", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.start", handle: "coder" },
      ],
      machines: [
        { uid: "u1", name: "MacBook-Pro", last_seen_ms: Date.now() },
      ],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("2 running");

    const countEl = card.querySelector(".runs--live")!;
    expect(countEl).not.toBeNull();
    fireEvent.click(countEl);
    expect(window.location.hash).toBe("#lens=runs&machine=u1");

    // The card BODY now reaches the SAME destination as the count. Before
    // 2026-08-23 it went to the residency room on a local card; the operator
    // asked for one destination ("clicking a machine ... should go to the
    // runs tab with a filter by machine"), which also removed the first-paint
    // flicker the locality branch existed to make harmless.
    window.location.hash = "";
    fireEvent.click(card.querySelector(".name")!);
    expect(window.location.hash).toBe("#lens=runs&machine=u1");
  });

  it("(#1903) tapping the running count with exactly 1 live session opens that run's session drill directly", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      machines: [{ uid: "u1", name: "MacBook-Pro", last_seen_ms: Date.now() }],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("1 running");
    fireEvent.click(card.querySelector(".runs--live")!);
    expect(window.location.hash).toBe("#dispatch=s1");
  });

  it("(#1903) the running count is keyboard operable and carries an accessible name", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      machines: [{ uid: "u1", name: "MacBook-Pro", last_seen_ms: Date.now() }],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const countEl = document.querySelector(".runs--live")!;
    expect(countEl).toHaveAttribute("role", "button");
    expect(countEl).toHaveAttribute("tabIndex", "0");
    expect(countEl.getAttribute("aria-label")).toBeTruthy();
    fireEvent.keyDown(countEl, { key: "Enter" });
    expect(window.location.hash).toBe("#dispatch=s1");
  });

  // (#1913) The two tests below pin BOTH sides of the `FLOW_LIVE_TTL_MS`
  // boundary explicitly, rather than relying on the other tests in this
  // file happening to sit comfortably inside it. Before this fix neither
  // direction was asserted: a session's liveness was implicitly "whatever
  // real wall-clock now happened to be" relative to a `T10:00` fixture.
  it("(#1913) a session 1s under the FLOW_LIVE_TTL_MS boundary still reads 1 running", async () => {
    const today = todayUTC();
    const lastRecordMs = Date.parse(`${today}T10:00:00.000Z`);
    mockFleetFetch({
      flowToday: [
        { ts: new Date(lastRecordMs).toISOString(), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      machines: [{ uid: "u1", name: "MacBook-Pro", last_seen_ms: lastRecordMs }],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    vi.setSystemTime(new Date(lastRecordMs + FLOW_LIVE_TTL_MS - 1000));
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("1 running");
    expect(card.querySelector(".runs--live")).not.toBeNull();
  });

  it("(#1913) a session 1s past the FLOW_LIVE_TTL_MS boundary reads 0 running, not stuck live", async () => {
    const today = todayUTC();
    const lastRecordMs = Date.parse(`${today}T10:00:00.000Z`);
    mockFleetFetch({
      flowToday: [
        { ts: new Date(lastRecordMs).toISOString(), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      machines: [{ uid: "u1", name: "MacBook-Pro", last_seen_ms: lastRecordMs }],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    vi.setSystemTime(new Date(lastRecordMs + FLOW_LIVE_TTL_MS + 1000));
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("0 running");
    // No live tap target once nothing is running (`machineRunsHash` returns
    // null), and the card body itself reads idle, not "dispatch in flight".
    expect(card.querySelector(".runs--live")).toBeNull();
    expect(card.textContent).toContain("idle");
  });

  // (#1903 QA fix) Was: the card body (`role="button"`) had no explicit
  // `aria-label`, so its computed accessible name absorbed ALL descendant
  // text — including the nested running-count button's own `aria-label`
  // ("open the 2 running dispatches on MacBook-Pro"), per ARIA's
  // presentational-children rule for a `button` descendant. The card
  // announced as "MacBook-Pro Apple M5 Max dispatch in flight open the 2
  // running dispatches on MacBook-Pro" instead of just its own name.
  // Nesting one interactive control inside another is an accepted,
  // documented exception here (the count needed its own tap target without
  // moving the card body's destination) — this pins that the OUTER card's
  // name stays deterministic despite the nesting.
  it("(#1903 QA fix) the card's own accessible name stays just its machine name, not polluted by the nested running-count button's aria-label", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.start", handle: "coder" },
      ],
      machines: [{ uid: "u1", name: "MacBook-Pro", last_seen_ms: Date.now() }],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card).toHaveAccessibleName("MacBook-Pro");
  });

  it("a scrubbed playhead moves the hero and the bars, but the activity axis stays the day's whole fixed span", async () => {
    const today = todayUTC();
    const dayTMin = Date.parse(`${today}T10:00:00.000Z`);
    const dayTMax = Date.parse(`${today}T12:00:00.000Z`);
    const records = [
      { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      { ts: `${today}T12:00:00.000Z`, machine_uid: "u1", session_id: "s1", action: "dispatch.complete", payload: { total_tokens: 600 } },
    ];

    const { rerender } = render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={records} tMax={dayTMax} tMin={dayTMin} playhead={dayTMax} historical />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".fleettl")).not.toBeNull());
    // Read the un-scrubbed axis text off the DOM (rather than asserting a
    // literal) — `clkhm`/`clkrange` render in the runner's local timezone,
    // so the only portable assertion is "unchanged after scrubbing", below.
    const axisBefore = document.querySelector(".tlhdr span")!.textContent;
    expect(axisBefore).toBeTruthy();

    rerender(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={records} tMax={dayTMax} tMin={dayTMin} playhead={dayTMin} historical />
      </QueryClientProvider>,
    );

    // The axis header is BYTE-IDENTICAL before and after scrubbing to tMin —
    // the bug this test guards collapsed it to a single repeated instant.
    expect(document.querySelector(".tlhdr span")!.textContent).toBe(axisBefore);
    // The playhead marker DID move, to the left edge of that fixed axis.
    expect((document.querySelector(".ph") as HTMLElement).style.left).toBe("0%");
    // The hero moved too — the completion is no longer visible at tMin.
    expect(screen.getByText("local tokens").previousSibling?.textContent).toBe("0");
  });
});

// ── (#1855) a rostered-but-silent machine must still render a card ──
//
// Before this fix, the fleet card list was `machineUids(flowData,
// liveMachines)` — a union of flow-derived uids and CURRENTLY-beating
// presence keys, with no read of the operator's declared roster at all. A
// machine added via `darkmux machine add` and never yet started (or down
// right now, with zero flow history under its name) produced no uid for
// either half of that union to find, so it vanished from the dashboard
// entirely — indistinguishable from never having been added.
describe("FleetLens — rostered-but-silent machine (#1855)", () => {
  it("a machine on the roster with zero flow history and no live beat renders an offline card, not nothing", async () => {
    mockFleetFetch({ roster: [{ id: "studio", address: "100.64.1.2:8765", added_unix_ms: 1000 }] });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(screen.getByText("studio")).toBeInTheDocument();
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("offline");
    // Reuses the SAME "offline"/`.absent` indicator a machine that WAS seen
    // and has since gone quiet already renders with — no parallel "silent"
    // vocabulary invented for this case (the project's "no snowflakes,
    // shared indicators" rule).
    expect(card.className).toContain("absent");
  });

  // The INVERTED case: a roster entry naming a machine that IS actually
  // live must not draw a SECOND, duplicate "offline" card for the same
  // machine beside its real one.
  it("a roster entry matching a currently-live machine does not duplicate its card", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "studio", schema_version: "1.20.0", beat_ts_ms: 1 }],
      roster: [{ id: "studio", address: "100.64.1.2:8765", added_unix_ms: 1000 }],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(document.querySelectorAll(".mach").length).toBe(1);
    expect(document.querySelector(".mach")!.className).not.toContain("absent");
  });

  // A genuinely gone machine (never rostered, never beating, never in flow
  // history) must not linger as if present — the roster fix must not make
  // every machine render forever regardless of evidence.
  it("a machine that is genuinely gone (not rostered, not beating, no flow history) renders no card at all", async () => {
    mockFleetFetch();
    renderFleetLens();
    await waitFor(() => expect(screen.getByText(/tokens · last/i)).toBeInTheDocument());
    expect(document.querySelector(".mach")).toBeNull();
  });
});

// ── (#2108, operator finding — "hero local/cloud figure collision") ──
//
// `.eventlog` is a fixed 380px side panel shown whenever the viewport is
// above the 768px mobile breakpoint (`App.tsx`'s `isMobile`), and
// `.app-shell__stage` carries 32px of its own horizontal padding — so in
// the viewport band (768px, 1180px], the STAGE's actual rendered width is
// BELOW 768px even though the raw viewport reads "desktop". `.savrow`'s
// OLD `max-width:768px` query keyed off the viewport, so in that band it
// stayed in flex-row mode inside a container narrower than it assumed,
// and the local/cloud/unattributed tiles collided against the chips
// rather than cleanly wrapping. The stylesheet, not a jsdom-computed
// style — jsdom performs no real layout, so this is the actual verifiable
// claim: the `.savrow` grid-switch rule's OWN threshold is the computed
// number (768 + 380 + 32 = 1180), not the un-widened 768 that produced
// the collision.
describe("FleetLens — hero grid-switch breakpoint accounts for the eventlog panel's width (#2108)", () => {
  it("the .savrow stylesheet rule switches to the 2-column grid at 1180px, not 768px", () => {
    const cssPath = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../styles.css");
    const css = readFileSync(cssPath, "utf-8");
    const match = css.match(/@media \(max-width: (\d+)px\) \{\s*\.savrow \{\s*display: grid;/);
    expect(match, "the .savrow grid-switch media query must exist").not.toBeNull();
    expect(Number(match![1])).toBe(1180);
  });
});
