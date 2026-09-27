import { describe, expect, it, vi } from "vitest";
import { QueryClient } from "@tanstack/react-query";
import type { FlowRecord } from "../types/handwritten";
import {
  ACTION,
  CATEGORY,
  LEVEL,
  STAGE,
  TIER,
  byTime,
  byTimeNewestFirst,
  ingest,
  ingestJsonl,
  isAsOf,
  recordsAsOf,
  recordsSince,
  isKnownAction,
  latestByTime,
  recKey,
  tagText,
  timesOf,
  unknownActionCount,
  wireOf,
  type NormRecord,
} from "./ingest";
import { activityOf } from "./eventFilters";
import { isHostSampleRecord } from "./machineDrawerScope";
import { startFlowTail } from "./sse";
import { liveSampleToRecord } from "./liveChannel";
import { buildFlowWindow, flowLiveSessions, flowToRenderModel, machPresent, mergeTailRecords, sessionRunning, shapeRecords } from "./flow";
import { applyRecordToMetrics, foldFlowRecords, indexGraph, stepMeterFor, type MissionGraph } from "../lenses/mission/graph";
import { liveExecutions } from "./tokenRate";
import { machActive } from "../lenses/fleet/cards";
import { buildActivityTimeline } from "../lenses/fleet/timeline";
import { runRegions } from "../lenses/session/sessionRun";

const T0 = Date.parse("2026-09-27T10:00:00Z");
const at = (sec: number) => new Date(T0 + sec * 1000).toISOString();
const raw = (action: string, sec: number | string, extra: Record<string, unknown> = {}) => ({
  ts: typeof sec === "number" ? at(sec) : sec,
  action,
  session_id: "s1",
  machine_uid: "u1",
  handle: "coder",
  payload: {},
  ...extra,
});

describe("ingest: the typed fields", () => {
  it("keeps a known action as its Action and an unknown one as its own text", () => {
    const [known, unknown] = ingest([raw("dispatch.start", 0), raw("dispatch.cycle.suspected", 1)]);
    expect(known.action).toBe(ACTION.DispatchStart);
    expect(unknown.action).toBe("dispatch.cycle.suspected");
    expect(activityOf(unknown)).toBe("dispatch.cycle.suspected");
  });

  it("does not read a spaced spelling as its dotted twin", () => {
    // The daemon serves one convention; a spaced action is an unknown one.
    const [spaced] = ingest([raw("dispatch start", 0)]);
    expect(spaced.action).toBe("dispatch start");
    expect(spaced.action === ACTION.DispatchStart).toBe(false);
    expect(activityOf(spaced)).toBe("dispatch start");
  });

  it("types level, category, stage and tier; an unknown value keeps its text and equals no constant", () => {
    const [r] = ingest([raw("dispatch.start", 0, { level: "warn", category: "telemetry", stage: "tier-decision", tier: "local" })]);
    expect(r.level === LEVEL.Warn && r.category === CATEGORY.Telemetry && r.stage === STAGE.TierDecision && r.tier === TIER.Local).toBe(true);
    const [odd] = ingest([raw("dispatch.start", 0, { level: "loud", category: "novel", stage: "verify", tier: "cloud" })]);
    expect([odd.level, odd.category, odd.stage, odd.tier].map(tagText)).toEqual(["loud", "novel", "verify", "cloud"]);
    const allConstants: unknown[] = [...Object.values(LEVEL), ...Object.values(CATEGORY), ...Object.values(STAGE), ...Object.values(TIER)];
    expect([odd.level, odd.category, odd.stage, odd.tier].some((v) => allConstants.includes(v))).toBe(false);
    const [bare] = ingest([{ ts: at(0) }]);
    expect([bare.action, bare.level, bare.category, bare.stage, bare.tier]).toEqual([undefined, undefined, undefined, undefined, undefined]);
    expect("level" in bare).toBe(false);
  });

  it("shows, searches and dedups by the text the wire carried", () => {
    const [a, b] = ingest([raw("dispatch.start", 0, { stage: "verify" }), raw("dispatch.start", 0, { stage: "ship-it" })]);
    expect(wireOf(a).stage).toBe("verify");
    expect(recKey(a)).not.toBe(recKey(b));
    expect(isKnownAction(a.action)).toBe(true);
    expect(isKnownAction(ingest([raw("dispatch start", 0)])[0].action)).toBe(false);
  });

  it("keeps every other field, including ones it does not know", () => {
    const [r] = ingest([raw("dispatch.start", 0, { reasoning: "why", live_cadence_ms: 250 })]);
    expect((r as unknown as { reasoning: string }).reasoning).toBe("why");
    expect((r as unknown as { live_cadence_ms: number }).live_cadence_ms).toBe(250);
    expect(wireOf(r)).not.toHaveProperty("tMs");
    expect(wireOf(r).ts).toBe(at(0));
  });

  it("parses ts once into tMs, null when missing or unparseable", () => {
    const [ok, bad, missing, num] = ingest([raw("operator.note", 5), raw("operator.note", "not a time"), { action: "operator.note" }, raw("operator.note", 0, { ts: 12345 })]);
    expect(ok.tMs).toBe(T0 + 5000);
    expect(bad.tMs).toBeNull();
    expect(missing.tMs).toBeNull();
    expect(num.tMs).toBeNull();
  });

  it("drops non-records and the schema header, and keeps order", () => {
    const out = ingest([{ _type: "schema", version: "1" }, null, 7, "x", [], raw("operator.note", 2), raw("operator.note", 1)]);
    expect(out.map((r) => r.tMs)).toEqual([T0 + 2000, T0 + 1000]);
  });
});

