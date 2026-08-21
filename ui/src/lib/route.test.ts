import { describe, it, expect, afterEach } from "vitest";
import { isLiveRoute, parseRoute, showsEventLog, type Route } from "./route";

function setHash(hash: string) {
  window.location.hash = hash;
}

afterEach(() => {
  window.location.hash = "";
});

describe("parseRoute", () => {
  it("defaults to fleet with no hash", () => {
    expect(parseRoute()).toEqual({ kind: "fleet" });
  });

  it("parses #lens=runs with a kind filter", () => {
    setHash("#lens=runs&kind=lab");
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "lab", run: null, machine: null });
  });

  it("parses the legacy #lens=lab alias, defaulting kind to lab", () => {
    setHash("#lens=lab");
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "lab", run: null, machine: null });
  });

  it("falls back to kind=all for an unrecognized kind value", () => {
    setHash("#lens=runs&kind=bogus");
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "all", run: null, machine: null });
  });

  // (#1809) The machine pin — finishing #1508 step 4.
  it("parses #lens=runs&machine=<uid>, composable with a kind filter", () => {
    setHash("#lens=runs&kind=lab&machine=some-uid");
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "lab", run: null, machine: "some-uid" });
  });

  it("machine defaults to null (every machine) when the param is absent", () => {
    setHash("#lens=runs");
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "all", run: null, machine: null });
  });

  it("parses #lens=machine with no uid as the local machine (uid: null)", () => {
    setHash("#lens=machine");
    expect(parseRoute()).toEqual({ kind: "machine", uid: null });
  });

  it("parses #lens=machine&uid=<uid> as a specific (possibly remote) machine drill-in", () => {
    setHash("#lens=machine&uid=some-remote-uid");
    expect(parseRoute()).toEqual({ kind: "machine", uid: "some-remote-uid" });
  });

  it("parses #lens=console&panel=<id>", () => {
    setHash("#lens=console&panel=role-list");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "role-list", opts: {} });
  });

  it("parses #lens=console with no panel as the default panel", () => {
    setHash("#lens=console");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "", opts: {} });
  });

  it("falls back to the default panel for an unrecognized panel id (matching legacy consoleQuery, not a blank page)", () => {
    setHash("#lens=console&panel=rm-rf-everything");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "", opts: {} });
  });

  // ── #1911: opts on the console route ──────────────────────────────

  it("parses opt.<name>=<value> against the panel's own declared table", () => {
    setHash("#lens=console&panel=run-list&opt.kind=lab");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "run-list", opts: { kind: "lab" } });
  });

  it("multiple opts on the same panel compose", () => {
    setHash("#lens=console&panel=run-list&opt.kind=mission&opt.all=all");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "run-list", opts: { kind: "mission", all: "all" } });
  });

  it("an unknown opt VALUE for a known name drops silently — never a blank page, never a passthrough", () => {
    setHash("#lens=console&panel=run-list&opt.kind=bogus");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "run-list", opts: {} });
  });

  it("an unknown opt NAME for the panel drops silently", () => {
    setHash("#lens=console&panel=run-list&opt.machine=studio");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "run-list", opts: {} });
  });

  it("an opt legal on a DIFFERENT panel is irrelevant here and drops", () => {
    // `kind` is a real opt name — just not one `mission-status` declares.
    setHash("#lens=console&panel=mission-status&opt.kind=lab");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "mission-status", opts: {} });
  });

  it("a panel with no declared opts ignores any opt.* param entirely", () => {
    setHash("#lens=console&panel=doctor&opt.all=all");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "doctor", opts: {} });
  });

  // ── #1911: the mission-status-all alias ───────────────────────────

  it("panel=mission-status-all resolves to mission-status with all forced", () => {
    setHash("#lens=console&panel=mission-status-all");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "mission-status", opts: { all: "all" } });
  });

  it("the alias's forced opt wins over a stray opt.* param claiming otherwise", () => {
    setHash("#lens=console&panel=mission-status-all&opt.all=recent");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "mission-status", opts: { all: "all" } });
  });

  it("run-list is a real, addressable panel id", () => {
    setHash("#lens=console&panel=run-list");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "run-list", opts: {} });
  });

  it("parses #session=<id>", () => {
    setHash("#session=abc-123");
    expect(parseRoute()).toEqual({ kind: "session", sessionId: "abc-123" });
  });

  it("parses #mission=<id> as the mission-graph lens route (#1868)", () => {
    setHash("#mission=my-mission");
    expect(parseRoute()).toEqual({ kind: "mission", missionId: "my-mission" });
  });

  it("mission precedence: lens=runs wins over a co-present mission= param", () => {
    setHash("#lens=runs&mission=my-mission");
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "all", run: null, machine: null });
  });

  it("an unrecognized lens value reports unknown, naming the raw hash", () => {
    setHash("#lens=bogus-lens");
    expect(parseRoute()).toEqual({ kind: "unknown", hash: "lens=bogus-lens" });
  });

  it("parses a bare date hash as a playback route (Packet 4)", () => {
    setHash("#2026-08-09");
    expect(parseRoute()).toEqual({ kind: "playback", date: "2026-08-09" });
  });

  it("parses ?date=<date> as a playback route when the hash is empty (Packet 4)", () => {
    const url = new URL(window.location.href);
    url.hash = "";
    url.search = "?date=2026-08-07";
    window.history.replaceState(null, "", url.toString());
    expect(parseRoute()).toEqual({ kind: "playback", date: "2026-08-07" });
    window.history.replaceState(null, "", "/");
  });

  it("an in-hash date wins over a co-present ?date= query param", () => {
    const url = new URL(window.location.href);
    url.hash = "#2026-08-09";
    url.search = "?date=2026-08-07";
    window.history.replaceState(null, "", url.toString());
    expect(parseRoute()).toEqual({ kind: "playback", date: "2026-08-09" });
    window.history.replaceState(null, "", "/");
  });

  it("a garbage non-date hash silently falls back to fleet, matching legacy's own targetDate() default", () => {
    setHash("#not-a-date");
    expect(parseRoute()).toEqual({ kind: "fleet" });
  });

  it("mission precedence: an explicit mission= wins over a co-present bare date hash", () => {
    // Not directly expressible as one hash string (a bare-date hash has no
    // room for a second param) — mirrors legacy's own precedence order via
    // the search-string form instead: `catalogQuery()` (mission/session) is
    // checked in boot() before `targetDate()`'s result is ever used for
    // anything but `date`, so an explicit mission always wins.
    const url = new URL(window.location.href);
    url.hash = "";
    url.search = "?mission=my-mission&date=2026-08-07";
    window.history.replaceState(null, "", url.toString());
    expect(parseRoute()).toEqual({ kind: "mission", missionId: "my-mission" });
    window.history.replaceState(null, "", "/");
  });
});

