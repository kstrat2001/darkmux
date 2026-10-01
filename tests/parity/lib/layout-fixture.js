// The layout suites' shared fixture (`next-parity-layout-*.spec.ts`).
//
// Those suites pin one rule: a box the operator sized to fit (the fleet hero,
// a fleet card, the run page's MODEL section) keeps ONE size across every
// state it can show. Nothing appears or disappears and shifts the layout. Text
// that comes and goes goes into a slot that already exists, or into space
// that is always reserved.
//
// This file builds one synthetic UTC day per state: a single coder execution
// on one synthetic machine, whose records stop exactly where the state holds.
// The day's `now` is where a live page's clock is pinned, and (via a trailing
// inert record where the state depends on elapsed time) where a playback
// page's playhead rests. Record shapes follow `ui/src/testing/pepperGrinderRun.ts`
// (a real run, projected fields only).
//
// Every state also names the words it must show (`runText`, `rateText`):
// a suite asserts them before measuring anything, so a fixture that drifts
// into some other state (every page reading "finished", say) fails loudly
// instead of measuring one state N times and calling it constant.

const UID = "layout-fixture-machine";
const MACHINE = { machine_id: "layout-mac", machine_uid: UID };
const MID = "layout-mission";

const at = (d, hms) => `${d}T${hms}Z`;
const msOf = (d, hms) => Date.parse(at(d, hms));

function build(sid) {
  const rec = (ts, action, payload = {}, extra = {}) => ({ ts, action, session_id: sid, handle: "coder", ...MACHINE, payload, ...extra });
  const usage = (ts, { prompt, completion, cached, purpose = "work", callKind = "turn", handle = "coder" }) => {
    const payload = {
      call_kind: callKind,
      purpose,
      token_source: "provider",
      prompt_tokens: prompt,
      completion_tokens: completion,
      total_tokens: prompt + completion,
      requested_model: "qwen-layout",
      endpoint: "http://127.0.0.1:1234/v1",
    };
    if (cached != null) payload.cached_tokens = cached;
    if (callKind === "turn") payload.turn_seq = 1;
    return { ts, action: "telemetry.tokens", category: "telemetry", source: "tokens", session_id: sid, handle, model: "qwen-layout", ...MACHINE, payload };
  };
  const beat = (d, hms, turn, gen, visible, more = {}) =>
    rec(at(d, hms), "dispatch.turn.heartbeat", { turn_seq: turn, sampled_at_ms: msOf(d, hms), generated_chars: gen, cumulative_chars: visible, ...more });
  // Turn 1: read the prompt, generate, call one tool, which completes.
  const prefix = (d, extra = {}, withUsage = true, armed = false) =>
    [
      // (#2950) `armed`: the run's resolved bounds say the thermal governor
      // is on, as every dispatch since #2165 records it (`dispatch_internal.rs`
      // `thermal_pacing_enabled`). Only an armed governor writes a thermal
      // rest, so a thermal REST state is always armed, and ACTIVE TIME then
      // names its thermal rest from the run's start ("0 s thermal rest").
      rec(
        at(d, "12:00:00"),
        "dispatch.start",
        { prompt_chars: 3000, ...(armed ? { bounds: { thermal_pacing_enabled: { value: true, source: "config" } } } : {}) },
        { source: "crew_dispatch", category: "work", model: "darkmux:qwen-layout" },
      ),
      beat(d, "12:00:01", 1, 0, 0, { prompt_chars: 16000 }),
      beat(d, "12:00:03", 1, 600, 600),
      beat(d, "12:00:05", 1, 1400, 1400),
      rec(at(d, "12:00:06"), "dispatch.turn", { turn_seq: 1, tool_calls_count: 1, generation_ms: 5000 }),
      ...(withUsage ? [usage(at(d, "12:00:06"), { prompt: 4000, completion: 350 })] : []),
      rec(at(d, "12:00:07"), "dispatch.tool", { tool_name: "read" }),
    ].map((r) => ({ ...r, ...extra }));
  const opener = (d) => beat(d, "12:00:08", 2, 0, 0, { prompt_chars: 20000 });
  const complete = (d) => rec(at(d, "12:00:09"), "dispatch.complete", { completion_tokens: 350, prompt_tokens: 4000 });
  const writing = (d, name) => {
    const out = [opener(d), beat(d, "12:00:10", 2, 500, 500)];
    for (let s = 12; s <= 30; s += 2) out.push(beat(d, `12:00:${String(s).padStart(2, "0")}`, 2, 800, 500, { phase: "writing_tool_call", ...(name ? { tool_name: name } : {}) }));
    return out;
  };
  return { sid, rec, usage, beat, prefix, opener, complete, writing };
}

