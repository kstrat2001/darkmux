import { describe, it, expect, vi, afterEach } from "vitest";
import { render, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { SessionReplay } from "./SessionReplay";

/**
 * (#2011) What the OPERATOR sees when a dispatch they are watching finishes.
 *
 * `useSessionLiveness.test.tsx` pins the fetch behavior; this pins the render,
 * because the report was about the screen: the pill stayed `RUNNING` and the
 * wall clock kept counting after the run had ended. A test asserting "a
 * completed run renders COMPLETE" passes against that bug — the end states
 * were never wrong. Only the TRANSITION was.
 *
 * Presence is a test INPUT here for the reason `useSessionLiveness.test.tsx`
 * gives at length: driven through the real `/fleet/sessions/live` query, a
 * session poll already scheduled when presence drops can deliver the terminal
 * record on its own and mask the missing fetch.
 *
 * The clock is frozen and advanced explicitly, per this project's own rule —
 * no fixture may mix a fixed timestamp with a clock-relative assertion.
 */

const h = vi.hoisted(() => ({ liveIds: new Set<string>(), liveMissions: new Set<string>() }));
// (#2725) `{ sessions, coverage }` — see `useSessionLiveness.test.tsx`.
vi.mock("../../hooks/useLiveSessionIds", () => ({
  useLiveSessionIds: (enabled = true) =>
    enabled
      ? { sessions: h.liveIds, missions: h.liveMissions, coverage: null }
      : { sessions: new Set<string>(), missions: new Set<string>(), coverage: null },
}));

const SID = "s-live";
const T0 = 1_800_000_000_000;
const iso = (ms: number) => new Date(ms).toISOString();

const START = {
  ts: iso(T0 - 600_000),
  action: "dispatch.start",
  session_id: SID,
  machine_id: "M",
  handle: "coder",
  payload: { role: "coder" },
};
const BEAT = { ts: iso(T0 - 3_000), action: "dispatch.turn.heartbeat", session_id: SID, machine_id: "M", payload: {} };
// `wall_ms` DISAGREES with the timestamps on purpose: `T(close) - start` is
// exactly 10:00, the runtime measured 10:15.92. Only one of those can be read
// off the record.
const DONE = { ts: iso(T0), action: "dispatch.complete", session_id: SID, machine_id: "M", payload: { wall_ms: 615920 } };

function renderReplay() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } });
  // A FRESH element per pass. Re-rendering the identical element object lets
  // React bail out of the render entirely, so `again()` would be a no-op and
  // the presence change would only be picked up incidentally, on the next
  // clock tick — which is exactly the render this test is trying to stop.
  const ui = () => (
    <QueryClientProvider client={queryClient}>
      <SessionReplay sessionId={SID} />
    </QueryClientProvider>
  );
  const r = render(ui());
  return { ...r, again: () => r.rerender(ui()) };
}

const pillText = () => document.querySelector(".session-run__header .pill")?.textContent ?? "";
const wallText = () =>
  // (#2890) A dispatch's run time is the MODEL section's ACTIVE TIME cell.
  [...document.querySelectorAll('.metrics[data-scope="model"] .mv')].map((e) => e.textContent).join("");


// (#2890) A live run's "so far" sits on the ACTIVE TIME cell's sub line, not
// in its value (the value is the bare time so it fits the grid cell).
const activeSub = () => {
  const cell = [...document.querySelectorAll('.metrics[data-scope="model"] .met')].find(
    (c) => c.querySelector(".ml")?.textContent === "ACTIVE TIME",
  );
  return cell?.querySelector(".msub")?.textContent ?? "";
};

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
  h.liveIds = new Set<string>();
  h.liveMissions = new Set<string>();
});

