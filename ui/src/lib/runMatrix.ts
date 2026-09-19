import type { RunKind } from "../types/generated/RunKind";
import type { RunStatus } from "../types/generated/RunStatus";
import type { AbandonReason } from "../types/generated/AbandonReason";

/**
 * (#2813) THE RUN STATE MATRIX — every kind of run, in every state it can
 * actually reach.
 *
 * Operator: *"there should be a test for every kind of run, in any kind of
 * state... this shouldn't be that difficult with all the kinds and statuses
 * registered as canonical enums."*
 *
 * Both axes ARE canonical enums, generated from Rust by `ts_rs` and kept
 * current by the drift guard in `quality.yml`. The table below is typed as
 * `Record<RunKind, Record<RunStatus, boolean>>`, so it is not a hand-written
 * list that can fall behind: omitting a kind, or a status within a kind, is a
 * COMPILE ERROR. A new variant on either axis breaks the build here until
 * somebody decides whether that cell is reachable.
 *
 * REACHABILITY IS NOT THE CARTESIAN PRODUCT. Three server-side mappers decide
 * a run's status and they reach different subsets
 * (`crates/darkmux-serve/src/runs.rs`):
 *
 * - `mission_run_status` — missions, and dispatches, which are a SHAPE of
 *   mission (`classify_mission`) and therefore share its mapper. All six.
 * - `lab_run_status` — lab runs. No `planned` (a lab run exists because it
 *   was dispatched) and no `unparseable` (that verdict comes from a mission
 *   envelope a lab run does not have).
 * - `ghost_runs` — untracked, flow-only sessions. Only what the flow stream
 *   alone can prove.
 *
 * A 3x6 grid would contain 13 cells that cannot occur. Testing those would be
 * fabricating states the system does not have, which is its own kind of lie.
 */
export const REACHABLE: Record<RunKind, Record<RunStatus, boolean>> = {
  mission: {
    planned: true,
    running: true,
    complete: true,
    error: true,
    abandoned: true,
    unparseable: true,
  },
  dispatch: {
    planned: true,
    running: true,
    complete: true,
    error: true,
    abandoned: true,
    unparseable: true,
  },
  lab: {
    planned: false,
    running: true,
    complete: true,
    error: true,
    abandoned: true,
    unparseable: false,
  },
};

/** Untracked (flow-only) runs, which `ghost_runs` synthesises. A separate
 * axis from `kind`: a ghost can be any kind, but only these statuses are
 * derivable from flow records with no run directory behind them. */
export const REACHABLE_UNTRACKED: Record<RunStatus, boolean> = {
  planned: false,
  running: true,
  complete: true,
  error: false,
  abandoned: true,
  unparseable: false,
};

/** `abandoned` carries a reason on the wire; every other status does not.
 * Splitting it here is what makes "aborted" and "no ending recorded"
 * separately testable rather than collapsing into one word. */
export const ABANDON_REASONS: readonly AbandonReason[] = ["aborted", "noterminal"];

export interface MatrixCell {
  kind: RunKind;
  status: RunStatus;
  tracked: boolean;
  abandonReason?: AbandonReason;
  /** Stable, human-readable cell id for test names and failure messages. */
  id: string;
}

/** Every reachable cell, derived from the tables above rather than listed. */
export function matrixCells(): MatrixCell[] {
  const cells: MatrixCell[] = [];
  const kinds = Object.keys(REACHABLE) as RunKind[];
  for (const kind of kinds) {
    for (const [status, trackedReachable] of Object.entries(REACHABLE[kind]) as Array<
      [RunStatus, boolean]
    >) {
      for (const tracked of [true, false]) {
        const reachable = tracked ? trackedReachable : REACHABLE_UNTRACKED[status];
        if (!reachable) continue;
        if (status === "abandoned") {
          for (const abandonReason of ABANDON_REASONS) {
            cells.push({
              kind,
              status,
              tracked,
              abandonReason,
              id: `${kind}/${status}(${abandonReason})/${tracked ? "tracked" : "ghost"}`,
            });
          }
        } else {
          cells.push({
            kind,
            status,
            tracked,
            id: `${kind}/${status}/${tracked ? "tracked" : "ghost"}`,
          });
        }
      }
    }
  }
  return cells;
}
