/**
 * Ports of `viewer.html`'s raw-flow-record derivation pipeline — the
 * machinery behind the machine lens's "runs on <machine>" list and its
 * local-machine identity resolution. Every function here is named at its
 * legacy source line. Validated line-for-line against the recorded corpus
 * (`tests/parity/corpus/flow-{today,yesterday}.json`) by hand-simulating
 * this exact logic in Node and diffing the result against
 * `tests/parity/goldens/machine.txt`'s runs section before this file was
 * written — see the packet report for the transcript. One bug surfaced by
 * that validation and is worth naming so a future port of another lens
 * doesn't repeat it: `loadLiveWindow()` (viewer.html:3497) fetches
 * `[prevDate, today]` IN THAT ORDER and concatenates in that order — a
 * session_id that recurs across the day boundary must resolve its
 * "first-seen" record from the EARLIER day first, which only happens if the
 * merge preserves fetch order. Concatenating `[today, yesterday]` instead
 * (the natural-feeling order) silently reorders which record `Array.find`
 * returns for a reused session id and desyncs the runs list's sort order
 * from rank ~10 onward with NO error — exactly the class of bug this
 * validate-before-port step exists to catch.
 */

import type { PresenceBeat } from "../types/handwritten";
// (#2813) The canonical status axis, generated from the Rust enum. Importing
// it here is the point: the label below is a total function of it, so the two
// cannot drift apart again.
import type { RunStatus } from "../types/generated/RunStatus";
import type { AbandonReason } from "../types/generated/AbandonReason";
import { isPlainObject } from "./guards";
import { missionClosed } from "./lifecycle";
import { runIndex } from "./runRef";
import { ACTION, CATEGORY, byTime, earliestByTime, ingestJsonl, ingestRecord, latestByTime, recKey, recordsAsOf, recordsSince, type NormRecord } from "./ingest";

/** `LIVE_WINDOW_MS` — viewer.html:3374. The rolling live window `RAW` is
 * bounded to; also the "N records · last Nh" meta-line's hour figure. */
export const LIVE_WINDOW_MS = 24 * 60 * 60 * 1000;


/** `todayUTC()` — viewer.html:3369. */
export function todayUTC(): string {
  return new Date().toISOString().slice(0, 10);
}

/** `prevDateUTC()` — viewer.html:3379. */
export function prevDateUTC(d: string): string {
  const dt = new Date(d + "T00:00:00Z");
  dt.setUTCDate(dt.getUTCDate() - 1);
  return dt.toISOString().slice(0, 10);
}

/** GETs a static playback source (`source.ts, the flow file`) and
 * ingests it — the static-build twin of `GET /flow/<date>`, read directly by
 * both `useRouteRecords` (the event log's data) and `PlaybackLens` (the
 * stage's data), each via the SAME query key so they share one fetch and can
 * never disagree about the file's contents (#1801).
 *
 * Returns `[]` rather than throwing on a network failure, a 404, or an
 * empty file — matching legacy's own `catch(e){ RAW=[]; }` around the same
 * fetch. A static build with no committed flow file (or one not yet built)
 * is a valid, if empty, playback — never a crash and never a silent
 * fallback to the live route (see `route.ts`'s own doc for why the ROUTE
 * itself does not depend on this fetch succeeding). */
export async function fetchStaticFlowRecords(src: string): Promise<NormRecord[]> {
  try {
    const res = await fetch(src);
    if (!res.ok) return [];
    const text = await res.text();
    return ingestJsonl(text);
  } catch {
    return [];
  }
}

/** `if(!injectedDate&&RAW.length)date=String(RAW[0].ts||"").slice(0,10)||date;`
 * — viewer.html:3902, the flowSrc branch's own date derivation. Takes the
 * records in FILE order, deliberately `records[0]` rather than the earliest
 * record, matching legacy's un-sorted read, and reads its parsed time as a
 * UTC day. Returns `null` on an empty array or a first record with no usable
 * time, so a caller supplies its OWN placeholder rather than this function
 * inventing one. */
export function firstRecordDate(records: readonly NormRecord[]): string | null {
  const t = records[0]?.tMs;
  return t == null ? null : utcDay(t);
}

const utcDay = (t: number): string => new Date(t).toISOString().slice(0, 10);

/** The earliest record's UTC day in a slice, or null. Records from the daemon
 * are not guaranteed to arrive sorted, so this scans rather than reading the
 * first element. */
export function earliestRecordDate(records: readonly NormRecord[]): string | null {
  const first = earliestByTime(records)?.tMs;
  return first == null ? null : utcDay(first);
}

