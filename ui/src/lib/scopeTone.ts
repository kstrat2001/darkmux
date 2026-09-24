/** The token-rate scope's trace takes the color of the lit state lamp, so the
 * tube itself says the state at a glance and the lamps are its legend. Each
 * state names the SAME `:root` token its lamp uses (`styles.css`), read at
 * draw time, so a lamp and its trace cannot drift apart. `none` is no live
 * execution (a mission between model steps): the tube stays phosphor green. */
export const SCOPE_TONE_TOKEN = {
  generating: "--scope-phosphor",
  prompt: "--lamp-prompt",
  tools: "--lamp-tools",
  rest: "--lamp-rest",
  stalled: "--lamp-stall",
  none: "--scope-phosphor",
  // (#2890) A stall the page could not trust (connection lost): the static
  // draws in a neutral gray, not a lamp's color, since no lamp is lit.
  nosignal: "--scope-nosignal",
} as const;

export type ScopeTone = keyof typeof SCOPE_TONE_TOKEN;

export type Rgb = [number, number, number];

/** `#rrggbb` (as `getComputedStyle` returns a custom property, possibly with
 * leading whitespace) to an RGB triple; `null` for anything else. */
export function parseHexRgb(value: string): Rgb | null {
  const m = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(value.trim());
  if (!m) return null;
  return [parseInt(m[1], 16), parseInt(m[2], 16), parseInt(m[3], 16)];
}

/** Mix toward white by `amount` (0..1): the sweep dot's hot core. */
export function lighten([r, g, b]: Rgb, amount: number): Rgb {
  const mix = (c: number) => Math.round(c + (255 - c) * amount);
  return [mix(r), mix(g), mix(b)];
}

/** The phosphor green, used when a token cannot be read (no document, or a
 *  value that is not a hex color). */
export const PHOSPHOR_FALLBACK: Rgb = [125, 255, 160];

/** Resolve a tone to RGB from the live stylesheet. */
export function toneRgb(tone: ScopeTone): Rgb {
  if (typeof document === "undefined") return PHOSPHOR_FALLBACK;
  const raw = getComputedStyle(document.documentElement).getPropertyValue(SCOPE_TONE_TOKEN[tone]);
  return parseHexRgb(raw) ?? PHOSPHOR_FALLBACK;
}
