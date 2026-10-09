/**
 * The ONE work-status chip (operator, 2026-09-03: "prefer re-usable and
 * consistent indicators … you would have to remember to adjust the effect in
 * 2 or more places").
 *
 * Before this, one fact — a unit of work is in progress — had three looks:
 * the run detail's `.pill[data-live]` (pulsing RUNNING), the mission header's
 * flat green `.mstatus.active`, the runs board's `.labbadge.running`, plus
 * the timeline's phase tag. Every scope of the work-unit ladder (mission ›
 * phase › task › step › run) now renders this chip. The raw status word is
 * the label (CSS uppercases it, so every golden that pins the TEXT is
 * unchanged); the look comes from a six-word internal vocabulary:
 *
 *   running  — in progress: accent, and the pulse (`data-live` modulates it
 *              exactly as the run detail's liveness state always did)
 *   done     — a good terminal: complete / finalized
 *   error    — a bad terminal: error
 *   degraded — a MIXED terminal (#2406): real output was produced, some of
 *              it was not — a phase with some tasks complete and some
 *              errored/abandoned. Warn color, same as `stopped` (a caution,
 *              not a failure — see `.fleetcov`'s own comment in
 *              `styles.css` for the same color choice at a different
 *              scope), but its OWN kind: `degraded` and `stopped` are
 *              different facts (a mix that shipped real output vs. an
 *              operator/budget kill) and must stay distinguishable by the
 *              raw status word even though they share a color family.
 *   stopped  — an operator or budget terminal: abandoned / escalated
 *   idle     — not started, or not claimable: planned / waiting / unparseable /
 *              not_reporting (a run on a machine that is not reporting, 5.0 R3)
 *   unknown  — a word this map does not list (a status newer than this build,
 *              or none at all): the raw word shows in the chip, dim, and in the
 *              DOM as `s-<raw>`. Never worded as idle.
 *
 * Styling lives in ONE place: `.wstatus` in `styles.css`, its color from the
 * one status palette (`[data-status-kind]`, which the fleet timeline's bar
 * reads too). A call site may add a layout class (`className`) but never a
 * second color/animation source.
 */
import type { LivenessState } from "./LivenessPulse";
import type { RunBadgeStatus } from "../lib/runStatusWord";
import type { GraphNodeStatus } from "../types/generated/GraphNodeStatus";
import type { MissionStatus } from "../types/generated/MissionStatus";

/**
 * THE word a pulsing chip says. (operator, 2026-09-04: a pulsing pill on the
 * mission view read ACTIVE while the step below it read RUNNING — "is this a
 * missed inconsistency?") It was: the raw status word was the label, so one
 * fact wore the scope's own vocabulary — `active` (mission), `running` (step,
 * run), `● live` (lab run). The kind already unified the LOOK; this unifies
 * the WORD for the one kind whose chips sit side by side while both pulse.
 * Terminal words stay raw on purpose: `finalized` / `complete` / `finished`
 * are different facts at different scopes, and none of them pulse.
 *
 * NOT this word, and not this component: the masthead's own pill dot
 * (`Masthead.tsx`'s `pillLabel`, #2412) says the record STREAM is connected. A page can be live
 * with nothing running, and a run can be running while the stream reconnects.
 * (operator, 2026-09-04: "live is a separate idea from a running job.")
 */
export const RUNNING_WORD = "running";
export type WorkStatusKind = "running" | "done" | "error" | "degraded" | "stopped" | "idle" | "unknown";

/** Every status word a scope hands this chip: a run's (with `not_reporting`),
 *  a mission's, a phase's or task's. Typed from the generated unions, so a new
 *  variant is a compile error here until it is given a kind. */
export type WorkStatusWord = RunBadgeStatus | MissionStatus | GraphNodeStatus;

const KIND: Record<WorkStatusWord, WorkStatusKind> = {
  running: "running",
  active: "running",
  complete: "done",
  finalized: "done",
  error: "error",
  // (#2406) Mixed terminal — real output was produced, some of it was not.
  // Its own kind, not folded into `stopped`: see this file's own doc for
  // why the two must stay distinguishable by word even though they share
  // a color.
  degraded: "degraded",
  aborted: "stopped",
  abandoned: "stopped",
  // (F2) A deliberate hand-off to a higher tier: unfinished by design, so a
  // caution like `stopped`, never the error color.
  escalated: "stopped",
  planned: "idle",
  waiting: "idle",
  unparseable: "idle",
  not_reporting: "idle",
  unknown: "unknown",
};

const isWorkStatusWord = (raw: string): raw is WorkStatusWord => Object.hasOwn(KIND, raw);

/** The chip's kind. A word the map does not list is `unknown`, never `idle`:
 *  an unrecognized status must not read as "nothing is happening". */
export function workStatusKind(raw: string | undefined): WorkStatusKind {
  return raw !== undefined && isWorkStatusWord(raw) ? KIND[raw] : "unknown";
}

export function WorkStatus({
  status,
  label,
  live,
  className,
  title,
}: {
  /** The status word from the data (`active`, `running`, `complete`, …). */
  status: WorkStatusWord | undefined;
  /** Override the visible text of a NON-running chip (a terminal's scope-specific
   *  word). A running chip always says `RUNNING_WORD`; the override is ignored. */
  label?: string;
  /** Liveness of the thing behind a `running` chip; drives the pulse's play state. */
  live?: LivenessState;
  className?: string;
  title?: string;
}) {
  const kind = workStatusKind(status);
  const raw = status ?? "unknown";
  const cls = ["wstatus", `is-${kind}`, `s-${raw}`, className].filter(Boolean).join(" ");
  return (
    <span className={cls} data-status-kind={kind} data-live={kind === "running" ? (live ?? "beating") : undefined} title={title}>
      {kind === "running" ? RUNNING_WORD : (label ?? raw)}
    </span>
  );
}
