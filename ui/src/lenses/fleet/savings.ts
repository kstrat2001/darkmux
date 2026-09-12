import { isDispatchComplete, T } from "../../lib/flow";
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
 *
 * (#2690) ONE rule for WHERE a dispatch ran, applied everywhere in this
 * function: **a SUCCESSFUL TERMINAL classifies itself, and nothing else
 * classifies it.** A `dispatch.complete` naming an `endpoint` is cloud; a
 * `dispatch.complete` naming none is local; anything else — a start, an
 * error, or no terminal at all — is UNKNOWN, the #1607 bucket that exists
 * precisely for "no evidence of its own".
 *
 * That rule replaces `epBySid`, a session-keyed map of "some bookend
 * (start, complete, OR error) named an endpoint" that both the token split
 * and the single-bookend run classification used to consult. `epBySid` was
 * wrong for the same reason #2635 removed it from `directRuns` and #2687
 * removed it from the per-bookend loop: a dispatch session id is
 * TASK-scoped for `dispatch.single_shot` and `dispatch.map`
 * (`darkmux_types::session_id::task` — "sibling seats fanned out within one
 * task share this key", `crates/darkmux-crew/src/step_kinds/builtins.rs`),
 * so its value is the UNION of every sibling seat's evidence. Reading that
 * union to classify ONE run paints one seat's evidence onto another.
 * Measured before this fix, with a hosted sibling still in flight and one
 * local sibling completed: `runs=1 cloudRuns=1 unknownRuns=0` — the
 * operator's own hardware's work reported as cloud with local 0, for the
 * whole (minutes-long) hosted call, and permanently if that sibling then
 * errored.
 *
 * Two consequences worth stating, because each changes a number:
 *
 * 1. A session whose `dispatch.start` named an endpoint but whose own
 *    `dispatch.complete` did not is now LOCAL, not cloud. No producer emits
 *    that shape: `endpoint` is stamped from one `endpoint_label` onto
 *    start/error/complete alike by `DispatchSingleShotStepKind::
 *    bookend_record` and `DispatchMapStepKind::bookend_record`
 *    (`builtins.rs:826-867`, `:1626-1669`), by `dispatch_remote`
 *    (`dispatch_internal.rs:3217/3239/3303/3331`), and by the container
 *    path from a single `remote_endpoint_raw_label`
 *    (`dispatch_internal.rs:5193` start, `:7531` terminal). So the only
 *    live way to hold a start's endpoint without its matching complete's is
 *    for the two to belong to DIFFERENT seats sharing a task-scoped id —
 *    exactly the case this fix exists to stop mis-attributing.
 * 2. A hosted attempt that ERRORED no longer marks its session cloud. It is
 *    unknown instead — never local, which is what #2688's `isDispatchError`
 *    registration was protecting, and that protection is now STRUCTURAL:
 *    `localSids` only ever admits a clean completion, so an errored
 *    dispatch cannot reach the local bucket by any path. What the
 *    error-as-cloud-evidence term additionally did was let ONE sibling's
 *    error re-classify ANOTHER sibling's clean local completion, which is
 *    the K1/K2 contradiction #2690 measured (dispatches reading "2 local"
 *    beside tiles reading LOCAL 0 / CLOUD 2,000 for the same session).
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
  // (#1607, rewritten #2690) Evidence of WHERE a session ran is registered
  // from its SUCCESSFUL TERMINALS only: a `dispatch.complete` naming an
  // `endpoint` adds its session to `cloudSids`, one naming none adds it to
  // `localSids`. The two sets are exact mirrors of each other and share one
  // pass, because they answer one question from one record — a session can
  // legitimately land in BOTH when sibling seats share a task-scoped id,
  // and `cloudSids` wins there (never credit cloud work as local; the
  // per-seat split is the #2665 follow-up).
  //
  // (#2690) This REPLACES `epBySid`, which registered from a
  // `dispatch.start` or `dispatch.error` too. A start proves only intent
  // and an error proves only that the run died before classifying itself —
  // the exact argument `localSids` has always made for excluding both from
  // the local side. Applying it symmetrically to the cloud side is the
  // whole of this change; see the module doc for what it moves and why the
  // producers make it safe.
  //
  // `dcTok` is the single-shot fallback's own totals (sessions with no
  // `telemetry.tokens` family at all) — REGARDLESS of endpoint (#1853). A
  // local single-shot dispatch (radio-router, radio-host) is exactly the
  // endpoint-less case: gating collection on `p.endpoint` (as this loop did
  // before #1853) meant its tokens never entered `dcTok` at all, so they
  // landed in no bucket — not even `unknown`. Collection stays
  // endpoint-blind; classification happens below, per completion.
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
  const dcTok = new Map<string, TokenPayload[]>();
  // Sessions POSITIVELY known cloud / POSITIVELY known local — the bar on
  // BOTH sides is a SUCCESSFUL TERMINAL, not any bookend at all. A
  // `dispatch.start` proves nothing (the review path only stamps its remote
  // classification when it CLOSES cleanly); `dispatch.error` is excluded
  // for the same reason — a run that died before classifying itself has not
  // told us where it ran. So the only positive evidence is a clean
  // completion, and what it says about its own `endpoint` field is the
  // answer. Everything else is unknown, which is the honest answer.
  const cloudSids = new Set<string>();
  const localSids = new Set<string>();

  for (const r of data) {
    const p = r.payload as TokenPayload | undefined;
    if (!r.session_id || !p) continue;
    if (!isDispatchComplete(r.action)) continue;
    if (p.endpoint) cloudSids.add(r.session_id);
    else localSids.add(r.session_id);
    if (hasAnyTokenCounts(p)) {
      const arr = dcTok.get(r.session_id);
      if (arr) arr.push(p);
      else dcTok.set(r.session_id, [p]);
    }
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
      // (#2690) `cloudSids`, not the retired `epBySid`. A `telemetry.tokens`
      // record carries no `endpoint` of its own on any producer (the only
      // emitter is the container path's per-turn tailer,
      // `crates/darkmux-crew/src/dispatch_internal.rs:8539`, whose payload
      // is `{turn_seq, prompt_tokens, completion_tokens, total_tokens}`), so
      // this split can only ever be session-scoped — which is exactly why
      // the evidence it reads has to be the session's own SUCCESSFUL
      // terminals and not the union of every bookend any sibling seat
      // emitted. Reading `epBySid` here made the telemetry-PRESENT and
      // telemetry-ABSENT forms of the same local work disagree completely
      // (`cloud=100 local=0` here vs `cloud=0 local=100` in `directRuns`,
      // which has classified per-completion since #2635); they now agree
      // because both read the same evidence.
      //
      // (#2690) Precedence is UNCHANGED: cloud beats local when a session's
      // own completions disagree, exactly as `epBySid` behaved. That case
      // is a real one — `tests/parity/corpus/flow-{yesterday,today}.json`
      // hold two sessions (`task-review-probe-high-task`,
      // `task-review-verify-task`) whose deterministic ids recur across the
      // day boundary the viewer loads as one window, local on 2026-08-07
      // and hosted on 2026-08-08, 268,225 tokens between them — and
      // resolving it needs per-bookend TURN attribution (#2665), which this
      // change does not attempt. Routing those to `unknown` instead was
      // tried and reverted: it is a different question from the one #2690
      // asks, it moves a quarter-million real tokens off the cloud tile on
      // evidence that genuinely names an endpoint, and no golden covers the
      // two-day window where it happens (the fleet golden renders
      // `flow-today.json` alone). Cloud-over-local also keeps the rule that
      // matters most here intact: hosted spend is never credited local.
      if (r.session_id && cloudSids.has(r.session_id)) {
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
    // with more than one real `dispatch.complete` bookend (the spanning-
    // session-id shape #1856 established, and the concurrent-sibling-seat
    // shape the task-scoped session id produces) — then it's that many
    // runs. Reusing `dcTok` (rather than a fresh collection) also means a
    // `sess`-member session's bookends are never double-counted against
    // `directRuns` below — `directRuns` already skips any sid `sess.has()`.
    //
    // (#2690) The guard is `length > 0`, not #2659's `length > 1`. Every
    // bookend classifies on its own `endpoint` field, at EVERY arity —
    // there is no longer a separate rule for the single-bookend case. The
    // `length > 1` restriction existed to protect one shape: a session
    // whose START named an endpoint but whose lone COMPLETE did not, which
    // `epBySid` (the union across start/complete/error) called cloud and a
    // per-bookend read would call local. That shape has no producer — every
    // dispatch path stamps one `endpoint_label` onto its start and its
    // terminal alike (citations in the module doc) — so the only live way
    // to observe it is two DIFFERENT seats sharing a task-scoped session
    // id, where "the start's endpoint" is a different seat's evidence and
    // painting it onto this completion is precisely the #2690 defect.
    // Collapsing the two branches also removes the arity-dependent
    // inconsistency the `(CONSIDER 3)` note below used to name: the same
    // sibling group no longer classifies its endpoint-less member LOCAL at
    // arity 2 and CLOUD at arity 1.
    const bookends = dcTok.get(k);
    if (bookends && bookends.length > 0) {
      // (#2659 follow-up, MUST FIX 1 — post-adversarial-review correction)
      // An earlier version of this branch fell back to a session-wide
      // FLOOR (the since-retired `epBySid` map, see the module doc)
      // whenever no bookend in the group carried its own endpoint, on the
      // theory that the endpoint evidence must simply live elsewhere (a
      // `dispatch.start` that wasn't restated, or an errored hosted
      // sibling). That reasoning does not hold for this population.
      // `dcTok`'s key is TASK-SCOPED for `dispatch.single_shot`
      // and `dispatch.map` (`dispatch_session_id` in
      // `crates/darkmux-crew/src/step_kinds/builtins.rs:892-900`, minted by
      // `darkmux_types::session_id::task` — "sibling seats fanned out
      // within one task share this key"), so a session-wide endpoint value
      // is the UNION of every sibling seat's own evidence: using it as a
      // floor paints every endpoint-less sibling with whatever ANY sibling
      // (including one that errored, or one whose usage-omitting hosted
      // complete never entered `dcTok`) happened to report. Measured: a
      // review-probe task with four sibling seats (one hosted/errored, two
      // local/complete, one hosted/complete) rendered "2 dispatches via
      // cloud" for its two local completions under the floor, when ground
      // truth is 2 local, 0 cloud — pinned in savings.test.ts. A second,
      // error-free producer hits the same path: a hosted seat whose
      // endpoint omits `usage` stamps `endpoint` on a `total_tokens: null`
      // complete (`single_shot.rs:51`), which never joins this group
      // because it fails `hasAnyTokenCounts` — the same floor still fired
      // off that evidence alone.
      //
      // Classification here is purely per-bookend: a bookend's OWN
      // `endpoint` field is the only evidence consulted for THIS bookend.
      // A sibling with no endpoint of its own is positive local evidence
      // (same criterion `localSids` uses), never floored by another
      // sibling's evidence. (#2690) Since the guard above relaxed to
      // `length > 0`, this is now the rule at EVERY arity, not just for
      // groups of two or more.
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
        // Known, narrower gap, still open after #2690: the aggregate TOKEN
        // split (`cloud`/`unknown` a few dozen lines up) classifies every
        // `telemetry.tokens` record in this session by the SESSION's
        // completions (`cloudSids`/`localSids`), not per-bookend — a
        // `telemetry.tokens` record carries no endpoint of its own on any
        // producer, so nothing narrower is available without partitioning a
        // session's turns at each bookend's timestamp. A genuinely MIXED
        // session (one local dispatch, one cloud dispatch, same session id)
        // therefore renders "N local + M cloud" on the DISPATCHES line
        // while ALL of its tokens fall to `unknown` — under-claimed, not
        // misattributed, which is the direction #1607 asks for. Tracked as
        // the #2665 follow-up; pinned in savings.test.ts so the gap is
        // visible, not silently assumed away.
        sessRuns++;
      }
    } else {
      // ZERO token-bearing bookends for this key: still in flight, closed
      // outside the window, closed with no usable token count, errored, or
      // a sessionless composite key. There is no per-run completion to
      // classify on, so fall back to the session's own SUCCESSFUL-terminal
      // evidence — a hosted completion that named an endpoint but reported
      // no usage (`single_shot.rs:51`'s `total_tokens: null` shape) is
      // real cloud evidence even though it can't enter `dcTok`. With no
      // terminal evidence at all the answer is `unknownRuns`, the #1607
      // bucket: crediting it cloud would misreport the operator's own
      // hardware, and crediting it local would credit hosted spend as free.
      // A sessionless composite key (`k` has no real `session_id`) is in
      // neither set, so it always lands in `unknownRuns`, same as the
      // token-level classification above treats a sessionless record. And
      // the same cloud-over-local precedence as that split, for the same
      // reason — see its comment.
      if (cloudSids.has(k)) cloudRuns++;
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
  // `endpoint` is set, local when it isn't. A session-level check (the
  // since-retired `epBySid.has(sid)` this used to read) is wrong here
  // specifically BECAUSE `dcTok`'s session id can be shared by sibling
  // seats (see the module-level comment above `dcTok`'s declaration) — a
  // session-wide endpoint value would credit a purely-local seat as cloud
  // (or vice versa) whenever ANY sibling under the same task-scoped sid
  // happened to name an endpoint. Each `dcTok` payload IS itself the
  // `dispatch.complete` record that proves its own classification, so it
  // needs no session-level lookup. (#2690) The `sess`-loop branch above now
  // reads the same way, at every arity, so the two paths agree.
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
