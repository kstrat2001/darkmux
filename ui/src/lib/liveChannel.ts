import { useSyncExternalStore } from "react";
import type { FlowRecord } from "../types/handwritten";
import { LIVE_UTILITY_END_ACTION, UTILITY_START_ACTION } from "./utilityJobs";

/**
 * (#2928) The LIVE channel, viewer side: sub-second model state and utility
 * job edges pushed by the local daemon as SSE `event: live` frames
 * (`crates/darkmux-serve/src/live_hub.rs`), never written to the flow log.
 *
 * Two channels, like a trading chart's ticks and candles. The durable
 * `dispatch.turn.heartbeat` records (one per 2 s) stay the record of what
 * happened and are all that playback, a reload or a peer machine's card ever
 * sees. The live samples are an in-memory overlay on top of them, held only
 * for executions that are running now and only on the page that received
 * them.
 *
 * Each live sample is turned into the SAME shape a durable record has (a
 * model sample becomes a heartbeat record, a utility edge a
 * `utility.start` / `utility.end` record), so every existing derivation
 * (rate, THINK/GEN, tool writing, compacting, the utility glyph) reads it
 * unchanged. The overlay is merged per consumer (`mergeLive`), never into
 * the page's record window, so counts, lists, token sums and the event log
 * never see a live sample.
 *
 * Precedence (`mergeLive`): live samples cover the span from the first to
 * the last live sample of an execution; a durable heartbeat inside that
 * span is dropped (it is one of the same runtime events, sampled coarser),
 * and one outside it is kept. So the page uses live samples while they
 * flow and falls back to durable heartbeats by itself when they stop (the
 * feed dropped, the daemon restarted, the page reloaded mid-run). A utility
 * edge the durable stream already carries (same job id, same edge) wins
 * over its live copy.
 */

/** The wire shape `darkmux_flow::live::LiveSample` serializes to. */
export interface LiveSampleWire {
  v: number;
  kind: "model" | "utility";
  session_id?: string;
  role?: string;
  model?: string;
  at_ms: number;
  cadence_ms: number;
  fields: Record<string, unknown>;
}

export const LIVE_WIRE_VERSION = 1;

/** Marks a record built from a live sample. Never set on a durable one. */
export interface LiveRecordMark {
  live?: true;
}

/** The overlay a consumer merges: per execution (model samples and that
 *  execution's compaction edges) and machine-wide utility edges (this
 *  machine's: the live channel is local-daemon only). */
export interface LiveOverlay {
  readonly version: number;
  readonly bySession: ReadonlyMap<string, readonly FlowRecord[]>;
  readonly utility: readonly FlowRecord[];
}

/** (#2928) What a replayed scope says on hover: playback has only the
 *  durable heartbeats, so fast transitions between them are not in it. */
export const REPLAY_GRANULARITY_NOTE =
  "Replayed from the recorded heartbeats, one every 2 s. Live, this scope follows the live channel's samples (every 250 ms by default), so short bursts show there and not here.";

export const EMPTY_OVERLAY: LiveOverlay = { version: 0, bySession: new Map(), utility: [] };

/** Samples kept per execution: the newest 64 (16 s at 250 ms). Rate and
 *  state read the last few; a longer history lives in durable heartbeats. */
export const MAX_LIVE_PER_SESSION = 64;
/** An execution with no live sample for this long leaves the overlay. */
export const LIVE_SESSION_TTL_MS = 60_000;
/** Utility edges are kept this long (a start past it has a stall bound the
 *  durable record carries anyway). */
export const LIVE_UTILITY_TTL_MS = 15 * 60_000;

function isoMs(ms: number): string {
  return new Date(ms).toISOString();
}

/** One wire sample as a record, or `null` for anything this build does not
 *  understand (an unknown version or kind, a malformed frame). */
