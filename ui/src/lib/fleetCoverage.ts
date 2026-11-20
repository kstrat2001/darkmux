/**
 * (#2725) The ONE predicate and the ONE caveat this app uses for "the fleet
 * presence substrate could not be read".
 *
 * (#3662) There is no warning box for it any more: each surface that derives
 * a claim from presence says what it knows. The masthead count carries the
 * caveat below; the fleet cards and the machine page read a peer as "not
 * streaming" / "not reported" (`useDegradedFleetSource`).
 *
 * Several readers ask the question — the masthead count, the fleet cards and
 * the machine page over `/fleet/machines/live`, and `hooks/useLiveSessionIds`
 * over `/fleet/dispatches/live` — and those endpoints sit on the SAME Redis
 * substrate and fail in the same state (`fleet_sessions_live_handler` and
 * `fleet_machines_live_handler` both emit `source_state::coverage_meta`).
 * A hook importing a component to borrow its predicate would have been the
 * wrong dependency direction; a second copy of the predicate would have been
 * a second vocabulary for one operator-visible condition, which is exactly
 * what #2683 spent its effort removing.
 *
 * Pure and React-free on purpose: the hooks own the queries, this module owns
 * the reading of what they return.
 */
import type { CoverageMeta } from "../types/generated/CoverageMeta";
import type { SourceState } from "../types/generated/SourceState";

/** The fleet source state when it is degraded, `null` otherwise
 * (`ok`/`off`/nothing tracked/no answer yet). One predicate, so the masthead's
 * count, the cards, the machine page and the session-liveness hook can never
 * disagree about whether coverage is degraded. */
export type DegradedFleetSource = Extract<SourceState, { state: "stale" } | { state: "unavailable" }>;

/**
 * Read a presence response's coverage. `meta` is the daemon's own report;
 * `unreadable` is whether the READ that should have produced it even got an
 * answer.
 *
 * Two different failures collapse onto the same `unavailable` state here,
 * deliberately, because they are the same fact to a reader: the daemon
 * reporting that IT could not read the fleet substrate, and this page not
 * being able to read the DAEMON (#2683 — a mid-session daemon death, where
 * the hooks hand every consumer an empty map/set that is indistinguishable
 * from a genuinely empty fleet). Both mean "presence could not be read",
 * which is what `presenceCaveat` says; splitting them would be a second
 * vocabulary for one operator-visible condition.
 *
 * `off` and `ok` say nothing, deliberately: a standalone machine has no fleet
 * substrate by design, and warning it would be the bug.
 */
export function degradedFleetSource(meta: CoverageMeta | null, unreadable: boolean): DegradedFleetSource | null {
  if (unreadable) return { state: "unavailable", detail: "the presence read failed" };
  const fleet = meta?.sources?.fleet;
  if (!fleet || fleet.state === "ok" || fleet.state === "off") return null;
  return fleet as DegradedFleetSource;
}

/** (#3662) What the masthead count says about degraded presence, in a few
 * words: the count still shows (the last answer beats no answer), and this
 * says why it is not asserted bare. An unreadable presence leaves only this
 * machine in the count (`metaLine.ts::machineCount` always counts self). */
export function presenceCaveat(fleet: DegradedFleetSource): string {
  if (fleet.state === "unavailable") return "this machine only: fleet presence unreadable";
  return `fleet presence ${Math.round(fleet.age_ms / 1000)}s old`;
}
