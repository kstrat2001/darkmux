import { describe, it, expect, afterEach } from "vitest";
import { parseRoute, type Route } from "./route";
import { canonicalHash, writeHash } from "./hashSync";

afterEach(() => {
  window.location.hash = "";
});

/** Round-trip: write the route to a hash string, set it as the CURRENT
 * `location.hash`, then parse it back with the real `route.ts` parser (not
 * a lookalike) and assert we recover the same [[Route]]. This is the
 * "closed loop" the packet brief calls for — `canonicalHash` and
 * `parseRoute` are independently-written inverses of each other, and this
 * is the only test that actually proves they agree. */
function roundTrip(route: Route) {
  const hash = canonicalHash(route);
  expect(hash, `canonicalHash(${JSON.stringify(route)}) must not be null for a round-trippable route`).not.toBeNull();
  window.location.hash = "#" + hash;
  return parseRoute();
}

describe("canonicalHash / parseRoute round-trip", () => {
  it("fleet round-trips through an empty hash", () => {
    const route: Route = { kind: "fleet" };
    expect(canonicalHash(route)).toBe("");
    expect(roundTrip(route)).toEqual(route);
  });

  it("runs (kind=all) round-trips WITHOUT a kind param — all is the implicit default", () => {
    const route: Route = { kind: "runs", runsKind: "all", run: null };
    expect(canonicalHash(route)).toBe("lens=runs");
    expect(roundTrip(route)).toEqual(route);
  });

  it("runs (kind=mission) round-trips with an explicit kind param", () => {
    const route: Route = { kind: "runs", runsKind: "mission", run: null };
    expect(canonicalHash(route)).toBe("lens=runs&kind=mission");
    expect(roundTrip(route)).toEqual(route);
  });

  it("runs (kind=dispatch) round-trips", () => {
    const route: Route = { kind: "runs", runsKind: "dispatch", run: null };
    expect(roundTrip(route)).toEqual(route);
  });

  it("runs (kind=lab) round-trips — the legacy #lens=lab upgrade target", () => {
    const route: Route = { kind: "runs", runsKind: "lab", run: null };
    expect(canonicalHash(route)).toBe("lens=runs&kind=lab");
    expect(roundTrip(route)).toEqual(route);
  });

  it("machine round-trips", () => {
    const route: Route = { kind: "machine" };
    expect(canonicalHash(route)).toBe("lens=machine");
    expect(roundTrip(route)).toEqual(route);
  });

  it("console with the default panel round-trips WITHOUT a panel param", () => {
    const route: Route = { kind: "console", panelId: "" };
    expect(canonicalHash(route)).toBe("lens=console");
    expect(roundTrip(route)).toEqual(route);
  });

  it("console with mission-status explicitly named round-trips to the SAME hash as the default (both mean 'default')", () => {
    const route: Route = { kind: "console", panelId: "mission-status" };
    expect(canonicalHash(route)).toBe("lens=console");
    // Parsing it back yields the EMPTY-string panelId form, not
    // "mission-status" literally — this is the same collapse legacy itself
    // performs (`pid==="mission-status"` deletes the param), so the round
    // trip lands on the canonical route, not the literal input.
    expect(roundTrip(route)).toEqual({ kind: "console", panelId: "" });
  });

  it("console with a non-default panel round-trips", () => {
    const route: Route = { kind: "console", panelId: "role-list" };
    expect(canonicalHash(route)).toBe("lens=console&panel=role-list");
    expect(roundTrip(route)).toEqual(route);
  });

  it("session round-trips", () => {
    const route: Route = { kind: "session", sessionId: "abc-123" };
    expect(canonicalHash(route)).toBe("session=abc-123");
    expect(roundTrip(route)).toEqual(route);
  });

  it("mission-redirect is never canonicalized (legacy does a full navigation, nothing to write back)", () => {
    const route: Route = { kind: "mission-redirect", missionId: "my-mission" };
    expect(canonicalHash(route)).toBeNull();
  });

  it("unknown is never canonicalized (a broken bookmark must stay visibly broken, not get silently rewritten)", () => {
    const route: Route = { kind: "unknown", hash: "lens=bogus-lens" };
    expect(canonicalHash(route)).toBeNull();
  });
});

describe("writeHash", () => {
  it("writes via replaceState (no new history entry) and is idempotent when the hash already matches", () => {
    window.location.hash = "#lens=machine";
    const lengthBefore = window.history.length;
    writeHash("lens=machine"); // already current — no-op guard
    expect(window.history.length).toBe(lengthBefore);
    expect(window.location.hash).toBe("#lens=machine");
  });

  it("rewrites a stale hash to the canonical form — the #lens=lab upgrade mechanism", () => {
    window.location.hash = "#lens=lab";
    const route = parseRoute(); // {kind:"runs", runsKind:"lab", run:null}
    writeHash(canonicalHash(route));
    expect(window.location.hash).toBe("#lens=runs&kind=lab");
    // And it stays a valid, equivalent route once rewritten.
    expect(parseRoute()).toEqual(route);
  });

  it("clears the hash entirely for the fleet route", () => {
    window.location.hash = "#lens=machine";
    writeHash(canonicalHash({ kind: "fleet" }));
    expect(window.location.hash).toBe("");
  });

  it("null is a no-op — mission-redirect/unknown routes are never rewritten", () => {
    window.location.hash = "#mission=my-mission";
    writeHash(canonicalHash({ kind: "mission-redirect", missionId: "my-mission" }));
    expect(window.location.hash).toBe("#mission=my-mission");
  });
});