/** Header owns liveness (operator, 2026-09-03): a mission page is a RECORDING
 * only once the mission has reached a terminal record; until then it is live,
 * whatever day its records carry. The dispatch route decides the same thing
 * from presence; a mission decides it from its own lifecycle records because
 * a mission's work spans many sessions. Returns the replay day, or null while
 * the mission is still running. */
export function missionReplayDate(records: readonly NormRecord[]): string | null {
  return missionClosed(runIndex(records).groups) ? earliestRecordDate(records) : null;
}

/** The `/flow-mission/:id` response body's own `truncated` flag —
 * `crates/darkmux-serve/src/lib.rs`'s `collect_records_by_field`, capped at
 * `MAX_CATALOG_RECORDS` (10,000). A bare array never carries this flag and
 * reads as `false`. `MissionGraphLens.tsx`'s own `srvTruncated` reads the
 * SAME field (`bodies[0].truncated`) for the same reason: a mission past the
 * server cap must say so, or "N of 10000" silently restates the cap as the
 * mission's whole history. */
export function bodyTruncated(body: unknown): boolean {
  if (!isPlainObject(body)) return false;
  return !!(body as { truncated?: boolean }).truncated;
}

/** The tail-cache-side counterpart to `buildFlowWindow`'s own dedup+window
 * filter — used by the live tail (SSE appends and the reconcile backstop,
 * viewer.html's `reconcileLiveWindow`, 3758-3782) so repeated `?since=`
 * polls with a deliberate overlap window (`RECONCILE_OVERLAP_MS`) don't grow
 * the tail cache's stored array by the overlap on every single poll.
 * `buildFlowWindow` itself ALSO dedups the final merged (day-fetch + tail)
 * result, so this isn't required for display correctness — it's what keeps
 * the underlying cache entry bounded across a long-lived tab, the same thing
 * `applyLive()`'s RAW age-out prunes for (viewer.html:3522-3539). */
export function mergeTailRecords(existing: readonly NormRecord[], incoming: readonly NormRecord[], cutMs: number): NormRecord[] {
  const seen = new Set(existing.map(recKey));
  const merged = existing.slice();
  for (const r of incoming) {
    const k = recKey(r);
    if (seen.has(k)) continue;
    seen.add(k);
    merged.push(r);
  }
  return recordsSince(merged, cutMs);
}

/** The view-model half of `flowToRenderModel()` (viewer.html:3195-3234):
 * ingested records plus one synthesized runtime row per session, sorted by
 * time. Deliberately does NOT window or dedup: those belong to the LIVE
 * two-day merge (`buildFlowWindow`, below), not to reading a record set.
 *
 * (#1800 P2) A historical day is shaped the same way and windowed NOT AT
 * ALL — legacy's playback boot is literally `DATA=flowToRenderModel(RAW)`
 * with no window step (viewer.html:3922). Feeding a replayed day through
 * `buildFlowWindow` instead would drop every record older than 24h, i.e. the
 * entire day. */
export function shapeRecords(records: readonly NormRecord[]): NormRecord[] {
  return [...records, ...perSessionRuntimeRecords(records)].sort(byTime);
}

/** The APPEND half of `flowToRenderModel()` — viewer.html:3223-3234. One
 * synthetic `source:"runtime"` telemetry record per session that emitted any
 * `dispatch.turn`, carrying that session's max `turn_seq` as its TURNS
 * metric. The subsystem view reads the metric from here rather than
 * re-scanning, which is why the record exists at all.
 *
 * (#1800) Ported because it is not merely internal: these records are part of
 * `DATA`, so they are COUNTED. `goldens/playback-date.txt`'s meta line reads
 * "2008 records" against a fixture holding 1993 real records and 15 sessions
 * with turns — the 15 are these. The live meta line does not state a count
 * (legacy moved it to the event pane), which is why nothing noticed their
 * absence until a replay had to say the number out loud.
 *
 * Legacy sorts the whole set by ts afterwards because these are appended out
 * of order, and the event log plus follow-latest both assume `DATA` is
 * temporal. `shapeRecords` does the same, so the sort is not this function's
 * own concern. Each row goes through `ingestRecord` like any other record, so
 * its `tMs` obeys the one bad-timestamp policy. */
