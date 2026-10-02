/**
 * Test-only: a fleet card built in one call (base, then live readings), and
 * the face a fully-answered page would draw for it. The lens builds cards in
 * two stages (`buildFleetCardBase` per data change, `withLiveReadings` per
 * render); a test that does not care about the split uses this.
 */
import { buildFleetCardBase, cardFace, withLiveReadings, type CardFace, type FleetCard } from "../lenses/fleet/cards";
import type { RowFacts } from "../lenses/fleet/viewRows";
import type { NormRecord } from "../lib/ingest";
import { DEFAULT_POLICY, type LifecyclePolicy, type Presence } from "../lib/lifecycle";
import type { LiveOverlay } from "../lib/liveChannel";
import type { RosterName } from "../lib/machineIdentity";
import type { PresenceBeat } from "../types/generated/PresenceBeat";
import type { MachineSpecsResponse } from "../types/generated/MachineSpecsResponse";
import type { Run } from "../types/generated/Run";

export function buildFleetCard(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: MachineSpecsResponse | null,
  presence: Presence,
  machAbsent: boolean,
  m: string,
  t: number,
  row: RowFacts | null = null,
  machineRuns: Run[] = [],
  connected = true,
  lastContactMs: number | null = null,
  roster: readonly RosterName[] = [],
  live: LiveOverlay | null = null,
  policy: LifecyclePolicy = DEFAULT_POLICY,
): FleetCard {
  return withLiveReadings(buildFleetCardBase(data, liveMachines, specs, presence, machAbsent, m, t, row, machineRuns, roster, policy), t, connected, lastContactMs, live);
}

/** The face of a card once every source has answered. */
export function faceOf(card: FleetCard, hasReading = false): CardFace {
  return cardFace(card, hasReading, { flow: true, presence: true, sessions: true, runs: true });
}
