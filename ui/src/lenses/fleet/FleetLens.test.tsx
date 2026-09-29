import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { render, screen, waitFor, fireEvent, act } from "@testing-library/react";
import { QueryClientProvider, QueryClient } from "@tanstack/react-query";
import { FleetLens } from "./FleetLens";

import { pepperAt, pepperRecords } from "../../testing/pepperGrinderRun";
import { todayUTC, prevDateUTC } from "../../lib/flow";
import { DEFAULT_POLICY } from "../../lib/lifecycle";
import { __runIndexBuilds } from "../../lib/runRef";
import { ACTION, __asOfFilterRuns } from "../../lib/ingest";
import { tokensOffMeter } from "./savings";
import { closeOpenModal } from "../../lib/dialogManager";
import { queryKeys } from "../../lib/queryKeys";
import { machineKeyHash } from "../../lib/machineKey";
import { __clockDebug } from "../../lib/clock";
import type { NormRecord } from "../../lib/ingest";
import { norm, normAll, type RawRecord } from "../../testing/records";

/** The TokenScope props a probe serialized, as far as these tests read them. */
type ScopeProbe = Record<string, unknown> & { clock: { kind: string; tMs: number }; restEndMs: number; centerUnit?: string };

// (#2886 pass 5, MUST — fresh-reviewer finding F5) Several fixes in this
// file stayed green while broken in the actual render path: the DOM-text
// assertions elsewhere in this file (`.mach-scope__rate`'s textContent) all
// read from the SAME `selectedExec` the tube reads from, so a bug that hit
// ONLY the tube's own props (not the neighboring text) had nothing here to
// catch it. Mocking `TokenScope` and recording every prop it's called with
// lets a test assert on what the operator's SCREEN actually receives —
// `tokensPerSec`/`tone`/`stalled`/`resting` — not merely on `FleetCard`
// fields that happen to agree with it today. `data-props` carries the
// latest call's props as JSON; `latestTokenScopeProps()` below reads it
// back typed.
vi.mock("../../components/TokenScope", () => ({
  TokenScope: (props: Record<string, unknown>) => <div data-testid="token-scope-probe" data-props={JSON.stringify(props)} />,
}));

// (#2928) A pass-through spy on the activity timeline's build, so a test can
// pin that live samples re-render the cards without rebuilding it.
vi.mock("./timeline", async (importOriginal) => {
  const real = await importOriginal<typeof import("./timeline")>();
  return { ...real, buildActivityTimeline: vi.fn(real.buildActivityTimeline) };
});

// (#2928 re-review, C-1) A pass-through spy on the card BASE builder: a live
// sample must re-derive the cards' readings without rebuilding their bases.
vi.mock("./cards", async (importOriginal) => {
  const real = await importOriginal<typeof import("./cards")>();
  return { ...real, buildFleetCardBase: vi.fn(real.buildFleetCardBase) };
});

// (#2911) A pass-through spy: every behavior is the real `tokensOffMeter`;
// the tick tests read its call count to pin that a tick does not make the
// hero recompute its token sums.
vi.mock("./savings", async (importOriginal) => {
  const real = await importOriginal<typeof import("./savings")>();
  return { ...real, tokensOffMeter: vi.fn(real.tokensOffMeter) };
});

function latestTokenScopeProps(): Record<string, unknown> {
  const nodes = document.querySelectorAll('[data-testid="token-scope-probe"]');
  const last = nodes[nodes.length - 1];
  if (!last) throw new Error("no TokenScope probe rendered");
  return JSON.parse(last.getAttribute("data-props")!) as Record<string, unknown>;
}

// (#1913) Every fixture below anchors its records at "T10:00" of `today`
// (`todayUTC()`), and liveness (each run's lifecycle, `DEFAULT_POLICY.staleAfterMs`) is
// judged against REAL wall-clock now. Left alone, that means the suite's
// pass/fail depended on what time of day it happened to run: before 10:00
// UTC the fixture sits in the future (trivially "fresh"), and after
// 10:05 UTC it's stale and every "N running" assertion goes red — a test
// that fails on a schedule, not intermittently. Freezing `Date` to a fixed
// instant makes the fixture-to-now distance an ASSERTED PARAMETER instead of
// something inherited from the clock. `toFake: ["Date"]` leaves
// setTimeout/setInterval alone, so `waitFor()`'s real-timer polling still
// works.
const FROZEN_NOW = "2026-06-15T10:02:00.000Z"; // 2 minutes after the T10:00 anchor, well inside DEFAULT_POLICY.staleAfterMs

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["Date"] });
  vi.setSystemTime(new Date(FROZEN_NOW));
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  window.location.hash = "";
  // See EventLogColumn.test.tsx's own comment: dialogManager's open/close
  // state is a module-level singleton that outlives `render()`/unmount.
  closeOpenModal({ restore: false });
});

function renderFleetLens(
  props: Parameters<typeof FleetLens>[0] = {},
  /** Share one client across two renders when a test needs the SECOND
   * render to start from an already-settled query cache — see the replay
   * gate's test for why a fresh client makes that assertion vacuous. */
  queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } }),
) {
  return render(
    <QueryClientProvider client={queryClient}>
      <FleetLens {...props} />
    </QueryClientProvider>,
  );
}

/** Waits until `/fleet/machines/live`, `/fleet/roster`, and `/machine/specs`
 * have all settled (success or error) in `queryClient`'s cache, rather than
 * inferring settlement from an incidental DOM condition.
 *
 * `rosterOnly` (#1855 follow-up) depends on all three of those queries, and
 * they resolve on THEIR OWN independent schedules — nothing forces them to
 * settle before an unrelated `waitFor` (e.g. one that only watches the
 * hero's text, which depends on the flow queries, not these) resolves.
 * Asserting an ABSENCE (e.g. "no phantom card renders") right after such an
 * unrelated wait is a race: the assertion can pass merely because the
 * roster/specs queries haven't resolved YET, which looks identical to the
 * fix genuinely excluding the entry — exactly the "a probe that passes
 * without executing is worse than no probe" trap. Waiting on the actual
 * query states removes the race instead of hoping the timing works out. */
async function waitForFleetQueriesSettled(queryClient: QueryClient) {
  await waitFor(() => {
    for (const key of [queryKeys.fleetMachinesLive(), queryKeys.fleetRoster(), queryKeys.machineSpecs()]) {
      const state = queryClient.getQueryState(key);
      expect(state?.status, `query ${JSON.stringify(key)} settled`).not.toBe("pending");
    }
  });
}

/** Routes `fetchJson` calls the same way the real daemon's endpoint set
 * does, keyed on the URL. `flowToday`/`flowYesterday` default to an empty
 * window so a test only has to name the records it actually cares about. */
function mockFleetFetch(opts: {
  flowToday?: unknown[];
  flowYesterday?: unknown[];
  machines?: unknown[];
  /** (#1809) This daemon's OWN `/machine/specs` — the confirmed-local
   * signal `localMachineUid` resolves against. Omitted (the default) keeps
   * the pre-existing 404 (no daemon has confirmed ANY card as local), so
   * every pre-#1809 test in this file is unaffected by this field's
   * addition. Set it to make ONE uid resolve as local — see the two
   * locality-split tests below for why that distinction now matters. */
  specs?: unknown;
  /** (#1923) `GET /runs` rows — omitted (the default) keeps the pre-existing
   * 404 (every pre-#1923 test in this file is unaffected), same pattern as
   * `specs` above. */
  runs?: unknown[];
  /** (#1855) `GET /fleet/roster` entries — the operator's DECLARED
   * topology. Omitted defaults to an empty roster (`error: null`), so
   * every pre-#1855 test in this file is unaffected. */
  roster?: unknown[];
  /** (#1855 follow-up, CONSIDER 4) `GET /fleet/roster`'s `error` field — a
   * present-but-corrupt roster file. Omitted (the default) keeps `null`,
   * so every pre-CONSIDER-4 test in this file is unaffected. */
  rosterError?: string;
  /** (#2958) Paths whose answer waits on the given promise: a daemon that
   *  is slow to answer one endpoint (the operator measured `/runs` at 3.3 s).
   *  Resolve the promise to let the answer through. */
  hold?: Record<string, Promise<void>>;
  /** (#2965) Paths that answer with this HTTP error status: a daemon whose
   *  read of that endpoint failed. */
  fail?: Record<string, number>;
  /** (#2965) Paths whose FIRST read answers 500 and every later one normally:
   *  a blip that has since healed. */
  failOnce?: string[];
  /** (#2965) Further days that answer `200 []`: the mock names today and
   *  yesterday when it is built, so a day reached by a rollover needs this. */
  emptyDays?: string[];
  /** Session ids `/fleet/sessions/live` reports beating. */
  sessions?: string[];
} = {}) {
  const today = todayUTC();
  const yesterday = prevDateUTC(today);
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      const path = String(url);
      const held = opts.hold?.[path];
      if (held) return held.then(() => answer(path));
      return answer(path);
    }),
  );
  function answer(path: string): Promise<Response> {
    const once = opts.failOnce?.indexOf(path) ?? -1;
    if (once >= 0) {
      opts.failOnce!.splice(once, 1);
      return Promise.resolve(new Response("boom", { status: 500, statusText: "Internal Server Error" }));
    }
    if (opts.emptyDays?.some((d) => path === `/flow/${d}`)) return Promise.resolve(new Response("[]", { status: 200 }));
    const failed = opts.fail?.[path];
    if (failed) return Promise.resolve(new Response("boom", { status: failed, statusText: "Internal Server Error" }));
    if (path === `/flow/${today}`) return Promise.resolve(new Response(JSON.stringify(opts.flowToday ?? []), { status: 200 }));
    if (path === `/flow/${yesterday}`) return Promise.resolve(new Response(JSON.stringify(opts.flowYesterday ?? []), { status: 200 }));
    if (path === "/fleet/machines/live") {
      return Promise.resolve(
        new Response(
          JSON.stringify({
            machines: opts.machines ?? [],
            meta: { sources: { fleet: { state: "off" } }, complete: true },
          }),
          { status: 200 },
        ),
      );
    }
    if (path === "/fleet/sessions/live") {
      const sessions = (opts.sessions ?? []).map((session_id) => ({ session_id }));
      return Promise.resolve(
        new Response(JSON.stringify({ sessions, meta: { sources: { fleet: { state: "ok" } }, complete: true } }), { status: 200 }),
      );
    }
    if (path === "/machine/specs") {
      if (opts.specs === undefined) return Promise.resolve(new Response("{}", { status: 404 }));
      return Promise.resolve(new Response(JSON.stringify(opts.specs), { status: 200 }));
    }
    if (path === "/runs") {
      if (opts.runs === undefined) return Promise.resolve(new Response("not recorded\n", { status: 404 }));
      return Promise.resolve(new Response(JSON.stringify({ runs: opts.runs, generated_at_ms: 1 }), { status: 200 }));
    }
    if (path === "/fleet/roster") {
      return Promise.resolve(
        new Response(JSON.stringify({ machines: opts.roster ?? [], error: opts.rosterError ?? null }), { status: 200 }),
      );
    }
    return Promise.resolve(new Response("not recorded\n", { status: 404 }));
  }
}

/** (#2958) A promise and the function that resolves it. */
function gate(): { promise: Promise<void>; open: () => void } {
  let open = () => {};
  const promise = new Promise<void>((resolve) => {
    open = resolve;
  });
  return { promise, open };
}

describe("FleetLens: presence is a fact about now", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  // A run silent for two hours whose session presence still beats: at the
  // live edge presence holds it running; with the playhead parked an hour
  // in, the same card judges from the records alone.
  const now = Date.parse(`${todayUTC()}T12:00:00.000Z`);
  const start = now - 2 * 3_600_000;
  const records = [{ ts: new Date(start).toISOString(), action: "dispatch.start", session_id: "s1", machine_uid: "u1", machine_id: "MacBook-Pro", handle: "coder" }];

  async function cardText(playhead?: number): Promise<string> {
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(now);
    mockFleetFetch({ flowToday: records, sessions: ["s1"] });
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens(playhead === undefined ? {} : { playhead }, qc);
    await waitFor(() => expect(qc.getQueryState(queryKeys.fleetSessionsLive())?.status).toBe("success"));
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    return document.querySelector(".mach")!.textContent ?? "";
  }

  it("at the live edge presence holds the silent run running", async () => {
    await cardText();
    await waitFor(() => expect(document.querySelector(".mach")!.textContent).toContain("1 running"));
  });

  it("a scrubbed playhead on a live day judges from records alone, however presence reads now", async () => {
    const text = await cardText(start + 3_600_000);
    expect(text).not.toContain("1 running");
  });
});

