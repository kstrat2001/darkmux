import { ACTION, byTime, latestByTime, type NormRecord } from "./ingest";
import { recordsOfGroup, runIndex } from "./runRef";
import { DEFAULT_POLICY, isRunning, lifecycleAt, NO_PRESENCE, type LifecyclePolicy, type Presence } from "./lifecycle";

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
  // (#2863 review round 2, finding 1) `id` is set ONLY on a SYNTHESIZED
  // header — one with no real `dispatch.turn` record of its own, so `rec`
  // is borrowed from another row purely for its timestamp. Reusing that
  // row's own identity as the header's React key produced two elements
  // with the same key (the header and the row it borrowed `rec` from),
  // and the two co-highlighted on selection. `id` gives the synthesized
  // header an identity nothing else in the list can collide with; a REAL
  // header (whose `rec` genuinely IS the `dispatch.turn` record) leaves
  // this undefined and keys off `rec` exactly as before.
  | { kind: "turn"; rec: NormRecord; turn: TurnInfo; id?: string }
  | { kind: "rest"; rec: NormRecord }
  | { kind: "rec"; rec: NormRecord };

type Fields = Record<string, unknown>;

function fields(r: NormRecord): Fields {
  return ((r as unknown as { fields?: Fields }).fields || (r as unknown as { payload?: Fields }).payload || {}) as Fields;
}

function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** (#2863 review round 2, finding 2) Whether the run attempt `rec` belongs
 *  to has ended as of `asOf` by the one lifecycle (`lib/lifecycle.ts`):
 *  closed, gone stale, or superseded by a relaunch. It decides whether an
 *  unfinished turn's header says "did not finish" (the run ended; this turn
 *  has no closing record) or "in progress" (the run has not ended yet, so
 *  "did not finish" would be a claim about the future). Read per RUN: a
 *  session two missions share is two runs. */
function attemptEnded(all: readonly NormRecord[], rec: NormRecord, asOf: number, policy: LifecyclePolicy, presence: Presence): boolean {
  const group = runIndex(all).groupOf(rec);
  const attempt = group ? group.attempts.findIndex((a) => a.records.includes(rec)) : -1;
  if (!group || attempt < 0) return false;
  const run = recordsOfGroup(group, { sessionId: group.sessionId, missionId: group.missionId, attempt });
  return !isRunning(lifecycleAt(run, asOf, policy, presence));
}

/** Whether a list is one session's, with turns to group by. */
export function groupsByTurn(all: NormRecord[]): boolean {
  const sessions = new Set(all.map((r) => r.session_id).filter(Boolean));
  return sessions.size === 1 && all.some((r) => r.action === ACTION.DispatchTurn);
}

/** The instant a list is read at (by default its newest record's), the
 *  policy it is judged by (by default the built-in one) and the presence
 *  (by default none). */
function readClock(
  all: readonly NormRecord[],
  asOf: number | undefined,
  policy: LifecyclePolicy | undefined,
  presence: Presence | undefined,
): { asOf: number; policy: LifecyclePolicy; presence: Presence } {
  return { asOf: asOf ?? latestByTime(all)?.tMs ?? -Infinity, policy: policy ?? DEFAULT_POLICY, presence: presence ?? NO_PRESENCE };
}

/**
 * @param visible the rows to show, NEWEST FIRST (as the list renders them)
 * @param all every record the list was given, so a hidden record (a
 *   heartbeat, a context reading) can still inform a turn's header
 * @param asOfArg the instant the list is read at; by default the newest
 *   record's
 * @param policyArg the lifecycle policy a turn's run is judged by
 * @param presenceArg the sessions presence reports live at that instant
 */
