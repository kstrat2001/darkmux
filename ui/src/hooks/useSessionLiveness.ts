import { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useLiveSessionIds } from "./useLiveSessionIds";
import { queryKeys, PRESENCE_POLL_MS } from "../lib/queryKeys";
import { getSource } from "../lib/source";
import type { DegradedFleetSource } from "../lib/fleetCoverage";

/**
 * (#2011) Is THIS session running, and should its slice keep being fetched?
 *
 * Two consumers ask the same question about the same session id and were
 * answering it with their own copy of the same three lines: `useRouteRecords`
 * (the event log's records for a `#dispatch=<id>` route) and `SessionReplay`
 * (the run-detail stage). They share a query cache slot
 * (`queryKeys.flowSession`), so they were already obliged to agree; now they
 * do so by construction.
 *
 * **Presence stays the primary liveness signal.** A slice-derived guess would
 * call a session dead the moment its `dispatch.complete` landed, even though
 * the reconciler had not yet closed it — that is why `useLiveSessionIds` is
 * the source here, and that reasoning is untouched.
 *
 * **What it left unhandled is the opposite race, which is the bug.** When the
 * reconciler drops the session, a presence-gated `refetchInterval` becomes
 * `false` and nothing ever fetches again. If the last live poll predated the
 * terminal record, the page freezes on that snapshot PERMANENTLY: the pill
 * stays `RUNNING`, and the wall clock — driven by the shared 1s store (#1972)
 * rather than by arriving records — keeps counting a number that is simply
 * wrong. A fresh load of the same run renders `COMPLETE` correctly, which is
 * what makes this so hard to see: the data is right, and only the page that
 * watched it happen is wrong.
 *
 * **Why the fix is a WINDOW and not a single final fetch.** The obvious repair
 * is one refetch on the live → not-live edge. That narrows the race but does
 * not close it, because of the emission ORDER on the producing side:
 * `crates/darkmux-crew/src/dispatch_internal.rs` (#638) stops the heartbeat
 * and DELetes the presence key *before* it writes `dispatch complete` —
 * "the container has exited — the session is no longer running... before the
 * dispatch.complete record below". So for a short interval the session is
 * absent from presence and its terminal record does not exist yet, and a
 * final fetch landing inside that interval reads exactly what the frozen page
 * already had. Polling for a bounded grace window after the drop covers it;
 * the immediate refetch is what makes the page snap in the common case rather
 * than waiting out a poll interval.
 *
 * The window is BOUNDED on purpose: a finished run left open in a tab must
 * stop asking the daemon for a slice that can no longer change.
 */

/** How long the slice keeps polling after presence drops the session.
 *
 *  Three presence cycles. It has to cover the producer-side gap described
 *  above (heartbeat key deleted, terminal record not yet written) plus the
 *  reconciler's own `session.end` edge for an ABANDONED run — the case where
 *  no clean `dispatch complete` is ever written and the close bracket comes
 *  from `presence_reconciler.rs` instead, shortly after it observes the same
 *  disappearance this hook is reacting to. One cycle would be a coin flip
 *  against both; three is ~15s of tail for a page that is otherwise idle. */
export const TERMINAL_GRACE_MS = 3 * PRESENCE_POLL_MS;

export interface SessionLiveness {
  /** Presence says this session is running right now. The honest answer to
   *  "is this a moving feed or a replay". */
  isLive: boolean;
  /** Whether this session's slice should keep being fetched — `isLive`, plus
   *  the bounded grace window after a drop. */
  shouldPoll: boolean;
  /** Presence has SEEN this session go from live to gone. Distinct from
   *  `!isLive`, which is also true for every session presence never listed
   *  (a replay, a run that ended last month, or a machine with no Redis at
   *  all — see `fleet_sessions_live_handler`, which returns an empty set
   *  when presence is off). Only the affirmative transition is evidence that
   *  the run has stopped; absence on its own is not.
   *
   *  (#2725) And a disappearance seen through a FAILED read is not the
   *  affirmative transition either — see `coverage` below. */
  endedByPresence: boolean;
  /** (#2725) Degraded fleet coverage on the presence read this liveness was
   *  derived from, in the shared vocabulary (`lib/fleetCoverage.ts`), or
   *  `null` when presence is healthy, switched off, or has not answered.
   *
   *  `useLiveSessionIds` used to discard this. It matters HERE because
   *  `endedByPresence` is consumed as direct evidence that a run stopped —
   *  `SessionReplay` stops the live clock on it, deliberately overriding its
   *  own quiet-threshold heuristic. When the presence READ fails (the daemon
   *  dies mid-run, Redis becomes unreachable), `fetchJson` returns
   *  `ok:false`, the live set empties, and the live → not-live edge fires
   *  for every session at once: the page would then report a running
   *  dispatch as finished, on the strength of a read that never happened.
   *  That is the #2683 defect one hook over, and the guard in the effect
   *  below is what closes it — the run may well have ended, but we did not
   *  see it, and the honest state is the one the quiet-threshold heuristic
   *  already handles.
   *
   *  Exposed, not merely consumed internally, so a surface that renders
   *  `isLive` has the same caveat available at the point of its own claim. */
  coverage: DegradedFleetSource | null;
}