describe("FleetLens", () => {
  it("always renders the hero, even at zero — never hides it while there's no data yet", async () => {
    mockFleetFetch();
    renderFleetLens();
    await waitFor(() => expect(screen.getByText(/tokens · last/i)).toBeInTheDocument());
    // Two "0" values (local + cloud tokens) render rather than the card
    // disappearing — the "hides late, pops in" defect this port guards
    // against (see `SavingsHero`'s own doc).
    expect(screen.getByText("darkmux tokens")).toBeInTheDocument(); // the section eyebrow
    // (#2834) The cloud tile is withdrawn; one figure is the hero now.
  });

  // (#2902) CACHED is a share of INPUT and UTILITY a share of ALL TOKENS, so
  // each renders as a part line under the figure it belongs to. As peer chips
  // they read as extra buckets to add, which double-counts.
  it("(#2902) renders cached under input and utility under all tokens, never as peer chips", async () => {
    const today = todayUTC();
    const usage = (ts: string, payload: Record<string, unknown>) => ({
      ts: `${today}T${ts}.000Z`, machine_uid: "u1", session_id: "s1", category: "telemetry", source: "tokens", action: "telemetry.tokens", payload,
    });
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        usage("10:00:05", { call_kind: "turn", purpose: "work", prompt_tokens: 1920, completion_tokens: 163, total_tokens: 2083, cached_tokens: 140 }),
        usage("10:00:09", { call_kind: "compaction", purpose: "utility", prompt_tokens: 130, completion_tokens: 15, total_tokens: 145 }),
        { ts: `${today}T10:01:00.000Z`, machine_uid: "u1", session_id: "s1", action: "dispatch.complete", payload: {} },
      ],
    });
    const { container } = renderFleetLens();
    const part = (sel: string) => Array.from(container.querySelectorAll(sel)).map((e) => e.textContent);
    await waitFor(() => expect(part(".savc .savpart")).toEqual(["140 cached"]));
    expect(part(".savlead .savpart")).toEqual(["145 utility"]);
    // The part lines sit with their parents: cached inside the INPUT chip,
    // utility beside the ALL TOKENS label. The figure is its own span so only
    // the word is uppercased ("17.62k utility", not "17.62K").
    const cached = container.querySelector(".savc .savpart")!;
    expect(cached.closest(".savc")?.querySelector(".scl")?.textContent).toBe("input");
    expect(cached.querySelector(".savpartv")?.textContent).toBe("140");
    expect(container.querySelector(".savlead .savpart .savpartv")?.textContent).toBe("145");
    // No chip is labeled cached or utility on its own.
    const labels = Array.from(container.querySelectorAll(".savc .scl")).map((e) => e.textContent);
    expect(labels).not.toContain("cached");
    expect(labels).not.toContain("utility");
  });

  // (#2902 review) The inverse: with no utility spend and no record reporting
  // `cached_tokens`, NO part line renders — not "0 utility", not "0 cached".
  // Before this test only the parity golden guarded it; `{t.utility &&
  // settled ?` mutated to `{settled ?` left vitest green.
  it("(#2902) with utility 0 and no cached_tokens reported, no part line renders at all", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        {
          ts: `${today}T10:00:05.000Z`, machine_uid: "u1", session_id: "s1", category: "telemetry", source: "tokens", action: "telemetry.tokens",
          payload: { call_kind: "turn", purpose: "work", prompt_tokens: 1920, completion_tokens: 163, total_tokens: 2083 },
        },
        { ts: `${today}T10:01:00.000Z`, machine_uid: "u1", session_id: "s1", action: "dispatch.complete", payload: {} },
      ],
    });
    const { container } = renderFleetLens();
    // Wait for the window to settle (the figures are silhouetted until then,
    // and no part line renders while unsettled either — the assertion below
    // must run against the SETTLED hero to mean anything).
    await waitFor(() => expect(container.querySelector(".savings")?.getAttribute("data-settled")).toBe("true"));
    expect(container.querySelector(".savc .scv")?.textContent).toBe("1.92k");
    expect(container.querySelectorAll(".savpart")).toHaveLength(0);
  });

  it("sums a locally-run session's telemetry into local tokens, and renders its machine card", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        {
          ts: `${today}T10:00:05.000Z`,
          machine_uid: "u1",
          session_id: "s1",
          category: "telemetry",
          source: "tokens",
          payload: { turn_seq: 1, prompt_tokens: 500, completion_tokens: 100, total_tokens: 600 },
        },
        { ts: `${today}T10:01:00.000Z`, machine_uid: "u1", session_id: "s1", action: "dispatch.complete", payload: { total_tokens: 600 } },
      ],
    });
    renderFleetLens();
    await waitFor(() => expect(screen.getByText("600")).toBeInTheDocument()); // fmtN(600) = "600" local tokens
    // Renders twice: the machine card AND the activity-timeline lane label.
    expect(screen.getAllByText("MacBook-Pro").length).toBeGreaterThanOrEqual(2);
    expect(screen.getByText("idle")).toBeInTheDocument(); // no live session -> idle, not "dispatch in flight"
  });

  // (#2060) The rendered-DOM twin of `cards.test.ts`'s pure-function coverage
  // — a mission's own top-level session (`session_id === mission_id`) and
  // its one live seat dispatch (same `mission_id`, its own `session_id`)
  // must read as ONE running run on the actual card, not two.
  it("(#2060) a running mission with one live seat renders '1 running', not '2 running'", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        // (#2886 pass 5) The mission's own run session opens on `run.start`
        // (`mission_launch.rs::run_bookend_record`), which is what
        // `liveExecutions` reads to exclude this session from the pager's
        // `card.executions` (see that function's own doc). As an execution,
        // this fixture read as TWO genuine executions sharing one collapsed
        // run — a real mismatch, correctly triggering #2881's "N run(s) · M
        // executions" wording, not the bug this older test predates and was
        // never about.
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "mission-1", mission_id: "mission-1", action: "run.start" },
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "seat-1", mission_id: "mission-1", action: "dispatch.start" },
      ],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("1 running");
    expect(card.textContent).not.toContain("2 running");
  });

  // (#1923) A machine whose lab run is BETWEEN dispatches — the COW clone,
  // the baseline hash, the verify command, scoring — has no dispatch in
  // flight, so no contract-2 bookends and no presence key: flow sees nothing
  // and `machActive` has no run to read. The `/runs` lab row
  // (written at start, RAII-guarded) is the only source that stays "running"
  // across that whole span, and without it the card reads "idle" /
  // "0 running" while the run is very much live.
  it("(#1923) a running lab run with zero flow activity still renders 'dispatch in flight'", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.now() }],
      runs: [{ id: "lab-1", kind: "lab", status: "running", machine: "MacBook-Pro", tracked: true }],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("dispatch in flight");
    expect(card.textContent).toContain("1 running");
    expect(card.textContent).not.toContain("idle");
  });

  // (#1923 review) The rendered-DOM twin of `cards.test.ts`'s double-count
  // case. A lab run's DISPATCH phase DOES ride the flow stream — the
  // provider calls `darkmux_crew::dispatch::dispatch`, which emits the
  // contract-2 bookends and spawns the session-presence emitter — so both
  // sources see the same one run. The card must say "1 running".
  it("(#1923) a lab run whose dispatch is live reads '1 running', not '2 running'", async () => {
    const today = todayUTC();
    const labSession = "darkmux-coding-long-agentic-1756000000000";
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.now() }],
      flowToday: [
        {
          ts: `${today}T10:00:00.000Z`,
          machine_uid: "u1",
          machine_id: "MacBook-Pro",
          session_id: labSession,
          action: "dispatch.start",
          handle: "coder",
        },
      ],
      runs: [{ id: "long-agentic-balanced-1756000000-1", kind: "lab", status: "running", machine: "MacBook-Pro", tracked: true }],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    await waitFor(() => expect(card.textContent).toContain("running"));
    expect(card.textContent).toContain("1 running");
    expect(card.textContent).not.toContain("2 running");
  });

  // (#1923 review) A `/runs` that cannot be read yields the SAME empty list
  // a healthy idle machine does, so the cards silently return to the exact
  // "idle while a lab run is live" reading #1923 removed. The lens has to
  // name the failure rather than render the absence as data.
  it("(#1923) says so when the /runs read fails, instead of asserting 'no lab runs'", async () => {
    // `runs` omitted -> the mock's 404, which is what a daemon whose `/runs`
    // 500s or times out looks like to `fetchJson` (`ok: false`).
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.now() }],
    });
    const { container } = renderFleetLens();
    await waitFor(() => expect(container.querySelector('.fleetcov[data-state="runs-unreadable"]')).toBeTruthy());
    expect(screen.getByText(/Run records are unavailable/i)).toBeInTheDocument();
    // The count still renders (the `?? []` fallback keeps the lens alive) —
    // the notice is what stops it being read as a confident zero.
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(document.querySelector(".mach")!.textContent).toContain("0 running");
  });

  // The INVERTED case, and the one that proves the notice is conditional: a
  // `/runs` that answers cleanly must stay silent. Without it, a notice
  // hardwired on would pass the test above.
  it("(#1923) stays silent when /runs answers cleanly, even with no runs at all", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.now() }],
      runs: [],
    });
    const { container } = renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(container.querySelector('.fleetcov[data-state="runs-unreadable"]')).toBeNull();
  });

  // The second inverted case: a REPLAY never reads `machineRuns` at all
  // (`buildFleetCard` gates the lab count on `liveMode`), so a failed
  // `/runs` costs a replayed day nothing and warning about it would be the
  // bug — the same historical gate `FleetCoverageNotice` already carries.
  //
  // Both halves render against ONE `QueryClient` on purpose. A fresh client
  // makes the replay assertion vacuous: the absence is satisfied by the
  // `/runs` query simply not having settled yet, so the test passes with
  // the gate deleted. Rendering the LIVE lens first and waiting for its
  // notice proves the failed read is already in the cache; the replay
  // render then starts from that settled failure, and its silence is the
  // gate's doing rather than a race.
  it("(#1923) stays silent on a replay, where the lab count is never read", async () => {
    const today = todayUTC();
    // `runs` omitted -> the same 404 the live case above warns about.
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.now() }],
    });
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const live = renderFleetLens({}, qc);
    await waitFor(() => expect(live.container.querySelector('.fleetcov[data-state="runs-unreadable"]')).toBeTruthy());
    live.unmount();

    const records: NormRecord[] = normAll([
      { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      { ts: `${today}T10:01:00.000Z`, machine_uid: "u1", session_id: "s1", action: "dispatch.complete" },
    ]);
    const { container } = renderFleetLens(
      {
        records,
        tMax: Date.parse(`${today}T10:01:00.000Z`),
        tMin: Date.parse(`${today}T10:00:00.000Z`),
        historical: true,
      },
      qc,
    );
    await waitFor(() => expect(container.querySelector(".mach")).not.toBeNull());
    expect(container.querySelector('.fleetcov[data-state="runs-unreadable"]')).toBeNull();
  });

  it("(#2068) the unattributed tile is ALWAYS in the hero, dimmed at zero, so a dispatch starting or finishing never re-flows the page", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        {
          ts: `${today}T10:00:05.000Z`,
          machine_uid: "u1",
          session_id: "s-local",
          action: "dispatch.start",
          category: "work",
          source: "crew",
          payload: { role: "coder", endpoint: "local" },
        },
      ],
    });
    renderFleetLens();
    // (#2830) Wait for SETTLEMENT, not for a label. `"darkmux tokens"` renders
    // in both states, so the original anchor let the figure assertion below
    // run against an unsettled hero. That was invisible while `.savnum`
    // rendered "0" in both states; now that the figure is withheld until
    // settled, the incidental anchor reads "" instead. Anchoring on
    // `data-settled="true"` asserts the state this test always meant --
    // `settled && 0` is a literal "0" (see `SavingsHero`'s own doc).
    await waitFor(() =>
      expect(document.querySelector('.savings[data-settled="true"]')).toBeTruthy(),
    );
    // (#2834) There is one tile now, so #2068's concern is satisfied by
    // construction rather than by remembering to render a zero: with nothing
    // to appear or vanish, an in-flight dispatch cannot change the hero's
    // geometry. The property is still asserted — exactly one lead tile,
    // present and settled — because "one tile" is the thing that makes the
    // reflow impossible, and a future split would silently reintroduce it.
    const tiles = document.querySelectorAll(".savings .savlead");
    expect(tiles).toHaveLength(1);
    expect(screen.getByText("darkmux tokens")).toBeInTheDocument(); // the section eyebrow
    expect(tiles[0].querySelector(".savnum")!.textContent).toBe("0");
  });

  /** (U5-1) The gap the `historical` test below could not see: `App.tsx`
   * renders `<FleetLens />` with NO props, so `historical` sits at its
   * default `false` and the live-only endpoints fired on the STATIC
   * demo — measured on the served build, `#lens=fleet` produced 404s for
   * `/fleet/machines/live`, `/fleet/sessions/live` and `/machine/specs`
   * plus their console errors. The prop describes the CALLER's intent (a
   * replay); only the BUILD can answer "is there a daemon at all" — the
   * #1801 rule `MachineLens`/`useFlowWindow`/`route.ts::isLiveRoute`
   * already follow. Rendered here exactly as `App.tsx` renders it: propless.
   *
   * (#1855 follow-up, CONSIDER 6) `/fleet/roster` joined this list without
   * a DEDICATED gate test of its own — `useFleetRoster(livePolling)` was
   * asserted three times in a row by comment
   * (`FleetLens.tsx`'s own doc on `roster`, `rosterOnly`, and the `cards`
   * useMemo) but never once by a test that would actually catch the gate
   * being dropped. Mutating `useFleetRoster(livePolling)` to
   * `useFleetRoster(true)` left the WHOLE suite green before this line was
   * added — this endpoint is folded into the same list this test already
   * polices, closing that gap the same way the presence endpoints are
   * already covered rather than inventing a second test for one more URL. */
  it("(U5-1) on a static build the propless FleetLens App renders never calls a live-only endpoint", async () => {
    const meta = document.createElement("meta");
    meta.name = "darkmux-flow-src";
    meta.content = "./demo-flow.jsonl";
    document.head.appendChild(meta);
    const seen: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        seen.push(String(url));
        return Promise.resolve(new Response("[]", { status: 200 }));
      }),
    );
    try {
      const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
      render(
        <QueryClientProvider client={queryClient}>
          <FleetLens />
        </QueryClientProvider>,
      );
      await waitFor(() => expect(document.querySelector(".savrow")).not.toBeNull());
      // Give every gated query a chance to fire before asserting none did
      // (a real timer — `vi.useFakeTimers` above fakes `Date` only).
      await new Promise((r) => setTimeout(r, 50));
      expect(
        seen.filter(
          (p) => p === "/fleet/machines/live" || p === "/fleet/sessions/live" || p === "/machine/specs" || p === "/fleet/roster",
        ),
      ).toEqual([]);
    } finally {
      document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((m) => m.remove());
    }
  });

  it("(#2067) on a static build the card's hardware line comes from the committed fleet snapshot, never from a daemon route", async () => {
    for (const [name, content] of [
      ["darkmux-flow-src", "./demo-flow.jsonl"],
      ["darkmux-fleet-src", "./demo-fleet.json"],
    ]) {
      const meta = document.createElement("meta");
      meta.name = name;
      meta.content = content;
      document.head.appendChild(meta);
    }
    const seen: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        const path = String(url);
        seen.push(path);
        if (path === "./demo-fleet.json") {
          return Promise.resolve(
            new Response(
              JSON.stringify({
                machines: [{ machine_uid: "u1", display_name: "m5-ultra-256gb", specs: "Apple M5 Ultra · 256 GB", beat_ts_ms: 1 }],
                meta: { sources: { fleet: { state: "ok" } }, complete: true },
              }),
              { status: 200 },
            ),
          );
        }
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
    try {
      const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
      const records = normAll([
        { ts: "2026-08-26T10:00:00.000Z", machine_uid: "u1", machine_id: "m5-ultra-256gb", action: "machine.online", source: "presence_reconciler" },
      ]);
      render(
        <QueryClientProvider client={queryClient}>
          <FleetLens records={records} tMax={Date.parse("2026-08-26T10:00:00.000Z")} historical />
        </QueryClientProvider>,
      );
      await waitFor(() => expect(screen.getByText("Apple M5 Ultra · 256 GB")).toBeInTheDocument());
      expect(screen.queryByText("hardware not reported")).not.toBeInTheDocument();
      expect(seen.filter((p) => p === "/fleet/machines/live" || p === "/machine/specs")).toEqual([]);
    } finally {
      document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((m) => m.remove());
    }
  });

  it("(#2890) a replay's activity window defaults to \"all\", the recording's own span", async () => {
    // A 34-minute recording under the 24h default was a sliver at the right
    // edge of the timeline (operator, 2026-09-25: "I'm spending most of my
    // time watching nothing happen").
    const mk = (ts: string, action: string) => norm({ ts, machine_uid: "u1", machine_id: "m5", session_id: "s1", action });
    const records = [mk("2026-08-26T10:00:00.000Z", "dispatch.start"), mk("2026-08-26T10:34:00.000Z", "dispatch.complete")];
    renderFleetLens({ records, tMin: Date.parse("2026-08-26T10:00:00.000Z"), tMax: Date.parse("2026-08-26T10:34:00.000Z"), historical: true });
    await waitFor(() => expect(document.querySelector(".twinb.on")?.textContent).toBe("all"));
  });

  it("(#2890) a picked preset replaces \"all\"; live has no \"all\" and keeps 24h", async () => {
    const mk = (ts: string, action: string) => norm({ ts, machine_uid: "u1", machine_id: "m5", session_id: "s1", action });
    const records = [mk("2026-08-26T01:00:00.000Z", "dispatch.start"), mk("2026-08-26T09:00:00.000Z", "dispatch.complete")];
    const r = renderFleetLens({ records, tMin: Date.parse("2026-08-26T01:00:00.000Z"), tMax: Date.parse("2026-08-26T09:00:00.000Z"), historical: true });
    await waitFor(() => expect(document.querySelector(".twinb.on")?.textContent).toBe("all"));
    fireEvent.click(screen.getByRole("button", { name: "1h" }));
    expect(document.querySelector(".twinb.on")?.textContent).toBe("1h");
    r.unmount();
    // Same records, not a replay: no "all", and the live 24h default.
    renderFleetLens({ records, tMin: Date.parse("2026-08-26T01:00:00.000Z"), tMax: Date.parse("2026-08-26T09:00:00.000Z") });
    await waitFor(() => expect(document.querySelector(".twinb.on")?.textContent).toBe("24h"));
    expect([...document.querySelectorAll(".twinb")].map((b) => b.textContent)).not.toContain("all");
  });

  it("(#2834) a session whose endpoint is unknown still counts: darkmux dispatched those tokens either way", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        {
          ts: `${today}T10:00:05.000Z`,
          machine_uid: "u1",
          session_id: "s-orphan",
          category: "telemetry",
          source: "tokens",
          payload: { turn_seq: 1, prompt_tokens: 900, completion_tokens: 100, total_tokens: 1000 },
        },
      ],
    });
    renderFleetLens();
    // (#2834) This session has telemetry but no dispatch bookend, so nothing
    // names its endpoint. It used to be quarantined in an "unattributed"
    // tile and EXCLUDED from the figure, so the hero would not imply the
    // tokens had run free.
    //
    // That quarantine only made sense while the figure claimed locality. It
    // no longer does: "darkmux tokens" is true of these tokens whether the
    // model ran on this machine, a fleet peer, or a hosted endpoint — which
    // is precisely why the claim was narrowed to one darkmux can actually
    // make. So they are counted, and nothing about where they ran is
    // asserted anywhere in the hero.
    await waitFor(() => expect(screen.getByText("1,000")).toBeInTheDocument());
    expect(document.querySelector(".savings .savnum")?.textContent).toBe("1,000");
    expect(document.querySelectorAll(".savings .savlead")).toHaveLength(1);
    // The withdrawn vocabulary must not survive anywhere in the hero.
    const hero = document.querySelector(".savings")!.textContent ?? "";
    for (const claim of ["local tokens", "cloud", "unattributed"]) {
      expect(hero.toLowerCase()).not.toContain(claim);
    }
  });

  // (drill-in packet, split by locality in #1809) The fleet-card click —
  // `data-act="machine" data-arg` in legacy (`ACTIONS.machine`,
  // `drillMachine(uid)`) — was previously a plain, non-interactive `<div>`,
  // and every card drilled to the SAME destination. #1809 splits that
  // destination by locality (see `FleetLens.tsx`'s own `machineDrillHash`
  // doc): the LOCAL card (confirmed against this daemon's OWN
  // `/machine/specs`) still reaches the residency room; anything this
  // daemon can't confirm as itself — including, but not limited to, a
  // genuinely remote machine — goes to the runs lens instead, pinned to
  // that machine. `MachineLens.test.tsx` covers what the residency-room
  // destination does with a REMOTE uid reached by a DIRECT deep-link
  // (still a real, supported route — see that file's own doc); these two
  // tests below are the LOCAL half. The REMOTE half — what a click on a
  // card this daemon can't confirm as itself actually does — is its own
  // test further down, the inverted case.
  it("clicking the LOCAL machine card navigates to the residency room via a real hash write", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      // This daemon's own /machine/specs identifies it AS the u1 machine —
      // the confirmed-local signal `localMachineUid` resolves against.
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    renderFleetLens();
    // "MacBook-Pro" renders TWICE — the machine card AND the activity-
    // timeline lane label (see the earlier test's own comment) — find the
    // CARD specifically via its ancestor class, not the singular query.
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("MacBook-Pro");
    expect(card).toHaveAttribute("role", "button");
    fireEvent.click(card);
    expect(window.location.hash).toBe("#lens=runs&machine=MacBook-Pro");
  });

  it("Enter/Space also activates the LOCAL fleet-card drill-in (keyboard parity with the click)", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    renderFleetLens();
    // "MacBook-Pro" renders TWICE — the machine card AND the activity-
    // timeline lane label (see the earlier test's own comment) — find the
    // CARD specifically via its ancestor class, not the singular query.
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("MacBook-Pro");
    fireEvent.keyDown(card, { key: "Enter" });
    expect(window.location.hash).toBe("#lens=runs&machine=MacBook-Pro");
  });

  // (#1809, merge-gate fix) The runs lens is for a machine POSITIVELY known
  // to be remote — nothing else. An earlier cut sent every unconfirmed card
  // there, which measured wrong on the first paint: `localUid` is null until
  // `/machine/specs` resolves, so the LOCAL card was clickable-and-wrong for
  // one frame (+0ms → runs, +100ms → machine). These two tests pin both
  // sides of the corrected rule, and the second is what stops the first from
  // being vacuous — without a confirmed-remote case, `machineDrillHash`
  // could always return the machine hash and every other test here would
  // still pass.
  it("an UNCONFIRMED card goes to the residency room — the destination that admits it is guessing", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      // No `specs` — locality unresolved, exactly the first-paint state.
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    fireEvent.click(document.querySelector(".mach")!);
    // The point of the single destination: an UNCONFIRMED card and a
    // confirmed one now agree, so there is no frame in which this card points
    // somewhere it will not point once `/machine/specs` resolves. That
    // flicker is what the old locality branch was working around.
    expect(window.location.hash).toBe("#lens=runs&machine=MacBook-Pro");
  });

  it("a CONFIRMED-REMOTE card goes to the runs lens, pinned to that machine", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        { ts: `${today}T10:00:01.000Z`, machine_uid: "u2", machine_id: "studio", session_id: "s2", action: "dispatch.start", handle: "coder" },
      ],
      // specs names THIS daemon as MacBook-Pro, so u2 is positively remote.
      specs: { machine_id: "MacBook-Pro" },
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelectorAll(".mach").length).toBe(2));
    const studio = [...document.querySelectorAll(".mach")].find((c) => c.textContent?.includes("studio"))!;
    fireEvent.click(studio);
    expect(window.location.hash).toBe("#lens=runs&machine=studio");
  });

  it("the savings hero renders tokens-only — no currency symbol or rate figure, even with non-zero savings (#803 regression coverage, restored post-#1806)", async () => {
    // Legacy's equivalent coverage
    // (`savings_hero_breakdown_is_classed_and_currency_free`, a source-text
    // scan of `viewer.html`'s savings-hero..`renderFleet` region) retired
    // with that file. This exercises the RENDERED hero instead — a fixture
    // with real, non-zero local tokens, so the assertion isn't vacuously
    // true against an empty "0" hero.
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        {
          ts: `${today}T10:00:05.000Z`,
          machine_uid: "u1",
          session_id: "s1",
          category: "telemetry",
          source: "tokens",
          payload: { turn_seq: 1, prompt_tokens: 500, completion_tokens: 100, total_tokens: 600 },
        },
        { ts: `${today}T10:01:00.000Z`, machine_uid: "u1", session_id: "s1", action: "dispatch.complete", payload: { total_tokens: 600 } },
      ],
    });
    const { container } = renderFleetLens();
    await waitFor(() => expect(screen.getByText("600")).toBeInTheDocument());
    const hero = container.querySelector(".savings");
    expect(hero).not.toBeNull();
    expect(hero!.textContent).not.toMatch(/[$€£]|USD|per million|\/M\b/);
  });

  // (#1869) The playback transport (`Scrubber`, rewind/play/speed/range)
  // only ever mounts inside `PlaybackLens`, which composes THIS component
  // rather than the other way around — `FleetLens` itself never renders it,
  // on any route. This is the live (default, no-hash) route's own coverage
  // of that: no `records`/`tMax`/`tMin`/`historical` props at all, the same
  // call every OTHER test in this file already makes.
  it("never renders a playback transport on the live route — the scrubber is PlaybackLens-only", async () => {
    mockFleetFetch();
    renderFleetLens();
    await waitFor(() => expect(screen.getByText(/tokens · last/i)).toBeInTheDocument());
    expect(document.querySelector(".scrub")).toBeNull();
    expect(screen.queryByRole("slider")).not.toBeInTheDocument();
  });

  // (#1869, QA gate — caught against a real daemon, not by any prior test)
  // `tMax` (the day's FIXED ceiling, used for the activity axis span) and
  // `playhead` (the scrub position) are two separate props now. A first
  // cut passed only one number for both, which was invisible in every unit
  // test above because none of them separate the two — this is the
  // integration-level regression test that does. Scrubbing all the way
  // back to `tMin` must NOT collapse the "ACTIVITY" axis header down to a
  // single repeated instant; the day's whole recorded span stays on
  // screen, with only the playhead marker (and the token hero, and the bar
  // classes) moving.
  // (#1903) The running COUNT is what the operator actually reads on a
  // fleet card ("N running"), and until now it carried no tap target of its
  // own — a click anywhere on the card, count included, fell through to
  // `machineDrillHash` and landed on the residency room. These three tests
  // pin the count's own destination, distinct from the card body's, without
  // touching `machineDrillHash` itself (covered above, unchanged).
  it("(#1903) tapping the running count with 2 live sessions navigates to the runs lens pinned to the machine; the card body still goes to the residency room", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.start", handle: "coder" },
      ],
      machines: [
        { uid: "u1", name: "MacBook-Pro", last_seen_ms: Date.now() },
      ],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("2 running");

    const countEl = card.querySelector(".runs--live")!;
    expect(countEl).not.toBeNull();
    fireEvent.click(countEl);
    expect(window.location.hash).toBe("#lens=runs&machine=MacBook-Pro");

    // The card BODY now reaches the SAME destination as the count. Before
    // 2026-08-23 it went to the residency room on a local card; the operator
    // asked for one destination ("clicking a machine ... should go to the
    // runs tab with a filter by machine"), which also removed the first-paint
    // flicker the locality branch existed to make harmless.
    window.location.hash = "";
    fireEvent.click(card.querySelector(".name")!);
    expect(window.location.hash).toBe("#lens=runs&machine=MacBook-Pro");
  });

  it("(#1903) tapping the running count with exactly 1 live session opens that run's session drill directly", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      machines: [{ uid: "u1", name: "MacBook-Pro", last_seen_ms: Date.now() }],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("1 running");
    fireEvent.click(card.querySelector(".runs--live")!);
    expect(window.location.hash).toBe("#dispatch=s1");
  });

  it("(#1903) the running count is keyboard operable and carries an accessible name", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      machines: [{ uid: "u1", name: "MacBook-Pro", last_seen_ms: Date.now() }],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const countEl = document.querySelector(".runs--live")!;
    expect(countEl).toHaveAttribute("role", "button");
    expect(countEl).toHaveAttribute("tabIndex", "0");
    expect(countEl.getAttribute("aria-label")).toBeTruthy();
    fireEvent.keyDown(countEl, { key: "Enter" });
    expect(window.location.hash).toBe("#dispatch=s1");
  });

  // (#1913) The two tests below pin BOTH sides of the `DEFAULT_POLICY.staleAfterMs`
  // boundary explicitly, rather than relying on the other tests in this
  // file happening to sit comfortably inside it. Before this fix neither
  // direction was asserted: a session's liveness was implicitly "whatever
  // real wall-clock now happened to be" relative to a `T10:00` fixture.
  it("(#1913) a session 1s under the DEFAULT_POLICY.staleAfterMs boundary still reads 1 running", async () => {
    const today = todayUTC();
    const lastRecordMs = Date.parse(`${today}T10:00:00.000Z`);
    mockFleetFetch({
      flowToday: [
        { ts: new Date(lastRecordMs).toISOString(), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      machines: [{ uid: "u1", name: "MacBook-Pro", last_seen_ms: lastRecordMs }],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    vi.setSystemTime(new Date(lastRecordMs + DEFAULT_POLICY.staleAfterMs - 1000));
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("1 running");
    expect(card.querySelector(".runs--live")).not.toBeNull();
  });

  it("(#1913) a session 1s past the DEFAULT_POLICY.staleAfterMs boundary reads 0 running, not stuck live", async () => {
    const today = todayUTC();
    const lastRecordMs = Date.parse(`${today}T10:00:00.000Z`);
    mockFleetFetch({
      flowToday: [
        { ts: new Date(lastRecordMs).toISOString(), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      machines: [{ uid: "u1", name: "MacBook-Pro", last_seen_ms: lastRecordMs }],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    vi.setSystemTime(new Date(lastRecordMs + DEFAULT_POLICY.staleAfterMs + 1000));
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("0 running");
    // No live tap target once nothing is running (`machineRunsHash` returns
    // null), and the card body itself reads idle, not "dispatch in flight".
    expect(card.querySelector(".runs--live")).toBeNull();
    expect(card.textContent).toContain("idle");
  });

  // (#1903 QA fix) Was: the card body (`role="button"`) had no explicit
  // `aria-label`, so its computed accessible name absorbed ALL descendant
  // text — including the nested running-count button's own `aria-label`
  // ("open the 2 running dispatches on MacBook-Pro"), per ARIA's
  // presentational-children rule for a `button` descendant. The card
  // announced as "MacBook-Pro Apple M5 Max dispatch in flight open the 2
  // running dispatches on MacBook-Pro" instead of just its own name.
  // Nesting one interactive control inside another is an accepted,
  // documented exception here (the count needed its own tap target without
  // moving the card body's destination) — this pins that the OUTER card's
  // name stays deterministic despite the nesting.
  it("(#1903 QA fix) the card's own accessible name stays just its machine name, not polluted by the nested running-count button's aria-label", async () => {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.start", handle: "coder" },
      ],
      machines: [{ uid: "u1", name: "MacBook-Pro", last_seen_ms: Date.now() }],
      specs: { machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max" },
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card).toHaveAccessibleName("MacBook-Pro");
  });

  // (Playback parity, Change A, finding #8 — 2026-09-24, operator decision)
  // This used to assert the OLD "replay draws the day's own fixed span"
  // behavior directly: scrubbing back moved the hero/bars but the
  // TIMELINE'S AXIS stayed pinned to the day's whole recorded range. That
  // premise is retired by design now — a replay draws the SAME rolling
  // window as live, anchored at the playhead, so scrubbing back MOVES the
  // whole window with it, exactly like a live viewer watching a session
  // recede out of a rolling window as time passes. The header text itself
  // (always "recent activity" now) is unaffected either way — this test is
  // rewritten to check the axis TIMES and the playhead marker position,
  // which are what actually move.
  it("a scrubbed playhead MOVES the rolling activity window with it (parity — not the old fixed day-span)", async () => {
    const today = todayUTC();
    const dayTMin = Date.parse(`${today}T10:00:00.000Z`);
    const dayTMax = Date.parse(`${today}T12:00:00.000Z`);
    const records = normAll([
      { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      { ts: `${today}T12:00:00.000Z`, machine_uid: "u1", session_id: "s1", action: "dispatch.complete", payload: { total_tokens: 600 } },
    ]);

    const { rerender } = render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={records} tMax={dayTMax} tMin={dayTMin} playhead={dayTMax} historical />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".fleettl")).not.toBeNull());
    // The header LABEL never changes (always "recent activity" now — see
    // this module's own doc); read the AXIS TIMES off the DOM instead
    // (rather than asserting a literal — `clkhm` renders in the runner's
    // local timezone), which is what actually moves with the playhead.
    expect(document.querySelector(".tlhdr span")!.textContent).toBe("recent activity");
    // (#2890) A replay now opens on "all" (the recording's fixed span); the
    // rolling window this test pins is what any picked preset does.
    fireEvent.click(screen.getByRole("button", { name: "24h" }));
    const axisBefore = [...document.querySelectorAll(".tlaxis span")].map((e) => e.textContent);
    expect(axisBefore.every(Boolean)).toBe(true);
    // The playhead marker sits at the window's own right edge at rest.
    expect((document.querySelector(".ph") as HTMLElement).style.left).toBe("100%");

    rerender(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={records} tMax={dayTMax} tMin={dayTMin} playhead={dayTMin} historical />
      </QueryClientProvider>,
    );

    // The rolling window MOVED with the scrub — the axis times are now
    // different (centered on `dayTMin`, not `dayTMax`), not byte-identical
    // to before. This is the parity behavior: the same thing scrubbing
    // live would do to its own rolling window.
    const axisAfter = [...document.querySelectorAll(".tlaxis span")].map((e) => e.textContent);
    expect(axisAfter).not.toEqual(axisBefore);
    // The playhead marker still sits at the window's own right edge —
    // scrubbing moves the WINDOW, not the marker's position within it.
    expect((document.querySelector(".ph") as HTMLElement).style.left).toBe("100%");
    // The hero moved too — the completion is no longer visible at tMin.
    expect(document.querySelector(".savings .savnum")?.textContent).toBe("0");
  });
});

// (#2881) The fleet card pager. Uses the `records`/`historical` render path
// directly (same as the scrubbed-playhead test above), not `mockFleetFetch`
// — the pager reads only `buildFleetCard`'s output, which this path drives
// with no separate live/playback branch to mock around.
describe("FleetLens pager (#2881)", () => {
  const D0 = Date.parse("2026-08-26T10:00:00.000Z");
  const at = (sec: number) => new Date(D0 + sec * 1000).toISOString();

  // s1: CODER, generating (~100 tok/s, the two heartbeats 2s apart).
  // s2: REVIEWER, resting (a dispatch.rest 15s window opened at 3s).
  // s3: FETCH-RENDER, prompt (a bare dispatch.start, no heartbeat yet).
  const threeExecutionRecords: NormRecord[] = normAll([
    { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
    { ts: at(0), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 0 } },
    { ts: at(2), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 2000, generated_chars: 800 } },
    { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.start", handle: "reviewer" },
    { ts: at(3), machine_uid: "u1", session_id: "s2", action: "dispatch.rest", payload: { ms: 15_000 } },
    { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s3", action: "dispatch.start", handle: "fetch-render" },
  ]);

  function renderThree(playheadSec: number) {
    return render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={threeExecutionRecords} tMax={D0 + playheadSec * 1000} tMin={D0} playhead={D0 + playheadSec * 1000} historical />
      </QueryClientProvider>,
    );
  }

  it("defaults to the busiest (generating) execution's own tube, role and rate — not the machine aggregate", async () => {
    renderThree(5);
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    expect(document.querySelector(".mach-scope__pager-n")!.textContent).toBe("1/3");
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("coder");
    // The rate line shows s1's OWN reading (100 tok/s), not the machine's
    // summed total (also 100 here, since only s1 is generating — see the
    // next assertion for where the total actually shows up).
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("100 tok/s");
    // (#2886 pass 5, MUST — fresh-reviewer finding F5) Pin what the TUBE
    // component itself receives, not just the neighboring text — a bug that
    // hits only the tube's own props (e.g. still reading the machine
    // aggregate) would leave every text assertion in this file green.
    expect(latestTokenScopeProps()).toMatchObject({ tokensPerSec: 100, state: "generating" });
    // (#2881) "the machine total moves to the count line" — no separate
    // "all" page.
    expect(document.querySelector(".runs--live")!.textContent).toBe("3 running · 100 tok/s");
  });

  // (#2886 pass 5, MUST — fresh-reviewer finding F2) A mission's seats all
  // collapse to ONE top-level run (`topLevelRuns`), but the pager
  // shows one page per seat — so `runsCount` (1) and `card.executions.length`
  // (9) genuinely disagree here, unlike the plain-dispatches case above
  // where they agree by construction.
  it("names both counts when runs and executions disagree (a mission's seats collapse to one run)", async () => {
    const missionId = "mission-1";
    const seatRecords: RawRecord[] = Array.from({ length: 8 }, (_, i) => ({
      ts: at(0),
      machine_uid: "u1",
      machine_id: "MacBook-Pro",
      session_id: `seat-${i + 2}`,
      action: "dispatch.start",
      mission_id: missionId,
      handle: "darkmux/crawler",
    }));
    const missionRecords: NormRecord[] = normAll([
      { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: missionId, action: "run.start", mission_id: missionId },
      { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "seat-1", action: "dispatch.start", mission_id: missionId, handle: "darkmux/crawler" },
      { ts: at(0), machine_uid: "u1", session_id: "seat-1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 0 } },
      { ts: at(2), machine_uid: "u1", session_id: "seat-1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 2000, generated_chars: 800 } },
      ...seatRecords,
    ]);
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={missionRecords} tMax={D0 + 5000} tMin={D0} playhead={D0 + 5000} historical />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    // 9 seat executions, 1 collapsed run.
    expect(document.querySelector(".mach-scope__pager-n")!.textContent).toBe("1/9");
    const countEl = document.querySelector(".runs--live, .runs")!;
    expect(countEl.textContent).toBe("1 run · 9 executions · 100 tok/s");
  });

  it("(#2890) PROMPT: the status line carries the estimated size; the tube is handed no center", async () => {
    const scopeProps = () =>
      JSON.parse(document.querySelector('[data-testid="token-scope-probe"]')!.getAttribute("data-props")!) as ScopeProbe;
    // One execution whose turn opener reported a 144,000-char prompt; no
    // billed turn yet, so the default 4 chars/token -> ~36k.
    const sized: NormRecord[] = normAll([
      { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "p1", action: "dispatch.start", handle: "coder" },
      { ts: at(1), machine_uid: "u1", session_id: "p1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 1000, generated_chars: 0, turn_seq: 1, prompt_chars: 144_000 } },
    ]);
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={sized} tMax={D0 + 3000} tMin={D0} playhead={D0 + 3000} historical />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__rate")).not.toBeNull());
    const line = document.querySelector(".mach-scope__rate")!;
    expect(line.textContent).toBe("processing ~36k");
    expect(line.getAttribute("title")).toBe("estimated prompt size: ~36k tokens");
    expect(scopeProps()).toMatchObject({ state: "prompt", centerLabel: null, centerUnit: null });
  });

  it("(#2890) PROMPT from an older host (no size): the plain words, no tooltip", async () => {
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={threeExecutionRecords.filter((r) => r.session_id === "s3")} tMax={D0 + 5000} tMin={D0} playhead={D0 + 5000} historical />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__rate")).not.toBeNull());
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("processing prompt");
    expect(document.querySelector(".mach-scope__rate")!.getAttribute("title")).toBeNull();
  });

  it("no pager renders for exactly one running execution — same as before this issue", async () => {
    const oneExecution = threeExecutionRecords.filter((r) => r.session_id === "s1");
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={oneExecution} tMax={D0 + 5000} tMin={D0} playhead={D0 + 5000} historical />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__rate")).not.toBeNull());
    expect(document.querySelector(".mach-scope__pager")).toBeNull();
    // The count line has NO tok/s suffix at N=1 — that's still the rate
    // line's job, as before.
    expect(document.querySelector(".runs--live")!.textContent).toBe("1 running");
  });

  it("(#2890) the card's tube center matches the run page's: the rate while generating, the countdown while resting", async () => {
    // `TokenScope` is mocked in this file (a probe that records its props),
    // so this reads what the card HANDS the tube; TokenScope.test.tsx pins
    // how the tube renders a center label.
    const scopeProps = () =>
      JSON.parse(document.querySelector('[data-testid="token-scope-probe"]')!.getAttribute("data-props")!) as ScopeProbe;
    renderThree(5);
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("100 tok/s");
    expect(scopeProps()).toMatchObject({ state: "generating", centerLabel: "100", centerUnit: "tok/s", centerCarried: false });
    fireEvent.click(screen.getByLabelText("next execution"));
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("rest 13s");
    // (#2890) The same center as the run page's: the countdown over "resting".
    expect(scopeProps()).toMatchObject({ centerLabel: "13s", centerUnit: "resting" });
    // (#2961) The rest's end on the page clock (the playhead here), which the
    // countdown is counted against; the playback is not playing (no
    // transport in this render), so the clock stands still.
    const p = scopeProps();
    expect(p.clock.kind).toBe("frozen");
    expect(Math.ceil((p.restEndMs - p.clock.tMs) / 1000)).toBe(13);
  });

  // (#2950) The card's status line says why it rests, from the rest record's
  // own `reason`/`state`, the way it says which tool is being generated.
  it("a resting execution's status line says why, with the whole reason on hover", async () => {
    const recs = threeExecutionRecords
      .filter((r) => r.session_id === "s2")
      .map((r) => (r.action === ACTION.DispatchRest ? (norm({ ...r, payload: { ms: 15_000, reason: "thermal", state: "serious" } })) : r));
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={recs} tMax={D0 + 5000} tMin={D0} playhead={D0 + 5000} historical />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__rate")).not.toBeNull());
    // (Operator, 2026-09-27) Both forms are in the line; CSS shows the full
    // one at desktop width and the one without the state on a phone
    // (`.mach-scope__why--*` in styles.css; the parity layout suite checks
    // which one is visible at each width).
    expect(document.querySelector(".mach-scope__why--full")!.textContent).toBe("thermal · serious");
    expect(document.querySelector(".mach-scope__why--word")!.textContent).toBe("thermal");
    expect(document.querySelector(".mach-scope__rate")!.getAttribute("title")).toBe("resting: thermal · serious");
  });

  it("an arrow click changes the page and does not fire the card's machine drill-in", async () => {
    renderThree(5);
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    expect(window.location.hash).toBe("");
    fireEvent.click(screen.getByLabelText("next execution"));
    expect(document.querySelector(".mach-scope__pager-n")!.textContent).toBe("2/3");
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("reviewer");
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("rest 13s");
    // (#1903-shaped) The arrow is its own tap target — stopPropagation kept
    // it from ALSO firing the outer card's `machineDrillHash` click.
    expect(window.location.hash).toBe("");
  });

  // (#2886 pass 5, MUST — fresh-reviewer finding F5) No test clicked the
  // PREVIOUS arrow specifically — a mutation wiring it to the SAME `+1` the
  // next arrow uses stayed green. Wrap-around from page 1 is the
  // distinguishing case: `+1` would land on page 2 (indistinguishable from
  // clicking "next"); a correct `-1` wraps to the LAST page.
  it("the previous arrow moves BACKWARD (wraps to the last page from page 1), not the same direction as next", async () => {
    renderThree(5);
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    expect(document.querySelector(".mach-scope__pager-n")!.textContent).toBe("1/3");
    fireEvent.click(screen.getByLabelText("previous execution"));
    expect(document.querySelector(".mach-scope__pager-n")!.textContent).toBe("3/3");
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("fetch-render");
  });

  it("the picked page sticks until that execution ends, then falls forward to the new busiest among what's left", async () => {
    const { rerender } = renderThree(5);
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    // Pick s2 (reviewer, resting) — one click forward from the default s1.
    fireEvent.click(screen.getByLabelText("next execution"));
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("reviewer");

    // s2 ends; s1 (generating) and s3 (prompt) are still running.
    const afterS2Ends: NormRecord[] = [
      ...threeExecutionRecords,
      norm({ ts: at(4), machine_uid: "u1", session_id: "s2", action: "dispatch.complete" }),
    ];
    rerender(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={afterS2Ends} tMax={D0 + 6000} tMin={D0} playhead={D0 + 6000} historical />
      </QueryClientProvider>,
    );

    // Still a pager (2 executions left), but the sticky pick (s2) is gone —
    // falls forward to the new busiest (s1, generating), not to whichever
    // index s2 used to occupy.
    await waitFor(() => expect(document.querySelector(".mach-scope__pager-n")!.textContent).toBe("1/2"));
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("coder");
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("100 tok/s");
  });

  // (#2886 pass 5, MUST — fresh-reviewer finding F3) A lit GEN lamp with no
  // reading yet (one heartbeat, no same-turn pair) must not print a
  // confident "0 tok/s" — same "—" the run page's tile already shows for
  // the identical case. The tube must not be driven by a fake 0 either —
  // pinned via the mocked TokenScope so a future regression that only hits
  // the tube (leaving the text correct) still goes red.
  it("shows '—', not '0 tok/s', while generating with no reading yet — and never drives the tube with a fake 0", async () => {
    // (mirrors `tokenRate.test.ts`'s own fixture note) `dispatch.start` sits
    // 5s before the heartbeat so `deriveLiveState`'s same-second marker tie
    // rule doesn't fire and read this as PROMPT instead of GENERATING.
    const oneFreshHeartbeat: NormRecord[] = normAll([
      { ts: at(-5), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "darkmux/coder" },
      { ts: at(0), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 40 } },
    ]);
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={oneFreshHeartbeat} tMax={D0} tMin={D0} playhead={D0} historical />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__rate")).not.toBeNull());
    // (#2955 review) Now the whole status line, a bare "—" said nothing:
    // "— tok/s", as the thinking case reads "— think tok/s". A known state
    // (generating) with no number yet keeps the reading style and the lit
    // dot; only "no signal" (no state at all) takes the dim, plain line.
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("— tok/s");
    expect(document.querySelector(".mach-scope__rate")).toBe(document.querySelector(".mach .stat"));
    expect(document.querySelector(".mach")!.className).not.toContain("nosignal");
    expect(latestTokenScopeProps()).toMatchObject({ tokensPerSec: null, state: "generating" });
  });

  // (#2886 pass 5, MUST — fresh-reviewer finding F6) The default page must
  // not flap: recomputing "the busiest execution" from scratch every tick
  // flipped a real fleet's default page 46 times in 863s, because two
  // GENERATING executions' fluctuating rates kept trading the tie-break.
  describe("the default page does not flap on a tie, only moves on a STRICT state-class win", () => {
    // s1: CODER, generating at 100 tok/s (0 -> 800 chars over 2s).
    // s2: REVIEWER, generating at 10 tok/s (0 -> 80 chars over 2s) at first.
    const twoGenerating = (s2Chars: number): NormRecord[] => normAll([
      { ts: at(-5), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "darkmux/coder" },
      { ts: at(0), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 0 } },
      { ts: at(2), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 2000, generated_chars: 800 } },
      { ts: at(-5), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.start", handle: "darkmux/reviewer" },
      { ts: at(0), machine_uid: "u1", session_id: "s2", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 0 } },
      { ts: at(2), machine_uid: "u1", session_id: "s2", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 2000, generated_chars: s2Chars } },
    ]);

    it("keeps the SAME default page once s2's rate overtakes s1's — both still generating", async () => {
      const { rerender } = render(
        <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
          <FleetLens records={twoGenerating(80)} tMax={D0 + 5000} tMin={D0} playhead={D0 + 5000} historical />
        </QueryClientProvider>,
      );
      // Initial pick: s1 is the faster of the two (100 vs 10 tok/s).
      await waitFor(() => expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("coder"));

      // s2's rate now FAR exceeds s1's (2000 chars -> 250 tok/s vs s1's
      // unchanged 100) — recomputing "busiest" from scratch would flip to
      // s2. Both are still `generating`, a tie at the STATE-CLASS level, so
      // the sticky default must not move.
      rerender(
        <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
          <FleetLens records={twoGenerating(2_000)} tMax={D0 + 5000} tMin={D0} playhead={D0 + 5000} historical />
        </QueryClientProvider>,
      );
      expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("coder");
      expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("100 tok/s");
    });

    it("DOES move once the currently-shown execution becomes strictly worse (generating -> rest) while the other keeps generating", async () => {
      // Same starting point as the no-flap test above: s1 is the faster of
      // the two, so it's the initial default.
      const { rerender } = render(
        <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
          <FleetLens records={twoGenerating(80)} tMax={D0 + 5000} tMin={D0} playhead={D0 + 5000} historical />
        </QueryClientProvider>,
      );
      await waitFor(() => expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("coder"));

      // s1 now rests (a real state-class change); s2 keeps generating.
      // s2 is STRICTLY busier now (generating beats rest) — this is a real
      // switch, not a flap, and must happen.
      const s1Rests: NormRecord[] = [...twoGenerating(2_000), norm({ ts: at(3), machine_uid: "u1", session_id: "s1", action: "dispatch.rest", payload: { ms: 15_000 } })];
      rerender(
        <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
          <FleetLens records={s1Rests} tMax={D0 + 6000} tMin={D0} playhead={D0 + 6000} historical />
        </QueryClientProvider>,
      );
      await waitFor(() => expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("reviewer"));
      expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("250 tok/s");
    });
  });

  // (#2886 pass 5, MUST — fresh-reviewer finding F4) The half-open evidence
  // threaded into each execution's OWN reading (`cards.ts`'s
  // `executionTokenReading` call — `lastHeartbeatMs([recs])`, not the
  // machine-wide `lastHeartbeatMs(liveTokRecordSets)` the AGGREGATE uses)
  // was previously only asserted on `card.executions[i].state` directly —
  // nothing rendered proved it reached the screen. This pins it at the
  // rendered surface: s1 (fresh, generating) and s2 (stale, would read
  // STALLED on its own) share one card. `lastContactMs` sits AFTER s2's own
  // deadline (so a CORRECT per-execution check trusts s2's stall) but
  // BEFORE s1's much-later deadline (so a WRONG machine-wide check — using
  // s1's fresher heartbeat as the deadline for BOTH executions — would
  // wrongly downgrade s2 to "no signal" instead).
  it("downgrades a stalled execution using ITS OWN last heartbeat as the half-open deadline, not the machine-wide one", async () => {
    const records: NormRecord[] = normAll([
      // s1: CODER, fresh — generating, last heartbeat at 95s.
      { ts: at(-5), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "darkmux/coder" },
      { ts: at(93), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 93_000, generated_chars: 0 } },
      { ts: at(95), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 95_000, generated_chars: 800 } },
      // s2: REVIEWER, stale — one heartbeat at 0s, long past STALL_AFTER_MS
      // (30s) by the t=100s playhead. Its OWN deadline is 0 + 30 = 30s.
      { ts: at(-5), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.start", handle: "darkmux/reviewer" },
      { ts: at(0), machine_uid: "u1", session_id: "s2", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 40 } },
    ]);
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        {/* lastContactMs = 40s: past s2's own 30s deadline (trust s2's
            stall) but nowhere near s1's 95+30=125s deadline (a machine-wide
            check would NOT trust it). */}
        <FleetLens records={records} tMax={D0 + 100_000} tMin={D0} playhead={D0 + 100_000} historical connected lastContactMs={D0 + 40_000} />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    // Default page is s1 (generating beats stalled/no-signal either way).
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("coder");
    fireEvent.click(screen.getByLabelText("next execution"));
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("reviewer");
    expect(document.querySelector(".mach-scope__rate")!.textContent?.toLowerCase()).toBe("stalled");
    expect(latestTokenScopeProps()).toMatchObject({ state: "stalled" });
  });

  // (#2886 pass 5, MUST — fresh-reviewer finding F4, second half) The test
  // above alone does not prove the half-open evidence is CONSULTED at all —
  // `lastContactMs=40s` trusts s2's stall either way: with the correct
  // per-execution deadline (30s) OR with no check running at all (passing
  // `undefined`, which trusts every stall unconditionally while connected).
  // This one moves `lastContactMs` BEFORE s2's own deadline, so the CORRECT
  // behavior downgrades to "no signal" — a result "no check ran" cannot
  // produce (it would still read "stalled").
  it("downgrades to 'no signal' when contact came BEFORE the stalled execution's own deadline", async () => {
    const records: NormRecord[] = normAll([
      { ts: at(-5), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "darkmux/coder" },
      { ts: at(93), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 93_000, generated_chars: 0 } },
      { ts: at(95), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 95_000, generated_chars: 800 } },
      // s2's own deadline is 0 + 30 = 30s.
      { ts: at(-5), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.start", handle: "darkmux/reviewer" },
      { ts: at(0), machine_uid: "u1", session_id: "s2", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 40 } },
    ]);
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        {/* lastContactMs = 20s: BEFORE s2's own 30s deadline. */}
        <FleetLens records={records} tMax={D0 + 100_000} tMin={D0} playhead={D0 + 100_000} historical connected lastContactMs={D0 + 20_000} />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    fireEvent.click(screen.getByLabelText("next execution"));
    expect(document.querySelector(".mach-scope__pager-role")!.textContent).toBe("reviewer");
    // (#2955 review) The plain no-signal status line, dim dot, not the reading.
    expect(document.querySelector(".mach .stat")!.textContent).toBe("no signal");
    expect(document.querySelector(".mach-scope__rate")).toBeNull();
    expect(document.querySelector(".mach")!.className).toContain("nosignal");
    expect(latestTokenScopeProps()).toMatchObject({ state: "nosignal" });
  });
});

