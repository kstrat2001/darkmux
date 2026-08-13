import { describe, it, expect, afterEach } from "vitest";
import { parseRoute, showsEventLog, type Route } from "./route";

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
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "lab", run: null });
  });

  it("parses the legacy #lens=lab alias, defaulting kind to lab", () => {
    setHash("#lens=lab");
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "lab", run: null });
  });

  it("falls back to kind=all for an unrecognized kind value", () => {
    setHash("#lens=runs&kind=bogus");
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "all", run: null });
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
    expect(parseRoute()).toEqual({ kind: "console", panelId: "role-list" });
  });

  it("parses #lens=console with no panel as the default panel", () => {
    setHash("#lens=console");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "" });
  });

  it("falls back to the default panel for an unrecognized panel id (matching legacy consoleQuery, not a blank page)", () => {
    setHash("#lens=console&panel=rm-rf-everything");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "" });
  });

  it("parses #session=<id>", () => {
    setHash("#session=abc-123");
    expect(parseRoute()).toEqual({ kind: "session", sessionId: "abc-123" });
  });

  it("parses #mission=<id> as a redirect route, not a rendered lens", () => {
    setHash("#mission=my-mission");
    expect(parseRoute()).toEqual({ kind: "mission-redirect", missionId: "my-mission" });
  });

  it("mission precedence: lens=runs wins over a co-present mission= param", () => {
    setHash("#lens=runs&mission=my-mission");
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "all", run: null });
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
    expect(parseRoute()).toEqual({ kind: "mission-redirect", missionId: "my-mission" });
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
  const hidden: Route["kind"][] = ["runs", "console", "machine"];
  const shown: Route["kind"][] = ["fleet", "session", "playback", "mission-redirect", "unknown"];

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
    expect(parseRoute()).toEqual({ kind: "runs", runsKind: "lab", run: null });
  });

  it("a bare date hash also wins, and may name a DIFFERENT day than the page", () => {
    injectMeta("darkmux-mode", "play");
    injectMeta("darkmux-date", "2026-08-07");
    window.location.hash = "#2026-08-01";
    expect(parseRoute()).toEqual({ kind: "playback", date: "2026-08-01" });
  });

  it("no metas at all (every test harness, and the static demo) stays fleet", () => {
    expect(parseRoute()).toEqual({ kind: "fleet" });
  });
});
