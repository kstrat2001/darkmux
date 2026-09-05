import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MissionGraphLens } from "./MissionGraphLens";
import { todayUTC } from "../../lib/flow";
import { queryKeys } from "../../lib/queryKeys";
import type { MissionGraph } from "./graph";
import type { FlowRecord } from "../../types/handwritten";

/** Returns the `QueryClient` too (unused by most callers) so a test can
 *  seed a cache slot this component doesn't fetch into itself — the live
 *  tail's `flowTail` slot, owned by `useLiveTail` (mounted internally, no
 *  SSE test seam on this component) — the same way `useLiveTail` itself
 *  would via `queryClient.setQueryData`. See the `.mproc` readout tests. */
function renderLens(
  missionId = "m1",
  onEvents?: (events: FlowRecord[], srvTruncated: boolean) => void,
  opts: {
    selectedStepId?: string | null;
    onSelectStep?: (stepId: string) => void;
    onStepHeader?: (fields: import("./graph").StepHeaderField[] | null) => void;
  } = {},
) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const result = render(
    <QueryClientProvider client={queryClient}>
      <MissionGraphLens
        missionId={missionId}
        onEvents={onEvents}
        selectedStepId={opts.selectedStepId}
        onSelectStep={opts.onSelectStep}
        onStepHeader={opts.onStepHeader}
      />
    </QueryClientProvider>,
  );
  return { ...result, queryClient };
}

/** The last `onEvents(events, srvTruncated)` call — the lens fires it on
 * every fold change (see that prop's own doc), so tests that only care
 * about the SETTLED value read the most recent one rather than the first. */
function lastEventsCall(spy: ReturnType<typeof vi.fn>): [FlowRecord[], boolean] | undefined {
  return spy.mock.calls.at(-1) as [FlowRecord[], boolean] | undefined;
}

/** Seeds the live-tail cache slot `useLiveTail` writes into — the SAME
 *  mechanism a real SSE `telemetry.process` message would use
 *  (`mergeTailRecords`/`setQueryData`), without standing up a real
 *  `EventSource` (this component has no injectable `eventSourceFactory`
 *  seam of its own). */
function seedLiveTail(queryClient: QueryClient, records: FlowRecord[]) {
  act(() => {
    queryClient.setQueryData(queryKeys.flowTail(todayUTC()), records);
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
  document.querySelectorAll('meta[name^="darkmux-"]').forEach((m) => m.remove());
});

const GRAPH: MissionGraph = {
  mission_id: "m1",
  mission_status: "finalized",
  nodes: [
    { id: "p1", label: "Investigate", kind: "phase", status: "complete", depth: 0 },
    {
      id: "a",
      label: "bundle",
      kind: "task",
      status: "complete",
      parentId: "p1",
      depth: 0,
      steps: [{ id: "a-step", label: "Shell", kind: "procedural.shell", status: "complete" }],
    },
  ],
  edges: [{ id: "e1", source: "p1", target: "a", kind: "contains" }],
};
// (#2332) The host readout is gated to RUNNING missions — a machine fact on a
// finalized mission is noise — so the readout tests render this twin.
const RUNNING_GRAPH: MissionGraph = { ...GRAPH, mission_status: "active" };

function mockFetch(
  opts: { graphStatus?: number; graph?: MissionGraph; flowMissionRecords?: unknown[]; flowMissionTruncated?: boolean } = {},
) {
  const graph = opts.graph ?? GRAPH;
  const graphStatus = opts.graphStatus ?? 200;
  const flowMissionRecords = opts.flowMissionRecords ?? [];
  const flowMissionTruncated = opts.flowMissionTruncated ?? false;
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      if (url.includes("/graph.json")) {
        return Promise.resolve(
          graphStatus === 200
            ? new Response(JSON.stringify(graph), { status: 200 })
            : new Response("not found", { status: graphStatus }),
        );
      }
      if (url.includes("/flow-mission/")) {
        return Promise.resolve(
          new Response(
            JSON.stringify({ records: flowMissionRecords, count: flowMissionRecords.length, truncated: flowMissionTruncated, generated_at_ms: 0 }),
            { status: 200 },
          ),
        );
      }
      if (url.startsWith(`/flow/${todayUTC()}`)) {
        return Promise.resolve(new Response(JSON.stringify([]), { status: 200 }));
      }
      return Promise.resolve(new Response("not found", { status: 404 }));
    }),
  );
}

