import { isDispatchComplete, isDispatchStart, isDispatchError, T } from "../../lib/flow";
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
 * `dispatch.complete` (the ONLY positive evidence a run stayed local — see
 * `localKeys` below) sliding past the playhead is exactly how "moving the
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
 * (#2659) The run count is NOT one per `sess` key. A deterministic
 * `mission_run` session id is reused across a re-launch or retry of the
 * same mission phase inside the viewer's window (#1856), and a task-scoped
 * id is shared by every sibling seat in one task — so one key legitimately
 * closes with more than one real completion, and counting `sess.size`
 * undercounted that population to 1. Every token-bearing completion is its
 * own run, classified on its OWN endpoint rather than on the key's
 * aggregate evidence, so a key holding a mixed local+cloud pair does not
 * paint both cloud. (The TOKEN split a few dozen lines down IS keyed on the
 * key's aggregate evidence, because a `telemetry.tokens` record names no
 * seat — see its own comment, and #2690.)
 *
 * (#2690) TWO rules for WHERE work ran, and the first one is what four
 * previous passes at this function all missed:
 *
 * **1. Every verdict is keyed on a RUN, never on a session id.** See
 * `runKey` below for the measurement. `darkmux_types::session_id`'s `task`
 * and `mission_run` constructors are DETERMINISTIC — byte-identical across
 * every launch of the same config, as their own doc says at
 * `crates/darkmux-types/src/session_id.rs:70-80` — so a session id names a
 * SHAPE of work, not an occurrence of it. #2635, #2687, #2688 and #2690 all
 * reasoned about one failure of that fact (sibling seats fanned out inside
 * ONE task share the key) and none of them about the other: the same key
 * also RECURS across entirely unrelated mission runs, and the viewer's
 * 24-hour window routinely holds several at once. A flat `Set<session_id>`
 * of verdicts therefore accumulates verdicts from runs that have nothing to
 * do with each other, and whichever one is consulted first wins.
 *
 * **2. Within a run, a `dispatch.complete` classifies itself.** One naming
 * an `endpoint` is cloud, one naming none is local. A `dispatch.start`
 * naming an endpoint is ALSO cloud evidence for its own run — hosted
 * billing begins at the start, and the per-turn `telemetry.tokens` family
 * streams for the whole call before any completion lands. A start naming no
 * endpoint proves nothing (that is also what an unclassified hosted start
 * looks like), and `dispatch.error` proves nothing either way.
 *
 * Rule 2 is deliberately ASYMMETRIC, and #2690 got that part right: a start
 * can prove cloud but never local. What #2690 got wrong was concluding that
 * the fix was to stop reading starts. `epBySid`'s defect was its KEY, not
 * its evidence — dropping the evidence while keeping the key made the
 * failure worse rather than better, because a stale LOCAL verdict from a
 * different mission run then had nothing left to out-vote it. Measured on
 * the committed corpus: `task-review-probe-high-task` completes locally
 * under three missions on 2026-08-07 and runs hosted under a fourth on
 * 2026-08-08, and 144,638 tokens of live Azure spend rendered on the LOCAL
 * tile for 17m14s with `cloudRuns=0`, so `hybridNote` read "the hybrid loop
 * is humming, keep it up" while an endpoint billed.
 *
 * What #2690 DID fix, and this pass keeps unchanged: the run count
 * classifies every token-bearing bookend on its OWN `endpoint`, at every
 * arity, consulting no set at all. That is where #2690's measured
 * `runs=1 cloudRuns=1 unknownRuns=0` defect lived — a local seat reported
 * as cloud because a hosted sibling's start was read through a
 * session-wide map.
 *
 * (#2709) SCOPE — #2701 re-keyed only the VERDICTS and said so; this pass
 * finishes the job by re-keying the GROUPING (`dcTok` and `sess`) onto the
 * same `runKey`, and by counting a run from the RUN KEY's own evidence
 * instead of from whichever collection happened to hold a bookend. The
 * three failures #2701 pinned as "known gaps" are what that closes:
 *
 *   (a) `runs` under-counted. Every run key that closed with a
 *       `dispatch.complete` is now a run. Measured on the committed
 *       two-day corpus: 50 run keys carry a completion while `runs`
 *       reported 40.
 *   (b) `cloudRuns` was not monotonic — measured `[0,0,1,1,0]` across an
 *       arrival sequence, resting on "1 local dispatch" beside a 100%-cloud
 *       token tile. A hosted terminal that reported no usage never entered
 *       `dcTok`, so the moment a local sibling's token-bearing completion
 *       joined the group, the hosted run stopped being counted at all.
 *       It is counted as its own run now (the EXTRA HOSTED RUN term in the
 *       run loop), so the same sequence reads `[0,0,1,1,1]`.
 *   (c) A whole run's tokens vanished: the double-count guard
 *       (`sess.has(...)`) was keyed on the bare session id, so a run was
 *       skipped because a DIFFERENT run under the same session id had
 *       telemetry. Measured: 9,999 hosted tokens absent from `total`
 *       entirely. The guard is run-scoped now, so it only ever skips the
 *       run it is actually about.
 *
 * What this pass does NOT close is #2690's TOKEN half, and that is a
 * measurement rather than a shrug: sibling seats fanned out within one task
 * share the session id AND the mission id by construction, and NOTHING on a
 * `telemetry.tokens` record names the seat it came from. `payload.step_id`
 * is stamped only by the container path's `stamp_step_id`
 * (`crates/darkmux-crew/src/dispatch_internal.rs`) and only when the
 * dispatch is a graph step; `handle` on such a record is the ROLE id
 * (`build_telemetry_record`, `crates/darkmux-crew/src/dispatch.rs:1179`);
 * `work_id` is `None` on every crew record builder. Measured across all
 * four committed corpora: ZERO of 774 `telemetry.tokens` records carry a
 * `payload.step_id`. There is no third coordinate to key on, so the token
 * split's cloud-over-local precedence for a mixed key stays exactly as
 * #2701 left it, and #2690's three token shapes stay open.
 *
 * Producer note, still true and still load-bearing: `endpoint` is stamped
 * from one `endpoint_label` onto start/error/complete alike by
 * `DispatchSingleShotStepKind::bookend_record` and
 * `DispatchMapStepKind::bookend_record` (`builtins.rs:826-867`,
 * `:1626-1669`), by `dispatch_remote`
 * (`dispatch_internal.rs:3217/3239/3303/3331`), and by the container path
 * from a single `remote_endpoint_raw_label` (`dispatch_internal.rs:5193`
 * start, `:7531` terminal). No producer names an endpoint on a start
 * without naming it on that run's own terminal, so reading starts adds
 * EARLINESS — the in-flight window — and never a verdict the terminal
 * would later contradict.
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

/** A token-bearing `dispatch.complete` payload. (#2709) It carries no key
 * of its own any more: `dcTok` is keyed by `runKey`, so the map key IS the
 * run key and a per-bookend copy would be a second spelling of the same
 * fact. (#2701 needed the copy because the grouping was still keyed on a
 * bare session id while the verdicts were keyed by run.) */
type Bookend = TokenPayload;

/** (#2690 fix-pass) The identity of ONE RUN, which is what every
 * where-did-this-run verdict in this function has to be keyed on.
 *
 * A bare `session_id` is NOT that identity. `darkmux_types::session_id`'s
 * `task(...)` and `mission_run(...)` constructors are DETERMINISTIC — their
 * own doc (`crates/darkmux-types/src/session_id.rs:70-80`) says the id is
 * byte-identical across every launch of the same config — so one session id
 * legitimately belongs to many unrelated runs, and a flat `Set<session_id>`
 * of verdicts accumulates them all. Measured on the committed corpus
 * (`tests/parity/corpus/flow-{yesterday,today}.json` +
 * `flow-session-task-list.json`): 14 distinct session ids each span more
 * than one mission — `task-review-probe-high-task` spans 5, and the ACP
 * panel's `task-list` / `task-__panel_args__` span 24 and 23 respectively.
 * Inside the viewer's 24-hour window those are concurrent members of the
 * same `Set`, so an earlier run's verdict out-votes a later run's evidence.
 *
 * `mission_id` is exactly the coordinate the deterministic session id drops,
 * so `(session_id, mission_id)` restores it. Verified on the same corpus:
 * ZERO session ids recur without a `mission_id` to separate them (the 43
 * mission-less session ids are all per-dispatch forms like
 * `crew-dispatch-<role>-<micros>-internal`, which are unique by
 * construction), and ZERO `telemetry.tokens` records carry a `mission_id`
 * that matches no completion of their own session — so the composite key
 * neither fails to separate a recurring run nor orphans a record that used
 * to classify.
 *
 * The sessionless fallback is the SAME composite the `sess` grouping uses,
 * so a record with no `session_id` cannot collide with another one. The
 * separator is `\u0000`, which cannot occur inside a session or mission id,
 * so no pair of ids can spell another pair's key. */
function runKey(r: FlowRecord): string {
  const sid = r.session_id || `ts:${r.ts}:${r.handle || ""}:${r.machine_uid || ""}`;
  return `${sid}\u0000${r.mission_id || ""}`;
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
  // (#1607, rewritten #2690, re-scoped #2690 fix-pass) Evidence of WHERE a
  // RUN ran, keyed on `runKey` — `(session_id, mission_id)` — and never on
  // a bare session id. See `runKey`'s own doc for why the bare id cannot
  // express this: it is deterministic, so it recurs across unrelated runs,
  // and a flat set of verdicts keyed on it accumulates every run's answer
  // into one bucket that the wrong run then reads.
  //
  // The two sides are NOT symmetric, and the asymmetry is the point:
  //
  //   `cloudKeys` — ANY bookend of this run (`dispatch.start`,
  //   `dispatch.complete` or `dispatch.error`) named an `endpoint`. One
  //   uniform rule, no carve-outs: if a run ever named a hosted endpoint,
  //   the tokens arriving under its key are hosted spend. A START counts
  //   because billing begins there, not at the completion, and the
  //   per-turn `telemetry.tokens` family streams throughout a multi-minute
  //   hosted call. An ERROR counts because a hosted attempt that died
  //   still burned hosted tokens. Refusing either leaves real spend
  //   creditable to LOCAL, which is the one direction this function must
  //   never take — and the carve-out for errors was measurably such a
  //   hole (a mixed run whose hosted seat's only in-view bookend is an
  //   error, beside a local seat's clean completion, credited the whole
  //   key LOCAL).
  //
  //   `localKeys` — a run's OWN clean `dispatch.complete` naming no
  //   endpoint, and nothing else. No bookend can prove LOCAL except that
  //   one: the ABSENCE of an endpoint on a start or an error is also what
  //   an unstamped or not-yet-classified hosted bookend looks like, so
  //   only a clean terminal that named no endpoint positively proves the
  //   work stayed on the operator's own hardware.
  //
  // `cloudKeys` WINS when one run lands in both (sibling seats sharing a
  // task-scoped id, one hosted and one local). That is over-claiming CLOUD,
  // never crediting hosted spend as local: if any seat in a task is hosted,
  // that task really did bill. The per-seat split is the #2665 follow-up.
  //
  // (#2690) What this KEEPS from #2690 is the run-count fix a few dozen
  // lines down — every token-bearing bookend classifies on its OWN
  // `endpoint` at every arity, consulting no set at all. That is where
  // #2690's measured defect (`runs=1 cloudRuns=1` for a local seat beside
  // an in-flight hosted sibling) actually lived, and it is untouched here.
  // What this RESTORES is start-as-cloud-evidence for the token split,
  // which #2690 removed along with `epBySid`. Removing it was measured to
  // move 144,638 tokens of live Azure spend onto the LOCAL tile for 17m14s
  // (see the fix-pass test in savings.test.ts); the actual defect in
  // `epBySid` was never that it read starts, but that it was keyed on a
  // recurring session id.
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
  // `hasAnyTokenCounts` is what keeps `dcTok` to bookends that actually
  // reported a token count. It is NOT a step-vs-dispatch filter (a HOSTED
  // `dispatch.map` step passes it and always has) — it is exactly what its
  // name says. A LOCAL `dispatch.map` step's own `dispatch complete`
  // bookend (`DispatchMapStepKind::bookend_record`,
  // `crates/darkmux-crew/src/step_kinds/builtins.rs`) reports NO token
  // field at all, because `stamp_remote_classification` is called there
  // only `if endpoint_label.is_some()` — so it fails the bar and never
  // enters `dcTok`.
  //
  // (#2709) That bookend is still real model work, and it is counted as a
  // run now — by the EXTRA LOCAL RUN term in the run loop, off `localKeys`,
  // NOT by loosening `hasAnyTokenCounts`. The distinction matters and is
  // why #2659 reverted "count every `isDispatchComplete` record": that
  // shape counts a token-less local map-step completion sharing a key with
  // a seat's genuine token-bearing completion as a SECOND run, which it is
  // not. A per-KEY presence term cannot double-count that way — the MUST
  // FIX 2 test pins it, and the `TERM:` tests pin each half separately.
  //
  // (#2709) `dcTok` is keyed by `runKey`, the SAME key the verdict sets
  // use. #2701 left it on the bare session id on purpose — re-keying it
  // moves `runs` itself — and pinned the three consequences as tests.
  // This is that measured change; the module doc above lists what each of
  // the three was and what closed it. The practical effect here is that a
  // group is now one RUN's bookends rather than the union of every run
  // that happened to reuse a deterministic session id.
  const dcTok = new Map<string, Bookend[]>();
  // RUNS positively known cloud / positively known local, keyed by
  // `runKey`. See the long comment at the top of this function for why the
  // two sides take different evidence, and `runKey`'s own doc for why the
  // key is `(session_id, mission_id)` and not a bare session id.
  //
  // TWO cloud sets, because the two consumers ask different questions and
  // a start is admissible evidence for exactly one of them:
  //
  //   `cloudKeys` (ANY endpoint-naming bookend) answers "are the tokens
  //   ARRIVING under this key hosted spend?" — asked by the token split,
  //   which has to decide about tokens that exist NOW, possibly mid-call
  //   and possibly after a failure.
  //
  //   `cloudTerminalKeys` (complete only) answers "where did this finished
  //   RUN run?" — asked by the run-count branch, which is counting runs and
  //   so has a terminal to read by definition. Keeping starts AND errors
  //   out of it is what preserves #2690's fix (an errored hosted sibling
  //   must not make a local sibling's dispatch read cloud) as well as
  //   monotonicity.
  //
  // Keeping starts and errors OUT of the run-count set is what stops a
  // hosted seat's START from counting as a finished run: a start says
  // billing has begun, not that a dispatch completed.
  //
  // (#2709) `cloudTerminalKeys` is ALSO what makes `cloudRuns` monotonic.
  // #2701 measured it falling — `[0,0,1,1,0]` — because `cloudRuns` had two
  // writers that OVERRODE each other: a zero-bookend group read this set,
  // and the moment one token-bearing completion joined the group the
  // per-bookend branch took over and re-decided the whole group from the
  // bookends it happened to hold. A hosted seat whose endpoint omits
  // `usage` emits `{endpoint, result_class: "ok", total_tokens: null}`
  // (`SingleShotReply::total_tokens` is `Option<u64>`,
  // `crates/darkmux-crew/src/single_shot.rs:51`, serialized at
  // `step_kinds/builtins.rs:1165-1175` while `endpoint_label` is stamped
  // unconditionally at `:844`), so it fails `hasAnyTokenCounts`, never
  // enters `dcTok`, and was invisible to that branch. The run loop below
  // ADDS the two sources instead of letting one replace the other, so a
  // hosted terminal that reported no usage keeps its own run. Pinned as an
  // arrival-sequence test in savings.test.ts on both a synthetic shape and
  // the committed corpus.
  const cloudKeys = new Set<string>();
  const cloudTerminalKeys = new Set<string>();
  const localKeys = new Set<string>();

  for (const r of data) {
    const p = r.payload as TokenPayload | undefined;
    if (!r.session_id || !p) continue;
    const isComplete = isDispatchComplete(r.action);
    const rkey = runKey(r);
    // ANY bookend of this run naming an endpoint proves CLOUD; none of them
    // can prove LOCAL. See above for the asymmetry.
    if (p.endpoint && (isComplete || isDispatchStart(r.action) || isDispatchError(r.action))) cloudKeys.add(rkey);
    if (!isComplete) continue;
    if (p.endpoint) cloudTerminalKeys.add(rkey);
    if (!p.endpoint) localKeys.add(rkey);
    if (hasAnyTokenCounts(p)) {
      const arr = dcTok.get(rkey);
      if (arr) arr.push(p);
      else dcTok.set(rkey, [p]);
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
      // A `telemetry.tokens` record carries no `endpoint` of its own on any
      // producer (the only emitter is the container path's per-turn tailer,
      // `crates/darkmux-crew/src/dispatch_internal.rs:8539`, whose payload
      // is `{turn_seq, prompt_tokens, completion_tokens, total_tokens}`), so
      // this split can never be per-seat — the finest grain available to it
      // is the RUN the record itself belongs to. Which is precisely why the
      // key has to be `runKey` and not a bare session id: this record names
      // its own `mission_id`, so joining on `(session_id, mission_id)` reads
      // the verdict of the run that actually emitted it.
      //
      // The bare-session-id version of this line is what shipped the defect
      // this fix-pass exists to remove. `task-review-probe-high-task`
      // completes LOCALLY under three separate missions on 2026-08-07 and
      // then runs HOSTED under a fourth on 2026-08-08; keyed on the bare id,
      // the three stale local verdicts sat in the local set and swallowed the
      // hosted run's live telemetry — 144,638 tokens of Azure spend rendered
      // on the LOCAL tile for the 17m14s between the hosted start and its
      // own completion, with `cloudRuns=0` so the hero read "the hybrid loop
      // is humming" while an endpoint billed. Keyed on `runKey` the stale
      // verdicts live under their own missions' keys and cannot be reached.
      //
      // Precedence: CLOUD beats LOCAL when one run's own evidence disagrees
      // with itself (sibling seats sharing a task-scoped id, one hosted and
      // one local). That over-claims cloud for a genuinely mixed task and
      // never credits hosted spend as local. Pinned by name in
      // savings.test.ts, here AND on its run-level twin below.
      //
      // TWO sites read this evidence and they do NOT agree. The run loop's
      // per-bookend token sum (the telemetry-ABSENT path, at the bottom of
      // this function) classifies each completion on its OWN payload and
      // never consults `cloudKeys`. That is deliberate — there the tokens
      // ARE the completion's own payload, which is self-describing, where a
      // `telemetry.tokens` record names no seat and no endpoint — but it
      // means the same mixed run answers differently depending on whether
      // it emitted a telemetry family. Measured: `local=0 cloud=110` with
      // telemetry, `local=110 cloud=0` without. Pinned in savings.test.ts
      // so the divergence is a decision a future change has to make
      // deliberately, not discover.
      //
      // (#2709) This is the one place #2690's TOKEN half would have to be
      // fixed, and it cannot be fixed from the records that exist. Sibling
      // seats fanned out within one task share the session id AND the
      // mission id, so `runKey` is identical for them by construction, and
      // no third coordinate is available: `payload.step_id` appears on ZERO
      // of the 774 `telemetry.tokens` records across all four committed
      // corpora (the container path stamps it only for a graph step),
      // `handle` on such a record is the ROLE id, and `work_id` is `None`
      // on every crew record builder. So the precedence above stands and
      // #2690's three token shapes are unchanged by this pass.
      const rk = runKey(r);
      if (cloudKeys.has(rk)) {
        cloud += p.total_tokens || 0;
      } else if (!localKeys.has(rk)) {
        unknown += p.total_tokens || 0;
      }
      // (#2709) Grouped by `runKey`, the same key the verdicts use — so a
      // group is ONE run's turns. `runKey` already carries the composite
      // fallback for a sessionless record (two sessionless records sharing
      // a ts must not merge into one pseudo-session; it would corrupt the
      // turn_seq decomposition below), so there is one key expression here
      // rather than two that have to be kept in step.
      const arr = sess.get(rk);
      if (arr) arr.push({ ...p, ts: r.ts });
      else sess.set(rk, [{ ...p, ts: r.ts }]);
    }
  }

  // The class decomposition (fresh/re-read/generated) stays COMBINED across
  // tiers (one breakdown, not one per tier) — only the headline split above
  // is per-tier.
  let fresh = 0;
  let reread = 0;
  let uncls = 0;
  for (const [, recs] of sess) {
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

  // (#2709) ONE run loop over ONE key space, replacing the two loops that
  // used to split it (a `sess`-keyed branch that decided a whole group, and
  // a `dcTok`-keyed `directRuns` fallback for groups the first had skipped).
  // Splitting the decision across two collections is what made `cloudRuns`
  // fall: whichever loop reached a key LAST re-decided it from the evidence
  // that loop happened to hold. Here, every run key contributes its terms
  // ADDITIVELY, so no source of evidence can cancel another.
  //
  // The key space is every run key ANY evidence named. `sess` and `dcTok`
  // alone are not enough: a run whose only trace is a token-less
  // `dispatch.complete` (a LOCAL `dispatch.map` step's summary bookend —
  // `DispatchMapStepKind::bookend_record`, which reports a token total only
  // when the step is hosted) appears in neither, and was counted nowhere.
  // Measured on the committed two-day corpus: 50 run keys carry a
  // completion, `runs` reported 40.
  //
  // The four terms, and what each one is for:
  //
  //   PER-BOOKEND — every token-bearing completion is one run, classified
  //   on its OWN `endpoint` and on nothing else. Unchanged since #2635/
  //   #2690: a sibling with no endpoint of its own is positive LOCAL
  //   evidence, never floored by another sibling's endpoint. This is where
  //   #2690's measured `runs=1 cloudRuns=1`-for-a-local-seat defect lived.
  //
  //   EXTRA HOSTED RUN — a run key whose terminal named an endpoint while
  //   NO token-bearing bookend of that key did. That is a completed hosted
  //   dispatch whose endpoint omitted `usage` (`single_shot.rs:51`'s
  //   `total_tokens: null` shape), so it can never enter `dcTok`. Counting
  //   it here is what makes `cloudRuns` monotonic: before, it was counted
  //   only while the key had no token-bearing bookend at all, so a local
  //   sibling's completion landing LATER silently removed it.
  //
  //   EXTRA LOCAL RUN — the same term on the local side: a terminal naming
  //   no endpoint while no token-bearing bookend of that key is
  //   endpoint-less. The token-less local map-step completion above is
  //   exactly this. It is deliberately NOT "one run per completion record":
  //   a key holding a token-bearing local completion AND a token-less local
  //   map-step completion still counts ONE local run, which is what #2659's
  //   `hasAnyTokenCounts` conjunct has always protected and what the MUST
  //   FIX 2 test pins.
  //
  //   UNKNOWN — no bookend and no terminal at all: in flight, closed
  //   outside the window, errored, or a sessionless composite key. The
  //   #1607 bucket. Crediting it cloud would misreport the operator's own
  //   hardware; crediting it local would credit hosted spend as free.
  //
  // The TOKEN sum for a key's bookends is gated on `!sess.has(k)` — the
  // telemetry-exclusive rule that stops a run with both a telemetry family
  // and a token-bearing completion from being counted twice. (#2709) That
  // guard is keyed by RUN now. Keyed on the bare session id it fired across
  // RUNS, skipping run B because a DIFFERENT run A under the same session
  // id had telemetry: measured, 9,999 hosted tokens absent from `total`
  // entirely — under-reported, not misattributed, which is what made it
  // easy to miss.
  //
  // Classification of a bookend's tokens is PER-COMPLETION (#2635), not
  // per-key: each `dcTok` payload IS the `dispatch.complete` record that
  // proves its own tier, so it needs no lookup. The `unknown` arm is a
  // defensive floor only — a payload lacking `endpoint` satisfies the exact
  // criterion `localKeys` used to add this same record's run key, so
  // `!localKeys.has(k)` cannot fire today. It is kept for the same reason
  // it always was: a floor against that invariant drifting.
  let runs = 0;
  let cloudRuns = 0;
  let unknownRuns = 0;
  const runKeys = new Set<string>([...sess.keys(), ...dcTok.keys(), ...cloudTerminalKeys, ...localKeys]);
  for (const k of runKeys) {
    const bookends = dcTok.get(k) ?? [];
    const countTokens = !sess.has(k);
    let sawCloudBookend = false;
    let sawLocalBookend = false;
    for (const p of bookends) {
      runs++;
      if (p.endpoint) {
        cloudRuns++;
        sawCloudBookend = true;
      } else if (localKeys.has(k)) {
        sawLocalBookend = true;
      } else {
        // Unreachable floor (see above); deliberately does NOT set
        // `sawLocalBookend`, so the EXTRA LOCAL RUN term below stays
        // consistent with it if the invariant ever does drift.
        unknownRuns++;
      }
      if (!countTokens) continue;
      // `remote_tokens` is the review path's spelling for its own spend; the
      // other three are null there. Last in the chain so it never overrides
      // a record that reported the standard fields.
      const tt = p.total_tokens || (p.prompt_tokens || 0) + (p.completion_tokens || 0) || p.remote_tokens || 0;
      total += tt;
      if (p.endpoint) cloud += tt;
      else if (!localKeys.has(k)) unknown += tt;
      // The gap this used to name — "a completion with no `endpoint` whose
      // endpoint-bearing `dispatch.start` has scrolled outside the playhead
      // window is credited local rather than unknown" — is narrow: a start
      // that IS in the window registers its own run key into `cloudKeys`,
      // and `cloudKeys` beats local on the token split. Only a start that
      // has genuinely fallen out of the window leaves this completion
      // looking local. Every current producer stamps `endpoint` on BOTH
      // bookends of a hosted call (see `bookend_record` in builtins.rs), so
      // no live data hits it today; pinned in savings.test.ts so a future
      // producer that stops double-stamping makes the gap loud.
      prompt += p.prompt_tokens || 0;
      completion += p.completion_tokens || 0;
      // One turn means the whole prompt is first-read, so this is exact
      // here, not an approximation.
      fresh += p.prompt_tokens || 0;
      // A `remote_tokens`-only payload has no prompt/completion split to
      // decompose, so without this its spend joins `total` while appearing
      // in NO class chip.
      if (isRemoteOnlyTokens(p)) uncls += tt;
    }
    // EXTRA HOSTED RUN. `cloudTerminalKeys`, never `cloudKeys` — a run is
    // counted by its own TERMINAL, and a hosted START is not one. Reading
    // starts here would count an in-flight hosted seat as a finished
    // dispatch, and a `dispatch.error` is not a completion either.
    if (cloudTerminalKeys.has(k) && !sawCloudBookend) {
      runs++;
      cloudRuns++;
    }
    // EXTRA LOCAL RUN. No `cloudRuns`/`unknownRuns` term: an endpoint-less
    // terminal is positive local evidence, and local is the implicit
    // remainder `runs - cloudRuns - unknownRuns`, never a residual that
    // absorbs the unproven.
    if (localKeys.has(k) && !sawLocalBookend) runs++;
    if (bookends.length === 0 && !cloudTerminalKeys.has(k) && !localKeys.has(k)) {
      runs++;
      unknownRuns++;
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
    runs,
    cloudRuns,
    unknownRuns,
  };
}
