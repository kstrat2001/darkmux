/** The dot and stem of the `(i)` affordance, drawn in `currentColor`. The circle is the
 * `.mm-odo-i` button's own border, so one shared style owns the shape; an SVG keeps the "i"
 * from depending on a font (a monospace "i" reads as a "1"). Decorative: the button carries
 * the accessible name. */
export function InfoGlyph() {
  return (
    <svg className="mm-odo-i__glyph" viewBox="0 0 10 10" aria-hidden="true" focusable="false">
      <circle cx="5" cy="2.2" r="1.05" fill="currentColor" />
      <rect x="4.15" y="4" width="1.7" height="4.4" rx="0.5" fill="currentColor" />
    </svg>
  );
}
