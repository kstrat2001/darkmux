import type { PresenceBeat } from "../types/generated/PresenceBeat";
import { recordsAsOf, type NormRecord } from "./ingest";

/**
 * THE machine identity module. Every viewer surface that decides WHICH
 * machine something belongs to decides it here, by uid.
 *
 * - The hardware uid is the identity. It is compared case-normalized
 *   (`sameUid`): flow records carry it UPPERCASE while a roster entry, the
 *   fleet view and `/machine/specs` may spell it differently, and a case
 *   difference must never split one machine into two.
 * - A name is for DISPLAY. One machine carries several over its life
 *   (`machine_id` defaults to the hostname, macOS reports it with and without
 *   `.local`, a roster id is a third spelling), and two machines can share
 *   one (default hostnames). Names therefore form an ALIAS SET per uid
 *   (`machineNames`), used for search and as the fallback match for a record
 *   that carries no uid at all (`matchesMachine`), and never as an identity:
 *   a name two machines share attributes to neither.
 * - Functions here return a uid in the FORM the caller's window already
 *   holds (`findUid`), so a value that came from the roster or the view can
 *   key the same maps the flow data keys.
 */

/** A uid in its canonical (comparison) form. */
export const canonUid = (uid: string): string => uid.toLowerCase();

/** Whether two uids name the same machine. A missing uid names no machine. */
export function sameUid(a: string | null | undefined, b: string | null | undefined): boolean {
  return !!a && !!b && canonUid(a) === canonUid(b);
}

/** The member of `uids` that names the same machine as `uid`, in the form
 *  `uids` spells it; `null` when none does. */
export function findUid(uids: Iterable<string>, uid: string | null | undefined): string | null {
  if (!uid) return null;
  for (const u of uids) if (sameUid(u, uid)) return u;
  return null;
}

/** A machine NAME reduced to what identifies it as a spelling: case-folded,
 *  without the mDNS `.local` suffix. Two names with the same key are the same
 *  alias. */
export function nameKey(name: string): string {
  return name.trim().toLowerCase().replace(/\.local$/, "");
}

/** What a record, a run or a link says about which machine it is: a uid when
 *  it carries one, else only a name. */
export interface MachineRef {
  uid?: string | null;
  name?: string | null;
}

/** Whether two references name the same machine: by uid when both carry one
 *  (a uid that differs is a different machine whatever the names say), else
 *  by name spelling. */
export function sameMachine(a: MachineRef, b: MachineRef): boolean {
  if (a.uid && b.uid) return sameUid(a.uid, b.uid);
  return !!a.name && !!b.name && nameKey(a.name) === nameKey(b.name);
}

/** A key that is equal for two references exactly when `sameMachine` says
 *  they agree: the canonical uid when there is one, else the name spelling.
 *  `null` for a reference that names nothing. */
export function machineRefKey(ref: MachineRef): string | null {
  if (ref.uid) return `uid:${canonUid(ref.uid)}`;
  return ref.name ? `name:${nameKey(ref.name)}` : null;
}

/** What one machine answers to: its uid when known, and the name spellings
 *  no OTHER machine also answers to. */
export interface MachineMatch {
  uid: string | null;
  /** `nameKey`s. A name shared with another machine is left out. */
  names: ReadonlySet<string>;
}

/** Whether `ref` belongs to `machine`: by uid when both have one, else by a
 *  name only this machine answers to. */
export function matchesMachine(ref: MachineRef, machine: MachineMatch): boolean {
  if (ref.uid && machine.uid) return sameUid(ref.uid, machine.uid);
  return !!ref.name && machine.names.has(nameKey(ref.name));
}

/** The records of the machine `uid` names. A machine nothing is known about
 *  (`null`) has none: a per-machine surface shows nothing, never everyone's. */
function recordsOfMachine(records: readonly NormRecord[], uid: string | null): NormRecord[] {
  return uid === null ? [] : records.filter((r) => sameUid(r.machine_uid, uid));
}

/** A machine page's records: the drilled machine's (`drilledKey` names one,
 *  `drilledUid` is what it resolved to, `null` while unresolved), else this
 *  machine's (`selfUid`), scoped to the playhead (`null`: the whole set). */
export function machinePageRecords(
  records: readonly NormRecord[],
  drilledKey: string | null,
  drilledUid: string | null,
  selfUid: string | null,
  playhead: number | null,
): NormRecord[] {
  const own = recordsOfMachine(records, drilledKey != null ? drilledUid : selfUid);
  return playhead === null ? own : recordsAsOf(own, playhead);
}

