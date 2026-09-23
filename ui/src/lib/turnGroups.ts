import type { FlowRecord } from "../types/handwritten";

/** (#2863) A run's event list, grouped by the turn each event belongs to.
 *
 * A TURN is the unit that happened: the model read the context, thought,
 * called tools. Listing its reasoning, its tool calls and the rest after it
 * as peers of the `turn` record itself made the turn a row among rows. Here
 * the turn record becomes the header its events sit under, and carries the
 * two things only a turn has: how long it took and how full the context was.
 *
 * Grouping applies only to ONE session's list that actually has turn
 * records (a run's page). A fleet or mission list mixes sessions, where
 * "turn 9" means nothing across them, so it stays flat.
 *
 * Assignment follows the order the host emits a turn's records, measured on
 * a live run: `dispatch.reasoning` (turn N), then `dispatch.turn` (N), then
 * N's `dispatch.tool` calls, then a `dispatch.rest`. A record that names its
 * own `turn_seq` uses it; any other takes the most recent turn before it in
 * time. */

export interface TurnInfo {
  seq: number;
  /** "answered" when the turn ended the run's work; otherwise its tool count. */
  why: string;
  /** Generation time. Exact when the host recorded it on the turn record
   * (`generation_ms`); otherwise approximated from whole-second timestamps
   * (first heartbeat of the turn to the turn record), marked `approx`. */
  durationMs: number | null;
  approx: boolean;
  inTok: number | null;
  outTok: number | null;
  thinkTok: number | null;
  /** The model's context window and compaction threshold, from the run's
   * `telemetry.context` records. `null` when none were recorded. */
  window: number | null;
  threshold: number | null;
}

export type TurnItem =
  | { kind: "turn"; rec: FlowRecord; turn: TurnInfo }
  | { kind: "rest"; rec: FlowRecord }
  | { kind: "rec"; rec: FlowRecord };

type Fields = Record<string, unknown>;

function fields(r: FlowRecord): Fields {
  return ((r as unknown as { fields?: Fields }).fields || (r as unknown as { payload?: Fields }).payload || {}) as Fields;
}

function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** Whether a list is one session's, with turns to group by. */
export function groupsByTurn(all: FlowRecord[]): boolean {
  const sessions = new Set(all.map((r) => r.session_id).filter(Boolean));
  return sessions.size === 1 && all.some((r) => r.action === "dispatch.turn");
}

/**
 * @param visible the rows to show, NEWEST FIRST (as the list renders them)
 * @param all every record the list was given, so a hidden record (a
 *   heartbeat, a context reading) can still inform a turn's header
 */
export function turnItems(visible: FlowRecord[], all: FlowRecord[]): TurnItem[] {
  if (!groupsByTurn(all)) return visible.map((rec) => ({ kind: "rec", rec }));

  const byTime = [...all].sort((a, b) => Date.parse(a.ts) - Date.parse(b.ts));
  const turnOf = new Map<FlowRecord, number | null>();
  const firstBeat = new Map<number, number>();
  let window: number | null = null;
  let threshold: number | null = null;
  let current: number | null = null;
  for (const r of byTime) {
    const f = fields(r);
    const own = num(f.turn_seq);
    if (r.action === "dispatch.turn" || r.action === "dispatch.reasoning") {
      if (own !== null) current = own;
    }
    turnOf.set(r, own ?? current);
    if (r.action === "dispatch.turn.heartbeat" && own !== null && !firstBeat.has(own)) {
      firstBeat.set(own, Date.parse(r.ts));
    }
    if (r.action === "telemetry.context") {
      window = num(f.max) ?? window;
      threshold = num(f.threshold) ?? threshold;
    }
  }

  const info = (r: FlowRecord): TurnInfo => {
    const f = fields(r);
    const seq = num(f.turn_seq) ?? 0;
    const usage = (f.usage || {}) as Fields;
    const exact = num(f.generation_ms);
    const beat = firstBeat.get(seq);
    const approxMs = beat !== undefined ? Date.parse(r.ts) - beat : null;
    const tools = num(f.tool_calls_count) ?? 0;
    return {
      seq,
      why: f.finish_reason === "stop" ? "answered" : `${tools} tool${tools === 1 ? "" : "s"}`,
      durationMs: exact ?? (approxMs !== null && approxMs >= 0 ? approxMs : null),
      approx: exact === null,
      inTok: num(usage.prompt_tokens),
      outTok: num(usage.completion_tokens),
      thinkTok: num(usage.reasoning_tokens),
      window,
      threshold,
    };
  };

  // Groups in newest-first order of their turns; within a group, the rows
  // keep the list's own newest-first order, with the turn record first as
  // the header and rests last, as the divider before the next (older) turn
  // is read.
  const groups = new Map<number | null, TurnItem[]>();
  const order: Array<number | null> = [];
  const headers = new Map<number | null, TurnItem>();
  const rests = new Map<number | null, TurnItem[]>();
  for (const rec of visible) {
    const t = turnOf.get(rec) ?? null;
    if (!groups.has(t)) {
      groups.set(t, []);
      order.push(t);
    }
    if (rec.action === "dispatch.turn") headers.set(t, { kind: "turn", rec, turn: info(rec) });
    else if (rec.action === "dispatch.rest") (rests.get(t) ?? rests.set(t, []).get(t)!).push({ kind: "rest", rec });
    else groups.get(t)!.push({ kind: "rec", rec });
  }
  const out: TurnItem[] = [];
  for (const t of order) {
    out.push(...(rests.get(t) ?? []));
    const h = headers.get(t);
    if (h) out.push(h);
    out.push(...groups.get(t)!);
  }
  return out;
}
