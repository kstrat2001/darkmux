import { isDispatchStart, isDispatchComplete } from "../../lib/flow";
/**
 * `tokensOffMeter()` — viewer.html:1416-1531 (#783, #1186, #1607). The
 * savings hero's summing logic: tokens kept off the (frontier) meter, split
 * local vs cloud vs unknown, plus the fresh/re-read/generated class
 * decomposition. Sums ONLY `telemetry.tokens` records (category=telemetry,
 * source=tokens) — `dispatch.complete` ALSO carries totals in its payload,
 * so summing both would double-count.
 *
 * Playhead-aware in legacy (`ts<=state.t`, so the odometer accumulates as
 * playback scrubs) — but this function itself carries NO playhead
 * parameter, and still doesn't after #1869 restored the scrubber. The gate
 * lives at the CALL SITE instead: `FleetLens` filters `data` to `ts <=
 * playhead` (its `scopedData`, built from the SAME `tMax` prop it already
 * threads through `cards.ts`/`timeline.ts`) before handing it to this
 * function, so every record `tokensOffMeter` ever sees is already
 * "visible as of the playhead" — no record here needs its own ts check
 * because none of them are in the future.
 *
 * That caller-side gate is a REAL behavior now, where before #1869 it was
 * vacuous: `/next`'s fleet lens was always the live rolling window
 * (`useFlowWindow`'s `nowMs - LIVE_WINDOW_MS` boundary already applied
 * before this function ever saw `data`), and `PlaybackLens` always passed
 * `computeTMax(records)` — the day's own true max — so `data` and
 * `data.filter(ts<=tMax)` were the same array either way. Scrubbing before
 * the day's end is what makes the filter load-bearing: a session's
 * `dispatch.complete` (the ONLY positive evidence a session ran local — see
 * `localSids` below) sliding past the playhead is exactly how "moving the
 * playhead back drops a session's tokens from the hero" happens — its
 * telemetry can stay in `data` while its completion falls out, which
 * reclassifies the session from local to unattributed. Live mode's filter
 * stays the same no-op it always was — `flowWindow.tMax` there is
 * `computeTMax(flowWindow.data)` by definition, so every record already
 * satisfies `ts <= tMax`. This function's own body is unchanged by any of
 * this; the caller does the work.
 *
 * TOKENS ONLY — never a currency figure (the operator multiplies by their
 * own per-token rate; darkmux supplies no rate and makes no $ claim, on
 * EITHER tier).
 *
 * (#1607) THREE states, not two: a session with no endpoint-bearing bookend
 * is UNKNOWN and is EXCLUDED from the savings claim rather than credited to
 * it as "local" — `local` is what's left after both `cloud` and `unknown`
 * are removed, never a residual that absorbs everything unproven. This is
 * the honesty-about-incomplete-data contract this function exists to
 * protect: a number that silently under-counts (or over-credits) is the
 * failure mode the whole viewer-port arc is fixing, so don't "simplify" this
 * away.
 */

import type { FlowRecord } from "../../types/handwritten";

/** The subset of a flow record's `payload` this function reads — token
 * telemetry (`category=telemetry,source=tokens`) and `dispatch.complete`'s
 * own totals both use this shape. Loose on purpose, same as the wire. */
interface TokenPayload {
  total_tokens?: number;
  prompt_tokens?: number;
  completion_tokens?: number;
  remote_tokens?: number;
  turn_seq?: number;
  endpoint?: string;
}

export interface TokensOffMeter {
  total: number;
  local: number;
  cloud: number;
  unknown: number;
  prompt: number;
  completion: number;
  fresh: number;
  reread: number;
  uncls: number;
  runs: number;
  cloudRuns: number;
}

/** (#2206/#2207, slop-chop pilot) Does this dispatch payload carry ANY
 * token count? Extracted from the `dispatch.complete` guard below — the
 * parenthesised half only; `isDispatchComplete` stays at the call site
 * (the unit of extraction is the concept, not the condition). The `!!`
 * is the one added token, forced by the boolean return; equivalence over
 * the full field space is pinned in savings.test.ts. */
export function hasAnyTokenCounts(p: {
  total_tokens?: number; prompt_tokens?: number;
  completion_tokens?: number; remote_tokens?: number;
}): boolean {
  return !!(p.total_tokens || p.prompt_tokens || p.completion_tokens || p.remote_tokens);
}

