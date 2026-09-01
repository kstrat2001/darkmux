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
import type { FlowRecord } from "../../types/handwritten";

// ─── wire types (crates/darkmux-serve/src/mission_graph.rs) ────────────────

export interface GraphStep {
  id: string;
  label: string;
  kind: string;
  status: string;
  startedTs?: number;
  completedTs?: number;
  tokensFinal?: number;
  turnsFinal?: number;
  toolsFinal?: number;
  cloud?: boolean;
  localOk?: boolean;
  model?: string;
}

export interface GraphNode {
  id: string;
  label: string;
  kind: "phase" | "task";
  status: string;
  parentId?: string;
  startedTs?: number;
  completedTs?: number;
  depth: number;
  description?: string;
  steps?: GraphStep[];
}

export interface GraphEdge {
  id: string;
  source: string;
  target: string;
  kind: "contains" | "depends_on" | "phase_order";
}

export interface MissionGraph {
  mission_id: string;
  mission_status: string;
  nodes: GraphNode[];
  edges: GraphEdge[];
  note?: string;
}

// ─── layout (mission-graph.html: computeLayout) ────────────────────────────
// The JS-side counterpart to the Rust `layer_tasks_by_depth`, which stamps
// `depth` onto every task node server-side. Positions nodes in bands: one
// band per phase (stacked top-to-bottom), tasks within a band laid out
// left-to-right by `depth` (rebased to the phase's own first column — see
// the inline comment below) so dependency order reads left-to-right.
export const COL_W = 260;
export const COL_GAP = 80;
// (#2057) These describe the card `.missionlens .mnode` actually draws,
// measured in a real browser at scale 1 (2026-08-28): a one-step card is
// 85 px, a plain step row adds ~17 px, a row carrying a model chip ~28 px.
// The old 40 + 20/row described a card the CSS no longer drew, and three
// two-step siblings overlapped by ~30 px each. `tests/e2e/mission-lens-
// layout-geometry.spec.js` asserts no two task boxes intersect; if the CSS
// grows a card again, that test is the thing that says so.
export const TASK_MIN_PITCH = 70;
export const TASK_HEADER_H = 68;
export const STEP_ROW_H = 28;
export const TASK_GAP = 16;
export const PHASE_LABEL_W = 40;
export const BAND_GAP = 40;
export const BAND_PAD = 56;

export function taskPitch(stepCount: number): number {
  return Math.max(TASK_MIN_PITCH, TASK_HEADER_H + stepCount * STEP_ROW_H + TASK_GAP);
}

