import type { FlowRecord } from "../types/handwritten";

/** (#2877) Live token-rate scope — pure derivation from flow records
 * already fetched for a session; zero model work, matches CLAUDE.md's "the
 * observer must not join the observed" (no dispatch, no extra fetch).
 *
 * A `dispatch.turn.heartbeat` record's type-specific data lives under
 * `fields` on the wire (schema 1.6+) and under `payload` on older/synthetic
 * records — same alias `lib/turnGroups.ts::fields()` reads locally rather
 * than importing (it is unexported there too), so this module keeps its
 * own copy to match that existing convention. */
type Fields = Record<string, unknown>;

function fields(r: FlowRecord): Fields {
  return ((r as unknown as { fields?: Fields }).fields || (r as unknown as { payload?: Fields }).payload || {}) as Fields;
}

function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** Fallback chars-per-token used before a session has any billed usage to
 * measure a real ratio from (its first turn's heartbeats, prior to that
 * turn's `telemetry.tokens` record). Matches the runtime's own documented
 * ~4-chars-per-token proxy (`runtime/src/loop_runner.rs`). */
export const DEFAULT_CHARS_PER_TOKEN = 4;

/** A `dispatch.turn.heartbeat`, reduced to the two numbers a rate needs. */
export interface HeartbeatSample {
  /** Unix ms this sample was taken. */
  atMs: number;
  /** Generated chars so far (content + reasoning when available). */
  chars: number;
}

/** Every `dispatch.turn.heartbeat` in `records`, reduced to time-ordered
 * samples. Additive-field aware (#2877 flow-schema 1.55.0): prefers the new
 * `sampled_at_ms` (ms precision) + `generated_chars` (includes reasoning),
 * and falls back to the record's own whole-second `ts` + the pre-existing
 * `cumulative_chars` (answer text only) for a heartbeat forwarded by an
 * older runtime — so a session straddling an upgrade, or an old recorded
 * run, still estimates a rate rather than showing nothing. Never throws: a
 * heartbeat missing every usable field is simply skipped. */
export function heartbeatSamples(records: FlowRecord[]): HeartbeatSample[] {
  const out: HeartbeatSample[] = [];
  for (const r of records) {
    if (r.action !== "dispatch.turn.heartbeat") continue;
    const f = fields(r);
    const chars = num(f.generated_chars) ?? num(f.cumulative_chars);
    if (chars === null) continue;
    const atMs = num(f.sampled_at_ms) ?? Date.parse(r.ts);
    if (!Number.isFinite(atMs)) continue;
    out.push({ atMs, chars });
  }
  out.sort((a, b) => a.atMs - b.atMs);
  return out;
}

/** Δchars/Δms between two samples, as chars/sec. `null` on a non-positive
 * time delta or a char count that went backward — either is a degenerate
 * pair (clock skew, a session id reused / restarted) that must not be
 * drawn as a negative or infinite rate. */
export function charsPerSecond(prev: HeartbeatSample, next: HeartbeatSample): number | null {
  const dtMs = next.atMs - prev.atMs;
  if (dtMs <= 0) return null;
  const dChars = next.chars - prev.chars;
  if (dChars < 0) return null;
  return (dChars / dtMs) * 1000;
}

/** Measured chars-per-token for a session so far: total generated chars
 * (the latest heartbeat's count) over total BILLED completion tokens
 * (summed `telemetry.tokens` records) — the same ratio the runtime itself
 * computes per turn (`chars_per_token` on `model.streaming.end`, see
 * `runtime/src/trajectory.rs`), derived here from data already on the flow
 * stream instead of needing a THIRD host field to carry the runtime's
 * per-turn figure across — #2877's host-side scope is additive-only on the
 * heartbeat (`sampled_at_ms`, `generated_chars`). Falls back to
 * `DEFAULT_CHARS_PER_TOKEN` until the first turn's usage lands. */
export function measuredCharsPerToken(records: FlowRecord[]): number {
  // Paired PER TURN: `generated_chars` restarts every turn, and the turn in
  // flight has chars but no billed tokens yet. Dividing the largest chars
  // seen anywhere by the finished turns' tokens read a model generating ~50
  // tok/s as 11.
  const charsByTurn = new Map<unknown, number>();
  const tokensByTurn = new Map<unknown, number>();
  for (const r of records) {
    const f = fields(r);
    if (r.action === "dispatch.turn.heartbeat") {
      const c = num(f.generated_chars) ?? num(f.cumulative_chars);
      if (c !== null) charsByTurn.set(f.turn_seq, Math.max(charsByTurn.get(f.turn_seq) ?? 0, c));
    } else if (r.action === "telemetry.tokens") {
      const t = num(f.completion_tokens);
      if (t !== null) tokensByTurn.set(f.turn_seq, (tokensByTurn.get(f.turn_seq) ?? 0) + t);
    }
  }
  let chars = 0;
  let tokens = 0;
  for (const [turn, t] of tokensByTurn) {
    const c = charsByTurn.get(turn) ?? 0;
    // A short turn is a bad calibration: text generated after its last 2s
    // heartbeat is never seen, and tool-call arguments bill as tokens but
    // are not counted as chars (measured: 566 chars against 391 tokens).
    if (c < MIN_CALIBRATION_CHARS || t <= 0) continue;
    chars += c;
    tokens += t;
  }
  if (tokens <= 0 || chars <= 0) return DEFAULT_CHARS_PER_TOKEN;
  return chars / tokens;
}

/** The fewest chars a finished turn must have produced before its chars/token
 *  ratio is trusted over the default. */
export const MIN_CALIBRATION_CHARS = 2_000;

export interface TokenRateReading {
  tokensPerSec: number;
  /** The sample the reading is as-of — lets a caller judge freshness. */
  atMs: number;
  /** Always `true` today: an estimate from Δchars/Δms and a measured-or-
   * assumed chars-per-token ratio, not the endpoint's own billed rate
   * (which only lands once the turn completes). Named per the issue's
   * "label it an estimate until billed usage arrives". */
  estimate: true;
}

/** The CURRENT tok/s reading for one execution — the rate between its two
 * most recent heartbeats. `null` before there are two samples yet (a fresh
 * execution, or one still on its very first heartbeat) — the caller reads
 * `null` as "no reading yet", distinct from a genuine `0`. */
export function currentTokenRate(records: FlowRecord[]): TokenRateReading | null {
  const samples = heartbeatSamples(records);
  if (samples.length < 2) return null;
  const prev = samples[samples.length - 2];
  const next = samples[samples.length - 1];
  const cps = charsPerSecond(prev, next);
  if (cps === null) return null;
  const charsPerToken = measuredCharsPerToken(records);
  return { tokensPerSec: cps / charsPerToken, atMs: next.atMs, estimate: true };
}

/** How long with no fresh heartbeat before a live execution reads as
 * STALLED rather than merely between beats — the scope's "decays toward a
 * flat ring" state. Set above the heartbeat cadence itself
 * (`HEARTBEAT_MIN_INTERVAL`, 2s, `crates/darkmux-crew/src/
 * dispatch_internal.rs`) with margin, so landing exactly on the rate-limit
 * boundary never misreads as a stall. */
export const STALL_AFTER_MS = 5000;

/** Whether an execution's heartbeat stream has gone quiet — no sample
 * within `STALL_AFTER_MS` of `nowMs`. `false` (not stalled) when there are
 * no heartbeats at all yet: "hasn't started producing" is a different
 * state from "was producing, then stopped". */
export function isStalled(records: FlowRecord[], nowMs: number): boolean {
  const samples = heartbeatSamples(records);
  if (samples.length === 0) return false;
  return nowMs - samples[samples.length - 1].atMs > STALL_AFTER_MS;
}

/** Aggregate tok/s across several running executions — a fleet machine
 * card's total across its `runningSessionIds`. Sums each execution's
 * current reading; an execution with no reading yet (fresh, or stalled
 * past two heartbeats returning null) contributes 0 rather than being
 * dropped, since it is still counted in the "N running" figure alongside
 * it. Returns `null` only when NOT ONE execution has a reading, so the
 * caller can distinguish "genuinely 0 tok/s right now" reporting from
 * "nothing to report yet" (though today both render the same "0"). */
export function aggregateTokenRate(perExecutionRecords: FlowRecord[][]): number | null {
  let any = false;
  let total = 0;
  for (const recs of perExecutionRecords) {
    const reading = currentTokenRate(recs);
    if (reading) {
      total += reading.tokensPerSec;
      any = true;
    }
  }
  return any ? total : null;
}
