/**
 * Shared step-row vocabulary (#1868, "Concept A" — hollow ring + tree rail +
 * muted-caps kind lead) — the SAME four-part row `mission-graph.html`'s
 * `stepRowEls` renders, reused by both `MissionCanvas.tsx` (inside a task
 * card, `.mn-step-row`) and `MissionTimelineView.tsx` (`.tlt-step`), so the
 * sub-item markup is byte-identical between the two renderers, matching the
 * legacy page's own "one shared block, two callers" design.
 */
import { workStatusKind } from "../../components/WorkStatus";
import type { ReactNode } from "react";
import { fmtModel, fmtTok, stepLead, stepSeat, type GraphStep, type StepMeter } from "./graph";
import { fmtElapsed } from "../../lib/format";
import { onIntentClick } from "../../lib/clickIntent";
import type { PhaseElapsed } from "./timeline";

/** `tools` is OPTIONAL here (unlike {@link StepMeter}'s own required field)
 * so this same renderer also takes a task-level {@link
 * import("./timeline").TaskAggMetrics}, which never tracks tool-calls at
 * the aggregate level — see that type's own doc. */
type MeterLike = Omit<StepMeter, "tools" | "wallMs" | "waiting"> & { tools?: number; wallMs?: number; waiting?: boolean };

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
const STEP_TIME_TITLE = "step time — the whole step span: setup, model work and gate. The run page's ACTIVE TIME (WALL CLOCK for a step with no model) is the dispatch's own run, and reads shorter.";

export function StepMeterEl({ meter }: { meter: MeterLike | undefined }) {
  if (!meter || !meter.show) return null;
  const time = timeBadge(meter);
  const children: ReactNode[] = [...(time ? [time] : []), ...countBadges(meter)];
  if (!children.length) {
    return (
      <span className="mn-step-meter">
        <span className="idle">· tok</span>
      </span>
    );
  }
  return <span className="mn-step-meter">{children}</span>;
}

/** The meter's time: the pulse while it works, its wall time once done, or
 *  (#3195) "waiting" while it is queued for its call slot, with no timer. */
function timeBadge(meter: MeterLike): ReactNode {
  if (meter.generating) {
    return (
      <span key="g" className="gen" data-hint={STEP_TIME_HINT} title={STEP_TIME_TITLE}>
        <span className="genpulse" />
        {meter.elapsedMs ? fmtElapsed(meter.elapsedMs) : "live"}
      </span>
    );
  }
  if (meter.waiting) {
    return (
      <span key="q" className="waiting" title="waiting for a call slot: its time starts when it gets one">
        waiting
      </span>
    );
  }
  if (meter.wallMs) {
    // (#2269) Finished: the pulse is gone, so the wall time stands alone.
    return (
      <span key="w" className="wall" data-hint={STEP_TIME_HINT} title={STEP_TIME_TITLE}>
        {fmtElapsed(meter.wallMs)}
      </span>
    );
  }
  return null;
}

/** The meter's counts: tokens, turns, tool calls, each only when nonzero. */
function countBadges(meter: MeterLike): ReactNode[] {
  const out: ReactNode[] = [];
  if (meter.tokens) {
    // (#2834) No `cloud` class here. It painted these tokens in the WARN
    // colour, so a step served by a local inference engine read as a cost
    // warning. The flag it keyed on is endpoint presence, which is not a
    // cost fact — see MissionGraphLens.
    out.push(
      <span key="t" className="tok">
        {fmtTok(meter.tokens) + " tok"}
      </span>,
    );
  }
  if (meter.turns) out.push(<span key="n">{meter.turns + (meter.turns === 1 ? " turn" : " turns")}</span>);
  if (meter.tools) out.push(<span key="c">{meter.tools + (meter.tools === 1 ? " tool" : " tools")}</span>);
  return out;
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
      data-status-kind={workStatusKind(step.status, step.abandonedReason)}
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
              onIntentClick(() => onSelect!(step.id))(e);
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

/** (#3195, operator 2026-10-10) A phase heading's running total, in the
 *  mission header's words: "N elapsed" while it runs, "ran N" once done. */
export function PhaseTime({ elapsed, className }: { elapsed: PhaseElapsed | null; className: string }) {
  if (!elapsed) return null;
  const t = fmtElapsed(elapsed.ms);
  return (
    <span className={className} title="phase time: its first step's start until now, or until its last step ended">
      {elapsed.running ? `${t} elapsed` : `ran ${t}`}
    </span>
  );
}