/** The uid this daemon reports for itself, once `/machine/specs` has answered. */
export const selfUidOf = (specs: SelfIdentity | null): string | null => specs?.machine_uid ?? null;

/** `uidOf()` — viewer.html:1107. */
export const uidOf = (r: NormRecord): string => r.machine_uid || "unknown";

/** (#2921) The label for a machine nothing has named. Never the hardware uid:
 * a uid lands in screenshots and identifies the physical machine. */
export const UNNAMED_MACHINE = "unnamed machine";

/** Whether a label is `UNNAMED_MACHINE`, with or without its ordinal. */
export const isUnnamedMachineLabel = (name: string): boolean =>
  name === UNNAMED_MACHINE || /^unnamed machine \d+$/.test(name);

/** `nameOf()` — viewer.html:1112. The newest `machine_id` a record carried
 * for this uid, then the presence beat's `display_name`, then
 * `UNNAMED_MACHINE` — never the uid itself (#2921; legacy fell back to it). */
export function nameOf(data: NormRecord[], liveMachines: Map<string, PresenceBeat>, m: string): string {
  if (m === "unknown") return "unknown";
  // (#2030) The MOST RECENT name this uid carried, not the first one found.
  //
  // This was `data.find(...)`, whose own doc conceded it "picks the first it
  // finds. That is fine for a label" — it is not. A machine legitimately
  // carries several names over time (hostname vs the mDNS `.local` form, or a
  // deliberate rename), and `find` returns whichever the window happens to
  // list first. Nothing about that tracks WHICH IS CURRENT.
  //
  // The operator hit the sharp end: one stray `machine.online` naming a
  // different machine landed in their flow directory, and their machine page
  // showed that name from then on — beside their own correct hardware, which
  // comes from `/machine/specs` and was never wrong. Hundreds of correct
  // records arrived afterwards and none of them displaced it.
  //
  // That is the real defect. A bad record is a thing that happens; a bad
  // record that CANNOT BE OUTVOTED is a design choice. Reading the newest
  // means the next correct record heals the display on its own, with no
  // intervention and no need to find and delete anything.
  //
  // Ties and unparsable timestamps keep the earlier winner, so a window with
  // no usable `ts` behaves exactly as before rather than picking arbitrarily.
  let best: NormRecord | null = null;
  let bestTs = -Infinity;
  for (const x of data) {
    if (!sameUid(uidOf(x), m) || !x.machine_id) continue;
    if (best === null || (x.tMs !== null && x.tMs > bestTs)) {
      best = x;
      bestTs = x.tMs ?? bestTs;
    }
  }
  if (best) return best.machine_id as string;
  return beatOf(liveMachines, m)?.display_name || UNNAMED_MACHINE;
}

/** The presence beat of the machine `uid` names, however the beat's key is
 *  cased. */
function beatOf(liveMachines: Map<string, PresenceBeat>, uid: string): PresenceBeat | undefined {
  const exact = liveMachines.get(uid);
  if (exact) return exact;
  const key = findUid(liveMachines.keys(), uid);
  return key === null ? undefined : liveMachines.get(key);
}

/** `machines()` — viewer.html:1123. */
export function machineUids(data: NormRecord[], liveMachines: Map<string, PresenceBeat>): string[] {
  return [...directory(data, liveMachines).uids];
}

/** The window's machines, one entry per MACHINE (a uid seen in two cases is
 *  one), and every name each has appeared under. Built in one pass over the
 *  window and shared by every identity question asked of it. */
interface Directory {
  /** Each machine's uid in the first form the window spells it: records
   *  first, then presence. */
  uids: string[];
  /** Names by canonical uid. */
  names: Map<string, Set<string>>;
}

const directoryCache = new WeakMap<NormRecord[], { liveMachines: Map<string, PresenceBeat>; dir: Directory }>();

