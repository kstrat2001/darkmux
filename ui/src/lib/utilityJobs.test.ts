import { describe, expect, test } from "vitest";
import { ACTION, type NormRecord } from "./ingest";
import { norm, type RawRecord } from "../testing/records";
import {
  UTILITY_JOB,
  UTILITY_JOB_DEFAULT_STALL_MS,
  isKnownUtilityJob,
  machineUtilityJob,
  openUtilityJobs,
  utilityJobVisual,
  utilityUsageByJob,
} from "./utilityJobs";

const M = "mach-a";
const at = (s: number) => new Date(Date.UTC(2026, 8, 27, 12, 0, s)).toISOString();
const ms = (s: number) => Date.parse(at(s));

function start(s: number, job: string, extra: RawRecord = {}, payload: Record<string, unknown> = {}): NormRecord {
  return norm({
    ts: at(s),
    action: ACTION.UtilityStart,
    category: "telemetry",
    source: "utility",
    machine_uid: M,
    model: "u4b",
    payload: { job, model: "u4b", stall_after_seconds: 30, ...payload },
    ...extra,
  });
}
function usage(s: number, job: string | undefined, extra: RawRecord = {}, payload: Record<string, unknown> = {}): NormRecord {
  return norm({
    ts: at(s),
    action: "telemetry.tokens",
    category: "telemetry",
    source: "tokens",
    machine_uid: M,
    payload: { purpose: "utility", call_kind: "single_shot", ...(job ? { job } : {}), total_tokens: 10, requested_model: "u4b", ...payload },
    ...extra,
  });
}
function err(s: number, job: string, extra: RawRecord = {}): NormRecord {
  return norm({ ts: at(s), action: ACTION.UtilityError, category: "telemetry", source: "utility", machine_uid: M, payload: { job, model: "u4b" }, ...extra });
}

describe("the one definition of the utility jobs", () => {
  test("every generated variant is a known job, and nothing else is", () => {
    for (const j of Object.values(UTILITY_JOB)) expect(isKnownUtilityJob(j)).toBe(true);
    expect(isKnownUtilityJob("dream_job")).toBe(false);
    expect(isKnownUtilityJob(undefined)).toBe(false);
  });

  test("each known job has its own visual; an unknown job gets the generic one, never none", () => {
    expect(utilityJobVisual(UTILITY_JOB.radio_routing)).toBe("radio");
    expect(utilityJobVisual(UTILITY_JOB.compaction)).toBe("compacting");
    expect(utilityJobVisual("dream_job")).toBe("generic");
  });
});

