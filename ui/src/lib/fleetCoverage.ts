/**
 * (#2725, extracted from `components/FleetCoverageNotice.tsx`) The ONE
 * predicate and the ONE sentence this app uses for "the fleet presence
 * substrate could not be read".
 *
 * It lived inside the notice component, which was fine while the notice was
 * the only thing that asked the question. Two readers now do — the notice
 * (and the masthead marker that shares its hook) over
 * `/fleet/machines/live`, and `hooks/useLiveSessionIds` over
 * `/fleet/sessions/live` — and those endpoints sit on the SAME Redis
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
import type { CoverageMeta, SourceState } from "../types/handwritten";

/** The fleet source state when it is worth warning about, `null` otherwise
 * (`ok`/`off`/nothing tracked/no answer yet). One predicate, so the masthead's
 * headline marker, the notice, and the session-liveness hook can never
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
 * which is what `fleetCoverageMessage` says; splitting them would be a second
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

/** The one sentence this app says about degraded presence, in its two
 * shapes. Verbatim from #1729 — the headline's marker reuses it as its
 * `title` rather than paraphrasing. */
export function fleetCoverageMessage(fleet: DegradedFleetSource): string {
  return fleet.state === "stale"
    ? `Fleet presence is stale${"age_ms" in fleet ? ` (${Math.round(fleet.age_ms / 1000)}s old)` : ""} — machines and run counts below may have moved on.`
    : "Fleet presence could not be read — machines and run counts below cover THIS MACHINE only, and are not the whole fleet.";
}
