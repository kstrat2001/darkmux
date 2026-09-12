import { describe, it, expect } from "vitest";
import { tokensOffMeter, hasAnyTokenCounts, isRemoteOnlyTokens } from "./savings";
import type { FlowRecord } from "../../types/handwritten";

function rec(overrides: Partial<FlowRecord>): FlowRecord {
  return { ts: "2026-08-08T00:00:00.000Z", ...overrides };
}

function tokenRec(sid: string, turnSeq: number | undefined, prompt: number, completion: number, ts = "2026-08-08T00:00:00.000Z"): FlowRecord {
  return rec({
    ts,
    session_id: sid,
    category: "telemetry",
    source: "tokens",
    payload: {
      prompt_tokens: prompt,
      completion_tokens: completion,
      total_tokens: prompt + completion,
      ...(turnSeq != null ? { turn_seq: turnSeq } : {}),
    },
  });
}

describe("tokensOffMeter", () => {
  it("counts a locally-run session (clean complete, no endpoint) as local, with the re-read/fresh split across its turns", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "s1", action: "dispatch.start", handle: "coder" }),
      tokenRec("s1", 1, 100, 20),
      tokenRec("s1", 2, 150, 30),
      rec({ session_id: "s1", action: "dispatch.complete", payload: { total_tokens: 300 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.total).toBe(300);
    expect(t.cloud).toBe(0);
    expect(t.unknown).toBe(0);
    expect(t.local).toBe(300);
    expect(t.completion).toBe(50);
    // turn 2's prompt (150) overlaps turn 1's prompt (100) by min(150,100)=100
    expect(t.reread).toBe(100);
    expect(t.fresh).toBe(150); // (100+150) - 100
    expect(t.runs).toBe(1);
    expect(t.cloudRuns).toBe(0);
    expect(t.unknownRuns).toBe(0);
  });

  it("counts a session with an endpoint-bearing dispatch bookend as cloud, not local", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "s2", action: "dispatch.start", handle: "coder", payload: { endpoint: "azure-foundry" } }),
      tokenRec("s2", 1, 200, 40),
      rec({ session_id: "s2", action: "dispatch.complete", payload: { total_tokens: 240, endpoint: "azure-foundry" } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.total).toBe(240);
    expect(t.cloud).toBe(240);
    expect(t.local).toBe(0);
    expect(t.unknown).toBe(0);
    expect(t.cloudRuns).toBe(1);
    expect(t.unknownRuns).toBe(0);
  });

  it("excludes a session with NO dispatch bookend at all from the local claim — unknown, not free (#1607)", () => {
    // Token telemetry with no dispatch.start/complete anywhere for the
    // session — darkmux has no evidence of where this ran.
    const data: FlowRecord[] = [tokenRec("s3", 1, 500, 10)];
    const t = tokensOffMeter(data);
    expect(t.total).toBe(510);
    expect(t.unknown).toBe(510);
    expect(t.cloud).toBe(0);
    // The load-bearing invariant: local is what's LEFT after cloud AND
    // unknown are removed — never a residual that silently absorbs the
    // unproven tokens as "off the meter".
    expect(t.local).toBe(0);
    // Still counted as a real dispatch for the run count, just not credited
    // as free.
    expect(t.runs).toBe(1);
    // (#2637) And the run-count analog of `unknown` above: this run is
    // unattributed, not silently folded into "local" the way a bare
    // `runs - cloudRuns` subtraction would.
    expect(t.unknownRuns).toBe(1);
    expect(t.cloudRuns).toBe(0);
  });

  it("a dispatch.error (died before classifying itself) does NOT count its session as local", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "s4", action: "dispatch.start", handle: "coder" }),
      tokenRec("s4", 1, 80, 5),
      rec({ session_id: "s4", action: "dispatch.error", payload: { exit_code: 1 } }),
    ];
    const t = tokensOffMeter(data);
    // dispatch.error is excluded from localSids by construction (only a
    // CLEAN dispatch.complete with no endpoint proves local) — this session
    // has no endpoint either, so it's unknown, not local.
    expect(t.unknown).toBe(85);
    expect(t.local).toBe(0);
    expect(t.unknownRuns).toBe(1);
  });

  it("a session missing turn_seq on any of its records falls into `uncls`, not fresh/reread", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "s5", action: "dispatch.start", handle: "coder" }),
      tokenRec("s5", undefined, 300, 60),
      rec({ session_id: "s5", action: "dispatch.complete", payload: { total_tokens: 360 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.uncls).toBe(300);
    expect(t.fresh).toBe(0);
    expect(t.reread).toBe(0);
  });

  it("the single-shot remote fallback (no telemetry family, dispatch.complete carries the totals) counts as cloud", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "s6", action: "dispatch.start", handle: "reviewer", payload: { endpoint: "gemini" } }),
      rec({
        session_id: "s6",
        action: "dispatch.complete",
        payload: { endpoint: "gemini", total_tokens: 900, prompt_tokens: 800, completion_tokens: 100 },
      }),
    ];
    const t = tokensOffMeter(data);
    expect(t.total).toBe(900);
    expect(t.cloud).toBe(900);
    expect(t.fresh).toBe(800); // one turn = the whole prompt is first-read
    expect(t.runs).toBe(1);
    expect(t.cloudRuns).toBe(1);
    expect(t.unknownRuns).toBe(0);
  });

  it("(#1853) the single-shot LOCAL fallback (no telemetry family, no endpoint, dispatch.complete carries the totals) counts as local — not invisible", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "s8", action: "dispatch.start", handle: "radio-router" }),
      rec({
        session_id: "s8",
        action: "dispatch.complete",
        payload: { total_tokens: 970, prompt_tokens: 900, completion_tokens: 70 },
      }),
    ];
    const t = tokensOffMeter(data);
    expect(t.total).toBe(970);
    expect(t.local).toBe(970);
    expect(t.cloud).toBe(0);
    expect(t.unknown).toBe(0);
    expect(t.fresh).toBe(900); // one turn = the whole prompt is first-read
    expect(t.runs).toBe(1);
    // A local direct run must not inflate the cloud run count — hybridNote
    // (#2637) derives local runs as `t.runs - t.cloudRuns - t.unknownRuns`.
    expect(t.cloudRuns).toBe(0);
    expect(t.unknownRuns).toBe(0);
  });

  it("(#1853, inverted) a session with BOTH a telemetry family AND a token-bearing local dispatch.complete is not double-counted", () => {
    // Before #1853, this session's dispatch.complete (token-bearing, no
    // endpoint) never entered `dcTok` at all (the endpoint gate skipped
    // collection), so the `sess.has` double-count guard below was never
    // exercised for the endpoint-less path. Now that collection is
    // endpoint-blind, the same guard has to hold: a session already fully
    // counted via its telemetry family must not ALSO have its
    // dispatch.complete totals summed in — that would double-count rather
    // than fix the undercount.
    const data: FlowRecord[] = [
      rec({ session_id: "s9", action: "dispatch.start", handle: "coder" }),
      tokenRec("s9", 1, 100, 20),
      rec({ session_id: "s9", action: "dispatch.complete", payload: { total_tokens: 120 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.total).toBe(120); // NOT 240
    expect(t.local).toBe(120);
    expect(t.cloud).toBe(0);
    expect(t.unknown).toBe(0);
    expect(t.runs).toBe(1); // NOT 2 (no phantom directRun for s9)
    expect(t.unknownRuns).toBe(0);
  });

  it("(#1853, inverted) a cloud single-shot session is still classified cloud, never local, once collection is endpoint-blind", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "s10", action: "dispatch.start", handle: "reviewer", payload: { endpoint: "azure-foundry" } }),
      rec({
        session_id: "s10",
        action: "dispatch.complete",
        payload: { endpoint: "azure-foundry", total_tokens: 500, prompt_tokens: 450, completion_tokens: 50 },
      }),
    ];
    const t = tokensOffMeter(data);
    expect(t.total).toBe(500);
    expect(t.cloud).toBe(500);
    expect(t.local).toBe(0);
    expect(t.unknown).toBe(0);
    expect(t.cloudRuns).toBe(1);
    expect(t.unknownRuns).toBe(0);
  });

  // (#2635) `dispatch.single_shot`'s session id is deliberately TASK-scoped
  // — sibling seats fanned out within one task can complete under the SAME
  // session id. `dcTok` used to be a single-value Map keyed by session id,
  // so the second completion silently overwrote the first: the reviewer's
  // exact repro was one task, two single-shot steps sharing session
  // `task:t3`, no telemetry family — a hosted seat ({endpoint, total_tokens:
  // 700}) and a local seat ({total_tokens: 500}). Pre-fix the LAST WRITE
  // won regardless of order, so ordering mattered and tokens vanished
  // (`total=500 cloud=500` when the local seat wrote last — a local seat's
  // tokens painted on the CLOUD tile, and the 700 real hosted tokens gone
  // entirely). Both orderings below must land on the SAME correct totals,
  // and neither seat's tokens may be lost.
  it("(#2635) task-scoped session collision — hosted-then-local — both seats' tokens survive, correctly attributed", () => {
    const data: FlowRecord[] = [
      rec({
        session_id: "task:t3",
        action: "dispatch.complete",
        payload: { endpoint: "azure:h/gpt-4o", total_tokens: 700 },
      }),
      rec({ session_id: "task:t3", action: "dispatch.complete", payload: { total_tokens: 500 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.total).toBe(1200);
    expect(t.cloud).toBe(700);
    expect(t.local).toBe(500);
    expect(t.unknown).toBe(0);
    expect(t.cloudRuns).toBe(1);
    expect(t.runs).toBe(2);
    expect(t.unknownRuns).toBe(0);
  });

  it("(#2635) task-scoped session collision — local-then-hosted — same totals regardless of order", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "task:t3b", action: "dispatch.complete", payload: { total_tokens: 500 } }),
      rec({
        session_id: "task:t3b",
        action: "dispatch.complete",
        payload: { endpoint: "azure:h/gpt-4o", total_tokens: 700 },
      }),
    ];
    const t = tokensOffMeter(data);
    expect(t.total).toBe(1200);
    expect(t.cloud).toBe(700);
    expect(t.local).toBe(500);
    expect(t.unknown).toBe(0);
    expect(t.cloudRuns).toBe(1);
    expect(t.runs).toBe(2);
    expect(t.unknownRuns).toBe(0);
  });

  it("(#2635) task-scoped session collision — two local seats — both counted, neither dropped", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "task:t3c", action: "dispatch.complete", payload: { total_tokens: 500 } }),
      rec({ session_id: "task:t3c", action: "dispatch.complete", payload: { total_tokens: 700 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.total).toBe(1200);
    expect(t.local).toBe(1200);
    expect(t.cloud).toBe(0);
    expect(t.unknown).toBe(0);
    expect(t.cloudRuns).toBe(0);
    expect(t.runs).toBe(2);
    expect(t.unknownRuns).toBe(0);
  });

  // (CONSIDER 3, #2635) Documents a currently-inert gap rather than fixing
  // it: a completion that carries no `endpoint` of its own is credited to
  // `local` even when the endpoint-bearing `dispatch.start` that would have
  // proven it hosted has scrolled outside the caller's playhead window (see
  // `tokensOffMeter`'s module doc on the playhead gate). Every producer
  // today stamps `endpoint` on BOTH bookends of a hosted call
  // (builtins.rs's `bookend_record`), so this can't happen from live data —
  // this test pins today's behavior so a future producer that stops
  // double-stamping makes the gap LOUD (a failing test) instead of a
  // silent misclassification.
  it("(CONSIDER 3, #2635) a lone endpoint-less completion is credited to local, even though its hosted start may be off-window — known gap, pinned", () => {
    const data: FlowRecord[] = [
      rec({
        session_id: "s11",
        action: "dispatch.complete",
        payload: { total_tokens: 640, prompt_tokens: 600, completion_tokens: 40 },
      }),
    ];
    const t = tokensOffMeter(data);
    expect(t.local).toBe(640);
    expect(t.cloud).toBe(0);
    expect(t.unknown).toBe(0);
    expect(t.unknownRuns).toBe(0);
  });

  // (#2637) The issue's own reproduction shape: five sessions where only
  // one is POSITIVELY local, two are positively cloud, and two carry token
  // telemetry but no dispatch bookend at all (darkmux has no evidence
  // either way). Before this fix, `hybridNote` read `runs - cloudRuns` and
  // credited both unattributed sessions to "local" (reading "3 local + 2
  // cloud" instead of the true "1 local + 2 cloud"). This test pins the
  // run-count-level three-way split `tokensOffMeter` now exposes;
  // hybridNote.test.ts pins the corrected rendered text.
  it("(#2637) five sessions — one local, two cloud, two unattributed — unknownRuns separates them from local", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "cloud1", action: "dispatch.start", handle: "reviewer", payload: { endpoint: "gemini" } }),
      tokenRec("cloud1", 1, 200, 40),
      rec({ session_id: "cloud1", action: "dispatch.complete", payload: { total_tokens: 240, endpoint: "gemini" } }),

      rec({ session_id: "cloud2", action: "dispatch.start", handle: "reviewer", payload: { endpoint: "gemini" } }),
      tokenRec("cloud2", 1, 100, 20),
      rec({ session_id: "cloud2", action: "dispatch.complete", payload: { total_tokens: 120, endpoint: "gemini" } }),

      rec({ session_id: "local1", action: "dispatch.start", handle: "coder" }),
      tokenRec("local1", 1, 80, 10),
      rec({ session_id: "local1", action: "dispatch.complete", payload: { total_tokens: 90 } }),

      // Both of these have token telemetry but NO dispatch.start/complete
      // anywhere for their session — no evidence of where they ran.
      tokenRec("unknown1", 1, 50, 5),
      tokenRec("unknown2", 1, 30, 5),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(5);
    expect(t.cloudRuns).toBe(2);
    expect(t.unknownRuns).toBe(2);
    // The implicit local-run count a consumer derives is runs - cloudRuns -
    // unknownRuns = 5 - 2 - 2 = 1, matching the ONE session with positive
    // local evidence (local1) — not 3, which is what the pre-fix
    // `runs - cloudRuns` subtraction would have produced.
    expect(t.runs - t.cloudRuns - t.unknownRuns).toBe(1);
  });

  // (#2659) The issue's own reproduction shape: a single session id closes
  // with TWO real `dispatch.complete` bookends (a deterministic
  // `mission_run` session id reused across a re-launch, per #1856 — same
  // population, different code path: #1856 fixed the TURN sort within a
  // session, this fixes the RUN count across sessions). Before this fix
  // `runs` was `sess.size` — one per distinct KEY — so this session
  // undercounted to 1 no matter how many real dispatches closed under it.
  it("(#2659) a session id spanning two dispatches counts as TWO runs, not one — sess.size undercounted this", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "spanning2", action: "dispatch.start", handle: "coder" }),
      tokenRec("spanning2", 1, 100, 10, "2026-08-08T00:00:00Z"),
      rec({ session_id: "spanning2", action: "dispatch.complete", payload: { total_tokens: 110 } }),

      // The re-launch: same session id, second dispatch, own bookend.
      rec({ session_id: "spanning2", action: "dispatch.start", handle: "coder" }),
      tokenRec("spanning2", 1, 200, 20, "2026-08-08T00:05:00Z"),
      rec({ session_id: "spanning2", action: "dispatch.complete", payload: { total_tokens: 220 } }),
    ];
    const t = tokensOffMeter(data);
    // Two real completions under one session id — this is what the fix is
    // for. The pre-fix behavior was `t.runs === 1` here.
    expect(t.runs).toBe(2);
    expect(t.cloudRuns).toBe(0);
    expect(t.unknownRuns).toBe(0);
    // Total tokens are unaffected — this bug was never a token-counting
    // bug, only a run-COUNT bug.
    expect(t.total).toBe(330);
  });

  // (#2659, inverted) The over-counting failure mode named in the issue: a
  // run that genuinely IS one dispatch (one session id, ONE
  // `dispatch.complete` bookend) must still count as one, even though the
  // fix now looks past `sess.size` to bookend count. A fix that counted
  // every telemetry turn or every start+complete pair as a separate run
  // would fail this the same way undercounting failed the test above.
  it("(#2659, inverted) an ordinary single-dispatch session still counts as ONE run under the bookend-count fix", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "ordinary2", action: "dispatch.start", handle: "coder" }),
      tokenRec("ordinary2", 1, 100, 10),
      tokenRec("ordinary2", 2, 50, 5),
      rec({ session_id: "ordinary2", action: "dispatch.complete", payload: { total_tokens: 165 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(1);
    expect(t.cloudRuns).toBe(0);
    expect(t.unknownRuns).toBe(0);
  });

  // (#2659) The classification MUST move with the run count, per the
  // issue's own caution: a session whose two bookends disagree (one local,
  // one cloud) must produce ONE cloudRun and no unknownRun, not two
  // cloudRuns (over-crediting cloud) and not a session-wide cloud
  // classification via the old `epBySid.has(k)` aggregate check (which
  // would have painted the local bookend cloud too).
  it("(#2659) a spanning session with mixed local+cloud bookends classifies each bookend on its OWN endpoint", () => {
    const data: FlowRecord[] = [
      // First dispatch: local (no endpoint).
      rec({ session_id: "mixed", action: "dispatch.start", handle: "coder" }),
      tokenRec("mixed", 1, 100, 10, "2026-08-08T00:00:00Z"),
      rec({ session_id: "mixed", action: "dispatch.complete", payload: { total_tokens: 110 } }),

      // Second dispatch under the SAME session id: cloud (named endpoint).
      rec({ session_id: "mixed", action: "dispatch.start", handle: "coder", payload: { endpoint: "gemini" } }),
      tokenRec("mixed", 1, 200, 20, "2026-08-08T00:05:00Z"),
      rec({ session_id: "mixed", action: "dispatch.complete", payload: { total_tokens: 220, endpoint: "gemini" } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(2);
    expect(t.cloudRuns).toBe(1);
    expect(t.unknownRuns).toBe(0);
    // Implicit local run count: 2 - 1 - 0 = 1, matching the ONE genuinely
    // local bookend — not 0 (which the old session-wide `epBySid.has(k)`
    // aggregate check would have produced, since the session DOES have an
    // endpoint-bearing bookend somewhere).
    expect(t.runs - t.cloudRuns - t.unknownRuns).toBe(1);
    // (post-review, pinning a KNOWN gap) The RUN split is per-bookend-exact
    // (1 local + 1 cloud, asserted above), but the aggregate TOKEN split
    // is NOT — `cloud`/`local` still classify every telemetry turn in this
    // session via the session-wide `epBySid`, which is true for "mixed"
    // (it DOES have a cloud bookend), so ALL 330 tokens land in `cloud`
    // and NONE in `local` — even though 110 of them were genuinely local.
    // This is the narrower gap named in `savings.ts`'s per-bookend loop
    // comment (a #2665 follow-up would need per-bookend TURN attribution
    // to close it), pinned here so it's visible rather than assumed away.
    expect(t.cloud).toBe(330);
    expect(t.local).toBe(0);
  });

  // (post-review MUST-FIX regression test) A session with exactly ONE
  // bookend must classify via the ORIGINAL `epBySid.has(k)` aggregate —
  // never via that bookend's own `endpoint` field in isolation. `epBySid`
  // registers from a `dispatch.start` OR a `dispatch.complete` naming an
  // endpoint; here the START names one but the single COMPLETE doesn't
  // (a producer edge case, not asserted to be common — see `savings.ts`'s
  // own `(CONSIDER 3, #2635)` note on completions that don't re-state their
  // start's endpoint). A `bookends[0].endpoint`-only check would flip this
  // session from cloud to local — a real regression the #2659 fix must not
  // introduce for the single-bookend case, which is nearly every session.
  it("(post-review) a single-bookend session with an endpoint on its START (not its lone COMPLETE) still classifies cloud via epBySid, not the bookend alone", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "start-only-ep", action: "dispatch.start", handle: "reviewer", payload: { endpoint: "azure-foundry" } }),
      tokenRec("start-only-ep", 1, 100, 10),
      rec({ session_id: "start-only-ep", action: "dispatch.complete", payload: { total_tokens: 110 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(1);
    expect(t.cloudRuns).toBe(1);
    expect(t.unknownRuns).toBe(0);
    // The token-level split already used `epBySid` (unchanged by this fix)
    // and was never at risk — pinned alongside the run-level assertions so
    // the two can't drift apart silently.
    expect(t.cloud).toBe(110);
    expect(t.local).toBe(0);
  });

  // (post-review, MINOR — a second population the fix widens) `dispatch.
  // single_shot`'s session id is TASK-scoped (`session_id::task`), so
  // sibling seats fanned out within one task can share it. The `(#2635)`
  // tests above already cover this shape for sessions with NO telemetry
  // family (handled by `directRuns`, unaffected by this fix). This pins
  // the OTHER case: sibling seats that ALSO carry a `telemetry.tokens`
  // family (so they're `sess` members) now correctly count as separate
  // runs too, via the same `dcTok`-bookend mechanism — before this fix
  // they collapsed to 1 (`sess.size`), same undercount shape as the
  // mission-run spanning case, just from a different producer.
  it("(post-review) three sibling single-shot seats sharing one task-scoped session id, each WITH telemetry, count as THREE runs", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "task:siblings", action: "dispatch.complete", payload: { total_tokens: 100 } }),
      tokenRec("task:siblings", 1, 90, 10, "2026-08-08T00:00:00Z"),

      rec({ session_id: "task:siblings", action: "dispatch.complete", payload: { total_tokens: 200 } }),
      tokenRec("task:siblings", 1, 180, 20, "2026-08-08T00:01:00Z"),

      rec({ session_id: "task:siblings", action: "dispatch.complete", payload: { total_tokens: 300 } }),
      tokenRec("task:siblings", 1, 270, 30, "2026-08-08T00:02:00Z"),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(3);
    expect(t.cloudRuns).toBe(0);
    expect(t.unknownRuns).toBe(0);
    expect(t.total).toBe(600);
  });

  it("a remote_tokens-only completion (the review path's own spelling) counts as cloud AND unclassified", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "s7", action: "dispatch.start", handle: "pr-reviewer", payload: { endpoint: "gemini" } }),
      rec({ session_id: "s7", action: "dispatch.complete", payload: { endpoint: "gemini", remote_tokens: 1200 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.total).toBe(1200);
    expect(t.cloud).toBe(1200);
    // No prompt/completion split to decompose — it has to land somewhere
    // visible, or the headline would silently exceed the row beneath it.
    expect(t.uncls).toBe(1200);
    expect(t.cloudRuns).toBe(1);
    expect(t.unknownRuns).toBe(0);
  });

  // (#1856, mechanism corrected in the fix-pass review) A session id can
  // legitimately be shared by TWO SEPARATE DISPATCHES: `mission_run(
  // mission_id, phase_id)` (`crates/darkmux-types/src/session_id.rs`) is
  // DETERMINISTIC, so re-launching or retrying the same mission phase
  // inside the viewer's 24h window reuses the identical session id, and
  // each launch's own coder dispatch restarts its turn counter at 1. (An
  // earlier version of this comment attributed the spanning shape to a
  // single launch's worktree/coder/verify steps all dispatching under one
  // session id — that's wrong on inspection: the worktree step makes no
  // model dispatch at all (`SeatClaim::NoModel`), and the verify step
  // mints its OWN `phase-review-<secs>` session id
  // (`phase_review_output_at`, `src/phase_cli.rs`) rather than reusing the
  // shared one. Only the coder step ever dispatches under the shared
  // `mission_run` id — so within ONE launch there is exactly one
  // turn-bearing dispatch, never a spanning shape. The restart only
  // appears across separate launches of the same phase.)
  //
  // Sorting by `turn_seq` alone (the pre-fix behavior) interleaves the two
  // launches' turns — turn_seq=1 from the later launch sorts adjacent to
  // turn_seq=1 from the earlier one even though they are 5 minutes apart —
  // and the overlap estimator then compares prompt sizes across a launch
  // boundary where no re-read relationship exists. Sorting by `ts` instead
  // groups each launch's turns together, so only the ONE genuine
  // launch-boundary pair is ever compared.
  //
  // Hand-computed (see PR description for the arithmetic): sorting by ts
  // yields prompt sequence [1000,50,60, 2000,80,90] → reread=320,
  // fresh=2960. The pre-fix turn_seq sort ties on turn_seq 1/2/3 across
  // the two launches and (stable sort, insertion order breaks the tie)
  // yields [1000,2000,50,80,60,90] → reread=1220, fresh=2060 — 900 tokens
  // misclassified in this deliberately small repro of the corpus-scale
  // 663k figure from the issue.
  it("(#1856) a session id shared by two separate dispatches (turn_seq restarts on re-launch) sorts by ts, not turn_seq — reread stays confined within each dispatch", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "spanning", action: "dispatch.start", handle: "coder" }),
      // Launch 1's coder dispatch: turn_seq 1..3, ts T..T+2s.
      tokenRec("spanning", 1, 1000, 0, "2026-08-08T00:00:00Z"),
      tokenRec("spanning", 2, 50, 0, "2026-08-08T00:00:01Z"),
      tokenRec("spanning", 3, 60, 0, "2026-08-08T00:00:02Z"),
      // Launch 2's coder dispatch (the phase re-launched, same deterministic
      // session id): turn_seq RESTARTS at 1, ts 5 minutes later.
      tokenRec("spanning", 1, 2000, 0, "2026-08-08T00:05:00Z"),
      tokenRec("spanning", 2, 80, 0, "2026-08-08T00:05:01Z"),
      tokenRec("spanning", 3, 90, 0, "2026-08-08T00:05:02Z"),
      rec({ session_id: "spanning", action: "dispatch.complete", payload: { total_tokens: 3280 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.reread).toBe(320);
    expect(t.fresh).toBe(2960);
    expect(t.reread + t.fresh).toBe(3280);
  });

  // (#1856 inverted case, strengthened in the fix-pass review — CONSIDER 5)
  // An ORDINARY session — one dispatch, turn_seq monotonic AND ts monotonic
  // — must classify identically under the ts-first sort as it always did.
  // A re-sort that fixes the spanning case but reclassifies ordinary
  // sessions would be worse than the bug it fixes.
  //
  // The ORIGINAL version of this test pushed its records into `data` in
  // ts/turn_seq order — so ANY comparator that happens to leave an
  // already-sorted array alone (including a no-op "don't sort at all" bug)
  // passed it too, proving nothing about the comparator specifically.
  // Pushed here out of insertion order instead (turn 2, then turn 1, then
  // turn 3) with prompt sizes chosen so insertion order and ts order
  // produce DIFFERENT reread/fresh splits — a no-sort or insertion-order-
  // preserving bug now fails this test, while the real ts-first sort
  // still produces the pre-fix-equivalent numbers.
  it("(#1856, inverted) an ordinary single-dispatch session with monotonic turn_seq AND ts is unaffected by the ts-first sort", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "ordinary", action: "dispatch.start", handle: "coder" }),
      // Insertion order (turn 2, turn 1, turn 3) deliberately disagrees
      // with both ts order and turn_seq order (both of which agree with
      // each other: turn1@:00 → turn2@:05 → turn3@:10).
      tokenRec("ordinary", 2, 100, 0, "2026-08-08T00:00:05Z"),
      tokenRec("ordinary", 1, 500, 0, "2026-08-08T00:00:00Z"),
      tokenRec("ordinary", 3, 300, 0, "2026-08-08T00:00:10Z"),
      rec({ session_id: "ordinary", action: "dispatch.complete", payload: { total_tokens: 900 } }),
    ];
    const t = tokensOffMeter(data);
    // Correct ts-ordered sequence is [500,100,300] (turn1, turn2, turn3):
    // rr = min(500,100) + min(100,300) = 100 + 100 = 200; fresh = 900-200=700.
    // (Preserving the wrong insertion order [100,500,300] would instead
    // give rr = min(100,500)+min(500,300) = 100+300 = 400, fresh = 500 —
    // a different, wrong answer this fixture now catches.)
    expect(t.reread).toBe(200);
    expect(t.fresh).toBe(700);
  });

  // (MUST FIX 2, #1856 fix-pass) Total-order proof. An earlier version of
  // the comparator branched per-pair ("if BOTH sides parse, compare by ts;
  // else fall to turn_seq") and was provably intransitive: with a
  // well-timed turn A, a corrupt-timestamp turn B, and a well-timed-but-
  // EARLIER turn C, that shape yielded A<B, B<C, AND A>C simultaneously —
  // three different sorted outputs depending on incidental input order
  // (`reread` observed swinging between 20 and 1010 purely from
  // permutation). The current comparator resolves each side to a number
  // BEFORE branching (an unparseable `ts` maps to `+Infinity`, sorting
  // last), which restores a real total order: EVERY permutation of these
  // three turns must sort into the exact same order (C, A, B) and produce
  // the exact same fresh/reread split.
  it("(MUST FIX 2, #1856 fix-pass) all six permutations of a well-timed/corrupt-timestamp/earlier-well-timed triple sort identically", () => {
    // A: well-timed, later. B: corrupt ts. C: well-timed, earlier.
    const A = () => tokenRec("totalorder", 1, 2000, 0, "2026-08-08T00:05:00Z");
    const B = () => tokenRec("totalorder", 5, 10, 0, "not-a-timestamp");
    const C = () => tokenRec("totalorder", 9, 1000, 0, "2026-08-08T00:00:00Z");
    const bookend = () =>
      rec({ session_id: "totalorder", action: "dispatch.complete", payload: { total_tokens: 3010 } });
    const start = () => rec({ session_id: "totalorder", action: "dispatch.start", handle: "coder" });

    const permutations: Record<string, () => FlowRecord[]> = {
      ABC: () => [A(), B(), C()],
      ACB: () => [A(), C(), B()],
      BAC: () => [B(), A(), C()],
      BCA: () => [B(), C(), A()],
      CAB: () => [C(), A(), B()],
      CBA: () => [C(), B(), A()],
    };

    // Correct total order is C, A, B (B's unparseable ts pushes it last,
    // regardless of its turn_seq): prompt sequence [1000, 2000, 10] →
    // rr = min(1000,2000) + min(2000,10) = 1000 + 10 = 1010;
    // fresh = 3010 - 1010 = 2000.
    for (const [label, build] of Object.entries(permutations)) {
      const data: FlowRecord[] = [start(), ...build(), bookend()];
      const t = tokensOffMeter(data);
      expect(t.reread, `permutation ${label}`).toBe(1010);
      expect(t.fresh, `permutation ${label}`).toBe(2000);
    }
  });

  // (#1856 tie case) Equal timestamps are exactly where a sort changes
  // behavior unpredictably. `ts` is second-precision (`ts_utc_now()`), so a
  // same-second tie is POSSIBLE in principle — measured across both parity
  // corpora (731 adjacent same-session token-telemetry pairs, fix-pass
  // review), same-second adjacent turns are ZERO in practice, not common.
  // The tiebreak below is a defensive floor for the case, not a response to
  // an observed one. Records are pushed into `data` OUT of turn_seq order
  // (turn_seq=2's record precedes turn_seq=1's) to prove the tiebreak reads
  // `turn_seq`, not the records' incidental array/insertion order — a naive
  // `ts`-only sort with no tiebreak would silently preserve the (wrong)
  // insertion order here instead.
  it("(#1856 tie case) records sharing one ts tiebreak on turn_seq, not on array insertion order", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "tied", action: "dispatch.start", handle: "coder" }),
      // Pushed turn_seq=2 BEFORE turn_seq=1 — insertion order disagrees
      // with turn_seq order on purpose.
      tokenRec("tied", 2, 999, 0, "2026-08-08T00:00:00Z"),
      tokenRec("tied", 1, 10, 0, "2026-08-08T00:00:00Z"),
      tokenRec("tied", 3, 500, 0, "2026-08-08T00:00:00Z"),
      rec({ session_id: "tied", action: "dispatch.complete", payload: { total_tokens: 1509 } }),
    ];
    const t = tokensOffMeter(data);
    // Correct turn_seq-ordered sequence is [10,999,500]:
    // rr = min(10,999) + min(999,500) = 10 + 500 = 510; fresh = 1509-510=999.
    // (Preserving the wrong insertion order [999,10,500] would instead give
    // rr=20, fresh=1489 — see the PR description's arithmetic.)
    expect(t.reread).toBe(510);
    expect(t.fresh).toBe(999);
  });

  it("returns all-zero on an empty window", () => {
    const t = tokensOffMeter([]);
    expect(t).toEqual({
      total: 0,
      local: 0,
      cloud: 0,
      unknown: 0,
      prompt: 0,
      completion: 0,
      fresh: 0,
      reread: 0,
      uncls: 0,
      runs: 0,
      cloudRuns: 0,
      unknownRuns: 0,
    });
  });
});

