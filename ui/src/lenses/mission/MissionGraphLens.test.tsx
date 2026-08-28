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
function renderLens(missionId = "m1") {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const result = render(
    <QueryClientProvider client={queryClient}>
      <MissionGraphLens missionId={missionId} />
    </QueryClientProvider>,
  );
  return { ...result, queryClient };
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

  it("the events toggle shows/hides the mission-scoped EventLogColumn", async () => {
    mockFetch();
    renderLens();
    await waitFor(() => expect(document.querySelector(".mnode")).not.toBeNull());
    const evbtn = screen.getByTitle("mission events");
    expect(document.querySelector(".eventlog")?.className).not.toMatch(/eventlog--hidden/);
    fireEvent.click(evbtn);
    expect(document.querySelector(".eventlog")?.className).toMatch(/eventlog--hidden/);
  });

  it("renders the mission-scoped events newest-first, regardless of backfill fetch order", async () => {
    // Regression coverage for a real bug caught while writing the parity
    // harness: `EventLogColumn` (reused as this lens's events pane) expects
    // ASCENDING input (`.slice(-LOG_CAP).reverse()` internally) — the same
    // convention every OTHER caller of it already follows. This lens's own
    // `events` derivation initially sorted descending (matching legacy's
    // own array shape) and fed that straight in, which made the column
    // show the OLDEST records instead of the newest.
    mockFetch({
      flowMissionRecords: [
        { ts: "2026-08-19T00:00:01Z", action: "mission start", handle: "m1", mission_id: "m1" },
        { ts: "2026-08-19T00:00:02Z", action: "phase start", handle: "p1", mission_id: "m1" },
        { ts: "2026-08-19T00:00:03Z", action: "mission close", handle: "m1", mission_id: "m1" },
      ],
    });
    renderLens();
    await waitFor(() => expect(document.querySelectorAll(".eventlog__rec").length).toBe(3));
    const rows = [...document.querySelectorAll(".eventlog__ractivity")].map((el) => el.textContent);
    expect(rows).toEqual(["mission close", "phase start", "mission start"]);
  });

  it("preserves the ORIGINAL backfill order among same-timestamp events, matching legacy's stable descending sort", async () => {
    // A second, narrower regression than the one above: fixing the
    // ascending/descending direction bug (previous test) is not enough on
    // its own — a naive ascending sort + EventLogColumn's own internal
    // `.reverse()` is stable in the WRONG direction for tied timestamps
    // (caught live against the parity goldens, see MissionGraphLens.tsx's
    // own `events` doc for the two-reversals-cancel fix). Three records
    // sharing one ts must render in their ORIGINAL array order, newest
    // group first — not reversed relative to each other.
    mockFetch({
      flowMissionRecords: [
        { ts: "2026-08-19T00:00:01Z", action: "phase complete", handle: "investigate", mission_id: "m1" },
        { ts: "2026-08-19T00:00:01Z", action: "phase complete", handle: "adjudicate", mission_id: "m1" },
        { ts: "2026-08-19T00:00:01Z", action: "phase complete", handle: "report", mission_id: "m1" },
        { ts: "2026-08-19T00:00:01Z", action: "mission close", handle: "m1", mission_id: "m1" },
      ],
    });
    renderLens();
    await waitFor(() => expect(document.querySelectorAll(".eventlog__rec").length).toBe(4));
    const subjects = [...document.querySelectorAll(".eventlog__rec")].map((el) => el.getAttribute("title"));
    expect(subjects).toEqual(["investigate", "adjudicate", "report", "m1"]);
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
    mockFetch({
      flowMissionRecords: [
        { ts: "2026-08-19T00:00:01Z", action: "dispatch start", handle: "h", session_id: "step-a", mission_id: "m1", payload: {} },
        { ts: "2026-08-19T00:00:01Z", action: "dispatch start", handle: "h", session_id: "step-b", mission_id: "m1", payload: {} },
      ],
    });
    renderLens();
    await waitFor(() => expect(document.querySelectorAll(".eventlog__rec").length).toBe(2));
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

  it("discloses the SERVER's own truncated cap on the events pane when /flow-mission/:id reports truncated:true (proved failing pre-fix: an earlier port discarded this flag entirely)", async () => {
    mockFetch({
      flowMissionRecords: [{ ts: "2026-08-19T00:00:01Z", action: "mission start", handle: "m1", mission_id: "m1" }],
      flowMissionTruncated: true,
    });
    renderLens();
    await waitFor(() => expect(document.querySelectorAll(".eventlog__rec").length).toBe(1));
    // A single record, well under EventLogColumn's own LOG_CAP (50) — the
    // "1+" is the SERVER's cap, not the client display cap, and must show
    // even when the client-side count isn't itself capped.
    expect(document.querySelector(".eventlog__qcount")?.textContent).toMatch(/1\+ events/);
  });

  it("does NOT append the server-truncated '+' when /flow-mission/:id reports truncated:false", async () => {
    mockFetch({
      flowMissionRecords: [{ ts: "2026-08-19T00:00:01Z", action: "mission start", handle: "m1", mission_id: "m1" }],
      flowMissionTruncated: false,
    });
    renderLens();
    await waitFor(() => expect(document.querySelectorAll(".eventlog__rec").length).toBe(1));
    expect(document.querySelector(".eventlog__qcount")?.textContent).toMatch(/^1 events$/);
  });

  it("the events pane header never claims a rolling 24h window — these are mission-scoped records, not the live window (#1868)", async () => {
    mockFetch();
    renderLens();
    await waitFor(() => expect(document.querySelector(".eventlog__head h3")).not.toBeNull());
    expect(document.querySelector(".eventlog__head h3")?.textContent).not.toMatch(/last \d+h/i);
  });

  it("(#1483) shows the host-activity readout once a telemetry.process sample lands on the live tail", async () => {
    mockFetch();
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

  it("(#1483) MACHINE-level, not mission-scoped — a telemetry.process sample with no mission_id still shows", async () => {
    // Host telemetry is never stamped with a mission_id (it's a whole-box
    // sample, not per-dispatch) — this proves the readout isn't accidentally
    // filtered through `recordInMission` the way the events pane is.
    mockFetch();
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
    mockFetch();
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
