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
    const route: Route = { kind: "runs", runsKind: "all", run: null, machine: null };
    expect(canonicalHash(route)).toBe("lens=runs");
    expect(roundTrip(route)).toEqual(route);
  });

  it("runs (kind=mission) round-trips with an explicit kind param", () => {
    const route: Route = { kind: "runs", runsKind: "mission", run: null, machine: null };
    expect(canonicalHash(route)).toBe("lens=runs&kind=mission");
    expect(roundTrip(route)).toEqual(route);
  });

  it("runs (kind=dispatch) round-trips", () => {
    const route: Route = { kind: "runs", runsKind: "dispatch", run: null, machine: null };
    expect(roundTrip(route)).toEqual(route);
  });

  it("runs (kind=lab) round-trips — the legacy #lens=lab upgrade target", () => {
    const route: Route = { kind: "runs", runsKind: "lab", run: null, machine: null };
    expect(canonicalHash(route)).toBe("lens=runs&kind=lab");
    expect(roundTrip(route)).toEqual(route);
  });

  it("machine (local, uid: null) round-trips WITHOUT a uid param", () => {
    const route: Route = { kind: "machine", uid: null };
    expect(canonicalHash(route)).toBe("lens=machine");
    expect(roundTrip(route)).toEqual(route);
  });

  it("machine (a specific/remote uid) round-trips with an explicit uid param — the drill-in widening", () => {
    const route: Route = { kind: "machine", uid: "studio-uid-123" };
    expect(canonicalHash(route)).toBe("lens=machine&uid=studio-uid-123");
    expect(roundTrip(route)).toEqual(route);
  });

  it("runs (kind=lab, a resolved run) round-trips with an explicit run param — the lab-run-detail deep link", () => {
    const route: Route = { kind: "runs", runsKind: "lab", run: "live/gate-1", machine: null };
    expect(canonicalHash(route)).toBe("lens=runs&kind=lab&run=live%2Fgate-1");
    expect(roundTrip(route)).toEqual(route);
  });

  it("runs (kind=all, a resolved run) ALSO round-trips with run — a lab row is reachable from kind=all too, matching legacy's state.level==='lab-run' gate being independent of state.runsKind", () => {
    const route: Route = { kind: "runs", runsKind: "all", run: "live/gate-1", machine: null };
    expect(canonicalHash(route)).toBe("lens=runs&run=live%2Fgate-1");
    expect(roundTrip(route)).toEqual(route);
  });

  // (#1809) The machine pin round-trips the same way `uid`/`run` do —
  // written only when set, and composable with the OTHER independent runs
  // params rather than replacing them.
  it("runs with a machine pin round-trips with an explicit machine param", () => {
    const route: Route = { kind: "runs", runsKind: "all", run: null, machine: "some-uid" };
    expect(canonicalHash(route)).toBe("lens=runs&machine=some-uid");
    expect(roundTrip(route)).toEqual(route);
  });

  it("a machine pin composes with a kind filter AND a resolved run on the same hash", () => {
    const route: Route = { kind: "runs", runsKind: "lab", run: "live/gate-1", machine: "some-uid" };
    expect(canonicalHash(route)).toBe("lens=runs&kind=lab&run=live%2Fgate-1&machine=some-uid");
    expect(roundTrip(route)).toEqual(route);
  });

  it("console with the default panel round-trips WITHOUT a panel param", () => {
    const route: Route = { kind: "console", panelId: "", opts: {} };
    expect(canonicalHash(route)).toBe("lens=console");
    expect(roundTrip(route)).toEqual(route);
  });

  // (#1904 QA fix) Was: "console with mission-status explicitly named
  // round-trips to the SAME hash as the default (both mean 'default')" —
  // that was true under legacy's `pid==="mission-status" ? p.delete("panel")
  // : ...` collapse, back when "" (no panel param) and "mission-status"
  // meant the exact same landing state. #1904 broke that premise: "" now
  // means the ACTIVITY view, a different thing than the `mission-status`
  // CLI panel. Collapsing the two here made `mission-status` literally
  // unreachable by URL — a fresh `#lens=console&panel=mission-status` boot
  // renders the right content once (`ConsolePanel`'s own `initialPanelId`
  // guard), then this function immediately rewrites the address bar back
  // to bare `#lens=console`, so a reload/bookmark/copy of that exact URL
  // silently lands on the activity view instead. `mission-status` now
  // round-trips like any other explicit panel — there is no more "both mean
  // default" collapse for ANY panel id.
  it("console with mission-status explicitly named round-trips to ITSELF, not the default (#1904 — the two are no longer the same view)", () => {
    const route: Route = { kind: "console", panelId: "mission-status", opts: {} };
    expect(canonicalHash(route)).toBe("lens=console&panel=mission-status");
    expect(roundTrip(route)).toEqual(route);
  });

  it("console with a non-default panel round-trips", () => {
    const route: Route = { kind: "console", panelId: "role-list", opts: {} };
    expect(canonicalHash(route)).toBe("lens=console&panel=role-list");
    expect(roundTrip(route)).toEqual(route);
  });

  // ── #1911: opts round-trip ─────────────────────────────────────────

  it("console with a non-default opt round-trips with opt.<name>=<value>", () => {
    const route: Route = { kind: "console", panelId: "run-list", opts: { kind: "lab" } };
    expect(canonicalHash(route)).toBe("lens=console&panel=run-list&opt.kind=lab");
    expect(roundTrip(route)).toEqual(route);
  });

  it("console with the DEFAULT opt value explicitly picked round-trips WITHOUT the opt param — one canonical hash for one variant", () => {
    const route: Route = { kind: "console", panelId: "run-list", opts: { kind: "all" } };
    expect(canonicalHash(route)).toBe("lens=console&panel=run-list");
  });

  it("multiple non-default opts round-trip sorted by name", () => {
    const route: Route = { kind: "console", panelId: "run-list", opts: { all: "all", kind: "lab" } };
    expect(canonicalHash(route)).toBe("lens=console&panel=run-list&opt.all=all&opt.kind=lab");
    expect(roundTrip(route)).toEqual(route);
  });

  // ── #1911: the mission-status-all alias upgrade ─────────────────────
  // Same shape as the pre-existing `#lens=lab` → `#lens=runs&kind=lab`
  // upgrade above: arriving on the alias already parses to the CANONICAL
  // route, so writing it back just names it in the address bar.

  it("a panel=mission-status-all deep link upgrades the address bar to panel=mission-status&opt.all=all", () => {
    window.location.hash = "#lens=console&panel=mission-status-all";
    const route = parseRoute();
    expect(route).toEqual({ kind: "console", panelId: "mission-status", opts: { all: "all" } });
    writeHash(canonicalHash(route));
    expect(window.location.hash).toBe("#lens=console&panel=mission-status&opt.all=all");
  });

  it("dispatch round-trips on the canonical spelling (#1974)", () => {
    const route: Route = { kind: "dispatch", dispatchId: "abc-123" };
    expect(canonicalHash(route)).toBe("dispatch=abc-123");
    expect(roundTrip(route)).toEqual(route);
  });

  it("(#1974) a legacy #session= bookmark is REWRITTEN to the canonical #dispatch= form — which is what makes the alias one-release rather than permanent", () => {
    window.location.hash = "#session=abc-123";
    const route = parseRoute();
    expect(route).toEqual({ kind: "dispatch", dispatchId: "abc-123" });
    writeHash(canonicalHash(route));
    expect(window.location.hash).toBe("#dispatch=abc-123");
  });

  it("mission is never canonicalized (#1868 — the hash it arrives on already IS canonical, nothing to compress)", () => {
    const route: Route = { kind: "mission", missionId: "my-mission" };
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

  it("null is a no-op — mission/unknown routes are never rewritten", () => {
    window.location.hash = "#mission=my-mission";
    writeHash(canonicalHash({ kind: "mission", missionId: "my-mission" }));
    expect(window.location.hash).toBe("#mission=my-mission");
  });
});
