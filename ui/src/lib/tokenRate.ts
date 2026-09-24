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
  /** The turn this sample belongs to, when the record says. `generated_chars`
   *  restarts every turn, so two samples from different turns never pair. */
  turn?: unknown;
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
    out.push({ atMs, chars, turn: f.turn_seq });
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
  const checkpointed = checkpointedTurns(records);
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
    // (#2886) A turn cut by a reasoning checkpoint streams its FULL text
    // into `generated_chars` (every continuation the checkpoint judged) but
    // bills only the final continuation's tokens — numerator and
    // denominator cover different spans, the same trap
    // `crates/darkmux-lab/src/lab/stats.rs`'s `billed_gen_fraction`/
    // `UNBILLED` exists to close on the lab side (that module reads
    // trajectory spans the viewer doesn't have; this is the flow-record
    // equivalent). Keyed on the `dispatch.checkpoint` record for this
    // turn_seq, never a ratio: measured, a checkpointed turn read 74,617
    // chars against 91 billed tokens (≈820 chars/token), a ratio no
    // heuristic threshold could distinguish from a genuinely terse turn.
    if (checkpointed.has(turn)) continue;
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

/** Every turn_seq with at least one CUTTING `dispatch.checkpoint` record —
 *  the harness's own record that the reasoning checkpoint forced this turn
 *  to conclude mid-stream (`crates/darkmux-crew/src/dispatch_internal.rs`'s
 *  `"dispatch.checkpoint" =>` handler forwards `turn_seq`/`verdict` on the
 *  payload; the runtime side, `runtime/src/loop_runner.rs`, sets
 *  `verdict: if degenerate { "conclude" } else { "continue" }` — `conclude`
 *  is the ONLY verdict that hands the model a forced prefill and ends the
 *  call early).
 *
 *  (#2886 pass 4, MUST — fresh-reviewer finding 1) A `continue` verdict did
 *  NOT cut anything: the checkpoint judged the turn mid-thought and let it
 *  keep going, so its `telemetry.tokens` bills the turn normally and it
 *  must stay in both the calibration and the average. Excluding EVERY
 *  checkpointed turn (the pre-pass-4 behavior) is the same class of bug
 *  this whole pass exists to fix — a real 6-turn run read 102 tok/s
 *  "avg · 5 of 6 turns" instead of ~210, because its one `continue`
 *  checkpoint (a genuinely fine, fully-billed turn) was thrown out too.
 *
 *  Shared by `measuredCharsPerToken` and `averageGenerationRate` (#2886) so
 *  the two derivations can't disagree on what counts as checkpointed. */
function checkpointedTurns(records: FlowRecord[]): Set<unknown> {
  const out = new Set<unknown>();
  for (const r of records) {
    if (r.action === "dispatch.checkpoint" && fields(r).verdict === "conclude") out.add(fields(r).turn_seq);
  }
  return out;
}

/** The fewest chars a finished turn must have produced before its chars/token
 *  ratio is trusted over the default. */
export const MIN_CALIBRATION_CHARS = 2_000;

/** A FINISHED run's generation rate reading: how many of the turns that
 *  paired a `generation_ms` with billed `completion_tokens` actually went
 *  into `tokensPerSec`, so the caller can label a partial average (the
 *  issue's "avg · 3 of 5 turns") instead of presenting it as unqualified. */
export interface GenerationRateReading {
  /** `null` when `totalTurns > 0` but every one of them was excluded as
   *  checkpointed — the caller must show "—", not fall back to a looser
   *  measurement, since NO turn here can be trusted (#2886). */
  tokensPerSec: number | null;
  /** Turns actually summed into `tokensPerSec` — excludes any turn with a
   *  `dispatch.checkpoint` record for its turn_seq. */
  billedTurns: number;
  /** Turns that paired a `generation_ms` with a billed `completion_tokens`
   *  entry, before the checkpoint exclusion. */
  totalTurns: number;
}

