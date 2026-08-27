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

  it("parses #dispatch=<id> — the canonical spelling (#1974)", () => {
    setHash("#dispatch=abc-123");
    expect(parseRoute()).toEqual({ kind: "dispatch", dispatchId: "abc-123" });
  });

  it("(#1974) still parses the legacy #session=<id> alias, so old bookmarks and printed deep links resolve", () => {
    setHash("#session=abc-123");
    expect(parseRoute()).toEqual({ kind: "dispatch", dispatchId: "abc-123" });
  });

  it("(#1974) prefers the canonical dispatch= when a malformed hash carries both, so canonicalHash's rewrite is idempotent rather than oscillating", () => {
    setHash("#dispatch=canonical&session=legacy");
    expect(parseRoute()).toEqual({ kind: "dispatch", dispatchId: "canonical" });
  });

  it("parses #mission=<id> as the mission-graph lens route (#1868)", () => {
    setHash("#mission=my-mission");
    expect(parseRoute()).toEqual({ kind: "mission", missionId: "my-mission" });
  });

  /// (#1920) `opt.*` must resolve search-vs-hash the same way every other
  /// NAMED param does via `get()` (`search.get(name) || hash.get(name)`).
  /// It did not: the collection loop let the hash overwrite
  /// unconditionally, so `opt.kind=` and `panel=` on one page obeyed
  /// opposite rules.
  it("opt.* precedence: a non-empty search value wins over the hash, like every other named param", () => {
    const url = new URL(window.location.href);
    url.hash = "#lens=console&panel=run-list&opt.kind=lab";
    url.search = "?opt.kind=mission";
    window.history.replaceState(null, "", url.toString());
    const r = parseRoute();
    expect(r.kind).toBe("console");
    if (r.kind !== "console") throw new Error("unreachable");
    expect(r.opts.kind).toBe("mission");
    window.history.replaceState(null, "", "/");
  });

  /// The other half of the same rule: an EMPTY search value is not a
  /// value, so the hash still supplies it, matching `get()`'s own `||`.
  it("opt.* precedence: an empty search value does not shadow the hash", () => {
    const url = new URL(window.location.href);
    url.hash = "#lens=console&panel=run-list&opt.kind=lab";
    url.search = "?opt.kind=";
    window.history.replaceState(null, "", url.toString());
    const r = parseRoute();
    if (r.kind !== "console") throw new Error("expected console route");
    expect(r.opts.kind).toBe("lab");
    window.history.replaceState(null, "", "/");
  });

  it("mission precedence: lens=runs wins over a co-present mission= param", () => {
    setHash("#lens=runs&mission=my-mission");
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "all", run: null, machine: null });
  });

  it("an unrecognized lens value reports unknown, naming the raw hash", () => {
    setHash("#lens=bogus-lens");
    expect(parseRoute()).toEqual({ kind: "unknown", hash: "lens=bogus-lens" });
  });

  // (#1920) `fleet` is the bare-root default but had no explicit `lens=`
  // form of its own — a plausible hash to type or share dead-ended at
  // `unknown` even though the fleet lens is real and already the default.
  // Named explicitly now, matching every other lens.
  it("parses #lens=fleet as the fleet route, not unknown", () => {
    setHash("#lens=fleet");
    expect(parseRoute()).toEqual({ kind: "fleet" });
  });

  // (#1920) `lens=` used to compare with strict `===`, so a hand-typed or
  // autocapitalized value never matched any branch and fell all the way to
  // `unknown` — where `kind=` already degrades gracefully via its own
  // `.toLowerCase()`. Each case below is a DIFFERENT real lens, so one test
  // can't silently pass by exercising the same branch twice.
  it("lowercases lens= the same way kind= already is, for every real lens", () => {
    setHash("#lens=RUNS");
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "all", run: null, machine: null });

    setHash("#lens=Lab");
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "lab", run: null, machine: null });

    setHash("#lens=MACHINE");
    expect(parseRoute()).toEqual({ kind: "machine", uid: null });

    setHash("#lens=Console");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "", opts: {} });

    setHash("#lens=FLEET");
    expect(parseRoute()).toEqual({ kind: "fleet" });
  });

  // A genuinely unrecognized value must still degrade to `unknown` after
  // lowercasing — this isn't a blanket "any string passes now" change.
  it("a value that is unrecognized even after lowercasing still reports unknown", () => {
    setHash("#lens=BOGUS-LENS");
    expect(parseRoute()).toEqual({ kind: "unknown", hash: "lens=BOGUS-LENS" });
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

// `showsEventLog`. The original rule was verified against the real legacy
// source plus a live computed-style probe (`getComputedStyle('.log').display`
// per lens), and this stayed the red-provable guard that the VERIFIED rule —
// not the packet brief's wrong claim — was what shipped.
//
// (#1066) `runs`/`console`/`machine` moved from `hidden` to `shown`. Their
// exclusion was parity with `viewer.html`, which was DELETED in #1865: the
// rule was matching a viewer that no longer exists, against an operator
// asking for the events panel as a collapsible mainstay on all tabs. A pane
// the operator can collapse is strictly more capable than one a route hides
// for them, and the collapse preference persists per session so a "mainstay"
// does not mean "reopens on every tab switch".
//
// The evidence trail above is kept rather than deleted. It records that the
// old rule was measured and right for its moment — which is what makes this
// a deliberate divergence rather than a correction of a mistake.
describe("showsEventLog", () => {
  // (#1868) `mission` is the ONE remaining exclusion, and it is structural
  // rather than parity: MissionGraphLens mounts its own EventLogColumn with
  // mission-scoped records, so the App-level column must not also render —
  // two event logs on one page, disagreeing about scope. This would hold even
  // if legacy had never existed, which is exactly why it survives #1066.
  const hidden: Route["kind"][] = ["mission"];
  const shown: Route["kind"][] = ["fleet", "dispatch", "playback", "unknown", "runs", "console", "machine"];

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
    expect(isLiveRoute({ kind: "dispatch", dispatchId: "s1" })).toBe(false);
    expect(isLiveRoute({ kind: "mission", missionId: "m1" })).toBe(false);
  });

  it("an unrelated darkmux meta does not make a page static — flow-src is the signal", () => {
    injectMeta("darkmux-mode", "play");
    expect(isLiveRoute({ kind: "fleet" })).toBe(true);
  });
});
