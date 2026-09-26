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
  let first = Infinity;
  let last = -Infinity;
  for (const r of live) {
    if (r.action !== "dispatch.turn.heartbeat") continue;
    const at = beatMs(r);
    if (at < first) first = at;
    if (at > last) last = at;
  }
  let durableEdges: Set<string> | null = null;
  const out: FlowRecord[] = [];
  for (const r of durable) {
    if (r.action === "dispatch.turn.heartbeat") {
      const at = beatMs(r);
      if (at >= first && at <= last) continue;
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

/** The overlay store. One per page; `liveStore` below is that one. */
export class LiveStore {
  private bySession = new Map<string, FlowRecord[]>();
  private lastSeen = new Map<string, number>();
  private utility: FlowRecord[] = [];
  private snap: LiveOverlay = EMPTY_OVERLAY;
  private listeners = new Set<() => void>();
  private version = 0;
  /** Render pacing: at most one notification per cadence (the samples' own
   *  `cadence_ms`), leading edge immediate, trailing edge scheduled. Every
   *  sample is kept in the snapshot either way; only how often subscribers
   *  are told is bounded, so a page watching several executions renders at
   *  the channel's cadence rather than once per sample per execution. */
  private cadenceMs = 250;
  private lastNotifyMs = -Infinity;
  private pending: ReturnType<typeof setTimeout> | null = null;

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
    if (rec.action === "dispatch.turn.heartbeat" && sid) {
      const list = [...(this.bySession.get(sid) ?? []), rec];
      this.bySession.set(sid, list.length > MAX_LIVE_PER_SESSION ? list.slice(-MAX_LIVE_PER_SESSION) : list);
      this.lastSeen.set(sid, nowMs);
    } else {
      this.utility = [...this.utility, rec].filter((r) => nowMs - Date.parse(r.ts) <= LIVE_UTILITY_TTL_MS).slice(-MAX_LIVE_PER_SESSION);
      if (sid) {
        this.bySession.set(sid, [...(this.bySession.get(sid) ?? []), rec].slice(-MAX_LIVE_PER_SESSION));
        this.lastSeen.set(sid, nowMs);
      }
    }
    this.prune(nowMs);
    this.publish();
    return true;
  }

  private prune(nowMs: number): void {
    for (const [sid, at] of this.lastSeen) {
      if (nowMs - at > LIVE_SESSION_TTL_MS) {
        this.lastSeen.delete(sid);
        this.bySession.delete(sid);
      }
    }
  }

  private publish(): void {
    this.version += 1;
    this.snap = { version: this.version, bySession: new Map(this.bySession), utility: this.utility };
    if (this.pending !== null) return;
    const wait = this.lastNotifyMs + this.cadenceMs - Date.now();
    if (wait <= 0) {
      this.notify();
    } else {
      this.pending = setTimeout(() => {
        this.pending = null;
        this.notify();
      }, wait);
    }
  }

  private notify(): void {
    this.lastNotifyMs = Date.now();
    for (const l of this.listeners) l();
  }

  snapshot = (): LiveOverlay => this.snap;

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };

  /** Test seam: forget everything. */
  reset(): void {
    if (this.pending !== null) clearTimeout(this.pending);
    this.pending = null;
    this.lastNotifyMs = -Infinity;
    this.bySession.clear();
    this.lastSeen.clear();
    this.utility = [];
    this.snap = EMPTY_OVERLAY;
  }
}

export const liveStore = new LiveStore();

const emptySubscribe = () => () => {};
const emptySnapshot = () => EMPTY_OVERLAY;

/** The live overlay, re-rendering the caller on every sample, while
 *  `enabled` (the live edge of a live route). Disabled, it is always
 *  `EMPTY_OVERLAY` and subscribes to nothing: playback, a scrubbed view and
 *  a static build never see a live sample. */
export function useLiveOverlay(enabled: boolean, store: LiveStore = liveStore): LiveOverlay {
  return useSyncExternalStore(enabled ? store.subscribe : emptySubscribe, enabled ? store.snapshot : emptySnapshot);
}
