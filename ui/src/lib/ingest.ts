/**
 * The viewer's one boundary for flow records.
 *
 * Every record the viewer reads enters here, from every route: a day file,
 * the SSE tail and its reconcile backstop, a session or mission slice, the
 * committed static flow file, a lab run's event feed, and the live channel's
 * samples. `ingest` parses each one ONCE:
 *
 * - `action`, `level`, `category`, `stage`, `tier` and `source` become opaque tags
 *   (`Tag`): the text the wire carried, typed so that the only thing they
 *   can be compared with is a constant from `ACTION`/`LEVEL`/`CATEGORY`/
 *   `STAGE`/`TIER`/`SOURCE`. A spelling this build does not know keeps its text and
 *   equals no constant.
 * - `ts` is parsed once into `tMs`, under the bad-timestamp policy below.
 *
 * After this module, viewer code matches on those typed fields. Comparing
 * one to a string literal does not compile; `ingest.boundary.test.ts` is a
 * second net for the shapes the type system cannot see (`Object.is`, a cast
 * through `unknown`), and for any record `ts` parsed outside this module.
 *
 * Bad-timestamp policy. A record whose `ts` is missing or does not parse has
 * `tMs === null`, and every surface treats it the same way:
 *
 * 1. It is inside every time range. `recordsAsOf` and `recordsSince` keep it
 *    whatever the cut, so a malformed record is visible and wrong-looking,
 *    never silently dropped (a terminal with a bad clock still closes its
 *    run).
 * 2. It contributes nothing to time arithmetic: it moves no minimum,
 *    maximum, span, rate or "last activity". A pick by time
 *    (`latestByTime`, `earliestByTime`) takes it only when no timed
 *    candidate exists, so a malformed record can always be outvoted.
 * 3. It sorts after every timed record (`byTime`), in arrival order.
 */

import type { FlowRecord } from "../types/generated/FlowRecord";
import type { Category } from "../types/generated/Category";
import type { ExecutionGrainAction } from "../types/generated/ExecutionGrainAction";
import type { FlowAction } from "../types/generated/FlowAction";
import type { DispatchEndPayload } from "../types/generated/DispatchEndPayload";
import type { FlowPayloads } from "../types/generated/FlowPayloads";
import type { FlowSource } from "../types/generated/FlowSource";
import type { Level } from "../types/generated/Level";
import type { Stage } from "../types/generated/Stage";
import type { Tier } from "../types/generated/Tier";
import { isPlainObject } from "./guards";

/** An action the live channel synthesizes and the daemon never writes: a
 *  utility job's end as a live sample delivers it (`lib/liveChannel.ts`).
 *  Its durable twin is the job's usage record or `utility.error`. */
// flow-action-guard:allow — a live-only action the daemon never writes
type LiveOnlyAction = "utility.end";

/** Every action spelling this build knows. */
export type Action = FlowAction | LiveOnlyAction;

declare const tagBrand: unique symbol;

/** A parsed record field, OPAQUE to the type system: its runtime value is
 * the text the wire carried, but its type is not `string`. So comparing it
 * to a string literal is a type error (TS2367 on `===`, TS2678 on `case`),
 * and so is every string method, `.includes`/`.has` on a `string[]`, a regex
 * `.test`, or using it as an index. The only values it can equal are the
 * constants below (`ACTION.DispatchStart`, `CATEGORY.Telemetry`, ...), and
 * the only way to read its text is `tagText`, for display and search.
 *
 * A spelling this build does not know keeps its text (the event log still
 * shows it, a search still finds it) and equals no constant: for an action
 * that is Rust's `FlowAction::Other`, for the closed fields it is their
 * `#[serde(other)] Unknown` arm. */
export interface Tag<K extends string, W extends string> {
  readonly [tagBrand]: { readonly kind: K; readonly wire: W };
}