describe("ingest: one test per entry point", () => {
  it("a /flow/<date> body: a bare array, parsed once per body object", () => {
    const body = [raw("dispatch.start", 0)];
    const first = ingest(body);
    expect(first).toHaveLength(1);
    expect(ingest(body)).toBe(first);
    expect(ingest([...body])).not.toBe(first);
  });

  it("a /flow-session or /flow-mission body: {records}", () => {
    expect(ingest({ records: [raw("dispatch.start", 0)], count: 1, truncated: false })[0].action).toBe(ACTION.DispatchStart);
  });

  it("the legacy {flow} wrapper, and anything else as no records", () => {
    expect(ingest({ flow: [raw("operator.note", 0)] })).toHaveLength(1);
    expect(ingest({ nope: [] })).toEqual([]);
    expect(ingest(null)).toEqual([]);
    expect(ingest("[]")).toEqual([]);
  });

  it("the committed static flow file: JSONL text", () => {
    const text = [JSON.stringify({ _type: "schema" }), "", JSON.stringify(raw("dispatch.start", 0)), "{truncated", JSON.stringify(raw("dispatch.complete", 3))].join("\n");
    expect(ingestJsonl(text).map((r) => r.action)).toEqual([ACTION.DispatchStart, ACTION.DispatchComplete]);
  });

  it("an SSE flow-tail message lands in the tail cache ingested", () => {
    const qc = new QueryClient();
    const sources: { onmessage?: (e: MessageEvent<string>) => void; close: () => void }[] = [];
    const factory = () => {
      const s = { close: () => {} } as { onmessage?: (e: MessageEvent<string>) => void; close: () => void };
      sources.push(s);
      return s as unknown as EventSource;
    };
    const handle = startFlowTail(qc, ["tail"], "2026-09-27", factory);
    sources[0].onmessage?.({ data: JSON.stringify(raw("dispatch.start", 0)) } as MessageEvent<string>);
    sources[0].onmessage?.({ data: JSON.stringify({ _type: "schema" }) } as MessageEvent<string>);
    sources[0].onmessage?.({ data: "{not json" } as MessageEvent<string>);
    const cached = qc.getQueryData<NormRecord[]>(["tail"]) ?? [];
    handle.close();
    expect(cached).toHaveLength(1);
    expect(cached[0].tMs).toBe(T0);
    expect(cached[0].action).toBe(ACTION.DispatchStart);
  });

  it("a live-channel sample becomes an ingested record", () => {
    const rec = liveSampleToRecord({ v: 1, kind: "model", at_ms: T0 + 250, session_id: "s1", role: "coder", fields: { turn_seq: 1 } });
    expect(rec?.action).toBe(ACTION.DispatchTurnHeartbeat);
    expect(rec?.tMs).toBe(T0 + 250);
  });

  it("a record the viewer synthesizes (the per-session runtime row) is ingested too", () => {
    const shaped = shapeRecords(ingest([raw("dispatch.turn", 1, { payload: { turn_seq: 2 } })]));
    const runtime = shaped.find((r) => r.source === "runtime");
    expect(runtime?.category).toBe(CATEGORY.Telemetry);
    expect(runtime?.tMs).toBe(T0 + 1000);
  });

  it("the reconcile backstop's merge keeps an untimed record inside the window", () => {
    const merged = mergeTailRecords(ingest([raw("operator.note", 100)]), ingest([raw("operator.note", 200), raw("operator.note", "garbage")]), T0 + 150_000);
    expect(merged.map((r) => r.tMs)).toEqual([T0 + 200_000, null]);
  });
});