export interface LayoutBox {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface Layout {
  positions: Record<string, { x: number; y: number }>;
  boxes: Record<string, LayoutBox>;
}

export function computeLayout(nodes: GraphNode[]): Layout {
  const phases = nodes.filter((n) => n.kind === "phase").sort((a, b) => a.depth - b.depth);
  const tasksByPhase: Record<string, GraphNode[]> = {};
  for (const n of nodes) {
    if (n.kind !== "task") continue;
    const p = n.parentId || "__none__";
    (tasksByPhase[p] = tasksByPhase[p] || []).push(n);
  }

  const positions: Record<string, { x: number; y: number }> = {};
  const boxes: Record<string, LayoutBox> = {};
  let bandTop = 0;
  const phaseList: Array<{ id: string; depth: number }> = phases.length ? phases : [{ id: "__none__", depth: 0 }];

  for (const phase of phaseList) {
    const tasks = tasksByPhase[phase.id] || [];
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
    let maxColumnHeight = 0;
    for (let d = 0; d <= maxDepth; d++) {
      const atDepth = byDepth[d] || [];
      const x = PHASE_LABEL_W + COL_W + d * (COL_W + COL_GAP);
      let yCursor = bandTop + BAND_PAD;
      for (const t of atDepth) {
        positions[t.id] = { x, y: yCursor };
        yCursor += taskPitch((t.steps || []).length);
      }
      maxColumnHeight = Math.max(maxColumnHeight, yCursor - (bandTop + BAND_PAD));
    }
    if (phase.id !== "__none__") {
      positions[phase.id] = { x: 0, y: bandTop + BAND_PAD };
    }
    const bandHeight = Math.max(160, maxColumnHeight + BAND_PAD * 2);
    if (phase.id !== "__none__") {
      const lastX = PHASE_LABEL_W + COL_W + maxDepth * (COL_W + COL_GAP);
      boxes[phase.id] = { x: 0, y: bandTop, w: lastX + COL_W + BAND_PAD, h: bandHeight };
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
  return { positions, boxes };
}

// ─── status vocabulary (mission-graph.html: normalizeMissionStatus,
// STATUS_RANK, statusRank, keepPageStatus) ──────────────────────────────────

export function normalizeMissionStatus(s: string | undefined): string | undefined {
  return s === "closed" ? "finalized" : s;
}

const STATUS_RANK: Record<string, number> = {
  planned: 0,
  running: 1,
  complete: 2,
  error: 2,
  abandoned: 2,
  active: 1,
  finalized: 2,
  closed: 2,
  aborted: 2,
  paused: 1,
};
const UNKNOWN_STATUS_RANK = 99;

export function statusRank(s: string | undefined): number {
  if (s === undefined) return UNKNOWN_STATUS_RANK;
  return STATUS_RANK[s] !== undefined ? STATUS_RANK[s] : UNKNOWN_STATUS_RANK;
}
export function isUnknownStatus(s: string | undefined): boolean {
  return s === undefined || STATUS_RANK[s] === undefined;
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
  sessionToStep: Record<string, string>;
  sessions: Set<string>;
}

export function indexGraph(g: { nodes: GraphNode[] }): GraphIndex {
  const nodeIds = new Set<string>();
  const stepToTask: Record<string, string> = {};
  const stepIds = new Set<string>();
  const taskIds = new Set<string>();
  const phaseIds = new Set<string>();
  const sessionToStep: Record<string, string> = {};
  const sessions = new Set<string>();
  for (const n of g.nodes) {
    nodeIds.add(n.id);
    if (n.kind === "phase") phaseIds.add(n.id);
    if (n.kind === "task") {
      taskIds.add(n.id);
      sessions.add("task-" + n.id);
    }
    for (const s of n.steps || []) {
      stepToTask[s.id] = n.id;
      stepIds.add(s.id);
      sessionToStep["step-" + s.id] = s.id;
      sessions.add("step-" + s.id);
    }
  }
  return { nodeIds, stepToTask, stepIds, taskIds, phaseIds, sessionToStep, sessions };
}

// ─── work metrics (mission-graph.html: isAiKind, stepForRecord,
// applyRecordToMetrics, seedMetricsFromGraph, stepDisplayMetrics,
// missionTotals) ────────────────────────────────────────────────────────────

export function isAiKind(kind: string | undefined): boolean {
  if (!kind) return false;
  // `-render` kinds are prompt builders, never dispatchers — excluded
  // BEFORE the prefix tests below (see mission-graph.html's own #1530 note).
  if (kind.endsWith("-render")) return false;
  if (kind.indexOf("dispatch.") === 0) return true;
  if (kind === "mission.coder" || kind === "mission.verify") return true;
  if (kind.indexOf("review.probe") === 0 || kind.indexOf("review.judge") === 0 || kind.indexOf("review.verify") === 0)
    return true;
  return false;
}

export interface StepMetrics {
  tokRun: number;
  tokFinal: number;
  turnRun: number;
  turnFinal: number;
  toolRun: number;
  toolFinal: number;
  cloud: boolean;
  localOk: boolean;
  startTs: number;
  endTs: number;
  /** Newest record ts (ms) correlated to this step — this port's derived
   * stand-in for legacy's out-of-React `STEP_LAST_RX` wall-clock ref; see
   * this module's own doc for why. */
  lastTs: number;
}

const EMPTY_METRICS: StepMetrics = {
  tokRun: 0,
  tokFinal: 0,
  turnRun: 0,
  turnFinal: 0,
  toolRun: 0,
  toolFinal: 0,
  cloud: false,
  localOk: false,
  startTs: 0,
  endTs: 0,
  lastTs: 0,
};

export type MetricsMap = Record<string, StepMetrics>;

/** `tsToMs` — mission-graph.html. Parses a flow-record `ts` (ISO string) or
 * an epoch NUMBER (seconds OR ms — a value below 1e12 is a seconds epoch)
 * to epoch ms; 0 when unparseable. */
export function tsToMs(ts: string | number | null | undefined): number {
  if (ts == null) return 0;
  if (typeof ts === "number") return ts < 1e12 ? ts * 1000 : ts;
  const t = Date.parse(ts);
  return isNaN(t) ? 0 : t;
}

/** `stepForRecord` — mission-graph.html. Three correlation keys, in order:
 * `payload.step_id`, `session_id` (the `step-<id>` default), `handle`.
 * `mission_id`, when present, is authoritative and never falls through. */
export function stepForRecord(rec: FlowRecord, idx: GraphIndex, missionId: string): string | null {
  if (rec.mission_id && rec.mission_id !== missionId) return null;
  const p = rec.payload || {};
  const stepId = typeof p.step_id === "string" ? p.step_id : undefined;
  if (stepId && idx.stepIds.has(stepId)) return stepId;
  if (rec.session_id && idx.sessionToStep[rec.session_id]) return idx.sessionToStep[rec.session_id];
  if (rec.handle && idx.stepIds.has(rec.handle)) return rec.handle;
  return null;
}

/** (#2223) A session id MINTED BY THE GRAPH rather than observed on a real
 * dispatch. `indexGraph` synthesizes `step-<id>`/`task-<id>` so a record
 * carrying one can be correlated back to its node (see `sessionToStep`),
 * and the mission's own bookkeeping records ride `mission-<id>`. None of
 * the three addresses a dispatch, so none is a legal `#dispatch=` target --
 * routing to one lands on a detail view with nothing to show. */
export function isSyntheticSession(sessionId: string): boolean {
  return sessionId.startsWith("step-") || sessionId.startsWith("task-") || sessionId.startsWith("mission-");
}

/** `stepDispatchSessions` (#2223) -- the INVERSE of {@link stepForRecord}:
 * for each step, the real dispatch `session_id` observed on that step's own
 * records, which is what makes the step drill-in able to reach the dispatch
 * detail view (`#dispatch=<id>`) instead of only scoping the events column.
 *
 * Correlates on `payload.step_id` ALONE -- deliberately the single strong
 * key, not `stepForRecord`'s three-key fallthrough. The other two keys
 * (`session_id` via `sessionToStep`, `handle`) are exactly the SYNTHETIC
 * ids this function exists to filter out, so feeding them back in would
 * resolve every step to its own graph-minted id and route the drill-in to
 * an empty detail view. A step whose records carry no `step_id` gets no
 * entry, and its caller keeps #2189's scoping behavior -- the honest
 * outcome for a procedural step that never dispatched a model at all.
 *
 * When a step's records name more than one dispatch (a retried step), the
 * MOST FREQUENT wins rather than the first or last seen: a handful of
 * bookkeeping records from an abandoned attempt should not outrank the
 * attempt that actually did the work, and "first" and "last" each pick the
 * wrong one depending on which way the retry went. */
export function stepDispatchSessions(records: FlowRecord[]): Record<string, string> {
  const tally: Record<string, Record<string, number>> = {};
  for (const rec of records) {
    const p = rec.payload || {};
    const stepId = typeof p.step_id === "string" ? p.step_id : "";
    const sid = typeof rec.session_id === "string" ? rec.session_id : "";
    if (!stepId || !sid || isSyntheticSession(sid)) continue;
    const forStep = (tally[stepId] ||= {});
    forStep[sid] = (forStep[sid] || 0) + 1;
  }
  const out: Record<string, string> = {};
  for (const [stepId, seen] of Object.entries(tally)) {
    let best = "";
    let bestN = 0;
    for (const [sid, n] of Object.entries(seen)) {
      if (n > bestN) {
        best = sid;
        bestN = n;
      }
    }
    if (best) out[stepId] = best;
  }
  return out;
}

/** `applyRecordToMetrics` — mission-graph.html. Folds one record into the
 * per-step metric accumulator, returning a NEW map only when something
 * changed (so a no-op record doesn't churn state). */
export function applyRecordToMetrics(metrics: MetricsMap, rec: FlowRecord, idx: GraphIndex, missionId: string): MetricsMap {
  const sid = stepForRecord(rec, idx, missionId);
  if (!sid) return metrics;
  const p = rec.payload || {};
  const cur = metrics[sid] || EMPTY_METRICS;
  const recMs = tsToMs(rec.ts);
  const next: StepMetrics = { ...cur, lastTs: Math.max(cur.lastTs, recMs) };

  const action = rec.action || "";
  const isTok = (rec.category === "telemetry" && rec.source === "tokens") || action === "telemetry.tokens";
  const isTurn = action === "dispatch.turn";
  const isTool = action === "dispatch.tool";
  const isComplete = action === "dispatch complete" || action === "dispatch.complete";
  const isStepResult = action === "step result";
  const isStart = action === "dispatch start" || action === "dispatch.start" || action === "step start";
  const isTerminal =
    action === "step complete" ||
    action === "step error" ||
    action === "dispatch complete" ||
    action === "dispatch.complete" ||
    action === "dispatch error" ||
    action === "dispatch.error";

  if (isStart && recMs) next.startTs = next.startTs ? Math.min(next.startTs, recMs) : recMs;
  if (isTerminal && recMs) next.endTs = Math.max(next.endTs, recMs);

  // Three-state local/cloud/unknown attribution — see mission-graph.html's
  // #1626 comment: `local` requires POSITIVE evidence (a clean terminal
  // with no endpoint), never a bare absence-of-endpoint default.
  if (p.endpoint) next.cloud = true;
  if (action === "dispatch complete" || action === "dispatch.complete") {
    if (!p.endpoint) next.localOk = true;
  }

  const finalTok = (typeof p.total_tokens === "number" ? p.total_tokens : 0) || (typeof p.tokens === "number" ? p.tokens : 0);
  const started = next.startTs > 0;
  if (isTok && started) {
    next.tokRun += typeof p.total_tokens === "number" ? p.total_tokens : 0;
  } else if (isTurn && started) {
    next.turnRun = typeof p.turns_so_far === "number" ? Math.max(next.turnRun, p.turns_so_far) : next.turnRun + 1;
  } else if (isTool && started) {
    next.toolRun = typeof p.tool_calls_so_far === "number" ? Math.max(next.toolRun, p.tool_calls_so_far) : next.toolRun + 1;
  } else if (isComplete) {
    if (finalTok) next.tokFinal = Math.max(next.tokFinal, finalTok);
    if (typeof p.total_turns === "number") next.turnFinal = Math.max(next.turnFinal, p.total_turns);
  } else if (isStepResult) {
    if (finalTok) next.tokFinal = Math.max(next.tokFinal, finalTok);
  }

  if (
    next.tokRun === cur.tokRun &&
    next.tokFinal === cur.tokFinal &&
    next.turnRun === cur.turnRun &&
    next.turnFinal === cur.turnFinal &&
    next.toolRun === cur.toolRun &&
    next.toolFinal === cur.toolFinal &&
    next.cloud === cur.cloud &&
    next.localOk === cur.localOk &&
    next.startTs === cur.startTs &&
    next.endTs === cur.endTs &&
    next.lastTs === cur.lastTs
  ) {
    return metrics;
  }
  return { ...metrics, [sid]: next };
}

/** `seedMetricsFromGraph` — mission-graph.html. Seeds the accumulator from
 * the finalized totals the server folded into graph.json, taking the max so
 * a live SSE value already climbing is never regressed. */
export function seedMetricsFromGraph(metrics: MetricsMap, g: { nodes: GraphNode[] } | null | undefined): MetricsMap {
  let out = metrics;
  for (const n of g?.nodes || []) {
    for (const s of n.steps || []) {
      const tf = typeof s.tokensFinal === "number" ? s.tokensFinal : 0;
      const nf = typeof s.turnsFinal === "number" ? s.turnsFinal : 0;
      const cf = typeof s.toolsFinal === "number" ? s.toolsFinal : 0;
      const cl = !!s.cloud;
      const lok = !!s.localOk;
      const st = tsToMs(s.startedTs);
      if (!tf && !nf && !cf && !cl && !lok && !st) continue;
      const cur = out[s.id] || EMPTY_METRICS;
      const ntf = Math.max(cur.tokFinal, tf);
      const nnf = Math.max(cur.turnFinal, nf);
      const ncf = Math.max(cur.toolFinal, cf);
      const ncl = cur.cloud || cl;
      const nlok = cur.localOk || lok;
      const curSt = cur.startTs || 0;
      const nst = curSt ? (st ? Math.min(curSt, st) : curSt) : st;
      if (ntf === cur.tokFinal && nnf === cur.turnFinal && ncf === cur.toolFinal && ncl === cur.cloud && nlok === cur.localOk && nst === curSt) {
        continue;
      }
      if (out === metrics) out = { ...metrics };
      out[s.id] = { ...cur, tokFinal: ntf, turnFinal: nnf, toolFinal: ncf, cloud: ncl, localOk: nlok, startTs: nst };
    }
  }
  return out;
}

export interface DisplayMetrics {
  tokens: number;
  turns: number;
  tools: number;
  cloud: boolean;
  localOk: boolean;
  has: boolean;
}

export function stepDisplayMetrics(m: StepMetrics | undefined): DisplayMetrics {
  if (!m) return { tokens: 0, turns: 0, tools: 0, cloud: false, localOk: false, has: false };
  const tokens = m.tokFinal || m.tokRun || 0;
  const turns = m.turnFinal || m.turnRun || 0;
  const tools = m.toolFinal || m.toolRun || 0;
  return { tokens, turns, tools, cloud: !!m.cloud, localOk: !!m.localOk, has: tokens > 0 || turns > 0 || tools > 0 };
}

export interface MissionTotals {
  local: number;
  cloud: number;
  unknown: number;
  total: number;
  turns: number;
}

export function missionTotals(metrics: MetricsMap): MissionTotals {
  let local = 0,
    cloud = 0,
    unknown = 0,
    turns = 0;
  for (const k of Object.keys(metrics)) {
    const d = stepDisplayMetrics(metrics[k]);
    if (d.cloud) cloud += d.tokens;
    else if (d.localOk) local += d.tokens;
    else unknown += d.tokens;
    turns += d.turns;
  }
  return { local, cloud, unknown, total: local + cloud + unknown, turns };
}

// ─── status transitions from flow records (mission-graph.html: STATUS_ACTIONS,
// statusFromRecord, App's onMessage node/step status-flip branch) ──────────

const STATUS_ACTIONS: Record<string, string> = {
  "step start": "running",
  "step complete": "complete",
  "step error": "error",
  "phase start": "running",
  "phase complete": "complete",
  "phase abandon": "abandoned",
  "mission start": "active",
  "mission close": "finalized",
  "mission pause": "paused",
  "mission resume": "active",
  "mission abort": "aborted",
};

export function statusFromRecord(rec: FlowRecord): string | undefined {
  const action = rec.action || "";
  return normalizeMissionStatus(STATUS_ACTIONS[action]);
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
export function applyFlowRecord(graph: MissionGraph, rec: FlowRecord, idx: GraphIndex, missionId: string): MissionGraph {
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
      return { ...n, status: advanced! };
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
      return { ...s, status: advanced! };
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
 * reducer in this port. */
export function foldFlowRecords(baseGraph: MissionGraph, records: FlowRecord[], idx: GraphIndex, missionId: string): MissionGraph {
  let g = baseGraph;
  for (const rec of records) g = applyFlowRecord(g, rec, idx, missionId);
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
    let merged = old !== undefined && keepPageStatus(old, n.status) ? { ...n, status: old } : n;
    if (merged.steps && merged.steps.length) {
      const steps = merged.steps.map((s) => {
        const oldS = prevStepStatus[s.id];
        return oldS !== undefined && keepPageStatus(oldS, s.status) ? { ...s, status: oldS } : s;
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
 * authoritative; absent, falls back to proxy matching on handle/session. */
export function recordInMission(rec: FlowRecord, idx: GraphIndex, missionId: string): boolean {
  if (rec.mission_id) return rec.mission_id === missionId;
  if (rec.phase_id && idx.phaseIds.has(rec.phase_id)) return true;
  if (rec.handle && (idx.nodeIds.has(rec.handle) || idx.stepIds.has(rec.handle))) return true;
  if (rec.session_id && idx.sessions.has(rec.session_id)) return true;
  const stepId = rec.payload && typeof rec.payload.step_id === "string" ? rec.payload.step_id : undefined;
  if (stepId && idx.stepIds.has(stepId)) return true;
  return false;
}

// ─── formatting (mission-graph.html: fmtTok, fmtModel, hhmmss, fmtElapsed) ──

export function fmtTok(n: number | null | undefined): string {
  if (n == null) return "0";
  if (n < 1000) return String(n);
  if (n < 1000000) return (n / 1000).toFixed(n < 10000 ? 1 : 0) + "k";
  return (n / 1000000).toFixed(1) + "m";
}

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

export function fmtElapsed(ms: number): string {
  const clamped = !ms || ms < 0 ? 0 : ms;
  const s = Math.floor(clamped / 1000);
  const m = Math.floor(s / 60);
  const hr = Math.floor(m / 60);
  const ss = String(s % 60).padStart(2, "0");
  if (hr > 0) return hr + ":" + String(m % 60).padStart(2, "0") + ":" + ss;
  return m + ":" + ss;
}

// ─── step meter (mission-graph.html: stepStartMs, stepMeterFor,
// STEP_LIVENESS_WINDOW_MS) ───────────────────────────────────────────────────

/** Twice the runtime's default inactivity budget (600s) — see
 * mission-graph.html's own extensive comment. */
export const STEP_LIVENESS_WINDOW_MS = 1200 * 1000;

export function stepStartMs(step: GraphStep, m: StepMetrics | undefined): number {
  if (m && m.startTs) return m.startTs;
  return tsToMs(step.startedTs);
}

export interface StepMeter {
  show: boolean;
  tokens: number;
  turns: number;
  tools: number;
  cloud: boolean;
  generating: boolean;
  elapsedMs: number;
}

/** `stepMeterFor` — mission-graph.html. `lastSignal` reads this port's
 * derived `m.lastTs` (see this module's own doc) in place of legacy's
 * out-of-React `STEP_LAST_RX` wall-clock ref. */
export function stepMeterFor(step: GraphStep, metrics: MetricsMap, now: number): StepMeter {
  const m = metrics[step.id];
  const d = stepDisplayMetrics(m);
  const show = isAiKind(step.kind) || d.has;
  const lastSignal = (m && m.lastTs) || 0;
  const startedAt = stepStartMs(step, m);
  const freshEnough = lastSignal ? now - lastSignal < STEP_LIVENESS_WINDOW_MS : !!startedAt && now - startedAt < STEP_LIVENESS_WINDOW_MS;
  const generating = step.status === "running" && freshEnough;
  const startMs = stepStartMs(step, m);
  const elapsedMs = generating && startMs && now ? Math.max(0, now - startMs) : 0;
  return { show: show || generating, tokens: d.tokens, turns: d.turns, tools: d.tools, cloud: d.cloud, generating, elapsedMs };
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
export function buildStepHeaderFields(step: GraphStep, metrics: MetricsMap, now: number, stepRecords: FlowRecord[]): StepHeaderField[] {
  const fields: StepHeaderField[] = [];
  fields.push({ key: "unit", label: "unit", value: step.label || step.id });
  if (step.kind) fields.push({ key: "kind", label: "kind", value: step.kind });

  const ordered = [...stepRecords].sort((a, b) => (a.ts < b.ts ? 1 : a.ts > b.ts ? -1 : 0));
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
  const meter = stepMeterFor(step, metrics, now);
  const startMs = stepStartMs(step, m);
  if (startMs) {
    const endMs = m && m.endTs ? m.endTs : meter.generating ? now : 0;
    const elapsed = endMs ? fmtElapsed(Math.max(0, endMs - startMs)) : meter.generating ? fmtElapsed(meter.elapsedMs) : "";
    fields.push({ key: "started", label: "started", value: elapsed ? `${hhmmss(startMs)} · ${elapsed}` : hhmmss(startMs) });
  }

  const d = stepDisplayMetrics(m);
  if (d.turns) fields.push({ key: "turns", label: "turns", value: String(d.turns) });
  if (d.tools) fields.push({ key: "tools", label: "tool calls", value: String(d.tools) });
  if (d.tokens) fields.push({ key: "tokens", label: "tokens", value: fmtTok(d.tokens) + (d.cloud ? " cloud" : "") });

  const findings = pick(["findings", "finding_count", "findings_count"]);
  if (findings) fields.push({ key: "findings", label: "findings", value: findings });

  const detectorKinds = new Set<string>();
  for (const rec of stepRecords) {
    const isDetector = rec.action === "telemetry.detector" || (rec.category === "telemetry" && rec.source === "detector");
    if (!isDetector) continue;
    const p = rec.payload;
    const kind = p ? (typeof p.kind === "string" ? p.kind : typeof p.detector === "string" ? p.detector : undefined) : undefined;
    if (kind) detectorKinds.add(kind);
  }
  if (detectorKinds.size) fields.push({ key: "detectors", label: "detectors", value: [...detectorKinds].sort().join(", ") });

  return fields;
}

// ─── phase-order edges + React-Flow-ready node/edge shaping
// (mission-graph.html: phaseOrderEdges, toRfNodes, toRfEdges) ──────────────

export function phaseOrderEdges(nodes: GraphNode[]): GraphEdge[] {
  const phases = nodes.filter((n) => n.kind === "phase").sort((a, b) => a.depth - b.depth);
  const edges: GraphEdge[] = [];
  for (let i = 0; i < phases.length - 1; i++) {
    edges.push({ id: "phase-order:" + phases[i].id + ":" + phases[i + 1].id, source: phases[i].id, target: phases[i + 1].id, kind: "phase_order" });
  }
  return edges;
}

/** Every edge this canvas actually DRAWS (`contains` is dropped — drawn by
 * phase-container enclosure instead — plus the client-synthesized
 * `phase_order` edges appended), regardless of rendering library. */
export function drawnEdges(graphEdges: GraphEdge[], graphNodes: GraphNode[]): GraphEdge[] {
  return graphEdges.filter((e) => e.kind !== "contains").concat(phaseOrderEdges(graphNodes));
}
