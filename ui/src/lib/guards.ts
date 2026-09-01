/**
 * (#2206/#2207, slop-chop pilot) "Is a non-array object" — the single
 * shared form of a rule that appears inline across `ui/src` (at least six
 * sites); this pilot converted the three in `eventFilters.ts` (two
 * `JSON.parse` result checks) and `flow.ts` (`bodyTruncated`,
 * `asRecordArray`). Known remaining lookalikes, left for a follow-up
 * sweep: `recordDetail.ts:45`, `RecordView.tsx:132,149`,
 * `MissionGraphLens.tsx:196,427`.
 *
 * Positive form of the original conditions:
 * `v !== null && typeof v === "object" && !Array.isArray(v)`.
 *
 * NAMING CAVEAT: despite the conventional reading of "plain object", this
 * accepts ANY non-array object — `Date`, `Map`, class instances,
 * `Object.create(null)` — because that is exactly what every original
 * inline site accepted. Callers wanting a literal-object check need a
 * prototype test this deliberately does not do.
 *
 * `v !== null` and the `!v` spelling flow.ts used are equivalent here — of
 * all falsy values only `null` has `typeof === "object"`, so the original
 * spellings agreed on every input. That claim is not left to De Morgan
 * reasoning: guards.test.ts pins both original spellings against this
 * function over every falsy type and the object-ish shapes.
 */
export function isPlainObject(v: unknown): v is Record<string, unknown> {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}