export function liveSampleToRecord(raw: unknown): (FlowRecord & LiveRecordMark) | null {
  if (!raw || typeof raw !== "object") return null;
  const s = raw as Partial<LiveSampleWire>;
  if (s.v !== LIVE_WIRE_VERSION || typeof s.at_ms !== "number" || !Number.isFinite(s.at_ms)) return null;
  const fields = s.fields && typeof s.fields === "object" ? { ...s.fields } : {};
  if (s.kind === "model") {
    if (typeof s.session_id !== "string" || !s.session_id) return null;
    if (typeof fields.sampled_at_ms !== "number") fields.sampled_at_ms = s.at_ms;
    return {
      ts: isoMs(s.at_ms),
      action: "dispatch.turn.heartbeat",
      session_id: s.session_id,
      handle: s.role,
      model: s.model,
      payload: fields,
      live: true,
      live_cadence_ms: s.cadence_ms,
    } as unknown as FlowRecord & LiveRecordMark;
  }
  if (s.kind === "utility") {
    const edge = fields.event;
    if (edge !== "start" && edge !== "end") return null;
    delete fields.event;
    const serves = typeof fields.serves === "string" && fields.serves ? fields.serves : s.session_id;
    if (edge === "start" && typeof fields.started_at_ms !== "number") fields.started_at_ms = s.at_ms;
    if (edge === "end" && typeof fields.ended_at_ms !== "number") fields.ended_at_ms = s.at_ms;
    return {
      ts: isoMs(s.at_ms),
      action: edge === "start" ? UTILITY_START_ACTION : LIVE_UTILITY_END_ACTION,
      session_id: edge === "start" ? serves : s.session_id,
      handle: s.role,
      model: s.model,
      source: "utility",
      payload: fields,
      live: true,
    } as unknown as FlowRecord & LiveRecordMark;
  }
  return null;
}

type Fields = Record<string, unknown>;
function fieldsOf(r: FlowRecord): Fields {
  const x = r as unknown as { payload?: Fields; fields?: Fields };
  return x.payload ?? x.fields ?? {};
}

function beatMs(r: FlowRecord): number {
  const v = fieldsOf(r).sampled_at_ms;
  return typeof v === "number" && Number.isFinite(v) ? v : Date.parse(r.ts);
}

function isUtilityEdge(r: FlowRecord): boolean {
  return r.action === UTILITY_START_ACTION || r.action === LIVE_UTILITY_END_ACTION;
}

/** A durable record's utility edge key (`start:<job_id>` / `end:<job_id>`),
 *  when it has one. A usage record or `utility.error` carrying a `job_id` is
 *  that job's end. */
function durableEdgeKey(r: FlowRecord): string | null {
  const id = fieldsOf(r).job_id;
  if (typeof id !== "string" || !id) return null;
  if (r.action === UTILITY_START_ACTION) return `start:${id}`;
  return `end:${id}`;
}

function liveEdgeKey(r: FlowRecord): string | null {
  const id = fieldsOf(r).job_id;
  if (typeof id !== "string" || !id) return null;
  return r.action === UTILITY_START_ACTION ? `start:${id}` : `end:${id}`;
}

/** `durable` with the live records merged in, by the precedence in the
 *  module doc. Returns `durable` itself (same reference) when there is
 *  nothing live to merge, so an idle page does no extra work. */
export function mergeLive(durable: readonly FlowRecord[], live: readonly FlowRecord[] | undefined): FlowRecord[] {
  if (!live || live.length === 0) return durable as FlowRecord[];
  const spans = liveSpans(live);
  let durableEdges: Set<string> | null = null;
  const out: FlowRecord[] = [];
  for (const r of durable) {
    if (r.action === "dispatch.turn.heartbeat") {
      const at = beatMs(r);
      if (spans.some(([a, b]) => at >= a && at <= b)) continue;
    } else {
      const k = durableEdgeKey(r);
      if (k) (durableEdges ??= new Set()).add(k);
    }
    out.push(r);
  }
  for (const r of live) {
    if (isUtilityEdge(r)) {
      const k = liveEdgeKey(r);
      if (k && durableEdges?.has(k)) continue;
    }
    out.push(r);
  }
  return out;
}

/** (#2928 review, C1) The stretches the live samples actually cover: a gap
 *  longer than two cadences plus the host's 250 ms poll ends a stretch. A
 *  durable heartbeat is dropped only INSIDE a stretch; one in a hole (the
 *  feed dropped, the daemon restarted, the page lost its stream) stays. */