// A record that only moves the day's end (a playback page's playhead) to the
// state's `now`: no session, so no run reads it; the fixture machine's own
// identity, so it adds no second card.
const tick = (d, hms) => ({ ts: at(d, hms), action: "layout tick", category: "debug", source: "layout", ...MACHINE });

/** (#2902 step 5) A hosted call's budget wait, as the gate writes it
 *  (`budget.rs::announce_wait`), announced at 12:00:08. */
const budgetWait = (b, d, endpoint, secs) =>
  b.rec(
    at(d, "12:00:08"),
    "budget.wait",
    {
      scope: "endpoint", endpoint_id: endpoint, policy: "wait", metric: "tokens", spent: 2000, limit: 2000, period: "1d",
      wait_ms: secs * 1000, resume_at_ms: msOf(d, "12:00:08") + secs * 1000, pid: 1,
      message: `darkmux: endpoint \`${endpoint}\` has reached its budget`,
    },
    { category: "telemetry", source: "budget", level: "warn", model: "gpt-layout" },
  );

/** Every state the MODEL section and a fleet card can show. Days are two
 *  apart so a live page's 24h window never reaches a neighbor's records. */
const STATES = [
  {
    id: "finished", date: "2026-08-01", now: "12:00:30", runText: "finished", rateText: null,
    recs: (b, d) => [...b.prefix(d), b.complete(d)],
  },
  {
    id: "prompt", date: "2026-08-03", now: "12:00:09", runText: "processing prompt", rateText: "processing", utilLive: "idle",
    recs: (b, d) => [...b.prefix(d), b.opener(d)],
  },
  {
    id: "think", date: "2026-08-05", now: "12:00:12", runText: "generating, thinking", rateText: "think tok/s",
    recs: (b, d) => [...b.prefix(d), b.opener(d), b.beat(d, "12:00:10", 2, 900, 0), b.beat(d, "12:00:12", 2, 1800, 0)],
  },
  {
    id: "generating", date: "2026-08-07", now: "12:00:12", runText: /run state: generating$/, rateText: / tok\/s$/,
    recs: (b, d) => [...b.prefix(d), b.opener(d), b.beat(d, "12:00:10", 2, 900, 900), b.beat(d, "12:00:12", 2, 1800, 1800)],
  },
  {
    id: "toolgen-named", date: "2026-08-09", now: "12:00:30", runText: "tool gen · write · 18s", rateText: "tool gen",
    recs: (b, d) => [...b.prefix(d), ...b.writing(d, "write")],
  },
  {
    id: "toolgen-unnamed", date: "2026-08-11", now: "12:00:30", runText: /tool gen · 18\s?s/, rateText: "tool gen",
    recs: (b, d) => [...b.prefix(d), ...b.writing(d, null)],
  },
  {
    id: "tool-running", date: "2026-08-13", now: "12:00:15", runText: "run state: tools", rateText: "tools",
    recs: (b, d) => [
      ...b.prefix(d),
      b.opener(d),
      b.beat(d, "12:00:10", 2, 900, 900),
      b.rec(at(d, "12:00:12"), "dispatch.turn", { turn_seq: 2, tool_calls_count: 2, generation_ms: 4000 }),
      b.rec(at(d, "12:00:13"), "dispatch.tool", { tool_name: "bash" }),
      tick(d, "12:00:15"),
    ],
  },
  // (#2963) darkmux running the SECOND call of a turn whose first call, a
  // write, has completed: the readout names that second call's own tool
  // and file, from the turn record's `tool_names` / `tool_paths` (FLOW
  // 1.64.0), in the slot TOOL GEN and REST use. A long path trims from the
  // LEFT, so the file name stays (`fileName`: the suite checks it is on
  // screen). `tool-file-mixed`: write, then read, reads "read · …" (the
  // word is the running call's, never the completed write's).
  // `tool-file-unlisted`: a turn record with no lists (an older host): the
  // neutral TOOLS state, no line at all.
  ...[
    // Short enough to show whole in the desktop slot (240px there, the
    // tube's column; wider on a phone).
    ["tool-file", "2026-09-26", "write", "/workspace/src/tokenRate.ts", "write · src/tokenRate.ts", true],
    [
      "tool-file-long",
      "2026-09-28",
      "write",
      "/workspace/crates/darkmux-serve/assets/viewer/lenses/session/deeply/nested/folder/tree/tokenReadout.ts",
      "write · crates/darkmux-serve/assets/viewer/lenses/session/deeply/nested/folder/tree/tokenReadout.ts",
      true,
    ],
    ["tool-file-mixed", "2026-07-30", "read", "/workspace/src/b.ts", "read · src/b.ts", true],
    ["tool-file-unlisted", "2026-07-28", "write", "/workspace/src/tokenRate.ts", null, false],
  ].map(([id, date, tool, path, words, listed]) => ({
    id, date, now: "12:00:15", runText: "run state: tools", rateText: "tools",
    noteText: words, ...(listed ? { fileName: path.split("/").pop() } : {}),
    recs: (b, d) => {
      const first = "/workspace/src/first.ts";
      return [
        ...b.prefix(d),
        b.opener(d),
        b.beat(d, "12:00:10", 2, 900, 900),
        b.rec(at(d, "12:00:12"), "dispatch.turn", { turn_seq: 2, tool_calls_count: 2, generation_ms: 4000, ...(listed ? { tool_names: ["write", tool], tool_paths: [first, path] } : {}) }),
        // The first call, a write, completed (its args as the runtime
        // forwards them, capped); the second, `tool` on `path`, is running.
        b.rec(at(d, "12:00:13"), "dispatch.tool", { tool_name: "write", args: JSON.stringify({ path: first, content: "export const x = 1;\n".repeat(40) }).slice(0, 512) }),
        tick(d, "12:00:15"),
      ];
    },
  })),
  {
    // A rest record with no `reason` (a host from before #2167): no line.
    id: "rest", date: "2026-08-15", now: "12:00:12", runText: /run state: rest \d+s$/, rateText: /^rest \d+s$/, noteText: null,
    recs: (b, d) => [...b.prefix(d), b.rec(at(d, "12:00:08"), "dispatch.rest", { ms: 20000 }), tick(d, "12:00:12")],
  },
  // (#2950) REST says why, from the rest record's own `reason`/`state`, in
  // the readout slot TOOL GEN uses (`noteText`: the run page's line under
  // the lamps; `null` where the record names no reason, so no line). One
  // state per reason a producer writes, and one this build does not know.
  // (Operator, 2026-09-27) A phone-width fleet card drops the state
  // (`rateTextPhone`, the line's VISIBLE text); the run page and a desktop
  // card keep it.
  ...[
    ["rest-turn-delay", "2026-09-10", { reason: "turn_delay" }, "turn delay \\(config\\)", "turn delay \\(config\\)"],
    ["rest-thermal", "2026-09-12", { reason: "thermal", state: "serious" }, "thermal · serious", "thermal"],
    ["rest-pacing", "2026-09-14", { reason: "thermal-duty-cycle", state: "fair" }, "thermal pacing · fair", "thermal pacing"],
    ["rest-battery", "2026-09-16", { reason: "battery", state: "18%" }, "battery · 18%", "battery"],
    ["rest-episode-limit", "2026-09-18", { reason: "thermal-episode-limit", state: "serious" }, "thermal hold · serious", "thermal hold"],
    ["rest-unknown", "2026-09-20", { reason: "solar-flare" }, "solar-flare", "solar-flare"],
    // (#2902 step 5) A budget pause on an agentic-remote run: the budget
    // pacer writes `state` as the endpoint id.
    ["rest-budget", "2026-07-22", { reason: "budget", state: "azure" }, "budget · azure", "budget"],
  ].map(([id, date, why, words, phoneWords]) => ({
    id, date, now: "12:00:12",
    runText: new RegExp(`run state: rest \\d+s · ${words}$`),
    // The readout lines carry the words alone; the tube counts down.
    noteText: new RegExp(`^${words}$`),
    rateText: new RegExp(`^${words}$`),
    rateTextPhone: new RegExp(`^${phoneWords}$`),
    // A thermal rest exists only on a run whose thermal governor is armed.
    armed: why.reason.startsWith("thermal"),
    recs: (b, d) => [...b.prefix(d, {}, true, why.reason.startsWith("thermal")), b.rec(at(d, "12:00:08"), "dispatch.rest", { ms: 20000, turn: 1, ...why }), tick(d, "12:00:12")],
  })),
  // (#2902 step 5, 5th review MF1) A HOSTED call held by its endpoint's
  // budget. Its gate writes one `budget.wait` (how long, which endpoint)
  // BEFORE any `dispatch start` (contract 2: the bookends open only around
  // the model call), and no `dispatch.rest`. It reads as the same REST words
  // as the agentic pause, on the run page and the fleet card, live and in
  // playback, with and without Redis presence.
  ...[
    // A standalone `darkmux dispatch` to a hosted endpoint: the wait is the
    // session's first and only record.
    { id: "rest-budget-hosted", date: "2026-07-20", endpoint: "azure", secs: 20, rest: "\\d+s", words: "budget · azure" },
    // (5th review C7) A day window and a long endpoint id: the countdown is
    // "23h 53m", and the id is trimmed so the line fits.
    // Minutes round up (never under-report): 85996 s left is "23h 54m". The
    // lamp status keeps the full id; the one-line slots trim it.
    { id: "rest-budget-hosted-long", date: "2026-07-18", endpoint: "azure-openai-eastus2-prod", secs: 86_000, rest: "23h 54m", words: "budget · azure-opena…", full: "budget · azure-openai-eastus2-prod", tube: "23h 54m" },
  ].map(({ id, date, endpoint, secs, rest, words, full, tube }) => ({
    id, date, now: "12:00:12", tubeText: tube,
    runText: new RegExp(`run state: rest ${rest} · ${full ?? words}$`), noteText: new RegExp(`^${words}$`), rateText: new RegExp(`^${words}$`), rateTextPhone: /^budget$/,
    recs: (b, d) => [budgetWait(b, d, endpoint, secs), tick(d, "12:00:12")],
  })),
  // A mission's hosted `dispatch.map` step held by its budget: the wait is on
  // the step's task session, under the mission's run-grain session. With
  // Redis presence (the task session beats: `presence`) and without it (the
  // records alone decide, as in playback).
  ...[
    { id: "rest-budget-mission-presence", date: "2026-07-24", presence: true },
    { id: "rest-budget-mission", date: "2026-07-26", presence: false },
  ].map(({ id, date, presence }) => {
    const mid = `layout-mission-${id}`;
    const task = `layout-task-${id}`;
    return {
      id, date, now: "12:00:12", runSid: `layout-run-${id}`,
      presence: presence ? [{ session_id: task, mission_id: mid }] : undefined,
      runText: /run state: rest \d+s · budget · azure$/, noteText: /^budget · azure$/, rateText: /^budget · azure$/, rateTextPhone: /^budget$/,
      recs: (b, d) => {
        const m = { mission_id: mid };
        return [
          { ts: at(d, "11:59:59"), action: "mission.start", session_id: `layout-run-${id}`, ...m, ...MACHINE, payload: {} },
          { ts: at(d, "11:59:59"), action: "run.start", session_id: `layout-run-${id}`, handle: "coder", ...m, ...MACHINE, payload: {} },
          { ts: at(d, "12:00:01"), action: "step.start", session_id: task, handle: "probe", ...m, ...MACHINE, payload: {} },
          { ...budgetWait(b, d, "azure", 20), session_id: task, handle: "probe", ...m },
          tick(d, "12:00:12"),
        ];
      },
    };
  }),
  // (#2950) The same run config's non-rest states, so the thermal REST
  // states are measured against states of a run like theirs (armed), not
  // against a run whose ACTIVE TIME has no thermal line at all.
  {
    id: "armed-generating", date: "2026-09-22", now: "12:00:12", runText: /run state: generating$/, rateText: / tok\/s$/, armed: true,
    recs: (b, d) => [...b.prefix(d, {}, true, true), b.opener(d), b.beat(d, "12:00:10", 2, 900, 900), b.beat(d, "12:00:12", 2, 1800, 1800)],
  },
  {
    id: "armed-toolgen", date: "2026-09-24", now: "12:00:30", runText: "tool gen · write · 18s", noteText: "tool gen · write · 18s", rateText: "tool gen", armed: true,
    recs: (b, d) => [...b.prefix(d, {}, true, true), ...b.writing(d, "write")],
  },
  {
    // A stall needs the clock 30s past the last heartbeat; the fleet's day
    // transport reaches it through `tick`.
    id: "stalled", date: "2026-08-17", now: "12:00:50", runText: "stalled", rateText: "stalled",
    recs: (b, d) => [...b.prefix(d), b.opener(d), b.beat(d, "12:00:10", 2, 500, 500), tick(d, "12:00:50")],
  },
  {
    // A mission between model steps: its run-grain session is in flight, its
    // one execution has completed, no model is working.
    id: "between-steps", date: "2026-08-19", now: "12:00:20", runText: "no model working", rateText: null, runSid: "layout-run",
    recs: (b, d) => {
      const m = { mission_id: MID };
      return [
        { ts: at(d, "11:59:59"), action: "mission.start", session_id: "layout-run", ...m, ...MACHINE, payload: {} },
        { ts: at(d, "11:59:59"), action: "run.start", session_id: "layout-run", handle: "coder", ...m, ...MACHINE, payload: {} },
        ...b.prefix(d, m),
        { ...b.complete(d), ...m },
        { ts: at(d, "12:00:10"), action: "step.complete", session_id: "layout-task", ...m, ...MACHINE, payload: {} },
        tick(d, "12:00:20"),
      ];
    },
  },
  {
    // (#2915) The execution compacting: turn 1's tool has completed and the
    // runtime's compactor is running (its `utility.start`, no usage record
    // yet). PROMPT stays lit; the readout slot counts; the machine's utility
    // strip shows the compacting glyph.
        // The run page's playback transport ends at the run's own last record
    // (the start itself), so it reads "compacting · 0s" there.
    id: "compacting", date: "2026-08-31", now: "12:00:15", runText: /compacting · \d+s$/, rateText: /^compacting · \d+s$/, utilVisual: "compacting", utilLive: "compacting · 7s",
    recs: (b, d) => [
      ...b.prefix(d),
      b.rec(at(d, "12:00:08"), "utility.start", { job: "compaction", model: "darkmux:util-layout", serves: b.sid, stall_after_ms: 600_000 }, { category: "telemetry", source: "utility", handle: "compactor" }),
      tick(d, "12:00:15"),
    ],
  },
  {
    // (#2915) A radio routing job on the machine while its execution reads
    // the next prompt: machine-level (no session), so the work model's
    // readings are PROMPT's and only the utility strip radiates.
    id: "radio-routing", date: "2026-09-02", now: "12:00:09", runText: "processing prompt", rateText: "processing", utilVisual: "radio", utilLive: "radio routing · 1s",
    recs: (b, d) => [
      ...b.prefix(d),
      b.opener(d),
      { ts: at(d, "12:00:08"), action: "utility.start", category: "telemetry", source: "utility", handle: "radio-router", ...MACHINE, payload: { job: "radio_routing", model: "darkmux:util-layout", stall_after_ms: 30_000 } },
    ],
  },
  {
    // (#2915 review, C7) A utility job this build has no visual for: the
    // strip shows the generic indicator; the machine page names it.
    id: "utility-generic", date: "2026-09-04", now: "12:00:09", runText: "processing prompt", rateText: "processing", utilVisual: "generic", utilLive: "dream job · 1s",
    recs: (b, d) => [
      ...b.prefix(d),
      b.opener(d),
      { ts: at(d, "12:00:08"), action: "utility.start", category: "telemetry", source: "utility", handle: "dream-role", ...MACHINE, payload: { job: "dream_job", job_id: "dream-1", model: "darkmux:util-layout", stall_after_ms: 30_000 } },
    ],
  },
  {
    // (#2915 review, C7) A routing job with no end past its 30s bound: the
    // strip's stalled glyph, while the execution rests.
    id: "utility-stalled", date: "2026-09-06", now: "12:00:45", runText: "rest", rateText: "rest", utilVisual: "radio", utilStalled: true, utilLive: "radio routing · stalled",
    recs: (b, d) => [
      ...b.prefix(d),
      b.rec(at(d, "12:00:08"), "dispatch.rest", { ms: 120000 }),
      { ts: at(d, "12:00:08"), action: "utility.start", category: "telemetry", source: "utility", handle: "radio-router", ...MACHINE, payload: { job: "radio_routing", job_id: "route-1", model: "darkmux:util-layout", stall_after_ms: 30_000 } },
      tick(d, "12:00:45"),
    ],
  },
  {
    // (#2915 review, C7) A routing usage record from before 1.61.0 (no
    // `job`): quiet strip; the machine page counts it under "other".
    id: "utility-legacy", date: "2026-09-08", now: "12:00:09", runText: "processing prompt", rateText: "processing", utilLive: "idle",
    recs: (b, d) => [
      ...b.prefix(d),
      b.opener(d),
      { ts: at(d, "12:00:08"), action: "telemetry.tokens", category: "telemetry", source: "tokens", handle: "radio-router", ...MACHINE, payload: { purpose: "utility", call_kind: "single_shot", token_source: "provider", total_tokens: 40, requested_model: "darkmux:util-layout", endpoint: "http://127.0.0.1:1234/v1" } },
    ],
  },
  {
    // A stall seen with the page's live connection down: the page cannot
    // tell a stalled model from a dropped stream, so the run page says "no
    // signal" under its lamps. Live only (playback has no connection).
    // (#2955 review) The fleet card too: its machine is running, and its
    // status line is the plain dim-dot "disconnected" (`fleetStat`), not a lit
    // reading. Live only on the card as on the run page.
    id: "no-signal", date: "2026-08-21", now: "12:00:50", runText: "disconnected", rateText: null, fleetStat: "disconnected", fleetPlayback: false, blockStream: true,
    recs: (b, d) => [...b.prefix(d), b.opener(d), b.beat(d, "12:00:10", 2, 500, 500), tick(d, "12:00:50")],
  },
];