describe("MissionGraphLens", () => {
  it("renders a loading state before the graph fetch resolves", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    renderLens();
    expect(screen.getByRole("status", { name: /loading mission m1/i })).toBeInTheDocument();
  });

  it("renders the mission id, status badge and phase/task/step DOM once the graph loads", async () => {
    mockFetch();
    renderLens();
    // `EventLogColumn`'s hidden `#logscope` ALSO carries "m1" (its
    // `scopeLabel`) — scope this to the header's `.midname` element so the
    // query stays unambiguous (matches the same discipline other lens
    // tests use around this shared component).
    await waitFor(() => expect(document.querySelector(".midname")?.textContent).toBe("m1"));
    expect(screen.getByText("finalized")).toBeInTheDocument();
    expect(document.querySelector(".phasegroup")).not.toBeNull();
    expect(document.querySelector(".mnode.k-task.s-complete")).not.toBeNull();
    expect(document.querySelector(".steprow.s-complete")).not.toBeNull();
  });

  it("renders an honest 'no mission found' message on a 404, never a blank page", async () => {
    mockFetch({ graphStatus: 404 });
    renderLens();
    await waitFor(() => expect(screen.getByRole("alert")).toBeInTheDocument());
    expect(screen.getByRole("alert").textContent).toMatch(/ephemeral or cleared run/i);
  });

  it("renders the raw diagnostic on a non-404 error, distinct from the 404 branch", async () => {
    mockFetch({ graphStatus: 500 });
    renderLens();
    await waitFor(() => expect(screen.getByRole("alert")).toBeInTheDocument());
    expect(screen.getByRole("alert").textContent).toMatch(/darkmux mission graph:/i);
  });

  it("the view-mode toggle switches between the canvas and the mobile timeline", async () => {
    mockFetch();
    renderLens();
    await waitFor(() => expect(document.querySelector(".mnode")).not.toBeNull());
    fireEvent.click(screen.getByTitle("switch renderer"));
    await waitFor(() => expect(document.querySelector(".tlphase")).not.toBeNull());
    expect(document.querySelector(".phasegroup")).toBeNull();
  });

  it("reports its scoped events upward via onEvents instead of rendering its own EventLogColumn (mainstay unification)", async () => {
    // (post-#2107/#2108) The lens's own "events" toggle button and inline
    // EventLogColumn are retired — see `MissionGraphLens.tsx`'s own doc.
    // This asserts the replacement contract: no events chrome of its own,
    // and the scoped fold reaches the caller via `onEvents`.
    const onEvents = vi.fn();
    mockFetch({
      flowMissionRecords: [{ ts: "2026-08-19T00:00:01Z", action: "mission start", handle: "m1", mission_id: "m1" }],
    });
    renderLens("m1", onEvents);
    await waitFor(() => expect(document.querySelector(".mnode")).not.toBeNull());
    expect(screen.queryByTitle("mission events")).toBeNull();
    expect(document.querySelector(".eventlog")).toBeNull();
    await waitFor(() => expect(lastEventsCall(onEvents)?.[0]).toHaveLength(1));
    expect(lastEventsCall(onEvents)?.[0][0].action).toBe("mission start");
    expect(lastEventsCall(onEvents)?.[1]).toBe(false);
  });

  it("reports the mission-scoped events in ASCENDING order via onEvents, regardless of backfill fetch order", async () => {
    // Regression coverage for a real bug caught while writing the parity
    // harness: `EventLogColumn` (this lens's events pane before the
    // mainstay-unification packet, and still the mainstay column's own
    // renderer today) expects ASCENDING input (`.slice(-LOG_CAP).reverse()`
    // internally, which is what turns it back into newest-first for
    // DISPLAY) — the same convention every OTHER caller of it already
    // follows. This lens's own `events` derivation initially sorted
    // descending (matching legacy's own array shape) and fed that straight
    // in, which made the column show the OLDEST records instead of the
    // newest. Asserted here on the pre-display array `onEvents` reports
    // (ascending, oldest first — EventLogColumn's own reversal is what
    // makes the rendered list newest-first), since this lens no longer
    // renders the column itself.
    const onEvents = vi.fn();
    mockFetch({
      flowMissionRecords: [
        { ts: "2026-08-19T00:00:01Z", action: "mission start", handle: "m1", mission_id: "m1" },
        { ts: "2026-08-19T00:00:02Z", action: "phase start", handle: "p1", mission_id: "m1" },
        { ts: "2026-08-19T00:00:03Z", action: "mission close", handle: "m1", mission_id: "m1" },
      ],
    });
    renderLens("m1", onEvents);
    await waitFor(() => expect(lastEventsCall(onEvents)?.[0]).toHaveLength(3));
    expect(lastEventsCall(onEvents)?.[0].map((r) => r.action)).toEqual(["mission start", "phase start", "mission close"]);
  });

  it("preserves the ORIGINAL backfill order among same-timestamp events, matching legacy's stable descending sort", async () => {
    // A second, narrower regression than the one above: fixing the
    // ascending/descending direction bug (previous test) is not enough on
    // its own — a naive ascending sort + EventLogColumn's own internal
    // `.reverse()` is stable in the WRONG direction for tied timestamps
    // (caught live against the parity goldens, see MissionGraphLens.tsx's
    // own `events` doc for the two-reversals-cancel fix). Three records
    // sharing one ts must come out of the pre-display (ascending) array
    // in the REVERSE of their original order — `EventLogColumn`'s own
    // reversal is what turns that back into "original order, newest group
    // first" for DISPLAY (the shape the DOM-based version of this test
    // asserted before the mainstay-unification packet moved the render
    // site out of this component).
    const onEvents = vi.fn();
    mockFetch({
      flowMissionRecords: [
        { ts: "2026-08-19T00:00:01Z", action: "phase complete", handle: "investigate", mission_id: "m1" },
        { ts: "2026-08-19T00:00:01Z", action: "phase complete", handle: "adjudicate", mission_id: "m1" },
        { ts: "2026-08-19T00:00:01Z", action: "phase complete", handle: "report", mission_id: "m1" },
        { ts: "2026-08-19T00:00:01Z", action: "mission close", handle: "m1", mission_id: "m1" },
      ],
    });
    renderLens("m1", onEvents);
    await waitFor(() => expect(lastEventsCall(onEvents)?.[0]).toHaveLength(4));
    expect(lastEventsCall(onEvents)?.[0].map((r) => r.handle)).toEqual(["m1", "report", "adjudicate", "investigate"]);
  });

  it("never collapses two genuinely different records that happen to share ts+action+handle", async () => {
    // Regression coverage for a real bug caught while writing the e2e
    // suite: an earlier dedup key (`ts+action+handle` alone, matching
    // mission-graph.html's OWN `backfillEvents` dedup key verbatim) is
    // legacy-faithful for the narrow job legacy uses it for (guarding one
    // events-panel backfill against itself), but this port also folds the
    // SAME deduped set into the METRICS accumulator, which legacy never
    // dedupes at all. Two sibling seats erroring in the same wall-clock
    // second, sharing a generic `handle`, collapsed into one — silently
    // dropping one seat's real tokens from the header meter.
    const onEvents = vi.fn();
    mockFetch({
      flowMissionRecords: [
        { ts: "2026-08-19T00:00:01Z", action: "dispatch start", handle: "h", session_id: "step-a", mission_id: "m1", payload: {} },
        { ts: "2026-08-19T00:00:01Z", action: "dispatch start", handle: "h", session_id: "step-b", mission_id: "m1", payload: {} },
      ],
    });
    renderLens("m1", onEvents);
    await waitFor(() => expect(lastEventsCall(onEvents)?.[0]).toHaveLength(2));
  });

  it("a daemon-less static build with NO published graph fixture shows an honest 'needs a daemon' notice, and asks a DAEMON for nothing", () => {
    const meta = document.createElement("meta");
    meta.name = "darkmux-flow-src";
    meta.content = "./demo-flow.jsonl";
    document.head.appendChild(meta);
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
    renderLens();
    expect(screen.getByText(/needs a running daemon/i)).toBeInTheDocument();
    // (C2) Was `not.toHaveBeenCalled()`. The lens now reads the COMMITTED
    // FLOW FILE on a static build (the shared `queryKeys.staticFlowSrc`
    // slot `App.tsx` already fills, so no extra request in the composed
    // app), which is a request — just never a daemon's. The invariant this
    // test exists for is the second clause, and it is asserted directly
    // rather than through a count that conflated the two.
    const urls = fetchMock.mock.calls.map((c) => String(c[0]));
    expect(urls.every((u) => u.endsWith("demo-flow.jsonl")), `unexpected fetch: ${JSON.stringify(urls)}`).toBe(true);
    expect(urls.some((u) => u.includes("/mission/") || u.includes("/flow-mission/") || u.includes("/flow/"))).toBe(false);
  });

  describe("static graphs fixture (#2032 packet 2)", () => {
    function injectStaticMetas() {
      const flow = document.createElement("meta");
      flow.name = "darkmux-flow-src";
      flow.content = "./demo-flow.jsonl";
      document.head.appendChild(flow);
      const graphs = document.createElement("meta");
      graphs.name = "darkmux-graphs-src";
      graphs.content = "./demo-graphs.json";
      document.head.appendChild(graphs);
    }

    /** Stubs `fetch` to answer ONLY the committed graphs-fixture path with
     *  `map` — anything else (there should be nothing else: every other
     *  query on this component is `enabled: daemonBacked`, which is false
     *  for every test in this block) 404s, so an unexpected extra call is
     *  visible in `calls` rather than silently satisfied. */
    function mockStaticFetch(map: Record<string, MissionGraph>) {
      const calls: string[] = [];
      const fetchMock = vi.fn((url: string) => {
        calls.push(url);
        if (url.endsWith("demo-graphs.json")) {
          return Promise.resolve(new Response(JSON.stringify(map), { status: 200 }));
        }
        return Promise.resolve(new Response("not found", { status: 404 }));
      });
      vi.stubGlobal("fetch", fetchMock);
      return { fetchMock, calls };
    }

    it("renders a mission's real task graph from the committed fixture map, with no fetch to the daemon's /mission/:id/graph.json route", async () => {
      injectStaticMetas();
      const { calls } = mockStaticFetch({ m1: GRAPH });
      renderLens("m1");
      await waitFor(() => expect(document.querySelector(".midname")?.textContent).toBe("m1"));
      expect(screen.getByText("finalized")).toBeInTheDocument();
      expect(document.querySelector(".phasegroup")).not.toBeNull();
      expect(document.querySelector(".mnode.k-task.s-complete")).not.toBeNull();
      expect(document.querySelector(".steprow.s-complete")).not.toBeNull();
      // (C2) Two committed-fixture calls now — the graphs map and the flow
      // file the events column folds — and NONE of them named the
      // daemon-only per-mission route. Both ride cache slots the shell
      // already fills, so the composed app downloads neither twice.
      expect(calls.some((u) => u.includes("/mission/"))).toBe(false);
      expect(calls.some((u) => u.endsWith("demo-graphs.json"))).toBe(true);
      expect(calls.every((u) => u.endsWith("demo-graphs.json") || u.endsWith("demo-flow.jsonl")), `unexpected fetch: ${JSON.stringify(calls)}`).toBe(true);
    });

    it("renders the lens's existing empty/absent state — not an error, not a permanent spinner — when the routed mission isn't in the fixture map", async () => {
      injectStaticMetas();
      mockStaticFetch({ "some-other-mission": GRAPH });
      renderLens("m1");
      await waitFor(() => expect(screen.getByRole("alert")).toBeInTheDocument());
      expect(screen.getByRole("alert").textContent).toMatch(/ephemeral or cleared run/i);
      // No retry control on the static-absent branch — there is no daemon
      // behind this page for a retry to reach.
      expect(screen.queryByTitle("retry — refetch graph")).toBeNull();
    });

    it("renders a loading state while the fixture map itself is still resolving", () => {
      injectStaticMetas();
      vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
      renderLens("m1");
      expect(screen.getByRole("status", { name: /loading mission m1/i })).toBeInTheDocument();
    });

    it("(C2) folds the COMMITTED FLOW FILE on a static build, so #mission=<id> is not stuck at 0 EVENTS", async () => {
      // The static-demo sibling of U4-1. All three of this lens's record
      // sources (`/flow-mission/<id>`, `/flow/<today>`, the live tail) are
      // `enabled: daemonBacked`, so on a daemon-less build `allRecords` was
      // empty by construction: the graph rendered from the committed fixture
      // map while the shared events column beside it read "0 EVENTS" — for a
      // day the page had already downloaded whole.
      //
      // Same rule U4-1 settled on: a static build has ONE committed file and
      // it is the answer on EVERY route. Read through `useDay`, which shares
      // the cache slot `App.tsx` already fills, so this costs no request.
      injectStaticMetas();
      const rec = {
        ts: "2026-08-26T07:36:48Z",
        action: "dispatch.start",
        session_id: "s-1",
        mission_id: "m1",
        machine_id: "mac",
        category: "work",
        source: "crew",
      };
      const other = { ...rec, ts: "2026-08-26T07:36:49Z", session_id: "s-2", mission_id: "m-other" };
      const calls: string[] = [];
      vi.stubGlobal(
        "fetch",
        vi.fn((url: string) => {
          calls.push(url);
          if (url.endsWith("demo-graphs.json")) return Promise.resolve(new Response(JSON.stringify({ m1: GRAPH }), { status: 200 }));
          if (url.endsWith("demo-flow.jsonl"))
            return Promise.resolve(new Response([JSON.stringify(rec), JSON.stringify(other)].join("\n"), { status: 200 }));
          return Promise.resolve(new Response("not found", { status: 404 }));
        }),
      );
      const onEvents = vi.fn();
      renderLens("m1", onEvents);
      await waitFor(() => expect(document.querySelector(".midname")?.textContent).toBe("m1"));
      await waitFor(() => expect(lastEventsCall(onEvents)?.[0].length).toBeGreaterThan(0));

      const [events] = lastEventsCall(onEvents)!;
      // Scoped to THIS mission — the committed file is the whole day, and
      // handing the column another mission's records would be a worse bug
      // than the empty column it replaces.
      expect(events.every((r) => r.mission_id === "m1")).toBe(true);
      expect(events.some((r) => r.session_id === "s-1")).toBe(true);
      // And still no daemon route asked for.
      expect(calls.some((u) => u.includes("/flow-mission/") || u.includes("/flow/"))).toBe(false);
    });

    it("(inverted case) with no darkmux-graphs-src meta, the daemon fetch path is used — the static branch never engages", async () => {
      // No static metas injected at all — the default daemon-backed
      // harness every other test in this file exercises.
      mockFetch();
      renderLens("m1");
      await waitFor(() => expect(document.querySelector(".midname")?.textContent).toBe("m1"));
      // Reaching the rendered graph here is only possible via the daemon's
      // `/mission/:id/graph.json` route (`mockFetch`'s ONLY 200 responder)
      // — the static fixture path was never given a `darkmux-graphs-src`
      // meta to resolve, so `staticGraphsQuery` stayed disabled throughout.
      expect(document.querySelector(".mnode.k-task.s-complete")).not.toBeNull();
    });
  });

  it("reports the SERVER's own truncated cap via onEvents when /flow-mission/:id reports truncated:true (proved failing pre-fix: an earlier port discarded this flag entirely)", async () => {
    // Whether the mainstay column then shows a "+" for this is that
    // component's own concern (`EventLogColumn.test.tsx`) — this only
    // guards that the flag reaches `onEvents` at all, the handoff an
    // earlier port silently dropped.
    const onEvents = vi.fn();
    mockFetch({
      flowMissionRecords: [{ ts: "2026-08-19T00:00:01Z", action: "mission start", handle: "m1", mission_id: "m1" }],
      flowMissionTruncated: true,
    });
    renderLens("m1", onEvents);
    await waitFor(() => expect(lastEventsCall(onEvents)?.[0]).toHaveLength(1));
    expect(lastEventsCall(onEvents)?.[1]).toBe(true);
  });

  it("does NOT report a server-truncated flag when /flow-mission/:id reports truncated:false", async () => {
    const onEvents = vi.fn();
    mockFetch({
      flowMissionRecords: [{ ts: "2026-08-19T00:00:01Z", action: "mission start", handle: "m1", mission_id: "m1" }],
      flowMissionTruncated: false,
    });
    renderLens("m1", onEvents);
    await waitFor(() => expect(lastEventsCall(onEvents)?.[0]).toHaveLength(1));
    expect(lastEventsCall(onEvents)?.[1]).toBe(false);
  });

  it("(#1483) shows the host-activity readout once a telemetry.process sample lands on the live tail", async () => {
    mockFetch({ graph: RUNNING_GRAPH });
    const { queryClient } = renderLens();
    await waitFor(() => expect(document.querySelector(".mnode")).not.toBeNull());
    expect(document.querySelector(".mproc")).toBeNull();

    seedLiveTail(queryClient, [
      { ts: new Date().toISOString(), action: "telemetry.process", category: "telemetry", source: "process", payload: { cpu: 41, gpu: 72, mem: 0.5 } },
    ]);

    await waitFor(() => expect(document.querySelector(".mproc")).not.toBeNull());
    const proc = document.querySelector(".mproc");
    expect(proc?.textContent).toContain("gpu");
    expect(proc?.textContent).toContain("72%");
    expect(proc?.textContent).toContain("cpu");
    expect(proc?.textContent).toContain("41%");
    // 72% is >= the 60% "hot" threshold — the GPU figure gets the emphasis
    // class, matching legacy's own `proc.gpu >= 60 ? "hot" : ""`.
    expect(document.querySelector(".mproc b.hot")?.textContent).toBe("72%");
  });
  it("(#2332) the host readout is for RUNNING missions only — a finalized mission shows none even with a fresh sample", async () => {
    mockFetch();
    const { queryClient } = renderLens();
    await waitFor(() => expect(document.querySelector(".mnode")).not.toBeNull());
    seedLiveTail(queryClient, [
      { ts: new Date().toISOString(), action: "telemetry.process", category: "telemetry", source: "process", payload: { cpu: 41, gpu: 72 } },
    ]);
    await new Promise((r) => setTimeout(r, 50));
    expect(document.querySelector(".mproc")).toBeNull();
    expect(document.querySelector(".mstatus")?.textContent).toBe("finalized");
  });

  it("(#1483) MACHINE-level, not mission-scoped — a telemetry.process sample with no mission_id still shows", async () => {
    // Host telemetry is never stamped with a mission_id (it's a whole-box
    // sample, not per-dispatch) — this proves the readout isn't accidentally
    // filtered through `recordInMission` the way the events pane is.
    mockFetch({ graph: RUNNING_GRAPH });
    const { queryClient } = renderLens();
    await waitFor(() => expect(document.querySelector(".mnode")).not.toBeNull());

    seedLiveTail(queryClient, [
      { ts: new Date().toISOString(), action: "telemetry.process", category: "telemetry", source: "process", payload: { cpu: 10, gpu: 5 } },
    ]);

    await waitFor(() => expect(document.querySelector(".mproc")).not.toBeNull());
  });

  it("(#1483) the readout is absent when no telemetry.process sample has ever arrived", async () => {
    mockFetch();
    renderLens();
    await waitFor(() => expect(document.querySelector(".mnode")).not.toBeNull());
    expect(document.querySelector(".mproc")).toBeNull();
  });

  it("(#1483) a telemetry.process sample expires after the 12s freshness window, on the next render", async () => {
    mockFetch({ graph: RUNNING_GRAPH });
    const { queryClient } = renderLens();
    await waitFor(() => expect(document.querySelector(".mnode")).not.toBeNull());

    const t0 = Date.now();
    const nowSpy = vi.spyOn(Date, "now").mockReturnValue(t0);
    try {
      seedLiveTail(queryClient, [
        { ts: new Date(t0).toISOString(), action: "telemetry.process", category: "telemetry", source: "process", payload: { cpu: 10, gpu: 5 } },
      ]);
      await waitFor(() => expect(document.querySelector(".mproc")).not.toBeNull());

      // Advance PAST the 12s freshness window and force a re-render —
      // `ProcEl` recomputes freshness from `Date.now()` at RENDER time
      // (matching legacy's own inline `Date.now() - proc.rx < 12000`, not a
      // ticking clock), so a re-render is what surfaces the expiry.
      nowSpy.mockReturnValue(t0 + 13_000);
      fireEvent.click(screen.getByTitle("toggle minimap"));

      await waitFor(() => expect(document.querySelector(".mproc")).toBeNull());
    } finally {
      nowSpy.mockRestore();
    }
  });
});

