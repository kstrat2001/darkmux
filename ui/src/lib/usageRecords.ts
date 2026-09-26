/**
 * (#2902 step 1a) Guards for the per-call usage records.
 *
 * Every model call now emits exactly one `telemetry.tokens` record, with
 * additive fields: `call_kind` (`"turn"` | `"single_shot"` | `"map_item"` |
 * `"compaction"`, the last from step 1b),
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

/** (#2902 step 1b) True for a runtime COMPACTOR call's usage record. */
export function isCompactionUsage(p: UsageFields | null | undefined): boolean {
  return !!p && p.call_kind === "compaction";
}

/** (#2902 step 1b) True for a usage record that belongs in ONE execution's
 *  own token numbers (the run page's tiles and mission rollup, the mission
 *  graph's step meter): what `countsInLegacyTokenSums` admits, minus the
 *  compactor's calls. A compactor call is a sub-execution of a utility role
 *  (CLAUDE.md contract 8), never blended into the primary's metrics. The
 *  fleet hero, which totals every call, keeps `countsInLegacyTokenSums`. */
export function countsInExecutionTokenSums(p: UsageFields | null | undefined): boolean {
  return countsInLegacyTokenSums(p) && !isCompactionUsage(p);
}

/** (#2902 step 1b) True when a record's `handle` names the execution the
 *  record belongs to. A compactor call's usage record is attributed to the
 *  compactor (`handle: "compactor"`), a sub-execution INSIDE the session, so
 *  it never names the session's own role (a header saying who ran a session
 *  must not lose its role because the run compacted). */
export function handleNamesExecution(r: { action?: string; payload?: unknown; fields?: unknown }): boolean {
  if (r.action !== "telemetry.tokens") return true;
  const p = (r.payload ?? r.fields) as UsageFields | null | undefined;
  return !isCompactionUsage(p);
}
