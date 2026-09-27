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
 * - quiet: a small dot in the identity gray, filled when the utility model is
 *   resident, hollow when it is not loaded, dashed when the viewer cannot
 *   tell (a fleet peer) or no utility model is known;
 * - radio routing: the dot radiates arcs (a radio signal);
 * - compacting: two chevrons squeeze toward the dot;
 * - any job this build has no visual for: the generic indicator, a ring
 *   pulsing out of the dot, so a new job is never silent;
 * - stalled (a start with no end past its own bound): the job's glyph, still,
 *   in the STALL lamp's color.
 *
 * Fast transitions are shown as they happen: the glyph is a function of the
 * reading at this instant, with no hold or smoothing.
 */
export function UtilityGlyph({
  strip,
  noSignal = false,
}: {
  strip: UtilityStrip;
  /** (#2958) The card has not had its first data yet: the strip draws
   *  quiet and its words say "no signal", not "idle", which would be a
   *  default rather than a reading. The box is unchanged. */
  noSignal?: boolean;
}) {
  const job = noSignal ? null : strip.job;
  const visual = job ? job.visual : "quiet";
  const residency =
    strip.model == null
      ? "no utility model known"
      : strip.resident === true
        ? "resident"
        : strip.resident === false
          ? "not loaded"
          : "residency unknown";
  const doing = noSignal ? "no signal" : job ? (job.stalled ? `${job.word}, stalled` : job.word) : "idle";
  const label = strip.model != null ? `utility model ${strip.model} · ${residency} · ${doing}` : `${residency} · ${doing}`;
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
      <svg viewBox="0 0 18 16" aria-hidden="true">
        <circle className="mach-util__dot" cx="9" cy="8" r="2.2" />
        {visual === "radio" && (
          <g className="mach-util__radio">
            <path className="mach-util__arc mach-util__arc--1" d="M6.2 5.2 A4 4 0 0 0 6.2 10.8 M11.8 5.2 A4 4 0 0 1 11.8 10.8" />
            <path className="mach-util__arc mach-util__arc--2" d="M3.6 2.8 A7.4 7.4 0 0 0 3.6 13.2 M14.4 2.8 A7.4 7.4 0 0 1 14.4 13.2" />
          </g>
        )}
        {visual === "compacting" && (
          <g className="mach-util__squeeze">
            <path className="mach-util__chev mach-util__chev--l" d="M1.5 4.5 L5 8 L1.5 11.5" />
            <path className="mach-util__chev mach-util__chev--r" d="M16.5 4.5 L13 8 L16.5 11.5" />
          </g>
        )}
        {visual === "generic" && <circle className="mach-util__ping" cx="9" cy="8" r="6" />}
      </svg>
    </span>
  );
}