describe("MissionGraphLens step drill-in (#2189)", () => {
  it("clicking a step row calls onSelectStep with that step's id", async () => {
    mockFetch();
    const onSelectStep = vi.fn();
    renderLens("m1", undefined, { onSelectStep });
    await waitFor(() => expect(document.querySelector(".steprow")).not.toBeNull());
    fireEvent.click(document.querySelector('.steprow[data-act="step-row"]')!);
    expect(onSelectStep).toHaveBeenCalledWith("a-step");
  });

  it("clicking anywhere on a single-step task node ALSO selects its one step (onNodeClick)", async () => {
    mockFetch();
    const onSelectStep = vi.fn();
    renderLens("m1", undefined, { onSelectStep });
    await waitFor(() => expect(document.querySelector(".mnode")).not.toBeNull());
    // Click the node's kind label, not the step row itself — this exercises
    // React Flow's own `onNodeClick`, not the row's `stopPropagation`ed one.
    fireEvent.click(document.querySelector(".mnode .mn-kind")!);
    expect(onSelectStep).toHaveBeenCalledWith("a-step");
  });

  it("the selected step's row carries the selected marker class", async () => {
    mockFetch();
    renderLens("m1", undefined, { selectedStepId: "a-step" });
    await waitFor(() => expect(document.querySelector(".steprow")).not.toBeNull());
    const row = document.querySelector(".steprow")!;
    expect(row.className).toMatch(/\bselected\b/);
    expect(row.getAttribute("data-selected")).toBe("1");
  });

  it("reports the selected step's header fields upward via onStepHeader, and null once cleared", async () => {
    mockFetch({
      flowMissionRecords: [
        { ts: "2026-08-19T00:00:01Z", action: "dispatch.start", handle: "a-step", mission_id: "m1", payload: { step_id: "a-step" } },
        { ts: "2026-08-19T00:00:02Z", action: "dispatch.complete", handle: "a-step", mission_id: "m1", payload: { step_id: "a-step", total_turns: 3, total_tokens: 1200 } },
      ],
    });
    const onStepHeader = vi.fn();
    const { rerender } = renderLens("m1", undefined, { selectedStepId: "a-step", onStepHeader });
    await waitFor(() => {
      const fields = onStepHeader.mock.calls.at(-1)?.[0];
      expect(fields).not.toBeNull();
      expect(fields.some((f: { key: string }) => f.key === "unit")).toBe(true);
    });

    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    rerender(
      <QueryClientProvider client={queryClient}>
        <MissionGraphLens missionId="m1" selectedStepId={null} onStepHeader={onStepHeader} />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(onStepHeader.mock.calls.at(-1)?.[0]).toBeNull());
  });

  it("a step selected on a COLLAPSED timeline task auto-expands that task so the step still renders", async () => {
    mockFetch();
    renderLens("m1", undefined, { selectedStepId: "a-step" });
    await waitFor(() => expect(document.querySelector(".mnode")).not.toBeNull());
    fireEvent.click(screen.getByTitle("switch renderer"));
    await waitFor(() => expect(document.querySelector(".tltask")).not.toBeNull());
    expect(document.querySelector(".tltask")!.className).toMatch(/\bopen\b/);
    expect(document.querySelector(".tlt-step")).not.toBeNull();
  });
});
// (header owns liveness, operator 2026-09-03) The lens no longer paints its
// own connection pill; the masthead's `#modebadge` is the one liveness
// indicator on every page.
describe("no lens-local liveness pill", () => {
  it("renders no .livepill", async () => {
    const { container } = renderLens();
    await new Promise((r) => setTimeout(r, 0));
    expect(container.querySelector(".livepill")).toBeNull();
  });
});