/** A FINISHED run's generation rate: billed completion tokens over the time
 *  the model spent generating (`dispatch.turn`'s `generation_ms`), paired by
 *  turn within each execution. Not the wall clock, which includes rests,
 *  tools and prompt reading (a real run read 45 tok/s over wall clock
 *  against ~80 over generation time). `null` when no turn carries
 *  `generation_ms` (a runtime older than flow schema 1.53).
 *
 * (#2886) A turn cut by a reasoning checkpoint bills only its final
 * continuation's tokens while `generation_ms` spans every continuation the
 * checkpoint judged — dividing the two together reads a model generating
 * ~150 tok/s as ~34. Excluded the same way `measuredCharsPerToken` excludes
 * it: keyed on a `dispatch.checkpoint` record for the turn_seq. */
export function averageGenerationRate(recordSets: FlowRecord[][]): GenerationRateReading | null {
  let tokens = 0;
  let ms = 0;
  let billedTurns = 0;
  let totalTurns = 0;
  for (const records of recordSets) {
    const genMs = new Map<unknown, number>();
    const tok = new Map<unknown, number>();
    const checkpointed = checkpointedTurns(records);
    for (const r of records) {
      const f = fields(r);
      if (r.action === "dispatch.turn") {
        const g = num(f.generation_ms);
        if (g !== null && g > 0) genMs.set(f.turn_seq, g);
      } else if (r.action === "telemetry.tokens") {
        const t = num(f.completion_tokens);
        if (t !== null) tok.set(f.turn_seq, (tok.get(f.turn_seq) ?? 0) + t);
      }
    }
    for (const [turn, g] of genMs) {
      const t = tok.get(turn);
      if (t == null) continue;
      totalTurns += 1;
      if (checkpointed.has(turn)) continue;
      billedTurns += 1;
      tokens += t;
      ms += g;
    }
  }
  if (totalTurns === 0) return null;
  return { tokensPerSec: billedTurns > 0 && ms > 0 ? tokens / (ms / 1000) : null, billedTurns, totalTurns };
}

export interface TokenRateReading {
  tokensPerSec: number;
  /** The sample the reading is as-of — lets a caller judge freshness. */
  atMs: number;
  /** Always `true` today: an estimate from Δchars/Δms and a measured-or-
   * assumed chars-per-token ratio, not the endpoint's own billed rate
   * (which only lands once the turn completes). Named per the issue's
   * "label it an estimate until billed usage arrives". */
  estimate: true;
  /** (#2885) `true` when this reading is NOT from the current turn's own
   *  two most recent heartbeats — the current turn has produced only one
   *  sample so far (or none), so the number is carried forward from the
   *  last turn (or earlier stretch) that DID produce a same-turn pair.
   *  Absent (not merely `false`) on a fresh reading, matching this file's
   *  existing convention for a flag that is meaningful only sometimes
   *  (`restSecondsLeft` on `LiveStateReading`). The caller renders a
   *  carried reading dimmed rather than as a fresh sample. */
  carried?: true;
}

/** The CURRENT tok/s reading for one execution — the rate between its two
 * most recent same-turn heartbeats. `null` only when NO same-turn pair
 * exists anywhere in `records` yet (a fresh execution, or one still on its
 * very first heartbeat ever) — the caller reads `null` as "no reading yet",
 * distinct from a genuine `0`.
 *
 * (#2885, "GEN lit but a flat ring and '—' at the start of every short
 * turn") A turn needs TWO heartbeats of its own before it has a rate, and on
 * a workload of short turns that can take several seconds — long enough for
 * the tile to read dead while the lamp says GEN. When the current turn
 * hasn't produced a pair yet, this falls back to `carriedTokenRate`, which
 * scans backward for the most recent turn that DID, per the issue's
 * "carry... the previous turn's, or the run's most recent reading".
 *
 * (#2886 pass 4, MUST — fresh-reviewer finding 3) A reasoning checkpoint
 * restarts `generated_chars` WITHIN the same turn_seq (real shapes measured:
 * 119,547 -> 1; 74,617 -> 3) — the current turn's own last two heartbeats
 * then read a negative Δchars and `charsPerSecond` returns `null`. That is
 * also a "nothing to read from THIS pair" case, same as the other two
 * branches above, so it falls back to the carry too instead of surfacing
 * `null` outright. */