// (Chrome packet) `showsEventLog` — verified against the real legacy source
// + a live computed-style probe (see the function's own doc for the
// evidence trail); this is the RED-PROVABLE guard that the verified rule,
// not the packet brief's wrong claim, is what shipped. Break the function
// (e.g. revert to `route.kind !== "runs" && route.kind !== "machine"`,
// dropping the console exclusion) and this test for "console" goes red.
describe("showsEventLog", () => {
  // (#1868) `mission` moved from `shown` to `hidden`: MissionGraphLens owns
  // its own events pane now (a second EventLogColumn mount, mission-scoped),
  // so the App-level column must not ALSO render for this route.
  const hidden: Route["kind"][] = ["runs", "console", "machine", "mission"];
  const shown: Route["kind"][] = ["fleet", "session", "playback", "unknown"];

  it.each(hidden)("hides the event log on %s", (kind) => {
    expect(showsEventLog({ kind } as Route)).toBe(false);
  });

  it.each(shown)("shows the event log on %s", (kind) => {
    expect(showsEventLog({ kind } as Route)).toBe(true);
  });
});

/**
 * (the flip, #1800) `/play/<date>` routing, which does NOT come from the URL.
 *
 * The server responds to that path with `inject_mode_meta(html, "play",
 * Some(date))` — the date lands in a meta tag in the response BODY, while
 * `location` reads `/play/2026-08-07` with no hash and no query. Legacy read
 * those metas at boot; this port did not, and had no reason to while
 * `/play/:date` served legacy and `/next` was live-only.
 *
 * Without this, the flip renders TODAY's live fleet view under yesterday's
 * address — silently, and in precisely the confidently-wrong shape the whole
 * #1800 gate existed to eliminate.
 */
