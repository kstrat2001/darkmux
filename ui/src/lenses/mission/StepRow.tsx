/**
 * Shared step-row vocabulary (#1868, "Concept A" — hollow ring + tree rail +
 * muted-caps kind lead) — the SAME four-part row `mission-graph.html`'s
 * `stepRowEls` renders, reused by both `MissionCanvas.tsx` (inside a task
 * card, `.mn-step-row`) and `MissionTimelineView.tsx` (`.tlt-step`), so the
 * sub-item markup is byte-identical between the two renderers, matching the
 * legacy page's own "one shared block, two callers" design.
 */
import type { ReactNode } from "react";
import { fmtElapsed, fmtModel, fmtTok, stepLead, stepSeat, type GraphStep, type StepMeter } from "./graph";

/** `tools` is OPTIONAL here (unlike {@link StepMeter}'s own required field)
 * so this same renderer also takes a task-level {@link
 * import("./timeline").TaskAggMetrics}, which never tracks tool-calls at
 * the aggregate level — see that type's own doc. */
type MeterLike = Omit<StepMeter, "tools"> & { tools?: number };

export function StepMeterEl({ meter }: { meter: MeterLike | undefined }) {
  if (!meter || !meter.show) return null;
  const children: ReactNode[] = [];
  if (meter.generating) {
    children.push(
      <span key="g" className="gen">
        <span className="genpulse" />
        {meter.elapsedMs ? fmtElapsed(meter.elapsedMs) : "live"}
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
 * from a bare `.steprow`). */
export function StepRow({ step, meter, extraClass }: { step: GraphStep; meter: StepMeter | undefined; extraClass: string }) {
  const seat = stepSeat(step.kind);
  return (
    <div className={`steprow ${extraClass} s-${step.status}`} title={step.label}>
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
