/**
 * The runs board's dimension filters (#2925): which runs a selection keeps,
 * and the facet counts the filter bar shows. Pure functions only; the
 * components are thin on top.
 *
 * Semantics: values within one dimension OR together, dimensions AND together.
 * A value's count is the number of runs that would show if that value were the
 * dimension's selection, given every OTHER dimension's selection (a standard
 * facet count), so a count never depends on the dimension it is listed under.
 */

import type { Run } from "../../types/generated/Run";
import { FILTER_DIMS, TIME_WINDOWS, isTimeWindow, type FilterDim, type FilterSel } from "../../lib/runsFilterQuery";
import { runActivity, runStatusLabel, runVerifyWord, shortModel } from "./format";

/** The value a field that CAN be set but is not reads as. */
export const NOT_SET = "(not set)";

/** What the filters need beyond the run itself. `now` is milliseconds. */
export interface FilterEnv {
  now: number;
  /** A run's machine identity key (`format.ts::machineKeyOfRuns`), or `null`. */
  machineOf: (r: Run) => string | null;
  /** Whether a run's machine is not reporting (`viewRows.machineNotReporting`):
   *  its running reads `unknown`, in the status filter as on the badge. */
  notReporting: (r: Run) => boolean;
}

/** Each discrete dimension's getter. `null` means the dimension does not apply
 * to the run (workload and verify are lab-only); such a run matches no
 * selection and is not counted under any value. */
const GETTERS: Record<Exclude<FilterDim, "time">, (r: Run, env: FilterEnv) => string | null> = {
  status: (r, env) => runStatusLabel(r, env.notReporting(r)) || NOT_SET,
  machine: (r, env) => env.machineOf(r) ?? NOT_SET,
  model: (r) => shortModel(r.model) || NOT_SET,
  role: (r) => r.role || NOT_SET,
  workload: (r) => (r.kind === "lab" ? r.workload || NOT_SET : null),
  verify: (r) => runVerifyWord(r),
  route: (r) => r.route || NOT_SET,
  tracked: (r) => (r.tracked ? "tracked" : "untracked"),
};

/** A run's value under a discrete dimension. `time` is a window, not a value,
 * so it has none. */
export function valueOf(r: Run, dim: FilterDim, env: FilterEnv): string | null {
  return dim === "time" ? null : GETTERS[dim](r, env);
}

/** Whether `r` falls inside the time window `win` (seconds back from now). */
function withinWindow(r: Run, win: number, env: FilterEnv): boolean {
  return runActivity(r) >= Math.floor(env.now / 1000) - win;
}

/** Whether `r` satisfies the selection `values` on `dim`. An empty selection
 * keeps every run. */
function keeps(r: Run, dim: FilterDim, values: string[], env: FilterEnv): boolean {
  if (values.length === 0) return true;
  if (dim === "time") return isTimeWindow(values[0]) && withinWindow(r, TIME_WINDOWS[values[0]], env);
  const v = valueOf(r, dim, env);
  return v !== null && values.includes(v);
}

/** The runs every dimension keeps, except `skip` when given. */
export function applyFilters(runs: Run[], sel: FilterSel, env: FilterEnv, skip?: FilterDim): Run[] {
  const active = FILTER_DIMS.filter((d) => d !== skip && sel[d].length > 0);
  return runs.filter((r) => active.every((d) => keeps(r, d, sel[d], env)));
}

/** One selectable value of a dimension with the runs it would show. */
export interface FacetChoice {
  value: string;
  count: number;
}

/** Order by count, then name, with "(not set)" always last. */
function byCountThenName(a: FacetChoice, b: FacetChoice): number {
  if ((a.value === NOT_SET) !== (b.value === NOT_SET)) return a.value === NOT_SET ? 1 : -1;
  return b.count - a.count || a.value.localeCompare(b.value);
}

/** The time choices, widest window last. Each counts the runs within it. */
function timeChoices(base: Run[], env: FilterEnv): FacetChoice[] {
  return (Object.keys(TIME_WINDOWS) as (keyof typeof TIME_WINDOWS)[]).map((value) => ({
    value,
    count: base.filter((r) => withinWindow(r, TIME_WINDOWS[value], env)).length,
  }));
}

/**
 * The choices for `dim`, counted over every OTHER dimension's selection. A
 * value no run in view carries is left out unless it is selected, so a
 * selection that now matches nothing still shows (count 0) and can be
 * unticked. Sorted by count, "(not set)" last; time keeps its window order.
 */
