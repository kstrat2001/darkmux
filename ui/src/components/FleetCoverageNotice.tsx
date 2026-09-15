/**
 * (#1729, moved out of `lenses/fleet/FleetLens.tsx` by #2683) Presence
 * coverage — whether the fleet substrate could actually be READ, as opposed
 * to being genuinely quiet.
 *
 * Every machine card, every "N running" count, and the masthead's own
 * `N ⬚ · last dispatch …` headline are derived from presence. When the fleet
 * substrate cannot be read, those surfaces do not go blank — they render
 * CONFIDENTLY WRONG: machines read idle, running work reads zero, timeline
 * bars lose their run colouring, and the headline states a machine count as
 * current. That is the dead-looking-seats bug (#1483) with a nicer layout.
 *
 * `off` and `ok` say nothing, deliberately: a standalone machine has no fleet
 * substrate by design, and warning it would be the bug.
 *
 * **Why this lives in `components/` and mounts from `App.tsx` (#2683).** It
 * was a `FleetLens` local, so the fleet view was caveated and the masthead —
 * which is GLOBAL, shows on every lens, and makes the same presence-derived
 * claim — was not. Every consumer of `useLiveMachines` (`App`'s headline,
 * `MachineLens`, `RunsBoard`, this lens) now sits underneath one notice
 * instead of each needing its own, which is the whole point of a shared
 * indicator: the alternative is four surfaces and four chances to forget one.
 * Mounting it in BOTH places would have shown the same banner twice on the
 * fleet route, so the lens-local copy is gone rather than duplicated.
 *
 * The wording is unchanged, byte for byte, from the #1729 original — a second
 * staleness vocabulary for the same signal would be the failure mode, not an
 * improvement.
 *
 * It also guards a regression that already happened once: this marker briefly
 * existed on `FleetStrip` and vanished when `FleetLens` replaced it on the
 * route, with nothing going red because FleetStrip's own tests kept passing
 * while it stopped being mounted. `FleetCoverage.test.tsx` covers the
 * component; `App.test.tsx` covers that it is still MOUNTED.
 *
 * (#2725) `FleetStrip` itself is now DELETED. It had carried a THIRD copy of
 * the sentences below — diverged from both of the two this component merged —
 * in a file nothing rendered, which is how a wording variant gets copied
 * forward. Deleting it removes the copy AND the passing-but-unmounted test
 * suite that made the original regression invisible.
 */
import { useFleetCoverage } from "../hooks/useLiveMachines";
import { getSource } from "../lib/source";
import { degradedFleetSource, fleetCoverageMessage, type DegradedFleetSource } from "../lib/fleetCoverage";

/** (#2725) The predicate, the type and the sentence moved to
 * `lib/fleetCoverage.ts` when `useLiveSessionIds` became a second reader of
 * the same signal — see that module's own doc. Re-exported here because this
 * component is where the app's readers already look for them. */
export { fleetCoverageMessage, type DegradedFleetSource };

/** `enabled` is the caller's live-vs-replay gate — see `useFleetCoverage`.
 * This hook SHARES `queryKeys.fleetMachinesLive` with `useLiveMachines`, so
 * it costs no extra request, and equally so a stray enabled observer would
 * re-open a poll a replay just gated off. */
export function useDegradedFleetSource(enabled: boolean): DegradedFleetSource | null {
  const { meta, unreadable } = useFleetCoverage(enabled && getSource().kind === "daemon");
  return degradedFleetSource(meta, unreadable);
}

/** `historical` — a replay has no live coverage to report (see
 * `useFleetCoverage`'s note); it is the caller's `!isLiveRoute(route)`. */
export function FleetCoverageNotice({ historical = false }: { historical?: boolean }) {
  const fleet = useDegradedFleetSource(!historical);
  if (!fleet) return null;
  return (
    <div className="fleetcov" data-state={fleet.state} role="status">
      <span className="fleetcov__icon">⚠</span>
      <span>{fleetCoverageMessage(fleet)}</span>
    </div>
  );
}
