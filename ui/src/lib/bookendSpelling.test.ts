/**
 * (#2927) Both producer spellings of the dispatch bookends — `dispatch start`
 * (crew, CLI) and `dispatch.start` (lab, runtime) — mean the same thing to
 * every consumer (#1852). The app's ingest paths (`normalizeRecords`,
 * `buildFlowWindow`) dot them before any lens sees them, so in the running
 * viewer these consumers get the dotted form; reading both here is defense in
 * depth for a consumer handed RAW records (unit fixtures, the mission graph's
 * stream). Each consumer is fed the spaced spelling verbatim and must read it
 * exactly as it reads the dotted one.
 */
import { describe, expect, it } from "vitest";
import type { FlowRecord } from "../types/handwritten";
import { dispatchEnd, dispatchErrored, dispatchRec, flowLiveSessions } from "./flow";
import { machActive } from "../lenses/fleet/cards";
import { readyParts } from "./metaLine";
import { deriveLiveState, executionRole, liveExecutions } from "./tokenRate";
import { runRegions } from "../lenses/session/sessionRun";
import { turnItems } from "./turnGroups";
import { recordDetail } from "./recordDetail";
import { activityOf } from "./eventFilters";

type Spelling = "spaced" | "dotted";
const SPELLINGS: Spelling[] = ["spaced", "dotted"];
const act = (s: Spelling, verb: "start" | "complete" | "error") => (s === "spaced" ? `dispatch ${verb}` : `dispatch.${verb}`);

const T0 = Date.parse("2026-09-26T10:00:00Z");
const at = (sec: number) => new Date(T0 + sec * 1000).toISOString();

function rec(action: string, sec: number, extra: Record<string, unknown> = {}): FlowRecord {
  return {
    ts: at(sec),
    action,
    session_id: "s1",
    machine_uid: "u1",
    machine_id: "box",
    handle: "coder",
    payload: {},
    ...extra,
  } as unknown as FlowRecord;
}

const openRun = (s: Spelling) => [rec(act(s, "start"), 0, { payload: { prompt_chars: 12 } }), rec("dispatch.turn", 5, { payload: { turn_seq: 1 } })];
const closedRun = (s: Spelling, end: "complete" | "error" = "complete") => [
  ...openRun(s),
  rec(act(s, end), 10, end === "error" ? { payload: { exit_code: 1 } } : {}),
];