function liveSpans(live: readonly FlowRecord[]): [number, number][] {
  const beats = live
    .filter((r) => r.action === "dispatch.turn.heartbeat")
    .map((r) => ({ at: beatMs(r), gap: 2 * cadenceOf(r) + HOST_POLL_MS }))
    .sort((a, b) => a.at - b.at);
  const spans: [number, number][] = [];
  for (const b of beats) {
    const cur = spans[spans.length - 1];
    if (cur && b.at - cur[1] <= b.gap) cur[1] = b.at;
    else spans.push([b.at, b.at]);
  }
  return spans;
}

/** The host tailer's trajectory poll: the most a sample can lag its window. */
const HOST_POLL_MS = 250;

function cadenceOf(r: FlowRecord): number {
  const c = (r as unknown as { live_cadence_ms?: unknown }).live_cadence_ms;
  return typeof c === "number" && Number.isFinite(c) && c > 0 ? c : 250;
}

type Mode = "opening" | "thinking" | "visible" | "writing";

/** How long a frame drawn for a state that came and went between renders
 *  (C2) stays up before the next one: about two frames at 60 Hz. */
export const TRANSIENT_FRAME_MS = 32;
/** At most this many such frames per session per notification, and this
 *  many in one train overall: a flapping stream never queues an animation
 *  of its own, one busy session never crowds out another's, and a train
 *  always ends in the latest state. What the caps drop is counted
 *  (`debugStats`). */
export const MAX_TRANSIENT_FRAMES_PER_SESSION = 4;
export const MAX_TRANSIENT_TRAIN = 12;
/** How often idle entries are pruned while the store holds any (C9). */
export const LIVE_PRUNE_EVERY_MS = 5_000;

interface SessionMode {
  mode: Mode;
  turn: unknown;
  gen: number;
  vis: number;
}

function modeAfter(prev: SessionMode | undefined, f: Record<string, unknown>): Mode {
  if (typeof f.prompt_chars === "number") return "opening";
  if (typeof f.phase === "string") return "writing";
  const gen = typeof f.generated_chars === "number" ? f.generated_chars : 0;
  const vis = typeof f.cumulative_chars === "number" ? f.cumulative_chars : 0;
  if (!prev || prev.turn !== f.turn_seq) return vis > 0 ? "visible" : gen > 0 ? "thinking" : "opening";
  if (vis > prev.vis) return "visible";
  if (gen > prev.gen) return "thinking";
  return prev.mode;
}

/** The overlay store. One per page; `liveStore` below is that one.
 *
 *  Render pacing: subscribers are told at most once per cadence (the
 *  samples' own `cadence_ms`), leading edge immediate, trailing edge
 *  scheduled, so a page watching several executions renders at the
 *  channel's cadence rather than once per sample each. Every sample is in
 *  the data either way.
 *
 *  (#2928 review, C2) Pacing must not swallow a state: a model state (or a
 *  utility job) that began after the last notification and has already
 *  ended when the next sample arrives is drawn for one frame
 *  (`TRANSIENT_FRAME_MS`) from a snapshot taken just before it ended, then
 *  the latest state. Fast transitions are shown as they happen, never
 *  averaged away. */
export class LiveStore {
  private bySession = new Map<string, FlowRecord[]>();
  private lastSeen = new Map<string, number>();
  private utility: FlowRecord[] = [];
  private latest: LiveOverlay = EMPTY_OVERLAY;
  private frames: LiveOverlay[] = [];
  /** Frames kept per session in the current train, and the train's length. */
  private framesPerSession = new Map<string, number>();
  private trainLength = 0;
  private stats = { framesKept: 0, framesDropped: 0, longestTrain: 0 };
  private listeners = new Set<() => void>();
  private version = 0;
  private cadenceMs = 250;
  private lastNotifyMs = -Infinity;
  private pending: ReturnType<typeof setTimeout> | null = null;
  private frameTimer: ReturnType<typeof setTimeout> | null = null;
  private pruneTimer: ReturnType<typeof setTimeout> | null = null;
  private modes = new Map<string, SessionMode>();
  /** Sessions whose current mode began after the last notification. */
  private unrenderedMode = new Set<string>();
  /** Utility jobs whose start arrived after the last notification. */
  private unrenderedStart = new Set<string>();