/** (#2955 review) Two executions generating on the fixture machine: the
 *  fleet card's pager state ("‹ 1/2 coder ›"). Not in `STATES`: the run page
 *  suite measures one execution, and only the fleet card has a pager. */
const MULTI = {
  id: "multi-exec",
  date: "2026-07-10",
  now: "12:00:12",
  recs: (b, d) => {
    const b2 = build("layout-exec-multi-exec-2");
    const gen = (x) => [...x.prefix(d), x.opener(d), x.beat(d, "12:00:10", 2, 900, 900), x.beat(d, "12:00:12", 2, 1800, 1800)];
    return [...gen(b), ...gen(b2)];
  },
};

/** The fleet hero with and without its part lines: an idle machine whose
 *  usage records do or do not report cached tokens and a utility call.
 *  `hero-notes` carries `operator.note` records the viewer no longer renders (#2983):
 *  a long mission-level `source: "orchestrator"` one, as flow archives still
 *  hold, and a session-scoped adjudication one. Neither may reach the hero. */
const NOTE_WORDS =
  "shipped the review path end to end on local models tonight, the crew caught four real findings and the report rendered cleanly on the phone";
const HEROES = [
  { id: "hero-plain", date: "2026-08-23", cached: false, util: false },
  { id: "hero-cached", date: "2026-08-25", cached: true, util: false },
  { id: "hero-util", date: "2026-08-27", cached: false, util: true },
  { id: "hero-both", date: "2026-08-29", cached: true, util: true },
  { id: "hero-notes", date: "2026-07-16", cached: false, util: false, notes: true },
].map((h) => ({
  ...h,
  now: "12:00:30",
  recs: (b, d) => [
    ...b.prefix(d, {}, false),
    b.usage(at(d, "12:00:06"), { prompt: 4000, completion: 350, cached: h.cached ? 140 : undefined }),
    ...(h.util ? [b.usage(at(d, "12:00:08"), { prompt: 120, completion: 25, purpose: "utility", callKind: "compaction", handle: "compactor" })] : []),
    b.complete(d),
    ...(h.notes
      ? [
          { ts: at(d, "12:00:10"), action: "operator.note", category: "work", tier: "operator", source: "orchestrator", handle: NOTE_WORDS, ...MACHINE },
          b.rec(at(d, "12:00:11"), "operator.note", {}, { category: "work", tier: "operator", source: "adjudication", handle: "verdict: pass" }),
        ]
      : []),
  ],
}));