// ── (#1855) a rostered-but-silent machine must still render a card ──
//
// Before this fix, the fleet card list was `machineUids(flowData,
// liveMachines)` — a union of flow-derived uids and CURRENTLY-beating
// presence keys, with no read of the operator's declared roster at all. A
// machine added via `darkmux machine add` and never yet started (or down
// right now, with zero flow history under its name) produced no uid for
// either half of that union to find, so it vanished from the dashboard
// entirely — indistinguishable from never having been added.
describe("FleetLens — rostered-but-silent machine (#1855)", () => {
  it("a machine on the roster with zero flow history and no live beat renders an offline card, not nothing", async () => {
    mockFleetFetch({ roster: [{ id: "studio", address: "100.64.1.2:8765", added_unix_ms: 1000 }] });
    renderFleetLens();
    // (#2958) "offline" waits on presence and the flow window.
    await waitFor(() => expect(document.querySelector(".mach")?.textContent).toContain("offline"));
    expect(screen.getByText("studio")).toBeInTheDocument();
    const card = document.querySelector(".mach")!;
    // Reuses the SAME "offline"/`.absent` indicator a machine that WAS seen
    // and has since gone quiet already renders with — no parallel "silent"
    // vocabulary invented for this case (the project's "no snowflakes,
    // shared indicators" rule).
    expect(card.className).toContain("absent");
    // (#2958 review M1) A machine that is off keeps the tube's box, its
    // screen powered off (no TokenScope, no canvas), so the card is one size.
    expect(card.querySelector('[data-testid="fleet-token-scope"] .token-scope-bezel[data-state="off"]')).not.toBeNull();
    expect(card.querySelector('[data-testid="token-scope-probe"]')).toBeNull();
  });

  it("(#2890) the machine name and hardware line carry their full text as a tooltip", async () => {
    const records = normAll([
      { ts: "2026-08-26T10:00:00.000Z", machine_uid: "u1", machine_id: "m1-max-32gb-studio", action: "machine.online", source: "presence_reconciler" },
    ]);
    renderFleetLens({ records, tMin: Date.parse("2026-08-26T09:00:00.000Z"), tMax: Date.parse("2026-08-26T10:00:00.000Z"), historical: true });
    await waitFor(() => expect(document.querySelector(".mach-name")).not.toBeNull());
    expect(document.querySelector(".mach-name")!.getAttribute("title")).toBe("m1-max-32gb-studio");
  });

  it("(#2890) an online machine with nothing running shows its tube idle", async () => {
    const records = normAll([
      { ts: "2026-08-26T10:00:00.000Z", machine_uid: "u1", machine_id: "m5", action: "machine.online", source: "presence_reconciler" },
    ]);
    renderFleetLens({ records, tMin: Date.parse("2026-08-26T09:00:00.000Z"), tMax: Date.parse("2026-08-26T10:00:00.000Z"), historical: true });
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.className).not.toContain("absent");
    const probe = card.querySelector('[data-testid="token-scope-probe"]');
    expect(probe && JSON.parse(probe.getAttribute("data-props")!)).toMatchObject({ state: "idle", size: "card", centerUnit: "idle" });
  });

  // (#1855) The card's HARDWARE line, on the same card. Rendering the
  // rostered-but-silent machine is half the fix; the other half is that its
  // card must not then assert something about it that nothing supports.
  // "hardware not reported" reads as a fact about the machine — it answered
  // and withheld its hardware — and nothing has been received from this one
  // at all.
  it("a rostered-but-silent card says its hardware is unknown, not that the machine failed to report it", async () => {
    mockFleetFetch({ roster: [{ id: "studio", address: "100.64.1.2:8765", added_unix_ms: 1000 }] });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("hardware unknown — nothing received");
    expect(card.textContent).not.toContain("hardware not reported");
  });

  // The INVERTED case for that line: a machine that DID beat, carrying no
  // `specs` (every peer on a build older than #2083 — and both machines in
  // this issue's own wire dump), keeps the original sentence. It answered.
  it("a machine that beat WITHOUT specs still reads 'hardware not reported'", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "studio", schema_version: "1.20.0", beat_ts_ms: 1 }],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("hardware not reported");
    expect(card.textContent).not.toContain("hardware unknown");
  });

  // And the fully-healthy inverted case: a beat that DOES carry hardware
  // renders the hardware and neither sentence. A change that made every card
  // read as uncertain would be as wrong as the one that made them all read
  // as confident.
  it("a machine beating WITH specs shows the hardware and no unknown line at all", async () => {
    mockFleetFetch({
      machines: [
        {
          machine_uid: "u1",
          display_name: "studio",
          schema_version: "1.20.0",
          beat_ts_ms: 1,
          specs: "Apple M1 Max · 32 GB",
        },
      ],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("Apple M1 Max · 32 GB");
    expect(card.textContent).not.toContain("hardware not reported");
    expect(card.textContent).not.toContain("hardware unknown");
  });

  // The INVERTED case: a roster entry naming a machine that IS actually
  // live must not draw a SECOND, duplicate "offline" card for the same
  // machine beside its real one.
  it("a roster entry matching a currently-live machine does not duplicate its card", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "studio", schema_version: "1.20.0", beat_ts_ms: 1 }],
      roster: [{ id: "studio", address: "100.64.1.2:8765", added_unix_ms: 1000 }],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(document.querySelectorAll(".mach").length).toBe(1);
    expect(document.querySelector(".mach")!.className).not.toContain("absent");
  });

  // A genuinely gone machine (never rostered, never beating, never in flow
  // history) must not linger as if present — the roster fix must not make
  // every machine render forever regardless of evidence.
  it("a machine that is genuinely gone (not rostered, not beating, no flow history) renders no card at all", async () => {
    mockFleetFetch();
    renderFleetLens();
    await waitFor(() => expect(screen.getByText(/tokens · last/i)).toBeInTheDocument());
    expect(document.querySelector(".mach")).toBeNull();
  });

  // (#1855 follow-up, F1) THE SELF-MACHINE PHANTOM: presence self-disables
  // when Redis is unset (the default), so a quiet window can carry ZERO
  // flow history and ZERO live beats for the daemon serving this very
  // page — even though its own roster entry exists (this project's
  // `darkmux-add-machine` skill walks the operator through creating it at
  // step 7). Before consulting `/machine/specs` here, that roster entry
  // fell through every other identity check and rendered a false "offline"
  // card for the machine that is, self-evidently, up and answering this
  // request. The honest render, absent any presence/flow evidence, is NO
  // card — same as the "genuinely gone" case just above — not a lying
  // "offline" one.
  it("a roster entry matching THIS machine's own /machine/specs identity, with zero flow/presence evidence, renders no phantom offline card", async () => {
    mockFleetFetch({
      specs: { machine_id: "studio", cpu_brand: "Apple M5 Max" },
      roster: [{ id: "studio", address: "100.64.1.2:8765", added_unix_ms: 1000 }],
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens({}, queryClient);
    await waitFor(() => expect(screen.getByText(/tokens · last/i)).toBeInTheDocument());
    await waitForFleetQueriesSettled(queryClient);
    expect(document.querySelector(".mach")).toBeNull();
  });

  // (#1855 follow-up, F2) THE MISMATCHED-NAME DUPLICATE: the roster id is
  // operator-typed prose with nothing validating it against what the peer
  // actually beats as. A live peer beating as one name and rostered under
  // a near-miss (here, differing only by the mDNS `.local` suffix — the
  // same alias class #2030 already hit for one machine's own two names)
  // used to render a SECOND, phantom "offline" card beside the real one —
  // three cards for two machines.
  it("a roster entry differing from a live beat only by the mDNS .local suffix does not duplicate its card", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro.local", schema_version: "1.20.0", beat_ts_ms: 1 }],
      roster: [{ id: "MacBook-Pro", address: "100.64.1.2:8765", added_unix_ms: 1000 }],
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens({}, queryClient);
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    await waitForFleetQueriesSettled(queryClient);
    expect(document.querySelectorAll(".mach").length).toBe(1);
    expect(document.querySelector(".mach")!.className).not.toContain("absent");
  });

  // (#2768) THE THREE-GENERATIONS-OF-RENAME DUPLICATE: the exact shape
  // #1855 follow-up F2's mDNS-suffix fix above does NOT reach. `laptop` and
  // `MacBook-Pro` share no substring at all — no case fold, no `.local`
  // strip, no whitespace trim closes that gap — so a roster entry with a
  // matching `machine_uid` but a wholly different `id` used to render as a
  // SECOND, phantom "offline" card beside the machine's real, live one
  // (measured live: one machine wearing `laptop` → `MacBook-Pro.local` →
  // `MacBook-Pro` over time rendered FOUR cards for two machines). A
  // same-name fixture (the test just above) would pass against this bug —
  // only the uid join can catch it.
  it("a roster entry whose machine_uid matches a live beat under a wholly different name does not duplicate its card, and the MACHINE's name titles it", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "00000000-0000-4000-8000-ABCDEF000020", display_name: "MacBook-Pro", schema_version: "1.20.0", beat_ts_ms: 1 }],
      roster: [{ id: "laptop", address: "127.0.0.1:8765", added_unix_ms: 1000, machine_uid: "00000000-0000-4000-8000-ABCDEF000020" }],
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens({}, queryClient);
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    await waitForFleetQueriesSettled(queryClient);
    // One card, not two.
    expect(document.querySelectorAll(".mach").length).toBe(1);
    const card = document.querySelector(".mach")!;
    expect(card.className).not.toContain("absent");
    // Relabeled to the roster's declared name, not the beat's display_name.
    // Scoped to the CARD's own name element: the activity timeline lane
    // below still legitimately labels its lane from `nameOf` (flow-derived,
    // untouched by this override — it is a separate question from the
    // card's label), so "MacBook-Pro" is still on the page elsewhere.
    // (#2802 regression fix) This assertion used to be INVERTED: #2768 had
    // the roster id override the card title, so it required "laptop" and
    // forbade "MacBook-Pro". That was inert while roster entries carried no
    // uid. Once #2802 began back-filling uids from flow history the override
    // started firing on entries nobody had aliased on purpose, and this
    // machine's card rendered as `laptop` — a June roster id pointing at a
    // port with no daemon — directly above an activity lane reading
    // `MacBook-Pro`. One machine, two names, one screen.
    //
    // The machine's own `machine_id` titles the card now: it is what every
    // flow record carries, what the activity lanes and run rows use, and what
    // `nameOf` was fixed in #2030 to track. The operator's alias is not
    // discarded — it rides along as secondary text — but it never replaces
    // the name the machine answers to.
    //
    // What #2768 actually fixed is asserted above and still holds: ONE card,
    // not two. That was always the defect; the relabel was a choice bundled
    // with it.
    expect(card.querySelector(".name")!.textContent).toContain("MacBook-Pro");
  });

  // The inverted case: a roster `machine_uid` that matches NOTHING
  // currently known is a genuinely separate, silent machine and must still
  // render its own "rostered, never seen" card — the uid join narrows,
  // it never widens into "any roster entry with a uid is accounted for."
  it("a roster entry with a machine_uid matching no known machine still renders its own offline card", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "00000000-0000-4000-8000-ABCDEF000020", display_name: "MacBook-Pro", schema_version: "1.20.0", beat_ts_ms: 1 }],
      roster: [{ id: "mini-1", address: "100.64.1.9:8765", added_unix_ms: 1000, machine_uid: "NEVER-SEEN-UID" }],
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens({}, queryClient);
    await waitFor(() => expect(document.querySelectorAll(".mach").length).toBeGreaterThanOrEqual(2));
    await waitForFleetQueriesSettled(queryClient);
    const cards = document.querySelectorAll(".mach");
    expect(cards.length).toBe(2);
    const names = [...cards].map((c) => c.querySelector(".name")!.textContent);
    expect(names.some((n) => n?.includes("MacBook-Pro"))).toBe(true);
    expect(names.some((n) => n?.includes("mini-1"))).toBe(true);
  });

  // (#1855 follow-up, CONSIDER 4) A present-but-corrupt roster file used to
  // be indistinguishable from a genuinely empty one: `useFleetRoster`
  // dropped `error` entirely, so every previously-visible rostered card
  // vanished again with NO signal that the read had failed — reproducing
  // #1855's own symptom via the very fix meant to close it. The literal
  // asserted here is the server's fixed wire string
  // (`fleet_roster_handler`'s `ROSTER_READ_FAILED`), never a raw parse
  // error, matching that crate's own redaction discipline.
  it("a corrupt roster file surfaces a visible notice instead of silently reading as an empty roster", async () => {
    mockFleetFetch({ rosterError: "the fleet roster file exists but could not be parsed" });
    renderFleetLens();
    await waitFor(() => expect(screen.getByText(/fleet roster unreadable/i)).toBeInTheDocument());
    expect(screen.getByText(/the fleet roster file exists but could not be parsed/)).toBeInTheDocument();
  });

  it("a healthy (non-corrupt) roster renders no roster-unreadable notice", async () => {
    mockFleetFetch({ roster: [{ id: "studio", address: "100.64.1.2:8765", added_unix_ms: 1000 }] });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(screen.queryByText(/fleet roster unreadable/i)).not.toBeInTheDocument();
  });
});

