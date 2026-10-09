// The promise this module exists for: every surface gives the same run the
// same phase at the same moment. Each fixture below is scrubbed across its
// boundaries, and at every instant the fleet card, the activity timeline's
// bar, the run page's pill and the live scope must agree on whether the run
// is in flight, and the timeline bar and the pill on how it ended.
process.env.TZ = "UTC";
import { describe, expect, it } from "vitest";
import { buildFleetCard } from "../testing/fleetCard";
import { buildActivityTimeline } from "../lenses/fleet/timeline";
import { runRegions } from "../lenses/session/sessionRun";
import { flowToRenderModel } from "./flow";
import { DEFAULT_POLICY, NO_PRESENCE, lifecycleAt, ownRowOf, toRunState } from "./lifecycle";
import { sessionRun } from "./runRef";
import { liveExecutions } from "./tokenRate";
import { normAll, type RawRecord } from "../testing/records";
import { recordsAsOf, type NormRecord } from "./ingest";
import { runBadgeStatus, runStatusLabel } from "../lenses/runs/format";
import { workStatusKind } from "../components/WorkStatus";
import type { Run } from "../types/generated/Run";

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
  const card = buildFleetCard(data, new Map(), null, new Set(), false, U, t);
  const bars = buildActivityTimeline(data, new Map(), [U], new Set(), t, t, 1440).lanes[0].bars.filter((b) => b.sid === sid);
  // The run page is handed its records cut at the playhead (SessionReplay).
  const view = runRegions(flowToRenderModel(recordsAsOf(data, t)), sid, t);
  const run = sessionRun(data, sid, t);
  const phase = run ? lifecycleAt(run, t, DEFAULT_POLICY).phase : "not_started";
  return {
    phase,
    cardRunning: card.runningSessionIds.includes(sid),
    barStatus: bars.at(-1)?.status ?? null,
    pillStatus: view.header.status,
    pageLive: view.live,
    scopeLive: liveExecutions([data.filter((r) => r.session_id === sid)], t).length > 0,
    expectedStatus: run ? toRunState(lifecycleAt(run, t, DEFAULT_POLICY)).status : null,
  };
}