for (const s of [...STATES, ...HEROES, MULTI]) {
  s.sid = `layout-exec-${s.id}`;
  s.records = s.recs(build(s.sid), s.date);
  s.nowMs = Date.parse(at(s.date, s.now)) + 500;
}

const ALL = [...STATES, ...HEROES, MULTI];
// Each state owns its day: two states on one date would merge into one day's
// records and measure neither (#2902 step 5: a rebase once put two there).
{
  const seen = new Map();
  for (const s of ALL) {
    if (seen.has(s.date)) throw new Error(`layout fixture: ${s.id} and ${seen.get(s.date)} share ${s.date}`);
    seen.set(s.date, s.id);
  }
}
const byDate = new Map(ALL.map((s) => [s.date, s.records]));

/** A date well after every fixture day: a page pinned here reads each
 *  fixture day as history (playback), never as today. */
const PLAYBACK_NOW = Date.parse("2026-09-28T12:00:00Z");

/**
 * Serve the fixture days to the page: `/flow/<date>`, `/flow-dispatch/<id>`,
 * `/flow-mission/<id>`, `/flow-days`, and a 404 for every other daemon route
 * (a fresh daemon with nothing else to say). The SSE stream passes through
 * to the suite's own server, which holds it open, so a live page reads as
 * connected; `blockStream` answers it with an immediately-closed body
 * instead, so the page reads as disconnected.
 */