describe("vocabulary skew is loud", () => {
  it("counts records whose action this build does not name", () => {
    const recs = ingest([raw("dispatch.start", 0), raw("dispatch start", 1), raw("wibble.fired", 2), { ts: at(3) }]);
    expect(unknownActionCount(recs)).toBe(2);
  });

  it("a retired action is known: not counted, not warned, still read by its other fields", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    try {
      const recs = ingest([
        raw("telemetry.process", 0, { category: "telemetry", source: "process", payload: { cpu: 12 } }),
        raw("mission.compile.error", 1),
        raw("mission reopen", 2),
      ]);
      expect(recs.every((r) => isKnownAction(r.action))).toBe(true);
      expect(unknownActionCount(recs)).toBe(0);
      expect(warn).not.toHaveBeenCalled();
      expect(recs.some((r) => r.action === ACTION.MachineTelemetry)).toBe(false);
      expect(isHostSampleRecord(recs[0])).toBe(true);
    } finally {
      warn.mockRestore();
    }
  });

  it("warns on the console once per unknown spelling", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    try {
      ingest([raw("skew.once", 0), raw("skew.once", 1), raw("skew.twice", 2), raw("dispatch.start", 3)]);
      ingest([raw("skew.once", 4)]);
      expect(warn.mock.calls.map((c) => String(c[0]).match(/"([^"]+)"/)?.[1])).toEqual(["skew.once", "skew.twice"]);
    } finally {
      warn.mockRestore();
    }
  });
});

describe("ingest: raw records cannot reach a lens", () => {
  it("a FlowRecord does not satisfy NormRecord", () => {
    const wire: FlowRecord = { ts: at(0), action: "dispatch.start" };
    // @ts-expect-error a raw wire record has not passed through `ingest`
    expect(activityOf(wire)).toBe("dispatch start");
    // @ts-expect-error nor does an array of them
    expect(() => shapeRecords([wire])).not.toThrow();
  });
});