export function currentTokenRate(records: FlowRecord[]): TokenRateReading | null {
  const samples = heartbeatSamples(records);
  if (samples.length < 2) return carriedTokenRate(records, samples);
  const prev = samples[samples.length - 2];
  const next = samples[samples.length - 1];
  // `generated_chars` restarts every turn: a new turn's first sample paired
  // with the previous turn's last spans the tool gap and read near 0.
  if (prev.turn !== undefined && next.turn !== undefined && prev.turn !== next.turn) return carriedTokenRate(records, samples);
  const cps = charsPerSecond(prev, next);
  if (cps === null) return carriedTokenRate(records, samples);
  const charsPerToken = measuredCharsPerToken(records);
  return { tokensPerSec: cps / charsPerToken, atMs: next.atMs, estimate: true };
}

/** (#2885) Scans backward from the end of `samples` for the most recent
 *  PRIOR same-turn pair — the last turn (or earlier stretch within the
 *  in-flight one) that did measure a rate — and returns it marked
 *  `carried: true`. `records`/`samples` cover the same set; `samples` is
 *  passed in rather than recomputed since every caller already has it.
 *  Returns `null` when no such pair exists anywhere (a genuinely fresh
 *  execution, or every pair so far spans only prompt reading), matching
 *  `currentTokenRate`'s pre-#2885 behavior for that case.
 *
 *  (#2886 pass 4, MUST — fresh-reviewer finding 2) A pair whose EARLIER
 *  sample reads `generated_chars: 0` is skipped, not carried. Every turn
 *  opens with a heartbeat at 0 chars sent before the first token, so that
 *  pair's span covers prompt reading (and whatever "still thinking" gap
 *  precedes the first token), not generation — carrying it read as a
 *  dimmed near-zero rate under a lit GEN lamp on the very next turn (real
 *  shape: turn 3 went 0 -> 4 chars over 13s). The scan keeps going past it
 *  for the most recent pair with real progress. */
function carriedTokenRate(records: FlowRecord[], samples: HeartbeatSample[]): TokenRateReading | null {
  for (let i = samples.length - 1; i > 0; i--) {
    const next = samples[i];
    const prev = samples[i - 1];
    if (prev.turn !== undefined && next.turn !== undefined && prev.turn !== next.turn) continue;
    if (prev.chars === 0) continue;
    const cps = charsPerSecond(prev, next);
    if (cps === null) continue;
    const charsPerToken = measuredCharsPerToken(records);
    return { tokensPerSec: cps / charsPerToken, atMs: next.atMs, estimate: true, carried: true };
  }
  return null;
}

/** How long with no fresh heartbeat before a live execution reads as
 * STALLED rather than merely between beats — the scope's "decays toward a
 * flat ring" state. Set above the heartbeat cadence itself
 * (`HEARTBEAT_MIN_INTERVAL`, 2s, `crates/darkmux-crew/src/
 * dispatch_internal.rs`) with margin, so landing exactly on the rate-limit
 * boundary never misreads as a stall.
 *
 * 30s, not the 5s it started at: a model writing a tool call can go 20+ s
 * with no stream chunks at all, because LM Studio buffers the arguments
 * rather than streaming them (measured: a turn that billed 2,105 tokens had
 * a 23s gap between heartbeats). At 5s that lit STALL during ordinary file
 * writing. A turn end, tool or rest is shown at once by its own record, so
 * this threshold only governs genuine silence. */
export const STALL_AFTER_MS = 30_000;

/** Whether an execution's heartbeat stream has gone quiet — no sample
 * within `STALL_AFTER_MS` of `nowMs`. `false` (not stalled) when there are
 * no heartbeats at all yet: "hasn't started producing" is a different
 * state from "was producing, then stopped". */
export function isStalled(records: FlowRecord[], nowMs: number): boolean {
  const samples = heartbeatSamples(records);
  if (samples.length === 0) return false;
  return nowMs - samples[samples.length - 1].atMs > STALL_AFTER_MS;
}

/** (#2877 pass 2, "is this resting? can't tell") The legible word a stopped
 * tube reads between heartbeats — the operator's phone note: a flat ring and
 * "—" is indistinguishable between a thermal rest, a model still doing
 * prompt processing on its first token, a tool executing, and a genuine
 * stall.
 *
 * `"generating"` is deliberately included (not just the four "quiet" states)
 * so a caller can derive its centered readout from ONE value: while
 * generating, show the number; otherwise, show the word — never a branch on
 * mode or on which caller is asking. */