export type NormAction = Tag<"action", string>;
type NormLevel = Tag<"level", string>;
type NormCategory = Tag<"category", string>;
type NormStage = Tag<"stage", string>;
type NormTier = Tag<"tier", string>;
export type NormSource = Tag<"source", string>;

declare const normBrand: unique symbol;

/** A flow record that has passed through `ingest`. Lenses and libs accept
 *  this type only; a raw `FlowRecord` does not satisfy it. */
export interface NormRecord extends Omit<FlowRecord, "action" | "level" | "category" | "stage" | "tier" | "source" | "payload" | "_type"> {
  /** The wire `payload`, read through `payloadOf` as its action's type. */
  payload?: Record<string, unknown>;
  /** `ts` parsed once; `null` when it is missing or does not parse. */
  readonly tMs: number | null;
  /** (#3017) The hub's receive order for this record: the Redis stream id
   *  (`<ms>-<seq>`) the daemon stamped as `hub_id`, as one comparable number
   *  (see {@link parseHubId}). `null` for a record that never passed through
   *  the hub (a local-only day-file line, a static replay). Cross-machine
   *  ordering reads this, never `tMs`, which is each writer's own clock. */
  readonly hub?: number | null;
  /** `payload`, aliased by the render model for records that only carry the
   *  one spelling. Added by the viewer; the wire's `FlowRecord` has `payload`. */
  fields?: Record<string, unknown>;
  /** A human title for the mission, stamped only by the demo's importer onto
   *  the committed playback file. No daemon writes it. */
  mission_title?: string;
  /** The `owner/repo#pr` a demo review mission reviewed; demo-only, like
   *  `mission_title`. */
  mission_reviewed?: string;
  action?: NormAction;
  level?: NormLevel;
  category?: NormCategory;
  stage?: NormStage;
  tier?: NormTier;
  source?: NormSource;
  readonly [normBrand]: true;
}

/** The text a tagged field carries: for display, search and dedup keys,
 *  never for deciding anything (compare against the constants instead). */
export function tagText(v: Tag<string, string> | undefined): string {
  return v === undefined ? "" : (v as unknown as string);
}

/** A record's payload read as the type of its own action: `undefined` when
 *  the record is of another action or carries none. The wire type is the
 *  writer's: an archived record written before a field existed lacks it, and
 *  one whose payload is not its action's type (`UnreadPayload` in Rust) reads
 *  as whatever JSON it holds. */
export function payloadOf<W extends keyof FlowPayloads>(
  rec: { readonly action?: NormAction; readonly payload?: Record<string, unknown> } | null | undefined,
  action: Tag<"action", W>,
): FlowPayloads[W] | undefined {
  return rec?.action === (action as unknown as NormAction) ? (rec.payload as FlowPayloads[W] | undefined) : undefined;
}

/** The step a record's payload names, for the actions whose payload type has a
 *  `step_id` (most of them): the one cross-action read of a payload. */
export function stepIdOf(rec: { readonly payload?: Record<string, unknown> } | null | undefined): string | undefined {
  const p = anyPayload(rec);
  return p && "step_id" in p && typeof p.step_id === "string" ? p.step_id : undefined;
}

/** A record's payload as the union of every action's payload type: what a
 *  reader that asks one question of many actions (which turn, which step) is
 *  honestly holding. Narrow with `in`; ask `payloadOf` when the action is
 *  known. */
export function anyPayload(rec: { readonly payload?: Record<string, unknown> } | null | undefined): FlowPayloads[keyof FlowPayloads] | undefined {
  const p = rec?.payload;
  return p && typeof p === "object" ? (p as FlowPayloads[keyof FlowPayloads]) : undefined;
}

/** The payload of a role execution's terminal record: `dispatch.complete` and
 *  `dispatch.error` share one type, so a reader of "how it ended" asks once. */
export function endPayloadOf(rec: { readonly action?: NormAction; readonly payload?: Record<string, unknown> } | null | undefined): DispatchEndPayload | undefined {
  return payloadOf(rec, ACTION.DispatchComplete) ?? payloadOf(rec, ACTION.DispatchError);
}

