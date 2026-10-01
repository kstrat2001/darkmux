/** The radio seat a peer grants: it accepts radio questions from this
 * machine. A permission, not a selection: several peers may grant it, and
 * which one answers is this machine's radio setting.
 * A broadcast glyph in the card's name row, in place of the words
 * "radio-host here", which crowded the hardware line; the tooltip (and the
 * accessible name) says what it means, and the hardware line's own tooltip
 * still spells the whole grant out. */
export const RADIO_SEAT_TITLE = "accepts radio: this machine will answer radio questions sent from here (radio-host)";

export function RadioSeatIcon() {
  return (
    <span className="radio-seat" data-testid="radio-seat" role="img" title={RADIO_SEAT_TITLE} aria-label={RADIO_SEAT_TITLE}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} strokeLinecap="round" aria-hidden="true" focusable="false">
        <circle cx="12" cy="12" r="1.6" fill="currentColor" stroke="none" />
        <path d="M8.5 8.5a5 5 0 0 0 0 7M15.5 8.5a5 5 0 0 1 0 7M5.6 5.6a9 9 0 0 0 0 12.8M18.4 5.6a9 9 0 0 1 0 12.8" />
      </svg>
    </span>
  );
}
