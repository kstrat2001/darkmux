import { isDispatchStart, isDispatchComplete, isDispatchError, T } from "../../lib/flow";
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
 *
 * (#2637) The RUN count gets the same three-way treatment, not just a
 * `runs`/`cloudRuns` pair — `unknownRuns` is the run-level analog of
 * `unknown`. Before this, no consumer could tell "positively local" from
 * "unattributed" among runs, and `hybridNote` derived local runs as
 * `runs - cloudRuns` — the exact residual-absorbs-everything mistake #1607
 * already ruled out for tokens, recurring one field over. Same rule here: a
 * run counts as `cloudRuns` when its own bookend named an endpoint,
 * `unknownRuns` when it has no positive evidence either way, and is
 * otherwise implicitly local (`runs - cloudRuns - unknownRuns` — never a
 * residual credited by default).
 *
 * (#2659) The run count itself is grouped by distinct `dispatch.complete`
 * BOOKEND, not by distinct `sess` key. A `sess` key is a session id (or the
 * sessionless composite fallback), and #1856 established that a
 * deterministic `mission_run` session id is reused across a re-launch or
 * retry of the same mission phase inside the viewer's window — so one key
 * can legitimately close with more than one real completion. Counting
 * `sess.size` (one per key) undercounted that population to 1; counting
 * bookends (via `dcTok`, see below) restores "a dispatch" to mean what the
 * operator reading the number expects. The per-bookend path only engages
 * for 2-OR-MORE bookends (see the guard's own comment below) — a session
 * with exactly one bookend keeps the ORIGINAL session-wide classification
 * unchanged, so this fix is purely additive for the overwhelmingly common
 * case. For a genuinely spanning session, `cloudRuns`/`unknownRuns` move
 * together with the count: each bookend is classified on its OWN endpoint,
 * not the session's aggregate evidence, so a session with a mixed
 * local+cloud pair of bookends doesn't paint the whole session cloud (the
 * TOKEN split a few dozen lines down is a known, narrower exception to
 * this — see the per-bookend loop's own comment).
 *
 * A second population this widens, not named in the issue: two sibling
 * `dispatch.single_shot` seats sharing one TASK-scoped session id
 * (`session_id::task`, `DispatchSingleShotStepKind::dispatch_session_id`)
 * that ALSO carry a `telemetry.tokens` family (rather than the token-less-
 * completion shape `directRuns` below already handled) now count as two
 * runs instead of one — correct, matching how `directRuns` already counted
 * task-scoped siblings when telemetry was absent.
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

/** A `sess`-grouped turn, carrying the record's own `ts` alongside its
 * payload — needed to sort turns chronologically (#1856) rather than by
 * `turn_seq` alone, which restarts at 1 on every dispatch — including a
 * RE-LAUNCH of the same mission phase under the same deterministic
 * `mission_run` session id (see the sort comment below for the corrected
 * mechanism). */
interface SessTurn extends TokenPayload {
  ts: string;
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
  /** (#2637) Runs with no positive evidence either way — same "unknown,
   * not free" honesty as `unknown` above, at run granularity. Excluded from
   * both `cloudRuns` and the implicit local count; never guessed. */
  unknownRuns: number;
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
  //
  // (#2659) The run count below groups by DISTINCT `dcTok` BOOKEND, not by
  // `sess`'s one-entry-per-key shape — reusing `dcTok` itself rather than a
  // parallel collection, so "what counts as a real dispatch bookend" stays
  // ONE definition (`isDispatchComplete` + `hasAnyTokenCounts`) instead of
  // two that could drift apart. `mission_run(mission_id, phase_id)`
  // (`crates/darkmux-types/src/session_id.rs`) is a DETERMINISTIC session
  // id, so re-launching or retrying the same mission phase inside the
  // viewer's window reuses the identical id, and each launch closes with
  // its OWN token-bearing `dispatch.complete`. Before this fix the run
  // count was `sess.size` — one per distinct KEY, regardless of how many
  // real completions happened under it — so a session that legitimately
  // spans two (or more) dispatches undercounted to 1.
  //
  // Deliberately NOT counting every `isDispatchComplete` record regardless
  // of content (tried first, reverted): a mission-graph `dispatch.map`
  // step's own `dispatch complete` bookend (`category: work, source:
  // scheduler, kind: "dispatch.map"`, `DispatchMapStepKind::bookend_record`
  // in `crates/darkmux-crew/src/step_kinds/builtins.rs`) only carries a
  // token total when the step is HOSTED — `stamp_remote_classification` is
  // called there only `if endpoint_label.is_some()`. A LOCAL `dispatch.map`
  // step's completion is real model work same as any other, but reports
  // NO token field at all, so `hasAnyTokenCounts` is false for it and it
  // never enters `dcTok` — same gap `hybridNote.ts` already documents at
  // its own module doc ("a dispatch whose completion carries no token
  // totals ... contributes 0 to `runs` too"), inherited here rather than
  // introduced by this fix. `hasAnyTokenCounts` is NOT a step-vs-dispatch
  // filter (a hosted `dispatch.map` step DOES pass it, and always has,
  // pre- and post- this fix) — it is exactly what its name says, "did this
  // bookend report any tokens," and this fix reuses that existing bar
  // rather than inventing a second, looser one that would have counted
  // the real corpus's local map-step completions (`tests/parity/corpus/
  // flow-yesterday.json`'s `task-review-probe-*-task` sessions) as extra
  // runs they aren't.
  const epBySid = new Map<string, string>();
  const dcTok = new Map<string, TokenPayload[]>();

  for (const r of data) {
    const p = r.payload as TokenPayload | undefined;
    if (!r.session_id || !p) continue;
    // (post-review fix-pass) `isDispatchError` joins the two terminals
    // already read here. `DispatchSingleShotStepKind::run` stamps the SAME
    // `endpoint_label.as_deref()` on its `"dispatch start"`, `"dispatch
    // error"`, and `"dispatch complete"` bookends alike (verified against
    // `crates/darkmux-crew/src/step_kinds/builtins.rs:1038-1055` — all
    // three `Self::bookend_record(...)` calls in `run_single_shot` pass the
    // identical `endpoint_label.as_deref()`), so a hosted attempt that died
    // before producing a token-bearing complete is still positively known
    // to have targeted an endpoint. Reading it here is free evidence for
    // the session-level token split (`cloud`/`unknown` a few dozen lines
    // down) and the single/zero-bookend classification branch below — it
    // does NOT feed the per-bookend group loop's own classification any
    // more, since that loop no longer consults `epBySid` at all (see its
    // own comment).
    if (p.endpoint && (isDispatchStart(r.action) || isDispatchComplete(r.action) || isDispatchError(r.action))) {
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
  const sess = new Map<string, SessTurn[]>();

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
      sess.get(k)!.push({ ...p, ts: r.ts });
    }
  }

  // The class decomposition (fresh/re-read/generated) stays COMBINED across
  // tiers (one breakdown, not one per tier) — only the headline split above
  // is per-tier.
  let fresh = 0;
  let reread = 0;
  let uncls = 0;
  let cloudRuns = 0;
  let unknownRuns = 0;
  let sessRuns = 0;
  for (const [k, recs] of sess) {
    // (#2659) A `sess` key is one dispatch, UNLESS `dcTok` shows it closed
    // with MORE THAN ONE real `dispatch.complete` bookend (the spanning-
    // session-id shape #1856 established) — then it's that many runs, each
    // classified on ITS OWN bookend rather than the session's aggregate
    // `epBySid`/`localSids` evidence. Classifying per-bookend (not per-key)
    // matters here specifically: a session whose two bookends disagree
    // (one local, one cloud) would otherwise paint the WHOLE session cloud
    // via `epBySid.has(k)`, same failure shape #1607 already ruled out for
    // the token-level split. Reusing `dcTok` (rather than a fresh
    // collection) also means a `sess`-member session's bookends are never
    // double-counted against `directRuns` below — `directRuns` already
    // skips any sid `sess.has()`, unchanged by this fix.
    //
    // (post-review MUST FIX) The guard is `length > 1`, NOT `length > 0` —
    // a session with exactly ONE bookend falls to the `else` below and
    // keeps the ORIGINAL `epBySid.has(k)` aggregate classification, never
    // `bookends[0].endpoint` alone. `epBySid` registers from a
    // `dispatch.start`, `dispatch.complete`, or `dispatch.error` (see its
    // own comment above); a session whose START named an endpoint but
    // whose single COMPLETE didn't (or vice versa) would silently flip
    // from cloud to local under a naive `bookends[0].endpoint` check — a
    // real classification regression for the overwhelmingly common
    // single-bookend case. Restricting the per-bookend path to `length >
    // 1` means every existing single-bookend session classifies EXACTLY
    // as it did before this fix; only the genuinely-spanning population
    // (2+ real completions) takes the new path. See the pinned regression
    // test.
    const bookends = dcTok.get(k);
    if (bookends && bookends.length > 1) {
      // (#2659 follow-up, MUST FIX 1 — post-adversarial-review correction)
      // An earlier version of this branch fell back to a session-wide
      // `epBySid` FLOOR whenever no bookend in the group carried its own
      // endpoint, on the theory that the endpoint evidence must simply
      // live elsewhere (a `dispatch.start` that wasn't restated, or an
      // errored hosted sibling). That reasoning does not hold for this
      // population. `dcTok`'s key is TASK-SCOPED for `dispatch.single_shot`
      // and `dispatch.map` (`dispatch_session_id` in
      // `crates/darkmux-crew/src/step_kinds/builtins.rs:892-900`, minted by
      // `darkmux_types::session_id::task` — "sibling seats fanned out
      // within one task share this key"). So THIS branch's own entry
      // condition — more than one dispatch.complete bookend under one
      // session id — selects precisely the concurrent-sibling-seat
      // population, not a single dispatch spanning multiple completions.
      // `epBySid` for that key is the UNION of every sibling seat's own
      // evidence, so using it as a floor paints every endpoint-less
      // sibling with whatever ANY sibling (including one that errored, or
      // one whose usage-omitting hosted complete never entered `dcTok`)
      // happened to report. Measured: a review-probe task with four
      // sibling seats (one hosted/errored, two local/complete, one
      // hosted/complete) rendered "2 dispatches via cloud" for its two
      // local completions under the floor, when ground truth is 2 local,
      // 0 cloud — pinned in savings.test.ts. A second, error-free producer
      // hits the same path: a hosted seat whose endpoint omits `usage`
      // stamps `endpoint` on a `total_tokens: null` complete
      // (`single_shot.rs:51`), which sets `epBySid` but fails
      // `hasAnyTokenCounts` and so never joins this group — the same floor
      // still fires off that evidence alone.
      //
      // Classification here is now purely per-bookend: a bookend's OWN
      // `endpoint` field is the only evidence consulted for THIS bookend.
      // A sibling with no endpoint of its own is positive local evidence
      // (same criterion `localSids` uses), never floored by another
      // sibling's evidence. `epBySid` is deliberately NOT read in this
      // loop at all.
      for (const b of bookends) {
        if (b.endpoint) cloudRuns++;
        // No `else` for unknownRuns here: a completion (this loop only
        // sees `isDispatchComplete` records) either carries its own
        // endpoint or it doesn't — and "doesn't" is exactly the criterion
        // `localSids` uses to prove a session local. A bookend with no
        // endpoint is therefore positive LOCAL evidence, not unknown; it
        // contributes to the implicit-local count via
        // `runs - cloudRuns - unknownRuns`, same as the single-bookend
        // case always did.
        //
        // Known, narrower gap (same shape as the `(CONSIDER 3, #2635)`
        // gap pinned below for the single-shot fallback): the aggregate
        // TOKEN split (`cloud`/`unknown` a few dozen lines up) still
        // classifies every `telemetry.tokens` record in this session via
        // the session-wide `epBySid`/`localSids`, not per-bookend — so a
        // genuinely MIXED session (one local dispatch, one cloud
        // dispatch, same session id) can render "N local + M cloud" on
        // the DISPATCHES line while the TOKENS tiles still show all of
        // that session's tokens under a single tier. Fixing that fully
        // needs per-bookend TURN attribution (partitioning a session's
        // turns at each bookend's timestamp) — tracked as a #2665
        // follow-up, not done here: this issue is scoped to the RUN
        // COUNT, and every session in the corpus with 2+ bookends today
        // has all-local bookends (no live data exercises the divergent-
        // attribution case). Pinned in savings.test.ts so the gap is
        // visible, not silently assumed away.
        //
        // (CONSIDER 3, post-review) A narrower, RELATED gap remains in the
        // `else` branch just below, not in this loop: a sibling seat whose
        // OWN completion is the sole `dcTok` entry for its (shared,
        // task-scoped) session id — because every OTHER sibling either
        // hasn't completed yet or never entered `dcTok` at all (the
        // omitted-usage shape above) — still falls to the arity<=1 `else`
        // branch and inherits `epBySid`'s session-wide evidence there,
        // same as it always has. That branch's `epBySid` read is REQUIRED
        // for the genuine single-dispatch case (a lone bookend whose own
        // START carried the endpoint — see the pinned regression test
        // below) and this fix does not touch it, so the identical group
        // (one hosted-endpoint sibling, one endpoint-less sibling) can
        // still classify its endpoint-less member LOCAL at arity 2 (this
        // loop) but CLOUD at arity 1 (the `else` branch), depending only
        // on whether the hosted sibling's own bookend happened to enter
        // `dcTok`. Not introduced by this fix (identical on main before
        // it), and not resolved by it either — named here rather than
        // silently assumed fixed. Distinguishing "one physical dispatch
        // whose start/complete disagree" from "one sibling seat completing
        // while others are still in flight or token-less" needs bookend
        // identity narrower than session id (`payload.step_id`, the same
        // #2665 mechanism named above) and is left as a follow-up.
        sessRuns++;
      }
    } else {
      // ZERO or exactly ONE bookend for this key — the original single-run
      // classification, byte-for-byte unchanged (zero bookends: still in
      // flight, or a sessionless composite key; one bookend: the ordinary
      // single-dispatch case, which is nearly every session in practice):
      // cloud when SOME bookend (a start counts too) named an endpoint,
      // unknown when there's no positive local evidence either. A
      // sessionless composite key (`k` has no real `session_id`) can never
      // be in `localSids`, so it always lands here, same as the
      // token-level classification above treats a sessionless record as
      // unknown.
      if (epBySid.has(k)) cloudRuns++;
      else if (!localSids.has(k)) unknownRuns++;
      sessRuns++;
    }
    const sp = recs.reduce((a, p) => a + (p.prompt_tokens || 0), 0);
    if (recs.every((p) => p.turn_seq != null)) {
      // (#1856) Sort by TIME, turn_seq only as a tiebreak — not the reverse.
      // `mission_run(mission_id, phase_id)` mints a DETERMINISTIC session id
      // (`crates/darkmux-types/src/session_id.rs`) — the same phase
      // re-launched or retried inside the viewer's 24h window reuses the
      // identical session id, and each launch's own coder dispatch restarts
      // its turn counter at 1. Sorting by `turn_seq` alone interleaves
      // chronologically unrelated turns from different launches: turn_seq=1
      // from a later launch sorts next to turn_seq=1 from an earlier one
      // even though they may be minutes (or longer) apart, and the overlap
      // estimator below then compares prompt sizes across a launch boundary
      // where no re-read relationship exists. Sorting by `ts` groups each
      // launch's turns together (only the ONE true launch-boundary pair is
      // ever compared, not every same-turn_seq pair), matching what
      // actually happened.
      //
      // (MUST FIX 2, #1856 fix-pass) The comparator below resolves BOTH
      // sides to a number BEFORE branching — never mixes a ts-comparison on
      // one pair with a turn_seq-comparison on another, which is what makes
      // this a genuine total order. An earlier version of this comparator
      // branched per-pair ("if both sides parse, compare by ts; else fall
      // to turn_seq") and was provably intransitive: with a well-timed A, a
      // corrupt-timestamp B, and a well-timed-but-earlier C, that shape
      // produced A<B, B<C, and A>C simultaneously — three different sorted
      // outputs depending on incidental input order. Mapping an unparseable
      // `ts` to `+Infinity` up front (sorting those turns LAST, never used
      // to decide an ordering relative to a well-timed turn by anything
      // other than "is it well-timed") restores a real total order: turns
      // compare on `(resolvedTs, turn_seq)` lexicographically, always.
      //
      // `ts` is second-precision (`ts_utc_now()`), so a same-`ts` tie is
      // POSSIBLE in principle and turn_seq is the correct tiebreak for it —
      // but measured across both parity corpora (731 adjacent same-session
      // token-telemetry pairs, `tests/parity/corpus/flow-{today,yesterday}.json`
      // + `docs/demo/demo-flow.jsonl`), same-second adjacent turns are ZERO,
      // not common. The tiebreak stays as a defensive floor (and as the
      // resolution for the corrupt-timestamp case above), not because ties
      // are expected in practice.
      //
      // (Known-narrower alternative, not adopted here — tracked as
      // #2665) Grouping turns by `session_id` PLUS `payload.step_id`
      // (stamped on every per-event flow record a graph-bound dispatch
      // emits, `crates/darkmux-crew/src/dispatch_internal.rs`'s
      // `TailerState::step_id` doc) would be exact and immune to clock
      // resolution for the worktree/coder/verify-in-one-launch shape —
      // but it does NOT disambiguate two SEPARATE launches of the SAME
      // step (the actual mechanism above): both stamp the identical
      // `(session_id, step_id)` pair, so grouping alone can't tell them
      // apart either. Adopting it would also mean changing the OUTER
      // `sess` grouping key that `runs`/`cloudRuns`/`unknownRuns` are
      // derived from too — the exact same shape of problem #2659 already
      // owns. Filed as #2665 rather than folded in here.
      const turns = recs
        .slice()
        .sort((a, b) => {
          const ta = T(a.ts);
          const tb = T(b.ts);
          const na = Number.isNaN(ta) ? Infinity : ta;
          const nb = Number.isNaN(tb) ? Infinity : tb;
          if (na !== nb) return na - nb;
          // A non-numeric `turn_seq` reaching this point is a pre-existing,
          // unrelated gap (this branch only runs turn_seq!=null records —
          // see the `recs.every` guard above); not addressed here.
          return (a.turn_seq as number) - (b.turn_seq as number);
        });
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
  // The `unknown`/`unknownRuns` branch stays a defensive floor: a payload
  // lacking `endpoint` satisfies the exact same criterion `localSids` used
  // to add this sid (`isDispatchComplete` + no `endpoint`, on this very
  // record), so `!localSids.has(sid)` should never fire — kept anyway
  // rather than assuming that invariant can't drift, same posture as
  // before. Concretely: `unknownRuns++` here is NOT a second live
  // contribution symmetric with the `sess`-loop classification above —
  // it is structurally unreachable under the current invariant (proved by
  // mutation: deleting it leaves `savings.test.ts` green). `unknownRuns`
  // has exactly ONE live producer today, the `sess`-loop branch a few dozen
  // lines up; this one is kept for the same reason its sibling `unknown +=
  // tt` always was — a floor against the invariant drifting, not evidence
  // it currently does anything. One turn means the whole prompt is
  // first-read, so `fresh += prompt` is exact here, not an approximation.
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
        unknownRuns++; // (#2637) same "unknown, not free" at run granularity
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
    runs: sessRuns + directRuns,
    cloudRuns,
    unknownRuns,
  };
}
