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
  const prefix = (d, extra = {}, withUsage = true) =>
    [
      rec(at(d, "12:00:00"), "dispatch start", { prompt_chars: 3000 }, { source: "crew_dispatch", category: "work", model: "darkmux:qwen-layout" }),
      beat(d, "12:00:01", 1, 0, 0, { prompt_chars: 16000 }),
      beat(d, "12:00:03", 1, 600, 600),
      beat(d, "12:00:05", 1, 1400, 1400),
      rec(at(d, "12:00:06"), "dispatch.turn", { turn_seq: 1, tool_calls_count: 1, generation_ms: 5000 }),
      ...(withUsage ? [usage(at(d, "12:00:06"), { prompt: 4000, completion: 350 })] : []),
      rec(at(d, "12:00:07"), "dispatch.tool", { tool_name: "read" }),
    ].map((r) => ({ ...r, ...extra }));
  const opener = (d) => beat(d, "12:00:08", 2, 0, 0, { prompt_chars: 20000 });
  const complete = (d) => rec(at(d, "12:00:09"), "dispatch complete", { completion_tokens: 350, prompt_tokens: 4000 });
  const writing = (d, name) => {
    const out = [opener(d), beat(d, "12:00:10", 2, 500, 500)];
    for (let s = 12; s <= 30; s += 2) out.push(beat(d, `12:00:${String(s).padStart(2, "0")}`, 2, 800, 500, { phase: "writing_tool_call", ...(name ? { tool_name: name } : {}) }));
    return out;
  };
  return { rec, usage, beat, prefix, opener, complete, writing };
}

// A record that only moves the day's end (a playback page's playhead) to the
// state's `now`: no session, so no run reads it; the fixture machine's own
// identity, so it adds no second card.
const tick = (d, hms) => ({ ts: at(d, hms), action: "layout tick", category: "debug", source: "layout", ...MACHINE });

/** Every state the MODEL section and a fleet card can show. Days are two
 *  apart so a live page's 24h window never reaches a neighbor's records. */
const STATES = [
  {
    id: "finished", date: "2026-08-01", now: "12:00:30", runText: "finished", rateText: null,
    recs: (b, d) => [...b.prefix(d), b.complete(d)],
  },
  {
    id: "prompt", date: "2026-08-03", now: "12:00:09", runText: "processing prompt", rateText: "processing",
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
  {
    id: "rest", date: "2026-08-15", now: "12:00:12", runText: "rest", rateText: "rest",
    recs: (b, d) => [...b.prefix(d), b.rec(at(d, "12:00:08"), "dispatch.rest", { ms: 20000 }), tick(d, "12:00:12")],
  },
  {
    // The run page's playback transport ends at the run's own last record,
    // so a stall (which needs the clock 30s past the last heartbeat) exists
    // only live there; the fleet's day transport reaches it through `tick`.
    id: "stalled", date: "2026-08-17", now: "12:00:50", runText: "stalled", rateText: "stalled", runPlayback: false,
    recs: (b, d) => [...b.prefix(d), b.opener(d), b.beat(d, "12:00:10", 2, 500, 500), tick(d, "12:00:50")],
  },
  {
    // A mission between model steps: its run-grain session is in flight, its
    // one execution has completed, no model is working.
    id: "between-steps", date: "2026-08-19", now: "12:00:20", runText: "no model working", rateText: null, runSid: "layout-run",
    recs: (b, d) => {
      const m = { mission_id: MID };
      return [
        { ts: at(d, "11:59:59"), action: "mission start", session_id: "layout-run", ...m, ...MACHINE, payload: {} },
        { ts: at(d, "11:59:59"), action: "dispatch start", source: "mission", session_id: "layout-run", handle: "coder", ...m, ...MACHINE, payload: {} },
        ...b.prefix(d, m),
        { ...b.complete(d), ...m },
        { ts: at(d, "12:00:10"), action: "step complete", session_id: "layout-task", ...m, ...MACHINE, payload: {} },
        tick(d, "12:00:20"),
      ];
    },
  },
  {
    // A stall seen with the page's live connection down: the page cannot
    // tell a stalled model from a dropped stream, so the run page says "no
    // signal" under its lamps. Live only (playback has no connection).
    id: "no-signal", date: "2026-08-21", now: "12:00:50", runText: "no signal", rateText: null, runPlayback: false, fleet: false, blockStream: true,
    recs: (b, d) => [...b.prefix(d), b.opener(d), b.beat(d, "12:00:10", 2, 500, 500), tick(d, "12:00:50")],
  },
];

/** The fleet hero with and without its part lines: an idle machine whose
 *  usage records do or do not report cached tokens and a utility call. */
const HEROES = [
  { id: "hero-plain", date: "2026-08-23", cached: false, util: false },
  { id: "hero-cached", date: "2026-08-25", cached: true, util: false },
  { id: "hero-util", date: "2026-08-27", cached: false, util: true },
  { id: "hero-both", date: "2026-08-29", cached: true, util: true },
].map((h) => ({
  ...h,
  now: "12:00:30",
  recs: (b, d) => [
    ...b.prefix(d, {}, false),
    b.usage(at(d, "12:00:06"), { prompt: 4000, completion: 350, cached: h.cached ? 140 : undefined }),
    ...(h.util ? [b.usage(at(d, "12:00:08"), { prompt: 120, completion: 25, purpose: "utility", callKind: "compaction", handle: "compactor" })] : []),
    b.complete(d),
  ],
}));

for (const s of [...STATES, ...HEROES]) {
  s.sid = `layout-exec-${s.id}`;
  s.records = s.recs(build(s.sid), s.date);
  s.nowMs = Date.parse(at(s.date, s.now)) + 500;
}

const ALL = [...STATES, ...HEROES];
const byDate = new Map(ALL.map((s) => [s.date, s.records]));

/** A date well after every fixture day: a page pinned here reads each
 *  fixture day as history (playback), never as today. */
const PLAYBACK_NOW = Date.parse("2026-09-20T12:00:00Z");

/**
 * Serve the fixture days to the page: `/flow/<date>`, `/flow-session/<id>`,
 * `/flow-mission/<id>`, `/flow-days`, and a 404 for every other daemon route
 * (a fresh daemon with nothing else to say). The SSE stream passes through
 * to the suite's own server, which holds it open, so a live page reads as
 * connected; `blockStream` answers it with an immediately-closed body
 * instead, so the page reads as disconnected.
 */
async function installLayoutRoutes(page, { blockStream = false } = {}) {
  await page.route("**/*", async (route) => {
    const url = new URL(route.request().url());
    const p = url.pathname;
    const json = (body) => route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(body) });
    if (/^\/flow\/\d{4}-\d{2}-\d{2}\/stream$/.test(p)) {
      if (blockStream) return route.fulfill({ status: 503, contentType: "text/plain", body: "layout harness: stream refused\n" });
      return route.continue();
    }
    let m = p.match(/^\/flow\/(\d{4}-\d{2}-\d{2})$/);
    if (m) return json(byDate.get(m[1]) ?? []);
    if (p === "/flow-days") return json([...byDate.keys()].sort().reverse().map((date) => ({ date, count: byDate.get(date).length })));
    // The daemon's catalog shape (`catalog_records_response`).
    const catalog = (records) => json({ records, count: records.length, truncated: false });
    m = p.match(/^\/flow-session\/(.+)$/);
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

module.exports = { STATES, HEROES, ALL, PLAYBACK_NOW, VIEWPORTS, installLayoutRoutes, measure };
