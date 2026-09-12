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
 * classifies every token-bearing bookend on its OWN `endpoint` at every
 * arity (the `length > 0` guard), consulting no set at all. That is where
 * #2690's measured `runs=1 cloudRuns=1 unknownRuns=0` defect lived — a
 * local seat reported as cloud because a hosted sibling's start was read
 * through a session-wide map.
 *
 * SCOPE, stated precisely so the next reader does not inherit an overclaim:
 * this pass re-keys the VERDICTS (`cloudKeys`/`cloudTerminalKeys`/
 * `localKeys`) by run. It does NOT re-key the GROUPING — `dcTok` and `sess`
 * are still keyed by bare `session_id` (see `dcTok`'s own comment), because
 * changing that changes what counts as a run and moves `runs` itself. So a
 * per-bookend classification is immune to the session id's failure modes
 * for the bookends it HOLDS, and says nothing about runs in the same group
 * that contributed no token-bearing bookend. Measured on the committed
 * corpus: 50 distinct `(session_id, mission_id)` keys carry a dispatch
 * bookend, and `runs` reports 40. Three consequences of that are pinned as
 * tests in savings.test.ts under "known gaps" rather than left to be
 * rediscovered a fifth time.
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
  /** This turn's own RUN key (`runKey`) — the coordinate the zero-bookend
   * run branch classifies on, so a group's verdict is drawn from the runs
   * its own turns belong to and never from a different run that reused the
   * session id. */
  rkey: string;
}

/** A `dispatch.complete` payload carried alongside the RUN key of the
 * record it came from, so the direct-run loop can consult evidence scoped
 * to that completion's own run rather than to its (possibly recurring)
 * session id. */
