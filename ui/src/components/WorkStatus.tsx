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
 * unchanged); the look comes from a five-word internal vocabulary:
 *
 *   running  — in progress: accent, and the pulse (`data-live` modulates it
 *              exactly as the run detail's liveness state always did)
 *   done     — a good terminal: complete / finished / finalized / closed
 *   error    — a bad terminal: error / errored / killed
 *   stopped  — an operator or budget terminal: aborted / abandoned /
 *              canceled / interrupted / paused
 *   idle     — not started, or a word this map does not know: planned /
 *              unparseable / undefined / anything new (loud in the DOM via
 *              `s-<raw>`, quiet on screen)
 *
 * Styling lives in ONE place: `.wstatus` in `styles.css`. A call site may add
 * a layout class (`className`) but never a second color/animation source.
 */
import type { LivenessState } from "./LivenessPulse";

export type WorkStatusKind = "running" | "done" | "error" | "stopped" | "idle";

const KIND: Record<string, WorkStatusKind> = {
  running: "running",
  active: "running",
  live: "running",
  complete: "done",
  finished: "done",
  finalized: "done",
  closed: "done",
  error: "error",
  errored: "error",
  killed: "error",
  aborted: "stopped",
  abandoned: "stopped",
  canceled: "stopped",
  interrupted: "stopped",
  paused: "stopped",
  planned: "idle",
  unparseable: "idle",
};

export function workStatusKind(raw: string | undefined): WorkStatusKind {
  if (!raw) return "idle";
  return KIND[raw.toLowerCase()] ?? "idle";
}

export function WorkStatus({
  status,
  label,
  live,
  className,
  title,
}: {
  /** The raw status word from the data (`active`, `running`, `complete`, …). */
  status: string | undefined;
  /** Override the visible text (the run detail passes its pre-uppercased label). */
  label?: string;
  /** Liveness of the thing behind a `running` chip; drives the pulse's play state. */
  live?: LivenessState;
  className?: string;
  title?: string;
}) {
  const kind = workStatusKind(status);
  const raw = (status ?? "unknown").toLowerCase();
  const cls = ["wstatus", `is-${kind}`, `s-${raw}`, className].filter(Boolean).join(" ");
  return (
    <span className={cls} data-live={kind === "running" ? (live ?? "beating") : undefined} title={title}>
      {label ?? raw}
    </span>
  );
}