// ── (#2814) SELF IS NEVER UNKNOWN — the rendered fixture ──────────────
describe("FleetLens — this machine identifies itself without the flow window (#2814)", () => {
  //
  // The operator's own acceptance test, verbatim: an EMPTY flow window, no
  // presence beats, no roster, with only `/machine/specs` answering. This is
  // not a contrived state — it is a fresh install, a machine whose Redis is
  // off (presence self-disables, the off-by-default case), and any machine
  // whose last flow record has aged out of the retained window. Time passing
  // is enough to reach it.
  //
  // Pre-#2814 this rendered ZERO cards. `machineUids` unions flow-derived
  // uids with beating presence keys and both are empty, and the F1
  // self-check inside `rosterOnlyEntries` correctly suppresses the self
  // roster entry as already-accounted-for — so nothing accounts for it. The
  // daemon answering the request drew nothing at all about itself.
  it("(#2814) renders THIS machine's own card from /machine/specs alone — empty window, no beats, no roster", async () => {
    const uid = "00000000-0000-4000-8000-ABCDEF000011";
    mockFleetFetch({
      specs: {
        machine_id: "MacBook-Pro",
        machine_uid: uid,
        cpu_brand: "Apple M5 Max",
        ram_total_bytes: 137438953472,
      },
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens({}, queryClient);
    await waitForFleetQueriesSettled(queryClient);
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const cards = [...document.querySelectorAll(".mach")];
    expect(cards.length).toBe(1);
    // Its own name — not the raw uid `nameOf` falls back to.
    expect(cards[0].textContent).toContain("MacBook-Pro");
    expect(cards[0].textContent).not.toContain(uid.slice(0, 8));
    // Its own hardware — read directly, never "hardware not reported".
    expect(cards[0].textContent).toContain("Apple M5 Max · 128 GB");
    expect(cards[0].textContent).not.toContain("hardware not reported");
    expect(cards[0].textContent).not.toContain("hardware unknown");
  });

  // The inverted case. Without it, a fix that unconditionally drew a card
  // for `specs.machine_uid` would pass the test above AND keep drawing a
  // duplicate beside the machine's real, live card once the window has
  // records again — which is the #2796 phantom, reintroduced from the other
  // direction.
  it("(#2814) does NOT draw a second card when the window already knows this machine's uid", async () => {
    const today = todayUTC();
    const uid = "00000000-0000-4000-8000-ABCDEF000011";
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: uid, machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      ],
      specs: { machine_id: "MacBook-Pro", machine_uid: uid, cpu_brand: "Apple M5 Max" },
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens({}, queryClient);
    await waitForFleetQueriesSettled(queryClient);
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(document.querySelectorAll(".mach").length).toBe(1);
  });

  // And the roster half: an entry the operator declared under a name this
  // machine no longer uses, carrying its uid. #2814 puts the self uid in the
  // card list unconditionally, so without the uid join in `rosterOnlyEntries`
  // that stale entry draws an "offline" phantom beside the live self card.
  it("(#2814) a stale roster entry carrying this machine's uid does not draw a phantom", async () => {
    const uid = "00000000-0000-4000-8000-ABCDEF000011";
    mockFleetFetch({
      roster: [{ id: "laptop", machine_uid: uid, address: "100.64.1.2:8765", added_unix_ms: 1000 }],
      specs: { machine_id: "MacBook-Pro", machine_uid: uid, cpu_brand: "Apple M5 Max" },
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderFleetLens({}, queryClient);
    await waitForFleetQueriesSettled(queryClient);
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(document.querySelectorAll(".mach").length).toBe(1);
    expect(document.querySelector(".mach")!.textContent).toContain("MacBook-Pro");
  });
});

// ── (#2108, operator finding — "hero local/cloud figure collision") ──
//
// `.eventlog` is a fixed 380px side panel shown whenever the viewport is
// above the 768px mobile breakpoint (`App.tsx`'s `isMobile`), and
// `.app-shell__stage` carries 32px of its own horizontal padding — so in
// the viewport band (768px, 1180px], the STAGE's actual rendered width is
// BELOW 768px even though the raw viewport reads "desktop". `.savrow`'s
// OLD `max-width:768px` query keyed off the viewport, so in that band it
// stayed in flex-row mode inside a container narrower than it assumed,
// and the local/cloud/unattributed tiles collided against the chips
// rather than cleanly wrapping. The stylesheet, not a jsdom-computed
// style — jsdom performs no real layout, so this is the actual verifiable
// claim: the `.savrow` grid-switch rule's OWN threshold is the computed
// number (768 + 380 + 32 = 1180), not the un-widened 768 that produced
// the collision.
describe("FleetLens — hero grid-switch breakpoint accounts for the eventlog panel's width (#2108)", () => {
  it("the .savrow stylesheet rule switches to the 2-column grid at 1180px, not 768px", () => {
    const cssPath = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../styles.css");
    const css = readFileSync(cssPath, "utf-8");
    const match = css.match(/@media \(max-width: (\d+)px\) \{\s*\.savrow \{\s*display: grid;/);
    expect(match, "the .savrow grid-switch media query must exist").not.toBeNull();
    expect(Number(match![1])).toBe(1180);
  });
});

// ── (#2817) waiting is not zero ──────────────────────────────────────────
//
// Operator: "the dashboard/fleet view shows all zeros for the token data as
// if it's waiting for the first flow record to initiate the values. but for
// at least a few seconds it reads as legit."
//
// Every figure in the hero computes to 0 from an empty window, and 0 is a
// MEASUREMENT — "this fleet used no cloud tokens" — held long enough to be
// believed. Same class as the other integrity defects in this area: stating
// a fact the data does not support.
//
// The distinction these two tests protect is what makes the fix correct
// rather than merely quiet. A SETTLED zero is real and must still read "0";
// a fresh fleet that has genuinely run nothing deserves to be told so.
// (#2958) The operator, watching the fleet page load on a phone: the cards
// said "idle" for the seconds `/runs` and `/fleet/roster` took to answer,
// while a run was live. A POSITIVE reading shows as soon as its own source
// has it; a NEGATIVE claim ("idle", "no model working", "0 running",
// "offline") waits for every source that could contradict it and says
// "no signal" (stat word, tube, and a dash for the count) until then.
describe("FleetLens — a card says no signal until its first data arrives (#2958)", () => {
  const BEAT = [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.parse(FROZEN_NOW) }];
  const cardScope = (card: Element) => JSON.parse(card.querySelector('[data-testid="token-scope-probe"]')!.getAttribute("data-props")!) as ScopeProbe;
  const stat = (card: Element) => card.querySelector(".stat")!.textContent;
  const utilLabel = (card: Element) => card.querySelector(".mach-util")!.getAttribute("aria-label")!;
  const SPECS = { machine_id: "MacBook-Pro", machine_uid: "u-self", cpu_brand: "Apple M5 Max", ram_total_bytes: 137438953472 };
  const newClient = () => new QueryClient({ defaultOptions: { queries: { retry: false } } });

  it("a live lab run: 'no signal' while /runs is unanswered, then 'dispatch in flight'", async () => {
    const runs = gate();
    mockFleetFetch({
      machines: BEAT,
      runs: [{ id: "lab-1", kind: "lab", status: "running", machine: "MacBook-Pro", tracked: true }],
      hold: { "/runs": runs.promise },
    });
    const queryClient = newClient();
    renderFleetLens({}, queryClient);
    // Presence and the flow window have answered (so the card exists and
    // its utility strip knows it is quiet); `/runs` has not.
    await waitForFleetQueriesSettled(queryClient);
    await waitFor(() => expect(document.querySelector('.savings[data-settled="true"]')).not.toBeNull());
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(stat(card)).toBe("no signal");
    expect(card.textContent).not.toContain("idle");
    expect(card.textContent).not.toContain("running");
    expect(card.querySelector(".runs")!.textContent).toBe("—");
    expect(card.className).toContain("nosignal");
    expect(card.className).not.toContain("absent");
    expect(card.className).not.toContain("active");
    expect(document.querySelectorAll('[data-testid="fleet-token-scope"]')).toHaveLength(1);
    expect(cardScope(card)).toMatchObject({ state: "nosignal", size: "card" });
    expect(cardScope(card).centerUnit ?? null).toBeNull();
    // The flow window HAS answered, and it is the only source of utility
    // jobs, so a quiet strip may say so.
    expect(utilLabel(card)).toMatch(/idle$/);

    runs.open();
    await waitFor(() => expect(stat(document.querySelector(".mach")!)).toBe("dispatch in flight"));
    const loaded = document.querySelector(".mach")!;
    expect(loaded.textContent).toContain("1 running");
    expect(loaded.className).not.toContain("nosignal");
    expect(cardScope(loaded)).toMatchObject({ state: "idle", centerUnit: "no model working" });
  });

  it("a genuinely idle machine: 'no signal' while loading, then 'idle' once its data says so", async () => {
    const runs = gate();
    mockFleetFetch({ machines: BEAT, runs: [], hold: { "/runs": runs.promise } });
    const queryClient = newClient();
    renderFleetLens({}, queryClient);
    await waitForFleetQueriesSettled(queryClient);
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(stat(document.querySelector(".mach")!)).toBe("no signal");
    expect(cardScope(document.querySelector(".mach")!)).toMatchObject({ state: "nosignal" });

    runs.open();
    await waitFor(() => expect(stat(document.querySelector(".mach")!)).toBe("idle"));
    const card = document.querySelector(".mach")!;
    expect(card.textContent).toContain("0 running");
    expect(cardScope(card)).toMatchObject({ state: "idle", centerUnit: "idle" });
    expect(utilLabel(card)).toMatch(/idle$/);
  });

  // Each source on its own: this machine's card (drawn from /machine/specs,
  // which is fast) must wait on every source its "idle" is derived from,
  // so holding any ONE of them holds "no signal".
  // The flow paths are named inside the test: `todayUTC()` reads the frozen
  // clock, which `beforeEach` sets after this list is built.
  const flowPaths = () => [`/flow/${todayUTC()}`, `/flow/${prevDateUTC(todayUTC())}`];
  for (const [what, held] of [
    ["the flow window", flowPaths],
    ["live machines", () => ["/fleet/machines/live"]],
    ["live sessions", () => ["/fleet/sessions/live"]],
    ["/runs", () => ["/runs"]],
  ] as const) {
    it(`this machine's own card says 'no signal' while ${what} alone is unanswered, then 'idle'`, async () => {
      const paths: string[] = held();
      const slow = gate();
      mockFleetFetch({ specs: SPECS, runs: [], hold: Object.fromEntries(paths.map((p) => [p, slow.promise])) });
      const queryClient = newClient();
      renderFleetLens({}, queryClient);
      await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
      // Every other source has answered before the card is read: the held
      // one is the only thing standing between the card and its reading.
      const others = { "/fleet/machines/live": queryKeys.fleetMachinesLive(), "/fleet/sessions/live": queryKeys.fleetSessionsLive(), "/runs": queryKeys.runs() };
      await waitFor(() => {
        for (const [path, key] of Object.entries(others)) {
          if (!paths.includes(path)) expect(queryClient.getQueryState(key)?.status, path).toBe("success");
        }
        if (what !== "the flow window") expect(document.querySelector('.savings[data-settled="true"]')).not.toBeNull();
      });
      const card = document.querySelector(".mach")!;
      expect(card.textContent).toContain("MacBook-Pro");
      expect(stat(card)).toBe("no signal");
      expect(card.querySelector(".runs")!.textContent).toBe("—");
      expect(cardScope(card)).toMatchObject({ state: "nosignal" });
      // Only the flow window can contradict a quiet utility strip.
      expect(utilLabel(card)).toMatch(what === "the flow window" ? /no signal$/ : /idle$/);

      slow.open();
      await waitFor(() => expect(stat(document.querySelector(".mach")!)).toBe("idle"));
    });
  }

  // (#2958 review C1/C2) A POSITIVE reading is not held back by a source
  // that cannot contradict it: two executions generating in the flow
  // window, and a utility job running, all show while `/runs` is still
  // unanswered.
  it("a live reading shows at once, with /runs still unanswered: tube, rate line, pager, count and utility job", async () => {
    const today = todayUTC();
    const t = (hms: string) => `${today}T${hms}.000Z`;
    const ms = (hms: string) => Date.parse(t(hms));
    const gen = (sid: string, handle: string) => [
      { ts: t("10:01:50"), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: sid, action: "dispatch.start", handle },
      { ts: t("10:01:56"), machine_uid: "u1", session_id: sid, action: "dispatch.turn.heartbeat", payload: { turn_seq: 1, sampled_at_ms: ms("10:01:56"), generated_chars: 0 } },
      { ts: t("10:01:58"), machine_uid: "u1", session_id: sid, action: "dispatch.turn.heartbeat", payload: { turn_seq: 1, sampled_at_ms: ms("10:01:58"), generated_chars: 800 } },
    ];
    const runs = gate();
    mockFleetFetch({
      machines: BEAT,
      flowToday: [
        ...gen("s1", "coder"),
        ...gen("s2", "reviewer"),
        { ts: t("10:01:59"), machine_uid: "u1", machine_id: "MacBook-Pro", action: "utility.start", source: "utility", handle: "radio-router", payload: { job: "radio_routing", model: "darkmux:util-4b", stall_after_seconds: 30 } },
      ],
      runs: [],
      hold: { "/runs": runs.promise },
    });
    const queryClient = newClient();
    renderFleetLens({}, queryClient);
    await waitForFleetQueriesSettled(queryClient);
    await waitFor(() => expect(document.querySelector(".mach-scope__pager")).not.toBeNull());
    expect(queryClient.getQueryState(queryKeys.runs())?.status, "/runs is still unanswered").toBe("pending");
    const card = document.querySelector(".mach")!;
    // (#2955) The live reading is the status line: shown at once, in
    // "dispatch in flight"'s place.
    expect(stat(card)).toMatch(/tok\/s$/);
    expect(card.className).toContain("active");
    expect(card.className).not.toContain("nosignal");
    expect(card.querySelectorAll('[data-testid="fleet-token-scope"]')).toHaveLength(1);
    expect(cardScope(card)).toMatchObject({ state: "generating" });
    expect(card.querySelector(".mach-scope__rate")!.textContent).toMatch(/tok\/s$/);
    expect(card.querySelector(".mach-scope__pager-n")!.textContent).toBe("1/2");
    expect(card.querySelector(".runs")!.textContent).toMatch(/^2 running · /);
    expect(card.querySelector(".mach-util")!.getAttribute("data-visual")).toBe("radio");
    expect(utilLabel(card)).toMatch(/radio routing$/);
  });

  // (#2958 review M1) An offline card is one size in every state: while
  // presence is unanswered it says "no signal" in the tube's box, and once
  // presence says it is gone it keeps that box with the screen powered off.
  it("a rostered-but-silent machine: 'no signal' while presence is unanswered, then 'offline' with a powered-off tube", async () => {
    const presence = gate();
    mockFleetFetch({ roster: [{ id: "studio", address: "100.64.1.2:8765", added_unix_ms: 1000 }], runs: [], hold: { "/fleet/machines/live": presence.promise } });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    await waitFor(() => expect(document.querySelector('.savings[data-settled="true"]')).not.toBeNull());
    const card = document.querySelector(".mach")!;
    expect(stat(card)).toBe("no signal");
    expect(card.className).not.toContain("absent");
    expect(cardScope(card)).toMatchObject({ state: "nosignal" });

    presence.open();
    await waitFor(() => expect(stat(document.querySelector(".mach")!)).toBe("offline"));
    const off = document.querySelector(".mach")!;
    expect(off.className).toContain("absent");
    expect(off.className).not.toContain("nosignal");
    const scope = off.querySelectorAll('[data-testid="fleet-token-scope"]');
    expect(scope).toHaveLength(1);
    expect(scope[0].querySelector('.token-scope-bezel[data-state="off"] .token-scope-screen')).not.toBeNull();
    // Powered off: no trace, no static, no center, no canvas.
    expect(scope[0].querySelector('[data-testid="token-scope-probe"]')).toBeNull();
    expect(scope[0].querySelector("canvas")).toBeNull();
  });

  // (#2958 review C4) Only the FIRST answer counts. At UTC midnight the
  // flow window rolls to a new day's key, which starts out pending; the
  // cards must not blink back to "no signal" while it loads.
  it("does not return to 'no signal' when the flow window rolls to a new day at UTC midnight", async () => {
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    vi.setSystemTime(new Date("2026-06-15T23:59:58.000Z"));
    const nextDay = gate();
    mockFleetFetch({ machines: BEAT, runs: [], hold: { "/flow/2026-06-16": nextDay.promise } });
    renderFleetLens();
    await waitFor(() => expect(stat(document.querySelector(".mach")!)).toBe("idle"));
    const fetchMock = vi.mocked(fetch);
    vi.setSystemTime(new Date("2026-06-16T00:00:03.000Z"));
    await act(async () => {
      vi.advanceTimersByTime(5_000);
    });
    // The rollover really happened: the new day's window was asked for.
    await waitFor(() => expect(fetchMock.mock.calls.some(([u]) => String(u) === "/flow/2026-06-16")).toBe(true));
    expect(stat(document.querySelector(".mach")!)).toBe("idle");
    expect(document.querySelector(".mach")!.className).not.toContain("nosignal");
  });

  // (#2958 second review, point 1) A roster entry is told apart from THIS
  // machine by `/machine/specs` (`rosterOnlyEntries`' F1 uid check). Until
  // specs answers, this machine's own roster entry is indistinguishable from
  // a silent peer, so its card must not say "offline" or "0 running": once
  // specs answers, that card is replaced by the machine's own idle one.
  it("this machine's own roster entry says 'no signal', not 'offline', while /machine/specs is unanswered", async () => {
    const specs = gate();
    mockFleetFetch({
      roster: [{ id: "laptop", address: "100.64.1.1:8765", added_unix_ms: 1000, machine_uid: "u-self" }],
      specs: SPECS,
      runs: [],
      hold: { "/machine/specs": specs.promise },
    });
    const queryClient = newClient();
    renderFleetLens({}, queryClient);
    await waitFor(() => expect(document.querySelector('.savings[data-settled="true"]')).not.toBeNull());
    await waitFor(() => {
      for (const key of [queryKeys.fleetMachinesLive(), queryKeys.fleetSessionsLive(), queryKeys.fleetRoster(), queryKeys.runs()]) {
        expect(queryClient.getQueryState(key)?.status, JSON.stringify(key)).not.toBe("pending");
      }
    });
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(queryClient.getQueryState(queryKeys.machineSpecs())?.status, "/machine/specs is still unanswered").toBe("pending");
    const card = document.querySelector(".mach")!;
    expect(stat(card)).toBe("no signal");
    expect(card.textContent).not.toContain("offline");
    expect(card.textContent).not.toContain("0 running");
    expect(card.querySelector(".runs")!.textContent).toBe("—");
    expect(card.className).not.toContain("absent");
    expect(cardScope(card)).toMatchObject({ state: "nosignal" });

    specs.open();
    await waitFor(() => expect(document.querySelector(".mach")!.textContent).toContain("MacBook-Pro"));
    await waitFor(() => expect(stat(document.querySelector(".mach")!)).toBe("idle"));
    expect(document.querySelectorAll(".mach")).toHaveLength(1);
  });

  // (#2958 second review, point 4) Offline wins over a reading: a machine
  // whose last records left an execution generating, and which then went
  // offline, draws the powered-off screen, not a live tube.
  it("an offline machine with a generating execution left open draws the powered-off tube, not a live one", async () => {
    const today = todayUTC();
    const t = (hms: string) => `${today}T${hms}.000Z`;
    const ms = (hms: string) => Date.parse(t(hms));
    mockFleetFetch({
      flowToday: [
        { ts: t("10:01:50"), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        { ts: t("10:01:56"), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { turn_seq: 1, sampled_at_ms: ms("10:01:56"), generated_chars: 0 } },
        { ts: t("10:01:58"), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { turn_seq: 1, sampled_at_ms: ms("10:01:58"), generated_chars: 800 } },
        { ts: t("10:01:59"), machine_uid: "u1", machine_id: "MacBook-Pro", action: "machine.offline", source: "presence_reconciler" },
      ],
      runs: [],
    });
    const queryClient = newClient();
    renderFleetLens({}, queryClient);
    await waitForFleetQueriesSettled(queryClient);
    await waitFor(() => expect(stat(document.querySelector(".mach")!)).toBe("offline"));
    const card = document.querySelector(".mach")!;
    expect(card.className).toContain("absent");
    const scope = card.querySelectorAll('[data-testid="fleet-token-scope"]');
    expect(scope).toHaveLength(1);
    expect(scope[0].querySelector('.token-scope-bezel[data-state="off"]')).not.toBeNull();
    expect(card.querySelector('[data-testid="token-scope-probe"]')).toBeNull();
    expect(card.querySelector(".mach-scope__rate")).toBeNull();
  });

  // (#2965) A failed flow read is not an answer that nothing happened. Both
  // `/flow/<day>` reads fail while presence shows the machine beating: the
  // records that would say it is working are exactly the ones missing, so
  // "idle · 0 running" is a claim nothing read. The card holds "no signal"
  // (and the app-level `FlowReadNotice` names the failure).
  for (const [what, failing] of [
    ["both days", () => [`/flow/${todayUTC()}`, `/flow/${prevDateUTC(todayUTC())}`]],
    ["today alone", () => [`/flow/${todayUTC()}`]],
  ] as const) {
    it(`says 'no signal', not 'idle', when the flow read fails (${what})`, async () => {
      const paths: string[] = failing();
      mockFleetFetch({ machines: BEAT, specs: SPECS, runs: [], fail: Object.fromEntries(paths.map((p) => [p, 500])) });
      const queryClient = newClient();
      renderFleetLens({}, queryClient);
      await waitForFleetQueriesSettled(queryClient);
      // Every source has answered, the failed flow read included: nothing is
      // still loading, so "no signal" here is the failure's, not a load's.
      await waitFor(() => {
        for (const key of [queryKeys.fleetSessionsLive(), queryKeys.runs(), queryKeys.flowDate(todayUTC()), queryKeys.flowDate(prevDateUTC(todayUTC()))]) {
          expect(queryClient.getQueryState(key)?.status, JSON.stringify(key)).toBe("success");
        }
      });
      await waitFor(() => expect(document.querySelector('.fleet-lens[data-state="loaded"]')).not.toBeNull());
      await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
      for (const card of Array.from(document.querySelectorAll(".mach"))) {
        expect(stat(card)).toBe("no signal");
        expect(card.textContent).not.toContain("idle");
        expect(card.textContent).not.toContain("offline");
        expect(card.querySelector(".runs")!.textContent).toBe("—");
        expect(cardScope(card)).toMatchObject({ state: "nosignal" });
        expect(utilLabel(card)).toMatch(/no signal$/);
      }
      // The token panel's zeros are a negative claim off the same read: it
      // keeps its loading silhouette rather than counting up to "0".
      expect(document.querySelector(".savings")!.getAttribute("data-settled")).toBe("false");
    });
  }

  // (#2965 review) A failure at the UTC-midnight rollover: the new day's
  // first read fails. The latch that keeps a PENDING new day from blinking
  // the cards back must not also hide a FAILED one; and the failed day is
  // retried, so the card heals on its own once the read succeeds.
  it("a failed read of the new day at UTC midnight says 'no signal', then heals when the retry succeeds", async () => {
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    vi.setSystemTime(new Date("2026-06-15T23:59:58.000Z"));
    mockFleetFetch({ machines: BEAT, runs: [], failOnce: ["/flow/2026-06-16"], emptyDays: ["2026-06-16"] });
    renderFleetLens();
    await waitFor(() => expect(stat(document.querySelector(".mach")!)).toBe("idle"));
    vi.setSystemTime(new Date("2026-06-16T00:00:03.000Z"));
    await act(async () => {
      vi.advanceTimersByTime(5_000);
    });
    await waitFor(() => expect(stat(document.querySelector(".mach")!)).toBe("no signal"));
    expect(document.querySelector(".savings")!.getAttribute("data-settled")).toBe("false");
    await act(async () => {
      vi.advanceTimersByTime(21_000);
    });
    await waitFor(() => expect(stat(document.querySelector(".mach")!)).toBe("idle"));
    expect(document.querySelector(".savings")!.getAttribute("data-settled")).toBe("true");
  });

  // (#2965) The inverted case: the same machine, the same sources, every read
  // healthy, reads "idle". Without it the test above passes for a card that
  // could never say "idle" at all.
  it("the same fleet with healthy flow reads says 'idle' — the inverted case", async () => {
    mockFleetFetch({ machines: BEAT, specs: SPECS, runs: [] });
    renderFleetLens({}, newClient());
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    await waitFor(() => {
      for (const card of Array.from(document.querySelectorAll(".mach"))) expect(stat(card)).toBe("idle");
    });
  });

  it("a replay has its records in hand and never shows 'no signal'", async () => {
    const records = normAll([
      { ts: "2026-08-26T10:00:00.000Z", machine_uid: "u1", machine_id: "m5", action: "machine.online", source: "presence_reconciler" },
    ]);
    renderFleetLens({ records, tMin: Date.parse("2026-08-26T09:00:00.000Z"), tMax: Date.parse("2026-08-26T10:00:00.000Z"), historical: true });
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(stat(document.querySelector(".mach")!)).toBe("idle");
  });
});

describe("FleetLens — the token hero distinguishes waiting from zero (#2817)", () => {
  it("silhouettes the figures while the flow window is still loading", async () => {
    // A fetch that never resolves: the window stays pending, which is the
    // several seconds the operator saw reading as legitimate data.
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));

    renderFleetLens();

    const hero = await waitFor(() => {
      const el = document.querySelector(".savings");
      expect(el).not.toBeNull();
      return el as HTMLElement;
    });
    expect(hero.getAttribute("data-settled")).toBe("false");
    expect(hero.getAttribute("aria-busy")).toBe("true");
  });

  it("shows a real 0 once the window settles with no dispatches", async () => {
    mockFleetFetch({ flowToday: [], flowYesterday: [] });

    renderFleetLens();

    const hero = await waitFor(() => {
      const el = document.querySelector('.savings[data-settled="true"]');
      expect(el).not.toBeNull();
      return el as HTMLElement;
    });
    expect(hero.getAttribute("aria-busy")).not.toBe("true");
    expect(
      hero.textContent,
      "a settled empty fleet HAS used no tokens — that zero is a measurement and must survive",
    ).toMatch(/0/);
  });
});

// (#2830) While the hero is loading, the real figures must not be visible.
//
// #2817 tried to achieve that with `color: transparent` plus a `::after`
// skeleton. Both halves fail. The colour is a specificity TIE against
// `.savlead.cloud .savnum` and `.savlead.unknown .savnum` (all three are
// (0,3,0)) which the variant rules win on source order, so the cloud and
// unattributed figures are never made transparent. And the skeleton is inset
// `0.12em/0.18em` inside a box whose `line-height: 1` is already shorter than
// the glyphs it paints, so whatever is still painted shows above and below it.
// The operator saw exactly that on a phone: local clean, cloud bleeding teal,
// unattributed bleeding grey.
//
// This test asserts the property that actually matters and that jsdom CAN
// judge: while unsettled, no tile renders figure text at all. "Is it visually
// covered" needs layout and is untestable here, which is precisely why the
// CSS-only approach shipped broken.
//
// It deliberately checks EVERY tile. An assertion written against the local
// tile alone passes on the broken build, because local is the one tile with no
// variant colour rule to beat it.
describe("savings hero: nothing leaks while loading (#2830)", () => {
  it("renders no figure text in any tile while the window is unsettled", async () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    renderFleetLens();

    const hero = await waitFor(() => {
      const el = document.querySelector('.savings[data-settled="false"]');
      expect(el, "the hero should be mounted and unsettled").toBeTruthy();
      return el as HTMLElement;
    });

    const figures = Array.from(hero.querySelectorAll(".savnum, .scv"));
    // Guard against a vacuous pass: if the selectors ever stop matching, an
    // empty list would satisfy the assertion below while proving nothing.
    expect(figures.length, "expected the hero's figure elements to exist").toBeGreaterThan(0);

    const leaked = figures
      .filter((el) => (el.textContent ?? "").trim() !== "")
      .map((el) => `${el.className}="${el.textContent}"`);
    expect(leaked, "no figure text may render while unsettled").toEqual([]);
  });

  // (#2886 pass 4, do-it — fresh-reviewer finding 7, "add tests for... both
  // dimmed renders") The DOM-level counterparts to `cards.test.ts`'s
  // data-layer coverage of `liveTokCarried`/the half-open no-signal read —
  // this proves the JSX actually stamps `data-carried`/"no signal" from
  // those fields, not just that the underlying derivation is correct.
  describe("the TOK/S rate line's carried and no-signal renders", () => {
    // Hardcoded to FROZEN_NOW's own date rather than `todayUTC()` — this
    // `describe` body runs at COLLECTION time, before `beforeEach`'s fake
    // timers are installed, so `todayUTC()` here would read the REAL
    // wall-clock date and build timestamps chronologically AFTER
    // FROZEN_NOW, which fails every `T(ts) <= t` liveness check silently
    // (found live: the card rendered "idle" with a contradictory
    // "1 running" tap target).
    // Anchored so the LAST heartbeat sits 2s before FROZEN_NOW (10:02:00) —
    // fresh under STALL_AFTER_MS (30s).
    const t1a = "2026-06-15T10:00:00.000Z";
    const t1b = "2026-06-15T10:00:02.000Z";
    const t1c = "2026-06-15T10:00:04.000Z";
    const t2 = "2026-06-15T10:01:58.000Z";

    it("marks the rate line (data-carried=true) when the reading is carried forward from an earlier turn", async () => {
      mockFleetFetch({
        flowToday: [
          { ts: t1a, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
          // Turn 1 opens at 0 (every turn does — finding 2), then two
          // real-progress intervals (400 chars/s each).
          { ts: t1a, machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: Date.parse(t1a), generated_chars: 0, turn_seq: 1 } },
          { ts: t1b, machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: Date.parse(t1b), generated_chars: 800, turn_seq: 1 } },
          { ts: t1c, machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: Date.parse(t1c), generated_chars: 1_600, turn_seq: 1 } },
          // Turn 2: lone first heartbeat — nothing of its own to read from yet.
          { ts: t2, machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: Date.parse(t2), generated_chars: 50, turn_seq: 2 } },
        ],
      });
      renderFleetLens({ connected: true });

      const rate = await waitFor(() => {
        const el = document.querySelector(".mach-scope__rate");
        expect(el, "the rate line should be mounted").toBeTruthy();
        return el as HTMLElement;
      });
      // Turn 1: 800 chars / 2s = 400 chars/s -> 100 tok/s at the default.
      expect(rate.textContent).toContain("100");
      expect(rate.getAttribute("data-carried")).toBe("true");
    });

    it("shows literal 'no signal' text, not 'stalled', when the page is disconnected over an otherwise-stale heartbeat", async () => {
      mockFleetFetch({
        flowToday: [
          { ts: t1a, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
          { ts: t1a, machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: Date.parse(t1a), generated_chars: 40 } },
          { ts: t1b, machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: Date.parse(t1b), generated_chars: 120 } },
        ],
      });
      renderFleetLens({ connected: false });

      // (#2955 review) "no signal" looks the same everywhere: the plain
      // status line with the dim dot (`.mach.nosignal`), never the lit
      // reading style, even on a card whose machine is running.
      await waitFor(() => expect(document.querySelector('[data-testid="fleet-token-scope"]')).not.toBeNull());
      const card = document.querySelector(".mach")!;
      await waitFor(() => expect(card.querySelector(".stat")!.textContent).toBe("no signal"));
      expect(card.querySelector(".mach-scope__rate"), "not the reading style").toBeNull();
      expect(card.className, "the dim dot").toContain("nosignal");
      expect(card.className, "the machine is still running").toContain("active");
      expect(latestTokenScopeProps()).toMatchObject({ state: "nosignal" });
    });
  });
});

