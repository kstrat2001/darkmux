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
import { queryKeys } from "../../lib/queryKeys";

// (#2886 pass 5, MUST — fresh-reviewer finding F5) Several fixes in this
// file stayed green while broken in the actual render path: the DOM-text
// assertions elsewhere in this file (`.mach-scope__rate`'s textContent) all
// read from the SAME `selectedExec` the tube reads from, so a bug that hit
// ONLY the tube's own props (not the neighboring text) had nothing here to
// catch it. Mocking `TokenScope` and recording every prop it's called with
// lets a test assert on what the operator's SCREEN actually receives —
// `tokensPerSec`/`tone`/`stalled`/`resting` — not merely on `FleetCard`
// fields that happen to agree with it today. `data-props` carries the
// latest call's props as JSON; `latestTokenScopeProps()` below reads it
// back typed.
vi.mock("../../components/TokenScope", () => ({
  TokenScope: (props: Record<string, unknown>) => <div data-testid="token-scope-probe" data-props={JSON.stringify(props)} />,
}));

function latestTokenScopeProps(): Record<string, unknown> {
  const nodes = document.querySelectorAll('[data-testid="token-scope-probe"]');
  const last = nodes[nodes.length - 1];
  if (!last) throw new Error("no TokenScope probe rendered");
  return JSON.parse(last.getAttribute("data-props")!);
}

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

/** Waits until `/fleet/machines/live`, `/fleet/roster`, and `/machine/specs`
 * have all settled (success or error) in `queryClient`'s cache, rather than
 * inferring settlement from an incidental DOM condition.
 *
 * `rosterOnly` (#1855 follow-up) depends on all three of those queries, and
 * they resolve on THEIR OWN independent schedules — nothing forces them to
 * settle before an unrelated `waitFor` (e.g. one that only watches the
 * hero's text, which depends on the flow queries, not these) resolves.
 * Asserting an ABSENCE (e.g. "no phantom card renders") right after such an
 * unrelated wait is a race: the assertion can pass merely because the
 * roster/specs queries haven't resolved YET, which looks identical to the
 * fix genuinely excluding the entry — exactly the "a probe that passes
 * without executing is worse than no probe" trap. Waiting on the actual
 * query states removes the race instead of hoping the timing works out. */