export type LiveState = "generating" | "rest" | "prompt" | "tools" | "stalled";

export interface LiveStateReading {
  state: LiveState;
  /** Whole seconds remaining in the rest window this state was derived from
   * (`Math.ceil` of the ms remaining) — present only when `state ===
   * "rest"`. */
  restSecondsLeft?: number;
}

/** A record whose action marks a state transition, reduced to its ordering
 * key (`atMs`) and what state it implies. `dispatch.rest` carries its own
 * `ms` (the window a countdown is measured against); the other two markers
 * don't need one. */
interface StateMarker {
  atMs: number;
  kind: "prompt" | "tools" | "rest";
  restMs?: number;
}

/** The single state derivation both the run page (`sessionRun.ts`'s
 * `liveTokScope`) and the fleet card (`cards.ts`'s `buildFleetCard`) read —
 * one function, no live/playback branch, because both callers already pass
 * records already cut to the page's clock (`playhead ?? now`) the same way
 * `heartbeatSamples`/`isStalled` are called today. `nowMs` is still honored
 * defensively here (any record timestamped after it is ignored) so the
 * function is correct even when handed a caller's raw, un-cut array.
 *
 * Priority, most recent evidence wins:
 * 1. A heartbeat within `STALL_AFTER_MS` → `"generating"` (the existing
 *    `isStalled` threshold, reused rather than re-derived).
 * 2. Otherwise, the latest of {`dispatch.start`/`dispatch.turn` →
 *    `"prompt"`, `dispatch.tool` → `"tools"`, `dispatch.rest` with a real
 *    `ms` → `"rest"`} that is AT OR AFTER the last heartbeat (a marker
 *    older than the last heartbeat explains nothing — the heartbeat is
 *    still the most recent evidence, so it falls through to stalled below).
 *    A `dispatch.rest` marker further resolves to `"prompt"` once `nowMs`
 *    has moved past its `ms` window — the rest already elapsed as far as
 *    this reading can tell, and the runtime is presumed to be starting the
 *    next turn's prompt processing.
 * 3. A heartbeat exists but has gone stale, and nothing marks the gap →
 *    `"stalled"` (the pre-existing `isStalled` rule, unchanged).
 * 4. No heartbeat at all and no marker → `"prompt"` (a session that has
 *    started producing no evidence yet reads the same as right after
 *    `dispatch.start`). */
