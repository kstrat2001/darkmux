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
import type { Grain } from "./runRef";
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
// across views." A run's status is decided ONCE, where the run ends, and every
// view renders that decision: the runs board and `darkmux run list` (the
// daemon's row), the fleet timeline bar, the run page's pill and its tiles.
// The decision is written to the row's source (the lab record, the mission
// envelope) AND to the run's flow terminal from the one decider
// (`interrupt::stop_reason`), so a view that has the row and a view that has
// only the flow (a peer, a ghost, `/runs` not answered yet) say the same.
//
// The fixtures are the runs that broke it, one per kind, each stopped by the
// operator with SIGTERM: a `darkmux lab run pepper-grinder`, a `darkmux
// dispatch`, and a `darkmux mission launch` (whose row's `dispatch_id` is its
// own run session, `<M>.run`).
describe("a run's status is decided once and every view renders it", () => {
  const END_S = 5679;
  const AFTER = T0 + (END_S + 60) * 1000;
  const stopped = { result_class: "error", error: "interrupted by an operator signal (SIGINT/SIGTERM/SIGHUP)", total_turns: 3, stop_reason: "SIGTERM" };
  const on = (session: string, mission: string | null) => (s: number, action: string, extra: RawRecord = {}): RawRecord =>
    rec(s, action, { session_id: session, ...(mission ? { mission_id: mission } : {}), ...extra });

  interface Fixture {
    readonly kind: Run["kind"];
    readonly session: string;
    readonly missionId: string | null;
    readonly records: NormRecord[];
    readonly row: Run;
  }
  const row = (kind: Run["kind"], id: string, dispatchId: string, status: Run["status"], abandoned?: Run["abandoned_reason"]): Run => ({
    id,
    kind,
    status,
    ...(abandoned ? { abandoned_reason: abandoned } : {}),
    dispatch_id: dispatchId,
    tracked: true,
    receive_key: 0,
  });

  const LAB_S = "pg-1.lab.adhoc.coder.pepper-grinder";
  const lab = on(LAB_S, null);
  const DISPATCH_S = "d-1.adhoc.coder.n1";
  const dispatch = on(DISPATCH_S, "d-1");
  const RUN_S = "m-1.run";
  const EXEC_S = "m-1.task.t1";
  const run = on(RUN_S, "m-1");
  const exec = on(EXEC_S, "m-1");
  const aborted: Fixture[] = [
    {
      kind: "lab",
      session: LAB_S,
      missionId: null,
      records: normAll([lab(0, "dispatch.start"), lab(37, "dispatch.turn"), lab(END_S, "dispatch.error", { level: "error", payload: stopped })]),
      row: row("lab", "pg-1", LAB_S, "abandoned", "aborted"),
    },
    {
      kind: "dispatch",
      session: DISPATCH_S,
      missionId: "d-1",
      records: normAll([dispatch(0, "dispatch.start"), dispatch(37, "dispatch.turn"), dispatch(END_S, "dispatch.error", { level: "error", payload: stopped })]),
      row: row("dispatch", "d-1", DISPATCH_S, "abandoned", "aborted"),
    },
    {
      kind: "mission",
      session: RUN_S,
      missionId: "m-1",
      records: normAll([
        run(0, "run.start", { handle: "review" }),
        exec(1, "dispatch.start"),
        exec(37, "dispatch.turn"),
        exec(END_S - 1, "dispatch.error", { level: "error", payload: stopped }),
        run(END_S, "run.error", { level: "error", handle: "review", payload: { result_class: "error", error: "interrupted by an operator signal", stop_reason: "SIGTERM" } }),
      ]),
      row: row("mission", "m-1", RUN_S, "abandoned", "aborted"),
    },
  ];

  /** The status, word and color kind one view shows. */
  const shown = (status: string | undefined, word: string | undefined) => ({ status, word, kind: workStatusKind(status) });
  const board = (r: Run) => shown(runBadgeStatus(r), runStatusLabel(r));

  /** What every view shows for the run on `sid` at `t`, judged `live` (the
   *  live edge) or in playback, with the board's `rows`. */
  function views(data: NormRecord[], rows: readonly Run[], sid: string, missionId: string | null, t: number, live: boolean) {
    const bar = buildActivityTimeline(data, new Map(), [U], new Set(), t, t, 1440, true, 0, t, undefined, null, [], DEFAULT_POLICY, new Set(), rows, live)
      .lanes[0].bars.find((b) => b.sid === sid);
    const grain: Grain | null = sessionRun(data, sid, t)?.group.grain ?? null;
    const page = runRegions(flowToRenderModel(recordsAsOf(data, t)), sid, t, true, null, null, NO_PRESENCE, DEFAULT_POLICY, null, false, ownRowOf(rows, sid, missionId, grain), live);
    const card = buildFleetCard(data, new Map(), null, new Set(), false, U, t);
    return {
      timeline: shown(bar?.status, bar?.title.split(" · ").at(-1)),
      page: shown(page.header.status, page.header.pillLabel.toLowerCase()),
      activeTimeSub: page.metrics.find((m) => m.label === "ACTIVE TIME")?.sub,
      pageLive: page.live,
      cardRunning: card.runningSessionIds.includes(sid),
    };
  }

  for (const f of aborted) {
    it(`a ${f.kind} the operator stopped reads aborted on every view, with its row and without it`, () => {
      const expected = board(f.row);
      expect(expected).toEqual({ status: "abandoned", word: "aborted", kind: "stopped" });
      for (const [label, rows] of [["with its row", [f.row]], ["without a row", []]] as const) {
        for (const live of [true, false]) {
          const v = views(f.records, rows, f.session, f.missionId, AFTER, live);
          const at = `${label}, ${live ? "live" : "playback"}`;
          expect(v.timeline, `${at}: fleet timeline bar`).toEqual(expected);
          expect(v.page, `${at}: run page pill`).toEqual(expected);
          expect(v.activeTimeSub ?? "", `${at}: run page tile`).not.toMatch(/errored|killed/);
          expect(v.cardRunning, `${at}: fleet card`).toBe(false);
        }
      }
    });
  }

  it("in playback, a run is running while it ran, and once it ended its ending is the row's", () => {
    for (const f of aborted) {
      const during = views(f.records, [f.row], f.session, f.missionId, T0 + 600_000, false);
      expect(during.timeline.status, f.kind).toBe("running");
      expect(during.page.status, f.kind).toBe("running");
      expect(during.cardRunning, f.kind).toBe(true);
    }
  });

  // The review's repro: a mission with one execution and
  // `DARKMUX_MISSION_WALL_CLOCK_TIMEOUT_SECONDS=15`. The bound cut the
  // execution (its `dispatch.error` names NO stop: the bound is not the
  // operator's) and the run completed degraded, as its row says. Its own run
  // session's bar and page read degraded, with the row and without it; the
  // execution reads its own ending, an error, never "aborted".
  it("a mission cut off by its own wall-clock bound reads degraded on its run's views and its execution reads error, never aborted", () => {
    const cut = { result_class: "error", error: "interrupted by an operator signal (the run's wall-clock bound) before completion", total_turns: 1 };
    const data = normAll([
      run(0, "run.start", { handle: "hostedwait" }),
      exec(1, "dispatch.start"),
      exec(15, "dispatch.error", { level: "error", payload: cut }),
      run(16, "run.complete", { handle: "hostedwait", payload: { result_class: "ok", status: "Degraded" } }),
    ]);
    const missionRow = row("mission", "m-1", RUN_S, "degraded");
    const t = T0 + 60_000;
    expect(board(missionRow)).toEqual({ status: "degraded", word: "degraded", kind: "degraded" });
    for (const rows of [[missionRow], []]) {
      for (const live of [true, false]) {
        const v = views(data, rows, RUN_S, "m-1", t, live);
        expect(v.timeline, `rows=${rows.length} live=${live}: bar`).toEqual(board(missionRow));
        expect(v.page, `rows=${rows.length} live=${live}: page`).toEqual(board(missionRow));
        const e = views(data, rows, EXEC_S, "m-1", t, live);
        expect(e.timeline.status).toBe("error");
        expect(e.page.status).toBe("error");
      }
    }
  });

  // A 4.0 mission row's `dispatch_id` is its own run session (`<M>.run`), so
  // its run session's views render the row. An execution's views never do:
  // the mission row's status is the mission's, not the execution's. A mission
  // row that names an execution (its run session out of the daemon's window,
  // or a pre-4.0 mission with none) is not that execution's either.
  it("a mission's row is its own run session's, and never an execution's", () => {
    const missionRow = row("mission", "m-1", RUN_S, "abandoned", "aborted");
    expect(ownRowOf([missionRow], RUN_S, "m-1", "run")).toBe(missionRow);
    expect(ownRowOf([missionRow], RUN_S, "m-2", "run"), "another mission's run session").toBeNull();
    expect(ownRowOf([missionRow], EXEC_S, "m-1", "execution")).toBeNull();
    const fallback = row("mission", "m-1", EXEC_S, "running");
    expect(ownRowOf([fallback], EXEC_S, "m-1", "execution"), "a mission row naming an execution").toBeNull();
    // The review's repro, as an older binary recorded it: the run session's
    // `run.complete` says nothing of the degraded outcome; the mission's row
    // does. The run session's bar and page render the row.
    const older = normAll([run(0, "run.start", { handle: "hostedwait" }), run(16, "run.complete", { handle: "hostedwait", payload: { result_class: "ok" } })]);
    const degradedRow = row("mission", "m-1", RUN_S, "degraded");
    for (const live of [true, false]) {
      const r = views(older, [degradedRow], RUN_S, "m-1", T0 + 60_000, live);
      expect(r.page, `live=${live}: page`).toEqual(board(degradedRow));
      expect(r.timeline, `live=${live}: bar`).toEqual(board(degradedRow));
    }
    const finished = normAll([exec(1, "dispatch.start"), exec(60, "dispatch.complete")]);
    const v = views(finished, [fallback], EXEC_S, "m-1", T0 + 120_000, true);
    expect(v.page.status).toBe("complete");
    expect(v.timeline.status).toBe("complete");
  });

  it("a dispatch row is a session's own only for the mission its records name", () => {
    const other = row("dispatch", "m-2", DISPATCH_S, "abandoned", "aborted");
    expect(ownRowOf([other], DISPATCH_S, "m-1", "execution")).toBeNull();
    expect(ownRowOf([other], DISPATCH_S, "m-2", "execution")).toBe(other);
    const ghost = row("dispatch", DISPATCH_S, DISPATCH_S, "abandoned", "aborted");
    expect(ownRowOf([ghost], DISPATCH_S, "m-1", "execution")).toBe(ghost);
  });

  // A `darkmux dispatch` run IS its one execution. Its execution ended with a
  // non-zero exit and no signal: the terminal is a `dispatch.error` naming no
  // stop, and the run's envelope (its row's source) records the same outcome
  // (`ResultClass::of_exit`, `dispatch_as_crew_of_one`). It used to finalize
  // `Degraded`: the row said degraded, a view with only the flow said error.
  it("a dispatch that failed with no signal reads error on every view, with its row and without it", () => {
    const failed = { result_class: "error", exit_code: 2, total_turns: 4 };
    const data = normAll([dispatch(0, "dispatch.start"), dispatch(37, "dispatch.turn"), dispatch(END_S, "dispatch.error", { level: "error", payload: failed })]);
    const dispatchRow = row("dispatch", "d-1", DISPATCH_S, "error");
    const expected = board(dispatchRow);
    expect(expected).toEqual({ status: "error", word: "error", kind: "error" });
    for (const [label, rows] of [["with its row", [dispatchRow]], ["without a row", []]] as const) {
      for (const live of [true, false]) {
        const v = views(data, rows, DISPATCH_S, "d-1", AFTER, live);
        const at = `${label}, ${live ? "live" : "playback"}`;
        expect(v.timeline, `${at}: fleet timeline bar`).toEqual(expected);
        expect(v.page, `${at}: run page pill`).toEqual(expected);
      }
    }
  });

  // A lab run is RUNNING until its own record ends, verify included. Its
  // dispatch session ends first (`dispatch.complete`); the run then verifies
  // the work, and its row (the lab record, `lifecycle.json`) still says
  // running. The row's decision used to read that ended session as the run
  // gone quiet ("no ending"), and this branch made every view render it.
  it("a lab run verifying after its dispatch ended reads running on every view", () => {
    const VERIFY_S = END_S + 30;
    const data = normAll([lab(0, "dispatch.start"), lab(37, "dispatch.turn"), lab(END_S, "dispatch.complete", { payload: { result_class: "ok", total_turns: 3 } })]);
    const labRow = row("lab", "pg-1", LAB_S, "running");
    const expected = board(labRow);
    expect(expected).toEqual({ status: "running", word: "running", kind: "running" });
    const t = T0 + VERIFY_S * 1000;
    const v = views(data, [labRow], LAB_S, null, t, true);
    expect(v.timeline, "fleet timeline bar").toEqual(expected);
    expect(v.page, "run page pill").toEqual(expected);
    expect(v.pageLive, "run page: in flight").toBe(true);
    const bar = buildActivityTimeline(data, new Map(), [U], new Set(), t, t, 1440, true, 0, t, undefined, null, [], DEFAULT_POLICY, new Set(), [labRow], true)
      .lanes[0].bars.find((b) => b.sid === LAB_S);
    expect((bar?.leftPct ?? 0) + (bar?.widthPct ?? 0), "the running bar reaches the playhead").toBeCloseTo(100, 5);
    const card = buildFleetCard(data, new Map(), null, new Set(), false, U, t, null, [labRow]);
    expect(card.runsCount, "fleet card counts it running").toBe(1);
  });

  // A run page's tiles state the shown ending, never the raw flow close. A lab
  // run recorded before terminals named a stop: its flow terminal reads error,
  // its row (the lab record) aborted. The pill said ABORTED while the ACTIVE
  // TIME tile's sub line said "errored".
  it("the run page's tiles read the shown state, not the flow's close edge", () => {
    const archived = normAll([lab(0, "dispatch.start"), lab(37, "dispatch.turn"), lab(END_S, "dispatch.error", { level: "error", payload: { result_class: "error", total_turns: 0 } })]);
    const labRow = row("lab", "pg-1", LAB_S, "abandoned", "aborted");
    for (const live of [true, false]) {
      const withRow = views(archived, [labRow], LAB_S, null, AFTER, live);
      expect(withRow.page).toEqual(board(labRow));
      expect(withRow.activeTimeSub ?? "", `live=${live}`).not.toMatch(/errored/);
    }
    // With no row the flow's error is the decision, and the tile says so.
    expect(views(archived, [], LAB_S, null, AFTER, true).activeTimeSub).toBe("errored");
    // The row decided error where the flow closed clean (a lab run whose
    // verify failed): the tile says errored, as the pill does.
    const clean = normAll([lab(0, "dispatch.start"), lab(END_S, "dispatch.complete", { payload: { result_class: "ok", total_turns: 3 } })]);
    const failed = views(clean, [row("lab", "pg-1", LAB_S, "error")], LAB_S, null, AFTER, true);
    expect(failed.page.status).toBe("error");
    expect(failed.activeTimeSub).toBe("errored");
  });
});
