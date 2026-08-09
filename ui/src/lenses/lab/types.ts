// Shapes served by `GET /lab/runs` (see `crates/darkmux-serve/src/lib.rs`'s
// lab handlers and `tests/parity/corpus/lab-runs.json`).
//
// Hand-written rather than ts-rs-generated: the lab payload has no ts-rs
// derives yet, and adding them is a separate change (it needs a cargo pass
// and a drift-guard entry). Typed by hand here so the lab lens is not
// blocked on that; replace with generated types when they land.

/** One seat's recorded staffing knobs, as snapshotted into the envelope. */
export interface LabSeat {
  name: string;
  model?: string | null;
  k?: number | null;
  max_tokens?: number | null;
  n_ctx?: number | null;
}

/** The `staffing` snapshot (#1247): what the run was actually configured as. */
export interface LabStaffing {
  probes?: LabSeat[] | null;
  judge?: LabSeat | null;
}

/** One lab run directory as `/lab/runs` reports it. */
export interface LabRun {
  dir: string;
  mtime_ms: number;
  case_ids: string[];
  bundles: number;
  raw_flags: number;
  deduped_flags: number;
  confirmed: number;
  needs_check: number;
  archived: number;
  degenerate: boolean;
  finished: boolean;
  has_funnels: boolean;
  has_events: boolean;
  /** Absent on older artifacts AND on runs whose first case hasn't finished. */
  staffing?: LabStaffing | null;
  crew?: string | null;
  exec_mode?: string | null;
}

/** Runs over the same corpus, newest-first, keyed by their sorted case ids. */
export interface LabSeries {
  key: string;
  runs: LabRun[];
}
