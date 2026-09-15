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
 */
import { useFleetCoverage } from "../hooks/useLiveMachines";
import { getSource } from "../lib/source";
import type { SourceState } from "../types/handwritten";

/** The fleet source state when it is worth warning about, `null` otherwise
 * (`ok`/`off`/nothing tracked/no answer yet). One predicate, so the masthead's
 * headline marker and the notice below can never disagree about whether
 * coverage is degraded. */
export type DegradedFleetSource = Extract<SourceState, { state: "stale" } | { state: "unavailable" }>;

/** `enabled` is the caller's live-vs-replay gate — see `useFleetCoverage`.
 * This hook SHARES `queryKeys.fleetMachinesLive` with `useLiveMachines`, so
 * it costs no extra request, and equally so a stray enabled observer would
 * re-open a poll a replay just gated off.
 *
 * Two different failures collapse onto the same `unavailable` state here,
 * deliberately, because they are the same fact to a reader: the daemon
 * reporting that IT could not read the fleet substrate, and this page not
 * being able to read the DAEMON (#2683 — a mid-session daemon death, where
 * `useLiveMachines` hands every consumer an empty map that is indistinguishable
 * from a genuinely empty fleet). Both mean "presence could not be read", which
 * is what the sentence says; splitting them would be a second vocabulary for
 * one operator-visible condition. */
export function useDegradedFleetSource(enabled: boolean): DegradedFleetSource | null {
  const { meta, unreadable } = useFleetCoverage(enabled && getSource().kind === "daemon");
  if (unreadable) return { state: "unavailable", detail: "the presence read failed" };
  const fleet = meta?.sources?.fleet;
  if (!fleet || fleet.state === "ok" || fleet.state === "off") return null;
  return fleet as DegradedFleetSource;
}

/** The one sentence this app says about degraded presence, in its two
 * shapes. Verbatim from #1729 — the headline's marker reuses it as its
 * `title` rather than paraphrasing. */
export function fleetCoverageMessage(fleet: DegradedFleetSource): string {
  return fleet.state === "stale"
    ? `Fleet presence is stale${"age_ms" in fleet ? ` (${Math.round(fleet.age_ms / 1000)}s old)` : ""} — machines and run counts below may have moved on.`
    : "Fleet presence could not be read — machines and run counts below cover THIS MACHINE only, and are not the whole fleet.";
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