function directory(data: NormRecord[], liveMachines: Map<string, PresenceBeat>): Directory {
  const hit = directoryCache.get(data);
  if (hit && hit.liveMachines === liveMachines) return hit.dir;
  const forms = new Map<string, string>();
  const names = new Map<string, Set<string>>();
  const note = (uid: string, name: string | null | undefined) => {
    const key = canonUid(uid);
    if (!forms.has(key)) forms.set(key, uid);
    if (!names.has(key)) names.set(key, new Set());
    if (name) names.get(key)?.add(name);
  };
  for (const r of data) note(uidOf(r), r.machine_id);
  for (const [uid, beat] of liveMachines) note(uid, beat.display_name);
  const dir = { uids: [...forms.values()], names };
  directoryCache.set(data, { liveMachines, dir });
  return dir;
}

/** Whether this window holds a record or a presence beat from the machine
 *  `uid` names. `false` means the viewer receives nothing from it, which is
 *  not the same as it being idle (5.0 R3, `lib/machineAvailability.ts`). */
export function windowHoldsMachine(data: NormRecord[], liveMachines: Map<string, PresenceBeat>, uid: string): boolean {
  return findUid(directory(data, liveMachines).uids, uid) !== null;
}

/** EVERY name a uid has appeared under — across the window's records and its
 * presence beat, not just the one `nameOf` happens to pick.
 *
 * One machine really does carry several names over time. `machine_id` defaults
 * to the hostname, and macOS reports both the short form and the mDNS `.local`
 * form depending on how the daemon was started, so a single stable
 * `machine_uid` accumulates records under both. Renaming a machine, or setting
 * `machine_id` explicitly after running without it, does the same thing.
 *
 * `nameOf` has to answer with ONE name, so it picks the most recent (#2030 —
 * it used to pick the first found, which let a single stale record outvote
 * every later one forever). That is right for a LABEL and still wrong for an
 * identity test: asking "is the name I show
 * for this uid equal to the name specs reports" fails whenever those two
 * happen to be different aliases of the same machine. Identity questions ask
 * this instead, and get a yes if ANY known alias matches. */
export function machineNames(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  uid: string,
): Set<string> {
  return new Set(directory(data, liveMachines).names.get(canonUid(uid)) ?? []);
}

/** (#2814) Is `m` the machine whose `/machine/specs` response this is —
 * i.e. the machine serving the page?
 *
 * SELF IS NEVER UNKNOWN. That is an invariant, not a preference: the machine
 * you are standing on has its own config and its own hardware probe and
 * needs no network, no peer and no history to identify itself.
 *
 * It did not hold, because `MachineSpecsResponse` carried only `machine_id` — a
 * NAME. With no identity to join on, the answer had to be derived from
 * names: "is the name specs reports one of the names this uid has been
 * OBSERVED under" (`machineNames`). Observations live in the rolling flow
 * window, so the answer inherited the window's lifetime and expired with
 * retention. Three states reach it with nothing exotic happening — a fresh
 * install, a machine whose Redis is off (presence self-disables, the
 * off-by-default state), and a rename whose old records have aged out. In
 * all three the machine failed to recognise itself and rendered "hardware
 * not reported" about hardware sitting in the very response used to draw the
 * page.
 *
 * `specs.machine_uid` is the same `darkmux_hardware::machine_uid()` probe
 * that keys every presence beat and stamps every flow record, so when it is
 * present the join is an identity comparison and needs no window at all. The
 * name path stays as the FALLBACK, not as a co-equal: it is what a peer or a
 * committed static fixture built before the field existed answers with, and
 * off macOS the probe legitimately has no value.
 *
 * Note the fallback is also strictly WEAKER, which is the second defect this
 * closes rather than merely routes around: a remote peer logging under the
 * same name this daemon reports passes the name join and gets credited with
 * this host's CPU and RAM. The uid join cannot make that mistake. */
export function isSelfMachine(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: SelfIdentity | null,
  m: string,
): boolean {
  if (!specs) return false;
  if (specs.machine_uid) return sameUid(specs.machine_uid, m);
  return !!specs.machine_id && sameUid(uidForName(data, liveMachines, specs.machine_id), m);
}

/** The one machine in the window that answers to `name`; `null` when none
 *  does, or when several do (a name two machines share names neither). */
export function uidForName(data: NormRecord[], liveMachines: Map<string, PresenceBeat>, name: string): string | null {
  const key = nameKey(name);
  const hits = directory(data, liveMachines).uids.filter((u) => [...machineNames(data, liveMachines, u)].some((n) => nameKey(n) === key));
  return hits.length === 1 ? hits[0] : null;
}