describe("parseRoute — the injected playback date (/play/<date>)", () => {
  function injectMeta(name: string, content: string) {
    const el = document.createElement("meta");
    el.setAttribute("name", name);
    el.setAttribute("content", content);
    document.head.appendChild(el);
  }

  afterEach(() => {
    document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
    window.location.hash = "";
  });

  it("routes to playback for that day when the server served a play page", () => {
    injectMeta("darkmux-mode", "play");
    injectMeta("darkmux-date", "2026-08-07");
    expect(parseRoute()).toEqual({ kind: "playback", date: "2026-08-07" });
  });

  it("a LIVE page is unaffected — mode=live injects no date and stays fleet", () => {
    injectMeta("darkmux-mode", "live");
    expect(parseRoute()).toEqual({ kind: "fleet" });
  });

  it("ignores a date meta without the play mode — the mode is the evidence", () => {
    // Reading the date alone would decide routing on weaker evidence than the
    // server actually gave.
    injectMeta("darkmux-mode", "live");
    injectMeta("darkmux-date", "2026-08-07");
    expect(parseRoute()).toEqual({ kind: "fleet" });
  });

  it("ignores a malformed injected date, falling back to live", () => {
    injectMeta("darkmux-mode", "play");
    injectMeta("darkmux-date", "not-a-date");
    expect(parseRoute()).toEqual({ kind: "fleet" });
  });

  it("an explicit hash WINS over the injected date", () => {
    // `/play/2026-08-07#lens=runs` must reach the runs lens rather than being
    // preempted by the page's own mode. The injected date is the LAST
    // fallback, not an override.
    injectMeta("darkmux-mode", "play");
    injectMeta("darkmux-date", "2026-08-07");
    window.location.hash = "#lens=runs&kind=lab";
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "lab", run: null, machine: null });
  });

  it("a bare date hash also wins, and may name a DIFFERENT day than the page", () => {
    injectMeta("darkmux-mode", "play");
    injectMeta("darkmux-date", "2026-08-07");
    window.location.hash = "#2026-08-01";
    expect(parseRoute()).toEqual({ kind: "playback", date: "2026-08-01" });
  });

  it("no metas at all (every test harness) stays fleet — the static demo now DOES inject darkmux-flow-src (#1801), see the describe block below", () => {
    expect(parseRoute()).toEqual({ kind: "fleet" });
  });
});

/**
 * (#1801) `darkmux-flow-src` — the static demo's committed `.jsonl` — forces
 * a playback route with no server-assigned date, mirroring legacy's own
 * `wantsPlayback = ... || !!flowSrc || ...` (viewer.html:3880), which forces
 * the playback branch regardless of any date the hash/query names. See
 * `route.ts`'s own doc on the widened `date: string | null` for why `null`
 * is the honest value here rather than a guessed placeholder.
 */
