// The promise this module exists for: every surface gives the same run the
// same phase at the same moment. Each fixture below is scrubbed across its
// boundaries, and at every instant the fleet card, the activity timeline's
// bar, the run page's pill and the live scope must agree on whether the run
// is in flight, and the timeline bar and the pill on how it ended.
process.env.TZ = "UTC";
import { describe, expect, it } from "vitest";
import { buildFleetCard } from "../lenses/fleet/cards";
import { buildActivityTimeline } from "../lenses/fleet/timeline";
import { runRegions } from "../lenses/session/sessionRun";
import { flowToRenderModel, statusClass } from "./flow";
import { DEFAULT_POLICY, lifecycleAt, toRunState } from "./lifecycle";
import { sessionRun } from "./runRef";
import { liveExecutions } from "./tokenRate";
import { normAll, type RawRecord } from "../testing/records";
import { recordsAsOf, type NormRecord } from "./ingest";

const T0 = Date.parse("2026-09-27T10:00:00Z");
const at = (s: number) => new Date(T0 + s * 1000).toISOString();
const U = "u1";
const rec = (s: number | null, action: string, extra: RawRecord = {}): RawRecord => ({
  ts: s === null ? "not-a-time" : at(s),
  action,
  session_id: "s1",
  machine_uid: U,
  handle: "coder",
  ...extra,
});
const STALE_S = DEFAULT_POLICY.staleAfterMs / 1000;

/** What each surface says about the run on `sid` at `t`. */
function surfaces(data: NormRecord[], sid: string, t: number) {
  const card = buildFleetCard(data, new Map(), null, new Set(), false, U, false, t);
  const bars = buildActivityTimeline(data, new Map(), [U], new Set(), t, t, 1440).lanes[0].bars.filter((b) => b.sid === sid);
  // The run page is handed its records cut at the playhead (SessionReplay).
  const view = runRegions(flowToRenderModel(recordsAsOf(data, t)), sid, t);
  const run = sessionRun(data, sid, t);
  const phase = run ? lifecycleAt(run, t, DEFAULT_POLICY).phase : "not_started";
  return {
    phase,
    cardRunning: card.runningSessionIds.includes(sid),
    barCls: bars.at(-1)?.cls ?? null,
    pillCls: view.header.pillCls,
    pageLive: view.live,
    scopeLive: liveExecutions([data.filter((r) => r.session_id === sid)], t).length > 0,
    expectedCls: run ? statusClass(toRunState(lifecycleAt(run, t, DEFAULT_POLICY))) : null,
  };
}

function expectAgreement(data: NormRecord[], sid: string, t: number, label: string) {
  const s = surfaces(data, sid, t);
  const running = s.phase === "open" || s.phase === "waiting";
  expect(s.cardRunning, `${label}: fleet card`).toBe(running);
  expect(s.pageLive, `${label}: run page`).toBe(running);
  expect(s.pillCls, `${label}: run pill`).toBe(s.expectedCls);
  if (s.phase !== "not_started") expect(s.barCls, `${label}: timeline bar`).toBe(s.expectedCls);
  return s;
}

describe("every surface states the same lifecycle (the PR 4 promise)", () => {
  it("a silent run: running inside the staleness window, stopped with no ending past it", () => {
    const data = normAll([rec(0, "dispatch.start"), rec(10, "dispatch.turn.heartbeat")]);
    expect(expectAgreement(data, "s1", T0 + 60_000, "inside").phase).toBe("open");
    expect(surfaces(data, "s1", T0 + 60_000).scopeLive).toBe(true);
    const past = T0 + (10 + STALE_S + 1) * 1000;
    expect(expectAgreement(data, "s1", past, "past").phase).toBe("stale");
    expect(surfaces(data, "s1", past).scopeLive).toBe(false);
  });

  it("a relaunched session id: closed between attempts, running once relaunched", () => {
    const data = normAll([rec(0, "dispatch.start"), rec(60, "dispatch.complete"), rec(300, "dispatch.start"), rec(310, "dispatch.turn")]);
    expect(expectAgreement(data, "s1", T0 + 100_000, "between").phase).toBe("closed");
    expect(expectAgreement(data, "s1", T0 + 320_000, "relaunched").phase).toBe("open");
  });

  it("a budget wait before any start: waiting on every surface, then closed by its stop", () => {
    const wait = rec(0, "budget.wait", { category: "telemetry", source: "budget", payload: { endpoint_id: "azure", wait_seconds: 86_000 } });
    const data = normAll([wait]);
    expect(expectAgreement(data, "s1", T0 + 3 * 3600_000, "waiting").phase).toBe("waiting");
    const stopped = normAll([wait, rec(4 * 3600, "budget.stop", { payload: { reason: "mission `m` is aborted" } })]);
    expect(expectAgreement(stopped, "s1", T0 + 5 * 3600_000, "stopped").phase).toBe("closed");
  });

  it("a terminal with a bad timestamp closes the run everywhere", () => {
    const data = normAll([rec(0, "dispatch.start"), rec(5, "dispatch.turn"), rec(null, "dispatch.complete")]);
    expect(expectAgreement(data, "s1", T0 + 10_000, "bad ts").phase).toBe("closed");
  });

  it("two missions sharing a session id never merge: A's end does not close B", () => {
    const data = normAll([
      rec(0, "dispatch.start", { mission_id: "A" }),
      rec(60, "dispatch.complete", { mission_id: "A" }),
      rec(600, "dispatch.start", { mission_id: "B" }),
      rec(610, "dispatch.turn.heartbeat", { mission_id: "B" }),
    ]);
    const t = T0 + 620_000;
    expect(expectAgreement(data, "s1", t, "shared").phase).toBe("open");
    const bars = buildActivityTimeline(data, new Map(), [U], new Set(), t, t, 1440).lanes[0].bars;
    expect(bars.map((b) => b.cls)).toEqual(["done", "run"]);
    expect(buildFleetCard(data, new Map(), null, new Set(), false, U, false, t).runsCount).toBe(1);
  });
});

// One time-pick rule: a run's start and latest activity are read by a scan,
// never by spreading its timestamps into `Math.max`/`Math.min`, whose
// argument count a long run outgrows.
describe("a long run", () => {
  it("is judged without spreading its timestamps into an argument list", () => {
    const raw: RawRecord[] = [rec(0, "dispatch.start")];
    for (let i = 1; i <= 300_000; i++) raw.push(rec(i / 100, "dispatch.turn"));
    const data = normAll(raw);
    const t = T0 + 3_000_000;
    const run = sessionRun(data, "s1", t);
    expect(run).not.toBeNull();
    const lc = lifecycleAt(run!, t, DEFAULT_POLICY);
    expect(lc.startMs).toBe(T0);
    expect(lc.lastActivityMs).toBe(T0 + 3_000_000);
  });
});