/** (#2915) `/machine/specs` for the fixture machine, so `#lens=machine`
 *  resolves it as THIS machine and renders its Utility section. Served only
 *  when a suite asks (`machineSpecs`), so every other page stays as before. */
const MACHINE_SPECS = {
  darkmux_version: "0.0.0-layout",
  flow_schema_version: "1.61.0",
  machine_id: MACHINE.machine_id,
  machine_uid: UID,
  os: "macos aarch64",
  ram_total_bytes: null,
  ram_free_for_ai_bytes: null,
  cpu_brand: null,
  loaded_models: [],
  lms_unreachable: false,
  utility_model: { id: "darkmux:util-layout", loaded: true, n_ctx: 32768 },
  redis_url_redacted: null,
  generated_at_ms: 0,
};

/** `GET /fleet/view` (`FleetView`) for a suite that asks (`fleetView`): the
 *  rows the daemon gathered, each with its card outcome and liveness.
 *  Without it the route stays a 404, so every other page draws its cards from
 *  the flow window alone, as before. */
function fleetViewOf(rows) {
  return {
    gathered_by: "daemon",
    local_machine_id: MACHINE.machine_id,
    presence: { state: "off" },
    roster_error: null,
    fetched_at_ms: 0,
    cache_ttl_ms: 0,
    gather_ms: 0,
    machines: rows,
  };
}