/** The identity fields of `MachineSpecsResponse` this module needs — structurally
 * typed rather than importing the whole interface, so `lib/flow.ts` (the
 * identity module every lens depends on) does not take a dependency on the
 * shape of one HTTP endpoint's whole response. Any `MachineSpecsResponse` satisfies
 * it. */
export interface SelfIdentity {
  machine_id?: string | null;
  machine_uid?: string | null;
}

/** (#2814) `nameOf` with the self-identity FLOOR applied — what to TITLE a
 * machine with, as opposed to `nameOf`'s "what has this uid been called".
 *
 * `nameOf` answers `UNNAMED_MACHINE` when the window holds no record naming
 * the machine (it answered with the raw uid until #2921). That is honest for
 * a uid nothing is known about, and wrong for the one uid the daemon can
 * name out of its own config.
 *
 * (#2921) This is THE machine-label helper: every surface that titles a
 * machine (fleet card, activity lane, machine page, app title, runs pin,
 * stats panel) goes through it, so none can render the uid.
 *
 * This became REQUIRED, not merely nicer, the moment self-identity resolved
 * by uid: before that, `localMachineUid` fell through to `?? machineId` on
 * an empty window and handed back the NAME as a uid, so `nameOf` echoed it
 * and every label read correctly by accident. Resolving the real uid is the
 * fix, and on its own it turns "runs on MacBook-Pro" into
 * "runs on <uid head>-…". Both halves ship together or the second one is a
 * regression.
 *
 * A FLOOR, not an override, and the shape of the condition is what makes it
 * one: it fires only where `nameOf` returned `UNNAMED_MACHINE`, i.e. where
 * it had nothing. Any observed name — including an alias older than the one
 * specs reports — still wins, because #2030's lesson is that a value which
 * cannot be outvoted is the defect rather than the fix. */
export function displayNameOf(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: SelfIdentity | null,
  m: string,
  /** (#2921 follow-up) The operator's declared roster. An entry whose
   *  `machine_uid` is `m` names a machine nothing else does. Empty (a replay,
   *  a static build, a caller with no roster) skips that step. */
  roster: readonly RosterName[] = NO_ROSTER,
): string {
  const own = ownMachineName(data, liveMachines, specs, roster, m);
  return own ?? unnamedLabel(data, liveMachines, specs, roster, m);
}

/** A stable empty roster, so the default never defeats `unnamedLabel`'s cache. */
const NO_ROSTER: readonly RosterName[] = [];

/** The structural slice of a roster entry `displayNameOf` reads. */
export interface RosterName {
  id: string;
  machine_uid?: string | null;
}

/** A name `m` has of its own, in precedence order: an observed one
 *  (`nameOf`), this daemon's specs name when `m` is this daemon, the roster
 *  id declared for `m`. `null` when it has none. */
export function ownMachineName(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: SelfIdentity | null,
  roster: readonly RosterName[],
  m: string,
): string | null {
  const derived = nameOf(data, liveMachines, m);
  if (derived !== UNNAMED_MACHINE) return derived;
  if (specs?.machine_id && isSelfMachine(data, liveMachines, specs, m)) return specs.machine_id;
  const declared = roster.find((e) => sameUid(e.machine_uid, m))?.id;
  return declared || null;
}

/** (#2921 follow-up) `UNNAMED_MACHINE`, with an ordinal from the second one
 *  on ("unnamed machine 2"), so two nameless machines never read alike.
 *
 *  The order is FIRST-SEEN in `data` (earliest record `ts`; a uid known only
 *  from a presence beat comes after every recorded one; ties by uid), not
 *  render order, so every surface handed the same window — the machine's
 *  card and its activity lane — gives the same machine the same ordinal,
 *  and a machine appearing later takes a higher number rather than
 *  renumbering the ones already shown. Only machines with no name of their
 *  own (see `ownMachineName`) take a number, and the number says nothing about the
 *  hardware. */
function unnamedLabel(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: SelfIdentity | null,
  roster: readonly RosterName[],
  m: string,
): string {
  if (m === "unknown") return "unknown";
  // One ordering per (window, beats, specs, roster), shared by every label
  // asked of it: a fleet page names each machine twice (card and lane) plus
  // the lane-width pass, and each ordering costs a scan per machine.
  const cached = unnamedOrderCache.get(data);
  let order =
    cached && cached.liveMachines === liveMachines && cached.specs === specs && cached.roster === roster ? cached.order : null;
  if (!order) {
    order = unnamedOrder(data, liveMachines, specs, roster);
    unnamedOrderCache.set(data, { liveMachines, specs, roster, order });
  }
  const i = order.indexOf(m);
  // A uid outside the window and the beats (a roster-only card's id) sorts
  // after every one in it.
  const at = i >= 0 ? i : order.length;
  return at === 0 ? UNNAMED_MACHINE : `${UNNAMED_MACHINE} ${at + 1}`;
}

