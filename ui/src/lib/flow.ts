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
import { ACTION, CATEGORY, byTime, ingestJsonl, ingestRecord, isAsOf, latestByTime, recKey, recordsAsOf, recordsSince, timesOf, type NormAction, type NormRecord } from "./ingest";

/** `LIVE_WINDOW_MS` — viewer.html:3374. The rolling live window `RAW` is
 * bounded to; also the "N records · last Nh" meta-line's hour figure. */
export const LIVE_WINDOW_MS = 24 * 60 * 60 * 1000;

/** `FLOW_LIVE_TTL_MS` — viewer.html:3342. How recent a session's last
 * record must be (vs wall-clock now) to count as "live" absent Redis
 * presence — see `flowLiveSessions` below. */
export const FLOW_LIVE_TTL_MS = 300 * 1000;

/** (#2902 step 5) How long past its announced resume time a budget wait
 * stays open with no further word from its waiter. A waiter still held at
 * its resume time announces again (a new `budget.wait` with the new time),
 * and one whose window has room writes `budget.resume` within one poll
 * (half a second) plus a registry re-read; a wait silent this long past its
 * resume time has lost its process, and reads as neither live nor resting. */
export const BUDGET_WAIT_GRACE_MS = 60 * 1000;


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
  const ts = timesOf(records);
  return ts.length ? utcDay(Math.min(...ts)) : null;
}

/** Header owns liveness (operator, 2026-09-03): a mission page is a RECORDING
 * only once the mission has reached a terminal record; until then it is live,
 * whatever day its records carry. The dispatch route decides the same thing
 * from presence; a mission decides it from its own lifecycle records because
 * a mission's work spans many sessions. Returns the replay day, or null while
 * the mission is still running. */
export function missionReplayDate(records: readonly NormRecord[]): string | null {
  const terminal = records.some((r) => r.action === ACTION.MissionClose || r.action === ACTION.MissionAbort);
  return terminal ? earliestRecordDate(records) : null;
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
  const ts = timesOf(data);
  return ts.length ? Math.max(...ts) : Date.now();
}

/** `recompute()`'s tMin — viewer.html:1051. Unused while `/next` was
 * live-only (the live timeline anchors on NOW and a fixed window); a REPLAY
 * spans `tMin..tMax`, which is what makes the axis describe the recorded day
 * rather than the last 24 hours of wall-clock. */