/** (#2206/#2207, slop-chop pilot) A review-path remote complete: remote
 * tokens present and NO local counts. Extracted from the classifier
 * below; same `!!` note as `hasAnyTokenCounts`. */
export function isRemoteOnlyTokens(p: {
  total_tokens?: number; prompt_tokens?: number;
  completion_tokens?: number; remote_tokens?: number;
}): boolean {
  return !p.total_tokens && !p.prompt_tokens && !p.completion_tokens && !!p.remote_tokens;
}

export function tokensOffMeter(data: FlowRecord[]): TokensOffMeter {
  // (#1607) Evidence of WHERE a session ran (`epBySid`) is registered from
  // ANY dispatch bookend carrying an `endpoint` — a `dispatch.complete` that
  // named its endpoint but carried no token totals still marks its session
  // cloud (the review path's `remote_tokens`-only completes are exactly
  // this case). `dcTok` is the single-shot fallback's own totals (sessions
  // with no `telemetry.tokens` family at all) — REGARDLESS of endpoint
  // (#1853). A local single-shot dispatch (radio-router, radio-host) is
  // exactly the endpoint-less case: gating collection on `p.endpoint` (as
  // this loop did before #1853) meant its tokens never entered `dcTok` at
  // all, so they landed in no bucket — not even `unknown`. Collection
  // stays endpoint-blind; `epBySid`/`localSids` decide the bucket at
  // classification time, below.
  //
  // (#2635 follow-up) `dcTok` holds an ARRAY per session id, not one
  // payload — `dispatch.single_shot`'s `dispatch_session_id` is
  // deliberately TASK-scoped ("sibling seats fanned out within one task
  // share this key", builtins.rs), so two sibling steps — one hosted, one
  // local — can legitimately complete under the SAME session id. A single
  // Map entry per sid (as this used to be) silently dropped every
  // completion but the last, and could paint one seat's tokens on the
  // other seat's tile at classification time. Accumulating means every
  // completion survives to be classified on its own terms below.
  const epBySid = new Map<string, string>();
  const dcTok = new Map<string, TokenPayload[]>();

  for (const r of data) {
    const p = r.payload as TokenPayload | undefined;
    if (!r.session_id || !p) continue;
    if (p.endpoint && (isDispatchStart(r.action) || isDispatchComplete(r.action))) {
      epBySid.set(r.session_id, String(p.endpoint));
    }
    if (isDispatchComplete(r.action) && hasAnyTokenCounts(p)) {
      const arr = dcTok.get(r.session_id);
      if (arr) arr.push(p);
      else dcTok.set(r.session_id, [p]);
    }
  }

  // Sessions POSITIVELY known local — the bar is a SUCCESSFUL TERMINAL with
  // no endpoint, not any bookend at all. A `dispatch.start` proves nothing
  // (the review path only stamps its remote classification when it CLOSES
  // cleanly); `dispatch.error` is excluded for the same reason — a run that
  // died before classifying itself has not told us where it ran. So the
  // only positive evidence is a clean completion that named no endpoint.
  // Everything else is unknown, which is the honest answer.
  const localSids = new Set<string>();
  for (const r of data) {
    const p = r.payload as TokenPayload | undefined;
    if (!r.session_id || !p) continue;
    if (isDispatchComplete(r.action) && !p.endpoint) localSids.add(r.session_id);
  }

  let total = 0;
  let prompt = 0;
  let completion = 0;
  let cloud = 0;
  let unknown = 0;
  const sess = new Map<string, TokenPayload[]>();

  for (const r of data) {
    if (r.category === "telemetry" && r.source === "tokens") {
      const p = (r.payload as TokenPayload) || {};
      total += p.total_tokens || 0;
      prompt += p.prompt_tokens || 0;
      completion += p.completion_tokens || 0;
      if (r.session_id && epBySid.has(r.session_id)) {
        cloud += p.total_tokens || 0;
      } else if (!r.session_id || !localSids.has(r.session_id)) {
        unknown += p.total_tokens || 0;
      }
      // Composite fallback key: two sessionless records sharing a ts must
      // not merge into one pseudo-session (it would corrupt the turn_seq
      // decomposition below). session_id is the norm; this is defensive.
      const k = r.session_id || `ts:${r.ts}:${r.handle || ""}:${r.machine_uid || ""}`;
      if (!sess.has(k)) sess.set(k, []);
      sess.get(k)!.push(p);
    }
  }

  // The class decomposition (fresh/re-read/generated) stays COMBINED across
  // tiers (one breakdown, not one per tier) — only the headline split above
  // is per-tier.
  let fresh = 0;
  let reread = 0;
  let uncls = 0;
  let cloudRuns = 0;
  for (const [k, recs] of sess) {
    if (epBySid.has(k)) cloudRuns++;
    const sp = recs.reduce((a, p) => a + (p.prompt_tokens || 0), 0);
    if (recs.every((p) => p.turn_seq != null)) {
      const turns = recs.slice().sort((a, b) => (a.turn_seq as number) - (b.turn_seq as number));
      let rr = 0;
      let prev: number | null = null;
      for (const p of turns) {
        const pi = p.prompt_tokens || 0;
        if (prev != null) rr += Math.min(pi, prev);
        prev = pi;
      }
      reread += rr;
      fresh += sp - rr;
    } else {
      uncls += sp;
    }
  }

  // (#1186, reclassified #1853) Single-shot fallback — sessions with no
  // telemetry family count their completion-record totals from `dcTok`.
  // The `sess.has` guard preserves the telemetry-exclusive rule that
  // prevents double-counting (a session that has BOTH a telemetry family
  // and a token-bearing completion is already fully counted above and is
  // skipped here).
  //
  // Classification happens HERE, not at collection — and (#2635) it is
  // PER-COMPLETION, not per-session: cloud when THIS payload's own
  // `endpoint` is set, local when it isn't. A session-level check
  // (`epBySid.has(sid)` as this used to read) is wrong here specifically
  // BECAUSE `dcTok`'s session id can be shared by sibling seats (see the
  // module-level comment above `dcTok`'s declaration) — `epBySid` would
  // credit a purely-local seat as cloud (or vice versa) whenever ANY
  // sibling under the same task-scoped sid happened to name an endpoint.
  // Each `dcTok` payload IS itself the `dispatch.complete` record that
  // proves its own classification, so it needs no session-level lookup.
  // The `unknown` branch stays a defensive floor: a payload lacking
  // `endpoint` satisfies the exact same criterion `localSids` used to add
  // this sid (`isDispatchComplete` + no `endpoint`, on this very record),
  // so `!localSids.has(sid)` should never fire — kept anyway rather than
  // assuming that invariant can't drift, same posture as before. One turn
  // means the whole prompt is first-read, so `fresh += prompt` is exact
  // here, not an approximation.
  let directRuns = 0;
  for (const [sid, payloads] of dcTok) {
    if (sess.has(sid)) continue;
    for (const p of payloads) {
      // `remote_tokens` is the review path's spelling for its own spend; the
      // other three are null there. Last in the chain so it never overrides
      // a record that reported the standard fields.
      const tt = p.total_tokens || (p.prompt_tokens || 0) + (p.completion_tokens || 0) || p.remote_tokens || 0;
      total += tt;
      if (p.endpoint) {
        cloud += tt;
        cloudRuns++;
      } else if (!localSids.has(sid)) {
        unknown += tt;
      }
      // Known, currently-inert gap: a completion that itself carries no
      // `endpoint` but whose endpoint-bearing `dispatch.start` sibling has
      // scrolled outside the caller's playhead window is credited to
      // `local` here rather than `unknown` — the wrong direction for the
      // #1607 honesty contract. Every current producer stamps `endpoint`
      // on BOTH bookends of a hosted call (see `bookend_record` in
      // builtins.rs), so no live data can hit this today; pinned in
      // savings.test.ts so a future producer that stops double-stamping
      // makes the gap loud instead of silent.
      prompt += p.prompt_tokens || 0;
      completion += p.completion_tokens || 0;
      fresh += p.prompt_tokens || 0;
      // A `remote_tokens`-only payload has no prompt/completion split to
      // decompose, so without this its spend joins `total` while appearing
      // in NO class chip.
      if (isRemoteOnlyTokens(p)) uncls += tt;
      directRuns++;
    }
  }

  // `local` is what is LEFT after both `cloud` and `unknown` are removed —
  // never a residual that absorbs everything unproven.
  return {
    total,
    local: total - cloud - unknown,
    cloud,
    unknown,
    prompt,
    completion,
    fresh,
    reread,
    uncls,
    runs: sess.size + directRuns,
    cloudRuns,
  };
}