const unnamedOrderCache = new WeakMap<
  NormRecord[],
  { liveMachines: Map<string, PresenceBeat>; specs: SelfIdentity | null; roster: readonly RosterName[]; order: string[] }
>();

/** The uids with no name of their own, in first-seen order. */
function unnamedOrder(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: SelfIdentity | null,
  roster: readonly RosterName[],
): string[] {
  const firstSeen = new Map<string, number>();
  for (const r of data) {
    const uid = r.machine_uid;
    if (!uid) continue;
    const at = r.tMs ?? Infinity;
    const prev = firstSeen.get(uid);
    if (prev === undefined || at < prev) firstSeen.set(uid, at);
  }
  for (const uid of liveMachines.keys()) if (!firstSeen.has(uid)) firstSeen.set(uid, Infinity);
  return [...firstSeen.entries()]
    .filter(([uid]) => ownMachineName(data, liveMachines, specs, roster, uid) === null)
    .sort(([ua, ta], [ub, tb]) => (ta !== tb ? (ta < tb ? -1 : 1) : ua < ub ? -1 : ua > ub ? 1 : 0))
    .map(([uid]) => uid);
}

/** `localMachineUid()` — viewer.html:2642-2644. Which uid IS this daemon,
 * for the nav-tab/deep-link entry into the machine page.
 *
 * Matches against EVERY alias a uid has used (`machineNames`), not just the one
 * `nameOf` returns. Legacy compared `nameOf(x) === machineId`, and so did this
 * — which silently failed on a machine whose window carries records under two
 * names: `nameOf` answered with the older alias, `/machine/specs` reported the
 * current one, no uid matched, and the `?? machineId` fallback then handed back
 * the NAME as if it were a uid. Every downstream comparison against a real uid
 * was false from there on. Found on a laptop logging as both `MacBook-Pro` and
 * `MacBook-Pro.local`.
 *
 * The `?? machineId` fallback stays for the case it was written for: a freshly
 * booted daemon that has produced no records or beats of its own yet, where
 * there is no uid to find and the raw name is the best available handle.
 *
 * (#2814) SELF IS NEVER UNKNOWN. `reportedUid` short-circuits all of the
 * above, and has to, because everything above is an OBSERVATION: it asks
 * which uid has been seen carrying this name, and that evidence lives in the
 * expiring flow window. The machine you are STANDING ON is not an
 * observation — `/machine/specs` reads its own hardware uid from its own
 * probe, with no network and no history. Routing self-identity through the
 * window meant a fresh install, a machine with presence off, or a rename
 * whose old records aged out could not identify itself, and the
 * `?? machineId` fallback then handed back a NAME as if it were a uid, so
 * every downstream uid comparison was false from there.
 *
 * It is a floor, not an override: it answers only the question "which uid am
 * I", which nothing else on this page can answer more authoritatively. When
 * it is absent — off macOS, a failed probe, a peer or static fixture built
 * before the field existed — the name path below runs exactly as before. */
export function localMachineUid(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  machineId: string | null | undefined,
  reportedUid?: string | null,
): string | null {
  // The reported uid, in the form the window spells it, so it keys the same
  // maps the flow data keys.
  if (reportedUid) return findUid(machineUids(data, liveMachines), reportedUid) ?? reportedUid;
  if (!machineId) return null;
  return uidForName(data, liveMachines, machineId) ?? machineId;
}

/** What the machine `id` answers to. `id` is a uid the window holds, or a
 *  roster-only card's roster id (a declared peer nothing has been seen from),
 *  whose roster entry may declare its uid. The names are this machine's alias
 *  set (observed, presence, roster, this daemon's own when `id` is self, and
 *  `extraNames`) minus every spelling another machine also answers to, so a
 *  name two machines share attributes to neither. */
