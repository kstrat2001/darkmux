import type { FlowRecord, PresenceBeat } from "../types/handwritten";
import { T, displayNameOf, machineUids, ownMachineName, type RosterName, type SelfIdentity } from "./flow";

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
 * - a machine whose name (`ownMachineName`: an observed `machine_id`, this
 *   daemon's specs name, then the roster id) no other machine holds is keyed
 *   by that bare name — what its fleet card is titled with;
 * - a machine whose name another machine also holds is `<name>~<hash>`, and
 *   a machine with no name is `unnamed-<hash>`, where `<hash>` is a short
 *   one-way hash of its uid (`machineKeyHash`, 6 hex by default);
 * - a roster-only card (declared, never seen) is keyed by its roster id,
 *   which is also the identity that card has always carried.
 *
 * Why a hash and not the "unnamed machine 2" ordinal the label shows: an
 * ordinal names a POSITION, and positions move — a machine gains a name, an
 * earlier unnamed one enters the window, a day rollover flips which twin was
 * seen first, another viewer's window numbers differently. A saved or shared
 * ordinal key would then open a DIFFERENT machine, which is worse than
 * opening none. A hash names the machine itself: `decodeMachineKey` matches
 * it against the uids in the page's context, so a stale key opens the same
 * machine (and is rewritten to its current key) or nothing. The hash is
 * short and one-way; it cannot be turned back into the uid.
 *
 * No two machines ever share a key: generated keys are assigned first, and a
 * name that equals one already taken (a machine literally named
 * `unnamed-3fa1c2`, or `studio~3fa1c2`), or the not-found marker, is
 * disambiguated with its own hash instead.
 *
 * Old links carried the uid itself. `decodeMachineKey` still resolves those
 * (a lenient read, case-insensitive, including a roster entry's declared uid
 * for a machine never seen) and reports `stale`, so the caller rewrites the
 * hash to the key.
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

/** What an unresolvable old uid link is rewritten to once the page's inputs
 *  have settled, so the uid does not stay in the address bar. Never assigned
 *  to a machine. */
export const MACHINE_NOT_FOUND_KEY = "not-found";

/** The label a pinned or drilled key shows when it names no machine. */
export const MACHINE_NOT_FOUND_LABEL = "machine not found";

const UNNAMED_PREFIX = "unnamed-";
const HASH_MIN = 6;

/** A short one-way hash of a uid, as 14 hex digits (cyrb53, over the
 *  lower-cased uid so a re-cased link names the same machine). Keys use a
 *  prefix of it. Not cryptographic: it only has to be distinct among the
 *  machines one page knows, and not be the uid. */
export function machineKeyHash(uid: string): string {
  const str = uid.toLowerCase();
  let h1 = 0xdeadbeef;
  let h2 = 0x41c6ce57;
  for (let i = 0; i < str.length; i++) {
    const ch = str.charCodeAt(i);
    h1 = Math.imul(h1 ^ ch, 2654435761);
    h2 = Math.imul(h2 ^ ch, 1597334677);
  }
  h1 = Math.imul(h1 ^ (h1 >>> 16), 2246822507) ^ Math.imul(h2 ^ (h2 >>> 13), 3266489909);
  h2 = Math.imul(h2 ^ (h2 >>> 16), 2246822507) ^ Math.imul(h1 ^ (h1 >>> 13), 3266489909);
  const n = 4294967296 * (2097151 & h2) + (h1 >>> 0);
  return n.toString(16).padStart(14, "0");
}

/** A generated key's hash part: `unnamed-<hex>` or `<anything>~<hex>`. */
const HASHED_KEY = /^(?:unnamed-|.*~)([0-9a-f]{6,14})$/;

interface KeyTable {
  keyOf: Map<string, string>;
  uidOf: Map<string, string>;
  /** Roster-only card ids (their card, key and label are the roster id). */
  rosterOnly: Set<string>;
  /** A roster entry's declared uid for a machine never seen -> its card id. */
  declaredUid: Map<string, string>;
}

const tableCache = new WeakMap<FlowRecord[], { ctx: MachineKeyContext; table: KeyTable }>();

function keyTable(ctx: MachineKeyContext): KeyTable {
  const hit = tableCache.get(ctx.data);
  if (hit && hit.ctx.liveMachines === ctx.liveMachines && hit.ctx.specs === ctx.specs && hit.ctx.roster === ctx.roster) {
    return hit.table;
  }
  const table = buildKeyTable(ctx);
  tableCache.set(ctx.data, { ctx, table });
  return table;
}

function buildKeyTable(ctx: MachineKeyContext): KeyTable {
  const { data, liveMachines, specs, roster } = ctx;
  // First-seen order (earliest record; presence-only and specs-only uids
  // after; ties by uid) — only used to make assignment deterministic.
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
  const seen = [...firstSeen.entries()]
    .sort(([ua, ta], [ub, tb]) => (ta !== tb ? (ta < tb ? -1 : 1) : ua < ub ? -1 : ua > ub ? 1 : 0))
    .map(([uid]) => uid);

  // Roster-only cards: an entry whose declared uid is not one of the seen
  // machines, and whose id is not already a seen machine's own name (the
  // fleet lens folds that one into the seen machine's card).
  const seenSet = new Set(seen);
  const names = new Map(seen.map((uid) => [uid, ownMachineName(data, liveMachines, specs, roster, uid)] as const));
  const seenNames = new Set([...names.values()].filter((n): n is string => n !== null));
  const rosterOnly: string[] = [];
  const declaredUid = new Map<string, string>();
  for (const entry of roster) {
    if (!entry.id) continue;
    if (entry.machine_uid && seenSet.has(entry.machine_uid)) continue;
    if (seenNames.has(entry.id) || rosterOnly.includes(entry.id)) continue;
    rosterOnly.push(entry.id);
    if (entry.machine_uid) declaredUid.set(entry.machine_uid.toLowerCase(), entry.id);
  }

  // Every identity a key can name, and its hash; the hash length is the
  // shortest (>= 6) that tells them all apart.
  const identities = [...seen, ...rosterOnly];
  const full = new Map(identities.map((id) => [id, machineKeyHash(id)] as const));
  let len = HASH_MIN;
  while (len < 14 && new Set([...full.values()].map((x) => x.slice(0, len))).size < full.size) len += 1;

  const keyOf = new Map<string, string>();
  const uidOf = new Map<string, string>();
  const taken = new Set<string>([MACHINE_NOT_FOUND_KEY]);
  const assign = (id: string, key: string) => {
    keyOf.set(id, key);
    uidOf.set(key, id);
    taken.add(key);
  };
  /** `<base><hash>`, lengthening the hash until the key is free. */
  const hashed = (id: string, base: string): string => {
    const hx = full.get(id) ?? machineKeyHash(id);
    for (let l = len; l <= 14; l++) {
      const k = `${base}${hx.slice(0, l)}`;
      if (!taken.has(k)) return k;
    }
    // Unreachable in practice (a full 53-bit collision on one page); still
    // never share a key.
    let i = 2;
    while (taken.has(`${base}${hx}.${i}`)) i += 1;
    return `${base}${hx}.${i}`;
  };

  const nameCount = new Map<string, number>();
  for (const n of names.values()) if (n !== null) nameCount.set(n, (nameCount.get(n) ?? 0) + 1);
  for (const id of rosterOnly) nameCount.set(id, (nameCount.get(id) ?? 0) + 1);

  // 1. Generated keys first (unnamed, and names held by several machines),
  //    so a machine whose NAME happens to spell one never takes it.
  for (const uid of seen) {
    const name = names.get(uid) ?? null;
    if (name === null) assign(uid, hashed(uid, UNNAMED_PREFIX));
    else if ((nameCount.get(name) ?? 0) > 1) assign(uid, hashed(uid, `${name}~`));
  }
  // 2. Unique names, bare when free; otherwise disambiguated like a shared
  //    one. Then roster-only ids, the same way.
  for (const uid of seen) {
    if (keyOf.has(uid)) continue;
    const name = names.get(uid) as string;
    assign(uid, taken.has(name) ? hashed(uid, `${name}~`) : name);
  }
  for (const id of rosterOnly) {
    assign(id, taken.has(id) || (nameCount.get(id) ?? 0) > 1 ? hashed(id, `${id}~`) : id);
  }
  return { keyOf, uidOf, rosterOnly: new Set(rosterOnly), declaredUid };
}

/** The key to put in the hash for the machine `uid`. Never the uid: a uid
 *  outside the context gets its name, or an `unnamed-<hash>` key. */
export function encodeMachineKey(ctx: MachineKeyContext, uid: string): string {
  const known = keyTable(ctx).keyOf.get(uid);
  if (known !== undefined) return known;
  const own = ownMachineName(ctx.data, ctx.liveMachines, ctx.specs, ctx.roster, uid);
  return own ?? `${UNNAMED_PREFIX}${machineKeyHash(uid).slice(0, HASH_MIN)}`;
}

export interface DecodedMachineKey {
  /** The machine (uid, or a roster-only card's id) the key names, or `null`
   *  when nothing in the context matches — the caller's not-found state. */
  uid: string | null;
  /** The key the hash SHOULD carry for that machine; `null` when unresolved. */
  key: string | null;
  /** The hash carried an old uid, or a key the machine has since outgrown
   *  (it gained a name, its twin left); rewrite it to `key`. */
  stale: boolean;
}

const NOT_FOUND: DecodedMachineKey = { uid: null, key: null, stale: false };

/** Resolve a hash key (or an old link's uid) to the machine it names. */
export function decodeMachineKey(ctx: MachineKeyContext, key: string): DecodedMachineKey {
  const { keyOf, uidOf, declaredUid } = keyTable(ctx);
  const current = uidOf.get(key);
  if (current !== undefined) return { uid: current, key, stale: false };
  if (key === MACHINE_NOT_FOUND_KEY) return NOT_FOUND;
  // An old link: the hash carried the uid (any case). A roster entry's
  // declared uid names its card even before that machine is ever seen.
  const lower = key.toLowerCase();
  for (const [id, k] of keyOf) {
    if (id.toLowerCase() === lower) return { uid: id, key: k, stale: true };
  }
  const declared = declaredUid.get(lower);
  if (declared !== undefined) return { uid: declared, key: keyOf.get(declared) ?? declared, stale: true };
  // A generated key minted over another window: match the machine by its
  // hash, never by position. Exactly one match, or not found.
  const m = HASHED_KEY.exec(key);
  if (m) {
    const hx = m[1];
    const hits = [...keyOf.keys()].filter((id) => machineKeyHash(id).startsWith(hx));
    if (hits.length === 1) return { uid: hits[0], key: keyOf.get(hits[0]) as string, stale: true };
  }
  return NOT_FOUND;
}

/** The label for a machine a key resolved to: a roster-only card's roster
 *  id, else the shared label (`displayNameOf`). */
export function machineLabel(ctx: MachineKeyContext, uid: string): string {
  if (keyTable(ctx).rosterOnly.has(uid)) return uid;
  return displayNameOf(ctx.data, ctx.liveMachines, ctx.specs, uid, ctx.roster);
}