function expectAgreement(data: NormRecord[], sid: string, t: number, label: string) {
  const s = surfaces(data, sid, t);
  const running = s.phase === "open" || s.phase === "waiting";
  expect(s.cardRunning, `${label}: fleet card`).toBe(running);
  expect(s.pageLive, `${label}: run page`).toBe(running);
  expect(s.pillStatus, `${label}: run pill`).toBe(s.expectedStatus);
  if (s.phase !== "not_started") expect(s.barStatus, `${label}: timeline bar`).toBe(s.expectedStatus);
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
    const wait = rec(0, "budget.wait", { category: "telemetry", source: "budget", payload: { endpoint_id: "azure", wait_ms: 86000000 } });
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
    expect(bars.map((b) => b.status)).toEqual(["complete", "running"]);
    expect(buildFleetCard(data, new Map(), null, new Set(), false, U, t).runsCount).toBe(1);
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

// The operator's rule (2026-10-07): "the color of the status should be a one
// time status setting so it should not even have a chance of being different
// across views." A run the daemon lists has its status decided once, on its
// `/runs` row (the runs board, `darkmux run list`); the run page and the fleet
// timeline render that decision, word and color, and never re-derive it.
//
// The fixture is the run that broke it: a `darkmux lab run pepper-grinder`
// stopped with SIGTERM. Its lab record says it was interrupted, so the board
// read "aborted" (yellow); its flow terminal, written before terminals named
// a stop, says only `result_class: error`, so the run page and the timeline
// bar re-derived "error" (red).
describe("every view renders the daemon's one decision for a run it lists", () => {
  const S = "pepper-grinder-coder-qwen38-1791342379-1.lab.adhoc.coder.pepper-grinder";
  const lab = (s: number, action: string, extra: RawRecord = {}): RawRecord => rec(s, action, { session_id: S, ...extra });
  const abortPayload = { result_class: "error", error: "dispatch terminated before completion (early return or panic)", total_turns: 0 };
  const END_S = 5679;
  const archived = normAll([
    lab(0, "dispatch.start"),
    lab(37, "dispatch.turn"),
    lab(END_S, "dispatch.error", { level: "error", payload: abortPayload }),
  ]);
  const row: Run = {
    id: "pepper-grinder-coder-qwen38-1791342379-1",
    kind: "lab",
    status: "abandoned",
    abandoned_reason: "aborted",
    dispatch_id: S,
    tracked: true,
    receive_key: 0,
  };

  /** The status, word and color kind one view shows. */
  const shown = (status: string | undefined, word: string | undefined) => ({ status, word, kind: workStatusKind(status) });
  const board = (r: Run) => shown(runBadgeStatus(r), runStatusLabel(r));

  /** What the run page and the timeline bar show for the run at `t`, judged
   *  `live` (the live edge) or in playback, with the board's rows. */
  function views(data: NormRecord[], rows: readonly Run[], t: number, live: boolean) {
    const bar = buildActivityTimeline(data, new Map(), [U], new Set(), t, t, 1440, true, 0, t, undefined, null, [], DEFAULT_POLICY, new Set(), rows, live)
      .lanes[0].bars.find((b) => b.sid === S);
    const page = runRegions(flowToRenderModel(recordsAsOf(data, t)), S, t, true, null, null, NO_PRESENCE, DEFAULT_POLICY, null, false, ownRowOf(rows, S, null), live).header;
    return { timeline: shown(bar?.status, bar?.title.split(" · ").at(-1)), page: shown(page.status, page.pillLabel.toLowerCase()) };
  }

  it("at the live edge, the run page and the timeline bar show the row's status, word and color", () => {
    const expected = board(row);
    expect(expected).toEqual({ status: "abandoned", word: "aborted", kind: "stopped" });
    const v = views(archived, [row], T0 + (END_S + 60) * 1000, true);
    expect(v.timeline, "fleet timeline bar").toEqual(expected);
    expect(v.page, "run page").toEqual(expected);
  });

  it("in playback, it is running while it ran, and once it ended its ending is the row's", () => {
    const during = views(archived, [row], T0 + 600_000, false);
    expect(during.timeline.status).toBe("running");
    expect(during.page.status).toBe("running");
    const after = views(archived, [row], T0 + (END_S + 60) * 1000, false);
    expect(after.timeline).toEqual(board(row));
    expect(after.page).toEqual(board(row));
  });

  it("at the live edge the row is the status even before the flow has the run's end", () => {
    // The daemon has the lab record's verdict; the flow terminal is not in yet.
    const open = normAll([lab(0, "dispatch.start"), lab(37, "dispatch.turn")]);
    const t = T0 + 60_000;
    const live = views(open, [row], t, true);
    expect(live.timeline).toEqual(board(row));
    expect(live.page).toEqual(board(row));
    // In playback the same instant is judged from the records: it was running then.
    expect(views(open, [row], t, false).page.status).toBe("running");
  });

  it("a dispatch row is a session's own only for the mission its records name", () => {
    const other: Run = { ...row, kind: "dispatch", id: "m-2" };
    expect(ownRowOf([other], S, "m-1")).toBeNull();
    expect(ownRowOf([other], S, "m-2")).toBe(other);
    const ghost: Run = { ...row, kind: "dispatch", id: S };
    expect(ownRowOf([ghost], S, "m-1")).toBe(ghost);
  });

  it("a mission's row is not its execution's: an execution page keeps its own status", () => {
    const mission: Run = { ...row, id: "m-1", kind: "mission", status: "running", abandoned_reason: undefined };
    expect(ownRowOf([mission], S, null)).toBeNull();
    const v = views(archived, [mission], T0 + (END_S + 60) * 1000, true);
    expect(v.page.status).toBe("error");
    expect(v.timeline.status).toBe("error");
  });

  it("with no row, a terminal that names the operator's stop reads aborted on every view", () => {
    const stopped = normAll([
      lab(0, "dispatch.start"),
      lab(37, "dispatch.turn"),
      lab(END_S, "dispatch.error", { level: "error", payload: { ...abortPayload, stop_reason: "SIGTERM" } }),
    ]);
    const v = views(stopped, [], T0 + (END_S + 60) * 1000, true);
    expect(v.timeline).toEqual(board(row));
    expect(v.page).toEqual(board(row));
  });
});