/** A constant table's wire strings, typed as tags of kind `K`. The one place
 *  a wire string becomes a tag besides `ingestRecord`. */
function tags<K extends string>() {
  return <const O extends Record<string, string>>(wires: O): { readonly [P in keyof O]: Tag<K, O[P]> } =>
    wires as unknown as { readonly [P in keyof O]: Tag<K, O[P]> };
}

const ACTION_WIRE = {
  AuditWriteFailed: "audit.write_failed",
  BatteryPauseUnsupported: "battery.pause_unsupported",
  BudgetWarn: "budget.warn",
  BudgetWait: "budget.wait",
  BudgetResume: "budget.resume",
  BudgetStop: "budget.stop",
  DispatchStart: "dispatch.start",
  DispatchComplete: "dispatch.complete",
  DispatchError: "dispatch.error",
  DispatchTurn: "dispatch.turn",
  DispatchTurnHeartbeat: "dispatch.turn.heartbeat",
  DispatchTool: "dispatch.tool",
  DispatchCompaction: "dispatch.compaction",
  DispatchCheckpoint: "dispatch.checkpoint",
  DispatchReasoning: "dispatch.reasoning",
  DispatchFeedbackInjected: "dispatch.feedback.injected",
  DispatchRest: "dispatch.rest",
  DispatchDegeneracyWarning: "dispatch.degeneracy.warning",
  DispatchWorkdirGitUnavailable: "dispatch.workdir_git_unavailable",
  DispatchRoute: "dispatch.route",
  GhVerbExecuted: "gh.verb.executed",
  HookFired: "hook.fired",
  HookFailed: "hook.failed",
  HookDryRun: "hook.dry_run",
  MachineOnline: "machine.online",
  MachineOffline: "machine.offline",
  MachineTelemetry: "machine.telemetry",
  MachineThermal: "machine.thermal",
  MachineBattery: "machine.battery",
  MachineBatteryHealth: "machine.battery_health",
  MissionStart: "mission.start",
  MissionClose: "mission.close",
  MissionAbort: "mission.abort",
  MissionGrow: "mission.grow",
  MissionDebriefPrompt: "mission.debrief.prompt",
  MissionRunFinalize: "mission.run.finalize",
  MissionRunAbort: "mission.run.abort",
  OperatorNote: "operator.note",
  OperatorCatch: "operator.catch",
  PhaseStart: "phase.start",
  PhaseComplete: "phase.complete",
  PhaseAbandon: "phase.abandon",
  PhaseIdAmbiguous: "phase.id_ambiguous",
  PhaseReviewBegin: "phase.review.begin",
  PhaseReviewAborted: "phase.review.aborted",
  PhaseReviewDispatch: "phase.review.dispatch",
  PhaseReviewFailed: "phase.review.failed",
  PhaseReviewVerdict: "phase.review.verdict",
  RadioRoute: "radio.route",
  RunStart: "run.start",
  RunComplete: "run.complete",
  RunError: "run.error",
  SessionEnd: "session.end",
  StepStart: "step.start",
  StepComplete: "step.complete",
  StepError: "step.error",
  StepResult: "step.result",
  StepTiming: "step.timing",
  StepSeatUnresolved: "step.seat_unresolved",
  StreamError: "stream.error",
  TelemetryTokens: "telemetry.tokens",
  TelemetryDetector: "telemetry.detector",
  TelemetryContext: "telemetry.context",
  TelemetryCompaction: "telemetry.compaction",
  TelemetryRuntime: "telemetry.runtime",
  TelemetryLms: "telemetry.lms",
  TierDecision: "tier.decision",
  ThermalStopUnresolved: "thermal.stop_unresolved",
  ThermalTier5Eject: "thermal.tier5_eject",
  ThermalTier5EjectFailed: "thermal.tier5_eject_failed",
  UtilityStart: "utility.start",
  UtilityError: "utility.error",
  // flow-action-guard:allow — a live-only action the daemon never writes
  UtilityEnd: "utility.end",
} as const satisfies Record<string, Action>;

