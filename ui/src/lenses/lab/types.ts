// Shapes served by `GET /lab/runs` (see `crates/darkmux-serve/src/lib.rs`'s
// lab handlers and `tests/parity/corpus/lab-runs.json`).
//
// These used to be hand-rolled again here, duplicating `LabRun`/
// `StaffingSnapshot`/`SeatStaffing` already defined in
// `src/types/handwritten.ts` (the project's one home for hand-written wire
// types — see that file's own doc). Reconciled onto the existing types
// instead of maintaining two copies of the same wire shape: `handwritten.ts`
// widened `SeatStaffing.model`/`.k` to optional and `LabRun.crew`/
// `.exec_mode`/`.staffing` to accept an explicit `null` (both needed by the
// pure logic in `./labSeries.ts` and its differential-tested spec), plus
// grew `has_funnels`/`has_events` (present on every real `/lab/runs` row —
// see the corpus fixture — but missing from the pre-existing type).
//
// `LabSeries` stays here rather than moving to `handwritten.ts`: it's
// lens-DERIVED data (`groupLabRunsByTask`'s grouped-by-corpus output), not a
// shape `/lab/runs` itself ever sends over the wire.
import type { LabRun } from "../../types/handwritten";

export type { LabRun, StaffingSnapshot as LabStaffing, SeatStaffing as LabSeat } from "../../types/handwritten";

/** Runs over the same corpus, newest-first, keyed by their sorted case ids. */
export interface LabSeries {
  key: string;
  runs: LabRun[];
}