export function machineMatch(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: SelfIdentity | null,
  roster: readonly RosterName[],
  id: string,
  extraNames: Iterable<string> = [],
): MachineMatch {
  const seen = findUid(directory(data, liveMachines).uids, id);
  const entry = seen === null ? roster.find((e) => e.id === id || sameUid(e.machine_uid, id)) : undefined;
  const uid = seen ?? entry?.machine_uid ?? id;
  const own = new Set<string>(extraNames);
  if (entry) own.add(entry.id);
  for (const n of ownAliases(data, liveMachines, specs, roster, seen, uid)) own.add(n);
  const others = namesOfOthers(data, liveMachines, roster, uid);
  return { uid: seen ?? entry?.machine_uid ?? null, names: new Set([...own].map(nameKey).filter((k) => !others.has(k))) };
}

/** The names `uid` answers to on its own account: what the window recorded
 *  (when it is seen), the roster ids declared for it, and this daemon's own
 *  name when it is this machine. */
function ownAliases(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: SelfIdentity | null,
  roster: readonly RosterName[],
  seen: string | null,
  uid: string,
): string[] {
  const names = seen === null ? [] : [...machineNames(data, liveMachines, seen)];
  for (const e of roster) if (sameUid(e.machine_uid, uid)) names.push(e.id);
  if (specs?.machine_id && seen !== null && isSelfMachine(data, liveMachines, specs, seen)) names.push(specs.machine_id);
  return names;
}

/** The `nameKey`s some machine other than `uid` answers to. */
function namesOfOthers(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  roster: readonly RosterName[],
  uid: string,
): Set<string> {
  const others = new Set<string>();
  for (const u of directory(data, liveMachines).uids) {
    if (sameUid(u, uid)) continue;
    for (const n of machineNames(data, liveMachines, u)) others.add(nameKey(n));
  }
  for (const e of roster) if (e.machine_uid && !sameUid(e.machine_uid, uid)) others.add(nameKey(e.id));
  return others;
}

/** The display label of each of `uids`, with machines that would read alike
 *  told apart: from the second on, a shared name takes an ordinal in `uids`
 *  order ("Mac", "Mac 2"). Never the uid. */
export function machineLabels(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: SelfIdentity | null,
  roster: readonly RosterName[],
  uids: readonly string[],
): Map<string, string> {
  const seenCount = new Map<string, number>();
  const labels = new Map<string, string>();
  for (const uid of uids) {
    const name = displayNameOf(data, liveMachines, specs, uid, roster);
    const key = nameKey(name);
    const n = (seenCount.get(key) ?? 0) + 1;
    seenCount.set(key, n);
    labels.set(uid, n === 1 ? name : `${name} ${n}`);
  }
  return labels;
}

/** The machine a record names, as a key equal across records of one machine:
 *  its canonical uid, else (a record from before uids were stamped) its name
 *  spelling. `null` for a record that names no machine. */
export const recordMachineKey = (r: NormRecord): string | null => machineRefKey({ uid: r.machine_uid, name: r.machine_id });

const NO_BEATS: Map<string, PresenceBeat> = new Map();

/** The machines a slice of records names: one per `recordMachineKey`, in
 *  first-seen order, each with the label to show it by and every name it
 *  has appeared under. */
export interface RecordMachines {
  keys: string[];
  /** Machines that would read alike are told apart by an ordinal. */
  label: Map<string, string>;
  /** Every `machine_id` spelling seen under the key. */
  aliases: Map<string, Set<string>>;
}

/** Built from the slice alone (no presence, no roster), so a label here can
 *  be an "unnamed machine" where a fleet card would know more. */
export function recordMachines(records: readonly NormRecord[]): RecordMachines {
  const first = new Map<string, NormRecord>();
  const aliases = new Map<string, Set<string>>();
  for (const r of records) {
    const key = recordMachineKey(r);
    if (key === null) continue;
    if (!first.has(key)) first.set(key, r);
    if (r.machine_id) aliases.set(key, (aliases.get(key) ?? new Set()).add(r.machine_id));
  }
  const slice = records as NormRecord[];
  const seen = new Map<string, number>();
  const label = new Map<string, string>();
  for (const [key, r] of first) {
    const name = r.machine_uid ? displayNameOf(slice, NO_BEATS, null, r.machine_uid) : (r.machine_id as string);
    const n = (seen.get(nameKey(name)) ?? 0) + 1;
    seen.set(nameKey(name), n);
    label.set(key, n === 1 ? name : `${name} ${n}`);
  }
  return { keys: [...first.keys()], label, aliases };
}