export function deriveLiveState(records: FlowRecord[], nowMs: number): LiveStateReading {
  // Cut ONCE, up front — every downstream read (`heartbeatSamples`,
  // `isStalled`, the marker scan) then agrees on "as of `nowMs`" instead of
  // each re-deriving its own future-safe view (or, worse, some doing it and
  // some not, which is what produced the bug this comment is guarding
  // against in review: `isStalled` on the UNCUT array would pick up a
  // heartbeat from beyond `nowMs` as its "last sample").
  const cut = records.filter((r) => {
    const atMs = Date.parse(r.ts);
    return !Number.isFinite(atMs) || atMs <= nowMs;
  });
  const beats = heartbeatSamples(cut);
  const lastBeatAt = beats.length ? beats[beats.length - 1].atMs : null;

  let marker: StateMarker | null = null;
  // `dispatch.tool` is emitted when a tool COMPLETES. A turn that ends with
  // N tool calls is TOOLS until N completions have arrived; after that the
  // model is reading the results, which is PROMPT. `pendingTools` is null
  // when the turn record does not say how many calls it made (older
  // records), and then a completion reads as TOOLS, the old behavior.
  let pendingTools: number | null = null;
  const ordered = [...cut].sort((a, b) => Date.parse(a.ts) - Date.parse(b.ts));
  for (const r of ordered) {
    const atMs = Date.parse(r.ts);
    if (!Number.isFinite(atMs)) continue;
    let m: StateMarker | null = null;
    if (r.action === "dispatch.start") {
      pendingTools = null;
      m = { atMs, kind: "prompt" };
    } else if (r.action === "dispatch.turn") {
      const calls = num(fields(r).tool_calls_count);
      pendingTools = calls;
      m = { atMs, kind: calls !== null && calls > 0 ? "tools" : "prompt" };
    } else if (r.action === "dispatch.tool") {
      if (pendingTools !== null && pendingTools > 0) pendingTools -= 1;
      m = { atMs, kind: pendingTools === null || pendingTools > 0 ? "tools" : "prompt" };
    } else if (r.action === "dispatch.rest") {
      // Only the completed-rest shape (`ms` present) counts — the
      // announce-only sibling (`pause: false, delay_ms`, no `ms`) is the
      // governor changing its PACING, not a rest (same filter
      // `sessionRun.ts`'s REST tiles already apply).
      const ms = num(fields(r).ms);
      if (ms !== null && ms > 0) m = { atMs, kind: "rest", restMs: ms };
    }
    if (m && (!marker || m.atMs >= marker.atMs)) marker = m;
  }

  // A marker's `atMs` comes from a record's own `ts`, which on the real
  // wire is WHOLE-SECOND (no `.SSS`); a heartbeat's `atMs` prefers the
  // ms-precise `sampled_at_ms`. A marker genuinely emitted in the SAME
  // second as the last heartbeat — a tool call dispatched right after the
  // turn that produced it — then truncates to a timestamp a few hundred ms
  // BELOW the heartbeat's precise one, reading as "older" even though it
  // followed it. Found via screenshot verification against the real
  // corpus: turn 12's `dispatch.tool` (ts truncates to :16.000) landed
  // right after its own last heartbeat (sampled_at_ms :16.141) and read as
  // "stalled" instead of "tools". Comparing against the heartbeat's OWN
  // second floor (not its precise ms) is the fix: two records in the same
  // second are treated as ties, and a tie goes to the marker, since a
  // marker only exists because SOMETHING happened after generation
  // stopped.
  const lastBeatSecondFloor = lastBeatAt === null ? null : Math.floor(lastBeatAt / 1000) * 1000;
  if (marker && (lastBeatSecondFloor === null || marker.atMs >= lastBeatSecondFloor)) {
    const found: StateMarker = marker;
    if (found.kind === "rest" && found.restMs != null) {
      const remaining = found.restMs - (nowMs - found.atMs);
      if (remaining > 0) return { state: "rest", restSecondsLeft: Math.ceil(remaining / 1000) };
      return { state: "prompt" };
    }
    return { state: found.kind };
  }

  // No marker since the last heartbeat: generating while it is fresh. This
  // is checked AFTER the markers, so a turn end or a rest recorded in the
  // seconds after the last heartbeat is shown at once rather than being
  // masked as "generating" until the stall threshold passes.
  //
  // (#2886 pass 4, CONSIDER-do-it — fresh-reviewer finding 4) A fresh
  // heartbeat reading `generated_chars: 0` is the turn's OWN opener,
  // emitted before the first token — the model is still reading the
  // prompt / thinking, not generating yet. Lighting GEN here would drive
  // the wave over a stretch that hasn't produced anything.
  if (lastBeatAt !== null && !isStalled(cut, nowMs)) {
    return beats[beats.length - 1].chars === 0 ? { state: "prompt" } : { state: "generating" };
  }
  return lastBeatAt !== null ? { state: "stalled" } : { state: "prompt" };
}

const isCloseEdge = (a: string | undefined): boolean =>
  a === "dispatch.complete" || a === "dispatch complete" || a === "dispatch.error" || a === "dispatch error" || a === "session.end";

/** The executions a live reading may come from, as of `nowMs`: not one that
 *  has already closed (its last rate and its last marker are history, and a
 *  finished execution's trailing `dispatch.turn` read as PROMPT forever),
 *  and not a mission's own run-grain session (its `dispatch start` is
 *  mission-sourced and bookends the whole run; it never generates, and its
 *  start read as PROMPT over a genuinely stalled execution). */
export function liveExecutions(perExecutionRecords: FlowRecord[][], nowMs: number): FlowRecord[][] {
  return perExecutionRecords.filter((recs) => {
    let runGrain = false;
    // A set is an execution only if it carries execution evidence. A
    // mission's lifecycle session (`mission start`, `phase start`) and its
    // scheduler task sessions (`step start`/`step complete`) carry none;
    // they read as PROMPT and outranked a real stall on every crawl.
    let evidence = false;
    for (const r of recs) {
      if (Date.parse(r.ts) > nowMs) continue;
      if (isCloseEdge(r.action)) return false;
      if (r.action === "dispatch.start" || r.action === "dispatch start") {
        evidence = true;
        if (r.source === "mission") runGrain = true;
      } else if (
        r.action === "dispatch.turn.heartbeat" ||
        r.action === "dispatch.turn" ||
        r.action === "dispatch.tool" ||
        r.action === "dispatch.rest"
      ) {
        evidence = true;
      }
    }
    return evidence && !runGrain;
  });
}