describe("the bad-timestamp policy", () => {
  const data = ingest([raw("operator.note", 10), raw("operator.note", "bad"), raw("operator.note", 30), raw("operator.note", 20)]);

  it("an untimed record is inside every as-of and since cut", () => {
    expect(recordsAsOf(data, T0).map((r) => r.ts)).toEqual(["bad"]);
    expect(recordsAsOf(data, T0 + 20_000).map((r) => r.ts)).toEqual([at(10), "bad", at(20)]);
    expect(recordsSince(data, T0 + 25_000).map((r) => r.ts)).toEqual(["bad", at(30)]);
    expect(isAsOf(data[1], -Infinity)).toBe(true);
  });

  it("an untimed record adds nothing to time arithmetic", () => {
    expect(timesOf(data)).toEqual([T0 + 10_000, T0 + 30_000, T0 + 20_000]);
  });

  it("an untimed record sorts after every timed one", () => {
    expect([...data].sort(byTime).map((r) => r.ts)).toEqual([at(10), at(20), at(30), "bad"]);
  });

  it("recordsAsOf returns the window itself when nothing is ahead, even with an untimed record", () => {
    expect(recordsAsOf(data, T0 + 60_000)).toBe(data);
  });

  it("a terminal with a bad timestamp closes the run on every surface", () => {
    const recs = ingest([
      raw("dispatch.start", 0),
      raw("dispatch.turn", 5, { payload: { turn_seq: 1 } }),
      raw("dispatch.complete", "not-a-time", { payload: { wall_ms: 5000 } }),
    ]);
    const t = T0 + 6_000;
    const none = new Set<string>();
    expect(sessionRunning(recs, none, "s1", t), "flow liveness").toBe(false);
    expect([...flowLiveSessions(recs, t)], "flow-derived live set").toEqual([]);
    expect(machActive(recs, none, "u1", t), "fleet card").toBe(false);
    expect(liveExecutions([recs], t), "scope readings").toEqual([]);
    const lane = buildActivityTimeline(recs, new Map(), ["u1"], none, t, t, 10).lanes[0];
    expect(lane.bars.map((b) => b.cls), "timeline bar").toEqual(["done"]);
    const view = runRegions(flowToRenderModel(recs), "s1", t);
    expect(view.header.pillLabel.toLowerCase(), "run page").toBe("complete");
  });

  it("a pick by time takes an untimed record only when nothing timed exists", () => {
    const [timed, untimed, later] = ingest([raw("operator.note", 10), raw("operator.note", "bad"), raw("operator.note", 20)]);
    expect(latestByTime([timed, untimed, later])).toBe(later);
    expect(latestByTime([untimed, timed])).toBe(timed);
    expect(latestByTime([untimed])).toBe(untimed);
    expect(latestByTime([])).toBeUndefined();
    // The machine's presence: a timed online outvotes an untimed offline.
    const edges = ingest([raw("machine.online", 10), raw("machine.offline", "bad")]);
    expect(machPresent(edges, new Map(), T0 + 60_000, "u1")).toBe(true);
    expect(machPresent(ingest([raw("machine.offline", "bad")]), new Map(), T0, "u1")).toBe(false);
  });

  it("a newest-first scan reads timed records first and untimed ones last", () => {
    const [a, u, b] = ingest([raw("operator.note", 10), raw("operator.note", "bad"), raw("operator.note", 20)]);
    expect([a, u, b].sort(byTimeNewestFirst).map((r) => r.ts)).toEqual([at(20), at(10), "bad"]);
  });

  it("the run page's CTX NOW is the latest timed sample's, not an untimed one's", () => {
    const ctx = (sec: number | string, used: number) => raw("telemetry.context", sec, { category: "telemetry", source: "context", payload: { used, max: 1000 } });
    const recs = ingest([raw("dispatch.start", 0), ctx(1, 100), ctx(2, 300), ctx("garbled", 900)]);
    const tile = runRegions(flowToRenderModel(recs), "s1", T0 + 3_000).metrics.find((m) => m.label === "CTX NOW");
    expect(tile?.value).toBe("300");
  });

  it("an untimed terminal closes the mission graph's step too, status and meter", () => {
    const graph: MissionGraph = {
      mission_id: "m1",
      mission_status: "active",
      generated_at_ms: T0 + 1_000,
      edges: [],
      nodes: [{ id: "t", label: "t", kind: "task", status: "running", depth: 0, steps: [{ id: "s", label: "s", kind: "dispatch.internal", status: "running" }] }],
    };
    const idx = indexGraph(graph);
    const recs = ingest([
      raw("dispatch.start", 0, { handle: "s", payload: { step_id: "s" } }),
      raw("dispatch.turn", 5, { handle: "s", payload: { step_id: "s" } }),
      raw("step.complete", "not-a-time", { handle: "s", payload: { step_id: "s" } }),
    ]);
    const folded = foldFlowRecords(graph, recs, idx, "m1");
    const step = folded.nodes[0].steps![0];
    expect(step.status, "step status").toBe("complete");
    let metrics = {};
    for (const r of recs) metrics = applyRecordToMetrics(metrics, r, idx, "m1");
    const meter = stepMeterFor(step, metrics, T0 + 60_000);
    expect(meter.generating, "step meter").toBe(false);
    expect(meter.wallMs, "the step's wall time ends at its last known activity").toBe(5_000);
  });

  it("the same untimed terminal, in the live window, is not dropped by the 24h cut", () => {
    const win = buildFlowWindow([], ingest([raw("dispatch.start", 0), raw("dispatch.complete", "bad")]), T0 + 1000);
    expect(win.map((r) => r.action)).toEqual([ACTION.DispatchStart, ACTION.DispatchComplete]);
  });
});