const LEVEL_WIRE = {
  Error: "error",
  Warn: "warn",
  Info: "info",
  Debug: "debug",
  Trace: "trace",
  Unknown: "unknown",
} as const satisfies Record<string, Level>;

const CATEGORY_WIRE = {
  Work: "work",
  Machinery: "machinery",
  Audit: "audit",
  Review: "review",
  Telemetry: "telemetry",
  Unknown: "unknown",
} as const satisfies Record<string, Category>;

const STAGE_WIRE = {
  Scope: "scope",
  Dispatch: "dispatch",
  Review: "review",
  Ship: "ship",
  Debrief: "debrief",
  TierDecision: "tier-decision",
  Unknown: "unknown",
} as const satisfies Record<string, Stage>;

const TIER_WIRE = {
  Operator: "operator",
  Frontier: "frontier",
  Darkmux: "darkmux",
  Unknown: "unknown",
} as const satisfies Record<string, Tier>;

const SOURCE_WIRE = {
  CrewDispatch: "crew_dispatch",
  Scheduler: "scheduler",
  PhaseLifecycle: "phase_lifecycle",
  MissionLifecycle: "mission_lifecycle",
  PhaseReview: "phase_review",
  MissionDebrief: "mission_debrief",
  HostSampler: "host_sampler",
  PresenceReconciler: "presence_reconciler",
  CmdGateAudit: "cmd_gate_audit",
  Hook: "hook",
  Host: "host",
  Detector: "detector",
  Runtime: "runtime",
  Tokens: "tokens",
  Context: "context",
  Compaction: "compaction",
  Lms: "lms",
  Thermal: "thermal",
  Battery: "battery",
  Budget: "budget",
  Utility: "utility",
  Orchestrator: "orchestrator",
  Adjudication: "adjudication",
  Manual: "manual",
  Frontier: "frontier",
  Unknown: "unknown",
} as const satisfies Record<string, FlowSource>;

/** Every action by name, named as `darkmux_flow::action::FlowAction` names
 *  its variants. Call sites compare against these; a literal will not
 *  typecheck. */
export const ACTION = tags<"action">()(ACTION_WIRE);
export const LEVEL = tags<"level">()(LEVEL_WIRE);
export const CATEGORY = tags<"category">()(CATEGORY_WIRE);
export const STAGE = tags<"stage">()(STAGE_WIRE);
export const TIER = tags<"tier">()(TIER_WIRE);
export const SOURCE = tags<"source">()(SOURCE_WIRE);

type ValuesOf<O> = O[keyof O];
type Covers<U, O> = [Exclude<U, ValuesOf<O>>] extends [never] ? true : false;
type Assert<T extends true> = T;
/** Compile-time proof that every member of each generated union has a name
 *  above: one `Assert` per union, so a variant ts-rs adds without a name
 *  here fails to typecheck on its own line. Exported only so the compiler
 *  keeps it; nothing imports it.
 *  @public */
export type EveryVariantNamed = [
  Assert<Covers<Action, typeof ACTION_WIRE>>,
  Assert<Covers<Level, typeof LEVEL_WIRE>>,
  Assert<Covers<Category, typeof CATEGORY_WIRE>>,
  Assert<Covers<Stage, typeof STAGE_WIRE>>,
  Assert<Covers<Tier, typeof TIER_WIRE>>,
  Assert<Covers<FlowSource, typeof SOURCE_WIRE>>,
];

const KNOWN_ACTIONS: ReadonlySet<string> = new Set(Object.values(ACTION_WIRE));

