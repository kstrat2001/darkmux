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

/** `recKey()` — viewer.html:3390. Dedup key for the two-day fetch overlap. */
function recKey(r: FlowRecord): string {
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

/** `loadLiveWindow()` + the dispatch-action slice of `flowToRenderModel()` —
 * viewer.html:3497-3512 / 3161-3187. `yesterday`/`today` MUST be passed in
 * that fetch order (see the module doc above for why). */
export function buildFlowWindow(yesterday: FlowRecord[], today: FlowRecord[], nowMs: number): FlowRecord[] {
  const merged = [...yesterday, ...today]
    .filter((r): r is FlowRecord => !!r && !r._type)
    .map((r) => ({ ...r, action: normalizeAction(r.action) }));
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

/** `localMachineUid()` — viewer.html:2642-2644. Which uid IS this daemon,
 * for the nav-tab/deep-link entry into the machine page. */
export function localMachineUid(
  data: FlowRecord[],
  liveMachines: Map<string, PresenceBeat>,
  machineId: string | null | undefined,
): string | null {
  if (!machineId) return null;
  return machineUids(data, liveMachines).find((x) => nameOf(data, liveMachines, x) === machineId) ?? machineId;
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
export function flowLiveSessions(data: FlowRecord[], nowMs: number): Set<string> {
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
export function liveSessionSet(data: FlowRecord[], liveSessionIds: Set<string>, nowMs: number): Set<string> {
  if (liveSessionIds.size) return liveSessionIds;
  return flowLiveSessions(data, nowMs);
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