describe("machineUtilityJob: the machine's live utility job", () => {
  test("a routing start with no end yet is the live job, counting from its start", () => {
    const live = machineUtilityJob([start(0, UTILITY_JOB.radio_routing)], ms(5));
    expect(live).toMatchObject({ job: UTILITY_JOB.radio_routing, known: true, sinceMs: ms(0), stalled: false, model: "u4b" });
  });

  test("its usage record ends it: quiet", () => {
    expect(machineUtilityJob([start(0, UTILITY_JOB.radio_routing), usage(2, UTILITY_JOB.radio_routing)], ms(5))).toBeNull();
  });

  test("a utility.error ends it too", () => {
    expect(machineUtilityJob([start(0, UTILITY_JOB.radio_routing), err(2, UTILITY_JOB.radio_routing)], ms(5))).toBeNull();
  });

  test("an end for a DIFFERENT job does not end it", () => {
    const recs = [start(0, UTILITY_JOB.radio_routing), usage(2, UTILITY_JOB.compaction, { session_id: "s1" }, { call_kind: "compaction" })];
    expect(machineUtilityJob(recs, ms(5))?.job).toBe(UTILITY_JOB.radio_routing);
  });

  test("an end for a different job in the SAME scope (both sessionless) does not end it", () => {
    expect(machineUtilityJob([start(0, UTILITY_JOB.radio_routing), usage(2, "dream_job")], ms(5))?.job).toBe(UTILITY_JOB.radio_routing);
  });

  test("fast transitions are shown as they happen: a start and end in the same second read quiet after, busy between", () => {
    const recs = [start(0, UTILITY_JOB.radio_routing), usage(0, UTILITY_JOB.radio_routing)];
    expect(machineUtilityJob(recs, ms(0))).toBeNull();
    expect(machineUtilityJob([recs[0]], ms(0))?.job).toBe(UTILITY_JOB.radio_routing);
  });

  test("a start with no end past its own bound reads STALL", () => {
    const live = machineUtilityJob([start(0, UTILITY_JOB.radio_routing)], ms(31));
    expect(live?.stalled).toBe(true);
    expect(machineUtilityJob([start(0, UTILITY_JOB.radio_routing)], ms(29))?.stalled).toBe(false);
  });

  test("a start without a bound takes the default inactivity window", () => {
    const r = start(0, UTILITY_JOB.radio_routing);
    delete (r.payload as Record<string, unknown>).stall_after_seconds;
    expect(machineUtilityJob([r], ms(0) + UTILITY_JOB_DEFAULT_STALL_MS - 1)?.stalled).toBe(false);
    expect(machineUtilityJob([r], ms(0) + UTILITY_JOB_DEFAULT_STALL_MS + 1)?.stalled).toBe(true);
  });

  test("a compaction start is ended by its execution's own usage record, and by its execution's next turn record when no call returned", () => {
    const s = start(0, UTILITY_JOB.compaction, { session_id: "s1" }, { serves: "s1" });
    expect(machineUtilityJob([s], ms(3))?.job).toBe(UTILITY_JOB.compaction);
    expect(machineUtilityJob([s, usage(2, UTILITY_JOB.compaction, { session_id: "s1" }, { call_kind: "compaction" })], ms(3))).toBeNull();
    const heartbeat = norm({ ts: at(2), action: "dispatch.turn.heartbeat", session_id: "s1", machine_uid: M, payload: {} });
    expect(machineUtilityJob([s, heartbeat], ms(3))).toBeNull();
    // Another execution's record does not end it.
    const other = { ...heartbeat, session_id: "s2" };
    expect(machineUtilityJob([s, other], ms(3))?.job).toBe(UTILITY_JOB.compaction);
  });

  test("a compaction usage record from before 1.61.0 (no `job`) still ends a compaction start", () => {
    const s = start(0, UTILITY_JOB.compaction, { session_id: "s1" }, { serves: "s1" });
    expect(machineUtilityJob([s, usage(2, undefined, { session_id: "s1" }, { call_kind: "compaction" })], ms(3))).toBeNull();
  });

  test("an unknown job (a newer darkmux's) is live and marked unknown, never dropped", () => {
    const live = machineUtilityJob([start(0, "dream_job")], ms(1));
    expect(live).toMatchObject({ job: "dream_job", known: false });
    expect(machineUtilityJob([start(0, "dream_job"), usage(1, "dream_job")], ms(2))).toBeNull();
  });

  test("records after `nowMs` are not read (playback)", () => {
    const recs = [start(0, UTILITY_JOB.radio_routing), usage(10, UTILITY_JOB.radio_routing)];
    expect(machineUtilityJob(recs, ms(5))?.job).toBe(UTILITY_JOB.radio_routing);
  });

  test("the latest open job wins when two overlap", () => {
    const recs = [start(0, UTILITY_JOB.compaction, { session_id: "s1" }, { serves: "s1" }), start(3, UTILITY_JOB.radio_routing)];
    expect(machineUtilityJob(recs, ms(4))?.job).toBe(UTILITY_JOB.radio_routing);
  });
});

describe("utilityUsageByJob: each job's recent usage", () => {
  test("counts calls and tokens per job, known jobs listed even at zero, unknown ones appended", () => {
    const rows = utilityUsageByJob([
      usage(1, UTILITY_JOB.radio_routing),
      usage(2, UTILITY_JOB.radio_routing, {}, { total_tokens: 5 }),
      usage(3, undefined, { session_id: "s" }, { call_kind: "compaction", total_tokens: 100 }),
      usage(4, "dream_job", {}, { total_tokens: 7 }),
      norm({ ...usage(5, UTILITY_JOB.radio_routing), payload: { purpose: "work", call_kind: "turn", total_tokens: 999 } }),
    ]);
    expect(rows).toEqual([
      { job: UTILITY_JOB.compaction, known: true, calls: 1, tokens: 100 },
      { job: UTILITY_JOB.radio_routing, known: true, calls: 2, tokens: 15 },
      { job: "dream_job", known: false, calls: 1, tokens: 7 },
    ]);
  });

  test("a utility record that names no job and is not a compaction is counted under an unnamed job, not lost", () => {
    const rows = utilityUsageByJob([usage(1, undefined)]);
    expect(rows.find((r) => !r.known)).toMatchObject({ job: null, calls: 1, tokens: 10 });
  });
});

