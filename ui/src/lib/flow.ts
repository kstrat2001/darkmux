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

import type { FlowRecord, PresenceBeat } from "../types/handwritten";

/** `LIVE_WINDOW_MS` — viewer.html:3374. The rolling live window `RAW` is
 * bounded to; also the "N records · last Nh" meta-line's hour figure. */
export const LIVE_WINDOW_MS = 24 * 60 * 60 * 1000;

/** `FLOW_LIVE_TTL_MS` — viewer.html:3342. How recent a session's last
 * record must be (vs wall-clock now) to count as "live" absent Redis
 * presence — see `flowLiveSessions` below. */
export const FLOW_LIVE_TTL_MS = 300 * 1000;

/** `RECENT_CAP` — viewer.html:1052. Default cap on the recent-runs list. */
export const RECENT_CAP = 20;

export const T = (s: string): number => Date.parse(s);

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

function normalizeAction(a: string | undefined): string | undefined {
  // flowToRenderModel() — viewer.html:3164-3170. Only the dispatch
  // lifecycle normalization matters for this lens (the turn/compaction
  // telemetry synthesis in the legacy function feeds OTHER lenses' log
  // stream, not the runs-list summary fields this port reads — see the
  // module doc's scope note in the packet report).
  if (a === "dispatch start") return "dispatch.start";
  if (a === "dispatch complete") return "dispatch.complete";
  if (a === "dispatch error") return "dispatch.error";
  return a;
}

/** `recKey()` — viewer.html:3390/3397. Dedup key for the two-day fetch
 * overlap AND (Packet 5) the live tail's SSE-append / reconcile-backstop
 * dedup (`SEEN_KEYS`, viewer.html:3396-3398) — exported so `useLiveTail`
 * can dedup against the exact same identity `buildFlowWindow` itself uses,
 * rather than inventing a second key shape that could silently drift from
 * this one. */
export function recKey(r: FlowRecord): string {
  return [
    r.ts,
    r.machine_uid || "",
    r.session_id || "",
    r.action || "",
    r.source || "",
    r.handle || "",
    r.level || "",
    r.stage || "",
    r.payload != null ? JSON.stringify(r.payload) : "",
  ].join("\x1f");
}

/** Parses a flow file's raw JSONL TEXT the way the daemon parses the
 * on-disk file the static demo commits a copy of — viewer.html:3899-3901
 * (the `flowSrc` branch): split on newlines, trim, drop empty lines,
 * `JSON.parse` each remaining line, drop any line that fails to parse.
 * Lenient by design, matching legacy exactly: a truncated last line or a
 * stray blank line must not fail the whole page, and the leading
 * `{"_type":"schema"}` header line parses FINE here — it is dropped later,
 * by `normalizeRecords`' `_type` filter, not by this function (#1801 — see
 * that function's own doc for why the two stay separate steps). */
export function parseFlowJsonl(text: string): FlowRecord[] {
  return text
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter(Boolean)
    .map((l): FlowRecord | null => {
      try {
        return JSON.parse(l) as FlowRecord;
      } catch {
        return null;
      }
    })
    .filter((r): r is FlowRecord => r !== null);
}

/** GETs a static playback source (`staticSource.ts::staticFlowSrc()`) and
 * parses it via `parseFlowJsonl` above — the static-build twin of
 * `GET /flow/<date>`, read directly by both `useRouteRecords` (the event
 * log's data) and `PlaybackLens` (the stage's data), each via the SAME
 * query key so they share one fetch and can never disagree about the file's
 * contents (#1801 — same "one source of truth" discipline this module's own
 * doc already establishes for `normalizeRecords`).
 *
 * Returns `[]` rather than throwing on a network failure, a 404, or an
 * empty file — matching legacy's own `catch(e){ RAW=[]; }` around the same
 * fetch. A static build with no committed flow file (or one not yet built)
 * is a valid, if empty, playback — never a crash and never a silent
 * fallback to the live route (see `route.ts`'s own doc for why the ROUTE
 * itself does not depend on this fetch succeeding). */
export async function fetchStaticFlowRecords(src: string): Promise<FlowRecord[]> {
  try {
    const res = await fetch(src);
    if (!res.ok) return [];
    const text = await res.text();
    return parseFlowJsonl(text);
  } catch {
    return [];
  }
}