describe("parseRoute — the static-demo flow-src route (#1801)", () => {
  function injectMeta(name: string, content: string) {
    const el = document.createElement("meta");
    el.setAttribute("name", name);
    el.setAttribute("content", content);
    document.head.appendChild(el);
  }

  afterEach(() => {
    document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
    window.location.hash = "";
  });

  it("resolves to a playback route with date: null when no hash is present", () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    expect(parseRoute()).toEqual({ kind: "playback", date: null });
  });

  it("wins over an explicit bare-date hash — there is no daemon to serve that OTHER day", () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    window.location.hash = "#2026-08-01";
    expect(parseRoute()).toEqual({ kind: "playback", date: null });
  });

  it("wins over ?date= too, for the same reason", () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    const url = new URL(window.location.href);
    url.hash = "";
    url.search = "?date=2026-08-07";
    window.history.replaceState(null, "", url.toString());
    expect(parseRoute()).toEqual({ kind: "playback", date: null });
    window.history.replaceState(null, "", "/");
  });

  it("still yields to an explicit #lens= deep link — flow-src is the LOWEST-precedence signal, not the highest", () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    window.location.hash = "#lens=runs&kind=lab";
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "lab", run: null, machine: null });
  });

  // Inverted case: without the meta, the exact same hash states behave
  // exactly as the rest of this file already asserts — a garbage/no-op
  // gate would make EVERY test above pass by accident if it fired
  // unconditionally instead of only under isStaticBuild().
  it("without the meta, no hash still resolves to plain fleet, not playback", () => {
    expect(parseRoute()).toEqual({ kind: "fleet" });
  });

  it("without the meta, a bare date hash still resolves to a REAL date, not null", () => {
    window.location.hash = "#2026-08-01";
    expect(parseRoute()).toEqual({ kind: "playback", date: "2026-08-01" });
  });
});

/**
 * (#1801, merge-gate finding) `isLiveRoute()` is the port's analog of legacy's
 * GLOBAL `wantsPlayback` gate (viewer.html:3880). It was keyed on route KIND
 * alone, and `parseRoute` resolves `lens=` BEFORE the static-build branch — so
 * `#lens=runs` on the daemon-less demo parsed to `{kind:"runs"}` and opened
 * every live consumer: an SSE stream, a 5s presence poll, and a mode badge
 * reading `◌ RECONNECTING` on a page with no daemon anywhere near it.
 *
 * Both directions are asserted deliberately. A gate tested only where it FIRES
 * can be vacuously true — the daemon-served cases below are what prove this
 * one did not simply turn every route non-live.
 */
describe("isLiveRoute — a daemon-less build is never live, on any lens", () => {
  function injectMeta(name: string, content: string) {
    const el = document.createElement("meta");
    el.setAttribute("name", name);
    el.setAttribute("content", content);
    document.head.appendChild(el);
  }

  afterEach(() => {
    document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
    window.location.hash = "";
  });

  const liveKinds: Route[] = [
    { kind: "fleet" },
    { kind: "runs", runsKind: "all", run: null, machine: null },
    { kind: "machine", uid: null },
    { kind: "console", panelId: "", opts: {} },
    { kind: "unknown", hash: "nonsense" },
  ];

  it("every normally-live route goes non-live once darkmux-flow-src is present", () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    for (const route of liveKinds) {
      expect(isLiveRoute(route), `${route.kind} should not be live on a static build`).toBe(false);
    }
  });

  it("the SAME routes stay live with no flow-src meta (a real daemon)", () => {
    for (const route of liveKinds) {
      expect(isLiveRoute(route), `${route.kind} should stay live behind a daemon`).toBe(true);
    }
  });

  it("historical routes are non-live either way — the kind test still stands on its own", () => {
    expect(isLiveRoute({ kind: "playback", date: "2026-08-07" })).toBe(false);
    expect(isLiveRoute({ kind: "session", sessionId: "s1" })).toBe(false);
    expect(isLiveRoute({ kind: "mission", missionId: "m1" })).toBe(false);
  });

  it("an unrelated darkmux meta does not make a page static — flow-src is the signal", () => {
    injectMeta("darkmux-mode", "play");
    expect(isLiveRoute({ kind: "fleet" })).toBe(true);
  });
});