/** One row of the view: a machine whose card the daemon read. `specs`
 *  overrides the card's `specs`; `over` overrides the row itself. */
function viewRow(specs, over = {}) {
  return {
    entry: null,
    is_this_machine: false,
    machine_uid: specs.machine_uid ?? null,
    uid_source: null,
    liveness: "live",
    last_beat_ms: null,
    received_at_ms: null,
    fetch_ms: 12,
    card: { state: "available", source: "listener", card: { specs: { ...MACHINE_SPECS, utility_model: null, ...specs } } },
    accepts: { state: "unknown" },
    ...over,
  };
}

/** The fixture machine as the daemon serving the page reports itself. */
const SELF_ROW = viewRow(MACHINE, { is_this_machine: true, accepts: { state: "this_machine" } });

/** A declared peer the daemon read a card from, that sends no presence beat
 *  (its Redis is off) and lets this machine run a profile and a role. */
const PEER_ROW = viewRow(
  { machine_id: "layout-peer", machine_uid: null, cpu_brand: "Apple M1 Max", ram_total_bytes: 34359738368 },
  {
    entry: { id: "layout-peer", address: "100.64.0.8:8765", added_unix_ms: 1 },
    liveness: "no_beat",
    accepts: { state: "granted", accepts: { peer_name: MACHINE.machine_id, profiles: ["diff-review"], roles: ["radio-host"], images: [], workspace: false } },
  },
);
PEER_ROW.card.card.serves_radio = true; // the card's own statement: the icon never reads `accepts`