/** (#1852) Before this, `savings.ts` compared the DOTTED literal only, and was
 * correct purely because its records had passed through `buildFlowWindow`,
 * which normalizes. Nothing stated or tested that coupling — so a caller that
 * fed it raw records (a direct Redis consumer, a new lens, a test) got silent
 * misattribution: every crew-lineage dispatch would fall to `unknown`.
 *
 * These feed RAW, un-normalized records in the spaced spelling the crew path
 * actually writes to disk. Red-proved: reverting to the literal comparison
 * flips `local` to 0 and dumps the whole total into `unknown`. */
describe("bookend spelling independence (#1852)", () => {
  const raw = (action: string, payload: Record<string, unknown>) =>
    ({
      ts: "2026-08-16T00:00:00Z",
      action,
      session_id: "s1",
      category: "machinery",
      source: "dispatch",
      payload,
    }) as unknown as FlowRecord;

  const tokens = () =>
    ({
      ts: "2026-08-16T00:00:01Z",
      action: "telemetry.tokens",
      session_id: "s1",
      category: "telemetry",
      source: "tokens",
      payload: { total_tokens: 1000, prompt_tokens: 900, completion_tokens: 100, turn_seq: 0 },
    }) as unknown as FlowRecord;

  it("attributes a SPACED-spelling completion as local, not unknown", () => {
    const out = tokensOffMeter([raw("dispatch complete", {}), tokens()]);
    expect(out.unknown).toBe(0);
    expect(out.total).toBe(1000);
  });

  it("still attributes the DOTTED spelling as local", () => {
    const out = tokensOffMeter([raw("dispatch.complete", {}), tokens()]);
    expect(out.unknown).toBe(0);
  });

  it("a SPACED completion naming an endpoint is still cloud, not local", () => {
    const out = tokensOffMeter([raw("dispatch complete", { endpoint: "azure" }), tokens()]);
    expect(out.cloud).toBe(1000);
    expect(out.unknown).toBe(0);
  });
});