  /** Ingest one SSE `live` frame's data. Returns whether it was used. */
  ingest(data: string, nowMs: number = Date.now()): boolean {
    let parsed: unknown;
    try {
      parsed = JSON.parse(data);
    } catch {
      return false;
    }
    const rec = liveSampleToRecord(parsed);
    if (!rec) return false;
    const cadence = (parsed as { cadence_ms?: unknown }).cadence_ms;
    if (typeof cadence === "number" && Number.isFinite(cadence)) this.cadenceMs = Math.min(1000, Math.max(50, cadence));
    const sid = rec.session_id;
    const f = fieldsOf(rec);
    if (rec.action === "dispatch.turn.heartbeat" && sid) {
      const prev = this.modes.get(sid);
      const mode = modeAfter(prev, f);
      if (prev && prev.mode !== mode && this.unrenderedMode.has(sid)) this.keepFrame(sid);
      if (!prev || prev.mode !== mode) this.unrenderedMode.add(sid);
      this.modes.set(sid, {
        mode,
        turn: f.turn_seq,
        gen: typeof f.generated_chars === "number" ? f.generated_chars : 0,
        vis: typeof f.cumulative_chars === "number" ? f.cumulative_chars : 0,
      });
      // (#2928 re-review) A refresh replaces the previous refresh of the
      // same state instead of piling up: a minute of silence would
      // otherwise push the very sample it refreshes out of the capped list.
      const prevList = this.bySession.get(sid) ?? [];
      const tail = prevList[prevList.length - 1];
      const isRefresh = typeof f.refreshed_at_ms === "number";
      const sameState = (r: FlowRecord | undefined) =>
        r !== undefined && typeof fieldsOf(r).refreshed_at_ms === "number" && fieldsOf(r).sampled_at_ms === f.sampled_at_ms && fieldsOf(r).turn_seq === f.turn_seq;
      const list = isRefresh && sameState(tail) ? [...prevList.slice(0, -1), rec] : [...prevList, rec];
      this.bySession.set(sid, list.length > MAX_LIVE_PER_SESSION ? list.slice(-MAX_LIVE_PER_SESSION) : list);
      this.lastSeen.set(sid, nowMs);
    } else {
      const jobId = typeof f.job_id === "string" ? f.job_id : null;
      if (rec.action === LIVE_UTILITY_END_ACTION && jobId !== null && this.unrenderedStart.has(jobId)) this.keepFrame(`utility:${jobId}`);
      if (rec.action === UTILITY_START_ACTION && jobId !== null) this.unrenderedStart.add(jobId);
      this.utility = [...this.utility, rec].filter((r) => nowMs - Date.parse(r.ts) <= LIVE_UTILITY_TTL_MS).slice(-MAX_LIVE_PER_SESSION);
      if (sid) {
        this.bySession.set(sid, [...(this.bySession.get(sid) ?? []), rec].slice(-MAX_LIVE_PER_SESSION));
        this.lastSeen.set(sid, nowMs);
      }
    }
    this.prune(nowMs);
    this.publish();
    this.schedulePrune();
    return true;
  }

  /** Keep the state as it is now (before this sample) as a frame to draw. */
  private keepFrame(key: string): void {
    const n = this.framesPerSession.get(key) ?? 0;
    if (n >= MAX_TRANSIENT_FRAMES_PER_SESSION || this.trainLength >= MAX_TRANSIENT_TRAIN) {
      this.stats.framesDropped += 1;
      return;
    }
    this.framesPerSession.set(key, n + 1);
    this.trainLength += 1;
    this.stats.framesKept += 1;
    this.stats.longestTrain = Math.max(this.stats.longestTrain, this.trainLength);
    this.frames.push({ version: ++this.version, bySession: new Map(this.bySession), utility: this.utility });
    this.showFrames();
  }

  /** (#2928 re-review, C-2) Transient-frame counters, for debugging (the
   *  dev build exposes them as `window.__darkmuxLive`); never in the UI. */
  debugStats(): { framesKept: number; framesDropped: number; longestTrain: number } {
    return { ...this.stats };
  }