describe("SessionReplay — the run finishing while the page is open (#2011)", () => {
  it("snaps to COMPLETE with the recorded wall clock when presence drops the session", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(T0);
    // The terminal record becomes readable only ONCE presence has dropped the
    // session — the producer's real order (`dispatch_internal.rs` #638 deletes
    // the presence key, THEN writes `dispatch complete`). So no poll taken
    // while the page believed the run was live could have carried it.
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        const records = h.liveIds.has(SID) ? [START, BEAT] : [START, BEAT, DONE];
        return new Response(JSON.stringify({ records, count: records.length }), { status: 200 });
      }),
    );
    h.liveIds = new Set([SID]);

    const { again } = renderReplay();
    await vi.waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());
    expect(pillText()).toContain("running");
    expect(activeSub()).toContain("so far");

    // The dispatch ends and the reconciler drops it from presence.
    h.liveIds = new Set<string>();
    await act(async () => {
      again();
      await vi.advanceTimersByTimeAsync(0);
    });

    await vi.waitFor(() => expect(pillText()).toContain("COMPLETE"));
    // 615920ms, from the record — NOT 10:00, which is what subtracting the two
    // record timestamps gives.
    expect(wallText()).toContain("10:15");
    expect(activeSub()).not.toContain("so far");
  });

  it("a mission run: live while an execution under its mission beats, then snaps to COMPLETE when it drops", async () => {
    // A mission's run-grain session never beats; only its executions do,
    // under their own ids. Keyed on the run's id alone the page was never
    // live, never saw the drop edge, and sat on RUNNING for the whole run.
    // Spelled as a mission really records it (`dispatch start`, space).
    vi.useFakeTimers();
    vi.setSystemTime(T0);
    const M = "m-2759";
    const mStart = { ...START, action: "dispatch start", mission_id: M };
    const mDone = { ...DONE, action: "dispatch complete", mission_id: M };
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (url.startsWith("/flow-mission/")) return new Response(JSON.stringify({ records: [], count: 0 }), { status: 200 });
        const records = h.liveMissions.has(M) ? [mStart, BEAT] : [mStart, BEAT, mDone];
        return new Response(JSON.stringify({ records, count: records.length }), { status: 200 });
      }),
    );
    h.liveIds = new Set(["an-execution-under-m"]);
    h.liveMissions = new Set([M]);

    const { again } = renderReplay();
    await vi.waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());
    expect(pillText()).toContain("running");

    // The last execution ends and presence drops it.
    h.liveIds = new Set<string>();
    h.liveMissions = new Set<string>();
    await act(async () => {
      again();
      await vi.advanceTimersByTimeAsync(0);
    });

    await vi.waitFor(() => expect(pillText()).toContain("COMPLETE"));
  });

  it("stops the elapsed counter when presence says the run is gone, even with no terminal record", async () => {
    // The abandoned case: the host process was killed, so no clean
    // `dispatch complete` is ever written, and the reconciler's `session.end`
    // edge may not have landed yet. Before this, the counter kept climbing for
    // a further ten minutes (`STALE_AFTER_MS`, the watchdog's kill timeout)
    // and then froze on whatever wrong number it had reached. Presence having
    // SEEN the session disappear is proof the run stopped; that beats a
    // ten-minute silence heuristic.
    vi.useFakeTimers();
    vi.setSystemTime(T0);
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify({ records: [START, BEAT], count: 2 }), { status: 200 })),
    );
    h.liveIds = new Set([SID]);

    const { again } = renderReplay();
    await vi.waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());

    // While presence says live, the counter advances on the shared clock even
    // though no record arrives (#1972). That behavior must survive.
    const ticking = wallText();
    act(() => {
      vi.advanceTimersByTime(5_000);
    });
    expect(wallText()).not.toBe(ticking);

    h.liveIds = new Set<string>();
    await act(async () => {
      again();
      await vi.advanceTimersByTimeAsync(0);
    });
    // A second flush: the presence edge is applied in an effect, so the render
    // that acts on it is one pass behind the render that observed the drop.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });

    // The counter steps BACK to the last record's own elapsed time as the
    // drop is applied (see `SessionReplay.tsx`'s comment on `endedByPresence`)
    // — read the settled value, then assert nothing moves it after that.
    const frozen = wallText();
    // (#2890) Read from the MODEL grid now: TURNS "—", TOOL CALLS "0",
    // ACTIVE TIME, then TOKENS IN/OUT and CONTEXT "—" (no telemetry in this
    // fixture). The frozen run time is the figure that matters here.
    expect(frozen).toBe("—09:57———");
    expect(activeSub()).toContain("so far");
    act(() => {
      vi.advanceTimersByTime(30_000);
    });
    expect(wallText()).toBe(frozen);
    // ...and the run is still honestly labeled unfinished, because no terminal
    // record says otherwise. Stopping the clock is not the same as claiming a
    // clean close.
    expect(pillText()).toContain("running");
  });
});
