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

  // (#2834) The local/cloud/unattributed split is withdrawn from this line.
  // Nine tests below used to pin the four templates that divided it. What
  // survives is the invariant they were really protecting — #2637's "never
  // imply local, cloud, or free" — now asserted over the whole input space
  // rather than one template at a time, plus the count and the wording.

  it("states how many dispatches ran, whatever the split would have been", () => {
    for (const [runs, cloudRuns, unknownRuns] of [
      [4, 0, 0],
      [3, 3, 0],
      [5, 2, 0],
      [5, 2, 2],
      [3, 0, 2],
      [2, 2, 2],
    ]) {
      const note = hybridNote([], { ...ZERO_TOKENS, runs, cloudRuns, unknownRuns });
      expect(note.text).toBe(`${runs} dispatches done. The fleet is humming, keep it up.`);
    }
  });

  it("singular dispatch wording at exactly one run", () => {
    const note = hybridNote([], { ...ZERO_TOKENS, runs: 1, cloudRuns: 0 });
    expect(note.text).toBe("1 dispatch done. The fleet is humming, keep it up.");
  });

  // (#2637, preserved through #2834) The note must never tell an operator
  // where their work ran or what it cost. It could not do so honestly — the
  // split it used keyed on endpoint presence, so a local inference server on
  // 127.0.0.1 was reported back as cloud. Asserted over the whole input
  // space, so no future template can reintroduce the claim in a branch a
  // single-case test does not reach.
  it("(#2637) never claims local, cloud, or free — for any combination of counts", () => {
    for (let runs = 0; runs <= 4; runs++) {
      for (let cloudRuns = 0; cloudRuns <= 4; cloudRuns++) {
        for (const unknownRuns of [0, 1, 5]) {
          const note = hybridNote([], { ...ZERO_TOKENS, runs, cloudRuns, unknownRuns });
          const t = note.text.toLowerCase();
          for (const claim of ["local", "cloud", "free", "off the meter", "unattributed"]) {
            expect(t, `runs=${runs} cloud=${cloudRuns} unknown=${unknownRuns}`).not.toContain(claim);
          }
        }
      }
    }
  });

  // The clamp that kept a hostile `TokensOffMeter` from rendering
  // "-1 dispatches" is gone with the subtraction that needed it, so the
  // property is asserted directly instead: no negative count, ever.
  it("(post-review) never renders a negative dispatch count", () => {
    for (const [runs, cloudRuns, unknownRuns] of [
      [2, 2, 1],
      [0, 3, 3],
      [1, 9, 9],
    ]) {
      const note = hybridNote([], { ...ZERO_TOKENS, runs, cloudRuns, unknownRuns });
      expect(note.text).not.toMatch(/-\d/);
    }
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





  // (post-review CONSIDER) The all-unattributed branch is NOT a rare edge
  // case — it's what a fresh window looks like before any dispatch's
  // bookend has posted an endpoint, and it renders on the public demo
  // (first ~2% of the replay). Its copy must match the upbeat register of
  // its two siblings above (a short observation + ", keep it up.") rather
  // than reading as a diagnostic ("no attribution darkmux could confirm").

  // (post-review CONSIDER, PROVEN) `hybridNote` takes its `TokensOffMeter`
  // argument as given — it does not re-derive `unknownRuns` from `data`, so
  // a caller-supplied struct where `unknownRuns` overcounts relative to
  // `runs`/`cloudRuns` must not render a negative dispatch count.
  // `hybridNote([], { runs: 2, cloudRuns: 2, unknownRuns: 1 })` rendered
  // "-1 dispatches local + 2 via cloud." before the `Math.max(0, …)` clamp.
});
