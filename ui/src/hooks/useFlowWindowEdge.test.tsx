import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { renderHook, waitFor, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import { FLOW_WINDOW_EDGE_GRAIN_MS, flowWindowEdgeMs, useFlowWindow } from "./useFlowWindow";
import { LIVE_WINDOW_MS, todayUTC } from "../lib/flow";
import { queryKeys } from "../lib/queryKeys";
import { normAll } from "../testing/records";

// (#2911) The fleet lens re-renders once a second while an execution is live
// and passes a fresh `Date.now()` each time. The window merge (copy,
// normalize, sort, dedup of every record) must not re-run for that: its only
// clock input is a 24h trailing edge. These pin that the merged array is the
// SAME object across such renders, and that the two things that must still
// move it (a new record, the edge crossing a grain) do.

const NOW = Date.parse("2026-08-08T12:00:30.000Z");

function wrapper(queryClient: QueryClient) {
  return ({ children }: { children: React.ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
}

describe("useFlowWindow: the window edge is coarse", () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(NOW);
    const today = todayUTC();
    // One record just inside the 24h edge, one fresh.
    const records = [
      { ts: new Date(NOW - LIVE_WINDOW_MS + 20_000).toISOString(), session_id: "old", action: "dispatch.start" },
      { ts: new Date(NOW - 5_000).toISOString(), session_id: "new", action: "dispatch.start" },
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) =>
        Promise.resolve(new Response(JSON.stringify(String(url).endsWith(today) ? records : []), { status: 200 })),
      ),
    );
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("floors to the grain", () => {
    expect(flowWindowEdgeMs(NOW)).toBe(Date.parse("2026-08-08T12:00:00.000Z"));
    expect(FLOW_WINDOW_EDGE_GRAIN_MS).toBe(60_000);
  });

  it("keeps the same data array across renders inside one grain, and moves it for a new record or a new grain", async () => {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const { result, rerender } = renderHook(({ now }) => useFlowWindow(now), {
      wrapper: wrapper(qc),
      initialProps: { now: NOW },
    });
    await waitFor(() => expect(result.current.data.map((r) => r.session_id)).toEqual(["old", "new"]));
    const first = result.current.data;

    // Three one-second ticks: same object, so nothing downstream keyed on it
    // recomputes either.
    for (const dt of [1_000, 2_000, 3_000]) {
      rerender({ now: NOW + dt });
      expect(result.current.data).toBe(first);
    }

    // A record arriving on the live tail lands at once, mid-grain.
    const today = todayUTC();
    act(() => {
      // The tail cache holds ingested records (`lib/sse.ts` appends through `ingestRecord`).
      qc.setQueryData(queryKeys.flowTail(today), normAll([
        { ts: new Date(NOW + 3_000).toISOString(), session_id: "tail", action: "dispatch.start" },
      ]));
    });
    await waitFor(() => expect(result.current.data.map((r) => r.session_id)).toEqual(["old", "new", "tail"]));

    // The next grain moves the edge past the old record, which ages out.
    const withTail = result.current.data;
    rerender({ now: NOW + 3_000 });
    expect(result.current.data).toBe(withTail);
    rerender({ now: NOW + 30_000 });
    expect(result.current.data).not.toBe(withTail);
    expect(result.current.data.map((r) => r.session_id)).toEqual(["new", "tail"]);
  });
});