interface Bookend extends TokenPayload {
  rkey: string;
}

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
  //
  // `dcTok` stays keyed on the bare SESSION id, deliberately: it is the
  // GROUPING that the run count and the `sess.has(sid)` double-count guard
  // below are built on, and re-keying it would change what counts as a run.
  // Each stored bookend instead carries its OWN `rkey`, so classification
  // is run-scoped even though grouping is not.
  //
  // That split is a real, measured limitation, not a free simplification.
  // Three consequences, all PRE-EXISTING (identical on `main`) and all
  // pinned in savings.test.ts under "known gaps":
  //
  //   (a) `runs` under-counts a recurring session id: 50 distinct
  //       `(session_id, mission_id)` keys carry a bookend on the committed
  //       corpus, and `runs` reports 40.
  //   (b) `cloudRuns` is not monotonic — see `cloudTerminalKeys` below.
  //   (c) `sess.has(sid)` skips a whole RUN's tokens when a DIFFERENT run
  //       under the same session id had a telemetry family. Measured: a
  //       hosted run's 9,999 tokens absent from `total` entirely, so the
  //       tiles under-report hosted spend rather than misattribute it.
  //
  // Fixing these means re-keying the grouping, which moves `runs` itself
  // and every surface derived from it. That is a separate, measured change
  // (#2665's neighborhood), not something to slip into a verdict fix.
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
  // Keeping starts and errors OUT of the run-count set AVOIDS ONE WAY of
  // making `cloudRuns` fall: a hosted seat's start would push a
  // zero-bookend group to `cloudRuns=1`, and its local sibling's own
  // completion would then take over via the per-bookend branch and drop it
  // back to 0.
  //
  // It does NOT make `cloudRuns` monotonic, and an earlier revision of this
  // comment claimed it did. That claim is false and was measured false:
  // `cloudRuns` has TWO writers — this zero-bookend branch and the
  // per-bookend branch — and the second takes over the moment ONE
  // token-bearing completion joins the group, re-deciding the whole group
  // from the bookends it happens to hold. A hosted seat whose endpoint
  // omits `usage` emits `{endpoint, result_class: "ok", total_tokens: null}`
  // (`SingleShotReply::total_tokens` is `Option<u64>`,
  // `crates/darkmux-crew/src/single_shot.rs:51`, serialized at
  // `step_kinds/builtins.rs:1165-1175` while `endpoint_label` is stamped
  // unconditionally at `:844`), so it fails `hasAnyTokenCounts`, never
  // enters `dcTok`, and is INVISIBLE to the per-bookend branch. Measured
  // sequence for that seat beside one local sibling: `[0,0,1,1,0]`.
  // Documented and pinned in savings.test.ts rather than claimed away; the
  // real fix is run-scoped GROUPING, which this PR deliberately does not
  // attempt (see the `dcTok` comment above).
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
      const b: Bookend = { ...p, rkey };
      const arr = dcTok.get(r.session_id);
      if (arr) arr.push(b);
      else dcTok.set(r.session_id, [b]);
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
      // the three stale local verdicts sat in `localSids` and swallowed the
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
      // THREE sites read this evidence, not two, and the third does NOT
      // apply this precedence: `directRuns` (the telemetry-ABSENT path,
      // near the bottom of this function) classifies each completion on its
      // OWN payload and never consults `cloudKeys`. That is deliberate —
      // there the tokens ARE the completion's own payload, which is
      // self-describing, where a `telemetry.tokens` record names no seat
      // and no endpoint — but it means the same mixed run answers
      // differently depending on whether it emitted a telemetry family.
      // Measured: `local=0 cloud=110` with telemetry, `local=110 cloud=0`
      // without. Pinned in savings.test.ts so the divergence is a decision
      // a future change has to make deliberately, not discover.
      const rk = runKey(r);
      if (cloudKeys.has(rk)) {
        cloud += p.total_tokens || 0;
      } else if (!localKeys.has(rk)) {
        unknown += p.total_tokens || 0;
      }
      // Composite fallback key: two sessionless records sharing a ts must
      // not merge into one pseudo-session (it would corrupt the turn_seq
      // decomposition below). session_id is the norm; this is defensive.
      const k = r.session_id || `ts:${r.ts}:${r.handle || ""}:${r.machine_uid || ""}`;
      if (!sess.has(k)) sess.set(k, []);
      // `rkey` rides along so the zero-bookend run branch below can classify
      // on the runs this group's own turns belong to. The GROUPING stays
      // session-keyed (changing it would change the run count); only the
      // evidence lookup is run-scoped.
      sess.get(k)!.push({ ...p, ts: r.ts, rkey: rk });
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
        // Known, narrower gap, still open: the aggregate TOKEN split
        // (`cloud`/`unknown` a few dozen lines up) classifies every
        // `telemetry.tokens` record by its RUN's evidence
        // (`cloudKeys`/`localKeys`), not per-bookend — a `telemetry.tokens`
        // record carries no endpoint of its own on any producer, so nothing
        // narrower is available without partitioning a run's turns at each
        // bookend's timestamp.
        //
        // (MUST FIX 4, fix-pass correction) What that means concretely,
        // stated to MATCH THE CODE: a genuinely MIXED run — sibling seats
        // under one `(session_id, mission_id)`, one hosted and one local —
        // renders "N local + M cloud" on the DISPATCHES line while ALL of
        // its tokens go to `cloud`, because `cloudKeys` wins the precedence
        // on the split above. The tokens do NOT fall to `unknown`; an
        // earlier revision of this comment claimed they did and cited
        // #1607's under-claim direction for it, which was wrong on both
        // counts — the test directly below the claim
        // ("...cloud=330, local=0") has always asserted the opposite.
        // Over-claiming CLOUD is the deliberate choice here: a task with any
        // hosted seat really did bill, so crediting the whole task's tokens
        // to cloud over-reports the meter and never under-reports it.
        // Tracked as the #2665 follow-up; pinned in savings.test.ts so the
        // gap is visible, not silently assumed away.
        sessRuns++;
      }
    } else {
      // ZERO token-bearing bookends for this key: still in flight, closed
      // outside the window, closed with no usable token count, errored, or
      // a sessionless composite key. There is no per-run completion to
      // classify on, so fall back to the run-scoped bookend evidence — a
      // hosted completion that named an endpoint but reported no usage
      // (`single_shot.rs:51`'s `total_tokens: null` shape) is real cloud
      // evidence even though it can't enter `dcTok`, and so is a hosted
      // start whose run has not closed yet. With no evidence at all the
      // answer is `unknownRuns`, the #1607 bucket: crediting it cloud would
      // misreport the operator's own hardware, and crediting it local would
      // credit hosted spend as free.
      //
      // The evidence consulted is the set of RUN keys this group's own
      // turns carry — NOT `k`, which is a bare session id and therefore the
      // union across every run that reused it. A group can legitimately
      // span runs (that is what a deterministic session id does), so the
      // question "did any run whose turns are in this group bill a hosted
      // endpoint" is the one that has to be asked. A sessionless composite
      // key yields run keys that no bookend can ever register, so it always
      // lands in `unknownRuns`, same as the token split treats a
      // sessionless record.
      //
      // (MUST FIX 3, fix-pass) Cloud-over-local, the SAME precedence as the
      // token split above and for the same reason. This ordering was
      // previously unpinned: mutating this line to
      // `cloudKeys.has(...) && !localKeys.has(...)` left the whole suite
      // green, while the identical rule on the token side WAS pinned. It is
      // pinned on both sides now.
      // `cloudTerminalKeys`, NOT `cloudKeys` — a run is classified by its
      // own TERMINAL here (see the two-set comment where they are built).
      // Reading a hosted start in this branch would make `cloudRuns` fall
      // back to 0 when a local sibling's completion later takes over via
      // the per-bookend branch, which is the monotonicity break #2690's
      // four-seat arrival test exists to catch.
      const groupKeys = new Set(recs.map((t) => t.rkey));
      let groupCloud = false;
      let groupLocal = false;
      for (const gk of groupKeys) {
        if (cloudTerminalKeys.has(gk)) groupCloud = true;
        if (localKeys.has(gk)) groupLocal = true;
      }
      if (groupCloud) cloudRuns++;
      else if (!groupLocal) unknownRuns++;
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
  // lacking `endpoint` satisfies the exact same criterion `localKeys` used
  // to add this bookend's own run key (`isDispatchComplete` + no
  // `endpoint`, on this very record), so `!localKeys.has(p.rkey)` should
  // never fire — and it is checked against the BOOKEND's own `rkey`, not
  // the session id, so the floor stays run-scoped like everything else —
  // kept anyway
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
      } else if (!localKeys.has(p.rkey)) {
        unknown += tt;
        unknownRuns++; // (#2637) same "unknown, not free" at run granularity
      }
      // (fix-pass) The gap this used to name — "a completion with no
      // `endpoint` whose endpoint-bearing `dispatch.start` has scrolled
      // outside the playhead window is credited local rather than unknown"
      // — is NARROWER now, because a start that IS in the window registers
      // its own run key into `cloudKeys`, and `cloudKeys` beats local on the
      // token split. Only a start that has genuinely fallen out of the
      // window leaves this completion looking local. Every current producer
      // stamps `endpoint` on BOTH bookends of a hosted call (see
      // `bookend_record` in builtins.rs), so no live data hits it today;
      // pinned in savings.test.ts so a future producer that stops
      // double-stamping makes the gap loud instead of silent.
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
