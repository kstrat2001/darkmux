/** (#2963) The width a box that trims its text from the LEFT (`direction:
 *  rtl` + `text-overflow: ellipsis`) should take so its "…" sits at the box's
 *  left edge. The browser draws the ellipsis right before the first WHOLE
 *  character that clears it, so the part of a character that does not fit
 *  shows as a gap before the "…"; narrowing the box to the ellipsis plus a
 *  whole number of characters removes it. For a monospace line: `full` is
 *  the text's whole width over `chars` characters (letter-spacing included),
 *  `ellipsis` the "…"'s width. Half a pixel of slack keeps the last whole
 *  character from being judged not to fit. `null` when the text fits, or
 *  when nothing could be measured (no layout). */
export function leftTrimWidth(m: { available: number; full: number; chars: number; ellipsis: number }): number | null {
  if (m.chars <= 0 || m.full <= 0 || m.available <= 0) return null;
  if (m.full <= m.available + 0.5) return null;
  const advance = m.full / m.chars;
  const whole = Math.max(0, Math.floor((m.available - m.ellipsis) / advance));
  return m.ellipsis + whole * advance + 0.5;
}
