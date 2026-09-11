import { describe, it, expect } from "vitest";
import { hybridNote } from "./hybridNote";
import type { TokensOffMeter } from "./savings";
import type { FlowRecord } from "../../types/handwritten";

function rec(overrides: Partial<FlowRecord>): FlowRecord {
  return { ts: "2026-08-08T00:00:00.000Z", ...overrides };
}

const ZERO_TOKENS: TokensOffMeter = {
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
};

describe("hybridNote", () => {
  it("a real orchestrator note wins over every deterministic template — the latest one, by timestamp", () => {
    const data: FlowRecord[] = [
      rec({ action: "note", source: "orchestrator", handle: "shipped m6", ts: "2026-08-08T10:00:00.000Z" }),
      rec({ action: "note", source: "orchestrator", handle: "shipped m7", ts: "2026-08-08T12:00:00.000Z" }),
    ];
    const note = hybridNote(data, { ...ZERO_TOKENS, runs: 5, cloudRuns: 0 });
    expect(note.text).toContain("shipped m7");
    expect(note.text).not.toContain("shipped m6");
    expect(note.hasHistory).toBe(true);
  });

  it("a session-scoped note (adjudication trail) is ignored — dashboard notes are mission-level only", () => {
    const data: FlowRecord[] = [rec({ action: "note", source: "orchestrator", session_id: "s1", handle: "verdict: pass" })];
    const note = hybridNote(data, { ...ZERO_TOKENS, runs: 3, cloudRuns: 3 });
    expect(note.text).not.toContain("verdict: pass");
    expect(note.hasHistory).toBe(false);
  });

  it("falls back to the latest mission.run record when there's no orchestrator note", () => {
    const data: FlowRecord[] = [
      rec({ action: "mission.run.start", mission_id: "m1", ts: "2026-08-08T09:00:00.000Z" }),
      rec({ action: "mission.run.complete", mission_id: "m2", ts: "2026-08-08T11:00:00.000Z" }),
    ];
    const note = hybridNote(data, ZERO_TOKENS);
    expect(note.text).toContain("m2");
    expect(note.text).not.toContain("m1");
    expect(note.hasHistory).toBe(false);
  });

  it("all-local runs get the local-only template", () => {
    const note = hybridNote([], { ...ZERO_TOKENS, runs: 4, cloudRuns: 0 });
    expect(note.text).toBe("4 local dispatches. The hybrid loop is humming, keep it up.");
  });

  it("all-cloud runs get the cloud-only template", () => {
    const note = hybridNote([], { ...ZERO_TOKENS, runs: 3, cloudRuns: 3 });
    expect(note.text).toBe("3 dispatches via cloud. The right brain for the job, keep it up.");
  });

  it("a mix of local and cloud runs gets the combined template", () => {
    const note = hybridNote([], { ...ZERO_TOKENS, runs: 5, cloudRuns: 2 });
    expect(note.text).toBe("3 dispatches local + 2 via cloud. The hybrid loop is humming, keep it up.");
  });

  it("singular dispatch wording at exactly one run", () => {
    const note = hybridNote([], { ...ZERO_TOKENS, runs: 1, cloudRuns: 0 });
    expect(note.text).toBe("1 local dispatch. The hybrid loop is humming, keep it up.");
  });

  it("zero runs and no notes/missions gets the invitation", () => {
    const note = hybridNote([], ZERO_TOKENS);
    expect(note.text).toBe("going hybrid takes nerve. the fleet is ready when you are.");
    expect(note.hasHistory).toBe(false);
  });

  // (#2637) The issue's own reproduction: `local=1000 cloud=5700 unknown=1200
  // runs=5 cloudRuns=2` rendered "3 dispatches local + 2 via cloud" — crediting
  // both unattributed sessions to "local". With `unknownRuns` in the picture,
  // the true local count is 5 - 2 - 2 = 1.
  it("(#2637) unattributed sessions are excluded from the local count, not credited to it", () => {
    const note = hybridNote([], { ...ZERO_TOKENS, runs: 5, cloudRuns: 2, unknownRuns: 2 });
    expect(note.text).toBe("1 dispatch local + 2 via cloud. The hybrid loop is humming, keep it up.");
    // Never claim anything about the two unattributed sessions — no mention
    // of "unattributed"/"unknown" and no implication they were free.
    expect(note.text).not.toMatch(/unattributed|unknown|free/i);
  });

  it("(#2637) a mix of local and cloud runs with NO unattributed sessions is unaffected by the fix", () => {
    const note = hybridNote([], { ...ZERO_TOKENS, runs: 5, cloudRuns: 2, unknownRuns: 0 });
    expect(note.text).toBe("3 dispatches local + 2 via cloud. The hybrid loop is humming, keep it up.");
  });

  it("(#2637) local + unattributed, no cloud at all — the unattributed count is silently excluded, never folded into local", () => {
    const note = hybridNote([], { ...ZERO_TOKENS, runs: 5, cloudRuns: 0, unknownRuns: 2 });
    expect(note.text).toBe("3 local dispatches. The hybrid loop is humming, keep it up.");
  });

  it("(#2637) cloud + unattributed, no local at all — the unattributed count is silently excluded, never folded into cloud", () => {
    const note = hybridNote([], { ...ZERO_TOKENS, runs: 5, cloudRuns: 3, unknownRuns: 2 });
    expect(note.text).toBe("3 dispatches via cloud. The right brain for the job, keep it up.");
  });

  it("(#2637) every run this window is unattributed — a dedicated honest line, not a local-only or cloud-only guess", () => {
    const note = hybridNote([], { ...ZERO_TOKENS, runs: 4, cloudRuns: 0, unknownRuns: 4 });
    expect(note.text).toBe("4 dispatches with no attribution darkmux could confirm. keep going.");
    expect(note.text).not.toMatch(/\blocal\b|\bcloud\b|\bfree\b/i);
  });
});
