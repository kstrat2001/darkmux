// The ONE map from a run's status to the word a viewer shows for it. The board
// row, the board filter, the run page's pill and the fleet timeline all read it,
// so one status never wears two words (5.0). `darkmux run list` words the same
// status in `src/run_list.rs::status_label`; its `abandoned` column keeps the
// reason on the subtitle line instead (fixed-width column, #1907).
import { NOT_REPORTING_STATUS } from "./machineAvailability";
import type { AbandonReason } from "../types/generated/AbandonReason";
import type { RunStatus } from "../types/generated/RunStatus";

/** A run's status as THIS viewer can claim it: the wire status, or `not_reporting`
 *  for a run recorded as running on a machine that is not reporting. */
export type RunBadgeStatus = RunStatus | typeof NOT_REPORTING_STATUS;

/** The word for a status. A total function: a new `RunStatus` is a compile
 *  error here until it is worded. `abandoned` splits on its recorded reason
 *  (#1907): an operator abort and a missing ending are different facts. */
export function runStatusWord(status: RunBadgeStatus, abandonReason?: AbandonReason): string {
  switch (status) {
    case "abandoned":
      return abandonReason === "aborted" ? "aborted" : "no ending recorded";
    case NOT_REPORTING_STATUS:
      return "not reporting";
    case "planned":
    case "running":
    case "complete":
    case "degraded":
    case "error":
    case "escalated":
    case "unparseable":
      return status;
    default: {
      const unhandled: never = status;
      return unhandled;
    }
  }
}