// (#2890) The fleet card's tube morphs through the same states as the run
// page's hero, and in TOOLS it gets the tool for its icon.
describe("FleetLens card scope: the tool icon (#2890)", () => {
  const D0 = Date.UTC(2026, 8, 24, 1, 0, 0);
  const at = (sec: number) => new Date(D0 + sec * 1000).toISOString();
  it("hands the card's scope the running call's tool while it is in TOOLS (#2963)", async () => {
    // search has completed; the turn's second call, read, runs.
    const records: NormRecord[] = normAll([
      { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "darkmux/coder" },
      { ts: at(1), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 1_000, generated_chars: 0 } },
      { ts: at(3), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 3_000, generated_chars: 800 } },
      { ts: at(4), machine_uid: "u1", session_id: "s1", action: "dispatch.turn", payload: { turn_seq: 1, tool_calls_count: 2, tool_names: ["search", "read"] } },
      { ts: at(5), machine_uid: "u1", session_id: "s1", action: "dispatch.tool", payload: { tool_name: "search" } },
    ]);
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={records} tMax={D0 + 6_000} tMin={D0} playhead={D0 + 6_000} historical />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector('[data-testid="fleet-token-scope"]')).not.toBeNull());
    expect(latestTokenScopeProps()).toMatchObject({ state: "tools", toolName: "read", size: "card" });
  });
});

