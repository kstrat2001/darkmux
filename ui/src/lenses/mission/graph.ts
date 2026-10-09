/**
 * Pure logic for the mission-graph lens (#1868) — a straight TypeScript port
 * of `crates/darkmux-serve/assets/mission-graph.html`'s own pure functions
 * (that file's own module doc names the endpoints + design this ports from;
 * every function below cites the line-shape it mirrors so a diff against
 * that file stays traceable). Deliberately free of React/React-Flow types —
 * `MissionGraphLens.tsx`/`MissionCanvas.tsx` map this module's plain output
 * into node/edge props; `timeline.ts` builds on the same status/metrics
 * primitives for the mobile renderer.
 *
 * One deliberate, documented divergence from the legacy page: legacy tracks
 * "when did we last hear from this step" in a plain ref OUTSIDE React state
 * (`STEP_LAST_RX`, mission-graph.html), stamped at SSE-receive wall-clock
 * time, because its own architecture is an imperative reducer applied one
 * record at a time as they arrive. This port instead FOLDS the whole known
 * record set (backfill + live tail) through {@link foldMissionState} on every
 * change, so "last heard from" is derived as the newest record TIMESTAMP
 * correlated to that step, not a separate receive-time side channel. The two
 * agree whenever record timestamps track real time closely (true for every
 * production dispatch), and differ only for a badly clock-skewed producer or
 * a client that was backgrounded long enough to miss ticks — an edge case,
 * named here rather than silently reproducing the imperative-ref pattern in
 * a codebase that already prefers pure, foldable state (`darkmux-crew`'s own
 * step reducers work the same way).
 */
import { compactThousands, fmtElapsed, type CompactStyle } from "../../lib/format";
import { PURPOSE, isUsageRecord, usageContribution } from "../../lib/usageRecords";
import { ACTION, CATEGORY, SOURCE, byTime, byTimeNewestFirst, isAfter, isAsOf, endPayloadOf, isDispatchFamily, isDispatchTerminal, latestByTime, payloadOf, receiveKey, stepIdOf, type NormAction, type NormRecord } from "../../lib/ingest";
import { lifecycleAt, type LifecyclePhase, type LifecyclePolicy } from "../../lib/lifecycle";
import { currentRun, groupOfRecords } from "../../lib/runRef";
import type { GraphEdge } from "../../types/generated/GraphEdge";
import type { GraphNode } from "../../types/generated/GraphNode";
import type { GraphNodeStatus } from "../../types/generated/GraphNodeStatus";
import type { MissionGraph } from "../../types/generated/MissionGraph";
import type { MissionStatus } from "../../types/generated/MissionStatus";
import type { StepRow } from "../../types/generated/StepRow";

// ─── wire types (crates/darkmux-serve/src/mission_graph.rs) ────────────────
//
// Generated from the server's own structs (`bun run types:regen`). `GraphStep`
// is the server's `StepRow`, renamed here because this lens has a `StepRow`
// component of its own.

export type { GraphEdge, GraphNode, MissionGraph };
export type GraphStep = StepRow;

export const COL_W = 260;
/** (#2104) A task card that carries step rows. Measured on the real
 * finalized crawl `crawl-1788402801-729335` at desktop width (2026-09-03):
 * the card's content wanted 338px against a 258px card — the widest metric
 * row (`4:56 319k tok 7 turns 19 tools`) is ~247px on its own, after a
 * ~84px step lead and 30px of tree rail. 360 covers that with slack for one
 * more digit; the meter's CSS ellipsis is the backstop past it. */
const TASK_W_WITH_STEPS = 360;

/** (#2104) A card's width is decided by its content class, here, once —
 * the layout, the React Flow node style and the phase box all read it. */
function taskWidth(task: GraphNode): number {
  return (task.steps || []).length ? TASK_W_WITH_STEPS : COL_W;
}
export const COL_GAP = 80;
// (#2057) These describe the card `.missionlens .mnode` actually draws,
// measured in a real browser at scale 1 (2026-08-28): a one-step card is
// 85 px, a plain step row adds ~17 px, a row carrying a model chip ~28 px.
// The old 40 + 20/row described a card the CSS no longer drew, and three
// two-step siblings overlapped by ~30 px each. `tests/e2e/mission-lens-
// layout-geometry.spec.js` asserts no two task boxes intersect; if the CSS
// grows a card again, that test is the thing that says so.
const TASK_MIN_PITCH = 70;
const TASK_HEADER_H = 68;
const STEP_ROW_H = 28;
const TASK_GAP = 16;
export const PHASE_LABEL_W = 40;
const BAND_GAP = 40;
export const BAND_PAD = 56;

function taskPitch(stepCount: number): number {
  return Math.max(TASK_MIN_PITCH, TASK_HEADER_H + stepCount * STEP_ROW_H + TASK_GAP);
}

