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
