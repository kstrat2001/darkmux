/** A machine serves radio: its allow-list grants the `radio-host` role to at
 * least one peer, so it answers radio questions for the peers it allows.
 * A fact about that machine alone (its card's `serves_radio`), so it reads the
 * same from every server: never who is granted, never a relationship with the
 * machine serving the viewer.
 * A broadcast glyph in the card's name row; the tooltip (and the accessible
 * name) says what it means. */
export function RadioSeatIcon({ machine }: { machine: string }) {
  const title = `${machine} serves radio: it answers radio questions for peers it allows.`;
  return (
    <span className="radio-seat" data-testid="radio-seat" role="img" title={title} aria-label={title}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={1.8} strokeLinecap="round" aria-hidden="true" focusable="false">
        <circle cx="12" cy="12" r="1.6" fill="currentColor" stroke="none" />
        <path d="M8.5 8.5a5 5 0 0 0 0 7M15.5 8.5a5 5 0 0 1 0 7M5.6 5.6a9 9 0 0 0 0 12.8M18.4 5.6a9 9 0 0 1 0 12.8" />
      </svg>
    </span>
  );
}
