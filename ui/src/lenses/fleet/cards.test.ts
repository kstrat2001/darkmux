import { describe, it, expect } from "vitest";
import { machActive, specOf, buildFleetCard } from "./cards";
import type { FlowRecord, MachineSpecs, PresenceBeat } from "../../types/handwritten";

function rec(overrides: Partial<FlowRecord>): FlowRecord {
  return { ts: "2026-08-08T00:00:00.000Z", ...overrides };
}

function beat(overrides: Partial<PresenceBeat>): PresenceBeat {
  return { machine_uid: "u1", display_name: "studio", schema_version: "1.18.0", beat_ts_ms: 1, ...overrides };
}

/** The playhead. `/next` has no scrubber, so it is always `tMax`; a value
 *  safely after every fixture timestamp stands in for that. */
const T_MAX = Date.parse("2026-08-09T00:00:00.000Z");

describe("machActive", () => {
  it("is true when a dispatch.start on the machine belongs to a live session", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(["s1"]), "m1", true, T_MAX)).toBe(true);
  });

  it("is false when the session isn't in the live set", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(), "m1", true, T_MAX)).toBe(false);
  });

  it("is false for a different machine's live session", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "m2", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(["s1"]), "m1", true, T_MAX)).toBe(false);
  });

  // (#1800 P2) The replay arm keys on the CLOSE-EDGE, not presence — the live
  // set is empty on a replay by construction, so a presence-keyed check would
  // report every recorded day as idle whether or not it was.
  it("replay: a session closed at or before the playhead is NOT active", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" }),
      rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.complete" }),
    ];
    expect(machActive(data, new Set(), "m1", false, T_MAX)).toBe(false);
  });

  // The INVERTED case, and the one that proves the check is doing work: same
  // empty live set, same replay mode, no close-edge -> still active. Without
  // this, a `machActive` hardwired to `false` in replay would pass the test
  // above and look correct.
  it("replay: a session with NO close-edge IS active, on the same empty live set", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(), "m1", false, T_MAX)).toBe(true);
  });

  // `session.end` alone closes a session (`sessionCloseEdge`) — an abandoned
  // or hard-killed dispatch never emits `dispatch.complete`, and reading only
  // the dispatch terminal drew such a machine active forever.
  it("replay: session.end alone closes it, with no dispatch terminal at all", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" }),
      rec({ machine_uid: "m1", session_id: "s1", action: "session.end" }),
    ];
    expect(machActive(data, new Set(), "m1", false, T_MAX)).toBe(false);
  });
});