async function waitForFleetQueriesSettled(queryClient: QueryClient) {
  await waitFor(() => {
    for (const key of [queryKeys.fleetMachinesLive(), queryKeys.fleetRoster(), queryKeys.machineSpecs()]) {
      const state = queryClient.getQueryState(key);
      expect(state?.status, `query ${JSON.stringify(key)} settled`).not.toBe("pending");
    }
  });
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
  /** (#1855 follow-up, CONSIDER 4) `GET /fleet/roster`'s `error` field — a
   * present-but-corrupt roster file. Omitted (the default) keeps `null`,
   * so every pre-CONSIDER-4 test in this file is unaffected. */
  rosterError?: string;
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
        return Promise.resolve(
          new Response(JSON.stringify({ machines: opts.roster ?? [], error: opts.rosterError ?? null }), { status: 200 }),
        );
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
    expect(screen.getByText("darkmux tokens")).toBeInTheDocument(); // the section eyebrow
    // (#2834) The cloud tile is withdrawn; one figure is the hero now.
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
        // (#2886 pass 5) `source: "mission"` on the mission's own top-level
        // session — a real mission bookend always carries it
        // (`mission_launch.rs::mission_bookend_record`) and it's what
        // `liveExecutions` reads to exclude this session from the pager's
        // `card.executions` (see that function's own doc). Without it here,
        // this fixture read as TWO genuine executions sharing one collapsed
        // run — a real mismatch, correctly triggering #2881's "N run(s) · M
        // executions" wording, not the bug this older test predates and was
        // never about.
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "mission-1", mission_id: "mission-1", action: "dispatch.start", source: "mission" },
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
    // (#2830) Wait for SETTLEMENT, not for a label. `"darkmux tokens"` renders
    // in both states, so the original anchor let the figure assertion below
    // run against an unsettled hero. That was invisible while `.savnum`
    // rendered "0" in both states; now that the figure is withheld until
    // settled, the incidental anchor reads "" instead. Anchoring on
    // `data-settled="true"` asserts the state this test always meant --
    // `settled && 0` is a literal "0" (see `SavingsHero`'s own doc).
    await waitFor(() =>
      expect(document.querySelector('.savings[data-settled="true"]')).toBeTruthy(),
    );
    // (#2834) There is one tile now, so #2068's concern is satisfied by
    // construction rather than by remembering to render a zero: with nothing
    // to appear or vanish, an in-flight dispatch cannot change the hero's
    // geometry. The property is still asserted — exactly one lead tile,
    // present and settled — because "one tile" is the thing that makes the
    // reflow impossible, and a future split would silently reintroduce it.
    const tiles = document.querySelectorAll(".savings .savlead");
    expect(tiles).toHaveLength(1);
    expect(screen.getByText("darkmux tokens")).toBeInTheDocument(); // the section eyebrow
    expect(tiles[0].querySelector(".savnum")!.textContent).toBe("0");
  });

  /** (U5-1) The gap the `historical` test below could not see: `App.tsx`
   * renders `<FleetLens />` with NO props, so `historical` sits at its
   * default `false` and the live-only endpoints fired on the STATIC
   * demo — measured on the served build, `#lens=fleet` produced 404s for
   * `/fleet/machines/live`, `/fleet/sessions/live` and `/machine/specs`
   * plus their console errors. The prop describes the CALLER's intent (a
   * replay); only the BUILD can answer "is there a daemon at all" — the
   * #1801 rule `MachineLens`/`useFlowWindow`/`route.ts::isLiveRoute`
   * already follow. Rendered here exactly as `App.tsx` renders it: propless.
   *
   * (#1855 follow-up, CONSIDER 6) `/fleet/roster` joined this list without
   * a DEDICATED gate test of its own — `useFleetRoster(livePolling)` was
   * asserted three times in a row by comment
   * (`FleetLens.tsx`'s own doc on `roster`, `rosterOnly`, and the `cards`
   * useMemo) but never once by a test that would actually catch the gate
   * being dropped. Mutating `useFleetRoster(livePolling)` to
   * `useFleetRoster(true)` left the WHOLE suite green before this line was
   * added — this endpoint is folded into the same list this test already
   * polices, closing that gap the same way the presence endpoints are
   * already covered rather than inventing a second test for one more URL. */
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
      expect(
        seen.filter(
          (p) => p === "/fleet/machines/live" || p === "/fleet/sessions/live" || p === "/machine/specs" || p === "/fleet/roster",
        ),
      ).toEqual([]);
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

  it("(#2890) a replay's activity window defaults to \"all\", the recording's own span", async () => {
    // A 34-minute recording under the 24h default was a sliver at the right
    // edge of the timeline (operator, 2026-09-25: "I'm spending most of my
    // time watching nothing happen").
    const mk = (ts: string, action: string) => ({ ts, machine_uid: "u1", machine_id: "m5", session_id: "s1", action }) as unknown as FlowRecord;
    const records = [mk("2026-08-26T10:00:00.000Z", "dispatch.start"), mk("2026-08-26T10:34:00.000Z", "dispatch.complete")];
    renderFleetLens({ records, tMin: Date.parse("2026-08-26T10:00:00.000Z"), tMax: Date.parse("2026-08-26T10:34:00.000Z"), historical: true });
    await waitFor(() => expect(document.querySelector(".twinb.on")?.textContent).toBe("all"));
  });

  it("(#2890) a picked preset replaces \"all\"; live has no \"all\" and keeps 24h", async () => {
    const mk = (ts: string, action: string) => ({ ts, machine_uid: "u1", machine_id: "m5", session_id: "s1", action }) as unknown as FlowRecord;
    const records = [mk("2026-08-26T01:00:00.000Z", "dispatch.start"), mk("2026-08-26T09:00:00.000Z", "dispatch.complete")];
    const r = renderFleetLens({ records, tMin: Date.parse("2026-08-26T01:00:00.000Z"), tMax: Date.parse("2026-08-26T09:00:00.000Z"), historical: true });
    await waitFor(() => expect(document.querySelector(".twinb.on")?.textContent).toBe("all"));
    fireEvent.click(screen.getByRole("button", { name: "1h" }));
    expect(document.querySelector(".twinb.on")?.textContent).toBe("1h");
    r.unmount();
    // Same records, not a replay: no "all", and the live 24h default.
    renderFleetLens({ records, tMin: Date.parse("2026-08-26T01:00:00.000Z"), tMax: Date.parse("2026-08-26T09:00:00.000Z") });
    await waitFor(() => expect(document.querySelector(".twinb.on")?.textContent).toBe("24h"));
    expect([...document.querySelectorAll(".twinb")].map((b) => b.textContent)).not.toContain("all");
  });

  it("(#2834) a session whose endpoint is unknown still counts: darkmux dispatched those tokens either way", async () => {
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
    // (#2834) This session has telemetry but no dispatch bookend, so nothing
    // names its endpoint. It used to be quarantined in an "unattributed"
    // tile and EXCLUDED from the figure, so the hero would not imply the
    // tokens had run free.
    //
    // That quarantine only made sense while the figure claimed locality. It
    // no longer does: "darkmux tokens" is true of these tokens whether the
    // model ran on this machine, a fleet peer, or a hosted endpoint — which
    // is precisely why the claim was narrowed to one darkmux can actually
    // make. So they are counted, and nothing about where they ran is
    // asserted anywhere in the hero.
    await waitFor(() => expect(screen.getByText("1,000")).toBeInTheDocument());
    expect(document.querySelector(".savings .savnum")?.textContent).toBe("1,000");
    expect(document.querySelectorAll(".savings .savlead")).toHaveLength(1);
    // The withdrawn vocabulary must not survive anywhere in the hero.
    const hero = document.querySelector(".savings")!.textContent ?? "";
    for (const claim of ["local tokens", "cloud", "unattributed"]) {
      expect(hero.toLowerCase()).not.toContain(claim);
    }
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

  // (Playback parity, Change A, finding #8 — 2026-09-24, operator decision)
  // This used to assert the OLD "replay draws the day's own fixed span"
  // behavior directly: scrubbing back moved the hero/bars but the
  // TIMELINE'S AXIS stayed pinned to the day's whole recorded range. That
  // premise is retired by design now — a replay draws the SAME rolling
  // window as live, anchored at the playhead, so scrubbing back MOVES the
  // whole window with it, exactly like a live viewer watching a session
  // recede out of a rolling window as time passes. The header text itself
  // (always "recent activity" now) is unaffected either way — this test is
  // rewritten to check the axis TIMES and the playhead marker position,
  // which are what actually move.
  it("a scrubbed playhead MOVES the rolling activity window with it (parity — not the old fixed day-span)", async () => {
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
    // The header LABEL never changes (always "recent activity" now — see
    // this module's own doc); read the AXIS TIMES off the DOM instead
    // (rather than asserting a literal — `clkhm` renders in the runner's
    // local timezone), which is what actually moves with the playhead.
    expect(document.querySelector(".tlhdr span")!.textContent).toBe("recent activity");
    // (#2890) A replay now opens on "all" (the recording's fixed span); the
    // rolling window this test pins is what any picked preset does.
    fireEvent.click(screen.getByRole("button", { name: "24h" }));
    const axisBefore = [...document.querySelectorAll(".tlaxis span")].map((e) => e.textContent);
    expect(axisBefore.every(Boolean)).toBe(true);
    // The playhead marker sits at the window's own right edge at rest.
    expect((document.querySelector(".ph") as HTMLElement).style.left).toBe("100%");

    rerender(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={records} tMax={dayTMax} tMin={dayTMin} playhead={dayTMin} historical />
      </QueryClientProvider>,
    );

    // The rolling window MOVED with the scrub — the axis times are now
    // different (centered on `dayTMin`, not `dayTMax`), not byte-identical
    // to before. This is the parity behavior: the same thing scrubbing
    // live would do to its own rolling window.
    const axisAfter = [...document.querySelectorAll(".tlaxis span")].map((e) => e.textContent);
    expect(axisAfter).not.toEqual(axisBefore);
    // The playhead marker still sits at the window's own right edge —
    // scrubbing moves the WINDOW, not the marker's position within it.
    expect((document.querySelector(".ph") as HTMLElement).style.left).toBe("100%");
    // The hero moved too — the completion is no longer visible at tMin.
    expect(document.querySelector(".savings .savnum")?.textContent).toBe("0");
  });
});

