import type { FlowRecord, PresenceBeat } from "../types/handwritten";
import {
  T,
  UNNAMED_MACHINE,
  displayNameOf,
  machineUids,
  ownMachineName,
  type RosterName,
  type SelfIdentity,
} from "./flow";

/**
 * (#2929) The machine identity the URL hash carries, and the ONE place that
 * translates it to and from a hardware uid.
 *
 * A `machine_uid` is the machine's hardware UUID. It identifies the physical
 * machine, so it must not ride in the address bar, where a screenshot or a
 * shared link carries it off the page (#2921 already keeps it out of every
 * rendered label; the URL was the remaining exit). The hash carries a KEY
 * instead:
 *
 * - the machine's own name (`ownMachineName`: an observed `machine_id`, this
 *   daemon's specs name, then the roster id) when it has one — the same name
 *   its fleet card is titled with;
 * - `<name>~<n>` for the n-th machine (first-seen order) sharing a name
 *   another machine already holds, so two machines never share a key;
 * - `unnamed-<n>` for a machine with no name, where `<n>` is the ordinal its
 *   label already shows ("unnamed machine 2" is `unnamed-2`; the first,
 *   labeled plain "unnamed machine", is `unnamed-1`);
 * - a roster-only card (declared, never seen) is keyed by its roster id,
 *   which is also the identity that card has always carried.
 *
 * None of those says anything about the hardware. The page resolves a key
 * back to the uid from its own window, presence beats, specs and roster
 * (`decodeMachineKey`), so everything downstream of the URL still keys on
 * the uid.
 *
 * Old links carried the uid itself. `decodeMachineKey` still resolves those
 * (a lenient read) and reports `legacy`, so the caller can rewrite the hash
 * to the key.
 *
 * Known limit, stated rather than hidden: an ordinal key is only as stable as
 * the window it was minted over. A machine that gains a name, or an unnamed
 * machine seen EARLIER in a later window, can shift which machine
 * `unnamed-2` names. A named key does not have this problem, which is why
 * the name always wins when there is one.
 */
export interface MachineKeyContext {
  data: FlowRecord[];
  liveMachines: Map<string, PresenceBeat>;
  specs: SelfIdentity | null;
  roster: readonly RosterName[];
}

/** A hardware uid's shape (a UUID), matched case-insensitively. Used to tell
 *  an old uid-carrying link from a key, and by tests to assert no hash ever
 *  carries one. */
export const UID_SHAPED = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i;

const UNNAMED_KEY_PREFIX = "unnamed-";

/** Every uid the page could draw a card for, mapped to its key, plus the
 *  reverse. Cached per context (the same identity-checked shape `flow.ts`'s
 *  label ordering uses), so a fleet page asking once per card pays once. */
interface KeyTable {
  keyOf: Map<string, string>;
  uidOf: Map<string, string>;
}

const tableCache = new WeakMap<FlowRecord[], { ctx: MachineKeyContext; table: KeyTable }>();

function keyTable(ctx: MachineKeyContext): KeyTable {
  const hit = tableCache.get(ctx.data);
  if (
    hit &&
    hit.ctx.liveMachines === ctx.liveMachines &&
    hit.ctx.specs === ctx.specs &&
    hit.ctx.roster === ctx.roster
  ) {
    return hit.table;
  }
  const table = buildKeyTable(ctx);
  tableCache.set(ctx.data, { ctx, table });
  return table;
}

/** The label-derived key: `unnamed-<n>` for an unnamed label, the name
 *  otherwise. Never the uid, because `displayNameOf` never answers with one. */
function keyFromLabel(label: string): string {
  if (label === UNNAMED_MACHINE) return `${UNNAMED_KEY_PREFIX}1`;
  const m = /^unnamed machine (\d+)$/.exec(label);
  return m ? `${UNNAMED_KEY_PREFIX}${m[1]}` : label;
}