/** A declared peer the daemon could not read a card from and hears no beat
 *  from: the "offline" card. */
const OFFLINE_ROW = viewRow(
  { machine_id: "layout-offline", machine_uid: null },
  {
    entry: { id: "layout-offline", address: "100.64.0.9:8765", added_unix_ms: 1 },
    liveness: "no_beat",
    card: { state: "unreachable", reason: "listener_off", detail: null },
  },
);

async function installLayoutRoutes(page, { blockStream = false, machineSpecs = false, holdRuns = false, roster = false, holdPresence = false, holdView = null, presence, fleetView } = {}) {
  await page.route("**/*", async (route) => {
    const url = new URL(route.request().url());
    const p = url.pathname;
    const json = (body) => route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(body) });
    // (#2958) A daemon slow to answer `/runs` (the operator measured 3.3 s):
    // never answered, so a fleet card stays in its before-first-data state.
    if (holdRuns && p === "/runs") return new Promise(() => {});
    // (#2958 review M1) `holdPresence`: `/fleet/machines/live` never
    // answers, so "offline" cannot be claimed yet. `roster`: one declared
    // machine the view cannot read and never hears, which renders as an
    // offline card beside this machine's own.
    if (holdPresence && p === "/fleet/machines/live") return new Promise(() => {});
    // `holdView`: a promise the fleet view's answer waits on, so a test can
    // look at the page while the cards' order is not final and again after.
    if (holdView && p === "/fleet/view") await holdView;
    // (#2902 step 5) Redis session presence for a state that has it: the
    // sessions the daemon reports as beating right now.
    if (presence && p === "/fleet/dispatches/live") return json({ dispatches: presence, meta: { sources: { fleet: { state: "ok" } }, complete: true } });
    if (roster && p === "/fleet/roster") return json({ machines: [{ id: "layout-offline", address: "100.64.0.9:8765", added_unix_ms: 1 }], error: null });
    const viewRows = fleetView ?? (roster ? [SELF_ROW, OFFLINE_ROW] : null);
    if (viewRows && p === "/fleet/view") return json(fleetViewOf(viewRows));
    if (/^\/flow\/\d{4}-\d{2}-\d{2}\/stream$/.test(p)) {
      if (blockStream) return route.fulfill({ status: 503, contentType: "text/plain", body: "layout harness: stream refused\n" });
      return route.continue();
    }
    let m = p.match(/^\/flow\/(\d{4}-\d{2}-\d{2})$/);
    if (m) return json(byDate.get(m[1]) ?? []);
    if (machineSpecs && p === "/machine/specs") return json(MACHINE_SPECS);
    if (p === "/flow-days") return json([...byDate.keys()].sort().reverse().map((date) => ({ date, count: byDate.get(date).length })));
    // The daemon's catalog shape (`catalog_records_response`).
    const catalog = (records) => json({ records, count: records.length, truncated: false });
    m = p.match(/^\/flow-dispatch\/(.+)$/);
    if (m) {
      const sid = decodeURIComponent(m[1]);
      return catalog(ALL.flatMap((s) => s.records).filter((r) => r.session_id === sid));
    }
    m = p.match(/^\/flow-mission\/(.+)$/);
    if (m) {
      const mid = decodeURIComponent(m[1]);
      return catalog(ALL.flatMap((s) => s.records).filter((r) => r.mission_id === mid));
    }
    if (p === "/" || p === "/index.html" || /\.(js|css|svg|png|ico|woff2?)$/.test(p)) return route.continue();
    return route.fulfill({ status: 404, contentType: "text/plain", body: "layout harness: nothing here\n" });
  });
}

/** Rounded (0.1px) width/height of every element matching each selector. */
async function measure(page, selectors) {
  return page.evaluate((sels) => {
    const out = {};
    for (const [k, sel] of Object.entries(sels)) {
      out[k] = [...document.querySelectorAll(sel)].map((e) => {
        const r = e.getBoundingClientRect();
        return { w: Math.round(r.width * 10) / 10, h: Math.round(r.height * 10) / 10 };
      });
    }
    return out;
  }, selectors);
}

const VIEWPORTS = {
  desktop: { width: 1280, height: 900 },
  phone: { width: 390, height: 844 },
};

module.exports = { STATES, HEROES, MULTI, ALL, PLAYBACK_NOW, VIEWPORTS, SELF_ROW, PEER_ROW, OFFLINE_ROW, viewRow, installLayoutRoutes, measure };
