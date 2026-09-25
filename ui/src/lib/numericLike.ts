/**
 * (#2878) The run page's MODEL/SYSTEM metric tiles (`SessionReplay.tsx`'s
 * `.met .mv`) already arrive as PRE-FORMATTED strings — `sessionRun.ts`
 * bakes rounding, thousands-grouping and even the `%` sign into `value`
 * before this component ever sees it (`view.metrics[i].value: string`).
 * There is no raw number left to hand `useCountUp` directly, and several
 * of these tiles are not numbers at all (a model name, a duration like
 * "4h 20m").
 *
 * This is the narrow bridge: recognize the shapes that genuinely ARE one
 * plain number (with commas and/or a trailing `%` already baked in,
 * exactly the two things every caller here bakes in) and hand back both
 * the number to tween and a formatter that reproduces the ORIGINAL
 * string's own shape — same comma grouping, same decimal places, same
 * `%`. Anything else (a name, a multi-part string, a duration) returns
 * `null` and the caller renders it exactly as before: unrecognized is not
 * an error case, it is simply "nothing to animate here".
 */
export interface NumericLikeValue {
  n: number;
  render: (n: number) => string;
}

const NUMERIC_LIKE = /^(-?[\d,]+(?:\.\d+)?)(%?)$/;

export function parseNumericLike(s: string): NumericLikeValue | null {
  const m = NUMERIC_LIKE.exec(s.trim());
  if (!m) return null;
  const [, raw, suffix] = m;
  const hasComma = raw.includes(",");
  const dot = raw.indexOf(".");
  const decimals = dot === -1 ? 0 : raw.length - dot - 1;
  const n = Number(raw.replace(/,/g, ""));
  if (!Number.isFinite(n)) return null;
  return {
    n,
    render: (v: number) => {
      const fixed = decimals > 0 ? v.toFixed(decimals) : String(Math.round(v));
      const [intPart, decPart] = fixed.split(".");
      const neg = intPart.startsWith("-");
      const digits = neg ? intPart.slice(1) : intPart;
      const grouped = hasComma ? digits.replace(/\B(?=(\d{3})+(?!\d))/g, ",") : digits;
      return `${neg ? "-" : ""}${grouped}${decPart ? `.${decPart}` : ""}${suffix}`;
    },
  };
}