function perSessionRuntimeRecords(records: readonly NormRecord[]): NormRecord[] {
  const perSession = new Map<string, { turns: number; ts: string; machineId?: string; machineUid?: string }>();
  for (const r of records) {
    if (r.action !== ACTION.DispatchTurn || !r.session_id) continue;
    const seq = Number((r.payload as { turn_seq?: unknown } | undefined)?.turn_seq) || 0;
    const prev = perSession.get(r.session_id);
    const entry = prev ?? { turns: 0, ts: r.ts };
    entry.turns = Math.max(entry.turns, seq);
    // `e.ts=r.ts` unconditionally — the LAST turn's timestamp, not the max.
    // Records arrive in ts order, so these agree; keeping legacy's form means
    // they keep agreeing if that ever stops being true.
    entry.ts = r.ts;
    if (r.machine_id) entry.machineId = r.machine_id;
    if (r.machine_uid) entry.machineUid = r.machine_uid;
    perSession.set(r.session_id, entry);
  }
  const out: NormRecord[] = [];
  for (const [sessionId, e] of perSession) {
    const row = ingestRecord({
      ts: e.ts,
      category: CATEGORY.Telemetry,
      source: "runtime",
      machine_id: e.machineId,
      machine_uid: e.machineUid,
      session_id: sessionId,
      fields: { turns: e.turns },
    });
    if (row) out.push(row);
  }
  return out;
}

/** `loadLiveWindow()` + the dispatch-action slice of `flowToRenderModel()` —
 * viewer.html:3497-3512 / 3161-3187. `yesterday`/`today` MUST be passed in
 * that fetch order (see the module doc above for why). */
export function buildFlowWindow(yesterday: readonly NormRecord[], today: readonly NormRecord[], nowMs: number): NormRecord[] {
  const windowed = recordsSince(shapeRecords([...yesterday, ...today]), nowMs - LIVE_WINDOW_MS);
  const seen = new Set<string>();
  return windowed.filter((r) => {
    const k = recKey(r);
    if (seen.has(k)) return false;
    seen.add(k);
    return true;
  });
}

/** `recompute()`'s tMax — viewer.html:1040-1041. */
export function computeTMax(data: readonly NormRecord[]): number {
  return latestByTime(data)?.tMs ?? Date.now();
}

/** `recompute()`'s tMin — viewer.html:1051. Unused while `/next` was
 * live-only (the live timeline anchors on NOW and a fixed window); a REPLAY
 * spans `tMin..tMax`, which is what makes the axis describe the recorded day
 * rather than the last 24 hours of wall-clock. */
export function computeTMin(data: readonly NormRecord[]): number {
  return earliestByTime(data)?.tMs ?? Date.now();
}

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
    if (uidOf(x) !== m || !x.machine_id) continue;
    if (best === null || (x.tMs !== null && x.tMs > bestTs)) {
      best = x;
      bestTs = x.tMs ?? bestTs;
    }
  }
  if (best) return best.machine_id as string;
  const b = liveMachines.get(m);
  return b?.display_name || UNNAMED_MACHINE;
}

/** `machines()` — viewer.html:1123. */
export function machineUids(data: NormRecord[], liveMachines: Map<string, PresenceBeat>): string[] {
  return [...new Set([...data.map(uidOf), ...liveMachines.keys()])];
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
  const names = new Set<string>();
  for (const r of data) {
    if (uidOf(r) === uid && r.machine_id) names.add(r.machine_id as string);
  }
  const beat = liveMachines.get(uid);
  if (beat?.display_name) names.add(beat.display_name);
  return names;
}

/** (#2814) Is `m` the machine whose `/machine/specs` response this is —
 * i.e. the machine serving the page?
 *
 * SELF IS NEVER UNKNOWN. That is an invariant, not a preference: the machine
 * you are standing on has its own config and its own hardware probe and
 * needs no network, no peer and no history to identify itself.
 *
 * It did not hold, because `MachineSpecs` carried only `machine_id` — a
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
  if (specs.machine_uid) return specs.machine_uid === m;
  return !!specs.machine_id && machineNames(data, liveMachines, m).has(specs.machine_id);
}

/** The identity fields of `MachineSpecs` this module needs — structurally
 * typed rather than importing the whole interface, so `lib/flow.ts` (the
 * identity module every lens depends on) does not take a dependency on the
 * shape of one HTTP endpoint's whole response. Any `MachineSpecs` satisfies
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
  const declared = roster.find((e) => e.machine_uid === m)?.id;
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
  if (reportedUid) return reportedUid;
  if (!machineId) return null;
  return machineUids(data, liveMachines).find((x) => machineNames(data, liveMachines, x).has(machineId)) ?? machineId;
}

/** What a lens is allowed to say about a run: one canonical status, plus the
 * payload facts a LABEL may render it with. */
