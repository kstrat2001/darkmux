import { describe, it, expect, afterEach } from "vitest";
import { parseRoute } from "./route";

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

  it("parses #lens=machine", () => {
    setHash("#lens=machine");
    expect(parseRoute()).toEqual({ kind: "machine" });
  });

  it("parses #lens=console&panel=<id>", () => {
    setHash("#lens=console&panel=role-list");
    expect(parseRoute()).toEqual({ kind: "console", panelId: "role-list" });
  });

  it("parses #lens=console with no panel as the default panel", () => {
    setHash("#lens=console");
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
