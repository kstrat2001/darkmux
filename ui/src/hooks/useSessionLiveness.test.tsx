import { describe, expect, it, vi, afterEach } from "vitest";
import { renderHook, waitFor, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { useRouteRecords } from "./useRouteRecords";
import { TERMINAL_GRACE_MS } from "./useSessionLiveness";
import { PRESENCE_POLL_MS } from "../lib/queryKeys";
import type { Route } from "../lib/route";
import type { FlowWindowResult } from "./useFlowWindow";

/**
 * (#2011) The live → done TRANSITION, which nothing covered.
 *
 * The operator watched a dispatch finish with the run-detail page open and it
 * never snapped to the done state: the pill stayed `RUNNING` and the wall
 * clock kept counting. A fresh load of the same run rendered `COMPLETE`
 * correctly — the records were right and the derivation was right. The page
 * open DURING the run simply never fetched them again.
 *
 * `refetchInterval` is gated on presence heartbeats, and when the reconciler
 * drops the session the interval becomes `false` and nothing fetches ever
 * again. If presence dropped it before a poll happened to catch
 * `dispatch complete`, the page froze on its last live snapshot permanently.
 *
 * **Why these tests drive presence directly rather than through
 * `/fleet/sessions/live`.** The other blocks in `useRouteRecords.test.tsx` let
 * the real presence query run and wait out real 5s intervals; that cannot
 * express THIS test. The presence poll and the session poll share a cadence,
 * so a session poll already scheduled when presence drops can land after the
 * drop and deliver the terminal record on its own — which makes the assertion
 * pass against the very bug it is written for, some fraction of the time.
 * Making presence a test INPUT is what makes "no poll could have caught it"
 * a property of the fixture instead of a coin flip.
 *
 * The fixture models the real emission ORDER, which is what makes the race
 * reachable at all: `dispatch_internal.rs` (#638) stops the heartbeat and
 * DELetes the presence key BEFORE it writes `dispatch complete` — "the
 * container has exited — the session is no longer running". So the terminal
 * record is, for a moment, invisible to a live-gated poll by construction.
 */

const h = vi.hoisted(() => ({ liveIds: new Set<string>() }));
vi.mock("./useLiveSessionIds", () => ({
  useLiveSessionIds: (enabled = true) => (enabled ? h.liveIds : new Set<string>()),
}));

const LIVE: FlowWindowResult = {
  settled: true,
  tMax: 0,
  data: [{ action: "LIVE-RECORD" }] as never,
};

const SID = "s-live";
const START = { ts: "2026-08-24T10:00:00Z", action: "dispatch.start", session_id: SID, handle: "coder" };
const DONE = {
  ts: "2026-08-24T10:10:15Z",
  action: "dispatch.complete",
  session_id: SID,
  payload: { wall_ms: 615920, result_class: "ok" },
};

function wrapper() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } });
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
}

/** The terminal record becomes readable only ONCE presence has already
 *  dropped the session — the real ordering above. Any fetch taken while
 *  presence still listed the session sees `[START]` and nothing else, so the
 *  ONLY way the assertion can see `DONE` is a fetch issued after the drop. */
function stubFetch() {
  const calls: string[] = [];
  const fetchMock = vi.fn(async (url: string) => {
    calls.push(String(url));
    const recs = h.liveIds.has(SID) ? [START] : [START, DONE];
    return {
      ok: true,
      status: 200,
      json: async () => ({ records: recs, count: recs.length, truncated: false, generated_at_ms: 0 }),
    };
  });
  vi.stubGlobal("fetch", fetchMock);
  return { calls, sliceFetches: () => calls.filter((u) => u.startsWith("/flow-session/")).length };
}

const ROUTE: Route = { kind: "dispatch", dispatchId: SID };

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
  h.liveIds = new Set<string>();
});

describe("the live → done transition on a session route (#2011)", () => {
  it("fetches once more when presence drops the session, so the terminal record cannot be missed", async () => {
    stubFetch();
    h.liveIds = new Set([SID]);

    const { result, rerender } = renderHook(() => useRouteRecords(ROUTE, LIVE), { wrapper: wrapper() });

    await waitFor(() => expect(result.current.records).toEqual([START]));
    // Presence says live, so this is a moving feed, not a replay.
    expect(result.current.historical).toBe(false);

    // The dispatch ends. The reconciler drops the session from presence.
    h.liveIds = new Set<string>();
    rerender();

    // Nothing but the transition itself can produce this: polling is off the
    // moment presence stops listing the session.
    await waitFor(() => expect(result.current.records).toEqual([START, DONE]));
    expect(result.current.historical).toBe(true);
  });

  it("does NOT poll a session presence never listed as live", async () => {
    // The inverted case. A fix that treated "presence does not list it" as the
    // trigger — rather than the live → not-live EDGE — would pass the test
    // above and then poll on every historical drill-in, including a replay of
    // a run that ended in January.
    //
    // The assertion is on POLLING, not on the count of immediate refetches.
    // An earlier version of this test asserted the latter and was VACUOUS:
    // deleting the edge guard left it green, because the spurious refetch it
    // triggers fires while the initial fetch is still in flight and react-query
    // simply joins the existing promise. Advancing past several poll intervals
    // is what makes the difference observable — the grace window opened by that
    // spurious edge polls, and this session must never poll at all.
    vi.useFakeTimers();
    const { sliceFetches } = stubFetch();

    const { result } = renderHook(() => useRouteRecords(ROUTE, LIVE), { wrapper: wrapper() });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(result.current.loading).toBe(false);
    expect(sliceFetches()).toBe(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(TERMINAL_GRACE_MS + 2 * PRESENCE_POLL_MS);
    });
    expect(sliceFetches()).toBe(1);
  });

  it("stops polling once the grace window closes — a presence drop is not a licence to poll forever", async () => {
    // The final fetch is a BOUNDED backstop, not a new steady state. Without
    // a bound, every finished run left open in a tab would keep asking the
    // daemon for a slice that can no longer change.
    vi.useFakeTimers();
    const { sliceFetches } = stubFetch();
    h.liveIds = new Set([SID]);

    const { result, rerender } = renderHook(() => useRouteRecords(ROUTE, LIVE), { wrapper: wrapper() });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(result.current.records).toEqual([START]);

    h.liveIds = new Set<string>();
    rerender();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(TERMINAL_GRACE_MS + 4 * PRESENCE_POLL_MS);
    });
    const settled = sliceFetches();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(10 * PRESENCE_POLL_MS);
    });
    expect(sliceFetches()).toBe(settled);
  });
});