describe("(#2911) the fleet card ticks while an execution is live", () => {
  const now = Date.parse(FROZEN_NOW);
  const ago = (ms: number) => new Date(now - ms).toISOString();

  it("the REST countdown counts down with NO new records, and the clock stops when the lens unmounts", async () => {
    // `Date` is frozen by the file's beforeEach; this test also fakes the
    // interval so a tick of the shared clock is an asserted event.
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    vi.setSystemTime(new Date(FROZEN_NOW));
    mockFleetFetch({
      flowToday: [
        { ts: ago(20_000), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s-rest", action: "dispatch.start", handle: "darkmux/coder" },
        { ts: ago(10_000), machine_uid: "u1", session_id: "s-rest", action: "dispatch.rest", payload: { ms: 30_000 } },
      ],
    });
    const r = renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach-scope__rate")?.textContent).toBe("rest 20s"));
    expect(__clockDebug().running).toBe(true);
    act(() => {
      vi.advanceTimersByTime(3_000);
    });
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("rest 17s");
    expect(latestTokenScopeProps()).toMatchObject({ state: "rest", centerLabel: "17s", centerUnit: "resting" });
    // (#2961) Live: the rest's end and the wall clock, read per frame.
    expect(latestTokenScopeProps()).toMatchObject({ restEndMs: now + 20_000, clock: { kind: "wall" } });
    r.unmount();
    expect(__clockDebug().running).toBe(false);
  });

  it("a tick recomputes the card, not the flow window", async () => {
    // The tick exists to move the card's clock-bound state (a REST
    // countdown, a stall, the live TTL). The window merge and the
    // per-session index built over it depend only on records, and a tick
    // brings none: rebuilding them every second was a ~100 ms hitch per
    // second on a busy day. Measured here as "no new session index is built
    // across ticks", which pins the window half: the merged array stayed the
    // same object. It does NOT pin that the card's lookups use the index (a
    // lookup reverted to a whole-window scan builds nothing either); that
    // half is pinned where each lookup lives, in `flow.test.ts` (the run
    // index) and `cards.test.ts` (the heartbeat reads).
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    vi.setSystemTime(new Date(FROZEN_NOW));
    mockFleetFetch({
      flowToday: [
        { ts: ago(20_000), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s-rest", action: "dispatch.start", handle: "darkmux/coder" },
        { ts: ago(10_000), machine_uid: "u1", session_id: "s-rest", action: "dispatch.rest", payload: { ms: 30_000 } },
      ],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach-scope__rate")?.textContent).toBe("rest 20s"));
    const builds = __runIndexBuilds();
    for (let i = 0; i < 3; i++) {
      act(() => {
        vi.advanceTimersByTime(1_000);
      });
    }
    // The card DID recompute: the countdown moved three seconds.
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("rest 17s");
    expect(__runIndexBuilds()).toBe(builds);
  });

  it("the flow-derived live TTL expires on the tick, with no new record", async () => {
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    vi.setSystemTime(new Date(FROZEN_NOW));
    mockFleetFetch({
      flowToday: [
        { ts: ago(DEFAULT_POLICY.staleAfterMs - 500), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s-old", action: "dispatch.start", handle: "darkmux/coder" },
        { ts: ago(DEFAULT_POLICY.staleAfterMs - 1_500), machine_uid: "u1", session_id: "s-old", action: "dispatch.rest", payload: { ms: 600_000 } },
      ],
    });
    renderFleetLens();
    // (#2955) A running card's status line may hold its reading ("rest
    // 10m") rather than "dispatch in flight", so "running" is the card's
    // `active` class here, and "idle" is the status line's own word.
    await waitFor(() => expect(document.querySelector(".mach")?.className).toContain("active"));
    act(() => {
      vi.advanceTimersByTime(2_000);
    });
    expect(document.querySelector(".mach")!.className).not.toContain("active");
    expect(document.querySelector(".mach .stat")!.textContent).toBe("idle");
  });

  it("an online machine with nothing running drives no clock at all", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.now() }],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(latestTokenScopeProps()).toMatchObject({ state: "idle" });
    expect(__clockDebug()).toEqual({ listeners: 0, running: false });
  });

  it("a replay never ticks, whatever is running at the playhead", async () => {
    const D0 = Date.parse("2026-08-26T10:00:00.000Z");
    const at = (sec: number) => new Date(D0 + sec * 1000).toISOString();
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens
          records={normAll([
            { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.start", handle: "reviewer" },
            { ts: at(3), machine_uid: "u1", session_id: "s2", action: "dispatch.rest", payload: { ms: 15_000 } },
          ])}
          tMax={D0 + 5000}
          tMin={D0}
          playhead={D0 + 5000}
          historical
        />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__rate")?.textContent).toBe("rest 13s"));
    expect(__clockDebug().running).toBe(false);
  });
});

describe("(#2911) fleet card wording", () => {
  const D0 = Date.parse("2026-08-26T10:00:00.000Z");
  const at = (sec: number) => new Date(D0 + sec * 1000).toISOString();
  function renderAt(records: NormRecord[], playheadSec: number) {
    return render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={records} tMax={D0 + playheadSec * 1000} tMin={D0} playhead={D0 + playheadSec * 1000} historical />
      </QueryClientProvider>,
    );
  }

  it("thinking with no rate yet reads '— think tok/s', not a bare '—'", async () => {
    // One heartbeat with reasoning chars and no visible chars: thinking,
    // and (one sample) no trusted pair to rate.
    renderAt(
      normAll([
        { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        { ts: at(2), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 2000, generated_chars: 300, cumulative_chars: 0 } },
      ]),
      4,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__rate")).not.toBeNull());
    const line = document.querySelector(".mach-scope__rate")!;
    expect(line.textContent).toBe("— think tok/s");
    expect(line.getAttribute("data-thinking")).toBe("true");
    expect(latestTokenScopeProps()).toMatchObject({ state: "generating", thinking: true, tokensPerSec: null, centerLabel: "—" });
  });

  it("a mission between model steps: 'dispatch in flight' above a tube that says 'no model working', not 'idle'", async () => {
    // A running lab run and no execution: active, nothing generating.
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.now() }],
      runs: [{ id: "lab-1", kind: "lab", status: "running", machine: "MacBook-Pro", tracked: true }],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(document.querySelector(".mach")!.textContent).toContain("dispatch in flight");
    expect(latestTokenScopeProps()).toMatchObject({ state: "idle", centerUnit: "no model working" });
  });

  // (#2955, operator 2026-09-27) The card is one height in every state, so a
  // live reading takes the status line's place instead of a line of its own:
  // the reading IS the status line, dot included, and the text rows beside
  // the tube are the same two ("stat", "runs") running or not.
  const textRows = (card: Element) =>
    [...card.querySelector(".mach-body--scope")!.children].filter((c) => !c.classList.contains("mach-scope")).map((c) => c.className.split(" ")[0]);

  it("(#2955) a live reading rides the status line: no extra row", async () => {
    renderAt(
      normAll([
        { ts: at(0), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
        { ts: at(0), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0, generated_chars: 0 } },
        { ts: at(2), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: D0 + 2000, generated_chars: 800 } },
      ]),
      3,
    );
    await waitFor(() => expect(document.querySelector(".mach-scope__rate")).not.toBeNull());
    const card = document.querySelector(".mach")!;
    const stat = card.querySelector(".stat")!;
    expect(card.querySelector(".mach-scope__rate"), "the reading is the status line itself").toBe(stat);
    expect(stat.querySelector(".dot"), "the status dot stays on the line").not.toBeNull();
    expect(stat.textContent).toBe("100 tok/s");
    expect(textRows(card)).toEqual(["stat", "runs"]);
  });

  it("(#2955) a card with nothing running has the same two text rows", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.now() }],
      runs: [{ id: "lab-1", kind: "lab", status: "running", machine: "MacBook-Pro", tracked: true }],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(document.querySelector(".mach .stat")!.textContent).toBe("dispatch in flight");
    expect(textRows(document.querySelector(".mach")!)).toEqual(["stat", "runs"]);
  });

  it("a machine with nothing running still says 'idle' in the tube", async () => {
    mockFleetFetch({
      machines: [{ machine_uid: "u1", display_name: "MacBook-Pro", schema_version: "1.43.0", beat_ts_ms: Date.now() }],
    });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach")).not.toBeNull());
    expect(document.querySelector(".mach")!.textContent).toContain("idle");
    expect(latestTokenScopeProps()).toMatchObject({ state: "idle", centerUnit: "idle" });
  });
});

