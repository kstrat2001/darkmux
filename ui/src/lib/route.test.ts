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

  it("a bare date hash (playback pin, out of scope for /next) reports unknown", () => {
    setHash("#2026-08-09");
    expect(parseRoute()).toEqual({ kind: "unknown", hash: "2026-08-09" });
  });
});
