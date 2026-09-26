/**
 * (#2902 step 1a) Guards for the per-call usage records.
 *
 * Every model call now emits exactly one `telemetry.tokens` record, with
 * additive fields: `call_kind` (`"turn"` | `"single_shot"` | `"map_item"`),
 * `requested_model`, `reported_model`, `endpoint` and `token_source`
 * (`"provider"`, or `"absent"` with no counts). Step 1a adds the records
 * WITHOUT changing anything on screen; the aggregator in #2902's step 1
 * replaces these per-surface sums. Until then, every existing token sum reads
 * exactly the records it read before 1a, through these two predicates.
 *
 * Kept in one module so the step-1 aggregator can delete them in one place.
 */

interface UsageFields {
  call_kind?: unknown;
  token_source?: unknown;
}

/** True for a usage record an existing token sum counted before step 1a:
 *  every record except the NEW single-shot lineage (whose tokens those sums
 *  still read from the `dispatch complete` record, as before) and the NEW
 *  count-less `token_source: "absent"` records (which would otherwise make a
 *  run look like it has a telemetry family and suppress its fallback). */
export function countsInLegacyTokenSums(p: UsageFields | null | undefined): boolean {
  if (!p) return true;
  return p.call_kind !== "single_shot" && p.token_source !== "absent";
}

/** True for a per-TURN usage record: `call_kind` absent (every record from
 *  before step 1a is a turn or a map item keyed by no turn) or `"turn"`. The
 *  live rate and chars-per-token calibration pair a turn's tokens with that
 *  turn's own heartbeats, so a single-shot or map-item call must never land
 *  in a turn's bucket. */
export function isTurnUsage(p: UsageFields | null | undefined): boolean {
  if (!p) return true;
  return p.call_kind === undefined || p.call_kind === "turn";
}