describe("hasAnyTokenCounts", () => {
  it("is false for an empty payload", () => {
    expect(hasAnyTokenCounts({})).toBe(false);
  });

  it("is false when all four fields are zero", () => {
    expect(hasAnyTokenCounts({ total_tokens: 0, prompt_tokens: 0, completion_tokens: 0, remote_tokens: 0 })).toBe(false);
  });

  it("is true when total_tokens is nonzero", () => {
    expect(hasAnyTokenCounts({ total_tokens: 5 })).toBe(true);
  });

  it("is true when prompt_tokens is nonzero", () => {
    expect(hasAnyTokenCounts({ prompt_tokens: 5 })).toBe(true);
  });

  it("is true when completion_tokens is nonzero", () => {
    expect(hasAnyTokenCounts({ completion_tokens: 5 })).toBe(true);
  });

  it("is true when remote_tokens is nonzero", () => {
    expect(hasAnyTokenCounts({ remote_tokens: 5 })).toBe(true);
  });
});

describe("isRemoteOnlyTokens", () => {
  it("is true when only remote_tokens is nonzero (others absent)", () => {
    expect(isRemoteOnlyTokens({ remote_tokens: 5 })).toBe(true);
  });

  it("is true when remote_tokens is nonzero and the other three are zero", () => {
    expect(isRemoteOnlyTokens({ total_tokens: 0, prompt_tokens: 0, completion_tokens: 0, remote_tokens: 5 })).toBe(true);
  });

  it("is false when total_tokens is nonzero", () => {
    expect(isRemoteOnlyTokens({ total_tokens: 1, remote_tokens: 5 })).toBe(false);
  });

  it("is false when prompt_tokens is nonzero", () => {
    expect(isRemoteOnlyTokens({ prompt_tokens: 1, remote_tokens: 5 })).toBe(false);
  });

  it("is false when completion_tokens is nonzero", () => {
    expect(isRemoteOnlyTokens({ completion_tokens: 1, remote_tokens: 5 })).toBe(false);
  });

  it("is false when remote_tokens is absent", () => {
    expect(isRemoteOnlyTokens({})).toBe(false);
  });

  it("is false when remote_tokens is zero", () => {
    expect(isRemoteOnlyTokens({ remote_tokens: 0 })).toBe(false);
  });
});
