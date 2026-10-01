/**
 * (#2925) The Machine dimension's two vocabularies and the bridge between
 * them. A filter VALUE is a machine identity (`format.ts::machineKeyOfRuns`:
 * `uid:<canonical>` or `name:<spelling>`), so one machine under several names
 * is one choice. The hash carries machine KEYS (`lib/machineKey.ts`), never a
 * uid. This module translates between the two and names each choice.
 */

import type { Run } from "../../types/generated/Run";
import { canonUid, machineMatch, machineUids, nameKey } from "../../lib/machineIdentity";
import { MACHINE_NOT_FOUND_KEY, MACHINE_NOT_FOUND_LABEL, UID_SHAPED, decodeMachineKey, encodeMachineKey, machineLabel, type MachineKeyContext } from "../../lib/machineKey";
import { machineKeyOfRuns, runMachineLabels } from "./format";
import { NOT_SET } from "./runFilters";

/** The value a hash key that names no machine selects: it matches no run, and
 * reads as "machine not found". */
const MACHINE_UNRESOLVED = "(machine not found)";

/** What the runs in view say about each machine identity. */
export interface MachineIndex {
  /** A run's machine identity (`null` when it names none). */
  keyOf: (r: Run) => string | null;
  /** The uid, as the runs spell it, of each `uid:` value. */
  rawUid: Map<string, string>;
  /** The name, as the runs spell it, of each `name:` value. */
  rawName: Map<string, string>;
  /** The display name of each value: the name on the machine's most recently
   * active run, told apart by an ordinal from a machine that reads alike. */
  names: Map<string, string>;
}

/** Each name only one known machine answers to, to that machine's identity:
 * a run filed under another spelling of a machine's name (no uid on it) is
 * still that machine's run. */
function aliasIdentities(ctx: MachineKeyContext): Map<string, string> {
  const out = new Map<string, string>();
  for (const uid of machineUids(ctx.data, ctx.liveMachines)) {
    for (const n of machineMatch(ctx.data, ctx.liveMachines, ctx.specs, ctx.roster, uid).names) out.set(n, `uid:${canonUid(uid)}`);
  }
  return out;
}

export function machineIndex(runs: Run[], ctx: MachineKeyContext, selectedKeys: readonly string[] = []): MachineIndex {
  const own = machineKeyOfRuns(runs);
  const alias = aliasIdentities(ctx);
  const keyOf = (r: Run): string | null => {
    const k = own(r);
    return k !== null && k.startsWith("name:") ? (alias.get(k.slice(5)) ?? k) : k;
  };
  const labels = runMachineLabels(runs);
  const rawUid = new Map<string, string>();
  const rawName = new Map<string, string>();
  const names = new Map<string, string>();
  for (const r of runs) {
    const key = keyOf(r);
    if (key === null) continue;
    if (r.machine_uid && !rawUid.has(key)) rawUid.set(key, r.machine_uid);
    if (r.machine && !rawName.has(key)) rawName.set(key, r.machine);
    const label = labels.get(r.id);
    if (label && !names.has(key)) names.set(key, label);
  }
  // A selected machine no run names (a roster card nothing has run on) is
  // still a machine the chip must name.
  const index: MachineIndex = { keyOf, rawUid, rawName, names };
  for (const key of selectedKeys) {
    const { uid } = decodeMachineKey(ctx, key);
    if (uid !== null) rawUid.set(valueOfMachineKey(ctx, index, key), uid);
  }
  return index;
}

/** The hash key for a filter value: the machine's key from the shared encoder,
 * or the spelling of a machine known only by name. */
export function keyOfMachineValue(ctx: MachineKeyContext, index: MachineIndex, value: string): string {
  const uid = index.rawUid.get(value);
  if (uid) return encodeMachineKey(ctx, uid);
  return index.rawName.get(value) ?? value;
}

/** The filter value a hash key selects: the machine it resolves to, else a
 * machine known only by that name, else `MACHINE_UNRESOLVED`. */
export function valueOfMachineKey(ctx: MachineKeyContext, index: MachineIndex, key: string): string {
  if (key === NOT_SET) return NOT_SET;
  const { uid } = decodeMachineKey(ctx, key);
  if (uid !== null) return `uid:${canonUid(machineMatch(ctx.data, ctx.liveMachines, ctx.specs, ctx.roster, uid).uid ?? uid)}`;
  const byName = `name:${nameKey(key)}`;
  return index.rawName.has(byName) ? byName : MACHINE_UNRESOLVED;
}

/** What to show for a machine value: its display name on the runs, else the
 * shared fleet label for its uid, else the value itself. */
export function machineValueLabel(ctx: MachineKeyContext, index: MachineIndex, value: string): string {
  if (value === MACHINE_UNRESOLVED) return MACHINE_NOT_FOUND_LABEL;
  const named = index.names.get(value) ?? index.rawName.get(value);
  if (named) return named;
  const uid = index.rawUid.get(value);
  return uid ? machineLabel(ctx, uid) : value;
}

/** The keys the hash SHOULD carry once the machine context has settled: an old
 * uid link or an outgrown key becomes the machine's current key, and a
 * uid-shaped value that still names nothing becomes the not-found marker so the
 * uid does not sit in the address bar. */
export function canonicalMachineKeys(ctx: MachineKeyContext, keys: string[]): string[] {
  return keys.map((key) => {
    const d = decodeMachineKey(ctx, key);
    if (d.stale && d.key) return d.key;
    return d.uid === null && UID_SHAPED.test(key) ? MACHINE_NOT_FOUND_KEY : key;
  });
}
