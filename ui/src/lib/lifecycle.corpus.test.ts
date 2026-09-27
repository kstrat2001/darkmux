// The lifecycle corpus, run against the viewer's executor. The same file is
// asserted by the daemon's (`runs.rs`'s `lifecycle_corpus`): one spec, two
// executors. A case names one run's records, an instant, and the phase and
// status every surface must state for it then.
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { lifecycleAt, toRunState, type LifecyclePolicy } from "./lifecycle";
import { currentRun, runIndex, sessionRun } from "./runRef";
import type { NormRecord } from "./ingest";
import { normAll, type RawRecord } from "../testing/records";

interface Case {
  name: string;
  run?: { session_id: string; mission_id?: string };
  as_of: string;
  records: RawRecord[];
  presence?: string[];
  phase: string;
  status: string;
  abandoned_reason?: string;
}

const corpus = JSON.parse(readFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../tests/lifecycle/cases.json"), "utf8")) as {
  policy: { stale_after_ms: number; budget_wait_grace_ms: number };
  cases: Case[];
};
const policy: LifecyclePolicy = { staleAfterMs: corpus.policy.stale_after_ms, budgetWaitGraceMs: corpus.policy.budget_wait_grace_ms };

/** The run a case judges: its `run` (a session, and a mission when named),
 *  else the session a `#dispatch=<id>` link names, as that route reads it. */
function judgedRun(data: NormRecord[], c: Case, asOf: number) {
  const sid = c.run?.session_id ?? String(data[0].session_id);
  if (c.run?.mission_id === undefined) return sessionRun(data, sid, asOf)!;
  const g = runIndex(data).groupsOfSession(sid).find((x) => x.missionId === c.run!.mission_id)!;
  return currentRun(g, asOf);
}

describe("lifecycle corpus (tests/lifecycle/cases.json)", () => {
  it("is not empty", () => {
    expect(corpus.cases.length).toBeGreaterThan(20);
  });
  for (const c of corpus.cases) {
    it(c.name, () => {
      const asOf = Date.parse(c.as_of);
      const run = judgedRun(normAll(c.records), c, asOf);
      const l = lifecycleAt(run, asOf, policy, new Set(c.presence ?? []));
      expect(l.phase).toBe(c.phase);
      const state = toRunState(l);
      expect(state.status).toBe(c.status);
      expect(state.abandonReason).toBe(c.abandoned_reason);
    });
  }
});
