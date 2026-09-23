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
  /** Model time (request sent to stream end, so prompt processing plus
   * generation). Exact when the host recorded it on the turn record
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
  // Keys are per ROLE EXECUTION, not the bare seq: each `dispatch start`
  // begins a new execution whose turns count from 1 again, and a session can
  // hold more than one (#2863 review). `null` = before any turn.
  let exec = 0;
  const key = (seq: number) => `${exec}:${seq}`;
  const turnOf = new Map<FlowRecord, string | null>();
  const firstBeat = new Map<string, number>();
  // Per-call usage by turn. A checkpointed turn takes several calls under
  // one seq, and the turn record's own `usage` is only the LAST call's, so
  // output and thinking are summed from these; input stays the last call's,
  // since the last prompt is the context the turn ended with.
  const callOut = new Map<string, number>();
  const callThink = new Map<string, number>();
  let window: number | null = null;
  let threshold: number | null = null;
  let current: string | null = null;
  // (#2863 review, finding 3) A turn's SEQ, for a group whose only records
  // are ones that name their own `turn_seq` but never got a `dispatch.turn`
  // (a checkpoint before the turn that would have completed it) — used to
  // synthesize that group's header below.
  const seqForKey = new Map<string, number>();
  for (const r of byTime) {
    const f = fields(r);
    if (r.action === "dispatch start" || r.action === "dispatch.start") {
      exec++;
      current = null;
    }
    const seq = num(f.turn_seq);
    const own = seq === null ? null : key(seq);
    // (#2863 review, finding 3) ANY record naming its own `turn_seq` advances
    // `current` — not just `dispatch.turn`/`dispatch.reasoning`. A turn that
    // never got a `dispatch.turn` record (it left a `dispatch.checkpoint`
    // instead, then errored) still moved the run forward; a record with no
    // seq of its own that arrives after it (the terminal error) belongs
    // there, not filed under the last turn that DID complete.
    if (own !== null) {
      current = own;
      if (seq !== null) seqForKey.set(own, seq);
    }
    turnOf.set(r, own ?? current);
    if (r.action === "dispatch.turn.heartbeat" && own !== null && !firstBeat.has(own)) {
      firstBeat.set(own, Date.parse(r.ts));
    }
    if (r.action === "telemetry.tokens" && own !== null) {
      const out = num(f.completion_tokens);
      const think = num(f.reasoning_tokens);
      if (out !== null) callOut.set(own, (callOut.get(own) ?? 0) + out);
      if (think !== null) callThink.set(own, (callThink.get(own) ?? 0) + think);
    }
    if (r.action === "telemetry.context") {
      window = num(f.max) ?? window;
      threshold = num(f.threshold) ?? threshold;
    }
  }

  const info = (r: FlowRecord): TurnInfo => {
    const f = fields(r);
    const seq = num(f.turn_seq) ?? 0;
    const k = turnOf.get(r) ?? "";
    const usage = (f.usage || {}) as Fields;
    const exact = num(f.generation_ms);
    const beat = firstBeat.get(k);
    const approxMs = beat !== undefined ? Date.parse(r.ts) - beat : null;
    const tools = num(f.tool_calls_count) ?? 0;
    return {
      seq,
      why: f.finish_reason === "stop" ? "answered" : `${tools} tool${tools === 1 ? "" : "s"}`,
      durationMs: exact ?? (approxMs !== null && approxMs >= 0 ? approxMs : null),
      approx: exact === null,
      inTok: num(usage.prompt_tokens),
      outTok: callOut.get(k) ?? num(usage.completion_tokens),
      thinkTok: callThink.get(k) ?? num(usage.reasoning_tokens),
      window,
      threshold,
    };
  };

  // Groups in newest-first order of their turns. Within a group: the turn's
  // rests come FIRST (a rest follows its turn in time, so newest-first puts
  // it above, as the divider between this turn and the newer one), then the
  // turn record as the header, then the turn's other rows in the list's own
  // newest-first order.
  const groups = new Map<string | null, TurnItem[]>();
  const order: Array<string | null> = [];
  const headers = new Map<string | null, TurnItem>();
  const rests = new Map<string | null, TurnItem[]>();
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
    let h = headers.get(t);
    // (#2863 review, finding 3) A group with a real turn number (it holds a
    // record naming its own `turn_seq`) but no `dispatch.turn` record — the
    // turn started and left evidence (a checkpoint, a reasoning note) but
    // never finished. Synthesize a header rather than leave the group
    // floating with no row explaining what it is. No duration or context
    // data unless the group's own records carry it (summed the same way a
    // real turn's is, from `telemetry.tokens`); a turn that never completed
    // has no `usage` to read an input count from.
    if (!h && t !== null && seqForKey.has(t)) {
      const anchor = groups.get(t)?.[0]?.rec ?? rests.get(t)?.[0]?.rec;
      if (anchor) {
        h = {
          kind: "turn",
          rec: anchor,
          turn: {
            seq: seqForKey.get(t)!,
            why: "did not finish",
            durationMs: null,
            approx: true,
            inTok: null,
            outTok: callOut.get(t) ?? null,
            thinkTok: callThink.get(t) ?? null,
            window,
            threshold,
          },
        };
      }
    }
    if (h) out.push(h);
    out.push(...groups.get(t)!);
  }
  return out;
}