/** Whether an action is one this build knows (`ACTION`). The one test of it;
 *  the vocabulary-skew tripwire counts the records that fail it. An action a
 *  release retired is not known: an archive's record of one reads as skew
 *  (#3036), and a surface that still reads one does so by its other fields
 *  (`telemetry.process` host samples, by `category` and `source`). */
export function isKnownAction(a: NormAction | undefined): boolean {
  return a !== undefined && KNOWN_ACTIONS.has(tagText(a));
}

/** How many of `records` carry an action this build does not name: the
 *  vocabulary-skew count the event log states beside its totals. */
export function unknownActionCount(records: readonly NormRecord[]): number {
  let n = 0;
  for (const r of records) if (r.action !== undefined && !isKnownAction(r.action)) n++;
  return n;
}

const warnedActions = new Set<string>();

/** Says once, on the console, that an action spelling reached the viewer
 *  unnamed: the loud half of the skew count, for whoever opens devtools
 *  rather than reads the chip. */
function warnUnknownAction(text: string): void {
  if (warnedActions.has(text)) return;
  warnedActions.add(text);
  console.warn(`darkmux viewer: flow action "${text}" is not in this build's vocabulary; records carrying it are shown but not interpreted.`);
}

/** A wire value as a tag, keeping its text whatever it is. */
function parseTag<K extends string>(v: unknown): Tag<K, string> | undefined {
  if (v === undefined || v === null) return undefined;
  return String(v) as unknown as Tag<K, string>;
}

function parseTs(ts: unknown): number | null {
  if (typeof ts !== "string" || !ts) return null;
  const t = Date.parse(ts);
  return Number.isFinite(t) ? t : null;
}

/** A hub stream id (`<ms>-<seq>`) as one number: `ms * 1024 + seq`, so ids
 *  compare as the hub assigned them. The stream's sequence restarts every
 *  millisecond and would have to exceed 1023 writes inside one millisecond to
 *  collide with the next one; the product stays below 2^53 until the year
 *  2255. `null` for anything that is not an id. */
export function parseHubId(id: unknown): number | null {
  if (typeof id !== "string") return null;
  const m = /^(\d+)-(\d+)$/.exec(id);
  if (!m) return null;
  const n = Number(m[1]) * 1024 + Math.min(Number(m[2]), 1023);
  return Number.isSafeInteger(n) ? n : null;
}

/** One raw value as a record, or `null` for anything that is not one (a
 *  non-object, or the `{"_type":"schema"}` header every flow file leads
 *  with). */
export function ingestRecord(raw: unknown): NormRecord | null {
  if (!isPlainObject(raw) || raw._type != null) return null;
  const out: Record<string, unknown> = { ...raw, tMs: parseTs(raw.ts), hub: parseHubId(raw.hub_id) };
  for (const key of TAGGED_FIELDS) assignTyped(out, key, parseTag(raw[key]));
  const action = out.action as NormAction | undefined;
  if (action !== undefined && !isKnownAction(action)) warnUnknownAction(tagText(action));
  return out as unknown as NormRecord;
}

const TAGGED_FIELDS = ["action", "level", "category", "stage", "tier", "source"] as const;

function assignTyped(out: Record<string, unknown>, key: string, v: unknown): void {
  if (v === undefined) delete out[key];
  else out[key] = v;
}

/** One result per body object: a query cache hands every reader the same
 *  body until it refetches, so a lens that re-reads it on each render (the
 *  mission graph folds on every tail append) pays for the parse once and
 *  gets the same array, which the per-window caches downstream key on. */
const bodyCache = new WeakMap<object, NormRecord[]>();

/** A response body as records. Accepts the daemon's `{records}` envelope, a
 *  bare array (a static file's lines) or the legacy `{flow}` wrapper; anything
 *  else is no records. Order is kept. The same body object always yields the same
 *  array, so a caller must not mutate it. */
