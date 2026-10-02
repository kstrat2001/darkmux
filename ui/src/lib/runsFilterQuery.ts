/**
 * The runs board's dimension filters as they ride in the hash (#2925): the
 * dimension list, the selection shape, and the parse/serialize pair. Pure and
 * dependency-free so `route.ts` and `hashSync.ts` can use it without importing
 * the board.
 *
 * Grammar: one param per dimension, repeated once per selected value
 * (`status=complete&status=running`), except `time`, which is a single window.
 * `machine` values are machine KEYS (`lib/machineKey.ts`), never uids.
 */

/** The filter bar's dimensions, in bar order. */
export const FILTER_DIMS = ["time", "status", "machine", "model", "role", "workload", "verify", "route", "tracked"] as const;
export type FilterDim = (typeof FILTER_DIMS)[number];

/** Each dimension's selected values; empty means no filter on it. */
export type FilterSel = Record<FilterDim, string[]>;

/** The time windows, as the hash spells them, with their length in seconds. */
export const TIME_WINDOWS = { "1h": 3600, "24h": 86400, "7d": 7 * 86400, "30d": 30 * 86400 } as const;
export type TimeWindow = keyof typeof TIME_WINDOWS;

export const isTimeWindow = (v: string): v is TimeWindow => Object.prototype.hasOwnProperty.call(TIME_WINDOWS, v);

export function emptyFilterSel(): FilterSel {
  return { time: [], status: [], machine: [], model: [], role: [], workload: [], verify: [], route: [], tracked: [] };
}

/** Whether no dimension carries a selection. */
export const isFilterSelEmpty = (sel: FilterSel): boolean => FILTER_DIMS.every((d) => sel[d].length === 0);

/** Read the selection out of `getAll` (a repeated-param reader). An unknown
 * time window is dropped rather than guessed at. */
export function parseFilterSel(getAll: (name: string) => string[]): FilterSel {
  const sel = emptyFilterSel();
  for (const dim of FILTER_DIMS) {
    const values = getAll(dim).map((v) => v.trim()).filter((v) => v !== "");
    sel[dim] = dim === "time" ? values.filter(isTimeWindow).slice(0, 1) : [...new Set(values)];
  }
  return sel;
}

/** The `[param, value]` pairs a selection writes, in bar order. */
export function filterSelPairs(sel: FilterSel): [string, string][] {
  return FILTER_DIMS.flatMap((dim) => sel[dim].map((v): [string, string] => [dim, v]));
}
