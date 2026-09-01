/**
 * "Is a plain object" — the single shared form of the rule that previously
 * appeared three times (eventFilters.ts's two `JSON.parse` result checks and
 * flow.ts's `bodyTruncated`). Positive form of the original conditions:
 * `v !== null && typeof v === "object" && !Array.isArray(v)`.
 *
 * Note: `v !== null` and `!v` are equivalent here — of all falsy values only
 * `null` has `typeof === "object"`, so the two original spellings
 * (`parsed === null || typeof parsed !== "object"` and
 * `!body || typeof body !== "object"`) agreed on every input.
 */
export function isPlainObject(v: unknown): v is Record<string, unknown> {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}