describe("(#2927) dispatch bookends read the same in either spelling", () => {
  for (const s of SPELLINGS) {
    describe(s, () => {
      it("fleet card: an open run is active (machActive)", () => {
        const data = openRun(s);
        expect(machActive(data, new Set(["s1"]), "u1", T0 + 6_000)).toBe(true);
      });

      it("flow liveness: an open run is live, a closed one is not", () => {
        expect([...flowLiveSessions(openRun(s), T0 + 6_000)]).toEqual(["s1"]);
        expect([...flowLiveSessions(closedRun(s), T0 + 11_000)]).toEqual([]);
      });

      it("dispatchRec / dispatchEnd / dispatchErrored find the bookends", () => {
        expect(dispatchRec(closedRun(s), "s1", "start")?.action).toBe(act(s, "start"));
        expect(dispatchEnd(closedRun(s), "s1")?.action).toBe(act(s, "complete"));
        const errored = closedRun(s, "error");
        expect(dispatchEnd(errored, "s1")?.action).toBe(act(s, "error"));
        expect(dispatchErrored(dispatchEnd(errored, "s1"))).toBe(true);
      });

      it("meta line: the last dispatch is dated from its start", () => {
        const beat = { machine_id: "box" } as never;
        expect(readyParts(openRun(s), new Map([["u1", beat]]), T0 + 120_000)?.ago).not.toBe("");
      });

      it("token rate: a fresh start reads PROMPT; a closed run is not a live execution; the role comes from the start", () => {
        expect(deriveLiveState([rec(act(s, "start"), 0)], T0 + 1_000).state).toBe("prompt");
        // A new start after a tool-calling turn resets the marker to PROMPT.
        const afterTools = [rec("dispatch.turn", 0, { payload: { turn_seq: 1, tool_calls_count: 2 } }), rec(act(s, "start"), 1)];
        expect(deriveLiveState(afterTools, T0 + 2_000).state).toBe("prompt");
        // A bare start is execution evidence; a mission-sourced (run-grain) one is not an execution.
        expect(liveExecutions([[rec(act(s, "start"), 0)]], T0 + 1_000)).toHaveLength(1);
        expect(liveExecutions([[rec(act(s, "start"), 0, { source: "mission" }), rec("dispatch.turn", 1)]], T0 + 2_000)).toEqual([]);
        expect(liveExecutions([closedRun(s)], T0 + 11_000)).toEqual([]);
        expect(liveExecutions([openRun(s)], T0 + 6_000)).toHaveLength(1);
        const twoRoles = [rec("dispatch.turn", 0, { handle: "early" }), rec(act(s, "start"), 1, { handle: "reviewer" })];
        expect(executionRole(twoRoles)).toBe("reviewer");
      });

      it("event log labels and detail read the spelling", () => {
        expect(activityOf(rec(act(s, "start"), 0))).toBe("dispatch start");
        expect(activityOf(rec(act(s, "complete"), 0))).toBe("dispatch end");
        expect(activityOf(rec(act(s, "error"), 0))).toBe("dispatch error");
        expect(recordDetail(rec(act(s, "start"), 0, { payload: { prompt_chars: 12 } }))).toBe("start (prompt: 12ch)");
      });
    });
  }

  it("run page: the spaced run renders exactly as the dotted one", () => {
    const strip = (v: unknown) => JSON.stringify(v).replaceAll("dispatch start", "dispatch.start").replaceAll("dispatch complete", "dispatch.complete");
    for (const end of ["complete", "error"] as const) {
      const spaced = runRegions(closedRun("spaced", end), "s1", T0 + 11_000);
      const dotted = runRegions(closedRun("dotted", end), "s1", T0 + 11_000);
      expect(strip(spaced)).toBe(strip(dotted));
    }
  });

  // The mission rollup (a run session with no telemetry of its own, reading
  // its inner executions) under either spelling. Utility is decided by the
  // usage record's own `purpose` (and, before `purpose` existed, by
  // `call_kind: "compaction"`), never by a role handle: the compactor's calls
  // ride inside the specialist's own session as sub-executions (contract 8),
  // and must not fold into the run's model numbers.
  it("run page: a compaction sub-execution under either spelling never folds into the run's model numbers", () => {
    const usage = (sid: string, sec: number, handle: string, p: Record<string, unknown>) => {
      const f = { total_tokens: Number(p.prompt_tokens) + Number(p.completion_tokens), ...p };
      return rec("telemetry.tokens", sec, { mission_id: "m1", session_id: sid, handle, category: "telemetry", source: "tokens", payload: f, fields: f });
    };
    const views = SPELLINGS.map((s) => {
      const m = { mission_id: "m1" };
      const runSess = [rec(act(s, "start"), 0, { ...m, session_id: "run" }), rec(act(s, "complete"), 30, { ...m, session_id: "run" })];
      const spec = [
        rec(act(s, "start"), 1, { ...m, session_id: "spec" }),
        usage("spec", 2, "coder", { prompt_tokens: 1000, completion_tokens: 200, call_kind: "turn", purpose: "work" }),
      ];
      const compaction = [usage("spec", 3, "compactor", { prompt_tokens: 50_000, completion_tokens: 9_000, call_kind: "compaction", purpose: "utility" })];
      const at = T0 + 31_000;
      return {
        withUtility: runRegions([...runSess, ...spec, ...compaction], "run", at).metrics,
        without: runRegions([...runSess, ...spec], "run", at).metrics,
        noWork: runRegions(runSess, "run", at).metrics,
      };
    });
    for (const v of views) {
      expect(JSON.stringify(v.withUtility)).toBe(JSON.stringify(v.without));
      // Non-vacuous: the specialist's own tokens DO reach the run.
      expect(JSON.stringify(v.without)).not.toBe(JSON.stringify(v.noWork));
    }
    const strip = (v: unknown) => JSON.stringify(v).replaceAll("dispatch start", "dispatch.start").replaceAll("dispatch complete", "dispatch.complete");
    expect(strip(views[0].withUtility)).toBe(strip(views[1].withUtility));
  });

  it("turn groups: a spaced start opens a new execution like a dotted one", () => {
    const run = (s: Spelling) => [rec(act(s, "start"), 0), rec("dispatch.turn", 1, { payload: { turn_seq: 1 } }), rec(act(s, "start"), 2), rec("dispatch.turn", 3, { payload: { turn_seq: 1 } })];
    const strip = (v: unknown) => JSON.stringify(v).replaceAll("dispatch start", "dispatch.start");
    const items = (s: Spelling) => {
      const rs = run(s);
      return turnItems(rs, rs);
    };
    const spaced = items("spaced");
    expect(strip(spaced)).toBe(strip(items("dotted")));
    expect(spaced.filter((i) => (i as { kind?: string }).kind === "turn")).toHaveLength(2);
  });
});