export function facetChoices(runs: Run[], sel: FilterSel, dim: FilterDim, env: FilterEnv): FacetChoice[] {
  const base = applyFilters(runs, sel, env, dim);
  if (dim === "time") return timeChoices(base, env);
  const counts = new Map<string, number>();
  for (const r of base) {
    const v = valueOf(r, dim, env);
    if (v !== null) counts.set(v, (counts.get(v) ?? 0) + 1);
  }
  for (const v of sel[dim]) if (!counts.has(v)) counts.set(v, 0);
  return [...counts].map(([value, count]) => ({ value, count })).sort(byCountThenName);
}

/** How many runs `dim`'s other filters leave in view: the total its choices
 * narrow from. */
export function facetTotal(runs: Run[], sel: FilterSel, dim: FilterDim, env: FilterEnv): number {
  return applyFilters(runs, sel, env, dim).length;
}

/** Whether choosing something under `dim` could change what is in view: false
 * when every run in view carries the same value (or sits in every window) and
 * nothing is selected yet, which is what the bar dims. */
export function canNarrow(choices: FacetChoice[], total: number, selected: string[]): boolean {
  return selected.length > 0 || choices.some((c) => c.count < total);
}

/** The counts the kind tabs show: each kind's runs under every dimension
 * filter. `all` is every kind. */
export function kindCounts(runs: Run[], kinds: readonly string[]): Record<string, number> {
  const counts: Record<string, number> = { all: runs.length };
  for (const k of kinds) if (k !== "all") counts[k] = runs.filter((r) => r.kind === k).length;
  return counts;
}

/** The pill and chip name of each dimension. */
export const DIM_LABEL: Record<FilterDim, string> = {
  time: "Time",
  status: "Status",
  machine: "Machine",
  model: "Model",
  role: "Role",
  workload: "Workload",
  verify: "Verify",
  route: "Route",
  tracked: "Tracked",
};

/** What each time window reads as. */
const TIME_LABEL: Record<keyof typeof TIME_WINDOWS, string> = {
  "1h": "Last hour",
  "24h": "Last 24 hours",
  "7d": "Last 7 days",
  "30d": "Last 30 days",
};

/** One dimension as the filter bar draws it. */
export interface FacetView {
  dim: FilterDim;
  choices: (FacetChoice & { label: string })[];
  /** The selected values, in selection order. */
  selected: string[];
  /** False when every run in view shares one value: the pill dims, and
   * still opens. */
  narrows: boolean;
}

/** Every dimension's facet, in bar order. The list never loses or gains a
 * dimension, whatever is selected: a pill keeps its place. `labelOf` names a
 * value for display (machines show their current name, not their identity). */
export function buildFacets(runs: Run[], sel: FilterSel, env: FilterEnv, labelOf: (dim: FilterDim, value: string) => string): FacetView[] {
  return FILTER_DIMS.map((dim) => {
    const choices = facetChoices(runs, sel, dim, env);
    const total = facetTotal(runs, sel, dim, env);
    return {
      dim,
      choices: choices.map((c) => ({ ...c, label: dim === "time" && isTimeWindow(c.value) ? TIME_LABEL[c.value] : labelOf(dim, c.value) })),
      selected: sel[dim],
      narrows: canNarrow(choices, total, sel[dim]),
    };
  });
}

/** The text on a pill for its selection: the one value, or a count. */
export function selectionText(facet: FacetView): string | null {
  if (facet.selected.length === 0) return null;
  if (facet.selected.length > 1) return `${facet.selected.length} selected`;
  const [only] = facet.selected;
  return facet.choices.find((c) => c.value === only)?.label ?? only;
}

/** (#2925) The runs board's one count line. `rendered` is the rows on screen
 * (the newest page unless expanded), `matching` the rows the kind tab and
 * filters keep, `total` every run under the kind tab. Each part is said only
 * when it differs from the next, so an unfiltered, uncapped board reads
 * "Showing 12 runs". */
export function runsCountText({ rendered, matching, total }: { rendered: number; matching: number; total: number }): string {
  const runs = (n: number) => `${n} run${n === 1 ? "" : "s"}`;
  const capped = rendered < matching;
  if (matching === total) return capped ? `Showing newest ${rendered} of ${runs(total)}` : `Showing ${runs(total)}`;
  return capped ? `Showing newest ${rendered} of ${matching} matching (${runs(total)})` : `Showing ${matching} of ${runs(total)}`;
}