// (#2881) The fleet card pager. Uses the `records`/`historical` render path
// directly (same as the scrubbed-playhead test above), not `mockFleetFetch`
// — the pager reads only `buildFleetCard`'s output, which this path drives
// with no separate live/playback branch to mock around.
describe("FleetLens pager (#2881)", () => {
  const D0 = Date.parse("2026-08-26T10:00:00.000Z");
  const at = (sec: number) => new Date(D0 + sec * 1000).toISOString();

  // s1: CODER, generating (~100 tok/s, the two heartbeats 2s apart).
  // s2: REVIEWER, resting (a dispatch.rest 15s window opened at 3s).
  // s3: FETCH-RENDER, prompt (a bare dispatch.start, no heartbeat yet).
  const threeExecutionRecords: FlowRecord[] = [
    { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
    { ts: at(0), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 0 } },
    { ts: at(2), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 2000, generated_chars: 800 } },
    { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.start", handle: "reviewer" },
    { ts: at(3), machine_uid: "u1", session_id: "s2", action: "dispatch.rest", payload: { ms: 15_000 } },
    { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s3", action: "dispatch.start", handle: "fetch-render" },
  ] as FlowRecord[];

  function renderThree(playheadSec: number) {
    return render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={threeExecutionRecords} tMax={D0 + playheadSec * 1000} tMin={D0} playhead={D0 + playheadSec * 1000} historical />
      </QueryClientProvider>,
    );
  }

  it("defaults to the busiest (generating) execution's own tube, role and rate — not the machine aggregate", async () => {
    renderThree(5);
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    expect(document.querySelector(".mach-scope__pager-n")!.textContent).toBe("1/3");
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("coder");
    // The rate line shows s1's OWN reading (100 tok/s), not the machine's
    // summed total (also 100 here, since only s1 is generating — see the
    // next assertion for where the total actually shows up).
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("100 tok/s");
    // (#2886 pass 5, MUST — fresh-reviewer finding F5) Pin what the TUBE
    // component itself receives, not just the neighboring text — a bug that
    // hits only the tube's own props (e.g. still reading the machine
    // aggregate) would leave every text assertion in this file green.
    expect(latestTokenScopeProps()).toMatchObject({ tokensPerSec: 100, state: "generating" });
    // (#2881) "the machine total moves to the count line" — no separate
    // "all" page.
    expect(document.querySelector(".runs--live")!.textContent).toBe("3 running · 100 tok/s");
  });

  // (#2886 pass 5, MUST — fresh-reviewer finding F2) A mission's seats all
  // collapse to ONE top-level run (`topLevelRunSessionIds`), but the pager
  // shows one page per seat — so `runsCount` (1) and `card.executions.length`
  // (9) genuinely disagree here, unlike the plain-dispatches case above
  // where they agree by construction.
  it("names both counts when runs and executions disagree (a mission's seats collapse to one run)", async () => {
    const missionId = "mission-1";
    const seatRecords: FlowRecord[] = Array.from({ length: 8 }, (_, i) => ({
      ts: at(0),
      machine_uid: "u1",
      machine_id: "MacBook-Pro",
      session_id: `seat-${i + 2}`,
      action: "dispatch.start",
      mission_id: missionId,
      handle: "darkmux/crawler",
    })) as FlowRecord[];
    const missionRecords: FlowRecord[] = [
      { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: missionId, action: "dispatch.start", source: "mission", mission_id: missionId },
      { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "seat-1", action: "dispatch.start", mission_id: missionId, handle: "darkmux/crawler" },
      { ts: at(0), machine_uid: "u1", session_id: "seat-1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 0 } },
      { ts: at(2), machine_uid: "u1", session_id: "seat-1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 2000, generated_chars: 800 } },
      ...seatRecords,
    ] as FlowRecord[];
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={missionRecords} tMax={D0 + 5000} tMin={D0} playhead={D0 + 5000} historical />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    // 9 seat executions, 1 collapsed run.
    expect(document.querySelector(".mach-scope__pager-n")!.textContent).toBe("1/9");
    const countEl = document.querySelector(".runs--live, .runs")!;
    expect(countEl.textContent).toBe("1 run · 9 executions · 100 tok/s");
  });

  it("no pager renders for exactly one running execution — same as before this issue", async () => {
    const oneExecution = threeExecutionRecords.filter((r) => r.session_id === "s1");
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={oneExecution} tMax={D0 + 5000} tMin={D0} playhead={D0 + 5000} historical />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__rate")).not.toBeNull());
    expect(document.querySelector(".mach-scope__pager")).toBeNull();
    // The count line has NO tok/s suffix at N=1 — that's still the rate
    // line's job, as before.
    expect(document.querySelector(".runs--live")!.textContent).toBe("1 running");
  });

  it("(#2890) the card's tube carries the live rate in its center while generating, nothing while resting", async () => {
    // `TokenScope` is mocked in this file (a probe that records its props),
    // so this reads what the card HANDS the tube; TokenScope.test.tsx pins
    // how the tube renders a center label.
    const scopeProps = () =>
      JSON.parse(document.querySelector('[data-testid="token-scope-probe"]')!.getAttribute("data-props")!);
    renderThree(5);
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("100 tok/s");
    expect(scopeProps()).toMatchObject({ state: "generating", centerLabel: "100", centerUnit: "tok/s", centerCarried: false });
    fireEvent.click(screen.getByLabelText("next execution"));
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("rest 13s");
    expect(scopeProps().centerLabel ?? null).toBeNull();
    expect(scopeProps().centerUnit ?? null).toBeNull();
  });

  it("an arrow click changes the page and does not fire the card's machine drill-in", async () => {
    renderThree(5);
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    expect(window.location.hash).toBe("");
    fireEvent.click(screen.getByLabelText("next execution"));
    expect(document.querySelector(".mach-scope__pager-n")!.textContent).toBe("2/3");
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("reviewer");
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("rest 13s");
    // (#1903-shaped) The arrow is its own tap target — stopPropagation kept
    // it from ALSO firing the outer card's `machineDrillHash` click.
    expect(window.location.hash).toBe("");
  });

  // (#2886 pass 5, MUST — fresh-reviewer finding F5) No test clicked the
  // PREVIOUS arrow specifically — a mutation wiring it to the SAME `+1` the
  // next arrow uses stayed green. Wrap-around from page 1 is the
  // distinguishing case: `+1` would land on page 2 (indistinguishable from
  // clicking "next"); a correct `-1` wraps to the LAST page.
  it("the previous arrow moves BACKWARD (wraps to the last page from page 1), not the same direction as next", async () => {
    renderThree(5);
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    expect(document.querySelector(".mach-scope__pager-n")!.textContent).toBe("1/3");
    fireEvent.click(screen.getByLabelText("previous execution"));
    expect(document.querySelector(".mach-scope__pager-n")!.textContent).toBe("3/3");
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("fetch-render");
  });

  it("the picked page sticks until that execution ends, then falls forward to the new busiest among what's left", async () => {
    const { rerender } = renderThree(5);
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    // Pick s2 (reviewer, resting) — one click forward from the default s1.
    fireEvent.click(screen.getByLabelText("next execution"));
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("reviewer");

    // s2 ends; s1 (generating) and s3 (prompt) are still running.
    const afterS2Ends: FlowRecord[] = [
      ...threeExecutionRecords,
      { ts: at(4), machine_uid: "u1", session_id: "s2", action: "dispatch.complete" } as FlowRecord,
    ];
    rerender(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={afterS2Ends} tMax={D0 + 6000} tMin={D0} playhead={D0 + 6000} historical />
      </QueryClientProvider>,
    );

    // Still a pager (2 executions left), but the sticky pick (s2) is gone —
    // falls forward to the new busiest (s1, generating), not to whichever
    // index s2 used to occupy.
    await waitFor(() => expect(document.querySelector(".mach-scope__pager-n")!.textContent).toBe("1/2"));
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("coder");
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("100 tok/s");
  });

  // (#2886 pass 5, MUST — fresh-reviewer finding F3) A lit GEN lamp with no
  // reading yet (one heartbeat, no same-turn pair) must not print a
  // confident "0 tok/s" — same "—" the run page's tile already shows for
  // the identical case. The tube must not be driven by a fake 0 either —
  // pinned via the mocked TokenScope so a future regression that only hits
  // the tube (leaving the text correct) still goes red.
  it("shows '—', not '0 tok/s', while generating with no reading yet — and never drives the tube with a fake 0", async () => {
    // (mirrors `tokenRate.test.ts`'s own fixture note) `dispatch.start` sits
    // 5s before the heartbeat so `deriveLiveState`'s same-second marker tie
    // rule doesn't fire and read this as PROMPT instead of GENERATING.
    const oneFreshHeartbeat: FlowRecord[] = [
      { ts: at(-5), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "darkmux/coder" },
      { ts: at(0), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 40 } },
    ] as FlowRecord[];
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={oneFreshHeartbeat} tMax={D0} tMin={D0} playhead={D0} historical />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__rate")).not.toBeNull());
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("—");
    expect(latestTokenScopeProps()).toMatchObject({ tokensPerSec: null, state: "generating" });
  });

  // (#2886 pass 5, MUST — fresh-reviewer finding F6) The default page must
  // not flap: recomputing "the busiest execution" from scratch every tick
  // flipped a real fleet's default page 46 times in 863s, because two
  // GENERATING executions' fluctuating rates kept trading the tie-break.
  describe("the default page does not flap on a tie, only moves on a STRICT state-class win", () => {
    // s1: CODER, generating at 100 tok/s (0 -> 800 chars over 2s).
    // s2: REVIEWER, generating at 10 tok/s (0 -> 80 chars over 2s) at first.
    const twoGenerating = (s2Chars: number): FlowRecord[] => [
      { ts: at(-5), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "darkmux/coder" },
      { ts: at(0), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 0 } },
      { ts: at(2), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 2000, generated_chars: 800 } },
      { ts: at(-5), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.start", handle: "darkmux/reviewer" },
      { ts: at(0), machine_uid: "u1", session_id: "s2", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 0 } },
      { ts: at(2), machine_uid: "u1", session_id: "s2", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 2000, generated_chars: s2Chars } },
    ] as FlowRecord[];

    it("keeps the SAME default page once s2's rate overtakes s1's — both still generating", async () => {
      const { rerender } = render(
        <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
          <FleetLens records={twoGenerating(80)} tMax={D0 + 5000} tMin={D0} playhead={D0 + 5000} historical />
        </QueryClientProvider>,
      );
      // Initial pick: s1 is the faster of the two (100 vs 10 tok/s).
      await waitFor(() => expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("coder"));

      // s2's rate now FAR exceeds s1's (2000 chars -> 250 tok/s vs s1's
      // unchanged 100) — recomputing "busiest" from scratch would flip to
      // s2. Both are still `generating`, a tie at the STATE-CLASS level, so
      // the sticky default must not move.
      rerender(
        <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
          <FleetLens records={twoGenerating(2_000)} tMax={D0 + 5000} tMin={D0} playhead={D0 + 5000} historical />
        </QueryClientProvider>,
      );
      expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("coder");
      expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("100 tok/s");
    });

    it("DOES move once the currently-shown execution becomes strictly worse (generating -> rest) while the other keeps generating", async () => {
      // Same starting point as the no-flap test above: s1 is the faster of
      // the two, so it's the initial default.
      const { rerender } = render(
        <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
          <FleetLens records={twoGenerating(80)} tMax={D0 + 5000} tMin={D0} playhead={D0 + 5000} historical />
        </QueryClientProvider>,
      );
      await waitFor(() => expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("coder"));

      // s1 now rests (a real state-class change); s2 keeps generating.
      // s2 is STRICTLY busier now (generating beats rest) — this is a real
      // switch, not a flap, and must happen.
      const s1Rests: FlowRecord[] = [...twoGenerating(2_000), { ts: at(3), machine_uid: "u1", session_id: "s1", action: "dispatch.rest", payload: { ms: 15_000 } } as FlowRecord];
      rerender(
        <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
          <FleetLens records={s1Rests} tMax={D0 + 6000} tMin={D0} playhead={D0 + 6000} historical />
        </QueryClientProvider>,
      );
      await waitFor(() => expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("reviewer"));
      expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("250 tok/s");
    });
  });

  // (#2886 pass 5, MUST — fresh-reviewer finding F4) The half-open evidence
  // threaded into each execution's OWN reading (`cards.ts`'s
  // `executionTokenReading` call — `lastHeartbeatMs([recs])`, not the
  // machine-wide `lastHeartbeatMs(liveTokRecordSets)` the AGGREGATE uses)
  // was previously only asserted on `card.executions[i].state` directly —
  // nothing rendered proved it reached the screen. This pins it at the
  // rendered surface: s1 (fresh, generating) and s2 (stale, would read
  // STALLED on its own) share one card. `lastContactMs` sits AFTER s2's own
  // deadline (so a CORRECT per-execution check trusts s2's stall) but
  // BEFORE s1's much-later deadline (so a WRONG machine-wide check — using
  // s1's fresher heartbeat as the deadline for BOTH executions — would
  // wrongly downgrade s2 to "no signal" instead).
  it("downgrades a stalled execution using ITS OWN last heartbeat as the half-open deadline, not the machine-wide one", async () => {
    const records: FlowRecord[] = [
      // s1: CODER, fresh — generating, last heartbeat at 95s.
      { ts: at(-5), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "darkmux/coder" },
      { ts: at(93), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 93_000, generated_chars: 0 } },
      { ts: at(95), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 95_000, generated_chars: 800 } },
      // s2: REVIEWER, stale — one heartbeat at 0s, long past STALL_AFTER_MS
      // (30s) by the t=100s playhead. Its OWN deadline is 0 + 30 = 30s.
      { ts: at(-5), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.start", handle: "darkmux/reviewer" },
      { ts: at(0), machine_uid: "u1", session_id: "s2", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 40 } },
    ] as FlowRecord[];
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        {/* lastContactMs = 40s: past s2's own 30s deadline (trust s2's
            stall) but nowhere near s1's 95+30=125s deadline (a machine-wide
            check would NOT trust it). */}
        <FleetLens records={records} tMax={D0 + 100_000} tMin={D0} playhead={D0 + 100_000} historical connected lastContactMs={D0 + 40_000} />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    // Default page is s1 (generating beats stalled/no-signal either way).
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("coder");
    fireEvent.click(screen.getByLabelText("next execution"));
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("reviewer");
    expect(document.querySelector(".mach-scope__rate")!.textContent?.toLowerCase()).toBe("stalled");
    expect(latestTokenScopeProps()).toMatchObject({ state: "stalled" });
  });

  // (#2886 pass 5, MUST — fresh-reviewer finding F4, second half) The test
  // above alone does not prove the half-open evidence is CONSULTED at all —
  // `lastContactMs=40s` trusts s2's stall either way: with the correct
  // per-execution deadline (30s) OR with no check running at all (passing
  // `undefined`, which trusts every stall unconditionally while connected).
  // This one moves `lastContactMs` BEFORE s2's own deadline, so the CORRECT
  // behavior downgrades to "no signal" — a result "no check ran" cannot
  // produce (it would still read "stalled").
  it("downgrades to 'no signal' when contact came BEFORE the stalled execution's own deadline", async () => {
    const records: FlowRecord[] = [
      { ts: at(-5), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "darkmux/coder" },
      { ts: at(93), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 93_000, generated_chars: 0 } },
      { ts: at(95), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 95_000, generated_chars: 800 } },
      // s2's own deadline is 0 + 30 = 30s.
      { ts: at(-5), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.start", handle: "darkmux/reviewer" },
      { ts: at(0), machine_uid: "u1", session_id: "s2", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 40 } },
    ] as FlowRecord[];
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        {/* lastContactMs = 20s: BEFORE s2's own 30s deadline. */}
        <FleetLens records={records} tMax={D0 + 100_000} tMin={D0} playhead={D0 + 100_000} historical connected lastContactMs={D0 + 20_000} />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    fireEvent.click(screen.getByLabelText("next execution"));
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("reviewer");
    expect(document.querySelector(".mach-scope__rate")!.textContent?.toLowerCase()).toBe("no signal");
    expect(latestTokenScopeProps()).toMatchObject({ state: "nosignal" });
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

  // (#1855) The card's HARDWARE line, on the same card. Rendering the
  // rostered-but-silent machine is half the fix; the other half is that its
  // card must not then assert something about it that nothing supports.
  // "hardware not reported" reads as a fact about the machine — it answered
  // and withheld its hardware — and nothing has been received from this one
  // at all.
  it("a rostered-but-silent card says its hardware is unknown, not that the machine failed to report it", async () => {
    mockFleetFetch({ roster: [{ id: "studio", address: "100.64.1.2:8765", added_unix_ms: 1000 }] });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("hardware unknown — nothing received");
    expect(card.textContent).not.toContain("hardware not reported");
  });

  // The INVERTED case for that line: a machine that DID beat, carrying no
  // `specs` (every peer on a build older than #2083 — and both machines in
  // this issue's own wire dump), keeps the original sentence. It answered.
  it("a machine that beat WITHOUT specs still reads 'hardware not reported'", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "studio", schema_version: "1.20.0", beat_ts_ms: 1 }],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("hardware not reported");
    expect(card.textContent).not.toContain("hardware unknown");
  });

  // And the fully-healthy inverted case: a beat that DOES carry hardware
  // renders the hardware and neither sentence. A change that made every card
  // read as uncertain would be as wrong as the one that made them all read
  // as confident.
  it("a machine beating WITH specs shows the hardware and no unknown line at all", async () => {
    mockFleetFetch({
      machines: [
        {
          machine_uid: "u1",
          display_name: "studio",
          schema_version: "1.20.0",
          beat_ts_ms: 1,
          specs: "Apple M1 Max · 32 GB",
        },
      ],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("Apple M1 Max · 32 GB");
    expect(card.textContent).not.toContain("hardware not reported");
    expect(card.textContent).not.toContain("hardware unknown");
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

  // (#1855 follow-up, F1) THE SELF-MACHINE PHANTOM: presence self-disables
  // when Redis is unset (the default), so a quiet window can carry ZERO
  // flow history and ZERO live beats for the daemon serving this very
  // page — even though its own roster entry exists (this project's
  // `darkmux-add-machine` skill walks the operator through creating it at
  // step 7). Before consulting `/machine/specs` here, that roster entry
  // fell through every other identity check and rendered a false "offline"
  // card for the machine that is, self-evidently, up and answering this
  // request. The honest render, absent any presence/flow evidence, is NO
  // card — same as the "genuinely gone" case just above — not a lying
  // "offline" one.
  it("a roster entry matching THIS machine's own /machine/specs identity, with zero flow/presence evidence, renders no phantom offline card", async () => {
    mockFleetFetch({
      specs: { machine_id: "studio", cpu_brand: "Apple M5 Max" },
      roster: [{ id: "studio", address: "100.64.1.2:8765", added_unix_ms: 1000 }],
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens({}, queryClient);
    await waitFor(() => expect(screen.getByText(/tokens · last/i)).toBeInTheDocument());
    await waitForFleetQueriesSettled(queryClient);
    expect(document.querySelector(".mach")).toBeNull();
  });

  // (#1855 follow-up, F2) THE MISMATCHED-NAME DUPLICATE: the roster id is
  // operator-typed prose with nothing validating it against what the peer
  // actually beats as. A live peer beating as one name and rostered under
  // a near-miss (here, differing only by the mDNS `.local` suffix — the
  // same alias class #2030 already hit for one machine's own two names)
  // used to render a SECOND, phantom "offline" card beside the real one —
  // three cards for two machines.
  it("a roster entry differing from a live beat only by the mDNS .local suffix does not duplicate its card", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro.local", schema_version: "1.20.0", beat_ts_ms: 1 }],
      roster: [{ id: "MacBook-Pro", address: "100.64.1.2:8765", added_unix_ms: 1000 }],
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens({}, queryClient);
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    await waitForFleetQueriesSettled(queryClient);
    expect(document.querySelectorAll(".mach").length).toBe(1);
    expect(document.querySelector(".mach")!.className).not.toContain("absent");
  });

  // (#2768) THE THREE-GENERATIONS-OF-RENAME DUPLICATE: the exact shape
  // #1855 follow-up F2's mDNS-suffix fix above does NOT reach. `laptop` and
  // `MacBook-Pro` share no substring at all — no case fold, no `.local`
  // strip, no whitespace trim closes that gap — so a roster entry with a
  // matching `machine_uid` but a wholly different `id` used to render as a
  // SECOND, phantom "offline" card beside the machine's real, live one
  // (measured live: one machine wearing `laptop` → `MacBook-Pro.local` →
  // `MacBook-Pro` over time rendered FOUR cards for two machines). A
  // same-name fixture (the test just above) would pass against this bug —
  // only the uid join can catch it.
  it("a roster entry whose machine_uid matches a live beat under a wholly different name does not duplicate its card, and the MACHINE's name titles it", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "F9ACF59C-UID", display_name: "MacBook-Pro", schema_version: "1.20.0", beat_ts_ms: 1 }],
      roster: [{ id: "laptop", address: "127.0.0.1:8765", added_unix_ms: 1000, machine_uid: "F9ACF59C-UID" }],
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens({}, queryClient);
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    await waitForFleetQueriesSettled(queryClient);
    // One card, not two.
    expect(document.querySelectorAll(".mach").length).toBe(1);
    const card = document.querySelector(".mach")!;
    expect(card.className).not.toContain("absent");
    // Relabeled to the roster's declared name, not the beat's display_name.
    // Scoped to the CARD's own name element: the activity timeline lane
    // below still legitimately labels its lane from `nameOf` (flow-derived,
    // untouched by this override — it is a separate question from the
    // card's label), so "MacBook-Pro" is still on the page elsewhere.
    // (#2802 regression fix) This assertion used to be INVERTED: #2768 had
    // the roster id override the card title, so it required "laptop" and
    // forbade "MacBook-Pro". That was inert while roster entries carried no
    // uid. Once #2802 began back-filling uids from flow history the override
    // started firing on entries nobody had aliased on purpose, and this
    // machine's card rendered as `laptop` — a June roster id pointing at a
    // port with no daemon — directly above an activity lane reading
    // `MacBook-Pro`. One machine, two names, one screen.
    //
    // The machine's own `machine_id` titles the card now: it is what every
    // flow record carries, what the activity lanes and run rows use, and what
    // `nameOf` was fixed in #2030 to track. The operator's alias is not
    // discarded — it rides along as secondary text — but it never replaces
    // the name the machine answers to.
    //
    // What #2768 actually fixed is asserted above and still holds: ONE card,
    // not two. That was always the defect; the relabel was a choice bundled
    // with it.
    expect(card.querySelector(".name")!.textContent).toContain("MacBook-Pro");
  });

  // The inverted case: a roster `machine_uid` that matches NOTHING
  // currently known is a genuinely separate, silent machine and must still
  // render its own "rostered, never seen" card — the uid join narrows,
  // it never widens into "any roster entry with a uid is accounted for."
  it("a roster entry with a machine_uid matching no known machine still renders its own offline card", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "F9ACF59C-UID", display_name: "MacBook-Pro", schema_version: "1.20.0", beat_ts_ms: 1 }],
      roster: [{ id: "mini-1", address: "100.64.1.9:8765", added_unix_ms: 1000, machine_uid: "NEVER-SEEN-UID" }],
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens({}, queryClient);
    await waitFor(() => expect(document.querySelectorAll(".mach").length).toBeGreaterThanOrEqual(2));
    await waitForFleetQueriesSettled(queryClient);
    const cards = document.querySelectorAll(".mach");
    expect(cards.length).toBe(2);
    const names = [...cards].map((c) => c.querySelector(".name")!.textContent);
    expect(names.some((n) => n?.includes("MacBook-Pro"))).toBe(true);
    expect(names.some((n) => n?.includes("mini-1"))).toBe(true);
  });

  // (#1855 follow-up, CONSIDER 4) A present-but-corrupt roster file used to
  // be indistinguishable from a genuinely empty one: `useFleetRoster`
  // dropped `error` entirely, so every previously-visible rostered card
  // vanished again with NO signal that the read had failed — reproducing
  // #1855's own symptom via the very fix meant to close it. The literal
  // asserted here is the server's fixed wire string
  // (`fleet_roster_handler`'s `ROSTER_READ_FAILED`), never a raw parse
  // error, matching that crate's own redaction discipline.
  it("a corrupt roster file surfaces a visible notice instead of silently reading as an empty roster", async () => {
    mockFleetFetch({ rosterError: "the fleet roster file exists but could not be parsed" });
    renderFleetLens();
    await waitFor(() => expect(screen.getByText(/fleet roster unreadable/i)).toBeInTheDocument());
    expect(screen.getByText(/the fleet roster file exists but could not be parsed/)).toBeInTheDocument();
  });

  it("a healthy (non-corrupt) roster renders no roster-unreadable notice", async () => {
    mockFleetFetch({ roster: [{ id: "studio", address: "100.64.1.2:8765", added_unix_ms: 1000 }] });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(screen.queryByText(/fleet roster unreadable/i)).not.toBeInTheDocument();
  });
});

