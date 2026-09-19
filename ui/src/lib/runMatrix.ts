import type { RunKind } from "../types/generated/RunKind";
import type { RunStatus } from "../types/generated/RunStatus";
import type { AbandonReason } from "../types/generated/AbandonReason";
import type { Run } from "../types/generated/Run";

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

/**
 * Untracked (flow-only) runs — the rows synthesised from flow records with
 * no run directory behind them.
 *
 * (#2812) This used to be one flat `Record<RunStatus, boolean>` applied to
 * every kind, on the reading that "a ghost can be any kind". It cannot,
 * and the flat table was wrong in both directions — it invented five
 * untracked LAB cells that no code path can produce, and it denied
 * `dispatch/error`, which `terminal_status_for_action` reaches on any
 * `dispatch error` record. Two server functions decide this, and each
 * synthesises exactly ONE kind:
 *
 * - `ghost_runs` -> `RunKind::Dispatch`, always. Terminal records give
 *   `complete` (`dispatch complete`), `error` (`dispatch error`) and
 *   `abandoned` (`session.end`); the staleness gate gives `running` or
 *   `abandoned`.
 * - `flow_mission_to_run` -> `RunKind::Mission`, always. A peer's mission,
 *   judged by its own terminal record and its sessions' liveness. No
 *   envelope is readable from here, so neither `error` nor `unparseable`.
 * - There is NO untracked lab row. A lab row exists only when this daemon
 *   can see the run directory, which is what `tracked` means for that kind.
 */
export const REACHABLE_UNTRACKED: Record<RunKind, Record<RunStatus, boolean>> = {
  mission: {
    planned: false,
    running: true,
    complete: true,
    error: false,
    abandoned: true,
    unparseable: false,
  },
  dispatch: {
    planned: false,
    running: true,
    complete: true,
    error: true,
    abandoned: true,
    unparseable: false,
  },
  lab: {
    planned: false,
    running: false,
    complete: false,
    error: false,
    abandoned: false,
    unparseable: false,
  },
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
        const reachable = tracked ? trackedReachable : REACHABLE_UNTRACKED[kind][status];
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

/**
 * A `Run` for one matrix cell, shaped the way `/runs` actually emits that
 * cell — one constructor, so a test cannot accidentally hand-build a row
 * the server would never produce, and every cell's row differs from every
 * other's only in the axes the matrix is about.
 *
 * `activityTs` is a REQUIRED parameter rather than a default, deliberately.
 * Defaulting it would let a caller inherit a healthy timestamp without
 * meaning to, and "the row has something to sort on" is exactly the
 * property #2812 turned on — a matrix whose fixtures silently supply it
 * proves nothing about it. Being explicit is also what lets a test pass
 * `undefined` on purpose, to exercise the unorderable row.
 */
export function runForCell(cell: MatrixCell, activityTs: number | undefined): Run {
  return {
    id: cell.id,
    kind: cell.kind,
    status: cell.status,
    tracked: cell.tracked,
    // The wire sets this only alongside `abandoned` (`Run::abandoned_reason`'s
    // own doc), so the fixture does too — a `complete` row carrying a
    // leftover reason is not a state the server can reach.
    ...(cell.abandonReason ? { abandoned_reason: cell.abandonReason } : {}),
    ...(activityTs === undefined ? {} : { updated_ts: activityTs }),
    machine: "MacBook-Pro",
  };
}