// (#2911) Live, the hero counts records as of the viewer's clock, as it did
// before #2911 and as the fleet cards do: a record stamped ahead of now (a
// peer whose clock runs fast) is left out until the clock reaches it. A
// replay gates on the playhead instead. The gate must not cost a
// whole-window filter per 1 Hz tick; `recordsAsOf` (flow.ts) is what keeps
// it off that path, and the last two tests pin that from the lens.
describe("(#2911) a record stamped ahead of the viewer's clock", () => {
  const records = () => {
    const today = todayUTC();
    return [
      { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
      // FROZEN_NOW is 10:02; both of these are two minutes in its future.
      { ts: `${today}T10:04:00.000Z`, machine_uid: "u1", session_id: "s1", action: "dispatch.complete", payload: { total_tokens: 600 } },
    ];
  };

  it("is excluded from the live hero while ahead of now, and counted once now passes it", async () => {
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    vi.setSystemTime(new Date(FROZEN_NOW));
    expect(Date.now()).toBeLessThan(Date.parse(`${todayUTC()}T10:04:00.000Z`));
    mockFleetFetch({ flowToday: records() });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".savings .savnum")?.textContent).toBe("0"));
    // The dispatch is running (its completion is still ahead), so the lens
    // ticks; 2m05s of ticks carry the clock past 10:04.
    act(() => {
      vi.advanceTimersByTime(125_000);
    });
    await waitFor(() => expect(document.querySelector(".savings .savnum")?.textContent).toBe("600"));
  });

  it("is excluded under a playhead before it", async () => {
    const today = todayUTC();
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens
          records={normAll(records())}
          tMax={Date.parse(`${today}T10:04:00.000Z`)}
          tMin={Date.parse(`${today}T09:00:00.000Z`)}
          playhead={Date.parse(`${today}T10:02:00.000Z`)}
          historical
        />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".savings .savnum")?.textContent).toBe("0"));
  });

  it("with nothing ahead of now, a tick hands the hero the window itself: no filter, no token recompute", async () => {
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    vi.setSystemTime(new Date(FROZEN_NOW));
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s-rest", action: "dispatch.start", handle: "darkmux/coder" },
        { ts: `${today}T10:01:50.000Z`, machine_uid: "u1", session_id: "s-rest", action: "dispatch.rest", payload: { ms: 30_000 } },
      ],
    });
    const filters = __asOfFilterRuns();
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".mach-scope__rate")?.textContent).toBe("rest 20s"));
    const calls = vi.mocked(tokensOffMeter).mock.calls.length;
    for (let i = 0; i < 3; i++) {
      act(() => {
        vi.advanceTimersByTime(1_000);
      });
    }
    // The lens DID re-render on each tick: the countdown moved.
    expect(document.querySelector(".mach-scope__rate")!.textContent).toBe("rest 17s");
    // Nothing is ahead of now, so the hero was never handed a filtered copy.
    expect(__asOfFilterRuns()).toBe(filters);
    expect(vi.mocked(tokensOffMeter).mock.calls.length).toBe(calls);
  });

  it("with a record ahead of now, a tick that crosses nothing does not re-filter", async () => {
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    vi.setSystemTime(new Date(FROZEN_NOW));
    mockFleetFetch({ flowToday: records() });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".savings .savnum")?.textContent).toBe("0"));
    const filters = __asOfFilterRuns();
    const calls = vi.mocked(tokensOffMeter).mock.calls.length;
    for (let i = 0; i < 3; i++) {
      act(() => {
        vi.advanceTimersByTime(1_000);
      });
    }
    // Still ahead: three ticks re-rendered the lens and filtered nothing.
    expect(__clockDebug().running).toBe(true);
    expect(__asOfFilterRuns()).toBe(filters);
    expect(vi.mocked(tokensOffMeter).mock.calls.length).toBe(calls);
    expect(document.querySelector(".savings .savnum")?.textContent).toBe("0");
  });
});

// (#2926) The fleet card over the same real run, in playback: the rate line
// under the tube is where its live text lives.
describe("(#2926) fleet card: THINK opener and TOOL GEN, from the real run", () => {
  function renderAt(records: NormRecord[], playhead: number) {
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={records} tMax={playhead} tMin={pepperAt("10:51:00")} playhead={playhead} historical />
      </QueryClientProvider>,
    );
  }
  const rateLine = () =>
    waitFor(() => {
      const el = document.querySelector(".mach-scope__rate");
      expect(el, "the rate line should be mounted").toBeTruthy();
      return el as HTMLElement;
    });
  const extra = { machine_uid: "u1" };

  it("turn 7's stream-open chunk: the previous turn's rate, dimmed, never ~1 think tok/s", async () => {
    renderAt(pepperRecords({ extra }), pepperAt("10:51:33"));
    const rate = await rateLine();
    expect(rate.getAttribute("data-thinking")).toBe("true");
    expect(rate.getAttribute("data-carried")).toBe("true");
    expect(Number(rate.textContent?.split(" ")[0])).toBeGreaterThan(50);
    expect(latestTokenScopeProps()).toMatchObject({ state: "generating", thinking: true, centerCarried: true });
  });

  it("the same opener on a session's first turn: '— think tok/s', no figure", async () => {
    renderAt(pepperRecords({ minTurn: 7, extra }), pepperAt("10:51:33"));
    const rate = await rateLine();
    expect(rate.textContent).toBe("— think tok/s");
    expect(latestTokenScopeProps()).toMatchObject({ state: "generating", centerLabel: "—" });
  });

  it("turn 10 writing a `write` call: 'tool gen · write · 18s' on the rate line, the tube keeps just 'tool gen'", async () => {
    renderAt(pepperRecords({ extra }), pepperAt("10:52:48.500"));
    const rate = await rateLine();
    expect(rate.textContent).toBe("tool gen · write · 18s");
    expect(latestTokenScopeProps()).toMatchObject({ state: "tools", toolName: "write", toolWriting: true, centerLabel: null, centerUnit: "tool gen" });
  });
});