export function turnItems(visible: NormRecord[], all: NormRecord[], asOfArg?: number, policyArg?: LifecyclePolicy, presenceArg?: Presence): TurnItem[] {
  const { asOf, policy, presence } = readClock(all, asOfArg, policyArg, presenceArg);
  if (!groupsByTurn(all)) return visible.map((rec) => ({ kind: "rec", rec }));

  const ordered = [...all].sort(byTime);
  // Keys are per ROLE EXECUTION, not the bare seq: each `dispatch start`
  // begins a new execution whose turns count from 1 again, and a session can
  // hold more than one (#2863 review). `null` = before any turn.
  let exec = 0;
  const key = (seq: number) => `${exec}:${seq}`;
  const turnOf = new Map<NormRecord, string | null>();
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
  // (#2863 review round 2, finding 8) The SEQ `current` is tracked at,
  // within the CURRENT execution only — reset alongside `current` on every
  // `dispatch start`. Guards the advance below against a record that names
  // an OLDER seq than the one already reached (a per-call `telemetry.tokens`
  // record for turn 3, delivered AFTER turn 4's checkpoint — measured
  // producer behavior: the per-call usage event can lag the checkpoint it
  // was accumulated under).
  let currentSeqNum: number | null = null;
  // (#2863 review, finding 3) A turn's SEQ, for a group whose only records
  // are ones that name their own `turn_seq` but never got a `dispatch.turn`
  // (a checkpoint before the turn that would have completed it) — used to
  // synthesize that group's header below.
  const seqForKey = new Map<string, number>();
  for (const r of ordered) {
    const f = fields(r);
    if (r.action === ACTION.DispatchStart) {
      exec++;
      current = null;
      currentSeqNum = null;
    }
    const seq = num(f.turn_seq);
    const own = seq === null ? null : key(seq);
    // (#2863 review, finding 3) ANY record naming its own `turn_seq` advances
    // `current` — not just `dispatch.turn`/`dispatch.reasoning`. A turn that
    // never got a `dispatch.turn` record (it left a `dispatch.checkpoint`
    // instead, then errored) still moved the run forward; a record with no
    // seq of its own that arrives after it (the terminal error) belongs
    // there, not filed under the last turn that DID complete.
    //
    // (#2863 review round 2, finding 8) Only FORWARD, within this
    // execution: a record naming a LOWER seq than the one already reached
    // must not pull `current` backward, or an un-seq'd record arriving
    // after it (the terminal error) would misfile under the stale turn
    // instead of the furthest one actually reached. The record still maps
    // to ITS OWN declared turn via `own` below regardless — this guard is
    // only about what `current` (the fallback for records with no seq of
    // their own) tracks.
    if (own !== null && seq !== null) {
      if (currentSeqNum === null || seq >= currentSeqNum) {
        current = own;
        currentSeqNum = seq;
      }
      seqForKey.set(own, seq);
    }
    turnOf.set(r, own ?? current);
    if (r.action === ACTION.DispatchTurnHeartbeat && own !== null && r.tMs !== null && !firstBeat.has(own)) {
      firstBeat.set(own, r.tMs);
    }
    if (r.action === ACTION.TelemetryTokens && own !== null) {
      const out = num(f.completion_tokens);
      const think = num(f.reasoning_tokens);
      if (out !== null) callOut.set(own, (callOut.get(own) ?? 0) + out);
      if (think !== null) callThink.set(own, (callThink.get(own) ?? 0) + think);
    }
    if (r.action === ACTION.TelemetryContext) {
      window = num(f.max) ?? window;
      threshold = num(f.threshold) ?? threshold;
    }
  }

  const info = (r: NormRecord): TurnInfo => {
    const f = fields(r);
    const seq = num(f.turn_seq) ?? 0;
    const k = turnOf.get(r) ?? "";
    const usage = (f.usage || {}) as Fields;
    const exact = num(f.generation_ms);
    const beat = firstBeat.get(k);
    const approxMs = beat !== undefined && r.tMs !== null ? r.tMs - beat : null;
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
    if (rec.action === ACTION.DispatchTurn) headers.set(t, { kind: "turn", rec, turn: info(rec) });
    else if (rec.action === ACTION.DispatchRest) (rests.get(t) ?? rests.set(t, []).get(t)!).push({ kind: "rest", rec });
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
        // (#2863 review round 2, finding 2) "did not finish" is a claim
        // about the PAST — the run ended and this turn has no closing
        // record. A checkpoint with no terminal record YET does not mean
        // the turn never will finish; it means the run is still going.
        const finished = attemptEnded(all, anchor, asOf, policy, presence);
        h = {
          kind: "turn",
          rec: anchor,
          // (#2863 review round 2, finding 1) `id` gives this synthesized
          // header its own identity — `t` is already unique per
          // execution+seq (see the `key()` closure above) and collides
          // with nothing else in the list, unlike `anchor`'s own recKey,
          // which is shared with a REAL row rendered elsewhere in this
          // same group.
          id: `synth:${t}`,
          turn: {
            seq: seqForKey.get(t)!,
            why: finished ? "did not finish" : "in progress",
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
