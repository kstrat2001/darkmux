import type { UtilityStrip } from "../lib/utilityJobs";

/**
 * (#2915) The fleet card's utility strip: darkmux's own jobs on the machine's
 * utility model, separate from the work model's scope.
 *
 * It lives at the END OF THE CARD'S NAME ROW, in a fixed 20 x 18 box that is
 * always there (quiet, running, stalled, offline alike), so nothing appears
 * or disappears and no box changes size between states (operator,
 * 2026-09-26). No visible text: the model, its residency and the job ride in
 * the tooltip and the accessible name, and the page's text (the parity
 * goldens) is unchanged.
 *
 * It is drawn as a robot (Lucide's `bot` icon, ISC, notice in
 * ui/vendor-licenses), because a bare dot was not readable as "the utility
 * model" (operator, 2026-10-01). Everything happens inside the robot, so
 * nothing spills into the name row:
 * - quiet: the robot in the identity gray, solid when the utility model is
 *   resident, faded when it is not loaded, dashed when the viewer cannot
 *   tell (a fleet peer) or no utility model is known;
 * - radio routing: its antenna blinks (a radio signal);
 * - compacting: its body squeezes;
 * - any job this build has no visual for: its eyes pulse, so a new job is
 *   never silent;
 * - stalled (a start with no end past its own bound): the job's motion
 *   stopped, in the STALL lamp's color.
 *
 * Fast transitions are shown as they happen: the glyph is a function of the
 * reading at this instant, with no hold or smoothing.
 */
export function UtilityGlyph({
  strip,
  noSignal = false,
}: {
  strip: UtilityStrip;
  /** (#2958) The source that would name a running job has not answered
   *  yet: a QUIET strip's words say "checking…", not "idle", which would be
   *  a default rather than a reading. A job already read still shows. The
   *  box is unchanged. */
  noSignal?: boolean;
}) {
  const job = strip.job;
  const visual = job ? job.visual : "quiet";
  const residency =
    strip.model == null
      ? "no utility model known"
      : strip.resident === true
        ? "resident"
        : strip.resident === false
          ? "not loaded"
          : "residency unknown";
  const doing = job ? (job.stalled ? `${job.word}, stalled` : job.word) : noSignal ? "checking…" : "idle";
  const label =
    strip.model != null ? `Utility model: ${strip.model}\n${residency} · ${doing}` : `Utility model: unknown\n${doing}`;
  const dot = strip.resident === true ? "filled" : strip.resident === false ? "hollow" : "unknown";
  return (
    <span
      className="mach-util"
      data-testid="fleet-utility"
      data-visual={visual}
      data-job={job?.job ?? ""}
      data-stalled={job?.stalled ? "true" : "false"}
      data-dot={dot}
      role="img"
      aria-label={label}
      title={label}
    >
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <g className="mach-util__bot">
          <path className="mach-util__antenna" d="M12 8V4H8" />
          <rect className="mach-util__body" width="16" height="12" x="4" y="8" rx="2" />
          <path d="M2 14h2" />
          <path d="M20 14h2" />
          <path className="mach-util__eye" d="M15 13v2" />
          <path className="mach-util__eye" d="M9 13v2" />
        </g>
      </svg>
    </span>
  );
}