// ── (#2814) SELF IS NEVER UNKNOWN — the rendered fixture ──────────────
describe("FleetLens — this machine identifies itself without the flow window (#2814)", () => {
  //
  // The operator's own acceptance test, verbatim: an EMPTY flow window, no
  // presence beats, no roster, with only `/machine/specs` answering. This is
  // not a contrived state — it is a fresh install, a machine whose Redis is
  // off (presence self-disables, the off-by-default case), and any machine
  // whose last flow record has aged out of the retained window. Time passing
  // is enough to reach it.
  //
  // Pre-#2814 this rendered ZERO cards. `machineUids` unions flow-derived
  // uids with beating presence keys and both are empty, and the F1
  // self-check inside `rosterOnlyEntries` correctly suppresses the self
  // roster entry as already-accounted-for — so nothing accounts for it. The
  // daemon answering the request drew nothing at all about itself.
  it("(#2814) renders THIS machine's own card from /machine/specs alone — empty window, no beats, no roster", async () => {
    mockFleetFetch({
      specs: {
        machine_id: "MacBook-Pro",
        machine_uid: "F9ACF59C-0E8B-5092-A6B4-7C07070737D2",
        cpu_brand: "Apple M5 Max",
        ram_total_bytes: 137438953472,
      },
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens({}, queryClient);
    await waitForFleetQueriesSettled(queryClient);
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const cards = [...document.querySelectorAll(".mach")];
    expect(cards.length).toBe(1);
    // Its own name — not the raw uid `nameOf` falls back to.
    expect(cards[0].textContent).toContain("MacBook-Pro");
    expect(cards[0].textContent).not.toContain("F9ACF59C");
    // Its own hardware — read directly, never "hardware not reported".
    expect(cards[0].textContent).toContain("Apple M5 Max · 128 GB");
    expect(cards[0].textContent).not.toContain("hardware not reported");
    expect(cards[0].textContent).not.toContain("hardware unknown");
  });

  // The inverted case. Without it, a fix that unconditionally drew a card
  // for `specs.machine_uid` would pass the test above AND keep drawing a
  // duplicate beside the machine's real, live card once the window has
  // records again — which is the #2796 phantom, reintroduced from the other
  // direction.
  it("(#2814) does NOT draw a second card when the window already knows this machine's uid", async () => {
    const today = todayUTC();
    const uid = "F9ACF59C-0E8B-5092-A6B4-7C07070737D2";
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: uid, machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      specs: { machine_id: "MacBook-Pro", machine_uid: uid, cpu_brand: "Apple M5 Max" },
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens({}, queryClient);
    await waitForFleetQueriesSettled(queryClient);
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(document.querySelectorAll(".mach").length).toBe(1);
  });

  // And the roster half: an entry the operator declared under a name this
  // machine no longer uses, carrying its uid. #2814 puts the self uid in the
  // card list unconditionally, so without the uid join in `rosterOnlyEntries`
  // that stale entry draws an "offline" phantom beside the live self card.
  it("(#2814) a stale roster entry carrying this machine's uid does not draw a phantom", async () => {
    const uid = "F9ACF59C-0E8B-5092-A6B4-7C07070737D2";
    mockFleetFetch({
      roster: [{ id: "laptop", machine_uid: uid, address: "100.64.1.2:8765", added_unix_ms: 1000 }],
      specs: { machine_id: "MacBook-Pro", machine_uid: uid, cpu_brand: "Apple M5 Max" },
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens({}, queryClient);
    await waitForFleetQueriesSettled(queryClient);
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(document.querySelectorAll(".mach").length).toBe(1);
    expect(document.querySelector(".mach")!.textContent).toContain("MacBook-Pro");
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

// ── (#2817) waiting is not zero ──────────────────────────────────────────
//
// Operator: "the dashboard/fleet view shows all zeros for the token data as
// if it's waiting for the first flow record to initiate the values. but for
// at least a few seconds it reads as legit."
//
// Every figure in the hero computes to 0 from an empty window, and 0 is a
// MEASUREMENT — "this fleet used no cloud tokens" — held long enough to be
// believed. Same class as the other integrity defects in this area: stating
// a fact the data does not support.
//
// The distinction these two tests protect is what makes the fix correct
// rather than merely quiet. A SETTLED zero is real and must still read "0";
// a fresh fleet that has genuinely run nothing deserves to be told so.
describe("FleetLens — the token hero distinguishes waiting from zero (#2817)", () => {
  it("silhouettes the figures while the flow window is still loading", async () => {
    // A fetch that never resolves: the window stays pending, which is the
    // several seconds the operator saw reading as legitimate data.
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));

    renderFleetLens();

    const hero = await waitFor(() => {
      const el = document.querySelector(".savings");
      expect(el).not.toBeNull();
      return el as HTMLElement;
    });
    expect(hero.getAttribute("data-settled")).toBe("false");
    expect(hero.getAttribute("aria-busy")).toBe("true");
  });

  it("shows a real 0 once the window settles with no dispatches", async () => {
    mockFleetFetch({ flowToday: [], flowYesterday: [] });

    renderFleetLens();

    const hero = await waitFor(() => {
      const el = document.querySelector('.savings[data-settled="true"]');
      expect(el).not.toBeNull();
      return el as HTMLElement;
    });
    expect(hero.getAttribute("aria-busy")).not.toBe("true");
    expect(
      hero.textContent,
      "a settled empty fleet HAS used no tokens — that zero is a measurement and must survive",
    ).toMatch(/0/);
  });
});

// (#2830) While the hero is loading, the real figures must not be visible.
//
// #2817 tried to achieve that with `color: transparent` plus a `::after`
// skeleton. Both halves fail. The colour is a specificity TIE against
// `.savlead.cloud .savnum` and `.savlead.unknown .savnum` (all three are
// (0,3,0)) which the variant rules win on source order, so the cloud and
// unattributed figures are never made transparent. And the skeleton is inset
// `0.12em/0.18em` inside a box whose `line-height: 1` is already shorter than
// the glyphs it paints, so whatever is still painted shows above and below it.
// The operator saw exactly that on a phone: local clean, cloud bleeding teal,
// unattributed bleeding grey.
//
// This test asserts the property that actually matters and that jsdom CAN
// judge: while unsettled, no tile renders figure text at all. "Is it visually
// covered" needs layout and is untestable here, which is precisely why the
// CSS-only approach shipped broken.
//
// It deliberately checks EVERY tile. An assertion written against the local
// tile alone passes on the broken build, because local is the one tile with no
// variant colour rule to beat it.
describe("savings hero: nothing leaks while loading (#2830)", () => {
  it("renders no figure text in any tile while the window is unsettled", async () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    renderFleetLens();

    const hero = await waitFor(() => {
      const el = document.querySelector('.savings[data-settled="false"]');
      expect(el, "the hero should be mounted and unsettled").toBeTruthy();
      return el as HTMLElement;
    });

    const figures = Array.from(hero.querySelectorAll(".savnum, .scv"));
    // Guard against a vacuous pass: if the selectors ever stop matching, an
    // empty list would satisfy the assertion below while proving nothing.
    expect(figures.length, "expected the hero's figure elements to exist").toBeGreaterThan(0);

    const leaked = figures
      .filter((el) => (el.textContent ?? "").trim() !== "")
      .map((el) => `${el.className}="${el.textContent}"`);
    expect(leaked, "no figure text may render while unsettled").toEqual([]);
  });

  // (#2886 pass 4, do-it — fresh-reviewer finding 7, "add tests for... both
  // dimmed renders") The DOM-level counterparts to `cards.test.ts`'s
  // data-layer coverage of `liveTokCarried`/the half-open no-signal read —
  // this proves the JSX actually stamps `data-carried`/"no signal" from
  // those fields, not just that the underlying derivation is correct.
  describe("the TOK/S rate line's carried and no-signal renders", () => {
    // Hardcoded to FROZEN_NOW's own date rather than `todayUTC()` — this
    // `describe` body runs at COLLECTION time, before `beforeEach`'s fake
    // timers are installed, so `todayUTC()` here would read the REAL
    // wall-clock date and build timestamps chronologically AFTER
    // FROZEN_NOW, which fails every `T(ts) <= t` liveness check silently
    // (found live: `machActive` read false, `sessionRunning` still read
    // true via a different path, so the card rendered "idle" with a
    // contradictory "1 running" tap target).
    // Anchored so the LAST heartbeat sits 2s before FROZEN_NOW (10:02:00) —
    // fresh under STALL_AFTER_MS (30s).
    const t1a = "2026-06-15T10:00:00.000Z";
    const t1b = "2026-06-15T10:00:02.000Z";
    const t1c = "2026-06-15T10:00:04.000Z";
    const t2 = "2026-06-15T10:01:58.000Z";

    it("marks the rate line (data-carried=true) when the reading is carried forward from an earlier turn", async () => {
      mockFleetFetch({
        flowToday: [
          { ts: t1a, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
          // Turn 1 opens at 0 (every turn does — finding 2), then two
          // real-progress intervals (400 chars/s each).
          { ts: t1a, machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: Date.parse(t1a), generated_chars: 0, turn_seq: 1 } },
          { ts: t1b, machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: Date.parse(t1b), generated_chars: 800, turn_seq: 1 } },
          { ts: t1c, machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: Date.parse(t1c), generated_chars: 1_600, turn_seq: 1 } },
          // Turn 2: lone first heartbeat — nothing of its own to read from yet.
          { ts: t2, machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: Date.parse(t2), generated_chars: 50, turn_seq: 2 } },
        ],
      });
      renderFleetLens({ connected: true });

      const rate = await waitFor(() => {
        const el = document.querySelector(".mach-scope__rate");
        expect(el, "the rate line should be mounted").toBeTruthy();
        return el as HTMLElement;
      });
      // Turn 1: 800 chars / 2s = 400 chars/s -> 100 tok/s at the default.
      expect(rate.textContent).toContain("100");
      expect(rate.getAttribute("data-carried")).toBe("true");
    });

    it("shows literal 'no signal' text, not 'stalled', when the page is disconnected over an otherwise-stale heartbeat", async () => {
      mockFleetFetch({
        flowToday: [
          { ts: t1a, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
          { ts: t1a, machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: Date.parse(t1a), generated_chars: 40 } },
          { ts: t1b, machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: Date.parse(t1b), generated_chars: 120 } },
        ],
      });
      renderFleetLens({ connected: false });

      const rate = await waitFor(() => {
        const el = document.querySelector(".mach-scope__rate");
        expect(el, "the rate line should be mounted").toBeTruthy();
        return el as HTMLElement;
      });
      expect(rate.textContent?.toLowerCase()).toContain("no signal");
      expect(rate.textContent?.toLowerCase()).not.toContain("stalled");
    });
  });
});

// (#2890) The fleet card's tube morphs through the same states as the run
// page's hero, and in TOOLS it gets the tool for its icon.
describe("FleetLens card scope: the tool icon (#2890)", () => {
  const D0 = Date.UTC(2026, 8, 24, 1, 0, 0);
  const at = (sec: number) => new Date(D0 + sec * 1000).toISOString();
  it("hands the card's scope the running execution's tool while it is in TOOLS", async () => {
    const records: FlowRecord[] = [
      { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "darkmux/coder" },
      { ts: at(1), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 1_000, generated_chars: 0 } },
      { ts: at(3), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 3_000, generated_chars: 800 } },
      { ts: at(4), machine_uid: "u1", session_id: "s1", action: "dispatch.turn", payload: { turn_seq: 1, tool_calls_count: 2 } },
      { ts: at(5), machine_uid: "u1", session_id: "s1", action: "dispatch.tool", payload: { tool_name: "search" } },
    ] as FlowRecord[];
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={records} tMax={D0 + 6_000} tMin={D0} playhead={D0 + 6_000} historical />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector('[data-testid="fleet-token-scope"]')).not.toBeNull());
    expect(latestTokenScopeProps()).toMatchObject({ state: "tools", toolName: "search", size: "card" });
  });
});