interface LayoutBox {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface Layout {
  positions: Record<string, { x: number; y: number }>;
  boxes: Record<string, LayoutBox>;
  /** (#2104) Per-task card width; the canvas applies it as the node's style width. */
  widths: Record<string, number>;
}

/**
 * (#2376) `narrow` selects the phone layout: every phase's tasks stack in a
 * SINGLE column (dependency order top-to-bottom) instead of the desktop's
 * side-by-side depth columns. Measured on the real 8-task, 3-phase graph
 * `mission-lens-layout-geometry.spec.js` uses as its fixture: the desktop
 * layout lays that graph out ~1378 flow-px wide, which forces `fitView` to
 * ~0.24 scale to fit a 358px-wide portrait pane (#2376's own reported
 * number) while leaving most of the pane's HEIGHT empty. Stacking narrows
 * the band to one card's width and trades the unused width for height, so
 * the fit becomes height- rather than width-bound and lands at a legible
 * scale instead. `narrow` is a caller-supplied flag (see `MissionCanvas`'s
 * `useIsMobile()` call), not something this function infers — layout is
 * pure and has no access to viewport state of its own.
 */
export function computeLayout(nodes: GraphNode[], narrow = false): Layout {
  const phases = nodes.filter((n) => n.kind === "phase").sort((a, b) => a.depth - b.depth);
  const tasksByPhase: Record<string, GraphNode[]> = {};
  for (const n of nodes) {
    if (n.kind !== "task") continue;
    const p = n.parentId || "__none__";
    (tasksByPhase[p] = tasksByPhase[p] || []).push(n);
  }

  const positions: Record<string, { x: number; y: number }> = {};
  const boxes: Record<string, LayoutBox> = {};
  const widths: Record<string, number> = {};
  let bandTop = 0;
  const phaseList: Array<{ id: string; depth: number }> = phases.length ? phases : [{ id: "__none__", depth: 0 }];

  for (const phase of phaseList) {
    const tasks = tasksByPhase[phase.id] || [];
    let maxColumnHeight = 0;
    // (#2104) Column zero starts just past the phase label — it used to
    // start a full COL_W further right, which read as a layout step missing
    // (a half-empty phase box with the card parked on its right). Each
    // column is as wide as its widest card, and the next column starts past
    // it, so a card sized to its content never overlaps its neighbor.
    const x = PHASE_LABEL_W + COL_GAP / 2;
    let rightEdge = x;
    if (narrow) {
      // (#2376) One column, ordered by depth (ties keep the graph's own
      // order — `Array.prototype.sort` is stable), stacked with the same
      // `taskPitch` the desktop columns use.
      let colW = COL_W;
      let yCursor = bandTop + BAND_PAD;
      const ordered = [...tasks].sort((a, b) => (a.depth || 0) - (b.depth || 0));
      for (const t of ordered) {
        const w = taskWidth(t);
        widths[t.id] = w;
        colW = Math.max(colW, w);
        positions[t.id] = { x, y: yCursor };
        yCursor += taskPitch((t.steps || []).length);
      }
      maxColumnHeight = yCursor - (bandTop + BAND_PAD);
      rightEdge = x + colW;
    } else {
      const byDepth: Record<number, GraphNode[]> = {};
      let maxDepth = 0;
      // Re-base each band to its own first column — see mission-graph.html's
      // `computeLayout` for the fan-in/phase-order-arrow bug this rebasing
      // fixes. Only the per-band OFFSET is dropped; relative depths (the
      // intra-phase dependency order) are untouched.
      let minDepth = Infinity;
      for (const t of tasks) minDepth = Math.min(minDepth, t.depth || 0);
      if (!isFinite(minDepth)) minDepth = 0;
      for (const t of tasks) {
        const d = Math.max(0, (t.depth || 0) - minDepth);
        maxDepth = Math.max(maxDepth, d);
        (byDepth[d] = byDepth[d] || []).push(t);
      }
      let colX = x;
      for (let d = 0; d <= maxDepth; d++) {
        const atDepth = byDepth[d] || [];
        let colW = COL_W;
        let yCursor = bandTop + BAND_PAD;
        for (const t of atDepth) {
          const w = taskWidth(t);
          widths[t.id] = w;
          colW = Math.max(colW, w);
          positions[t.id] = { x: colX, y: yCursor };
          yCursor += taskPitch((t.steps || []).length);
        }
        maxColumnHeight = Math.max(maxColumnHeight, yCursor - (bandTop + BAND_PAD));
        rightEdge = colX + colW;
        colX = rightEdge + COL_GAP;
      }
    }
    if (phase.id !== "__none__") {
      positions[phase.id] = { x: 0, y: bandTop + BAND_PAD };
    }
    const bandHeight = Math.max(160, maxColumnHeight + BAND_PAD * 2);
    if (phase.id !== "__none__") {
      boxes[phase.id] = { x: 0, y: bandTop, w: rightEdge + BAND_PAD, h: bandHeight };
    }
    bandTop += bandHeight + BAND_GAP;
  }
  // (#2057) One width for every band: the widest one's. Bands sized to their
  // own content and anchored left put each phase's center somewhere
  // different, so the phase→phase edge (bottom-center to top-center) ran
  // diagonally. Equal widths put every center on one line and the edges
  // are vertical.
  let widest = 0;
  for (const b of Object.values(boxes)) widest = Math.max(widest, b.w);
  for (const b of Object.values(boxes)) b.w = widest;
  return { positions, boxes, widths };
}

// ─── status vocabulary (mission-graph.html: STATUS_RANK, statusRank,
// keepPageStatus) ────────────────────────────────────────────────────────────

/** The statuses a rank is defined for, from the generated unions: a new
 *  variant is a compile error here until it is ranked. `unknown` (a mission
 *  the daemon could not classify) deliberately has none. */
type RankedStatus = Exclude<GraphNodeStatus | MissionStatus, "unknown">;

const STATUS_RANK: Record<RankedStatus, number> = {
  planned: 0,
  running: 1,
  complete: 2,
  error: 2,
  abandoned: 2,
  // (#2406) A phase-only terminal — a genuine MIX of complete and
  // errored/abandoned tasks. MUST rank alongside the other terminals: left
  // out of this table, `statusRank`/`isUnknownStatus` would treat it as
  // "unknown", and `keepPageStatus`'s own doc says unknown "never wins once
  // HELD" — a live `"phase complete"` flow record (rank 2) arriving after
  // the initial snapshot already rendered `degraded` would then silently
  // overwrite it back to `complete`, regressing the exact signal this
  // status exists to carry, client-side, on every SSE tick.
  degraded: 2,
  // (#2343) A task/phase ADMITTED to a wave but not yet dispatched
  // (`mission_graph.rs::derive_task_status`). Ranks WITH `running`/`active`
  // — it is the same point in the lifecycle, told honestly — for the same
  // reason `degraded` above is in this table at all: left out, `statusRank`
  // treats it as "unknown", `keepPageStatus` returns false in BOTH
  // directions, and the very next `"phase start"` flow record newer than
  // the snapshot (emitted at WAVE ADMISSION — the exact instant this status
  // exists to describe) overwrites `waiting` back to `running`, so the chip
  // reads `RUNNING · 7 waiting` until the next reconcile poll. Measured
  // against the LIVE fold (`foldFlowRecords`, the wired path — note
  // `mergeGraphs`'s monotone ratchet is NOT a safety net here: it has no
  // production callers, #2527).
  waiting: 1,
  active: 1,
  finalized: 2,
  aborted: 2,
};
const UNKNOWN_STATUS_RANK = 99;

export function statusRank(s: string | undefined): number {
  return isRanked(s) ? STATUS_RANK[s] : UNKNOWN_STATUS_RANK;
}
const isRanked = (s: string | undefined): s is RankedStatus => s !== undefined && Object.hasOwn(STATUS_RANK, s);
export function isUnknownStatus(s: string | undefined): boolean {
  return !isRanked(s);
}
/** Whether a merge should KEEP the page's current value over an incoming
 * one. See mission-graph.html's own extensive comment on the asymmetry:
 * unknown wins on ARRIVAL (it's newer than this build knows) but never wins
 * once HELD (it's also the value understood least). */
export function keepPageStatus(oldStatus: string | undefined, incoming: string | undefined): boolean {
  if (isUnknownStatus(oldStatus) || isUnknownStatus(incoming)) return false;
  return statusRank(oldStatus) >= statusRank(incoming);
}

// ─── graph indexing (mission-graph.html: indexGraph) ───────────────────────

export interface GraphIndex {
  nodeIds: Set<string>;
  stepToTask: Record<string, string>;
  stepIds: Set<string>;
  taskIds: Set<string>;
  phaseIds: Set<string>;
}

export function indexGraph(g: { nodes: GraphNode[] }): GraphIndex {
  const nodeIds = new Set<string>();
  const stepToTask: Record<string, string> = {};
  const stepIds = new Set<string>();
  const taskIds = new Set<string>();
  const phaseIds = new Set<string>();
  for (const n of g.nodes) {
    nodeIds.add(n.id);
    if (n.kind === "phase") phaseIds.add(n.id);
    if (n.kind === "task") taskIds.add(n.id);
    for (const s of n.steps || []) {
      stepToTask[s.id] = n.id;
      stepIds.add(s.id);
    }
  }
  return { nodeIds, stepToTask, stepIds, taskIds, phaseIds };
}

// ─── work metrics (mission-graph.html: isAiKind, stepForRecord,
// applyRecordToMetrics, seedMetricsFromGraph, stepDisplayMetrics,
// missionTotals) ────────────────────────────────────────────────────────────

export function isAiKind(kind: string | undefined): boolean {
  if (!kind) return false;
  // `-render` kinds are prompt builders, never dispatchers, and `-collect`
  // kinds are the procedural fan-in step AFTER a `dispatch.map` (they read
  // that step's already-completed results, never dispatch themselves) —
  // both excluded BEFORE the prefix tests below (#1530; `-collect` per
  // #2310 P2 review finding I4).
  if (kind.endsWith("-render") || kind.endsWith("-collect")) return false;
  // (#2430) `dispatch.summary` folds the unit outcomes already on disk; it
  // dispatches no model, though its id shares the `dispatch.` prefix.
  if (kind === "dispatch.summary") return false;
  if (kind.indexOf("dispatch.") === 0) return true;
  if (kind === "mission.coder" || kind === "mission.verify") return true;
  return false;
}

export interface StepMetrics {
  /** (#3067) The plain sum of every usage record folded so far (utility
   *  included): the one token sum, the figure the server's `tokensFinal` is
   *  too. */
  tokRun: number;
  /** The utility part of `tokRun`. */
  tokUtilityRun: number;
  /** The server's tokens for the step (`seedMetricsFromGraph`), the same sum over the
   *  records on disk; see `turnFinal`. The display takes the larger of it and `tokRun`
   *  (both are sums of the same records, so never added). */
  tokFinal: number;
  /** The utility part of `tokFinal`. */
  tokUtilityFinal: number;
  turnRun: number;
  /** The server's finalized turns for the step (`seedMetricsFromGraph`): the sum of its
   *  executions' `total_turns`. */
  turnFinal: number;
  /** The `total_turns` of every terminal folded so far, summed: one term per execution,
   *  as `tokRun` sums one term per usage record. Kept apart from `turnFinal` because the
   *  seed and the fold read the same records, so adding one to the other would count
   *  them twice; the display takes the larger. */
  turnsEnded: number;
  toolRun: number;
  toolFinal: number;
  startTs: number;
  endTs: number;
  /** Newest record ts (ms) correlated to this step — this port's derived
   * stand-in for legacy's out-of-React `STEP_LAST_RX` wall-clock ref; see
   * this module's own doc for why. */
  lastTs: number;
  /** Whether the step's own `step.start` bookend has been seen. Such a step
   *  ends only on its `step.complete`/`step.error`: a dispatch terminal is
   *  one execution's end (a map step holds one per item), not the step's. */
  stepBookended?: boolean;
  /** (#3017) The receive key ({@link receiveKey}) of the newest terminal
   *  folded: what tells a retry (a start received AFTER the step ended) from
   *  a sibling start of the same attempt. */
  endKey?: number | null;
  /** Whether that newest terminal was a failure. Only a failed attempt is
   *  retried, so only then does a later start restart the span; a step whose
   *  dispatches ran one after another keeps its whole span. */
  endFailed?: boolean;
}

const EMPTY_METRICS: StepMetrics = {
  tokRun: 0,
  tokUtilityRun: 0,
  tokFinal: 0,
  tokUtilityFinal: 0,
  turnRun: 0,
  turnFinal: 0,
  turnsEnded: 0,
  toolRun: 0,
  toolFinal: 0,
  startTs: 0,
  endTs: 0,
  lastTs: 0,
};

export type MetricsMap = Record<string, StepMetrics>;

/** `tsToMs` — mission-graph.html. Parses a graph step's own timestamp
 * (`startedTs`/`completedTs`: an ISO string, or an epoch NUMBER in seconds
 * OR ms — a value below 1e12 is a seconds epoch) to epoch ms; 0 when
 * unparseable. A flow record's time is its `tMs`, parsed at ingest. */
export function tsToMs(stamp: string | number | null | undefined): number {
  if (stamp == null) return 0;
  if (typeof stamp === "number") return stamp < 1e12 ? stamp * 1000 : stamp;
  const t = Date.parse(stamp);
  return isNaN(t) ? 0 : t;
}

/** `stepForRecord` — mission-graph.html. Two correlation keys, in order:
 * `payload.step_id` (every record of a step's own session names its step,
 * and a dispatch run as a graph step stamps it on its per-event records),
 * then `handle` (the scheduler's step lifecycle records). The session id is
 * never parsed: it is an opaque join key. `mission_id`, when present, is
 * authoritative and never falls through. */
export function stepForRecord(rec: NormRecord, idx: GraphIndex, missionId: string): string | null {
  if (rec.mission_id && rec.mission_id !== missionId) return null;
  const stepId = stepIdOf(rec);
  if (stepId && idx.stepIds.has(stepId)) return stepId;
  if (rec.handle && idx.stepIds.has(rec.handle)) return rec.handle;
  return null;
}

/** `stepDispatchSessions` (#2223) -- the INVERSE of {@link stepForRecord}:
 * for each step, the dispatch session id observed on that step's own
 * records, which is what lets the step drill-in reach the dispatch detail
 * view (`#dispatch=<id>`) instead of only scoping the events column.
 *
 * The discriminator is EVIDENCE OF DISPATCH, never the shape of the
 * session id (an opaque join key): a session counts only through records
 * whose action is a `dispatch.*` bookend/turn (`isDispatchFamily`). Steps
 * that never dispatched (procedural steps, bookkeeping-only sessions)
 * produce no entry, and the caller keeps #2189's scoping -- the honest
 * fallback.
 *
 * Selection, when a step's records name more than one dispatch session:
 * 1. A session whose dispatch records carry THIS mission's `mission_id`
 *    beats any session that doesn't -- a pre-4.0 archive's records may
 *    carry none, so null-mission records are admitted, but they can never
 *    outrank records positively tagged as ours.
 * 2. Otherwise the session with the LATEST dispatch-action timestamp wins,
 *    not the most records: a looped-then-killed attempt emits hundreds of
 *    turn records while the successful retry emits a dozen, so frequency
 *    selects the failure; recency selects the attempt that represents the
 *    step's current state. Count breaks ties.
 *
 * "Latest" is the hub's receive order (#3017) when both sessions' records
 * carry it, because a retry on another machine is stamped by THAT machine's
 * clock, which can run minutes behind the first attempt's. A session with no
 * hub-ordered record (a replay, a local-only line) is compared by its own
 * time, and only against another such session.
 */
export function stepDispatchSessions(records: NormRecord[], missionId: string): Record<string, string> {
  type Tally = { n: number; lastKey: number; ours: boolean };
  const tally: Record<string, Record<string, Tally>> = {};
  for (const rec of records) {
    // (#2223) Evidence that a dispatch actually ran under this session, as
    // opposed to the session merely appearing in the record stream:
    // mission/phase/step bookkeeping and telemetry do not count.
    if (!isDispatchFamily(rec.action)) continue;
    const stepId = stepIdOf(rec) ?? "";
    const sid = typeof rec.session_id === "string" ? rec.session_id : "";
    if (!stepId || !sid) continue;
    if (rec.mission_id && rec.mission_id !== missionId) continue;
    const forStep = (tally[stepId] ||= {});
    const t = (forStep[sid] ||= { n: 0, lastKey: 0, ours: false });
    t.n += 1;
    t.lastKey = Math.max(t.lastKey, receiveKey(rec) ?? 0);
    if (rec.mission_id === missionId) t.ours = true;
  }
  const out: Record<string, string> = {};
  for (const [stepId, seen] of Object.entries(tally)) {
    let best = "";
    let bestT: Tally | null = null;
    for (const [sid, t] of Object.entries(seen)) {
      if (!bestT || attemptBeats(t, bestT)) {
        best = sid;
        bestT = t;
      }
    }
    if (best) out[stepId] = best;
  }
  return out;
}

/** Whether attempt `t` is the better representative of its step than `best`:
 *  one positively tagged with this mission, else the more recent, else the
 *  one with more records. */
function attemptBeats(t: { n: number; lastKey: number; ours: boolean }, best: { n: number; lastKey: number; ours: boolean }): boolean {
  if (t.ours !== best.ours) return t.ours;
  return t.lastKey !== best.lastKey ? t.lastKey > best.lastKey : t.n > best.n;
}

const METRIC_KEYS: readonly (keyof StepMetrics)[] = [
  "tokRun", "tokUtilityRun", "tokFinal", "tokUtilityFinal", "turnRun", "turnFinal", "turnsEnded", "toolRun", "toolFinal",
  "startTs", "endTs", "endKey", "endFailed", "lastTs", "stepBookended",
];

function sameMetrics(a: StepMetrics, b: StepMetrics): boolean {
  return METRIC_KEYS.every((k) => a[k] === b[k]);
}

/** The running and final counts one record carries, each read through its own
 *  action's payload type; `null` for a count the record does not have. */
function recordFigures(rec: NormRecord): { turnsSoFar: number | null; toolCallsSoFar: number | null; totalTurns: number | null } {
  const count = (n: unknown): number | null => (typeof n === "number" ? n : null);
  const end = endPayloadOf(rec);
  return {
    turnsSoFar: count(payloadOf(rec, ACTION.DispatchTurn)?.turns_so_far),
    toolCallsSoFar: count(payloadOf(rec, ACTION.DispatchTool)?.tool_calls_so_far),
    totalTurns: count(end?.total_turns),
  };
}

/** Fold a start or terminal into the step's span (start, end). Records must be
 *  folded in the hub's receive order (`byReceiveOrder`), not by each writer's
 *  clock (#3017): a start the hub received AFTER the step's last terminal is a
 *  RETRY, so the span restarts at it and the step reads running. Without hub
 *  ids the span is the earliest start to the latest terminal, as before. A
 *  terminal with no usable time still ends the step, at the latest time the
 *  step is known to have been alive (the bad-timestamp policy). */
function foldSpan(
  cur: StepMetrics,
  next: StepMetrics,
  ev: { isStart: boolean; isTerminal: boolean; failed: boolean; recMs: number; key: number | null },
): void {
  if (ev.isStart && ev.recMs) {
    const retried = ev.key !== null && cur.endFailed === true && cur.endKey != null && ev.key > cur.endKey;
    if (retried) {
      next.startTs = ev.recMs;
      next.endTs = 0;
      next.endKey = null;
      next.endFailed = false;
    } else {
      next.startTs = next.startTs ? Math.min(next.startTs, ev.recMs) : ev.recMs;
    }
  }
  if (ev.isTerminal) {
    next.endTs = Math.max(next.endTs, ev.recMs || next.lastTs || next.startTs);
    if (ev.key !== null && ev.key >= (next.endKey ?? 0)) {
      next.endKey = ev.key;
      next.endFailed = ev.failed;
    }
  }
}

/** (#3067) Fold one usage record's tokens into a step's running figures: the
 *  total, and the utility part named beside it. */
function foldUsage(next: StepMetrics, usage: ReturnType<typeof usageContribution>): void {
  if (!usage) return;
  next.tokRun += usage.total;
  if (usage.purpose === PURPOSE.utility) next.tokUtilityRun += usage.total;
}

/** `applyRecordToMetrics` — mission-graph.html. Folds one record into the
 * per-step metric accumulator, returning a NEW map only when something
 * changed (so a no-op record doesn't churn state). */
export function applyRecordToMetrics(metrics: MetricsMap, rec: NormRecord, idx: GraphIndex, missionId: string): MetricsMap {
  const sid = stepForRecord(rec, idx, missionId);
  if (!sid) return metrics;
  const cur = metrics[sid] || EMPTY_METRICS;
  const recMs = rec.tMs ?? 0;
  const next: StepMetrics = { ...cur, lastTs: Math.max(cur.lastTs, recMs) };

  const action = rec.action;
  // (#2902 step 2a, #3067) The step's running figure is the plain sum of its
  // usage records through the one sum's per-record half, utility calls
  // included and named: the same figure the runs board and the server's
  // `tokensFinal` show. `null` for a non-usage record.
  const usage = usageContribution(rec);
  const isUsage = isUsageRecord(rec);
  const isTurn = action === ACTION.DispatchTurn;
  const isTool = action === ACTION.DispatchTool;
  const isStart = action === ACTION.DispatchStart || action === ACTION.StepStart;
  const stepBookended = cur.stepBookended || action === ACTION.StepStart;
  if (stepBookended) next.stepBookended = true;
  const isTerminal = action === ACTION.StepComplete || action === ACTION.StepError || (!stepBookended && isDispatchTerminal(action));

  foldSpan(cur, next, { isStart, isTerminal, failed: action === ACTION.StepError || action === ACTION.DispatchError, recMs, key: receiveKey(rec) });

  const fig = recordFigures(rec);
  const started = next.startTs > 0;
  if (isUsage && started) {
    foldUsage(next, usage);
  } else if (isTurn && started) {
    next.turnRun = fig.turnsSoFar !== null ? Math.max(next.turnRun, fig.turnsSoFar) : next.turnRun + 1;
  } else if (isTool && started) {
    next.toolRun = fig.toolCallsSoFar !== null ? Math.max(next.toolRun, fig.toolCallsSoFar) : next.toolRun + 1;
  } else if (isDispatchTerminal(action)) {
    if (fig.totalTurns !== null) next.turnsEnded += fig.totalTurns;
  }

  if (sameMetrics(next, cur)) return metrics;
  return { ...metrics, [sid]: next };
}

/** `hasNoMetricsData` — the "this step reports no measurements yet" guard
 * from `seedMetricsFromGraph` (mission-graph.html): true exactly when all
 * six finalized fields are falsy. Single object parameter on purpose —
 * three adjacent numbers and two adjacent booleans would be easy to pass
 * in the wrong order positionally. */
export function hasNoMetricsData(m: {
  tokensFinal: number; turnsFinal: number; toolsFinal: number; startedMs: number;
}): boolean {
  return !m.tokensFinal && !m.turnsFinal && !m.toolsFinal && !m.startedMs;
}

/** A wire count, 0 when absent. */
function countOf(n: unknown): number {
  return typeof n === "number" ? n : 0;
}

/** (#3067) The tokens field of a step's detail, with its utility part named
 *  beside it: one total, the part that is darkmux's own calls shown, never
 *  subtracted. */
function tokenFields(d: DisplayMetrics): { key: string; label: string; value: string }[] {
  const out = d.tokens ? [{ key: "tokens", label: "tokens", value: fmtTok(d.tokens) }] : [];
  if (d.tokens && d.utility) out.push({ key: "utility", label: "of which utility", value: fmtTok(d.utility) });
  return out;
}

/** `seedMetricsFromGraph` — mission-graph.html. Seeds the accumulator from
 * the finalized totals the server folded into graph.json, taking the max so
 * a live SSE value already climbing is never regressed. */
export function seedMetricsFromGraph(metrics: MetricsMap, g: { nodes: GraphNode[] } | null | undefined): MetricsMap {
  let out = metrics;
  for (const n of g?.nodes || []) {
    for (const s of n.steps || []) {
      const tf = countOf(s.tokensFinal);
      const uf = countOf(s.tokensUtility);
      const nf = typeof s.turnsFinal === "number" ? s.turnsFinal : 0;
      const st = tsToMs(s.startedTs);
      // A graph step carries no tool total (the server's `StepRow` has none):
      // tool counts come from the step's flow records alone.
      if (hasNoMetricsData({ tokensFinal: tf, turnsFinal: nf, toolsFinal: 0, startedMs: st })) continue;
      const cur = out[s.id] || EMPTY_METRICS;
      const ntf = Math.max(cur.tokFinal, tf);
      const nuf = Math.max(cur.tokUtilityFinal, uf);
      const nnf = Math.max(cur.turnFinal, nf);
      const curSt = cur.startTs || 0;
      const nst = curSt ? (st ? Math.min(curSt, st) : curSt) : st;
      if (ntf === cur.tokFinal && nuf === cur.tokUtilityFinal && nnf === cur.turnFinal && nst === curSt) {
        continue;
      }
      if (out === metrics) out = { ...metrics };
      out[s.id] = { ...cur, tokFinal: ntf, tokUtilityFinal: nuf, turnFinal: nnf, startTs: nst };
    }
  }
  return out;
}

export interface DisplayMetrics {
  tokens: number;
  /** The utility part of `tokens` (compaction, radio routing), named beside it. */
  utility: number;
  turns: number;
  tools: number;
  has: boolean;
}

export function stepDisplayMetrics(m: StepMetrics | undefined): DisplayMetrics {
  if (!m) return { tokens: 0, utility: 0, turns: 0, tools: 0, has: false };
  // (#2902 step 2a, #3067) The usage records' plain sum: what this page folded
  // live, or what the server summed from disk, whichever is larger.
  const tokens = Math.max(m.tokRun, m.tokFinal);
  const utility = Math.max(m.tokUtilityRun, m.tokUtilityFinal);
  const turns = Math.max(m.turnFinal, m.turnsEnded) || m.turnRun || 0;
  const tools = m.toolFinal || m.toolRun || 0;
  return { tokens, utility, turns, tools, has: tokens > 0 || turns > 0 || tools > 0 };
}

export interface MissionTotals {
  total: number;
  turns: number;
}

/** The mission meter: every step's own figure, summed. (#2902 step 2a) The
 *  withdrawn local/cloud/unknown split (#2834) is gone from the data too. */
export function missionTotals(metrics: MetricsMap): MissionTotals {
  let total = 0,
    turns = 0;
  for (const k of Object.keys(metrics)) {
    const d = stepDisplayMetrics(metrics[k]);
    total += d.tokens;
    turns += d.turns;
  }
  return { total, turns };
}

// ─── status transitions from flow records (mission-graph.html: STATUS_ACTIONS,
// statusFromRecord, App's onMessage node/step status-flip branch) ──────────

const STATUS_ACTIONS: ReadonlyMap<NormAction, GraphNodeStatus | MissionStatus> = new Map<NormAction, GraphNodeStatus | MissionStatus>([
  [ACTION.StepStart, "running"],
  [ACTION.StepComplete, "complete"],
  [ACTION.StepError, "error"],
  [ACTION.PhaseStart, "running"],
  [ACTION.PhaseComplete, "complete"],
  [ACTION.PhaseAbandon, "abandoned"],
  [ACTION.MissionStart, "active"],
  [ACTION.MissionClose, "finalized"],
  [ACTION.MissionAbort, "aborted"],
]);

export function statusFromRecord(rec: NormRecord): GraphNodeStatus | MissionStatus | undefined {
  return rec.action === undefined ? undefined : STATUS_ACTIONS.get(rec.action);
}

/** Whether `rec` is a step terminal naming an operator's stop: the step
 *  ended abandoned, aborted (`Step::end_unfinished`, the daemon's own
 *  decision, which the graph snapshot carries), never errored. */
function stoppedByOperator(rec: NormRecord): boolean {
  return typeof payloadOf(rec, ACTION.StepError)?.stop_reason === "string";
}

/** `applyFlowRecord` — the pure counterpart to mission-graph.html's App
 * `onMessage`'s status-flip branch: given the current graph + one flow
 * record + its index, returns the graph with that ONE node/step row's
 * status advanced (rank-guarded, never regressed), or the SAME graph
 * reference when the record names no status transition, a foreign mission,
 * or an unrecognized handle. Does NOT touch metrics or the events list —
 * those are separate folds ({@link applyRecordToMetrics}, {@link recordInMission})
 * over the same record stream, matching the legacy page's own three
 * independent effects of one incoming record. */
export function applyFlowRecord(graph: MissionGraph, rec: NormRecord, idx: GraphIndex, missionId: string): MissionGraph {
  const newStatus = statusFromRecord(rec);
  const handle = rec.handle;
  if (!newStatus || !handle) return graph;
  if (rec.mission_id && rec.mission_id !== missionId) return graph;

  const advance = (oldStatus: string | undefined): string | undefined => (keepPageStatus(oldStatus, newStatus) ? oldStatus : newStatus);

  if (idx.nodeIds.has(handle)) {
    let changed = false;
    const nodes = graph.nodes.map((n) => {
      if (n.id !== handle) return n;
      const advanced = advance(n.status);
      if (advanced === n.status) return n;
      changed = true;
      return { ...n, status: advanced as GraphNode["status"] };
    });
    return changed ? { ...graph, nodes } : graph;
  }

  const taskId = idx.stepToTask[handle];
  if (!taskId) return graph;
  let changed = false;
  const nodes = graph.nodes.map((n) => {
    if (n.id !== taskId || !n.steps) return n;
    const steps = n.steps.map((s) => {
      if (s.id !== handle) return s;
      const advanced = advance(s.status);
      if (advanced === s.status) return s;
      changed = true;
      const { abandonedReason: _, ...rest } = s;
      const next: GraphStep = stoppedByOperator(rec)
        ? { ...rest, status: "abandoned", abandonedReason: "aborted" }
        : { ...rest, status: advanced as GraphStep["status"] };
      return next;
    });
    return changed ? { ...n, steps } : n;
  });
  return changed ? { ...graph, nodes } : graph;
}

/** Fold a whole (already-sorted-ascending-by-ts) record set onto a base
 * graph via {@link applyFlowRecord}, one at a time. The bulk counterpart
 * `MissionGraphLens` recomputes from on every backfill/live-tail change,
 * rather than the legacy page's incremental per-record `setState` — see
 * this module's own doc for why a pure fold replaces the imperative
 * reducer in this port.
 *
 * (#2518) Skips any record whose `ts` is NOT STRICTLY NEWER than
 * `baseGraph.generated_at_ms` — a record at or before the snapshot's own
 * build time is already baked into that snapshot's fresh, server-computed
 * values (`mission_graph.rs::derive_task_status`/`phase_task_rollup`
 * re-derive from CURRENT data on every build), so replaying it can only
 * ever be redundant or actively WRONG, never additive.
 *
 * Before this gate, a handle whose own node status a flow record can flip
 * (`STATUS_ACTIONS` — phases and steps; tasks carry no status-bearing
 * action of their own) had every record it had EVER received replayed
 * against the fresh snapshot on every fold, `keepPageStatus`-guarded. That
 * guard is right for a LIVE record racing ahead of the periodic
 * `graph.json` poll — the exact case a record here that IS newer than the
 * snapshot still needs to win, unconditionally, same as before this fix.
 * It is wrong for a HISTORICAL record: a phase legitimately reads
 * `planned` again in a fresh, later snapshot (`derive_task_status` can
 * regress `Running`→`Planned` between a task's steps, and #2406 makes a
 * phase's own display status a rollup of its tasks') and an old "phase
 * start" record from back when the phase first went `Running` would
 * out-rank it and PIN the chip at `running` forever — proven with a real
 * fold+render before this fix landed (`running`→`planned` snapshot regress
 * + a stale matching record ⇒ the chip stayed on `running` after the
 * reconcile poll; see this repo's `mission-lens-status-vocab.spec.js`
 * "#2518" case). Filtering by snapshot recency here, rather than loosening
 * `keepPageStatus` itself, keeps the unknown-status arrival/held asymmetry
 * `keepPageStatus` documents completely untouched — this only decides
 * which records are even ELIGIBLE to reach it.
 *
 * `<=`, not `<`: a record stamped in the exact same instant the snapshot
 * was built carries no information the snapshot doesn't already have.
 * A record with no usable time is folded (`isAfter`): nothing proves the
 * snapshot already holds it, and an untimed terminal must still end its
 * step, as it ends its run everywhere else.
 * `generated_at_ms` missing (older fixtures, hand-built graphs with no
 * opinion on freshness) folds every record, unfiltered — today's
 * pre-#2518 behavior, preserved as the lenient-on-read default. */
export function foldFlowRecords(baseGraph: MissionGraph, records: NormRecord[], idx: GraphIndex, missionId: string): MissionGraph {
  const snapshotMs = baseGraph.generated_at_ms;
  let g = baseGraph;
  for (const rec of records) {
    if (snapshotMs !== undefined && !isAfter(rec, snapshotMs)) continue;
    g = applyFlowRecord(g, rec, idx, missionId);
  }
  return g;
}

/** `mergeGraphs` — mission-graph.html. Merge a freshly-fetched disk snapshot
 * into the page's current graph: structure refreshes from disk, per-node
 * (and per-step) STATUS is monotone — disk wins only when strictly MORE
 * advanced. Kept for the periodic graph.json refetch path, which can race a
 * live status the fold above already advanced. */
export function mergeGraphs(prevGraph: MissionGraph | null, fresh: MissionGraph): MissionGraph {
  if (!prevGraph) return fresh;
  const prevStatus: Record<string, string> = {};
  const prevStepStatus: Record<string, string> = {};
  for (const n of prevGraph.nodes) {
    prevStatus[n.id] = n.status;
    for (const s of n.steps || []) prevStepStatus[s.id] = s.status;
  }
  const nodes = fresh.nodes.map((n) => {
    const old = prevStatus[n.id];
    let merged = old !== undefined && keepPageStatus(old, n.status) ? { ...n, status: old as GraphNode["status"] } : n;
    if (merged.steps && merged.steps.length) {
      const steps = merged.steps.map((s) => {
        const oldS = prevStepStatus[s.id];
        return oldS !== undefined && keepPageStatus(oldS, s.status) ? { ...s, status: oldS as GraphStep["status"] } : s;
      });
      merged = { ...merged, steps };
    }
    return merged;
  });
  return { ...fresh, nodes };
}

// ─── events panel (mission-graph.html: recordInMission) ────────────────────

/** `recordInMission` — mission-graph.html. Does this record belong to THIS
 * mission (the events panel filter)? `mission_id`, when present, is
 * authoritative; absent, falls back to proxy matching on phase, handle and
 * `payload.step_id`. */
export function recordInMission(rec: NormRecord, idx: GraphIndex, missionId: string): boolean {
  if (rec.mission_id) return rec.mission_id === missionId;
  if (rec.phase_id && idx.phaseIds.has(rec.phase_id)) return true;
  if (rec.handle && (idx.nodeIds.has(rec.handle) || idx.stepIds.has(rec.handle))) return true;
  const stepId = stepIdOf(rec);
  if (stepId && idx.stepIds.has(stepId)) return true;
  return false;
}

// ─── formatting (mission-graph.html: fmtTok, fmtModel, hhmmss) ─────────────
// (U3-7/U5-2) `fmtElapsed` moved to `lib/format.ts` — one duration
// formatter for the whole app; see its own doc.

/** The graph's compact token count (`1.5k` / `15k` / `2.5M`) — step rows,
 * node labels, the totals line. Its decimals are deliberately NOT `fmtC`'s:
 * a step row is a one-line meter beside a duration and a seat, where
 * `15.00k` spends two characters on a precision nobody reads there, so the
 * thousands arm keeps one decimal only below 10k and none above.
 *
 * (#2919) The arms themselves hand over through `compactThousands`, the one
 * shared rule: this used to switch at 1,000,000 with no regard to rounding,
 * so 999,500–999,999 printed `1000k`. It also printed a lowercase `m`, an
 * inheritance from `mission-graph.html` that no golden pins; `M` now, the
 * same letter as every other count in the app. */
export function fmtTok(n: number | null | undefined): string {
  if (n == null) return "0";
  if (n < 1000) return String(n);
  return compactThousands(n, GRAPH_TOK_STYLE);
}

const GRAPH_TOK_STYLE: CompactStyle = { k: (n) => (n < 10_000 ? 1 : 0), m: 1 };

export function fmtModel(m: string | undefined): string {
  if (!m) return "";
  const s = m.indexOf("darkmux:") === 0 ? m.slice(8) : m;
  const slash = s.lastIndexOf("/");
  return slash >= 0 ? s.slice(slash + 1) : s;
}

export function hhmmss(ts: string | number): string {
  const d = new Date(ts);
  if (isNaN(d.getTime())) return "";
  const p = (x: number) => String(x).padStart(2, "0");
  return p(d.getHours()) + ":" + p(d.getMinutes()) + ":" + p(d.getSeconds());
}

// ─── step meter (mission-graph.html: stepStartMs, stepMeterFor) ────────────

export function stepStartMs(step: GraphStep, m: StepMetrics | undefined): number {
  if (m && m.startTs) return m.startTs;
  return tsToMs(step.startedTs);
}

/** (#2269) The step's end, same precedence as {@link stepStartMs}: the
 * metrics stream's own `endTs` first, the node's `completedTs` as the
 * fallback. `0` while the step is still running (or never ran). */
export function stepEndMs(step: GraphStep, m: StepMetrics | undefined): number {
  if (m && m.endTs) return m.endTs;
  return tsToMs(step.completedTs);
}

export interface StepMeter {
  show: boolean;
  tokens: number;
  turns: number;
  tools: number;
  generating: boolean;
  elapsedMs: number;
  /** (#2269) The step's own wall time: start → end once finished, start →
   * now while running (the same number `elapsedMs` pulses with). `0` when
   * the step never started. Rendered muted on a finished row, where the
   * pulse — and with it the only time the row had — is gone. */
  wallMs: number;
}

/** Each step's run phase as of an instant (`stepPhasesAt`), by step id. */
export type StepPhases = ReadonlyMap<string, LifecyclePhase>;
export const NO_STEP_PHASES: StepPhases = new Map();

/** A step's records by session: each session a run of its own. */
export type StepSessions = ReadonlyMap<string, readonly NormRecord[]>;

/** The records {@link stepForRecord} attributes to each step, by session:
 *  a step spans its task session's step bookends and one dispatch session
 *  per item it fans out (`dispatch.map`, review's seats and draws), and
 *  each session is a run of its own. Time order, as given. */
export function recordsByStep(records: readonly NormRecord[], idx: GraphIndex, missionId: string): Map<string, Map<string, NormRecord[]>> {
  const out = new Map<string, Map<string, NormRecord[]>>();
  for (const rec of records) {
    const stepId = stepForRecord(rec, idx, missionId);
    if (!stepId) continue;
    const sessions = out.get(stepId) ?? new Map<string, NormRecord[]>();
    out.set(stepId, sessions);
    const key = rec.session_id ?? "";
    const list = sessions.get(key);
    if (list) list.push(rec);
    else sessions.set(key, [rec]);
  }
  return out;
}

/** How far along a phase is, for picking a step's from its sessions': in
 *  flight outranks everything, so one working item keeps the step alive. */
const PHASE_RANK: Record<LifecyclePhase, number> = { not_started: 0, closed: 1, stale: 2, waiting: 3, open: 4 };

const STEP_BOOKENDS: ReadonlySet<NormAction> = new Set([ACTION.StepStart, ACTION.StepComplete, ACTION.StepError]);

/** Whether the step's own latest bookend as of `now` is its terminal: the
 *  scheduler's word on the step, which outranks any item session left
 *  without a terminal of its own. */
function stepEnded(sessions: StepSessions, now: number): boolean {
  let latest: NormRecord | undefined;
  for (const recs of sessions.values()) {
    const b = latestByTime(recs.filter((r) => r.action !== undefined && STEP_BOOKENDS.has(r.action) && isAsOf(r, now)));
    if (b && (!latest || byTime(b, latest) >= 0)) latest = b;
  }
  return latest !== undefined && latest.action !== ACTION.StepStart;
}

/** Each step's run lifecycle phase as of `now` (`lib/lifecycle.ts`), the
 *  one every surface judges a run by: closed once its own step terminal
 *  landed; before that each of its sessions judged on its own, and the step
 *  in flight while any of them is. */
export function stepPhasesAt(byStep: ReadonlyMap<string, StepSessions>, now: number, policy: LifecyclePolicy): StepPhases {
  const out = new Map<string, LifecyclePhase>();
  for (const [stepId, sessions] of byStep) {
    let phase: LifecyclePhase = "not_started";
    if (stepEnded(sessions, now)) {
      out.set(stepId, "closed");
      continue;
    }
    for (const recs of sessions.values()) {
      const p = lifecycleAt(currentRun(groupOfRecords(recs), now), now, policy).phase;
      if (PHASE_RANK[p] > PHASE_RANK[phase]) phase = p;
    }
    out.set(stepId, phase);
  }
  return out;
}

/** `stepMeterFor` — mission-graph.html. A running step is generating while
 * its run is in flight by its lifecycle (`phases`, from `stepPhasesAt`):
 * open or held by a budget wait. A step with no records of its own is not. */
export function stepMeterFor(step: GraphStep, metrics: MetricsMap, now: number, phases: StepPhases = NO_STEP_PHASES): StepMeter {
  const m = metrics[step.id];
  const d = stepDisplayMetrics(m);
  const show = isAiKind(step.kind) || d.has;
  const phase = phases.get(step.id);
  const generating = step.status === "running" && (phase === "open" || phase === "waiting");
  const startMs = stepStartMs(step, m);
  const elapsedMs = generating && startMs && now ? Math.max(0, now - startMs) : 0;
  const endMs = stepEndMs(step, m) || (step.status === "running" && now ? now : 0);
  const wallMs = startMs && endMs ? Math.max(0, endMs - startMs) : 0;
  return { show: show || generating, tokens: d.tokens, turns: d.turns, tools: d.tools, generating, elapsedMs, wallMs };
}

// ─── step row vocabulary (mission-graph.html: stepLead, stepSeat) ──────────

export function stepLead(s: GraphStep): string {
  return s.label || s.kind || "step";
}
export function stepSeat(kind: string | undefined): string {
  if (!kind) return "";
  const i = kind.lastIndexOf(":");
  return i >= 0 ? kind.slice(i + 1) : "";
}

// ─── step header block (#2189, step drill-in) ──────────────────────────────

export interface StepHeaderField {
  key: string;
  label: string;
  value: string;
}

/** Builds the small step-header block's field list -- "unit id, source,
 * rule, sha (short), status, started/elapsed, turns, tool calls, findings,
 * tokens, and any detector kinds fired" (#2189's own wording). PROGRESSIVE
 * by design: a field appears only when real data backs it -- a non-crawl
 * step (no `source`/`rule`/`sha`/`findings` in its records) renders a
 * shorter list, never a placeholder "--" row.
 *
 * Two data sources, matching the split this module already makes elsewhere:
 * `step`/`metrics` (the graph's own per-step accumulator -- status, started/
 * elapsed, turns/tools/tokens, already computed by `applyRecordToMetrics`/
 * `stepMeterFor`) for the fields every step kind can carry, and
 * `stepRecords` (this ONE step's own raw flow records -- `payload.step_id`
 * equality, the same scoping rule the mainstay events column uses; see
 * `MissionGraphLens`'s own doc) for the crawl-shaped extras that have no
 * home in `GraphStep`/`StepMetrics` yet. Scanned NEWEST-FIRST so the most
 * recent record wins when more than one carries the same key (a crawl unit
 * can emit `source`/`rule` more than once while working through a batch). */
export function buildStepHeaderFields(step: GraphStep, metrics: MetricsMap, now: number, stepRecords: NormRecord[], phases: StepPhases = NO_STEP_PHASES): StepHeaderField[] {
  const fields: StepHeaderField[] = [];
  fields.push({ key: "unit", label: "unit", value: step.label || step.id });
  if (step.kind) fields.push({ key: "kind", label: "kind", value: step.kind });

  const ordered = [...stepRecords].sort(byTimeNewestFirst);
  const pick = (keys: string[]): string | undefined => {
    for (const rec of ordered) {
      const p = rec.payload;
      if (!p) continue;
      for (const k of keys) {
        const v = p[k];
        if (typeof v === "string" && v) return v;
        if (typeof v === "number" && Number.isFinite(v)) return String(v);
      }
    }
    return undefined;
  };

  const source = pick(["source"]);
  if (source) fields.push({ key: "source", label: "source", value: source });
  const rule = pick(["rule", "rule_id"]);
  if (rule) fields.push({ key: "rule", label: "rule", value: rule });
  const sha = pick(["sha", "head_sha", "commit_sha"]);
  if (sha) fields.push({ key: "sha", label: "sha", value: sha.slice(0, 8) });

  fields.push({ key: "status", label: "status", value: step.status || "planned" });

  const m = metrics[step.id];
  const meter = stepMeterFor(step, metrics, now, phases);
  const startMs = stepStartMs(step, m);
  if (startMs) {
    const endMs = m && m.endTs ? m.endTs : meter.generating ? now : 0;
    const elapsed = endMs ? fmtElapsed(Math.max(0, endMs - startMs)) : meter.generating ? fmtElapsed(meter.elapsedMs) : "";
    fields.push({ key: "started", label: "started", value: elapsed ? `${hhmmss(startMs)} · ${elapsed}` : hhmmss(startMs) });
  }

  const d = stepDisplayMetrics(m);
  if (d.turns) fields.push({ key: "turns", label: "turns", value: String(d.turns) });
  if (d.tools) fields.push({ key: "tools", label: "tool calls", value: String(d.tools) });
  // (#2834) The " cloud" suffix is withdrawn — see MissionGraphLens.
  fields.push(...tokenFields(d));

  const findings = pick(["findings", "finding_count", "findings_count"]);
  if (findings) fields.push({ key: "findings", label: "findings", value: findings });

  const detectorKinds = new Set<string>();
  for (const rec of stepRecords) {
    const isDetector = rec.action === ACTION.TelemetryDetector || (rec.category === CATEGORY.Telemetry && rec.source === SOURCE.Detector);
    if (!isDetector) continue;
    const kind = payloadOf(rec, ACTION.TelemetryDetector)?.kind;
    if (kind) detectorKinds.add(kind);
  }
  if (detectorKinds.size) fields.push({ key: "detectors", label: "detectors", value: [...detectorKinds].sort().join(", ") });

  return fields;
}

// ─── phase-order edges + React-Flow-ready node/edge shaping
// (mission-graph.html: phaseOrderEdges, toRfNodes, toRfEdges) ──────────────

/** An edge as drawn: the server's `contains` / `depends_on` edges, plus the
 *  `phase_order` edges this viewer derives itself from phase depth (the server
 *  never sends one). */
export type DrawnEdge = Omit<GraphEdge, "kind"> & { kind: GraphEdge["kind"] | "phase_order" };

export function phaseOrderEdges(nodes: GraphNode[]): DrawnEdge[] {
  const phases = nodes.filter((n) => n.kind === "phase").sort((a, b) => a.depth - b.depth);
  const edges: DrawnEdge[] = [];
  for (let i = 0; i < phases.length - 1; i++) {
    edges.push({ id: "phase-order:" + phases[i].id + ":" + phases[i + 1].id, source: phases[i].id, target: phases[i + 1].id, kind: "phase_order" });
  }
  return edges;
}

/** Every edge this canvas actually DRAWS (`contains` is dropped — drawn by
 * phase-container enclosure instead — plus the client-synthesized
 * `phase_order` edges appended), regardless of rendering library. */
export function drawnEdges(graphEdges: GraphEdge[], graphNodes: GraphNode[]): DrawnEdge[] {
  const serverEdges: DrawnEdge[] = graphEdges.filter((e) => e.kind !== "contains");
  return serverEdges.concat(phaseOrderEdges(graphNodes));
}
