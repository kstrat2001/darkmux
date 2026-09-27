// (#2928) The live channel, viewer side: samples become records of the
// shapes the derivations already read, merge by the live-over-durable
// precedence, fall back to durable heartbeats when they stop, and never
// reach playback.
import { describe, expect, test, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import type { FlowRecord } from "../types/handwritten";
import { EMPTY_OVERLAY, LiveStore, MAX_LIVE_PER_SESSION, LIVE_SESSION_TTL_MS, MAX_TRANSIENT_FRAMES_PER_SESSION, MAX_TRANSIENT_TRAIN, TRANSIENT_FRAME_MS, liveSampleToRecord, mergeLive, useLiveOverlay } from "./liveChannel";
import { deriveLiveState, currentTokenRate, heartbeatSamples } from "./tokenRate";
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
    // The transition back is shown too (after the think state's own frame).
    vi.useFakeTimers();
    store.ingest(wireModel(T0 + 1_500, 300, 240), T0 + 1_500);
    vi.advanceTimersByTime(1_000);
    vi.useRealTimers();
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

describe("(#2928 review, C1) a hole in the live feed does not erase durable heartbeats", () => {
  test("durable beats inside a 20 s hole between two live stretches are kept", () => {
    const before = [T0, T0 + 250, T0 + 500].map((at, i) => liveSampleToRecord(JSON.parse(wireModel(at, 10 + i, 10 + i)))!);
    const after = [T0 + 20_500, T0 + 20_750].map((at, i) => liveSampleToRecord(JSON.parse(wireModel(at, 900 + i, 900 + i)))!);
    const d = [durableBeat(T0 + 250, 11, 11), durableBeat(T0 + 6_000, 300, 300), durableBeat(T0 + 12_000, 600, 600), durableBeat(T0 + 20_600, 901, 901)];
    const merged = mergeLive(d, [...before, ...after]);
    const kept = merged.filter((r) => r.action === "dispatch.turn.heartbeat" && !(r as { live?: boolean }).live).map((r) => (r as unknown as { payload: { sampled_at_ms: number } }).payload.sampled_at_ms);
    expect(kept).toEqual([T0 + 6_000, T0 + 12_000]);
  });
});

describe("(#2928 review, C2) a state that came and went between renders is drawn for a frame", () => {
  test("a 60 ms think burst whose edges arrive inside one pacing window lights THINK once", () => {
    vi.useFakeTimers();
    vi.setSystemTime(T0);
    const store = new LiveStore();
    const durable = [start()];
    const seen: boolean[] = [];
    store.subscribe(() => {
      const s = deriveLiveState(mergeLive(durable, store.snapshot().bySession.get(SID)), T0 + 1_100);
      seen.push(s.thinking === true);
    });
    store.ingest(wireModel(T0 + 1_000, 100, 100), T0); // rendered (leading edge)
    // All inside one 250 ms window: last visible, first think, last think, first visible.
    store.ingest(wireModel(T0 + 1_020, 120, 120), T0);
    store.ingest(wireModel(T0 + 1_040, 140, 120), T0);
    store.ingest(wireModel(T0 + 1_080, 180, 120), T0);
    store.ingest(wireModel(T0 + 1_100, 200, 140), T0);
    // The frame stays up for a moment even though newer data has arrived.
    const up = deriveLiveState(mergeLive(durable, store.snapshot().bySession.get(SID)), T0 + 1_100);
    expect(up.thinking, "the think frame is what is drawn right now").toBe(true);
    vi.advanceTimersByTime(1_000);
    expect(deriveLiveState(mergeLive(durable, store.snapshot().bySession.get(SID)), T0 + 1_100).thinking).toBeFalsy();
    expect(seen.some(Boolean), `renders: ${seen}`).toBe(true);
    expect(seen[seen.length - 1], "the final state is visible text").toBe(false);
    vi.useRealTimers();
  });

  test("a sub-second utility job whose start and end land in one window is drawn open once", () => {
    vi.useFakeTimers();
    vi.setSystemTime(T0);
    const store = new LiveStore();
    const open: boolean[] = [];
    store.subscribe(() => open.push(utilityStrip([], M, T0 + 100, { id: "u4b", loaded: true }, store.snapshot().utility).job !== null));
    store.ingest(wireModel(T0, 1, 1), T0); // leading edge taken by something else
    store.ingest(wireUtility(T0 + 10, "start", "fast"), T0);
    store.ingest(wireUtility(T0 + 60, "end", "fast"), T0);
    vi.advanceTimersByTime(1_000);
    expect(open.some(Boolean), `renders: ${open}`).toBe(true);
    expect(open[open.length - 1]).toBe(false);
    vi.useRealTimers();
  });
});

describe("(#2928 review, C9) idle entries leave on a timer", () => {
  test("a session with no new samples is pruned without any new sample arriving", () => {
    vi.useFakeTimers();
    vi.setSystemTime(T0);
    const store = new LiveStore();
    let notified = 0;
    store.subscribe(() => notified++);
    store.ingest(wireModel(T0, 1, 1), T0);
    expect(store.snapshot().bySession.has(SID)).toBe(true);
    const before = notified;
    vi.advanceTimersByTime(LIVE_SESSION_TTL_MS + 10_000);
    expect(store.snapshot().bySession.has(SID)).toBe(false);
    expect(notified, "subscribers are told").toBeGreaterThan(before);
    vi.useRealTimers();
  });
});

describe("(#2928 re-review, MF-A) a host refresh never enters rate math, only freshness", () => {
  // The opener at T, refreshed by the host at +250/+500/+750 (its own
  // `sampled_at_ms` kept, `refreshed_at_ms` the host's clock), then the
  // first real chunk, runtime-stamped EARLIER than the last refresh.
  const opener = (at: number) =>
    JSON.stringify({ v: 1, kind: "model", session_id: SID, at_ms: at, cadence_ms: 250, fields: { turn_seq: 2, sampled_at_ms: T0, generated_chars: 0, cumulative_chars: 0, prompt_chars: 9_000, ...(at > T0 ? { refreshed_at_ms: at } : {}) } });
  const recs = () => {
    const store = new LiveStore();
    for (const at of [T0, T0 + 250, T0 + 500, T0 + 750]) store.ingest(opener(at), at);
    store.ingest(wireModel(T0 + 700, 120, 120, 2), T0 + 760);
    store.ingest(wireModel(T0 + 1_200, 240, 240, 2), T0 + 1_210);
    return mergeLive([start()], store.snapshot().bySession.get(SID));
  };

  test("the first chunk pairs with the real opener, not a refresh: no spike", () => {
    const withRefreshes = currentTokenRate(recs());
    // Without the refreshes: the same two real samples decide.
    const store = new LiveStore();
    store.ingest(opener(T0), T0);
    store.ingest(wireModel(T0 + 700, 120, 120, 2), T0 + 760);
    store.ingest(wireModel(T0 + 1_200, 240, 240, 2), T0 + 1_210);
    const without = currentTokenRate(mergeLive([start()], store.snapshot().bySession.get(SID)));
    expect(withRefreshes?.tokensPerSec).toBeCloseTo(without!.tokensPerSec, 6);
    expect(withRefreshes!.tokensPerSec).toBeLessThan(100);
  });

  test("a refresh is never a sample of its own", () => {
    const samples = heartbeatSamples(recs());
    expect(samples.map((x) => x.chars)).toEqual([0, 120, 240]);
    expect(samples[0].freshMs, "the opener's state held until the last refresh").toBe(T0 + 750);
  });

  test("generation started: GEN, never PROMPT from a later-stamped refresh", () => {
    expect(deriveLiveState(recs(), T0 + 1_300).state).toBe("generating");
  });

  test("a long prompt kept fresh by refreshes is PROMPT, not STALL", () => {
    const store = new LiveStore();
    for (let at = T0; at <= T0 + 40_000; at += 250) store.ingest(opener(at), at);
    const held = store.snapshot().bySession.get(SID)!;
    expect(held, "the opener and ONE latest refresh, not 160 refreshes").toHaveLength(2);
    const merged = mergeLive([start()], held);
    expect(heartbeatSamples(merged)[0]?.freshMs).toBe(T0 + 40_000);
    const s = deriveLiveState(merged, T0 + 40_100);
    expect(s.state).toBe("prompt");
  });
});

describe("(#2928 re-review, C-2) transient frames are bounded per session, counted, and a train ends", () => {
  const flap = (store: LiveStore, sid: string, n: number, base: number) => {
    let gen = 0;
    let vis = 0;
    for (let i = 0; i < n; i++) {
      gen += 10;
      if (i % 2 === 0) vis += 10;
      store.ingest(JSON.stringify({ v: 1, kind: "model", session_id: sid, at_ms: base + i, cadence_ms: 250, fields: { turn_seq: 1, sampled_at_ms: base + i, generated_chars: gen, cumulative_chars: vis } }), T0);
    }
  };

  test("the cap is per session, and what it drops is counted", () => {
    vi.useFakeTimers();
    vi.setSystemTime(T0);
    const store = new LiveStore();
    store.ingest(wireModel(T0, 1, 1), T0); // takes the leading edge
    flap(store, "a", 12, T0 + 10);
    flap(store, "b", 3, T0 + 10);
    const st = store.debugStats();
    expect(st.framesKept, "session b's frames are not crowded out by a").toBeGreaterThanOrEqual(MAX_TRANSIENT_FRAMES_PER_SESSION + 1);
    expect(st.framesDropped).toBeGreaterThan(0);
    vi.useRealTimers();
  });

  test("sustained flapping never keeps a frame train running: the latest state is drawn", () => {
    vi.useFakeTimers();
    vi.setSystemTime(T0);
    const store = new LiveStore();
    let latestShown = 0;
    store.subscribe(() => {
      if (store.snapshot() === store.latestForTest()) latestShown++;
    });
    store.ingest(wireModel(T0, 1, 1), T0);
    for (let k = 0; k < 40; k++) {
      flap(store, "a", 4, T0 + 100 + k * 10);
      vi.advanceTimersByTime(TRANSIENT_FRAME_MS);
    }
    expect(latestShown, "the latest state was drawn between trains, while flapping went on").toBeGreaterThan(1);
    vi.advanceTimersByTime(5_000);
    expect(store.snapshot()).toBe(store.latestForTest());
    expect(store.debugStats().longestTrain).toBeLessThanOrEqual(MAX_TRANSIENT_TRAIN);
    vi.useRealTimers();
  });
});