/** Priority order for `aggregateLiveState` — the most informative state
 * wins when several executions disagree. Generating always wins (matches
 * `aggregateTokenRate` summing whatever IS producing); among the quiet
 * states, `rest` is surfaced first since it is the one this pass exists to
 * make legible ("is this resting? can't tell" — the operator's own note),
 * then `tools` and `prompt` (both genuine, if less specific, evidence of
 * activity), and `stalled` only when nothing else explains the quiet. */
const STATE_PRIORITY: Record<LiveState, number> = { generating: 0, rest: 1, tools: 2, prompt: 3, stalled: 4 };

/** Aggregate state across several running executions (a fleet machine
 * card's `runningSessionIds`, or a mission's rolled-up sibling sessions) —
 * the best (lowest-`STATE_PRIORITY`) reading among them. `"prompt"` when
 * there are no executions at all, matching a fresh session's own default. */
export function aggregateLiveState(perExecutionRecords: FlowRecord[][], nowMs: number): LiveStateReading | null {
  let best: LiveStateReading | null = null;
  for (const recs of liveExecutions(perExecutionRecords, nowMs)) {
    const reading = deriveLiveState(recs, nowMs);
    if (!best || STATE_PRIORITY[reading.state] < STATE_PRIORITY[best.state]) best = reading;
  }
  // No live execution: no model is working (a mission between model steps,
  // or nothing running at all), so there is no state to claim.
  return best;
}

/** The short word (or `"rest Ns"`) a caller renders for every state except
 * `"generating"` — that one is deliberately NOT handled here, since its
 * center readout is the tok/s NUMBER, a decision that belongs to the
 * caller (which also knows whether it has a number to show). Both
 * placements (`SessionReplay.tsx`'s tile, `FleetLens.tsx`'s rate line) call
 * this for the same four words, so the wording can't drift between them. */
export function liveStateLabel(reading: LiveStateReading): string {
  switch (reading.state) {
    case "rest":
      return `rest ${reading.restSecondsLeft ?? 0}s`;
    case "prompt":
      // Not the bare word: the run page shows a "prompt · N chars"
      // disclosure just above the tile.
      return "reading prompt";
    case "tools":
      return "tools";
    case "stalled":
      return "stalled";
    case "generating":
      return "";
  }
}

export interface AggregatedTokenRate {
  tokensPerSec: number;
  /** (#2885) `true` when ANY contributing execution's reading is carried
   *  forward rather than freshly measured from its own two most recent
   *  same-turn heartbeats — the caller dims the number instead of showing
   *  it as a fresh sample. Kept OUT of `tokensPerSec` itself (no separate
   *  "carried total") since a caller only ever renders one dimmed/not-dimmed
   *  number for the whole reading, matching the run tile and fleet card's
   *  existing single-number renderers. */
  carried: boolean;
}

/** Aggregate tok/s across several running executions — a fleet machine
 * card's total across its `runningSessionIds`. Sums each execution's
 * current reading; an execution with no reading yet (fresh, or stalled
 * past two heartbeats returning null) contributes 0 rather than being
 * dropped, since it is still counted in the "N running" figure alongside
 * it. Returns `null` only when NOT ONE execution has a reading, so the
 * caller can distinguish "genuinely 0 tok/s right now" reporting from
 * "nothing to report yet" (though today both render the same "0"). */
export function aggregateTokenRate(perExecutionRecords: FlowRecord[][], nowMs: number): AggregatedTokenRate | null {
  let any = false;
  let total = 0;
  let carried = false;
  for (const recs of liveExecutions(perExecutionRecords, nowMs)) {
    // Only an execution that is generating right now contributes: a resting
    // or tool-running one still has a "last rate" from its last turn, and a
    // mission summed them (review: 350 tok/s with one execution at ~40).
    if (deriveLiveState(recs, nowMs).state !== "generating") continue;
    const reading = currentTokenRate(recs.filter((r) => !(Date.parse(r.ts) > nowMs)));
    if (reading) {
      total += reading.tokensPerSec;
      any = true;
      if (reading.carried) carried = true;
    }
  }
  return any ? { tokensPerSec: total, carried } : null;
}