describe("specOf", () => {
  const specs: MachineSpecs = {
    darkmux_version: "2.5.0",
    flow_schema_version: "1.18.0",
    machine_id: "MacBook-Pro",
    os: "macos",
    ram_total_bytes: 137438953472, // 128 GiB
    ram_free_for_ai_bytes: null,
    cpu_brand: "Apple M5 Max",
    loaded_models: [],
    lms_unreachable: false,
    utility_model: null,
    redis_url_redacted: null,
    generated_at_ms: 0,
  };

  it("prefers the live /machine/specs probe for THIS machine", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "u1", machine_id: "MacBook-Pro" })];
    expect(specOf(data, new Map(), specs, "u1")).toBe("Apple M5 Max · 128 GB");
  });

  it("still recognizes THIS machine when its records use a different alias than specs reports", () => {
    // One uid, two names — `machine_id` defaults to the hostname, and macOS
    // reports both the short and `.local` forms depending on how the daemon
    // started. `nameOf` answers with the first alias it finds; specs reports
    // the current one. Comparing those two directly made the machine fail to
    // recognize its own hardware and render "hardware not reported".
    const data: FlowRecord[] = [
      rec({ machine_uid: "u1", machine_id: "MacBook-Pro.local" }),
      rec({ machine_uid: "u1", machine_id: "MacBook-Pro" }),
    ];
    expect(specOf(data, new Map(), specs, "u1")).toBe("Apple M5 Max · 128 GB");
  });

  it("does NOT claim this daemon's hardware for a machine that merely shares no alias", () => {
    // The inverted case: a genuinely remote machine must keep falling through
    // to its own presence beat, or the fix would credit every card with the
    // local host's CPU and RAM.
    const data: FlowRecord[] = [rec({ machine_uid: "u2", machine_id: "studio" })];
    const live = new Map([["u2", beat({ machine_uid: "u2", display_name: "studio", specs: "M1 Max · 32 GB" })]]);
    expect(specOf(data, live, specs, "u2")).toBe("M1 Max · 32 GB");
  });

  it("falls back to the presence beat's own spec string for a remote machine", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "u2", machine_id: "studio" })];
    const live = new Map([["u2", beat({ machine_uid: "u2", display_name: "studio", specs: "M1 Max · 32 GB" })]]);
    expect(specOf(data, live, specs, "u2")).toBe("M1 Max · 32 GB");
  });

  it("returns '' (renders the specdim fallback) for a remote machine with no reported hardware", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "u2", machine_id: "studio" })];
    const live = new Map([["u2", beat({ machine_uid: "u2", display_name: "studio" })]]);
    expect(specOf(data, live, specs, "u2")).toBe("");
  });

  it("the unknown bucket names any claimed-but-unverified machine_ids", () => {
    const data: FlowRecord[] = [rec({ machine_id: "someones-laptop" })]; // no machine_uid -> uidOf() = "unknown"
    expect(specOf(data, new Map(), null, "unknown")).toBe("unverified · claimed: someones-laptop");
  });

  it("the unknown bucket with no claimed names reads 'unidentified'", () => {
    expect(specOf([], new Map(), null, "unknown")).toBe("unidentified (no hardware uid)");
  });
});

describe("buildFleetCard", () => {
  it("an absent machine reads 'offline' regardless of activity", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" })];
    const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), /* machAbsent */ true, "u1", true, T_MAX);
    expect(card.stat).toBe("offline");
  });

  it("a present machine with a live dispatch reads 'dispatch in flight'", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" })];
    const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", true, T_MAX);
    expect(card.stat).toBe("dispatch in flight");
    expect(card.runsCount).toBe(1);
    expect(card.runsLabel).toBe("running");
  });

  it("a present machine with no live dispatch reads 'idle', even with completed history", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.complete" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", true, T_MAX);
    expect(card.stat).toBe("idle");
    // LIVE counts only running sessions — a completed dispatch from earlier
    // today must not inflate the count into reading as current crew.
    expect(card.runsCount).toBe(0);
  });

  // (#1800 P2) The replay arm of the SAME two branches. `goldens/playback-date.txt`
  // reads "48 specialists" where `goldens/fleet.txt` reads "0 running"; both
  // come from here, and the port had only the live arm.
  it("replay: counts the whole day's sessions and labels them 'specialists'", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.complete" }),
      rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.complete" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", false, T_MAX);
    expect(card.runsCount).toBe(2);
    expect(card.runsLabel).toBe("specialists");
    // The day's work is over: idle, not "dispatch in flight".
    expect(card.stat).toBe("idle");
  });

  it("replay: one session is 'specialist', singular", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.complete" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", false, T_MAX);
    expect(card.runsCount).toBe(1);
    expect(card.runsLabel).toBe("specialist");
  });

  // The regression this pair guards: the SAME records, the SAME empty live
  // set, differing only in mode. A replay that reused the live arm reports 0.
  it("live vs replay disagree on the same closed-out day, and that is the point", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.complete" }),
    ];
    expect(buildFleetCard(data, new Map(), null, new Set(), false, "u1", true, T_MAX).runsCount).toBe(0);
    expect(buildFleetCard(data, new Map(), null, new Set(), false, "u1", false, T_MAX).runsCount).toBe(1);
  });
});