/** `if(!injectedDate&&RAW.length)date=String(RAW[0].ts||"").slice(0,10)||date;`
 * — viewer.html:3902, the flowSrc branch's own date derivation. Takes the
 * RAW parsed array (file order, BEFORE `normalizeRecords` drops the
 * `{"_type":"schema"}` header line) — deliberately `records[0]`, not the
 * earliest `ts`, matching legacy's own un-sorted read exactly, quirk
 * included: a file whose first LINE is the schema header (no `ts` field)
 * yields `null` here the same way legacy's derivation falls through to
 * whatever `date` already defaulted to. Returns `null` on an empty array, an
 * unreachable file, or a first record with no usable `ts` so a caller
 * supplies its OWN placeholder rather than this function inventing one. */
export function firstRecordDate(records: FlowRecord[]): string | null {
  const ts = records[0]?.ts;
  if (!ts) return null;
  const date = String(ts).slice(0, 10);
  return date || null;
}

/** A `/flow/<date>` response body is EITHER a bare array or one of two
 * wrapper shapes — `loadLiveWindow()`, viewer.html:3502:
 * `Array.isArray(body)?body:(body.records||body.flow||[])`. The recorded
 * corpus is always a bare array; this stays loose for parity with the
 * legacy tolerance rather than assuming the shape never changes. Shared by
 * `useFlowWindow` (the day-fetch) and `useLiveTail` (the reconcile
 * backstop's `?since=` fetch, which hits the SAME `/flow/<date>` handler). */
export function asRecordArray(body: unknown): FlowRecord[] {
  if (Array.isArray(body)) return body as FlowRecord[];
  if (body && typeof body === "object") {
    const obj = body as { records?: FlowRecord[]; flow?: FlowRecord[] };
    return obj.records ?? obj.flow ?? [];
  }
  return [];
}

/** The tail-cache-side counterpart to `buildFlowWindow`'s own dedup+window
 * filter — used by `useLiveTail`'s reconcile backstop (viewer.html's
 * `reconcileLiveWindow`, 3758-3782) so repeated `?since=` polls with a
 * deliberate overlap window (`RECONCILE_OVERLAP_MS`) don't grow the tail
 * cache's stored array by the overlap on every single poll. `buildFlowWindow`
 * itself ALSO dedups the final merged (day-fetch + tail) result, so this
 * isn't required for display correctness — duplicates would render
 * correctly either way — it's what keeps the underlying cache entry bounded
 * across a long-lived tab, the same thing `applyLive()`'s RAW age-out prunes
 * for (viewer.html:3522-3539). */
export function mergeTailRecords(existing: FlowRecord[], incoming: FlowRecord[], cutMs: number): FlowRecord[] {
  const seen = new Set(existing.map(recKey));
  const merged = existing.slice();
  for (const r of incoming) {
    if (!r || !r.ts) continue;
    if (T(r.ts) < cutMs) continue;
    const k = recKey(r);
    if (seen.has(k)) continue;
    seen.add(k);
    merged.push(r);
  }
  return merged.filter((r) => T(r.ts) >= cutMs);
}

/** The RECORD-SHAPING half of `flowToRenderModel()` — viewer.html:3195-3204.
 * Drops non-record meta lines and normalizes the space-separated legacy
 * action spellings to their dotted forms. Deliberately does NOT window or
 * dedup: those belong to the LIVE two-day merge (`buildFlowWindow`, below),
 * not to reading a record set.
 *
 * (#1800 P2) Split out because a historical day must be shaped the same way
 * and windowed NOT AT ALL — legacy's playback boot is literally
 * `DATA=flowToRenderModel(RAW)` with no window step (viewer.html:3922).
 * Feeding a replayed day through `buildFlowWindow` instead would have
 * dropped every record older than 24h — i.e. the entire day — and running it
 * unshaped leaves the flow file's leading `{"_type":"schema"}` header in the
 * set, where it renders as a phantom "unknown" machine card, an
 * `Invalid Date other` log row, and a third timeline lane. Both were live
 * before this split. */