export function useSessionLiveness(sessionId: string | null, missionId: string | null = null): SessionLiveness {
  // The `enabled` gate #1800 P2 added: a replay must not poll live presence.
  // Passing the result away is not enough — the query still fires and still
  // describes NOW.
  const { sessions: liveSessions, missions: liveMissions, coverage } = useLiveSessionIds(
    sessionId !== null && getSource().kind === "daemon",
  );
  // A mission's run-grain session never beats; its executions do, under their
  // own ids. So a run that belongs to a mission is live while any beat names it.
  const isLive =
    sessionId !== null && (liveSessions.has(sessionId) || (missionId !== null && liveMissions.has(missionId)));
  // (#2725) The presence read that produced `liveSessions` could not see the
  // fleet. Held in a ref so the effect below reads it WITHOUT re-running when
  // it changes: coverage recovering is not itself an edge, and re-running on
  // it would re-arm a window the effect had already decided about.
  const coverageRef = useRef(coverage);
  coverageRef.current = coverage;

  const queryClient = useQueryClient();
  // The session id presence last reported LIVE — not a boolean, so that
  // switching routes to a different session cannot inherit the previous
  // one's edge and fire a spurious refetch against the new id.
  const lastLiveSid = useRef<string | null>(null);
  // The session the grace window belongs to, for the same reason: a route
  // change invalidates the window instead of leaving it armed over a session
  // it was never about.
  const [graceFor, setGraceFor] = useState<string | null>(null);
  const [endedFor, setEndedFor] = useState<string | null>(null);

  useEffect(() => {
    if (isLive) {
      lastLiveSid.current = sessionId;
      // A presence blip (a Redis hiccup, a dropped request) that briefly
      // empties the live set must not latch: seeing the session live again
      // un-does both the grace window and the ended flag.
      setGraceFor(null);
      setEndedFor(null);
      return;
    }
    // Only the live → not-live EDGE fires. Absence on its own is the ordinary
    // state of every historical drill-in, and refetching on it would hammer
    // the daemon on every replay.
    if (sessionId === null || lastLiveSid.current !== sessionId) return;
    lastLiveSid.current = null;
    setGraceFor(sessionId);
    // (#2725) `endedByPresence` is an AFFIRMATIVE claim — `SessionReplay`
    // stops its live clock on it, overriding the quiet-threshold heuristic
    // that otherwise governs. A session vanishing from a read that FAILED is
    // not evidence the run stopped; it is evidence we stopped being able to
    // look, and every session on the machine vanishes at once when it
    // happens. So the ended flag is withheld while coverage is degraded, and
    // the page falls back to the heuristic it already has. The grace window
    // above is NOT withheld: a bounded refetch of the session slice is the
    // right thing to do either way, and it is what recovers the terminal
    // record if the run really did end.
    if (coverageRef.current === null) setEndedFor(sessionId);
    // Fetch immediately as well as polling: in the common case the terminal
    // record is already written, and waiting out a poll interval to show it
    // is a visible pause on the one page whose job is watching this run end.
    void queryClient.refetchQueries({ queryKey: queryKeys.flowSession(sessionId), exact: true });
    const timer = setTimeout(() => setGraceFor((cur) => (cur === sessionId ? null : cur)), TERMINAL_GRACE_MS);
    return () => clearTimeout(timer);
  }, [isLive, sessionId, queryClient]);

  return {
    isLive,
    shouldPoll: isLive || (graceFor !== null && graceFor === sessionId),
    endedByPresence: endedFor !== null && endedFor === sessionId,
    coverage,
  };
}