// (#2332 review) The header's sub-line and meter, pinned — the first cut
// shipped an elapsed clock that froze whenever no step was running and a
// "ran 0:00" for a mission with no terminal record; nothing here was red.
describe("mission header sub-line and meter (#2332)", () => {
  const MINTED = "crawl-x-1788484173-b562e3";
  const minted = (status: string): MissionGraph => ({ ...GRAPH, mission_id: MINTED, mission_status: status });
  const sub = () => document.querySelector(".missionlens .msub")?.textContent ?? "";

  it("shows the id's name and keeps the full id on the element; the hash rides the sub-line", async () => {
    mockFetch({ graph: minted("finalized") });
    renderLens(MINTED);
    await waitFor(() => expect(document.querySelector(".midname")?.textContent).toBe("crawl-x"));
    expect(document.querySelector(".midname")?.getAttribute("data-mission-id")).toBe(MINTED);
    expect(sub()).toContain("b562e3");
  });

  it("started HH:MM falls back to the id's epoch when no record has started anything", async () => {
    mockFetch({ graph: minted("finalized") });
    renderLens(MINTED);
    await waitFor(() => expect(document.querySelector(".midname")).not.toBeNull());
    expect(sub()).toMatch(/started \d\d:\d\d/);
  });

  it("elapsed keeps ticking on an ACTIVE mission even when no step is running (a sign-off gate)", async () => {
    mockFetch({ graph: minted("active") });
    renderLens(MINTED);
    await waitFor(() => expect(sub()).toContain("elapsed"));
    const before = sub();
    await new Promise((r) => setTimeout(r, 2300));
    expect(sub(), "the clock must move while the mission is active").not.toBe(before);
  });

  it("a finalized mission with a start but no terminal record shows NO duration, not 'ran 0:00'", async () => {
    mockFetch({
      graph: minted("finalized"),
      flowMissionRecords: [{ ts: "2026-08-19T00:00:01Z", action: "dispatch.start", handle: "a-step", mission_id: MINTED, payload: { step_id: "a-step" } }],
    });
    renderLens(MINTED);
    await waitFor(() => expect(sub()).toContain("started"));
    expect(sub()).not.toContain("ran");
  });

  it("the meter shows the cloud share only when there is one; the split lives in the tooltip", async () => {
    mockFetch({
      graph: minted("finalized"),
      flowMissionRecords: [
        { ts: "2026-08-19T00:00:01Z", action: "dispatch.start", handle: "a-step", mission_id: MINTED, payload: { step_id: "a-step" } },
        { ts: "2026-08-19T00:00:02Z", action: "dispatch.complete", handle: "a-step", mission_id: MINTED, payload: { step_id: "a-step", total_turns: 1, total_tokens: 5000, endpoint: "https://cloud.example" } },
      ],
    });
    renderLens(MINTED);
    await waitFor(() => expect(document.querySelector(".mmeter")?.textContent).toContain("(5.0k cloud)"));
    expect(document.querySelector(".mmeter")?.getAttribute("title")).toContain("5.0k cloud");
  });

  it("tapping the name reveals the full id when the clipboard is unavailable (plain-http phone over the tailnet)", async () => {
    mockFetch({ graph: minted("finalized") });
    renderLens(MINTED);
    await waitFor(() => expect(document.querySelector(".midname")?.textContent).toBe("crawl-x"));
    fireEvent.click(document.querySelector(".midname")!);
    expect(document.querySelector(".midname")?.textContent).toBe(MINTED);
  });
});
