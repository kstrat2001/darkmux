/**
 * The mobile vertical-timeline renderer (#1868, #1404 lineage) — a straight
 * port of `mission-graph.html`'s `renderMissionTimeline`, consuming the
 * grouped shape `timeline.ts::groupTimeline` produces. DOM class vocabulary
 * (`.tlphase`, `.tltask`, `.tlt-hd`, `.tlt-steps`, `.tlt-step`) is kept
 * IDENTICAL to the legacy page for the same reason `MissionCanvas.tsx` keeps
 * `.mnode`/`.phasegroup` — the parity goldens and the e2e specs both key on
 * it.
 */
import { StepMeterEl, StepRow } from "./StepRow";
import { groupTimeline, type TaskAggMetrics } from "./timeline";
import type { GraphEdge, GraphNode, MetricsMap } from "./graph";

function TltCount({ open, stepCount }: { open: boolean; stepCount: number }) {
  if (open || stepCount === 0) return <span className="tlt-count" />;
  return <span className="tlt-count">{stepCount + (stepCount === 1 ? " step" : " steps")}</span>;
}

function TaskCard({
  task,
  waitsOn,
  agg,
  steps,
  open,
  onToggle,
  selectedStepId,
  onSelectStep,
}: {
  task: GraphNode;
  waitsOn: string[];
  agg: TaskAggMetrics;
  steps: ReturnType<typeof groupTimeline>[number]["tasks"][number]["steps"];
  open: boolean;
  onToggle: () => void;
  /** (#2189, step drill-in) See `MissionGraphLens`'s own doc. */
  selectedStepId?: string | null;
  onSelectStep?: (stepId: string) => void;
}) {
  return (
    <div className={`tltask s-${task.status}${open ? " open" : ""}`}>
      <div
        className="tlt-hd"
        role="button"
        tabIndex={0}
        onClick={onToggle}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onToggle();
          }
        }}
      >
        <span className="tlt-dot" />
        <span className="tlt-name" title={task.description || task.label}>
          {task.label}
        </span>
        <TltCount open={open} stepCount={(task.steps || []).length} />
        <span className="tlt-meter-cell">
          <StepMeterEl meter={agg} />
        </span>
        <span className="tlt-chev">▸</span>
      </div>
      {waitsOn.length ? <div className="tlt-waits">waits on: {waitsOn.join(", ")}</div> : null}
      {open ? (
        <div className="tlt-steps">
          {steps.length ? (
            steps.map(({ step, meter }) => (
              <StepRow
                key={step.id}
                step={step}
                meter={meter}
                extraClass="tlt-step"
                selected={selectedStepId === step.id}
                onSelect={onSelectStep}
              />
            ))
          ) : (
            <div className="tlt-empty">no steps</div>
          )}
        </div>
      ) : null}
    </div>
  );
}

export function MissionTimelineView({
  nodes,
  edges,
  metrics,
  now,
  note,
  expanded,
  onToggleTask,
  selectedStepId,
  onSelectStep,
}: {
  nodes: GraphNode[];
  edges: GraphEdge[];
  metrics: MetricsMap;
  now: number;
  note?: string;
  expanded: Record<string, boolean>;
  onToggleTask: (id: string) => void;
  /** (#2189, step drill-in) See `MissionGraphLens`'s own doc. */
  selectedStepId?: string | null;
  onSelectStep?: (stepId: string) => void;
}) {
  const groups = groupTimeline(nodes, edges, metrics, now);
  return (
    <div className="timeline missionlens__timeline">
      {note ? <div className="tlnote">{note}</div> : null}
      {groups.map(({ phase, tasks }) => (
        <div key={phase.id} className={`tlphase s-${phase.status}`}>
          <div className="tlph-hd">
            <span className="tlph-dot" />
            <span className="tlph-name" title={phase.description || phase.label}>
              {phase.label}
            </span>
            <span className="tlph-tag">{phase.status}</span>
          </div>
          <div className="tltasks">
            {tasks.length ? (
              tasks.map(({ task, waitsOn, agg, steps }) => (
                <TaskCard
                  key={task.id}
                  task={task}
                  waitsOn={waitsOn}
                  agg={agg}
                  steps={steps}
                  open={!!expanded[task.id]}
                  onToggle={() => onToggleTask(task.id)}
                  selectedStepId={selectedStepId}
                  onSelectStep={onSelectStep}
                />
              ))
            ) : (
              <div className="tlt-waits" style={{ padding: "6px 4px" }}>
                no tasks in this phase
              </div>
            )}
          </div>
        </div>
      ))}
    </div>
  );
}
