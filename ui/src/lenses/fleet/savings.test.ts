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
    // (post-review, pinning a KNOWN gap; UNCHANGED by #2690) The RUN split
    // is per-bookend-exact (1 local + 1 cloud, asserted above), but the
    // aggregate TOKEN split is NOT — a `telemetry.tokens` record carries no
    // endpoint of its own, so the split can only read SESSION-scoped
    // evidence, and this session's evidence names an endpoint somewhere, so
    // ALL 330 tokens land in `cloud` and NONE in `local` even though 110 of
    // them were genuinely local. #2690 changed WHICH session-scoped
    // evidence is read (the session's own completions, not the union across
    // start/complete/error) but deliberately not the cloud-over-local
    // precedence — the real corpus has two cross-day sessions of exactly
    // this shape and moving them is a #2665 question, not a #2690 one. This
    // is the narrower gap named in `savings.ts`'s per-bookend loop comment,
    // pinned here so it's visible rather than assumed away.
    expect(t.cloud).toBe(330);
    expect(t.local).toBe(0);
  });

  // (#2690, SUPERSEDES a #2687 test) The predecessor of this test asserted
  // the opposite — that a lone bookend classifies via the session-wide
  // `epBySid` union ("still classifies cloud via epBySid, not the bookend
  // alone"). It was protecting a shape no producer emits. Every dispatch
  // path stamps ONE `endpoint_label` onto its start, its error, and its
  // complete alike: `DispatchSingleShotStepKind::bookend_record` and
  // `DispatchMapStepKind::bookend_record` (`crates/darkmux-crew/src/
  // step_kinds/builtins.rs:826-867` and `:1626-1669`, both ending in
  // `stamp_remote_classification(&mut payload, endpoint_label, None)`),
  // `dispatch_remote` (`dispatch_internal.rs:3217/3239/3303/3331`), and the
  // container path from a single `remote_endpoint_raw_label`
  // (`dispatch_internal.rs:5193` on the start, `:7531` on the terminal).
  //
  // So an endpoint held on a start whose own completion lacks one means the
  // two records belong to DIFFERENT SEATS sharing a task-scoped session id
  // (`darkmux_types::session_id::task`) — and reading the start's endpoint
  // onto this completion is not "recovering lost evidence", it is painting
  // one seat's evidence onto another. That is #2690's defect, and the same
  // argument #2635 used to remove the identical lookup from `directRuns`
  // and #2687 used to remove it from the per-bookend loop.
  //
  // What replaces it: the completion classifies itself. No endpoint of its
  // own ⇒ LOCAL, the same criterion `localSids` has always used to prove a
  // session local.
  it("(#2690) a single-bookend session whose endpoint is on its START but not its lone COMPLETE classifies LOCAL — the completion classifies itself", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "start-only-ep", action: "dispatch.start", handle: "reviewer", payload: { endpoint: "azure-foundry" } }),
      tokenRec("start-only-ep", 1, 100, 10),
      rec({ session_id: "start-only-ep", action: "dispatch.complete", payload: { total_tokens: 110 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(1);
    expect(t.cloudRuns).toBe(0);
    expect(t.unknownRuns).toBe(0);
    // Implicit local — never a residual, computed the way every consumer
    // must compute it.
    expect(t.runs - t.cloudRuns - t.unknownRuns).toBe(1);
    // The token split moves with it: both now read the SAME evidence
    // (the session's own successful terminals), so they cannot disagree.
    expect(t.cloud).toBe(0);
    expect(t.local).toBe(110);
    expect(t.unknown).toBe(0);
  });

  // (#2690) The case the retired lookup WAS right about, and which this fix
  // must not trade away: a genuine single hosted dispatch. Both bookends
  // name the endpoint (what every producer actually emits), so the
  // completion classifies itself cloud with no session-wide lookup needed.
  it("(#2690, inverted) the genuine single hosted dispatch — endpoint on BOTH bookends — still classifies cloud", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "genuine-hosted", action: "dispatch.start", handle: "reviewer", payload: { endpoint: "azure-foundry" } }),
      tokenRec("genuine-hosted", 1, 100, 10),
      rec({ session_id: "genuine-hosted", action: "dispatch.complete", payload: { total_tokens: 110, endpoint: "azure-foundry" } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(1);
    expect(t.cloudRuns).toBe(1);
    expect(t.unknownRuns).toBe(0);
    expect(t.cloud).toBe(110);
    expect(t.local).toBe(0);
    expect(t.unknown).toBe(0);
  });

  // (#2690, the STEADY-STATE measurement from the issue) A hosted reasoning
  // seat is minutes of wall clock (`builtins.rs:1146-1152`), and for that
  // whole window it has a `dispatch.start` naming its endpoint and no
  // terminal. Its local sibling seat, sharing the task-scoped session id,
  // has completed cleanly. Measured on main: `runs=1 cloudRuns=1
  // unknownRuns=0 localRuns=0` and the hero read "1 dispatch via cloud" —
  // the operator's OWN HARDWARE's work reported as cloud with local 0,
  // durably (if the hosted sibling then errors and no second local sibling
  // closes, it never corrects). Ground truth: 1 local dispatch, 0 cloud.
  it("(#2690 steady state) a hosted sibling in flight does not paint a completed local sibling cloud", () => {
    const sid = "task:review-probe-steady";
    const data: FlowRecord[] = [
      // The hosted seat: started, still running, no terminal.
      rec({ session_id: sid, action: "dispatch.start", handle: "judge-hosted", payload: { endpoint: "azure-foundry" } }),
      // The local seat: ran and closed cleanly.
      rec({ session_id: sid, action: "dispatch.start", handle: "judge-local" }),
      tokenRec(sid, 1, 90, 10, "2026-08-08T00:01:00Z"),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 100 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(1);
    expect(t.cloudRuns).toBe(0);
    expect(t.unknownRuns).toBe(0);
    expect(t.runs - t.cloudRuns - t.unknownRuns).toBe(1);
    expect(t.total).toBe(100);
    expect(t.local).toBe(100);
    expect(t.cloud).toBe(0);
    expect(t.unknown).toBe(0);
  });

  // (#2690, shape K1) The live rolling window's LEFT boundary is what makes
  // this reachable: a long hosted dispatch that began before `nowMs -
  // LIVE_WINDOW_MS` and failed inside it leaves a `dispatch.error` naming
  // its endpoint with no `dispatch.start` in view. (Playback scrubbing
  // cannot produce it — its filter is `ts <= playhead`, so a start never
  // drops while its error survives. Reachability in live data is SUSPECTED,
  // not observed.) On main, #2688's `isDispatchError` registration then put
  // that endpoint into `epBySid`, and the lone LOCAL sibling completion
  // under the same task-scoped id read `runs=1 cloudRuns=1 cloud=1000
  // local=0`.
  it("(#2690, K1) a hosted sibling's dispatch.error does not reclassify a local sibling's clean completion", () => {
    const sid = "task:review-probe-k1";
    const data: FlowRecord[] = [
      // The hosted seat's start is OUTSIDE the window; only its error is in.
      rec({ session_id: sid, action: "dispatch.error", handle: "judge-hosted", payload: { endpoint: "azure-foundry", result_class: "error" } }),
      rec({ session_id: sid, action: "dispatch.start", handle: "judge-local" }),
      tokenRec(sid, 1, 900, 100, "2026-08-08T00:01:00Z"),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 1000 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(1);
    expect(t.cloudRuns).toBe(0);
    expect(t.unknownRuns).toBe(0);
    expect(t.total).toBe(1000);
    expect(t.cloud).toBe(0);
    expect(t.local).toBe(1000);
    expect(t.unknown).toBe(0);
  });

  // (#2690, shape K2) The same window shape with TWO local siblings closed.
  // On main this was a CONTRADICTION ON ONE SCREEN: the run count took the
  // per-bookend path (#2687) and read `runs=2 cloudRuns=0` — "2 local
  // dispatches" — while the token tiles still read the session-wide
  // `epBySid` and showed LOCAL TOKENS 0 / CLOUD TOKENS 2,000 for the very
  // same session. Both halves now read the same evidence, so the two lines
  // agree by construction.
  it("(#2690, K2) the dispatches line and the token tiles agree — no local-dispatches-with-zero-local-tokens screen", () => {
    const sid = "task:review-probe-k2";
    const data: FlowRecord[] = [
      rec({ session_id: sid, action: "dispatch.error", handle: "judge-hosted", payload: { endpoint: "azure-foundry", result_class: "error" } }),
      rec({ session_id: sid, action: "dispatch.start", handle: "judge-local-1" }),
      tokenRec(sid, 1, 900, 100, "2026-08-08T00:01:00Z"),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 1000 } }),
      rec({ session_id: sid, action: "dispatch.start", handle: "judge-local-2" }),
      tokenRec(sid, 1, 900, 100, "2026-08-08T00:02:00Z"),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 1000 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(2);
    expect(t.cloudRuns).toBe(0);
    expect(t.unknownRuns).toBe(0);
    expect(t.total).toBe(2000);
    expect(t.cloud).toBe(0);
    expect(t.local).toBe(2000);
    // The invariant the contradiction violated: a screen showing N local
    // dispatches and zero local tokens for the same data is incoherent.
    const localRuns = t.runs - t.cloudRuns - t.unknownRuns;
    expect(localRuns).toBe(2);
    expect(t.local).toBeGreaterThan(0);
  });

  // (#2690, the ARITY-1 half of the `(CONSIDER 3)` gap #2687 named and
  // could not close) One hosted seat whose endpoint omits `usage` (so its
  // endpoint-bearing completion never enters `dcTok`) beside exactly ONE
  // local seat that did report tokens. #2687's own comment recorded the
  // consequence: "the identical group (one hosted-endpoint sibling, one
  // endpoint-less sibling) can still classify its endpoint-less member
  // LOCAL at arity 2 but CLOUD at arity 1, depending only on whether the
  // hosted sibling's own bookend happened to enter `dcTok`". The arity-2
  // form is pinned above ("a hosted sibling reporting no usage doesn't
  // float two local siblings to cloud"); this is the arity-1 form, and it
  // must give the SAME answer. RED-PROVEN: restoring the `length > 1`
  // guard sends this key to the zero-bookend branch, where the session's
  // hosted completion reads `cloudRuns=1` for a run that was local.
  it("(#2690) one local seat beside a usage-omitting hosted sibling classifies local at arity 1, exactly as it does at arity 2", () => {
    const arity1 = (sid: string): FlowRecord[] => [
      rec({ session_id: sid, action: "dispatch.start", handle: "hosted", payload: { endpoint: "azure-foundry" } }),
      rec({ session_id: sid, action: "dispatch.complete", payload: { endpoint: "azure-foundry", total_tokens: null } }),
      rec({ session_id: sid, action: "dispatch.start", handle: "local-1" }),
      tokenRec(sid, 1, 90, 10, "2026-08-08T00:01:00Z"),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 100 } }),
    ];
    const t = tokensOffMeter(arity1("task:null-usage-arity1"));
    expect(t.runs).toBe(1);
    expect(t.cloudRuns).toBe(0);
    expect(t.unknownRuns).toBe(0);
    expect(t.runs - t.cloudRuns - t.unknownRuns).toBe(1);
    // The TOKEN split is a different question and #2690 does not answer it:
    // a `telemetry.tokens` record has no endpoint of its own, this session
    // does hold a hosted completion, and cloud-over-local precedence is
    // unchanged — so the tokens read cloud while the run reads local. That
    // is the #2665 gap, and it is what the arity-2 form of this same group
    // has always done (pinned in the test above); asserting it here keeps
    // the two arities visibly identical instead of only claiming they are.
    expect(t.total).toBe(100);
    expect(t.cloud).toBe(100);
    expect(t.local).toBe(0);
    expect(t.unknown).toBe(0);
  });

  // (#2690) The zero-token-bearing-bookend branch's OWN cloud term. A
  // hosted seat whose endpoint omits `usage` stamps `endpoint` on a
  // `total_tokens: null` completion (`single_shot.rs:51`), which passes the
  // endpoint test but fails `hasAnyTokenCounts`, so it never enters
  // `dcTok`. There is no per-run bookend to classify on, but there IS a
  // successful terminal naming an endpoint — real cloud evidence, and
  // throwing it away would put a genuinely-completed hosted dispatch in
  // `unknownRuns` while its tokens still read cloud, splitting the two
  // halves of one screen apart again. RED-PROVEN: deleting
  // `if (cloudSids.has(k)) cloudRuns++;` flips this to `cloudRuns=0,
  // unknownRuns=1`.
  it("(#2690) a hosted completion reporting no usage still counts its run cloud when no token-bearing bookend exists", () => {
    const sid = "task:null-usage-solo";
    const data: FlowRecord[] = [
      rec({ session_id: sid, action: "dispatch.start", handle: "hosted", payload: { endpoint: "azure-foundry" } }),
      rec({ session_id: sid, action: "dispatch.complete", payload: { endpoint: "azure-foundry", total_tokens: null } }),
      tokenRec(sid, 1, 90, 10, "2026-08-08T00:01:00Z"),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(1);
    expect(t.cloudRuns).toBe(1);
    expect(t.unknownRuns).toBe(0);
    // The token split reads the same evidence, so the tiles agree with the
    // dispatches line rather than contradicting it.
    expect(t.total).toBe(100);
    expect(t.cloud).toBe(100);
    expect(t.local).toBe(0);
  });

  // (#2690) The telemetry-present / telemetry-absent pair for LOCAL work,
  // the mirror of the hosted pair pinned above. Same dispatch shape (a
  // hosted sibling in flight over a completed local seat), once with the
  // local seat's per-turn `telemetry.tokens` family present (the container
  // path, `dispatch_internal.rs:8539`) and once with only its completion's
  // own totals (the single-shot path). On main these diverged COMPLETELY —
  // `cloud=100 local=0` through the `sess` loop versus `cloud=0 local=100`
  // through `directRuns`, which has classified per-completion since #2635.
  it("(#2690) telemetry-present and telemetry-absent forms of the same LOCAL work classify identically", () => {
    const startOnlyHosted = (sid: string) =>
      rec({ session_id: sid, action: "dispatch.start", handle: "hosted-sibling", payload: { endpoint: "azure-foundry" } });

    const dataPresent: FlowRecord[] = [
      startOnlyHosted("task:pair-present"),
      tokenRec("task:pair-present", 1, 90, 10, "2026-08-08T00:01:00Z"),
      rec({ session_id: "task:pair-present", action: "dispatch.complete", payload: { total_tokens: 100 } }),
    ];
    const dataAbsent: FlowRecord[] = [
      startOnlyHosted("task:pair-absent"),
      rec({
        session_id: "task:pair-absent",
        action: "dispatch.complete",
        payload: { total_tokens: 100, prompt_tokens: 90, completion_tokens: 10 },
      }),
    ];
    const tPresent = tokensOffMeter(dataPresent);
    const tAbsent = tokensOffMeter(dataAbsent);

    expect(tPresent.runs).toBe(tAbsent.runs);
    expect(tPresent.cloudRuns).toBe(tAbsent.cloudRuns);
    expect(tPresent.unknownRuns).toBe(tAbsent.unknownRuns);
    expect(tPresent.total).toBe(tAbsent.total);
    expect(tPresent.cloud).toBe(tAbsent.cloud);
    expect(tPresent.local).toBe(tAbsent.local);
    expect(tPresent.unknown).toBe(tAbsent.unknown);
    // And the value both agree on is the truthful one.
    expect(tPresent.local).toBe(100);
    expect(tPresent.cloud).toBe(0);
    expect(tPresent.cloudRuns).toBe(0);
  });

  // (#2690) MONOTONICITY over the WHOLE arrival sequence, not just one
  // transition. A cloud count that goes DOWN as a real cloud dispatch lands
  // is the symptom an operator actually notices, and the arity<=1 branch
  // was its last remaining source: #2687's own monotonicity test had to
  // exclude the "after B alone" prefix, where a lone local completion read
  // cloudRuns=1 (floored by A's endpoint through `epBySid`) and then
  // dropped to 0 as C arrived. Every prefix of the four-seat sequence is
  // now non-decreasing, with no carve-out.
  it("(#2690) cloudRuns is non-decreasing across EVERY prefix of the four-seat arrival sequence", () => {
    const sid = "task:review-probe-monotone";
    const arrivals: FlowRecord[] = [
      rec({ session_id: sid, action: "dispatch.start", handle: "reviewer-A", payload: { endpoint: "azure-foundry" } }),
      rec({ session_id: sid, action: "dispatch.error", payload: { endpoint: "azure-foundry", result_class: "error" } }),
      rec({ session_id: sid, action: "dispatch.start", handle: "reviewer-B" }),
      tokenRec(sid, 1, 90, 10, "2026-08-08T00:01:00Z"),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 100 } }),
      rec({ session_id: sid, action: "dispatch.start", handle: "reviewer-C" }),
      tokenRec(sid, 1, 180, 20, "2026-08-08T00:02:00Z"),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 200 } }),
      rec({ session_id: sid, action: "dispatch.start", handle: "reviewer-D", payload: { endpoint: "azure-foundry" } }),
      tokenRec(sid, 1, 270, 30, "2026-08-08T00:03:00Z"),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 300, endpoint: "azure-foundry" } }),
    ];

    const observed: number[] = [];
    for (let i = 1; i <= arrivals.length; i++) {
      observed.push(tokensOffMeter(arrivals.slice(0, i)).cloudRuns);
    }
    for (let i = 1; i < observed.length; i++) {
      expect(observed[i]).toBeGreaterThanOrEqual(observed[i - 1]);
    }
    // And the sequence itself: 0 until D's own hosted completion lands.
    expect(observed[observed.length - 1]).toBe(1);
    expect(Math.max(...observed.slice(0, observed.length - 1))).toBe(0);
  });

  // (post-adversarial-review correction, #2659/#2687 follow-up) An earlier
  // version of this test asserted that a `dispatch.start`'s endpoint
  // FLOORS every bookend in a spanning group to cloud even when neither
  // bookend restates it — that was the defect this fix removes, not
  // desired behavior. `start-only-multi`'s session id is shared by TWO
  // real completions with no endpoint of their own; the corrected,
  // per-bookend rule classifies both LOCAL, exactly matching what a
  // single-bookend session with an endpoint-only START would NOT do (see
  // the test above it — that one stays cloud because it is genuinely ONE
  // dispatch, not a group of siblings).
  it("(post-review) a spanning group whose bookends carry no endpoint of their own classifies LOCAL, not floored by a dispatch.start", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "start-only-multi", action: "dispatch.start", handle: "coder", payload: { endpoint: "azure-foundry" } }),
      tokenRec("start-only-multi", 1, 100, 10, "2026-08-08T00:00:00Z"),
      rec({ session_id: "start-only-multi", action: "dispatch.complete", payload: { total_tokens: 110 } }),
      tokenRec("start-only-multi", 1, 200, 20, "2026-08-08T00:05:00Z"),
      rec({ session_id: "start-only-multi", action: "dispatch.complete", payload: { total_tokens: 220 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(2);
    expect(t.cloudRuns).toBe(0);
    expect(t.unknownRuns).toBe(0);
  });

  // (MUST FIX 1, #2659/#2687 follow-up — the FALSIFYING measurement) The
  // producer this finding was actually named for: a review-probe task with
  // FOUR sibling seats sharing one task-scoped session id — A (hosted,
  // ERRORS), B and C (local, complete cleanly), D (hosted, completes with
  // real usage). Measured against the floored code this fix removes: at
  // the moment C completes (the second local completion), the group had
  // `runs=2, cloudRuns=2` — "2 dispatches via cloud" — when ground truth
  // is 2 local, 0 cloud. The per-bookend rule (no `epBySid` floor) gets
  // this exactly right.
  it("(MUST FIX 1) four sibling seats sharing one session id: after the second local completion, cloudRuns=0 and both are local", () => {
    const sid = "task:review-probe-1";
    const data: FlowRecord[] = [
      rec({ session_id: sid, action: "dispatch.start", handle: "reviewer-A", payload: { endpoint: "azure-foundry" } }),
      rec({ session_id: sid, action: "dispatch.error" }),
      rec({ session_id: sid, action: "dispatch.start", handle: "reviewer-B" }),
      tokenRec(sid, 1, 90, 10, "2026-08-08T00:01:00Z"),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 100 } }),
      rec({ session_id: sid, action: "dispatch.start", handle: "reviewer-C" }),
      tokenRec(sid, 1, 180, 20, "2026-08-08T00:02:00Z"),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 200 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(2);
    expect(t.cloudRuns).toBe(0);
    // Implicit local = runs - cloudRuns - unknownRuns.
    expect(t.unknownRuns).toBe(0);
    expect(t.runs - t.cloudRuns - t.unknownRuns).toBe(2);
  });

  // (MUST FIX 1 continued) Extending the same four-seat sequence to D's
  // real hosted completion. `cloudRuns` climbs from 0 (after C) to 1
  // (after D) — non-decreasing across this transition, which is the exact
  // shape the FLOORED code got backwards: with the floor, D's own endpoint
  // disabled `bookendHasEndpoint`'s floor for the WHOLE group, dropping
  // `cloudRuns` from 2 (after C, floored) to 1 (after D) — a decrease the
  // operator would have watched happen live. The fix restores the correct
  // direction for this transition.
  //
  // (#2690 update) This test's carve-out is GONE. It used to say the full
  // arrival sequence was NOT monotone, because the arity<=1 branch floored
  // a LONE bookend via the session-wide `epBySid` map — `cloudRuns` read 1
  // right after B alone completed (A's start/error having registered the
  // endpoint) and dipped to 0 as C arrived. #2690 removed that lookup, so
  // every prefix is now non-decreasing; the whole-sequence claim is pinned
  // separately below.
  it("(MUST FIX 1) cloudRuns is non-decreasing across the arity-2-to-3 transition (C completing, then D)", () => {
    const sid = "task:review-probe-2";
    const throughC: FlowRecord[] = [
      rec({ session_id: sid, action: "dispatch.start", handle: "reviewer-A", payload: { endpoint: "azure-foundry" } }),
      rec({ session_id: sid, action: "dispatch.error" }),
      rec({ session_id: sid, action: "dispatch.start", handle: "reviewer-B" }),
      tokenRec(sid, 1, 90, 10, "2026-08-08T00:01:00Z"),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 100 } }),
      rec({ session_id: sid, action: "dispatch.start", handle: "reviewer-C" }),
      tokenRec(sid, 1, 180, 20, "2026-08-08T00:02:00Z"),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 200 } }),
    ];
    const afterC = tokensOffMeter(throughC);
    expect(afterC.cloudRuns).toBe(0);

    const throughD: FlowRecord[] = [
      ...throughC,
      rec({ session_id: sid, action: "dispatch.start", handle: "reviewer-D", payload: { endpoint: "azure-foundry" } }),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 300, endpoint: "azure-foundry" } }),
    ];
    const afterD = tokensOffMeter(throughD);
    expect(afterD.runs).toBe(3);
    expect(afterD.cloudRuns).toBe(1);
    // Non-decreasing across THIS transition (0 -> 1), unlike the floored
    // code's 2 -> 1 drop for the equivalent transition.
    expect(afterD.cloudRuns).toBeGreaterThanOrEqual(afterC.cloudRuns);
  });

  // (MUST FIX 1, second independent producer) A hosted seat whose endpoint
  // omits `usage` entirely stamps `endpoint` on a `total_tokens: null`
  // complete (`single_shot.rs:51`) — it sets `epBySid` but fails
  // `hasAnyTokenCounts`, so it never enters `dcTok` and never joins this
  // group. Before this fix, that alone was enough to float the floor: two
  // genuinely local siblings under the same task-scoped id both flipped to
  // cloud. The per-bookend rule is unmoved by evidence outside the group.
  it("(MUST FIX 1) a hosted sibling reporting no usage doesn't float two local siblings to cloud", () => {
    const sid = "task:null-usage-hosted";
    const data: FlowRecord[] = [
      rec({ session_id: sid, action: "dispatch.start", handle: "hosted", payload: { endpoint: "azure-foundry" } }),
      rec({ session_id: sid, action: "dispatch.complete", payload: { endpoint: "azure-foundry", total_tokens: null } }),
      rec({ session_id: sid, action: "dispatch.start", handle: "local-1" }),
      tokenRec(sid, 1, 90, 10, "2026-08-08T00:01:00Z"),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 100 } }),
      rec({ session_id: sid, action: "dispatch.start", handle: "local-2" }),
      tokenRec(sid, 1, 180, 20, "2026-08-08T00:02:00Z"),
      rec({ session_id: sid, action: "dispatch.complete", payload: { total_tokens: 200 } }),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(2);
    expect(t.cloudRuns).toBe(0);
    expect(t.unknownRuns).toBe(0);
    // (#2690) The TOKEN split for this same group, asserted so the arity-1
    // form of it can be SHOWN to match rather than merely claimed to. The
    // hosted sibling's completion DOES name an endpoint, and the split is
    // session-scoped with cloud-over-local precedence, so both local seats'
    // tokens read cloud here — the #2665 gap, unchanged by #2690.
    expect(t.total).toBe(300);
    expect(t.cloud).toBe(300);
    expect(t.local).toBe(0);
  });

  // (MUST FIX 1) The telemetry-present and telemetry-absent forms of the
  // same dispatch shape (a hosted single dispatch that names its endpoint
  // on both bookends) must classify IDENTICALLY — the `sess`-loop path
  // (telemetry family present) and the `directRuns` fallback (telemetry
  // family absent) are two different code paths reaching the SAME
  // `dcTok` evidence, and neither should disagree with the other now that
  // both are purely per-bookend.
  it("(MUST FIX 1) telemetry-present and telemetry-absent forms of the same hosted dispatch classify identically", () => {
    const dataAbsent: FlowRecord[] = [
      rec({ session_id: "direct-cloud-1", action: "dispatch.start", payload: { endpoint: "azure-foundry" } }),
      rec({ session_id: "direct-cloud-1", action: "dispatch.complete", payload: { endpoint: "azure-foundry", total_tokens: 500 } }),
    ];
    const tAbsent = tokensOffMeter(dataAbsent);

    const dataPresent: FlowRecord[] = [
      rec({ session_id: "direct-cloud-2", action: "dispatch.start", payload: { endpoint: "azure-foundry" } }),
      tokenRec("direct-cloud-2", 1, 450, 50, "2026-08-08T00:00:00Z"),
      rec({ session_id: "direct-cloud-2", action: "dispatch.complete", payload: { endpoint: "azure-foundry", total_tokens: 500 } }),
    ];
    const tPresent = tokensOffMeter(dataPresent);

    expect(tAbsent.runs).toBe(tPresent.runs);
    expect(tAbsent.cloudRuns).toBe(tPresent.cloudRuns);
    expect(tAbsent.cloud).toBe(tPresent.cloud);
    expect(tAbsent.total).toBe(tPresent.total);
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

  // (MUST FIX 2, post-adversarial-review correction, #2659 follow-up)
  // `hasAnyTokenCounts` is the single conjunct standing between "one real
  // dispatch bookend" and "any `dispatch.complete` at all" entering
  // `dcTok` — and per the module doc above `dcTok`'s declaration, a LOCAL
  // `dispatch.map` step's own summary bookend carries NO token field,
  // sharing a session id with a sibling seat's genuine, token-bearing
  // completion. Without the conjunct, that token-less bookend would ALSO
  // enter `dcTok`, pushing this session's bookend count from 1 to 2 and
  // routing it through the multi-bookend path — counting TWO runs for
  // what is really one model dispatch plus one step-completion record
  // that never called a model at all.
  //
  // An earlier version of this test used `payload: {}` for the token-less
  // completion — a shape NO producer ever emits. `DispatchMapStepKind::
  // bookend_record` (`crates/darkmux-crew/src/step_kinds/builtins.rs:1626-
  // 1660`) always seeds `{step_id, kind: "dispatch.map", runtime:
  // "scheduler"}`, and its "dispatch complete" call site
  // (`builtins.rs:2053-2064`) merges `{result_class, items_in, ok_count,
  // failed_count}` on top — `stamp_remote_classification` no-ops both its
  // fields when `endpoint_label` is `None` (a local step), so NEITHER
  // `endpoint` NOR any token field is ever added, but the record is never
  // an empty object. `payload: {}` let a mutant substituting
  // `hasAnyTokenCounts(p)` for `Object.keys(p).length > 0` pass this test
  // AND the full suite (44/44 here, 1697/1697 across the UI suite) while
  // being wrong on exactly this shape: the real 6-key payload below has
  // `Object.keys(p).length > 0` true, so the mutant would count it as a
  // second bookend and double the run count. RED-PROVEN by deleting the
  // `&& hasAnyTokenCounts(p)` conjunct AND by substituting the
  // `Object.keys` mutant at the `dcTok` collection site: this test flips
  // from `runs === 1` to `runs === 2` under either mutation.
  it("(MUST FIX 2) a token-bearing completion plus a token-LESS local map-step completion sharing one session id count as ONE run, not two", () => {
    const data: FlowRecord[] = [
      rec({ session_id: "task:map-mix", action: "dispatch.start", handle: "coder" }),
      tokenRec("task:map-mix", 1, 90, 10, "2026-08-08T00:00:00Z"),
      // The genuine model dispatch's own bookend — carries tokens.
      rec({ session_id: "task:map-mix", action: "dispatch.complete", payload: { total_tokens: 100 } }),
      // A LOCAL `dispatch.map` step's own summary bookend under the SAME
      // session id — the ACTUAL producer shape from `bookend_record` +
      // its "dispatch complete" call site, carrying no `endpoint` and no
      // token field, but very much NOT an empty object.
      rec({
        session_id: "task:map-mix",
        action: "dispatch.complete",
        payload: {
          step_id: "map-step-1",
          kind: "dispatch.map",
          runtime: "scheduler",
          result_class: "ok",
          items_in: 3,
          ok_count: 3,
          failed_count: 0,
        },
      }),
    ];
    const t = tokensOffMeter(data);
    expect(t.runs).toBe(1);
    expect(t.cloudRuns).toBe(0);
    expect(t.unknownRuns).toBe(0);
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
