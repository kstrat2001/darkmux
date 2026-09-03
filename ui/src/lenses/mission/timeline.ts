/**
 * Mobile vertical-timeline grouping (#1868) — a straight port of
 * `mission-graph.html`'s `timelineActive`/`isNarrowViewport`/`initMinimap`/
 * `taskAggMetrics`/the phase→task grouping half of `renderMissionTimeline`.
 * `MissionTimelineView.tsx` renders the grouped shape this module produces;
 * this module owns none of the DOM.
 */
import type { GraphEdge, GraphNode, MetricsMap, StepMeter } from "./graph";
import { isAiKind, stepDisplayMetrics, stepEndMs, stepMeterFor, stepStartMs } from "./graph";

/** (#1404) The renderer breakpoint — kept `<= 700` to match the CSS
 * `@media (max-width:700px)` (inclusive at exactly 700px). */
export function isNarrowViewport(width: number): boolean {
  return width <= 700;
}

/** (#1404/#1432) THE renderer decision, in one place. */
export function timelineActive(viewMode: "auto" | "canvas" | "timeline", isMobile: boolean): boolean {
  return viewMode === "timeline" || (viewMode === "auto" && isMobile);
}

const MINIMAP_STORAGE_KEY = "dmux.mmap";

/** `initMinimap` — mission-graph.html. The persisted operator choice wins;
 * absent, defaults OFF at every width (see that file's own #1594 comment —
 * an unlooked-at overview costs attention it doesn't repay, and the widget
 * is genuinely broken in the 701-1000px band). */
export function initMinimap(storage: Pick<Storage, "getItem"> = window.localStorage): boolean {
  try {
    const v = storage.getItem(MINIMAP_STORAGE_KEY);
    if (v === "1") return true;
    if (v === "0") return false;
  } catch {
    // ignore — storage unavailable
  }
  return false;
}

export function persistMinimap(on: boolean, storage: Pick<Storage, "setItem"> = window.localStorage): void {
  try {
    storage.setItem(MINIMAP_STORAGE_KEY, on ? "1" : "0");
  } catch {
    // ignore — storage unavailable
  }
}

/** Deliberately WITHOUT a `tools` field — mission-graph.html's own
 * `taskAggMetrics` never tracks tool-calls at the task-aggregate level,
 * only per-step (see this function's own doc). `StepMeterEl` (shared with
 * the per-step meter) takes `tools` as optional for exactly this reason —
 * a task-level aggregate meter never shows a tool count, matching legacy's
 * untyped `stepMeterEl(agg)` call against a plain object with no `.tools`
 * key. */
export interface TaskAggMetrics {
  show: boolean;
  tokens: number;
  turns: number;
  cloud: boolean;
  generating: boolean;
  /** (#2269) While any step runs: the task SPAN (earliest step start → now),
   * so the row's pulsing timer is the task's, not the running step's. `0`
   * when nothing runs — the meter's pulse is a liveness signal. */
  elapsedMs: number;
  /** (#2269) Earliest step start → latest step end (or now while any step
   * runs). Meaningful in either state; `0` when no step ever started. */
  spanMs: number;
  /** (#2269) Sum of each step's own wall time (running steps count to now).
   * Equals `spanMs` for a strictly sequential task; less when steps idle
   * between each other; more when they overlap. */
  sumMs: number;
  /** (#2269) What the shared meter renders once nothing runs: the task
   * span. Same field the per-step meter uses, so one renderer serves both. */
  wallMs: number;
}

/** `taskAggMetrics` — mission-graph.html. Aggregate a task's step metrics
 * for the collapsed task-card summary. */
export function taskAggMetrics(task: GraphNode, metrics: MetricsMap, now: number): TaskAggMetrics {
  let tokens = 0,
    turns = 0,
    cloud = false,
    generating = false,
    firstStartMs = 0,
    lastEndMs = 0,
    sumMs = 0;
  for (const s of task.steps || []) {
    const m = metrics[s.id];
    const d = stepDisplayMetrics(m);
    tokens += d.tokens;
    turns += d.turns;
    if (d.cloud) cloud = true;
    const running = s.status === "running";
    if (running) generating = true;
    // (#2269) EVERY started step contributes to the task's duration — the
    // old code read the start only off RUNNING steps, so a sequential
    // task's timer restarted with each step and disappeared at the end.
    const st = stepStartMs(s, m);
    if (!st) continue;
    firstStartMs = firstStartMs ? Math.min(firstStartMs, st) : st;
    const en = stepEndMs(s, m) || (running && now ? now : 0);
    if (en) {
      lastEndMs = Math.max(lastEndMs, en);
      sumMs += Math.max(0, en - st);
    }
  }
  const ai = (task.steps || []).some((s) => isAiKind(s.kind));
  const spanMs = firstStartMs && lastEndMs ? Math.max(0, lastEndMs - firstStartMs) : 0;
  const elapsedMs = generating ? spanMs : 0;
  return { show: ai || tokens > 0 || turns > 0 || generating, tokens, turns, cloud, generating, elapsedMs, spanMs, sumMs, wallMs: spanMs };
}

export interface TimelineStep {
  step: NonNullable<GraphNode["steps"]>[number];
  meter: StepMeter;
}

export interface TimelineTask {
  task: GraphNode;
  waitsOn: string[];
  agg: TaskAggMetrics;
  steps: TimelineStep[];
}

export interface TimelinePhase {
  phase: { id: string; label: string; status: string; description?: string };
  tasks: TimelineTask[];
}

/** The phase→task grouping half of `renderMissionTimeline` — mission-graph.html.
 * Returns the plain data shape `MissionTimelineView.tsx` maps to DOM;
 * per-step metric shaping (`stepMeterFor`) is folded in here so the view
 * component stays a pure renderer. A freeform mission with tasks but no
 * phase nodes gets one implicit "tasks" section, matching legacy. */
export function groupTimeline(nodes: GraphNode[], edges: GraphEdge[], metrics: MetricsMap, now: number): TimelinePhase[] {
  const nodeLabel: Record<string, string> = {};
  for (const n of nodes) nodeLabel[n.id] = n.label;

  const waitsOn: Record<string, string[]> = {};
  for (const e of edges) {
    if (e.kind !== "depends_on") continue;
    (waitsOn[e.target] = waitsOn[e.target] || []).push(nodeLabel[e.source] || e.source);
  }

  const phases = nodes.filter((n) => n.kind === "phase").sort((a, b) => a.depth - b.depth);
  const tasksByPhase: Record<string, GraphNode[]> = {};
  for (const n of nodes) {
    if (n.kind !== "task") continue;
    (tasksByPhase[n.parentId || "__none__"] = tasksByPhase[n.parentId || "__none__"] || []).push(n);
  }
  for (const k of Object.keys(tasksByPhase)) tasksByPhase[k].sort((a, b) => a.depth - b.depth);

  let phaseList: Array<{ id: string; label: string; status: string; description?: string }> = phases;
  if (!phaseList.length && (tasksByPhase["__none__"] || []).length) {
    phaseList = [{ id: "__none__", label: "tasks", status: "planned", description: "" }];
  }

  return phaseList.map((phase) => {
    const tasks = tasksByPhase[phase.id] || [];
    return {
      phase,
      tasks: tasks.map((task) => ({
        task,
        waitsOn: waitsOn[task.id] || [],
        agg: taskAggMetrics(task, metrics, now),
        steps: (task.steps || []).map((step) => ({ step, meter: stepMeterFor(step, metrics, now) })),
      })),
    };
  });
}