export interface RunState {
  status: RunStatus;
  /** `error` only. The terminal record reported a kill/timeout rather than a
   * plain failure — a rendering nuance of `error`, not a status of its own. */
  killed: boolean;
  /** `abandoned` only. Mirrors `Run.abandoned_reason` on the wire. */
  abandonReason?: AbandonReason;
}

/**
 * (#2813) The word a lens shows for a run. A TOTAL function of the canonical
 * status — a lens may choose WORDS, it may never choose STATES.
 *
 * This used to take four booleans and return `running | killed | errored |
 * complete | canceled`, a vocabulary that overlapped the server's
 * `RunStatus` in only two values and had no total function between them. It
 * was a faithful port of `viewer.html`, written before a typed API existed;
 * the enum arrived afterwards and this never went back. Meanwhile
 * `runStatusLabel` in the runs lens had been doing it correctly all along —
 * status verbatim, `abandoned` split on its reason.
 *
 * The `never` binding below is the part that keeps this true: adding a
 * `RunStatus` variant is a COMPILE ERROR here until it is handled. The
 * generated-types drift guard in CI keeps `RunStatus` current with Rust; this
 * keeps the LABELS current with `RunStatus`. Without it, CI is happy while a
 * lens quietly ignores the enum, which is how the two vocabularies coexisted.
 */
export function statusLabel(state: RunState): string {
  switch (state.status) {
    case "planned":
      return "planned";
    case "running":
      return "running";
    case "complete":
      return "complete";
    case "error":
      return state.killed ? "killed" : "errored";
    case "abandoned":
      return state.abandonReason === "aborted" ? "aborted" : "no ending recorded";
    case "unparseable":
      return "unknown";
    default: {
      const unhandled: never = state.status;
      return unhandled;
    }
  }
}

/** `machPresent()` — viewer.html:1321-1327. true=present, false=absent,
 * null=unknown (no evidence either way). */
export function machPresent(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  tMax: number,
  m: string,
): boolean | null {
  if (liveMachines.has(m)) return true;
  const edges = recordsAsOf(data, tMax).filter(
    (r) => uidOf(r) === m && (r.action === ACTION.MachineOnline || r.action === ACTION.MachineOffline),
  );
  // The latest edge by time; an untimed one only when no edge is timed.
  const last = latestByTime(edges);
  return last === undefined ? null : last.action === ACTION.MachineOnline;
}

function compKey(sessionId: string | undefined, ts: string | undefined): string {
  return `${sessionId || ""}\x1f${ts || ""}`;
}

/** `flowToRenderModel()` — viewer.html:3171-3242. Shapes an ingested
 * record array (the `/flow-session/<id>` or `/flow-mission/<id>` "replay this
 * thing" payload — the session drill-in's data source, `lenses/session/
 * sessionRun.ts`) into the shape `runRegions()` reads:
 *
 * - `fields` ALIASED from `payload` when a record carries the latter but
 *   not the former (schema 1.6+ carries type-specific data under `fields`
 *   on the wire; older/synthesized records only have `payload`), which the
 *   live window's `shapeRecords` does not do, so a session-view consumer
 *   reading `r.fields` uniformly needs THIS pass
 * - a `dispatch.compaction` record retagged as compaction telemetry, UNLESS
 *   a dedicated `telemetry.compaction` sibling already covers the same
 *   `(session_id, ts)` (the #1122 double-count guard)
 * - a category default (`work` absent a `source`, else `telemetry`)
 * - then `shapeRecords`: the synthesized per-session "runtime" row (the
 *   ONLY source for the session view's TURNS metric) and the time sort. */
export function flowToRenderModel(records: readonly NormRecord[]): NormRecord[] {
  const compTelemetryKeys = new Set<string>();
  for (const r of records) {
    if (r.action === ACTION.TelemetryCompaction) compTelemetryKeys.add(compKey(r.session_id, r.ts));
  }
  const retagged = records.map((r) => {
    const o: NormRecord = { ...r };
    if (o.payload && !o.fields) o.fields = o.payload;
    if (o.action === ACTION.DispatchCompaction && !compTelemetryKeys.has(compKey(o.session_id, o.ts))) {
      const p = (o.payload || {}) as { before_messages?: number; after_messages?: number };
      o.category = CATEGORY.Telemetry;
      o.source = "compaction";
      o.fields = { from: p.before_messages || 0, to: p.after_messages || 0 };
    }
    if (!o.category) o.category = o.source ? CATEGORY.Telemetry : CATEGORY.Work;
    return o;
  });
  return shapeRecords(retagged);
}
