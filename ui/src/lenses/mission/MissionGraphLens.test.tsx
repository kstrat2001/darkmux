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

  it("a daemon-less static build with NO published graph fixture shows an honest 'needs a daemon' notice, never attempts a fetch", () => {
    const meta = document.createElement("meta");
    meta.name = "darkmux-flow-src";
    meta.content = "./demo-flow.jsonl";
    document.head.appendChild(meta);
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
    renderLens();
    expect(screen.getByText(/needs a running daemon/i)).toBeInTheDocument();
    expect(fetchMock).not.toHaveBeenCalled();
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
      const { fetchMock, calls } = mockStaticFetch({ m1: GRAPH });
      renderLens("m1");
      await waitFor(() => expect(document.querySelector(".midname")?.textContent).toBe("m1"));
      expect(screen.getByText("finalized")).toBeInTheDocument();
      expect(document.querySelector(".phasegroup")).not.toBeNull();
      expect(document.querySelector(".mnode.k-task.s-complete")).not.toBeNull();
      expect(document.querySelector(".steprow.s-complete")).not.toBeNull();
      // Exactly one network call — the fixture map itself — and NONE of
      // them named the daemon-only per-mission route.
      expect(fetchMock).toHaveBeenCalledTimes(1);
      expect(calls.some((u) => u.includes("/mission/"))).toBe(false);
      expect(calls[0]).toContain("demo-graphs.json");
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