// (#2921) A machine the window only knows by its hardware uid must never be
// labeled with that uid: it lands in screenshots and identifies the machine.
describe("(#2921) fleet page: no hardware uid is ever rendered as a label", () => {
  // Fixture uid, uppercase like the real ones; not any real machine's.
  const FAKE_UID = "00000000-0000-4000-8000-ABCDEF000001";
  const UUID_RE = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i;
  function renderFleet(records: NormRecord[]) {
    const playhead = pepperAt("10:51:33");
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={records} tMax={playhead} tMin={pepperAt("10:51:00")} playhead={playhead} historical />
      </QueryClientProvider>,
    );
  }
  const uidShapedText = () => {
    const found: string[] = [];
    if (UUID_RE.test(document.body.textContent ?? "")) found.push("text");
    for (const attr of ["title", "aria-label"]) {
      for (const el of document.querySelectorAll(`[${attr}]`)) {
        if (UUID_RE.test(el.getAttribute(attr) ?? "")) found.push(`${attr} on .${el.className}`);
      }
    }
    return found;
  };

  it("records carrying only machine_uid: card and lane read 'unnamed machine', no uid text or title", async () => {
    renderFleet(pepperRecords({ extra: { machine_uid: FAKE_UID, machine_id: undefined } }));
    await waitFor(() => expect(document.querySelector(".lane .lname")).toBeTruthy());
    expect(document.querySelector(".lane .lname")?.textContent).toBe("unnamed machine");
    expect(document.querySelector(".mach-name")?.textContent).toContain("unnamed machine");
    expect(uidShapedText()).toEqual([]);
  });

  // The reported scenario: a live daemon with an empty DARKMUX_HOME (no
  // roster), whose records carry only the uid.
  const liveUidOnly = () => [
    { ts: new Date(Date.now() - 60_000).toISOString(), action: "dispatch.start", session_id: "s-live", machine_uid: FAKE_UID, handle: "coder" },
  ];
  it("live, no roster, no specs: the lane and card read 'unnamed machine'", async () => {
    mockFleetFetch({ flowToday: liveUidOnly() });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".lane .lname")).toBeTruthy());
    expect(document.querySelector(".lane .lname")?.textContent).toBe("unnamed machine");
    expect(uidShapedText()).toEqual([]);
  });

  it("live, this daemon's own uid: its specs name titles the lane like the card", async () => {
    mockFleetFetch({ flowToday: liveUidOnly(), specs: { machine_id: "scratch-box", machine_uid: FAKE_UID } });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".lane .lname")?.textContent).toBe("scratch-box"));
    expect(document.querySelector(".mach-name")?.textContent).toContain("scratch-box");
    expect(uidShapedText()).toEqual([]);
  });

  it("the record's machine_id names the machine when present", async () => {
    renderFleet(pepperRecords({ extra: { machine_uid: FAKE_UID } }));
    await waitFor(() => expect(document.querySelector(".lane .lname")).toBeTruthy());
    expect(document.querySelector(".lane .lname")?.textContent).toBe("MacBook-Pro");
    expect(uidShapedText()).toEqual([]);
  });
});

describe("(#2921 follow-up) fleet page: roster names and unnamed ordinals", () => {
  const A = "00000000-0000-4000-8000-ABCDEF000001";
  const B = "00000000-0000-4000-8000-ABCDEF000003";
  const UUID_RE = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i;
  const start = (uid: string, sid: string, agoMs: number) => ({
    ts: new Date(Date.now() - agoMs).toISOString(),
    action: "dispatch.start",
    session_id: sid,
    machine_uid: uid,
    handle: "coder",
  });
  const names = (sel: string) => [...document.querySelectorAll(sel)].map((el) => el.textContent ?? "");

  it("a uid-only machine with a roster entry reads its roster id on card and lane", async () => {
    mockFleetFetch({ flowToday: [start(A, "s-a", 60_000)], roster: [{ id: "studio", address: "100.64.1.2:8765", added_unix_ms: 1000, machine_uid: A }] });
    renderFleetLens();
    await waitFor(() => expect(document.querySelector(".lane .lname")?.textContent).toBe("studio"));
    expect(names(".mach-name").join(" ")).toContain("studio");
    expect(UUID_RE.test(document.body.textContent ?? "")).toBe(false);
  });

  it("two unnamed machines: distinct ordinals, and each machine's card and lane agree", async () => {
    mockFleetFetch({ flowToday: [start(B, "s-b", 120_000), start(A, "s-a", 60_000)] });
    renderFleetLens();
    await waitFor(() => expect(document.querySelectorAll(".lane .lname")).toHaveLength(2));
    const lanes = names(".lane .lname");
    expect(new Set(lanes)).toEqual(new Set(["unnamed machine", "unnamed machine 2"]));
    const cards = [...document.querySelectorAll<HTMLElement>(".mach[data-arg]")];
    // (#2929) `data-arg` carries the machine key, never the uid.
    const cardName = (uid: string) =>
      cards.find((c) => c.getAttribute("data-arg") === `unnamed-${machineKeyHash(uid).slice(0, 6)}`)?.querySelector(".mach-name")?.textContent;
    for (const c of cards) expect(UUID_RE.test(c.getAttribute("data-arg") ?? "")).toBe(false);
    // B was seen first.
    expect(cardName(B)).toBe("unnamed machine");
    expect(cardName(A)).toBe("unnamed machine 2");
    expect(new Set(names(".mach-name"))).toEqual(new Set(lanes));
  });
});

// (#2929) The machine a card link names rides in the address bar, so it is a
// machine KEY (the machine's name, or "unnamed-<n>"), never the hardware uid.
// FAKE uuids in the repo's fake form with hex LETTERS in the tail, one lowercase
// and the rest uppercase, so a leak would be caught case-insensitively.
describe("(#2929) fleet-card links carry a machine key, never the hardware uid", () => {
  const UUID_RE = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i;
  const NAMED = "00000000-0000-4000-8000-ABCDEF000001";
  const UNNAMED_1 = "00000000-0000-4000-8000-abcdef000002";
  const UNNAMED_2 = "00000000-0000-4000-8000-ABCDEF000004";

  function mountThree() {
    const today = todayUTC();
    mockFleetFetch({
      flowToday: [
        { ts: `${today}T10:00:00.000Z`, machine_uid: NAMED, machine_id: "studio", session_id: "s1", action: "dispatch.start", handle: "coder" },
        { ts: `${today}T10:00:01.000Z`, machine_uid: UNNAMED_1, session_id: "s2", action: "dispatch.start", handle: "coder" },
        { ts: `${today}T10:00:01.500Z`, machine_uid: UNNAMED_1, session_id: "s3", action: "dispatch.start", handle: "coder" },
        { ts: `${today}T10:00:02.000Z`, machine_uid: UNNAMED_2, session_id: "s4", action: "dispatch.start", handle: "coder" },
      ],
    });
    renderFleetLens();
  }
  const cardNamed = (name: string) =>
    [...document.querySelectorAll(".mach")].find((c) => c.querySelector(".name")?.textContent?.trim().endsWith(name))!;

  it("card body click and Enter: a name, or distinct unnamed ordinals — no uid in any hash", async () => {
    mountThree();
    await waitFor(() => expect(document.querySelectorAll(".mach").length).toBe(3));
    const hashes: Record<string, string[]> = {};
    for (const name of ["studio", "unnamed machine", "unnamed machine 2"]) {
      const card = cardNamed(name);
      expect(card, name).toBeTruthy();
      window.location.hash = "";
      fireEvent.click(card);
      const byClick = window.location.hash;
      window.location.hash = "";
      fireEvent.keyDown(card, { key: "Enter" });
      hashes[name] = [byClick, window.location.hash];
    }
    expect(hashes["studio"]).toEqual(["#lens=runs&machine=studio", "#lens=runs&machine=studio"]);
    const k1 = `unnamed-${machineKeyHash(UNNAMED_1).slice(0, 6)}`;
    const k2 = `unnamed-${machineKeyHash(UNNAMED_2).slice(0, 6)}`;
    expect(hashes["unnamed machine"]).toEqual([`#lens=runs&machine=${k1}`, `#lens=runs&machine=${k1}`]);
    expect(hashes["unnamed machine 2"]).toEqual([`#lens=runs&machine=${k2}`, `#lens=runs&machine=${k2}`]);
    for (const h of Object.values(hashes).flat()) expect(UUID_RE.test(h), h).toBe(false);
    // (C6) The card's `data-arg` hook is the same key, never the uid.
    expect(cardNamed("unnamed machine 2").getAttribute("data-arg")).toBe(k2);
    for (const c of document.querySelectorAll(".mach[data-arg]")) expect(UUID_RE.test(c.getAttribute("data-arg") ?? "")).toBe(false);
  });

  it("the running-count tap (2 live runs) carries the key too", async () => {
    mountThree();
    await waitFor(() => expect(document.querySelectorAll(".mach").length).toBe(3));
    const card = cardNamed("unnamed machine");
    const count = card.querySelector(".runs--live");
    expect(count).not.toBeNull();
    window.location.hash = "";
    fireEvent.click(count!);
    expect(window.location.hash).toBe(`#lens=runs&machine=unnamed-${machineKeyHash(UNNAMED_1).slice(0, 6)}`);
    expect(UUID_RE.test(window.location.hash)).toBe(false);
  });
});

// (#2915) Utility work on the fleet card, over the real run: the work model's
// tube reads "compacting" while its execution compacts, and the machine's
// utility strip shows the job (a radio signal while routing, the generic
// indicator for a job this build has no visual for), quiet otherwise.
describe("(#2915) fleet card: utility work is visible", () => {
  function renderAt(records: NormRecord[], playhead: number) {
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <FleetLens records={records} tMax={playhead} tMin={pepperAt("10:51:00")} playhead={playhead} historical />
      </QueryClientProvider>,
    );
  }
  const extra = { machine_uid: "u1" };
  const SID = pepperRecords({ extra })[0].session_id;
  const rec = (hms: string, action: string, payload: Record<string, unknown>, more: Record<string, unknown> = {}) =>
    norm({ ts: `2026-09-26T${hms}Z`, action, category: "telemetry", machine_uid: "u1", machine_id: "pepper", payload, ...more });
  // Turn 9's tool completes at 10:52:09; turn 10's opener is 10:52:22.
  const compactStart = rec("10:52:10", "utility.start", { job: "compaction", model: "darkmux:util-4b", serves: SID, stall_after_seconds: 600 }, { session_id: SID, source: "utility", handle: "compactor" });
  const compactEnd = rec("10:52:20", "telemetry.tokens", { purpose: "utility", call_kind: "compaction", job: "compaction", total_tokens: 900 }, { session_id: SID, source: "tokens", handle: "compactor" });
  const routeStart = (job: string) => rec("10:52:12", "utility.start", { job, model: "darkmux:util-4b", stall_after_seconds: 30 }, { source: "utility", handle: "radio-router" });
  const routeEnd = rec("10:52:14", "telemetry.tokens", { purpose: "utility", call_kind: "single_shot", job: "radio_routing", total_tokens: 40 }, { source: "tokens", handle: "radio-router" });
  const rateLine = () =>
    waitFor(() => {
      const el = document.querySelector(".mach-scope__rate");
      expect(el).toBeTruthy();
      return el as HTMLElement;
    });
  const strip = () =>
    waitFor(() => {
      const el = document.querySelector('[data-testid="fleet-utility"]');
      expect(el, "every card carries the utility strip").toBeTruthy();
      return el as HTMLElement;
    });

  it("compacting: the rate line counts, the tube reads 'compacting' with the utility treatment, PROMPT stays the state", async () => {
    renderAt([...pepperRecords({ extra }), compactStart], pepperAt("10:52:15"));
    expect((await rateLine()).textContent).toBe("compacting · 5s");
    expect(latestTokenScopeProps()).toMatchObject({ state: "prompt", centerLabel: null, centerUnit: "compacting", utility: true });
    expect((await strip()).getAttribute("data-visual")).toBe("compacting");
  });

  it("the compaction's usage record ends it: plain PROMPT, the brain, a quiet strip", async () => {
    renderAt([...pepperRecords({ extra }), compactStart, compactEnd], pepperAt("10:52:21"));
    expect((await rateLine()).textContent).toBe("processing prompt");
    const props = latestTokenScopeProps();
    expect(props).toMatchObject({ state: "prompt", centerUnit: null });
    expect(props.utility).toBeUndefined();
    expect((await strip()).getAttribute("data-visual")).toBe("quiet");
  });

  it("radio routing: the strip radiates while the job runs, and the work model's tube is untouched", async () => {
    const before = pepperAt("10:52:13");
    renderAt([...pepperRecords({ extra }), routeStart("radio_routing")], before);
    const el = await strip();
    expect(el.getAttribute("data-visual")).toBe("radio");
    expect(el.querySelectorAll(".mach-util__arc")).toHaveLength(2);
    expect(latestTokenScopeProps().utility).toBeUndefined();
  });

  it("radio routing ended: quiet", async () => {
    renderAt([...pepperRecords({ extra }), routeStart("radio_routing"), routeEnd], pepperAt("10:52:15"));
    const el = await strip();
    expect(el.getAttribute("data-visual")).toBe("quiet");
    expect(el.querySelectorAll(".mach-util__arc")).toHaveLength(0);
  });

  it("a job this build has no visual for gets the generic indicator", async () => {
    renderAt([...pepperRecords({ extra }), routeStart("dream_job")], pepperAt("10:52:13"));
    const el = await strip();
    expect(el.getAttribute("data-visual")).toBe("generic");
    expect(el.querySelector(".mach-util__ping")).toBeTruthy();
    expect(el.getAttribute("aria-label")).toContain("dream job");
  });

  it("a routing job with no end past its bound reads stalled", async () => {
    renderAt([...pepperRecords({ extra }), routeStart("radio_routing")], pepperAt("10:52:43"));
    expect((await strip()).getAttribute("data-stalled")).toBe("true");
  });

  it("the strip adds no text to the card (the model rides in the tooltip)", async () => {
    renderAt([...pepperRecords({ extra }), routeStart("radio_routing")], pepperAt("10:52:13"));
    const el = await strip();
    expect(el.textContent).toBe("");
    expect(el.getAttribute("title")).toContain("darkmux:util-4b");
  });
});

// (#2928) The live channel reaches the fleet card at the live edge only.
describe("(#2928) the live overlay on the rendered fleet card", () => {
  const at = (s: string) => `${todayUTC()}T${s}Z`;
  const records = (): NormRecord[] => normAll([
    { ts: at("10:01:00.000"), machine_uid: "u1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start", handle: "coder" },
    { ts: at("10:01:56.000"), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { turn_seq: 1, sampled_at_ms: Date.parse(at("10:01:56.000")), generated_chars: 40, cumulative_chars: 40 } },
    { ts: at("10:01:58.000"), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { turn_seq: 1, sampled_at_ms: Date.parse(at("10:01:58.000")), generated_chars: 120, cumulative_chars: 120 } },
  ]);
  const feedLive = async () => {
    const { liveStore } = await import("../../lib/liveChannel");
    liveStore.reset();
    for (const [t, gen] of [["10:01:59.000", 220], ["10:01:59.250", 420]] as const) {
      const ms = Date.parse(at(t));
      liveStore.ingest(JSON.stringify({ v: 1, kind: "model", session_id: "s1", at_ms: ms, cadence_ms: 250, fields: { turn_seq: 1, sampled_at_ms: ms, generated_chars: gen, cumulative_chars: gen } }), ms);
    }
    return liveStore;
  };

  it("live: the card's rate comes from the live samples (200 tok/s), not the 2 s heartbeats (10 tok/s)", async () => {
    const store = await feedLive();
    mockFleetFetch({ flowToday: records() });
    const { container } = renderFleetLens();
    await waitFor(() => expect(container.querySelector(".mach")?.textContent ?? "").toContain("200 tok/s"));
    expect(container.querySelector('[data-testid="fleet-token-scope"]')?.getAttribute("title")).toBeNull();
    store.reset();
  });

  it("live samples re-render the cards without rebuilding the activity timeline within the second", async () => {
    const { buildActivityTimeline } = await import("./timeline");
    const { liveStore } = await import("../../lib/liveChannel");
    liveStore.reset();
    mockFleetFetch({ flowToday: records() });
    const { container } = renderFleetLens();
    await waitFor(() => expect(container.querySelector(".mach")?.textContent ?? "").toContain("10 tok/s"));
    // Let every query the lens fires settle first (a late `/runs` answer is
    // a real input change and rightly rebuilds).
    await act(async () => {
      await new Promise((r) => setTimeout(r, 100));
    });
    const builds = vi.mocked(buildActivityTimeline).mock.calls.length;
    const { buildFleetCardBase } = await import("./cards");
    const bases = vi.mocked(buildFleetCardBase).mock.calls.length;
    // The wall clock moves on inside the same second, as it does between
    // samples.
    vi.setSystemTime(new Date(Date.parse(FROZEN_NOW) + 300));
    act(() => {
      for (const [t, gen] of [["10:01:59.000", 220], ["10:01:59.250", 420]] as const) {
        const ms = Date.parse(at(t));
        liveStore.ingest(JSON.stringify({ v: 1, kind: "model", session_id: "s1", at_ms: ms, cadence_ms: 250, fields: { turn_seq: 1, sampled_at_ms: ms, generated_chars: gen, cumulative_chars: gen } }), ms);
      }
    });
    await waitFor(() => expect(container.querySelector(".mach")?.textContent ?? "").toContain("200 tok/s"));
    // Still inside one wall second: the cards moved, the timeline did not rebuild.
    expect(vi.mocked(buildActivityTimeline).mock.calls.length).toBe(builds);
    expect(vi.mocked(buildFleetCardBase).mock.calls.length, "the card bases were not rebuilt for a live sample").toBe(bases);
    liveStore.reset();
  });

  it("replay: the same samples in the store change nothing (the durable 10 tok/s)", async () => {
    const store = await feedLive();
    mockFleetFetch({});
    const t = Date.parse(FROZEN_NOW);
    const { container } = renderFleetLens({ records: records(), tMax: t, tMin: Date.parse(at("10:00:00.000")), playhead: t, historical: true });
    await waitFor(() => expect(container.querySelector(".mach")?.textContent ?? "").toContain("tok/s"));
    expect(container.querySelector(".mach")!.textContent).toContain("10 tok/s");
    expect(container.querySelector(".mach")!.textContent).not.toContain("200 tok/s");
    expect(container.querySelector('[data-testid="fleet-token-scope"]')?.getAttribute("title")).toContain("one every 2 s");
    store.reset();
  });
});