// (#2915 review) Pairing by job id, orphans, terminals, ms times.
describe("(#2915 review) machineUtilityJob pairing", () => {
  const withId = (r: NormRecord, id: string, ms?: Record<string, number>): NormRecord =>
    norm({ ...r, payload: { ...(r.payload as Record<string, unknown>), job_id: id, ...(ms ?? {}) } });

  test("MUST 1: an orphaned routing start never absorbs a later job's end: [orphan, start, end] -> quiet", () => {
    const recs = [
      withId(start(0, UTILITY_JOB.radio_routing), "a1"),
      withId(start(10, UTILITY_JOB.radio_routing), "b1"),
      withId(usage(11, UTILITY_JOB.radio_routing), "b1"),
    ];
    expect(machineUtilityJob(recs, ms(12))).toBeNull();
    // ...and stays quiet long after, where the orphan would have read stalled.
    expect(machineUtilityJob(recs, ms(12) + 3_600_000)).toBeNull();
  });

  test("an end closes ITS start by id, not the oldest or newest open one", () => {
    const recs = [
      withId(start(0, UTILITY_JOB.radio_routing), "a1"),
      withId(start(1, UTILITY_JOB.radio_routing), "b1"),
      withId(usage(2, UTILITY_JOB.radio_routing), "a1"),
    ];
    expect(machineUtilityJob(recs, ms(3))).toMatchObject({ job: UTILITY_JOB.radio_routing, sinceMs: ms(1) });
  });

  test("records without ids (older writers): an end closes the most recent open start and drops older same-kind ones", () => {
    const recs = [start(0, UTILITY_JOB.radio_routing), start(10, UTILITY_JOB.radio_routing), usage(11, UTILITY_JOB.radio_routing)];
    expect(machineUtilityJob(recs, ms(12))).toBeNull();
  });

  test("MUST 2: a compaction whose runtime was killed is closed by its execution's dispatch error", () => {
    const s = start(0, UTILITY_JOB.compaction, { session_id: "s1" }, { serves: "s1" });
    const errRec = norm({ ts: at(4), action: "dispatch.error", session_id: "s1", machine_uid: M, payload: {} });
    expect(machineUtilityJob([s, errRec], ms(5))).toBeNull();
    const done = norm({ ...errRec, action: "dispatch.complete" });
    expect(machineUtilityJob([s, done], ms(5))).toBeNull();
  });

  test("C3: a terminal in the SAME second as the start still closes it", () => {
    const s = withId(start(4, UTILITY_JOB.compaction, { session_id: "s1" }, { serves: "s1" }), "s1:compaction:1", { started_at_ms: ms(4) + 400 });
    const errRec = norm({ ts: at(4), action: "dispatch.error", session_id: "s1", machine_uid: M, payload: {} });
    expect(machineUtilityJob([s, errRec], ms(5))).toBeNull();
  });

  test("a heartbeat is judged by its own sample time: one sampled before the start leaves it open, one after closes it", () => {
    const s = withId(start(4, UTILITY_JOB.compaction, { session_id: "s1" }, { serves: "s1" }), "s1:compaction:1", { started_at_ms: ms(4) + 400 });
    const beat = (sampled: number) => norm({ ts: at(4), action: "dispatch.turn.heartbeat", session_id: "s1", machine_uid: M, payload: { sampled_at_ms: sampled } });
    expect(machineUtilityJob([beat(ms(4) + 100), s], ms(5))?.job).toBe(UTILITY_JOB.compaction);
    expect(machineUtilityJob([s, beat(ms(4) + 700)], ms(5))).toBeNull();
  });

  test("C4: a sub-second job (start and end in one whole second) is open between its ms times and quiet after", () => {
    const recs = [
      withId(start(8, UTILITY_JOB.radio_routing), "r1", { started_at_ms: ms(8) + 100 }),
      withId(usage(8, UTILITY_JOB.radio_routing), "r1", { ended_at_ms: ms(8) + 600 }),
    ];
    expect(machineUtilityJob(recs, ms(8) + 300)?.job).toBe(UTILITY_JOB.radio_routing);
    expect(machineUtilityJob(recs, ms(8) + 300)?.sinceMs).toBe(ms(8) + 100);
    expect(machineUtilityJob(recs, ms(8) + 700)).toBeNull();
  });
});

describe("a utility record with no usable time (the bad-timestamp policy)", () => {
  const NOW = Date.parse("2026-09-27T10:00:00Z");
  const start = (ts: string) => norm({ ts, action: ACTION.UtilityStart, session_id: "s1", payload: { job: "compaction", job_id: "j1" } });
  const end = (ts: string) => norm({ ts, action: ACTION.UtilityError, session_id: "s1", payload: { job: "compaction", job_id: "j1" } });

  test("an untimed start is kept, open as of now rather than dropped", () => {
    const open = openUtilityJobs([start("garbage")], NOW);
    expect(open.map((o) => [o.job, o.sinceMs])).toEqual([["compaction", NOW]]);
  });

  test("an untimed end still closes its job", () => {
    expect(openUtilityJobs([start("2026-09-27T09:59:00Z"), end("garbage")], NOW)).toEqual([]);
  });
});
