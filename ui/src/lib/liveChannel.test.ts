// (#2928) The live channel, viewer side: samples become records of the
// shapes the derivations already read, merge by the live-over-durable
// precedence, fall back to durable heartbeats when they stop, and never
// reach playback.
import { describe, expect, test, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import type { FlowRecord } from "../types/handwritten";
import { EMPTY_OVERLAY, LiveStore, MAX_LIVE_PER_SESSION, LIVE_SESSION_TTL_MS, liveSampleToRecord, mergeLive, useLiveOverlay } from "./liveChannel";
import { deriveLiveState, currentTokenRate } from "./tokenRate";
import { LIVE_UTILITY_END_ACTION, UTILITY_JOB, UTILITY_START_ACTION, utilityStrip } from "./utilityJobs";

const T0 = Date.UTC(2026, 8, 27, 12, 0, 0);
const SID = "live-sess";
const M = "mach-a";

/** A durable heartbeat as the host tailer writes it (whole-second `ts`). */
function durableBeat(atMs: number, gen: number, vis: number, turn = 1): FlowRecord {
  return {
    ts: new Date(Math.floor(atMs / 1000) * 1000).toISOString().replace(/\.\d+Z$/, "Z"),
    action: "dispatch.turn.heartbeat",
    session_id: SID,
    machine_uid: M,
    payload: { turn_seq: turn, sampled_at_ms: atMs, generated_chars: gen, cumulative_chars: vis },
  } as FlowRecord;
}

function wireModel(atMs: number, gen: number, vis: number, turn = 1): string {
  return JSON.stringify({
    v: 1,
    kind: "model",
    session_id: SID,
    role: "coder",
    model: "work-35b",
    at_ms: atMs,
    cadence_ms: 250,
    fields: { turn_seq: turn, sampled_at_ms: atMs, generated_chars: gen, cumulative_chars: vis },
  });
}

function wireUtility(atMs: number, edge: "start" | "end", jobId: string, extra: Record<string, unknown> = {}): string {
  return JSON.stringify({
    v: 1,
    kind: "utility",
    role: "radio-router",
    model: "u4b",
    at_ms: atMs,
    cadence_ms: 250,
    fields: { event: edge, job: UTILITY_JOB.radio_routing, job_id: jobId, stall_after_seconds: 30, ...extra },
  });
}

const start = (): FlowRecord => ({ ts: new Date(T0 - 60_000).toISOString(), action: "dispatch.start", session_id: SID, machine_uid: M } as FlowRecord);

describe("liveSampleToRecord", () => {
  test("a model sample is a heartbeat record at ms precision, marked live", () => {
    const r = liveSampleToRecord(JSON.parse(wireModel(T0 + 250, 40, 20)))!;
    expect(r.action).toBe("dispatch.turn.heartbeat");
    expect(r.session_id).toBe(SID);
    expect(Date.parse(r.ts)).toBe(T0 + 250);
    expect((r as unknown as { payload: Record<string, unknown> }).payload.sampled_at_ms).toBe(T0 + 250);
    expect(r.live).toBe(true);
  });

  test("utility edges become a start and a live end", () => {
    const s = liveSampleToRecord(JSON.parse(wireUtility(T0, "start", "j1")))!;
    const e = liveSampleToRecord(JSON.parse(wireUtility(T0 + 300, "end", "j1")))!;
    expect(s.action).toBe(UTILITY_START_ACTION);
    expect(e.action).toBe(LIVE_UTILITY_END_ACTION);
    const p = (r: FlowRecord) => (r as unknown as { payload: Record<string, unknown> }).payload;
    expect(p(s).started_at_ms).toBe(T0);
    expect(p(e).ended_at_ms).toBe(T0 + 300);
    expect(p(s).event).toBeUndefined();
  });

  test("anything this build does not understand is dropped", () => {
    expect(liveSampleToRecord({ ...JSON.parse(wireModel(T0, 1, 1)), v: 2 })).toBeNull();
    expect(liveSampleToRecord({ ...JSON.parse(wireModel(T0, 1, 1)), kind: "shell" })).toBeNull();
    expect(liveSampleToRecord({ ...JSON.parse(wireModel(T0, 1, 1)), session_id: undefined })).toBeNull();
    expect(liveSampleToRecord({ ...JSON.parse(wireUtility(T0, "start", "j")), fields: { event: "maybe" } })).toBeNull();
    expect(liveSampleToRecord("nope")).toBeNull();
  });
});

describe("mergeLive: live over durable, durable when live stops", () => {
  const live = [T0 + 2_250, T0 + 2_500, T0 + 2_750].map((at, i) => liveSampleToRecord(JSON.parse(wireModel(at, 100 + i * 10, 100 + i * 10)))!);

  test("nothing live: the durable array itself (no copy, no work)", () => {
    const d = [durableBeat(T0, 1, 1)];
    expect(mergeLive(d, undefined)).toBe(d);
    expect(mergeLive(d, [])).toBe(d);
  });

  test("a durable heartbeat inside the live span is dropped; outside it is kept", () => {
    const d = [durableBeat(T0, 50, 50), durableBeat(T0 + 2_500, 110, 110), durableBeat(T0 + 4_000, 200, 200)];
    const merged = mergeLive(d, live);
    const beats = merged.filter((r) => r.action === "dispatch.turn.heartbeat");
    expect(beats.filter((r) => !(r as { live?: boolean }).live).map((r) => Date.parse(r.ts))).toEqual([T0, T0 + 4_000]);
    expect(beats.filter((r) => (r as { live?: boolean }).live)).toHaveLength(3);
  });

  test("the feed stopping falls back to durable heartbeats by itself", () => {
    // Live samples ended at +2.75 s; durable beats kept arriving. The rate
    // reads the newest pair, which is now durable.
    const d = [start(), durableBeat(T0 + 6_000, 600, 600), durableBeat(T0 + 8_000, 1_000, 1_000)];
    const reading = currentTokenRate(mergeLive(d, live));
    expect(reading?.atMs).toBe(T0 + 8_000);
  });

  test("a durable utility edge wins over its live copy; a live edge alone is kept", () => {
    const liveStart = liveSampleToRecord(JSON.parse(wireUtility(T0, "start", "a")))!;
    const liveEnd = liveSampleToRecord(JSON.parse(wireUtility(T0 + 300, "end", "a")))!;
    const durableStart = { ...liveStart, live: undefined } as FlowRecord;
    const merged = mergeLive([durableStart], [liveStart, liveEnd]);
    expect(merged.filter((r) => r.action === UTILITY_START_ACTION)).toHaveLength(1);
    expect(merged.filter((r) => r.action === LIVE_UTILITY_END_ACTION)).toHaveLength(1);
  });
});

describe("the issue's defect: a short think burst between 2 s heartbeats", () => {
  // Visible text for 1 s, a 500 ms think burst, then text again. The durable
  // heartbeats at 0 and 2 s straddle the burst and read GEN; the live
  // samples read THINK while it happens.
  const durable = [start(), durableBeat(T0, 100, 100), durableBeat(T0 + 2_000, 400, 300)];

  test("durable alone cannot see the burst", () => {
    const at = T0 + 1_250;
    const s = deriveLiveState(durable.filter((r) => Date.parse(r.ts) <= at), at);
    expect(s.state).toBe("generating");
    expect(s.thinking).toBeFalsy();
  });

  test("the live samples show it as it happens", () => {
    const store = new LiveStore();
    store.ingest(wireModel(T0 + 1_000, 200, 200), T0 + 1_000);
    store.ingest(wireModel(T0 + 1_250, 260, 200), T0 + 1_250); // reasoning: the total grew, the text did not
    const at = T0 + 1_250;
    const merged = mergeLive(durable.filter((r) => Date.parse(r.ts) <= at), store.snapshot().bySession.get(SID));
    const s = deriveLiveState(merged, at);
    expect(s.state).toBe("generating");
    expect(s.thinking).toBe(true);
    // The transition back is shown at once too.
    store.ingest(wireModel(T0 + 1_500, 300, 240), T0 + 1_500);
    const back = deriveLiveState(mergeLive(durable.filter((r) => Date.parse(r.ts) <= T0 + 1_500), store.snapshot().bySession.get(SID)), T0 + 1_500);
    expect(back.thinking).toBeFalsy();
  });
});

describe("the utility glyph shows a sub-second job live", () => {
  // The durable stream delivers a 300 ms routing job's start and end
  // together, after it has ended: it is never drawn open.
  const specsBinding = { id: "u4b", loaded: true };

  test("live start: open for the job's 300 ms; live end: closed", () => {
    const store = new LiveStore();
    store.ingest(wireUtility(T0, "start", "r1"), T0);
    const during = utilityStrip([], M, T0 + 100, specsBinding, store.snapshot().utility);
    expect(during.job?.visual).toBe("radio");
    expect(during.job?.sinceMs).toBe(T0);
    store.ingest(wireUtility(T0 + 300, "end", "r1"), T0 + 300);
    expect(utilityStrip([], M, T0 + 350, specsBinding, store.snapshot().utility).job).toBeNull();
  });

  test("the durable copies arriving later neither reopen it nor close a newer job", () => {
    const store = new LiveStore();
    store.ingest(wireUtility(T0, "start", "r1"), T0);
    store.ingest(wireUtility(T0 + 300, "end", "r1"), T0 + 300);
    store.ingest(wireUtility(T0 + 500, "start", "r2"), T0 + 500);
    const durable = [
      { ts: new Date(T0).toISOString(), action: UTILITY_START_ACTION, machine_uid: M, payload: { job: UTILITY_JOB.radio_routing, job_id: "r1", started_at_ms: T0, stall_after_seconds: 30 } },
      { ts: new Date(T0).toISOString(), action: "telemetry.tokens", machine_uid: M, payload: { purpose: "utility", job: UTILITY_JOB.radio_routing, job_id: "r1", ended_at_ms: T0 + 300 } },
    ] as FlowRecord[];
    const strip = utilityStrip(durable, M, T0 + 600, specsBinding, store.snapshot().utility);
    expect(strip.job?.sinceMs).toBe(T0 + 500);
  });

  test("no overlay (playback, a peer machine): the strip reads durable records only", () => {
    const store = new LiveStore();
    store.ingest(wireUtility(T0, "start", "r1"), T0);
    expect(utilityStrip([], M, T0 + 100, specsBinding).job).toBeNull();
  });
});

describe("LiveStore", () => {
  test("keeps the newest samples per execution and forgets a quiet one", () => {
    const store = new LiveStore();
    for (let i = 0; i < MAX_LIVE_PER_SESSION + 10; i++) store.ingest(wireModel(T0 + i * 250, i, i), T0 + i * 250);
    const list = store.snapshot().bySession.get(SID)!;
    expect(list).toHaveLength(MAX_LIVE_PER_SESSION);
    expect(Date.parse(list[list.length - 1].ts)).toBe(T0 + (MAX_LIVE_PER_SESSION + 9) * 250);
    const later = T0 + (MAX_LIVE_PER_SESSION + 9) * 250 + LIVE_SESSION_TTL_MS + 1;
    store.ingest(wireUtility(later, "start", "r9"), later);
    expect(store.snapshot().bySession.has(SID)).toBe(false);
  });

  test("junk is ignored and publishes nothing", () => {
    const store = new LiveStore();
    const before = store.snapshot();
    expect(store.ingest("{not json")).toBe(false);
    expect(store.ingest(JSON.stringify({ v: 1, kind: "shell", at_ms: 1, cadence_ms: 1, fields: {} }))).toBe(false);
    expect(store.snapshot()).toBe(before);
  });
});

describe("useLiveOverlay", () => {
  test("disabled (playback, scrubbed, static): always the empty overlay, whatever arrives", () => {
    const store = new LiveStore();
    store.ingest(wireModel(T0, 1, 1), T0);
    const { result } = renderHook(() => useLiveOverlay(false, store));
    expect(result.current).toBe(EMPTY_OVERLAY);
    act(() => {
      store.ingest(wireModel(T0 + 250, 2, 2), T0 + 250);
    });
    expect(result.current).toBe(EMPTY_OVERLAY);
  });

  test("enabled: re-renders with each sample", () => {
    const store = new LiveStore();
    const { result } = renderHook(() => useLiveOverlay(true, store));
    expect(result.current.bySession.size).toBe(0);
    act(() => {
      store.ingest(wireModel(T0, 1, 1), T0);
    });
    expect(result.current.bySession.get(SID)).toHaveLength(1);
  });
});

describe("LiveStore render pacing", () => {
  test("at most one notification per cadence; every sample still lands in the snapshot", () => {
    vi.useFakeTimers();
    vi.setSystemTime(T0);
    const store = new LiveStore();
    let notified = 0;
    store.subscribe(() => notified++);
    // Three executions' samples inside one 250 ms window.
    for (const [sid, gen] of [["a", 1], ["b", 2], ["c", 3]] as const) {
      store.ingest(JSON.stringify({ v: 1, kind: "model", session_id: sid, at_ms: T0, cadence_ms: 250, fields: { generated_chars: gen } }), T0);
    }
    expect(notified, "the leading edge is immediate").toBe(1);
    expect(store.snapshot().bySession.size, "nothing is dropped or delayed in the data").toBe(3);
    vi.advanceTimersByTime(249);
    expect(notified).toBe(1);
    vi.advanceTimersByTime(1);
    expect(notified, "the trailing edge carries the rest").toBe(2);
    vi.advanceTimersByTime(1000);
    expect(notified, "and nothing more without new samples").toBe(2);
    vi.useRealTimers();
  });
});
