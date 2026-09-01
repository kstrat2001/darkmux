/**
 * The step drill-in's small header block (#2189) -- renders directly ABOVE
 * the mainstay events column (both the desktop right-column mount and the
 * phone bottom sheet's Events tab, via `EventLogColumn`'s own `headerExtra`
 * slot) whenever a mission step is selected. Two jobs: name the unit
 * (`scopeLabel`) with a one-tap way back to the whole mission
 * (`onBack`), and lay out whatever step fields `graph.ts::buildStepHeaderFields`
 * actually found -- PROGRESSIVE by construction (that function's own doc),
 * so this component renders exactly the fields it's handed, never a
 * placeholder for one that's missing.
 *
 * Rendered even when the step has ZERO events -- the #2163 lesson this
 * packet's brief names explicitly: a selected step with no records must
 * show the header + `EventLogColumn`'s own existing empty state, never a
 * nonzero record count with a blank body. This component doesn't know or
 * care how many records exist; `EventLogColumn` already renders its own
 * honest empty state from an empty `records` array, so there is nothing
 * extra to gate here -- mounting this block is unconditional on "a step is
 * selected", not on "the step has records".
 */
import type { StepHeaderField } from "./graph";

export function StepHeaderBlock({ missionId, fields, onBack }: { missionId: string; fields: StepHeaderField[]; onBack: () => void }) {
  const unit = fields.find((f) => f.key === "unit")?.value ?? "step";
  const rest = fields.filter((f) => f.key !== "unit");
  return (
    <div className="stephdr" data-act="step-header">
      <div className="stephdr__top">
        <button type="button" className="stephdr__back" data-act="step-back" title={`back to ${missionId}`} onClick={onBack}>
          {"‹"} mission
        </button>
        <span className="stephdr__unit" title={unit}>
          {unit}
        </span>
      </div>
      {rest.length ? (
        <dl className="stephdr__fields">
          {rest.map((f) => (
            <div className="stephdr__field" key={f.key}>
              <dt>{f.label}</dt>
              <dd title={f.value}>{f.value}</dd>
            </div>
          ))}
        </dl>
      ) : null}
    </div>
  );
}
