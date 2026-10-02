import type { FlowReadFailure } from "../hooks/useFlowWindow";

/**
 * (#2965) A `/flow/<day>` read failed. The flow window settles with no
 * records from that day, which reads exactly like a quiet day: every card
 * would say "idle · 0 running" off a read that never happened. The cards and
 * the machine page hold "checking…" for the claims that read backs (see
 * `cardFace`'s doc, #2958); this says why.
 *
 * Mounted once, from `App.tsx`'s notice row beside `FleetCoverageNotice`,
 * because the window is app-wide: the masthead, the fleet cards and the
 * machine page all read it. Same `.fleetcov` shape and `role="status"` as its
 * siblings, and worded to share no phrase with any of them ("could not be
 * read", "unavailable", "unreadable"), so a reader, or a test's `getByText`,
 * can tell which source failed when more than one fires at once.
 */
export function FlowReadNotice({ failure }: { failure: FlowReadFailure | null }) {
  if (failure === null) return null;
  // Name the day that failed: when only yesterday's did, today's records are
  // all there, and "recent activity" would be wrong.
  const which =
    failure.today && failure.yesterday
      ? "Flow records for today and yesterday"
      : failure.today
        ? "Today's flow records"
        : "Yesterday's flow records";
  return (
    <div className="fleetcov" data-state="flow-unreadable" role="status">
      <span className="fleetcov__icon">⚠</span>
      <span>
        {which} failed to load ({failure.message}), so machines say checking… rather than idle.
      </span>
    </div>
  );
}