function buildKeyTable(ctx: MachineKeyContext): KeyTable {
  const { data, liveMachines, specs, roster } = ctx;
  // First-seen order — the same ordering `flow.ts` numbers unnamed machines
  // by (earliest record; presence-only and specs-only uids after; ties by
  // uid), so a duplicate name's `~n` suffix is as stable as the ordinal is.
  const firstSeen = new Map<string, number>();
  for (const r of data) {
    const uid = r.machine_uid;
    if (!uid) continue;
    const t = T(r.ts);
    const at = Number.isFinite(t) ? t : Infinity;
    const prev = firstSeen.get(uid);
    if (prev === undefined || at < prev) firstSeen.set(uid, at);
  }
  for (const uid of machineUids(data, liveMachines)) if (!firstSeen.has(uid)) firstSeen.set(uid, Infinity);
  if (specs?.machine_uid && !firstSeen.has(specs.machine_uid)) firstSeen.set(specs.machine_uid, Infinity);
  const ordered = [...firstSeen.entries()]
    .sort(([ua, ta], [ub, tb]) => (ta !== tb ? (ta < tb ? -1 : 1) : ua < ub ? -1 : ua > ub ? 1 : 0))
    .map(([uid]) => uid);

  const keyOf = new Map<string, string>();
  const uidOf = new Map<string, string>();
  const nameCount = new Map<string, number>();
  for (const uid of ordered) {
    const name = ownMachineName(data, liveMachines, specs, roster, uid);
    let key: string;
    if (name === null) {
      key = keyFromLabel(displayNameOf(data, liveMachines, specs, uid, roster));
    } else {
      const n = (nameCount.get(name) ?? 0) + 1;
      nameCount.set(name, n);
      key = n === 1 ? name : `${name}~${n}`;
    }
    keyOf.set(uid, key);
    if (!uidOf.has(key)) uidOf.set(key, uid);
  }
  // Roster-only cards (declared, never seen) carry their roster id as their
  // identity (`FleetLens`'s `rosterOnly` cards), so the id is both the key
  // and what the key resolves to. An entry whose declared uid is already
  // known is that machine, keyed above.
  for (const entry of roster) {
    if (entry.machine_uid && keyOf.has(entry.machine_uid)) continue;
    if (!entry.id || uidOf.has(entry.id) || keyOf.has(entry.id)) continue;
    keyOf.set(entry.id, entry.id);
    uidOf.set(entry.id, entry.id);
  }
  return { keyOf, uidOf };
}

/** The key to put in the hash for the machine `uid`. Never the uid: a uid
 *  outside the context falls back to the key its label implies. */
export function encodeMachineKey(ctx: MachineKeyContext, uid: string): string {
  return keyTable(ctx).keyOf.get(uid) ?? keyFromLabel(displayNameOf(ctx.data, ctx.liveMachines, ctx.specs, uid, ctx.roster));
}

export interface DecodedMachineKey {
  /** The uid the key names, or `null` when nothing in the context matches
   *  (the caller shows its not-found state). */
  uid: string | null;
  /** The key the hash SHOULD carry for that machine; `null` when unresolved. */
  key: string | null;
  /** The hash carried the uid itself (an old link); rewrite it to `key`. */
  legacy: boolean;
}

/** Resolve a hash key (or an old link's uid) to the machine's uid. */
export function decodeMachineKey(ctx: MachineKeyContext, key: string): DecodedMachineKey {
  const { keyOf, uidOf } = keyTable(ctx);
  const byKey = uidOf.get(key);
  if (byKey !== undefined) return { uid: byKey, key, legacy: false };
  // An old link: the hash carried the uid. Compared case-insensitively for a
  // uid-shaped value, since a hand-typed or re-cased link names the same
  // hardware.
  const exact = keyOf.get(key);
  if (exact !== undefined) return { uid: key, key: exact, legacy: true };
  if (UID_SHAPED.test(key)) {
    const lower = key.toLowerCase();
    for (const [uid, k] of keyOf) {
      if (uid.toLowerCase() === lower) return { uid, key: k, legacy: true };
    }
  }
  return { uid: null, key: null, legacy: false };
}