export function computeTMin(data: readonly NormRecord[]): number {
  const ts = timesOf(data);
  return ts.length ? Math.min(...ts) : Date.now();
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

/** `sessionsOn()` — viewer.html:1124. */
export function sessionsOn(data: NormRecord[], m: string): string[] {
  return [...new Set(data.filter((r) => uidOf(r) === m && r.session_id).map((r) => r.session_id as string))];
}

/** One machine's (session_id, mission_id) PAIR — the fleet timeline's own
 * unit of a "bar" (#2125). `sessionsOn` above dedups on `session_id` alone,
 * which review missions violate on purpose: `dispatch.map`'s per-item
 * session id is `session_id::task(&step.task_id)` — a FIXED string per
 * review config (`task-review-probe-mid-task` etc), reused verbatim by
 * every review run (`crates/darkmux-crew/src/step_kinds/
 * builtins.rs::DispatchMapStepKind::dispatch_session_id`, and the
 * server-side `#1918` doc names the same defect). Two DIFFERENT review
 * missions on the same machine therefore share one `session_id` but carry
 * DIFFERENT `mission_id`s — `sessionsOn`'s plain string-set collapses them
 * into ONE entry, so `dispatchRec`/`dispatchEnd` (unscoped `Array.find`)
 * paired whichever mission's `dispatch start` happened to come first with
 * whichever mission's terminal/abort happened to come first, drawing one
 * bar spanning both missions' real spans — measured live as a 20-hour
 * "canceled" span for a mission that actually ran 23 minutes.
 *
 * `missionId` is `undefined` for a session that never carried one (a bare
 * `dispatch --profile` outside any mission) — those keep behaving exactly
 * as `sessionsOn` always has, since there is nothing to disambiguate. */
export interface MachineSessionRun {
  sessionId: string;
  missionId?: string;
}
export function sessionRunsOn(data: NormRecord[], m: string): MachineSessionRun[] {
  const seen = new Set<string>();
  const out: MachineSessionRun[] = [];
  // (#2125 follow-up) A session's bookends (`session.end`, some dispatch
  // records) may carry no `mission_id` while its work records do. Those
  // mission-less records belong to the session's mission when it has exactly
  // ONE; only a session that genuinely spans several missions (the review
  // pipeline's reused step sessions) keeps them apart. Without this, one
  // session renders as two overlapping bars.
  const missionsBySid = new Map<string, Set<string>>();
  for (const r of data) {
    if (uidOf(r) !== m || !r.session_id || !r.mission_id) continue;
    let set = missionsBySid.get(r.session_id);
    if (!set) { set = new Set(); missionsBySid.set(r.session_id, set); }
    set.add(r.mission_id);
  }
  for (const r of data) {
    if (uidOf(r) !== m || !r.session_id) continue;
    const only = missionsBySid.get(r.session_id);
    const missionId = r.mission_id || (only && only.size === 1 ? [...only][0] : undefined);
    const key = `${r.session_id}\x1f${missionId ?? ""}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push({ sessionId: r.session_id, missionId });
  }
  return out;
}

/** (#2911) One window's records grouped by `session_id`, each group in the
 * window's own order. Built once per window ARRAY (a `WeakMap` keyed on its
 * identity) and reused by every per-session lookup below.
 *
 * Why it exists: those lookups (`dispatchRec`, `sessEnd`, `sessionRunning`)
 * used to scan the whole window per session, and the fleet cards, the
 * flow-derived liveness set and the activity timeline ask them for every
 * session on every render. That was affordable while the lens only
 * re-rendered on new records; once a live execution re-renders it every
 * second (#2911's countdown tick) it was the bulk of a ~100 ms hitch per
 * second on a busy day. The window array is stable across those ticks
 * (`useFlowWindow` keys it on a coarse edge), so the index is built once
 * per new window and each tick's lookups touch one session's records.
 *
 * The contract this relies on: a window array is never mutated after it is
 * first read. Every producer builds a new array (`buildFlowWindow`, the
 * playback slices), so that already holds; a caller that appended to an
 * array in place after reading it would get the stale grouping. */
const sessionIndexCache = new WeakMap<readonly NormRecord[], Map<unknown, readonly NormRecord[]>>();
/** Returned on every miss, shared: typed `readonly` (as are the groups) so a
 *  caller cannot `push` into it and corrupt every later lookup. */
const NO_RECORDS: readonly NormRecord[] = [];
let sessionIndexBuilds = 0;

/** Test-only: how many session indexes have been built. A test that ticks a
 *  lens asserts this does NOT move, which pins that the window array stayed
 *  the same object across the tick. It cannot see whether a lookup went
 *  through the index (a whole-window scan builds nothing); the callers'
 *  own tests pin that by reading from an index the array has outgrown. */
export function __sessionIndexBuilds(): number {
  return sessionIndexBuilds;
}

export function sessionRecords(data: readonly NormRecord[], sid: string): readonly NormRecord[] {
  let index = sessionIndexCache.get(data);
  if (!index) {
    // Keyed on `session_id` exactly as the record carries it, whatever its
    // type: the scans this replaces compared `r.session_id === sid`, and a
    // `Map` key matches the same way (SameValueZero). Its one difference,
    // `NaN` equal to itself, is answered below the way the scan answered it.
    const groups = new Map<unknown, NormRecord[]>();
    for (const r of data) {
      if (!r) continue;
      const group = groups.get(r.session_id);
      if (group) group.push(r);
      else groups.set(r.session_id, [r]);
    }
    index = groups;
    sessionIndexCache.set(data, index);
    sessionIndexBuilds++;
  }
  if (Number.isNaN(sid)) return NO_RECORDS;
  return index.get(sid) ?? NO_RECORDS;
}

/** `dispatch()` — viewer.html:1125. `missionId` (#2125), when given, scopes
 * the match to records naming that mission — see `sessionRunsOn`'s own doc
 * for why a bare `session_id` match is unsafe for a review-shaped session.
 * `undefined` (every pre-existing caller) preserves the exact prior
 * session_id-only behavior. */
export function dispatchRec(data: NormRecord[], sid: string, act: "start" | "complete" | "error", missionId?: string): NormRecord | undefined {
  const action = DISPATCH_ACT[act];
  return sessionRecords(data, sid).find(
    (r) => r.session_id === sid && r.action === action && (missionId === undefined || !r.mission_id || r.mission_id === missionId),
  );
}

/** `dispatchEnd()` — viewer.html:1131. */
export function dispatchEnd(data: NormRecord[], sid: string, missionId?: string): NormRecord | undefined {
  return dispatchRec(data, sid, "complete", missionId) ?? dispatchRec(data, sid, "error", missionId);
}

const DISPATCH_ACT = { start: ACTION.DispatchStart, complete: ACTION.DispatchComplete, error: ACTION.DispatchError } as const;

/** `dispatchErrored()` — viewer.html:1132. */
export const dispatchErrored = (rec: NormRecord | undefined): boolean => !!rec && rec.action === ACTION.DispatchError;

/** `dispatchKilled()` — viewer.html:1133. Watchdog kill = exit 137. */
export const dispatchKilled = (rec: NormRecord | undefined): boolean =>
  dispatchErrored(rec) && (rec?.payload as { exit_code?: number } | undefined)?.exit_code === 137;

/** `sessEnd()` — viewer.html:1149. `missionId` (#2125) — see `dispatchRec`'s
 * own doc. */
export function sessEnd(data: NormRecord[], sid: string, missionId?: string): NormRecord | undefined {
  return sessionRecords(data, sid).find(
    (r) => r.session_id === sid && r.action === ACTION.SessionEnd && (missionId === undefined || !r.mission_id || r.mission_id === missionId),
  );
}

/** `sessionCloseEdge()` — viewer.html:1171-1175. The EARLIEST of the dispatch
 * terminal and the reconciler's `session.end`. Every "is this session done /
 * where does its bar end" decision goes through here rather than bare
 * `dispatchEnd`: a session whose ONLY terminal is `session.end` (abandoned,
 * hard-killed, shipped without a clean complete) must read as ENDED, not
 * in-flight to the playhead.
 *
 * (#2125) `missionId`, threaded straight through — an abort closes only ITS
 * OWN mission's open steps; a bare `mission_id`-less lookup would let a
 * SIBLING mission's abort (or `session.end`) close this one's bar too. */
export function sessionCloseEdge(data: NormRecord[], sid: string, missionId?: string): NormRecord | undefined {
  const c = dispatchEnd(data, sid, missionId);
  const e = sessEnd(data, sid, missionId);
  if (c && e) return byTime(c, e) <= 0 ? c : e;
  return c ?? e;
}

/** (#2902 step 5, 5th review MF1) The budget wait still OPEN in `own` as
 * of `t`: the newest `budget.wait` at or before `t`, with no
 * `budget.resume`, `budget.stop`, dispatch terminal or `session.end` after
 * it, and not silent past its resume time plus [`BUDGET_WAIT_GRACE_MS`].
 * `null` otherwise. A hosted call's gate writes its wait BEFORE any
 * `dispatch start` (contract 2), so this is the one piece of live work a
 * session can have with no start at all: every liveness gate asks it. */
export function openBudgetWait(own: readonly NormRecord[], t: number): NormRecord | null {
  const seen = recordsAsOf(own, t);
  const open = latestByTime(seen.filter((r) => r.action === ACTION.BudgetWait));
  if (!open) return null;
  if (seen.some((r) => r !== open && r.action !== undefined && BUDGET_WAIT_CLOSERS.has(r.action) && byTime(r, open) >= 0)) return null;
  // A wait with no usable time has no deadline to outlive (the bad-timestamp
  // policy): it stays open until something closes it.
  if (open.tMs === null) return open;
  const p = (open.payload ?? open.fields ?? {}) as { wait_seconds?: unknown };
  const secs = typeof p.wait_seconds === "number" && Number.isFinite(p.wait_seconds) ? Math.max(0, p.wait_seconds) : 0;
  return t <= open.tMs + secs * 1000 + BUDGET_WAIT_GRACE_MS ? open : null;
}

/** What ends a budget wait: its call went ahead, its run was stopped, or its
 *  execution or session ended. */
const BUDGET_WAIT_CLOSERS: ReadonlySet<NormAction> = new Set<NormAction>([
  ACTION.BudgetResume,
  ACTION.BudgetStop,
  ACTION.DispatchComplete,
  ACTION.DispatchError,
  ACTION.SessionEnd,
]);

/** `sessionRunning()` — viewer.html:1183-1187. THE single source of truth for
 * "is this session in flight?"
 *
 * (Playback parity, Change A, finding #7) This used to run two DIFFERENT
 * algorithms selected by a `liveMode` boolean: live trusted `liveSet`
 * (presence) alone; replay asked only "is there a close edge before `t`",
 * with no staleness check. A session that started, sent one heartbeat, and
 * then went silent for 30 minutes with no terminal record read
 * `running=true` under the replay arm and `running=false` under live's own
 * flow-derived fallback (`flowLiveSessions`) at the identical instant.
 *
 * Now there is ONE algorithm, run over records up to `t` in both modes:
 *
 * 1. Presence (`liveSet`) — an OPTIONAL, purely ADDITIVE input. If it says
 *    the session is live, that's authoritative; it never subtracts. A
 *    replay caller always passes an empty set (there is no presence to
 *    read about a past day), so this branch is simply never true there.
 * 2. A close edge at or before `t` — the session is done, full stop.
 * 3. Otherwise, TTL-self-healing exactly like the live flow-derived
 *    fallback: the session must have started by `t`, and its most recent
 *    activity as of `t` must be within `FLOW_LIVE_TTL_MS`. This is what
 *    makes step 2's absence non-authoritative forever — an orphaned
 *    session with no terminal record ages out of "running" the same way in
 *    both modes, measured from `t` rather than `Date.now()` so a replay at
 *    a past instant gets the SAME answer a live viewer got at that instant.
 *
 * (#2125) `missionId` narrows the close-edge lookup only — presence has no
 * mission dimension to disambiguate (it's a bare session-id set, and
 * `dispatch.map`'s hosted seats never write to it at all — see
 * `liveSessionSet`'s own #2123 doc), so that collision risk, if any, lives
 * entirely in that separate, already-fixed gap, not here. */
export function sessionRunning(
  data: NormRecord[],
  liveSet: Set<string>,
  sid: string,
  t: number,
  missionId?: string,
): boolean {
  if (liveSet.has(sid)) return true;
  const close = sessionCloseEdge(data, sid, missionId);
  if (close && isAsOf(close, t)) return false;
  // (#2902 step 5) A call held by its budget is running, however long ago
  // its wait was announced (a day window's wait outlasts the TTL below).
  if (openBudgetWait(sessionRecords(data, sid), t)) return true;
  const own = recordsAsOf(sessionRecords(data, sid), t);
  if (!own.some((r) => r.action === ACTION.DispatchStart)) return false;
  const activityTimes = timesOf(own);
  const lastActivity = activityTimes.length ? Math.max(...activityTimes) : -Infinity;
  return t - lastActivity <= FLOW_LIVE_TTL_MS;
}

/** `statusVisual()` — viewer.html:1140-1145. Only `lbl` is consumed here —
 * `cls`/`pill` are CSS class names in legacy, invisible to `innerText`. */
export interface RunStatePredicates {
  open: boolean;
  errored: boolean;
  killed: boolean;
  clean: boolean;
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
 * (#2813) Map a lens's locally-derived predicates onto the CANONICAL
 * `RunStatus`. This is the only place the flow-record lenses are allowed to
 * decide a status, and it can only ever return one of the six the server
 * defines.
 *
 * The predicates themselves stay where they are — each lens derives them from
 * a different slice of the flow stream and they are not interchangeable. What
 * changes is that they now SELECT a status instead of inventing one.
 *
 * The old mapping, preserved exactly: `open` -> running; `errored` -> error
 * (with `killed` as a nuance WITHIN error, which is what the legacy
 * `killed ? "killed" : "errored"` meant — `killed` was never a peer of
 * `abandoned`); `clean` -> complete; anything else -> abandoned with no
 * ending recorded, which is what the legacy `"canceled"` described.
 */
export function runStateFrom(p: RunStatePredicates): RunState {
  if (p.open) return { status: "running", killed: false };
  if (p.errored) return { status: "error", killed: p.killed };
  if (p.clean) return { status: "complete", killed: false };
  return { status: "abandoned", killed: false, abandonReason: "noterminal" };
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

/** `flowLiveSessions()` — viewer.html:1343-1358. Flow-derived liveness
 * fallback for when Redis session-presence (`/fleet/sessions/live`) is
 * empty. `nowMs` is REAL wall-clock now (frozen via Playwright's clock in
 * the parity spec), not `tMax` — see the legacy comment this ports. */
export function flowLiveSessions(data: NormRecord[], nowMs: number, liveMode = true): Set<string> {
  // `if(!document.body.classList.contains('live-mode')) return new Set();`
  // (viewer.html:3378) — the gate this port dropped, because until #1800 P2
  // nothing here ever ran outside live mode. Without it a REPLAY derives
  // liveness from flow records and reads a past day's sessions as running
  // right now: cards go "dispatch in flight", the machine card counts
  // "N running" instead of the day's specialists, and timeline bars draw
  // yellow. Replay is presence-agnostic by construction.
  if (!liveMode) return new Set();
  const lastBySid = new Map<string, number>();
  const started = new Set<string>();
  const waited = new Set<string>();
  for (const r of data) {
    if (!r.session_id) continue;
    const prev = lastBySid.get(r.session_id);
    if (r.tMs !== null && (prev === undefined || r.tMs > prev)) lastBySid.set(r.session_id, r.tMs);
    if (r.action === ACTION.DispatchStart) started.add(r.session_id);
    if (r.action === ACTION.BudgetWait) waited.add(r.session_id);
  }
  const out = new Set<string>();
  // (#2902 step 5) A hosted call held by its budget has no start yet (its
  // gate runs before the bookends), and its one record can be hours old.
  for (const sid of waited) {
    if (openBudgetWait(sessionRecords(data, sid), nowMs)) out.add(sid);
  }
  for (const sid of started) {
    if (sessEnd(data, sid) || dispatchEnd(data, sid)) continue; // terminal/abandoned → not running
    if (nowMs - (lastBySid.get(sid) ?? 0) <= FLOW_LIVE_TTL_MS) out.add(sid);
  }
  return out;
}

/** `liveSessionSet()` — viewer.html:1373-1379, widened (#2123). Legacy (and
 * this port until now) treated Redis presence as all-or-nothing: ANY beat
 * anywhere in the fleet made the WHOLE presence set authoritative, and the
 * flow-derived fallback below was never even consulted. That was safe only
 * under an assumption that stopped holding once darkmux grew more than one
 * dispatch path: `darkmux:session-presence:<sid>` is refreshed by
 * `dispatch.internal`'s own container-heartbeat thread
 * (`crates/darkmux-crew/src/dispatch_internal.rs`) — the ONE writer, grep-
 * confirmed. A mission/review dispatch that fans out through `dispatch.map`
 * (hosted/remote probe + judge seats, no container, no heartbeat thread —
 * see `crates/darkmux-crew/src/step_kinds/builtins.rs::DispatchMapStepKind`)
 * never writes a beat AT ALL, for any of its sessions.
 *
 * On a Redis-enabled multi-machine fleet (`config.redis.enabled`, the
 * fleet-topology default — Studio hub + laptop peer), presence is near-never
 * EMPTY: the hub's own `dispatch.internal` work keeps `liveSessionIds`
 * non-zero pretty much continuously. Before this fix that non-zero-but-
 * elsewhere set silently WON over the flow-derived fallback, so a live
 * review mission's own sessions — never beaten, always absent from
 * presence — read as not-running on every machine in the fleet: the fleet
 * card's "0 running" and a machine card stuck on "idle" while LM Studio
 * burned tokens (#2123's reported symptom).
 *
 * The fix: UNION rather than either/or. Presence still answers instantly
 * and cheaply for whatever it DOES cover (`dispatch.internal` work); the
 * flow-derived heuristic — already correct and already exercised whenever
 * Redis is off entirely — fills in exactly the sessions presence has no
 * opinion about, rather than being shadowed by an unrelated beat elsewhere
 * in the fleet. `flowLiveSessions` already gates itself off in replay mode
 * (`liveMode=false` returns `new Set()`), so a replay's presence-agnostic
 * contract is unchanged by this — it still reads only from `liveSessionIds`,
 * which is itself always empty in replay (`useLiveSessionIds`'s `enabled`
 * gate). */
export function liveSessionSet(
  data: NormRecord[],
  liveSessionIds: Set<string>,
  nowMs: number,
  liveMode = true,
): Set<string> {
  const flowDerived = flowLiveSessions(data, nowMs, liveMode);
  if (!liveSessionIds.size) return flowDerived;
  if (!flowDerived.size) return liveSessionIds;
  return new Set([...liveSessionIds, ...flowDerived]);
}

/** `lastTs()` — viewer.html:1187. A session's last recorded activity —
 * where an orphan's timeline bar ends when it aged out of presence with no
 * close-edge (so the bar stops at its last sign of life, not at "now").
 * `missionId` (#2125) narrows to one mission's own activity — see
 * `dispatchRec`'s own doc; `undefined` preserves the exact prior behavior. */
export function lastTs(data: NormRecord[], sid: string, missionId?: string): number {
  let m = 0;
  for (const r of data) {
    if (r.session_id === sid && (missionId === undefined || r.mission_id === missionId) && r.tMs !== null && r.tMs > m) m = r.tMs;
  }
  return m;
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