export function ingest(body: unknown): NormRecord[] {
  if (typeof body !== "object" || body === null) return [];
  const cached = bodyCache.get(body);
  if (cached) return cached;
  const rows = Array.isArray(body) ? body : isPlainObject(body) ? (body.records ?? body.flow) : undefined;
  const out: NormRecord[] = [];
  if (Array.isArray(rows)) {
    for (const raw of rows) {
      const r = ingestRecord(raw);
      if (r) out.push(r);
    }
  }
  bodyCache.set(body, out);
  return out;
}

/** A flow file's JSONL text as records: one JSON value per non-empty line,
 *  a line that does not parse dropped rather than failing the file. */
export function ingestJsonl(text: string): NormRecord[] {
  const rows: unknown[] = [];
  for (const line of text.split(/\r?\n/)) {
    const l = line.trim();
    if (!l) continue;
    try {
      rows.push(JSON.parse(l));
    } catch {
      // A truncated last line or a stray fragment must not fail the page.
    }
  }
  return ingest(rows);
}

// ─── action predicates that need the text itself ──────────────────────────

/** Every action that is a record OF a role execution: the viewer's copy of
 *  `darkmux_flow::FlowAction::grain`, keyed by the union the Rust list
 *  generates so a drift is a type error. */
const EXECUTION_GRAIN_WIRE: { readonly [W in ExecutionGrainAction]: true } = {
  "budget.warn": true,
  "budget.wait": true,
  "budget.resume": true,
  "budget.stop": true,
  "dispatch.start": true,
  "dispatch.complete": true,
  "dispatch.error": true,
  "dispatch.turn": true,
  "dispatch.turn.heartbeat": true,
  "dispatch.tool": true,
  "dispatch.compaction": true,
  "dispatch.checkpoint": true,
  "dispatch.reasoning": true,
  "dispatch.feedback.injected": true,
  "dispatch.rest": true,
  "dispatch.degeneracy.warning": true,
  "dispatch.workdir_git_unavailable": true,
  "telemetry.tokens": true,
  "telemetry.detector": true,
  "telemetry.context": true,
  "telemetry.compaction": true,
  "telemetry.runtime": true,
  "telemetry.lms": true,
};

/** Whether an action is a record of a role execution. */
export const isExecutionAction = (a: NormAction | undefined): boolean => a !== undefined && Object.hasOwn(EXECUTION_GRAIN_WIRE, tagText(a));

/** The key the token and run counts group a record under: the execution it
 *  names. A record written before 4.0 names none, and none is invented for
 *  it (#3036: ingest stamps nothing); the counts key it by its session and
 *  mission instead (a bare session id is not an identity in a pre-4.0
 *  archive: a task session named only its task, so the same id recurs across
 *  unrelated runs, #2690/#2709), or, with no session, by the record itself
 *  (its time, handle and machine). The same fallback as the Rust usage fold's
 *  `usage_sum::execution_key`. */
export function executionOf(r: NormRecord): string {
  if (r.execution_id !== undefined) return r.execution_id;
  const text = (v: unknown): string => (typeof v === "string" ? v : "");
  const [session, mission] = [text(r.session_id), text(r.mission_id)];
  return session === ""
    ? `unnamed::${mission}:${text(r.ts)}:${text(r.handle)}:${text(r.machine_uid)}`
    : `unnamed:${session}:${mission}`;
}

/** A liveness bookend: the unit it brackets (contract 8's grains: a whole
 *  `run`, or one role `execution`) and its edge. The viewer's copy of
 *  `darkmux_flow::FlowAction::bookend`, keyed by the actions it names. */
export interface Bookend {
  readonly grain: "run" | "execution";
  readonly edge: "start" | "complete" | "error";
}

const BOOKENDS: ReadonlyMap<NormAction, Bookend> = new Map<NormAction, Bookend>([
  [ACTION.RunStart, { grain: "run", edge: "start" }],
  [ACTION.RunComplete, { grain: "run", edge: "complete" }],
  [ACTION.RunError, { grain: "run", edge: "error" }],
  [ACTION.DispatchStart, { grain: "execution", edge: "start" }],
  [ACTION.DispatchComplete, { grain: "execution", edge: "complete" }],
  [ACTION.DispatchError, { grain: "execution", edge: "error" }],
]);