  /** Test seam: the latest state, whatever frame is up. */
  latestForTest(): LiveOverlay {
    return this.latest;
  }

  private showFrames(): void {
    if (this.frameTimer !== null) return;
    const step = () => {
      if (this.frames.length === 0) {
        this.frameTimer = null;
        this.notify();
        return;
      }
      this.frameTimer = setTimeout(() => {
        this.frames.shift();
        step();
      }, TRANSIENT_FRAME_MS);
      for (const l of this.listeners) l();
    };
    step();
  }

  private prune(nowMs: number): boolean {
    let changed = false;
    for (const [sid, at] of this.lastSeen) {
      if (nowMs - at > LIVE_SESSION_TTL_MS) {
        this.lastSeen.delete(sid);
        this.bySession.delete(sid);
        this.modes.delete(sid);
        changed = true;
      }
    }
    const kept = this.utility.filter((r) => nowMs - Date.parse(r.ts) <= LIVE_UTILITY_TTL_MS);
    if (kept.length !== this.utility.length) {
      this.utility = kept;
      changed = true;
    }
    return changed;
  }

  /** (#2928 review, C9) While anything is held, prune on a timer too, so an
   *  execution that stopped sending leaves without waiting for another. */
  private schedulePrune(): void {
    if (this.pruneTimer !== null) return;
    this.pruneTimer = setTimeout(() => {
      this.pruneTimer = null;
      if (this.prune(Date.now())) this.publish();
      if (this.bySession.size > 0 || this.utility.length > 0) this.schedulePrune();
    }, LIVE_PRUNE_EVERY_MS);
  }

  private publish(): void {
    this.version += 1;
    this.latest = { version: this.version, bySession: new Map(this.bySession), utility: this.utility };
    if (this.pending !== null || this.frameTimer !== null) return;
    const wait = this.lastNotifyMs + this.cadenceMs - Date.now();
    if (wait <= 0) {
      this.notify();
    } else {
      this.pending = setTimeout(() => {
        this.pending = null;
        if (this.frameTimer === null) this.notify();
      }, wait);
    }
  }

  private notify(): void {
    this.lastNotifyMs = Date.now();
    this.unrenderedMode.clear();
    this.unrenderedStart.clear();
    this.framesPerSession.clear();
    this.trainLength = 0;
    for (const l of this.listeners) l();
  }

  /** What subscribers draw: a transient frame while one is up, else the
   *  latest state. */
  snapshot = (): LiveOverlay => this.frames[0] ?? this.latest;

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };

  /** Test seam: forget everything. */
  reset(): void {
    for (const t of [this.pending, this.frameTimer, this.pruneTimer]) if (t !== null) clearTimeout(t);
    this.pending = null;
    this.frameTimer = null;
    this.pruneTimer = null;
    this.lastNotifyMs = -Infinity;
    this.bySession.clear();
    this.lastSeen.clear();
    this.modes.clear();
    this.unrenderedMode.clear();
    this.unrenderedStart.clear();
    this.frames = [];
    this.framesPerSession.clear();
    this.trainLength = 0;
    this.utility = [];
    this.latest = EMPTY_OVERLAY;
  }
}

export const liveStore = new LiveStore();

// (#2928 re-review, C-2) Debug counters in a dev build only.
if (import.meta.env?.DEV && typeof window !== "undefined") {
  (window as unknown as { __darkmuxLive?: () => unknown }).__darkmuxLive = () => liveStore.debugStats();
}

const emptySubscribe = () => () => {};
const emptySnapshot = () => EMPTY_OVERLAY;

/** The live overlay, re-rendering the caller on every sample, while
 *  `enabled` (the live edge of a live route). Disabled, it is always
 *  `EMPTY_OVERLAY` and subscribes to nothing: playback, a scrubbed view and
 *  a static build never see a live sample. */
export function useLiveOverlay(enabled: boolean, store: LiveStore = liveStore): LiveOverlay {
  return useSyncExternalStore(enabled ? store.subscribe : emptySubscribe, enabled ? store.snapshot : emptySnapshot);
}
