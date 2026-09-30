import { ACTION, type NormRecord } from "../lib/ingest";
import { norm } from "./records";
import type { PresenceBeat } from "../types/generated/PresenceBeat";
import type { Run } from "../types/generated/Run";
import type { RosterName, SelfIdentity } from "../lib/machineIdentity";

/**
 * The realistic three-machine fleet every machine-identity test shares.
 *
 * - `MacBook-Pro` is this machine. It has logged under two spellings
 *   (`MacBook-Pro` earlier, `MacBook-Pro.local` later), so its alias set has
 *   two members.
 * - `m1-max-32gb-studio` is a peer on the hub.
 * - `darkbook` is a declared peer. `streamsHere: false` makes it roster-only:
 *   it is in the operator's roster and nothing it writes reaches this window.
 * - Two more machines both named `Mac` (default hostnames) exist only when
 *   `twinMacs` is set.
 *
 * Flow uids are UPPERCASE, as in a real record; the roster and the fleet view
 * spell the same uids in lower case.
 */
export const FLEET_UID = {
  mbp: "00000000-0000-4000-8000-ABCDEF000021",
  studio: "00000000-0000-4000-8000-ABCDEF000022",
  darkbook: "00000000-0000-4000-8000-ABCDEF000023",
  macA: "00000000-0000-4000-8000-ABCDEF000024",
  macB: "00000000-0000-4000-8000-ABCDEF000025",
} as const;

export const lower = (uid: string): string => uid.toLowerCase();

export interface MachineFleet {
  data: NormRecord[];
  liveMachines: Map<string, PresenceBeat>;
  roster: RosterName[];
  /** This machine's own `/machine/specs` identity, uid spelled in lower case. */
  specs: SelfIdentity;
}

export interface FleetOptions {
  /** Whether darkbook's records reach this window. Default true. */
  streamsHere?: boolean;
  /** Add the two machines that share the display name `Mac`. */
  twinMacs?: boolean;
}

const T0 = Date.parse("2026-10-01T10:00:00Z");

function rec(uid: string, name: string, minute: number, session: string): NormRecord {
  return norm({
    ts: new Date(T0 + minute * 60_000).toISOString(),
    machine_uid: uid,
    machine_id: name,
    session_id: session,
    action: ACTION.DispatchStart,
  });
}

export function machineFleet(opts: FleetOptions = {}): MachineFleet {
  const { streamsHere = true, twinMacs = false } = opts;
  const data: NormRecord[] = [
    rec(FLEET_UID.mbp, "MacBook-Pro", 0, "mbp-1"),
    rec(FLEET_UID.mbp, "MacBook-Pro.local", 5, "mbp-2"),
    rec(FLEET_UID.studio, "m1-max-32gb-studio", 1, "studio-1"),
  ];
  if (streamsHere) data.push(rec(FLEET_UID.darkbook, "darkbook", 2, "darkbook-1"));
  if (twinMacs) {
    data.push(rec(FLEET_UID.macA, "Mac", 3, "mac-a-1"), rec(FLEET_UID.macB, "Mac", 4, "mac-b-1"));
  }
  const liveMachines = new Map<string, PresenceBeat>([
    [FLEET_UID.studio, { machine_uid: FLEET_UID.studio, display_name: "m1-max-32gb-studio", schema_version: "1.0.0", beat_ts_ms: T0 } as PresenceBeat],
  ]);
  const roster: RosterName[] = [
    { id: "macbook-pro", machine_uid: lower(FLEET_UID.mbp) },
    { id: "studio", machine_uid: lower(FLEET_UID.studio) },
    { id: "darkbook", machine_uid: lower(FLEET_UID.darkbook) },
  ];
  return { data, liveMachines, roster, specs: { machine_id: "MacBook-Pro", machine_uid: lower(FLEET_UID.mbp) } };
}

/** One `/runs` row. `machine_uid` is the uid spelled as the server stamps it. */
export function fleetRun(id: string, machine: string | undefined, machine_uid?: string): Run {
  return { id, kind: "dispatch", status: "complete", tracked: false, machine, machine_uid } as Run;
}