/** The bookend an action is, or `null` when it is none. */
export const bookendOf = (a: NormAction | undefined): Bookend | null => (a === undefined ? null : (BOOKENDS.get(a) ?? null));

/** A bookend start, at either grain. */
export const isBookendStart = (a: NormAction | undefined): boolean => bookendOf(a)?.edge === "start";

/** A bookend terminal, at either grain: the "did this run or execution
 *  stop" question. */
export const isBookendTerminal = (a: NormAction | undefined): boolean => {
  const edge = bookendOf(a)?.edge;
  return edge === "complete" || edge === "error";
};

/** An execution's terminal: the "did this dispatch stop" question. */
export const isDispatchTerminal = (a: NormAction | undefined): boolean => bookendOf(a)?.grain === "execution" && isBookendTerminal(a);

/** Any `dispatch.*` action, known or not: evidence that model-dispatch work
 *  ran under a record's session. The one family test that must see an
 *  unknown spelling, which is why it lives here. */
export const isDispatchFamily = (a: NormAction | undefined): boolean => a !== undefined && tagText(a).startsWith("dispatch.");

// ─── time policy ──────────────────────────────────────────────────────────

/** Ascending by `tMs`, untimed records last (policy rule 3). Stable. */
export function byTime(a: NormRecord, b: NormRecord): number {
  if (a.tMs === null) return b.tMs === null ? 0 : 1;
  if (b.tMs === null) return -1;
  return a.tMs - b.tMs;
}

/** Ascending by the hub's receive order (#3017), for folding records that
 *  come from machines whose clocks disagree. Records that carry a hub id sort
 *  by it. A record with none (it never passed through the hub: a local-only
 *  day-file line, a static replay) sorts after every hub-ordered one, by its
 *  own time: only that tail is ordered by a clock, and only ever against
 *  records the hub has no say over. With no hub ids at all this is
 *  {@link byTime}. Stable. */
export function byReceiveOrder(a: NormRecord, b: NormRecord): number {
  const ha = a.hub ?? null;
  const hb = b.hub ?? null;
  if (ha !== null && hb !== null) return ha - hb;
  if (ha !== null) return -1;
  if (hb !== null) return 1;
  return byTime(a, b);
}

/** Newest first, untimed records after every timed one (policy rule 3): the
 *  order a "most recent wins" scan reads, so a malformed record wins only
 *  when nothing timed does. Stable. */
export function byTimeNewestFirst(a: NormRecord, b: NormRecord): number {
  if (a.tMs === null) return b.tMs === null ? 0 : 1;
  if (b.tMs === null) return -1;
  return b.tMs - a.tMs;
}

/** The latest record by time, under policy rule 2: the latest timed one, and
 *  an untimed one (the last in arrival order) only when none is timed. */
export function latestByTime<R extends NormRecord>(records: readonly R[]): R | undefined {
  let best: R | undefined;
  for (const r of records) {
    if (r.tMs === null) {
      if (best === undefined || best.tMs === null) best = r;
    } else if (best === undefined || best.tMs === null || r.tMs >= best.tMs) {
      best = r;
    }
  }
  return best;
}

/** The earliest record by time, under policy rule 2: the earliest timed one
 *  (the first in arrival order on a tie), and an untimed one (the first) only
 *  when none is timed. `latestByTime`'s mirror. */
export function earliestByTime<R extends NormRecord>(records: readonly R[]): R | undefined {
  let best: R | undefined;
  for (const r of records) {
    if (r.tMs === null) {
      if (best === undefined) best = r;
    } else if (best === undefined || best.tMs === null || r.tMs < best.tMs) {
      best = r;
    }
  }
  return best;
}