/** The most recent `dispatch.turn.heartbeat` sample across several
 *  executions' records — `null` when none of them has ever produced one.
 *  (#2886 pass 4, finding 5) This is the timestamp
 *  `liveStateWhileConnected`'s half-open check compares the daemon's last
 *  confirmed contact against: the deadline a genuine stall claim needs
 *  contact evidence AFTER. */
export function lastHeartbeatMs(perExecutionRecords: FlowRecord[][]): number | null {
  let latest: number | null = null;
  for (const recs of perExecutionRecords) {
    const samples = heartbeatSamples(recs);
    if (!samples.length) continue;
    const at = samples[samples.length - 1].atMs;
    if (latest === null || at > latest) latest = at;
  }
  return latest;
}

/** (#2886 pass 3, "STALL while disconnected") Whether a `"stalled"` reading
 *  may be trusted, or must be treated as "we cannot say". A stall is a claim
 *  that the RUN has gone silent; when the PAGE has lost its own connection
 *  to the daemon, no new record could have arrived regardless of what the
 *  run is actually doing, so the same 30s silence means something
 *  different — the run might be generating right now. `connected` is read
 *  by the caller from the SAME liveness source the header renders
 *  (`hooks/useLiveTail.ts`'s `LiveTailStatus`) — this function adds no
 *  second liveness mechanism of its own; it only knows a boolean (and,
 *  below, a timestamp read from that same source).
 *
 *  Only `"stalled"` is downgraded. Every other state
 *  (`generating`/`rest`/`tools`/`prompt`) is read from records already in
 *  hand, which a lost connection does not retroactively invalidate — those
 *  may be a little stale, not actively WRONG the way a false STALL claim is.
 *
 *  Returns `null` on a downgrade — the exact "no live execution" reading
 *  both callers already render as every lamp off and no rate
 *  (`ScopeLamps`'s `state === null` branch, `TokenScope`'s `tone="none"`),
 *  so "no signal" needs no new visual vocabulary, only a caller that knows
 *  the connection is down.
 *
 *  (#2886 pass 4, do-it — fresh-reviewer finding 5, "half-open connection
 *  race") `STALL_AFTER_MS` (30s) can fire before the header's own watchdog
 *  notices the connection dropped (`LIVE_CONTACT_TIMEOUT_MS`, ~40s,
 *  `hooks/useLiveTail.ts`) — a half-open connection (the host slept, the
 *  path went away with no TCP reset) delivers no visible `error` event, so
 *  `connected` stays `true` for that whole gap while heartbeats have
 *  already gone silent. `halfOpen`, when provided, closes it: a stall is
 *  trusted only when the daemon has answered (`lastContactMs` — the SAME
 *  `useLiveTail` bookkeeping the header's watchdog itself reads, exposed
 *  rather than re-derived) AFTER the deadline the stalled execution's own
 *  last heartbeat (`lastHeartbeatMs`) missed — i.e. `lastContactMs >=
 *  lastHeartbeatMs + STALL_AFTER_MS`. Omitted entirely (the pre-pass-4
 *  2-arg call), this check is skipped and only the coarse `connected`
 *  boolean governs — every existing caller/test that doesn't pass it is
 *  unaffected. Also skipped when `lastHeartbeatMs` is itself `null` —
 *  nothing to compare a contact deadline against. */
export function liveStateWhileConnected(
  reading: LiveStateReading | null,
  connected: boolean,
  halfOpen?: { lastContactMs: number | null; lastHeartbeatMs: number | null },
): LiveStateReading | null {
  if (reading?.state !== "stalled") return reading;
  if (!connected) return null;
  if (halfOpen && halfOpen.lastHeartbeatMs != null) {
    const deadline = halfOpen.lastHeartbeatMs + STALL_AFTER_MS;
    if (halfOpen.lastContactMs == null || halfOpen.lastContactMs < deadline) return null;
  }
  return reading;
}
