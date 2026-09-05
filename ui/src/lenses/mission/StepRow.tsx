/**
 * Shared step-row vocabulary (#1868, "Concept A" — hollow ring + tree rail +
 * muted-caps kind lead) — the SAME four-part row `mission-graph.html`'s
 * `stepRowEls` renders, reused by both `MissionCanvas.tsx` (inside a task
 * card, `.mn-step-row`) and `MissionTimelineView.tsx` (`.tlt-step`), so the
 * sub-item markup is byte-identical between the two renderers, matching the
 * legacy page's own "one shared block, two callers" design.
 */
import type { ReactNode } from "react";
import { fmtModel, fmtTok, stepLead, stepSeat, type GraphStep, type StepMeter } from "./graph";
import { fmtElapsed } from "../../lib/format";

/** `tools` is OPTIONAL here (unlike {@link StepMeter}'s own required field)
 * so this same renderer also takes a task-level {@link
 * import("./timeline").TaskAggMetrics}, which never tracks tool-calls at
 * the aggregate level — see that type's own doc. */
type MeterLike = Omit<StepMeter, "tools" | "wallMs"> & { tools?: number; wallMs?: number };

/** (U3-6) What this badge's duration actually measures, spelled out. The
 * number here is the STEP SPAN — `stepStartMs` → `stepEndMs`, which brackets
 * setup, the model's own work, and the gate around it. The session drill-in's
 * WALL CLOCK tile is a NARROWER quantity, the dispatch's own `wall_ms`, so
 * the same step legitimately reads 10:36 here and 10:07 there. Neither screen
 * used to say so; both do now, through the same `data-hint` + `title` pair.
 *
 * `data-hint` rather than a text node on purpose: `tests/parity/lib/
 * extract-graph.js` reads `.mn-step-meter`'s `textContent` into the frozen
 * `goldens/mission-graph-*.txt`, and the session lens's own pane labels
 * already establish the CSS-generated form as this project's answer to
 * "a label that should be seen but not enter the golden". */
const STEP_TIME_HINT = "step";
const STEP_TIME_TITLE = "step time — the whole step span: setup, model work and gate. The session drill-in's WALL CLOCK is the model's own run, and reads shorter.";

export function StepMeterEl({ meter }: { meter: MeterLike | undefined }) {
  if (!meter || !meter.show) return null;
  const children: ReactNode[] = [];
  if (meter.generating) {
    children.push(
      <span key="g" className="gen" data-hint={STEP_TIME_HINT} title={STEP_TIME_TITLE}>
        <span className="genpulse" />
        {meter.elapsedMs ? fmtElapsed(meter.elapsedMs) : "live"}
      </span>,
    );
  }
  if (!meter.generating && meter.wallMs) {
    // (#2269) Finished: the pulse is gone, so the wall time stands alone.
    children.push(
      <span key="w" className="wall" data-hint={STEP_TIME_HINT} title={STEP_TIME_TITLE}>
        {fmtElapsed(meter.wallMs)}
      </span>,
    );
  }
  if (meter.tokens) {
    children.push(
      <span key="t" className={"tok" + (meter.cloud ? " cloud" : "")}>
        {fmtTok(meter.tokens) + " tok"}
      </span>,
    );
  }
  if (meter.turns) {
    children.push(<span key="n">{meter.turns + (meter.turns === 1 ? " turn" : " turns")}</span>);
  }
  if (meter.tools) {
    children.push(<span key="c">{meter.tools + (meter.tools === 1 ? " tool" : " tools")}</span>);
  }
  if (!children.length) {
    return (
      <span className="mn-step-meter">
        <span className="idle">· tok</span>
      </span>
    );
  }
  return <span className="mn-step-meter">{children}</span>;
}

/** `stepRowEls` — mission-graph.html. `extraClass` distinguishes the canvas
 * card's row (`mn-step-row`) from the timeline's (`tlt-step`) — the SAME
 * distinction legacy's own dual call sites make, kept for parity-extractor
 * selector compatibility (`extract-graph.js` selects `.tlt-step` separately
 * from a bare `.steprow`).
 *
 * (#2189, step drill-in) `onSelect`/`selected` are optional so every
 * existing call site (and every existing parity/golden test) that doesn't
 * pass them keeps rendering byte-identically — `onSelect` absent means "no
 * drill-in for this row" (a `<div>`, not a `<button>`, exactly as before).
 * When present, the row becomes keyboard-activatable
 * (`role="button" tabIndex={0}`, the same `Enter`/`Space` pattern
 * `MissionTimelineView.tsx`'s own `.tlt-hd` toggle already uses) and
 * `stopPropagation`s its click — this row can be nested INSIDE a React Flow
 * node (the canvas's `.mn-step-row` case), and without the stop, a tap here
 * would also bubble up to `MissionCanvas`'s own `onNodeClick`, which
 * resolves the SAME id for a single-step node, so it never disagrees — it
 * would just be redundant work, not a bug. Stopping it is still the right
 * call: it keeps the row the sole source of truth for its own click,
 * exactly like `MissionCanvas`'s node click already defers to a step row
 * when one exists (see that file's own doc). */
export function StepRow({
  step,
  meter,
  extraClass,
  selected = false,
  onSelect,
}: {
  step: GraphStep;
  meter: StepMeter | undefined;
  extraClass: string;
  selected?: boolean;
  onSelect?: (stepId: string) => void;
}) {
  const seat = stepSeat(step.kind);
  const clickable = !!onSelect;
  return (
    <div
      className={`steprow ${extraClass} s-${step.status}${selected ? " selected" : ""}`}
      title={step.label}
      data-act={clickable ? "step-row" : undefined}
      data-selected={selected ? "1" : undefined}
      role={clickable ? "button" : undefined}
      tabIndex={clickable ? 0 : undefined}
      aria-pressed={clickable ? selected : undefined}
      onClick={
        clickable
          ? (e) => {
              e.stopPropagation();
              onSelect!(step.id);
            }
          : undefined
      }
      onKeyDown={
        clickable
          ? (e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                e.stopPropagation();
                onSelect!(step.id);
              }
            }
          : undefined
      }
    >
      <span className="sring" />
      <span className="slead">{stepLead(step)}</span>
      {seat ? <span className="sname">{seat}</span> : null}
      {step.model ? (
        <span className="smodel" title={step.model}>
          {fmtModel(step.model)}
        </span>
      ) : null}
      <StepMeterEl meter={meter} />
    </div>
  );
}