/** Whether `r` is at or before `t` under policy rule 1: the per-record form
 *  of `recordsAsOf`'s cut, for a single record already in hand. */
export function isAsOf(r: NormRecord, t: number): boolean {
  return r.tMs === null || r.tMs <= t;
}

/** The latest timed record's `tMs`, once per window ARRAY. */
const latestCache = new WeakMap<readonly NormRecord[], number>();
function latestTimed(data: readonly NormRecord[]): number {
  let latest = latestCache.get(data);
  if (latest === undefined) {
    latest = -Infinity;
    for (const r of data) if (r.tMs !== null && r.tMs > latest) latest = r.tMs;
    latestCache.set(data, latest);
  }
  return latest;
}

/** The last cut per window array, with the range of `t` it stays exact for:
 *  `t >= lo` (the latest record it includes) and `t < hi` (the earliest
 *  record it excludes). */
const asOfCache = new WeakMap<readonly NormRecord[], { out: NormRecord[]; lo: number; hi: number }>();
let asOfFilterRuns = 0;

/** Test-only: how many times `recordsAsOf` has actually filtered a window. */
export function __asOfFilterRuns(): number {
  return asOfFilterRuns;
}

/** The records of `data` as of instant `t`: the one as-of cut every surface
 * uses, live (`t` = now) and in playback (`t` = the playhead) alike.
 *
 * (#2911) Cheap on a clock tick. When nothing in the window is later than
 * `t` (the normal live case) it returns `data` itself, the same reference
 * every tick, and filters nothing. Otherwise it filters once and returns
 * that same result until the window array changes or `t` crosses the next
 * excluded record. A window array is never mutated after it is first read;
 * every producer builds a new one. */
export function recordsAsOf<R extends NormRecord>(data: readonly R[], t: number): R[] {
  if (latestTimed(data) <= t) return data as R[];
  const cached = asOfCache.get(data);
  if (cached && t >= cached.lo && t < cached.hi) return cached.out as R[];
  asOfFilterRuns++;
  const out: R[] = [];
  let lo = -Infinity;
  let hi = Infinity;
  for (const r of data) {
    if (isAsOf(r, t)) {
      out.push(r);
      if (r.tMs !== null && r.tMs > lo) lo = r.tMs;
    } else if (r.tMs !== null && r.tMs < hi) {
      hi = r.tMs;
    }
  }
  asOfCache.set(data, { out, lo, hi });
  return out;
}

/** Whether `r` is strictly after `t` under policy rule 1: an untimed record
 *  is, since nothing proves it is not (the mission graph folds only records
 *  newer than its snapshot, and an untimed terminal must still close its
 *  step). */
export function isAfter(r: NormRecord, t: number): boolean {
  return r.tMs === null || r.tMs > t;
}

/** Whether `r` is at or after `t` under policy rule 1: an untimed record
 *  is, as for `isAfter`. */
export function isAtOrAfter(r: NormRecord, t: number): boolean {
  return r.tMs === null || r.tMs >= t;
}

/** The records of `data` at or after `t` (a window's trailing edge), under
 *  the same policy: an untimed record is kept. */
export function recordsSince<R extends NormRecord>(data: readonly R[], t: number): R[] {
  return data.filter((r) => isAtOrAfter(r, t));
}

/** The record as the wire carried it, without the fields `ingest` adds. For
 *  surfaces that show or search a record's own fields (the detail panel,
 *  the event log's quick search), which must not see `tMs`. A tagged field's
 *  runtime value is its wire text, so an unknown `stage: "verify"` shows and
 *  searches as `verify`, not as a collapsed "unknown". */
export function wireOf(r: NormRecord): Record<string, unknown> {
  const { tMs: _parsed, ...wire } = r;
  return wire;
}

/** Dedup identity for a record: two deliveries of the same line (the
 *  two-day fetch overlap, the SSE tail against its reconcile backstop)
 *  share it. */
export function recKey(r: NormRecord): string {
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