export function normalizeRecords(records: FlowRecord[]): FlowRecord[] {
  const shaped = records
    .filter((r): r is FlowRecord => !!r && !r._type)
    .map((r) => ({ ...r, action: normalizeAction(r.action) }));
  return [...shaped, ...perSessionRuntimeRecords(records)].sort((a, b) => T(a.ts) - T(b.ts));
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
 * temporal. `normalizeRecords` does the same, so the sort is not this
 * function's own concern.
 *
 * Reads from the RAW records deliberately — `dispatch.turn` needs no action
 * normalization (it has no space-separated legacy spelling), and taking the
 * pre-filter set keeps this independent of the shaping step's ordering. */
function perSessionRuntimeRecords(records: FlowRecord[]): FlowRecord[] {
  const perSession = new Map<string, { turns: number; ts: string; machineId?: string; machineUid?: string }>();
  for (const r of records) {
    if (!r || r._type || r.action !== "dispatch.turn" || !r.session_id) continue;
    const seq = (r.payload as { turn_seq?: number } | undefined)?.turn_seq ?? 0;
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
  return [...perSession.entries()].map(([sessionId, e]) => ({
    ts: e.ts,
    category: "telemetry",
    source: "runtime",
    machine_id: e.machineId,
    machine_uid: e.machineUid,
    session_id: sessionId,
    fields: { turns: e.turns },
  })) as FlowRecord[];
}

/** `loadLiveWindow()` + the dispatch-action slice of `flowToRenderModel()` —
 * viewer.html:3497-3512 / 3161-3187. `yesterday`/`today` MUST be passed in
 * that fetch order (see the module doc above for why). */
export function buildFlowWindow(yesterday: FlowRecord[], today: FlowRecord[], nowMs: number): FlowRecord[] {
  const merged = normalizeRecords([...yesterday, ...today]);
  const windowed = merged.filter((r) => T(r.ts) >= nowMs - LIVE_WINDOW_MS);
  const seen = new Set<string>();
  return windowed.filter((r) => {
    const k = recKey(r);
    if (seen.has(k)) return false;
    seen.add(k);
    return true;
  });
}

/** `recompute()`'s tMax — viewer.html:1040-1041. */
export function computeTMax(data: FlowRecord[]): number {
  const ts = data.map((r) => T(r.ts)).filter((n) => !Number.isNaN(n));
  return ts.length ? Math.max(...ts) : Date.now();
}

/** `recompute()`'s tMin — viewer.html:1051. Unused while `/next` was
 * live-only (the live timeline anchors on NOW and a fixed window); a REPLAY
 * spans `tMin..tMax`, which is what makes the axis describe the recorded day
 * rather than the last 24 hours of wall-clock. */
export function computeTMin(data: FlowRecord[]): number {
  const ts = data.map((r) => T(r.ts)).filter((n) => !Number.isNaN(n));
  return ts.length ? Math.min(...ts) : Date.now();
}

/** `uidOf()` — viewer.html:1107. */
export const uidOf = (r: FlowRecord): string => r.machine_uid || "unknown";

/** `nameOf()` — viewer.html:1112. */
export function nameOf(data: FlowRecord[], liveMachines: Map<string, PresenceBeat>, m: string): string {
  if (m === "unknown") return "unknown";
  const r = data.find((x) => uidOf(x) === m && x.machine_id);
  if (r) return r.machine_id as string;
  const b = liveMachines.get(m);
  return b?.display_name || m;
}

/** `machines()` — viewer.html:1123. */
export function machineUids(data: FlowRecord[], liveMachines: Map<string, PresenceBeat>): string[] {
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
 * `nameOf` has to answer with ONE name, so it picks the first it finds. That is
 * fine for a label and wrong for an identity test: asking "is the name I show
 * for this uid equal to the name specs reports" fails whenever those two
 * happen to be different aliases of the same machine. Identity questions ask
 * this instead, and get a yes if ANY known alias matches. */
export function machineNames(
  data: FlowRecord[],
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
 * there is no uid to find and the raw name is the best available handle. */
export function localMachineUid(
  data: FlowRecord[],
  liveMachines: Map<string, PresenceBeat>,
  machineId: string | null | undefined,
): string | null {
  if (!machineId) return null;
  return machineUids(data, liveMachines).find((x) => machineNames(data, liveMachines, x).has(machineId)) ?? machineId;
}

/** `sessionsOn()` — viewer.html:1124. */
export function sessionsOn(data: FlowRecord[], m: string): string[] {
  return [...new Set(data.filter((r) => uidOf(r) === m && r.session_id).map((r) => r.session_id as string))];
}

/** `dispatch()` — viewer.html:1125. */
export function dispatchRec(data: FlowRecord[], sid: string, act: string): FlowRecord | undefined {
  return data.find((r) => r.session_id === sid && r.action === "dispatch." + act);
}

/** `dispatchEnd()` — viewer.html:1131. */
export function dispatchEnd(data: FlowRecord[], sid: string): FlowRecord | undefined {
  return dispatchRec(data, sid, "complete") ?? dispatchRec(data, sid, "error");
}

/** `dispatchErrored()` — viewer.html:1132. */
export const dispatchErrored = (rec: FlowRecord | undefined): boolean => !!rec && rec.action === "dispatch.error";

/** `dispatchKilled()` — viewer.html:1133. Watchdog kill = exit 137. */
export const dispatchKilled = (rec: FlowRecord | undefined): boolean =>
  dispatchErrored(rec) && (rec?.payload as { exit_code?: number } | undefined)?.exit_code === 137;

/** `sessEnd()` — viewer.html:1149. */
export function sessEnd(data: FlowRecord[], sid: string): FlowRecord | undefined {
  return data.find((r) => r.session_id === sid && r.action === "session.end");
}

/** `sessionCloseEdge()` — viewer.html:1171-1175. The EARLIEST of the dispatch
 * terminal and the reconciler's `session.end`. Every "is this session done /
 * where does its bar end" decision goes through here rather than bare
 * `dispatchEnd`: a session whose ONLY terminal is `session.end` (abandoned,
 * hard-killed, shipped without a clean complete) must read as ENDED, not
 * in-flight to the playhead. */
export function sessionCloseEdge(data: FlowRecord[], sid: string): FlowRecord | undefined {
  const c = dispatchEnd(data, sid);
  const e = sessEnd(data, sid);
  if (c && e) return T(c.ts) <= T(e.ts) ? c : e;
  return c ?? e;
}

/** `sessionRunning()` — viewer.html:1183-1187. THE single source of truth for
 * "is this session in flight?", and the reason it takes `liveMode`:
 *
 * - **live** keys on PRESENCE (`liveSet`), which is TTL-self-healing — an
 *   orphaned session with no close-edge ages out of the live set on its own.
 * - **replay** keys on the durable CLOSE-EDGE relative to the playhead: the
 *   session was running iff nothing had closed it yet at time `t`.
 *
 * (#1800 P2) Before this, the port had only the live arm, because `/next` had
 * no historical route that reached it. A replay asking presence about a past
 * day is the "confidently wrong" failure `FleetLens`'s own doc names. */
export function sessionRunning(
  data: FlowRecord[],
  liveSet: Set<string>,
  sid: string,
  liveMode: boolean,
  t: number,
): boolean {
  if (liveMode) return liveSet.has(sid);
  const close = sessionCloseEdge(data, sid);
  return !(close && T(close.ts) <= t);
}

/** `statusVisual()` — viewer.html:1140-1145. Only `lbl` is consumed here —
 * `cls`/`pill` are CSS class names in legacy, invisible to `innerText`. */
export function statusLabel(args: { open: boolean; errored: boolean; killed: boolean; clean: boolean }): string {
  if (args.open) return "running";
  if (args.errored) return args.killed ? "killed" : "errored";
  if (args.clean) return "complete";
  return "canceled";
}

/** `machPresent()` — viewer.html:1321-1327. true=present, false=absent,
 * null=unknown (no evidence either way). */
export function machPresent(
  data: FlowRecord[],
  liveMachines: Map<string, PresenceBeat>,
  tMax: number,
  m: string,
): boolean | null {
  if (liveMachines.has(m)) return true;
  const edges = data
    .filter((r) => uidOf(r) === m && (r.action === "machine.online" || r.action === "machine.offline") && T(r.ts) <= tMax)
    .sort((a, b) => T(a.ts) - T(b.ts));
  if (!edges.length) return null;
  return edges[edges.length - 1].action === "machine.online";
}

/** `flowLiveSessions()` — viewer.html:1343-1358. Flow-derived liveness
 * fallback for when Redis session-presence (`/fleet/sessions/live`) is
 * empty. `nowMs` is REAL wall-clock now (frozen via Playwright's clock in
 * the parity spec), not `tMax` — see the legacy comment this ports. */
export function flowLiveSessions(data: FlowRecord[], nowMs: number, liveMode = true): Set<string> {
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
  for (const r of data) {
    if (!r.session_id) continue;
    const t = T(r.ts);
    const prev = lastBySid.get(r.session_id);
    if (prev === undefined || t > prev) lastBySid.set(r.session_id, t);
    if (r.action === "dispatch.start") started.add(r.session_id);
  }
  const out = new Set<string>();
  for (const sid of started) {
    if (sessEnd(data, sid) || dispatchEnd(data, sid)) continue; // terminal/abandoned → not running
    if (nowMs - (lastBySid.get(sid) ?? 0) <= FLOW_LIVE_TTL_MS) out.add(sid);
  }
  return out;
}

/** `liveSessionSet()` — viewer.html:1373-1379. Redis presence when it has
 * ANY beat (authoritative), else the flow-derived fallback. */
export function liveSessionSet(
  data: FlowRecord[],
  liveSessionIds: Set<string>,
  nowMs: number,
  liveMode = true,
): Set<string> {
  if (liveSessionIds.size) return liveSessionIds;
  return flowLiveSessions(data, nowMs, liveMode);
}

/** One row of the machine lens's runs list — the fields `recentRow()`
 * (viewer.html:1745) actually renders into its COLLAPSED `<summary>` (the
 * only state the parity harness's `innerText` extraction ever observes;
 * `<details>` boots closed and this lens never opens one — see
 * `tests/parity/lib/extract-lens.js`'s module doc). */
export interface MachineRunNode {
  lbl: string;
  closeTs: number | null;
  startTs: number | null;
  sid: string;
  handle: string;
  model: string;
  mission: string | null;
  donePayload: Record<string, unknown> | null;
}

/** The `sessionsOn(m).map(...)` body inside `renderMachine()` —
 * viewer.html:1914-1957, sorted the same way (closeTs, falling back to
 * startTs, descending — most-recent-first). */
export function buildMachineRuns(
  data: FlowRecord[],
  liveMachines: Map<string, PresenceBeat>,
  liveSessionIds: Set<string>,
  tMax: number,
  nowMs: number,
  m: string,
): MachineRunNode[] {
  const machAbsent = machPresent(data, liveMachines, tMax, m) === false;
  const liveSet = liveSessionSet(data, liveSessionIds, nowMs);
  const sids = sessionsOn(data, m);

  const nodes: MachineRunNode[] = sids.map((sid) => {
    const s = dispatchRec(data, sid, "start");
    const c = dispatchEnd(data, sid);
    const e = sessEnd(data, sid);
    const closeCands = [c ? T(c.ts) : null, e ? T(e.ts) : null].filter((x): x is number => x != null);
    const closeTs = closeCands.length ? Math.min(...closeCands) : null;
    const closedBy = closeTs != null && closeTs <= tMax;
    const cleanClose = !!c && T(c.ts) === closeTs && !dispatchErrored(c);
    const errClose = !!c && T(c.ts) === closeTs && dispatchErrored(c);
    const liveNow = liveSet.has(sid);
    const killed = dispatchKilled(c);
    const lbl = statusLabel({
      open: !closedBy && !machAbsent && (liveNow || closeTs != null),
      errored: closedBy && errClose,
      killed,
      clean: closedBy && cleanClose,
    });
    const startTs = s ? T(s.ts) : null;
    const donePayload = c?.payload ?? null;

    if (s) {
      return {
        lbl,
        closeTs,
        startTs,
        sid,
        handle: s.handle || sid,
        model: s.model || "?",
        mission: s.mission_id ?? null,
        donePayload,
      };
    }
    // Fallback: no dispatch.start — pull handle/model/mission from the
    // first record on this session; "no start" says the lifecycle wasn't
    // captured here (viewer.html:1944-1953).
    const first = data.find((r) => r.session_id === sid);
    const handle = first?.handle || `session ${sid}`;
    const model = first?.model || "?";
    const fbTs = closeTs ?? (first?.ts ? T(first.ts) : null);
    return {
      lbl: "no start",
      closeTs,
      startTs: fbTs,
      sid,
      handle,
      model,
      mission: first?.mission_id ?? null,
      donePayload,
    };
  });

  return nodes.sort((a, b) => (b.closeTs ?? b.startTs ?? 0) - (a.closeTs ?? a.startTs ?? 0));
}

/** Records on `m` with no `session_id` — the "unscoped records" teaser
 * (viewer.html:1977). */
export function looseRecords(data: FlowRecord[], m: string): FlowRecord[] {
  return data.filter((r) => uidOf(r) === m && !r.session_id);
}

/** `lastTs()` — viewer.html:1187. A session's last recorded activity —
 * where an orphan's timeline bar ends when it aged out of presence with no
 * close-edge (so the bar stops at its last sign of life, not at "now"). */
export function lastTs(data: FlowRecord[], sid: string): number {
  let m = 0;
  for (const r of data) {
    if (r.session_id === sid) {
      const t = T(r.ts);
      if (t > m) m = t;
    }
  }
  return m;
}

function compKey(sessionId: string | undefined, ts: string | undefined): string {
  return `${sessionId || ""}\x1f${ts || ""}`;
}

/** `flowToRenderModel()` — viewer.html:3171-3242. Normalizes a raw record
 * array (the `/flow-session/<id>` or `/flow-mission/<id>` "replay this
 * thing" payload — the session drill-in's data source, `lenses/session/
 * sessionRun.ts`) into the shape `runRegions()` reads:
 *
 * - action dotting (`normalizeAction`, already used by `buildFlowWindow`
 *   above — reused here, not re-derived)
 * - `fields` ALIASED from `payload` when a record carries the latter but
 *   not the former (schema 1.6+ carries type-specific data under `fields`
 *   on the wire; older/synthesized records only have `payload`) — this is
 *   the piece `buildFlowWindow` above does NOT do (that pipeline's own doc
 *   notes its normalization is deliberately narrower, scoped to what the
 *   machine lens's runs-list needs), so a session-view consumer reading
 *   `r.fields` uniformly needs THIS pass, not that one
 * - a `dispatch.compaction` record retagged as compaction telemetry, UNLESS
 *   a dedicated `telemetry.compaction` sibling already covers the same
 *   `(session_id, ts)` (the #1122 double-count guard)
 * - a category default (`work` absent a `source`, else `telemetry`)
 * - a SYNTHESIZED per-session "runtime" telemetry record carrying the max
 *   `dispatch.turn` `turn_seq` — the ONLY source for the session view's
 *   TURNS metric
 *
 * Sorted by ts at the end (the synthesized runtime records are appended out
 * of order, and the whole point of this pass is a temporally-ordered
 * array). NOT the same pipeline `buildFlowWindow` runs for the fleet/
 * machine lenses' live window — see that function's own doc for why the
 * two stay separate rather than one growing to cover both call sites'
 * needs. */
export function flowToRenderModel(records: FlowRecord[]): FlowRecord[] {
  const compTelemetryKeys = new Set<string>();
  for (const r of records) {
    if (r && r.action === "telemetry.compaction") {
      compTelemetryKeys.add(compKey(r.session_id, r.ts));
    }
  }

  const out: FlowRecord[] = records
    .filter((r): r is FlowRecord => !!r && !r._type)
    .map((r) => {
      const o: FlowRecord = { ...r, action: normalizeAction(r.action) };
      if (o.payload && !o.fields) o.fields = o.payload;
      if (o.action === "dispatch.compaction" && !compTelemetryKeys.has(compKey(o.session_id, o.ts))) {
        const p = (o.payload || {}) as { before_messages?: number; after_messages?: number };
        o.category = "telemetry";
        o.source = "compaction";
        o.fields = { from: p.before_messages || 0, to: p.after_messages || 0 };
      }
      if (!o.category) o.category = o.source ? "telemetry" : "work";
      return o;
    });

  const perSession = new Map<string, { turns: number; ts: string }>();
  for (const r of records) {
    if (r.action === "dispatch.turn" && r.session_id) {
      const seq = Number((r.payload as { turn_seq?: number } | undefined)?.turn_seq) || 0;
      const existing = perSession.get(r.session_id);
      if (existing) {
        existing.turns = Math.max(existing.turns, seq);
        existing.ts = r.ts;
      } else {
        perSession.set(r.session_id, { turns: seq, ts: r.ts });
      }
    }
  }
  for (const [sid, agg] of perSession) {
    out.push({ ts: agg.ts, category: "telemetry", source: "runtime", session_id: sid, fields: { turns: agg.turns } });
  }

  return out.sort((a, b) => T(a.ts) - T(b.ts));
}
