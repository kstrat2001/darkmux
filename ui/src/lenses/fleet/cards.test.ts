import { describe, it, expect } from "vitest";
import { machActive, specOf, buildFleetCard, rosterOnlyEntries, rosterAliasFor, specUnknownLabel } from "./cards";
import type { FlowRecord, MachineSpecs, PresenceBeat, RosterMachineEntry } from "../../types/handwritten";
import type { Run } from "../../types/generated/Run";

function run(overrides: Partial<Run> & Pick<Run, "id" | "kind" | "status">): Run {
  return { tracked: true, ...overrides };
}

function rec(overrides: Partial<FlowRecord>): FlowRecord {
  return { ts: "2026-08-08T00:00:00.000Z", ...overrides };
}

function beat(overrides: Partial<PresenceBeat>): PresenceBeat {
  return { machine_uid: "u1", display_name: "studio", schema_version: "1.18.0", beat_ts_ms: 1, ...overrides };
}

function rosterEntry(overrides: Partial<RosterMachineEntry> & Pick<RosterMachineEntry, "id">): RosterMachineEntry {
  return { address: "100.64.1.2:8765", added_unix_ms: 1000, ...overrides };
}

function machineSpecs(overrides: Partial<MachineSpecs> & Pick<MachineSpecs, "machine_id">): MachineSpecs {
  return {
    darkmux_version: "3.7.1",
    flow_schema_version: "1.18.0",
    os: "macos",
    ram_total_bytes: null,
    ram_free_for_ai_bytes: null,
    cpu_brand: null,
    loaded_models: [],
    lms_unreachable: false,
    utility_model: null,
    redis_url_redacted: null,
    generated_at_ms: 0,
    ...overrides,
  };
}

/** The playhead. `/next` has no scrubber, so it is always `tMax`; a value
 *  safely after every fixture timestamp stands in for that. */
const T_MAX = Date.parse("2026-08-09T00:00:00.000Z");

describe("machActive", () => {
  it("is true when a dispatch.start on the machine belongs to a live session", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(["s1"]), "m1", T_MAX)).toBe(true);
  });

  it("is false when the session isn't in the live set", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(), "m1", T_MAX)).toBe(false);
  });

  it("is false for a different machine's live session", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "m2", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(["s1"]), "m1", T_MAX)).toBe(false);
  });

  // (#1800 P2) The replay arm keys on the CLOSE-EDGE, not presence — the live
  // set is empty on a replay by construction, so a presence-keyed check would
  // report every recorded day as idle whether or not it was.
  it("replay: a session closed at or before the playhead is NOT active", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" }),
      rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.complete" }),
    ];
    expect(machActive(data, new Set(), "m1", T_MAX)).toBe(false);
  });

  // The INVERTED case, and the one that proves the check is doing work: same
  // empty live set, no close-edge, FRESH (inside the TTL as of the playhead)
  // -> still active. Without this, a `machActive` hardwired to `false` in
  // replay would pass the test above and look correct. (Playback parity,
  // Change A, finding #7) The start record is now placed just before `T_MAX`
  // — inside `FLOW_LIVE_TTL_MS` — rather than relying on the fixture's
  // far-past default `ts`: `sessionRunning` no longer reads "no close edge"
  // alone as running forever; see the orphan case below and
  // `flow.sessionRunning.parity.test.ts` for the regression this guards.
  it("replay: a session with NO close-edge IS active, on the same empty live set", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start", ts: new Date(T_MAX - 60_000).toISOString() }),
    ];
    expect(machActive(data, new Set(), "m1", T_MAX)).toBe(true);
  });

  // (Playback parity, Change A, finding #7) The case the OLD replay
  // algorithm could not express at all: no close edge, but stale well past
  // `FLOW_LIVE_TTL_MS` as of the playhead — an orphaned session the
  // container's own watchdog would already have killed. The old "no close
  // edge => active" rule read this as running forever.
  it("replay: a session with NO close-edge but stale past the TTL is NOT active", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(), "m1", T_MAX)).toBe(false);
  });

  // `session.end` alone closes a session (`sessionCloseEdge`) — an abandoned
  // or hard-killed dispatch never emits `dispatch.complete`, and reading only
  // the dispatch terminal drew such a machine active forever.
  it("replay: session.end alone closes it, with no dispatch terminal at all", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" }),
      rec({ machine_uid: "m1", session_id: "s1", action: "session.end" }),
    ];
    expect(machActive(data, new Set(), "m1", T_MAX)).toBe(false);
  });

  // (#1869) `T_MAX` was always the day's true max pre-scrubber, so a
  // `dispatch.start` was never AFTER it — this restores legacy's own
  // `visible()` gate (`machActive(m){return visible().some(...)}`), which
  // this port had dropped as an unconditional no-op. A scrubbable playhead
  // makes it a real case: a session that hasn't started yet as of the
  // playhead must not read "in flight", even though `sessionRunning`'s
  // close-edge check (finding no close, because there's nothing to close
  // yet) would otherwise call it running.
  it("replay: a session that hasn't started yet as of the playhead is NOT active", () => {
    const playhead = Date.parse("2026-08-08T00:00:00.000Z"); // before the fixture's own default ts
    const data: FlowRecord[] = [
      rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start", ts: "2026-08-08T00:05:00.000Z" }),
    ];
    expect(machActive(data, new Set(), "m1", playhead)).toBe(false);
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

  // ── (#2814) SELF IS NEVER UNKNOWN ────────────────────────────────────
  //
  // Every `specOf` assertion above hands the self branch a window that
  // already carries the machine's own records, which is what let the
  // `machineNames(...)` join look correct. It is not correct: that set is
  // the names the uid has been OBSERVED under, it lives in the expiring
  // flow window, and it is empty on a fresh install, on a quiet machine
  // with presence off, and after a rename whose old records aged out. The
  // machine standing on its own hardware then reports "hardware not
  // reported" about hardware it read directly.
  const specsWithUid = { ...specs, machine_uid: "F9ACF59C-0E8B-5092-A6B4-7C07070737D2" };

  it("(#2814) recognises THIS machine on an empty window with no beats, via the reported uid", () => {
    expect(specOf([], new Map(), specsWithUid, "F9ACF59C-0E8B-5092-A6B4-7C07070737D2")).toBe("Apple M5 Max · 128 GB");
  });

  it("(#2814) recognises THIS machine when the window knows the uid ONLY under a stale name", () => {
    // The live #2796 shape: one uid, renamed `laptop` -> `MacBook-Pro`. Here
    // only the old name survives in the window, so the alias set holds
    // `laptop` and specs reports `MacBook-Pro` — the name join misses, the
    // uid join cannot.
    const data: FlowRecord[] = [rec({ machine_uid: "F9ACF59C-0E8B-5092-A6B4-7C07070737D2", machine_id: "laptop" })];
    expect(specOf(data, new Map(), specsWithUid, "F9ACF59C-0E8B-5092-A6B4-7C07070737D2")).toBe("Apple M5 Max · 128 GB");
  });

  it("(#2814) a reported uid does NOT credit a different machine with this host's hardware", () => {
    // The inverted case. A remote peer that happens to log under the same
    // NAME this daemon reports would pass the old alias join; it must not
    // pass the uid join.
    const data: FlowRecord[] = [rec({ machine_uid: "u2", machine_id: "MacBook-Pro" })];
    const live = new Map([["u2", beat({ machine_uid: "u2", display_name: "MacBook-Pro", specs: "M1 Max · 32 GB" })]]);
    expect(specOf(data, live, specsWithUid, "u2")).toBe("M1 Max · 32 GB");
  });

  it("(#2814) keeps the alias join when specs reports no uid at all", () => {
    // Non-macOS, a failed `ioreg`, or a peer/static fixture built before the
    // field existed. Absence degrades to the pre-#2814 behavior; it never
    // means "not this machine".
    const data: FlowRecord[] = [rec({ machine_uid: "u1", machine_id: "MacBook-Pro" })];
    expect(specOf(data, new Map(), specs, "u1")).toBe("Apple M5 Max · 128 GB");
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

  // (Playback parity, Change A, findings #3/#4 — 2026-09-24) This used to be
  // "replay: counts the whole day's sessions and labels them 'specialists'"
  // — `goldens/playback-date.txt` read "48 specialists" where
  // `goldens/fleet.txt` read "0 running" for the SAME closed-out day, because
  // replay counted every session that EVER ran that day regardless of the
  // playhead. That is the parity defect the audit named, not a feature: a
  // replay of a day whose work is already finished, probed AT ITS END, now
  // reads "0 running" — the same word and the same count a live viewer would
  // have seen at that instant, because nothing is actually running any more.
  it("replay: two finished sessions read '0 running', not '2 specialists'", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.complete" }),
      rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.complete" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", false, T_MAX);
    expect(card.runsCount).toBe(0);
    expect(card.runsLabel).toBe("running");
    expect(card.stat).toBe("idle");
  });

  // The regression this pair used to guard was the OPPOSITE of parity: "live
  // and replay disagree on the same closed-out day, and that is the point."
  // Change A's whole point is that they must NOT disagree at the same
  // instant — this is the parity check that replaces it.
  it("live and replay AGREE on the same closed-out day, probed at its end (parity)", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.complete" }),
    ];
    expect(buildFleetCard(data, new Map(), null, new Set(), false, "u1", true, T_MAX).runsCount).toBe(0);
    expect(buildFleetCard(data, new Map(), null, new Set(), false, "u1", false, T_MAX).runsCount).toBe(0);
  });

  // (#2060) A mission's own top-level session (`session_id === mission_id`,
  // see `mission_bookend_record`) and a seat step's session it launched
  // (`mission_id` set, `session_id` its own) are the SAME run at the fleet
  // card's grain — one mission dispatching one seat must read "1 running",
  // not "2 running".
  it("(#2060) a mission's own session collapses with its seat/step session into ONE running run", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "u1", session_id: "mission-1", mission_id: "mission-1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "seat-1", mission_id: "mission-1", action: "dispatch.start" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(["mission-1", "seat-1"]), false, "u1", true, T_MAX);
    expect(card.runsCount).toBe(1);
    expect(card.runsLabel).toBe("running");
    // The single "in flight" tap target must land on the MISSION's own
    // session, not whichever seat happened to be encountered first.
    expect(card.runningSessionIds).toEqual(["mission-1"]);
  });

  // (#2060 review) The INVERTED order, and the only case that actually pins
  // the `|| isTopLevel` half of the drill-in preference. `sessionsOn`
  // preserves record order, so when the SEAT's record comes first the
  // mission's own session arrives with a representative already recorded —
  // `!existing` alone would keep the seat and drill-in would land on it.
  // With the mission-first fixture above, `!existing` picks the mission
  // regardless, so that test passes with the preference deleted.
  it("(#2060) the mission's own session wins the drill-in even when a seat's record comes FIRST", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "u1", session_id: "seat-1", mission_id: "mission-1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "mission-1", mission_id: "mission-1", action: "dispatch.start" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(["mission-1", "seat-1"]), false, "u1", true, T_MAX);
    expect(card.runsCount).toBe(1);
    expect(card.runningSessionIds).toEqual(["mission-1"]);
  });

  // (#2060) A concurrent STANDALONE dispatch (no `mission_id`) is genuinely
  // separate activity and must still count on its own alongside the mission.
  it("(#2060) a standalone dispatch beside a running mission still counts as a second run", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "u1", session_id: "mission-1", mission_id: "mission-1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "seat-1", mission_id: "mission-1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "solo-1", action: "dispatch.start" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(["mission-1", "seat-1", "solo-1"]), false, "u1", true, T_MAX);
    expect(card.runsCount).toBe(2);
  });

  // (#2060) Two DIFFERENT missions each with their own live seat must still
  // count as two runs — the collapse is per-mission, not "any mission_id
  // present collapses everything".
  it("(#2060) two different missions' seats never collapse into each other", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "u1", session_id: "mission-1", mission_id: "mission-1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "mission-2", mission_id: "mission-2", action: "dispatch.start" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(["mission-1", "mission-2"]), false, "u1", true, T_MAX);
    expect(card.runsCount).toBe(2);
  });

  // (#2877) Live token-rate scope input. `liveTokRate` is what
  // `FleetLens.tsx` gates the mini scope's mount on — `null` means "render
  // plain idle text, mount zero TokenScope instances".
  describe("liveTokRate", () => {
    // Heartbeats anchored 2s apart, ending exactly AT the playhead `t` —
    // "fresh" for `isStalled`'s purposes, same as a real live poll where the
    // newest heartbeat landed just before the client's own "now".
    const BEAT1 = T_MAX - 2000;
    const BEAT2 = T_MAX;

    it("is null while idle, even with completed heartbeat history", () => {
      const data: FlowRecord[] = [
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT2, generated_chars: 120 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.complete" }),
      ];
      const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", true, T_MAX);
      expect(card.stat).toBe("idle");
      expect(card.liveTokRate).toBeNull();
    });

    it("a running session with fewer than two heartbeats mounts the scope at 0, not no scope", () => {
      const data: FlowRecord[] = [
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT2, generated_chars: 40 } }),
      ];
      const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", true, T_MAX);
      expect(card.stat).toBe("dispatch in flight");
      // One fresh heartbeat: generating, but not enough samples for a rate
      // yet. The scope is up at 0 rather than absent.
      expect(card.liveTokRate).toBe(0);
      expect(card.liveTokState).toBe("generating");
    });

    it("is a positive number once a running session has two FRESH heartbeats to derive Δchars/Δms from", () => {
      const data: FlowRecord[] = [
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT2, generated_chars: 120 } }),
      ];
      const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", true, T_MAX);
      // 80 chars / 2000ms = 40 chars/sec, DEFAULT_CHARS_PER_TOKEN (4) → 10 tok/s.
      expect(card.liveTokRate).toBeCloseTo(10, 5);
      expect(card.liveTokStalled).toBe(false);
    });

    it("sums across two concurrently running sessions on the same machine", () => {
      const data: FlowRecord[] = [
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT2, generated_chars: 120 } }),
        rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.start" }),
        rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
        rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT2, generated_chars: 200 } }),
      ];
      const card = buildFleetCard(data, new Map(), null, new Set(["s1", "s2"]), false, "u1", true, T_MAX);
      // s1: 40 chars/sec / 4 = 10 tok/s. s2: 80 chars/sec / 4 = 20 tok/s.
      expect(card.liveTokRate).toBeCloseTo(30, 5);
    });

    // (Playback parity, Change A, finding #3 — 2026-09-24) This used to be
    // "is always null in replay (liveMode=false), even with a running-shaped
    // session" — the OLD divergent behavior the audit's finding #3 named
    // directly ("live: dispatch in flight · 89 tok/s · 1 running; playback:
    // dispatch in flight · 1 specialist" for the SAME instant). A replay
    // caller's `liveSet` is empty in practice (there is no presence to read
    // about a past day), and the session's own freshness — via
    // `sessionRunning`'s TTL fallback, not presence — is what makes it read
    // as running, in both modes, so the tok/s scope is a fact about the
    // recorded instant rather than a live-only instrument.
    it("computes a real rate for a running-shaped session in replay too (parity)", () => {
      // Record `ts` (not just the heartbeat payload's `sampled_at_ms`) has
      // to be FRESH as of `T_MAX` too — `sessionRunning`'s TTL fallback
      // measures staleness off the record's own `ts`, matching a real flow
      // record where the two are close together. Both defaulted to the
      // fixture's far-past `rec()` default `ts` here would make the
      // session read as an orphan (finding #7's own fix), which is a
      // different case than the one under test.
      const data: FlowRecord[] = [
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start", ts: new Date(BEAT1).toISOString() }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", ts: new Date(BEAT1).toISOString(), payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", ts: new Date(BEAT2).toISOString(), payload: { sampled_at_ms: BEAT2, generated_chars: 120 } }),
      ];
      // Empty `liveSet` — a real replay call never has presence to consult.
      const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", false, T_MAX);
      expect(card.liveTokRate).toBeCloseTo(10, 5);
    });

    // (#2877 dogfood finding, live daemon) A mission observed live stuck
    // `status: "running"` (no terminal record) hours after its last real
    // heartbeat. Before this fix, `aggregateTokenRate` happily reported
    // whatever its LAST two heartbeats measured — a fleet card reading a
    // confident "N tok/s" for a session that stopped producing hours ago.
    it("reads 0, not a stale historical rate, once the session's heartbeats go quiet", () => {
      const data: FlowRecord[] = [
        // `ts` matches the heartbeats' own (equally ancient) clock —
        // `dispatch.start`'s `ts` is the only clock it has, and a mismatched
        // one here (the old `rec()` default, 2026) would read as a NEWER
        // marker than the stale heartbeats and wrongly explain the gap as
        // "prompt" rather than genuinely stalled.
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start", ts: new Date(1_000).toISOString() }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", ts: new Date(1_000).toISOString(), payload: { sampled_at_ms: 1_000, generated_chars: 40 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", ts: new Date(3_000).toISOString(), payload: { sampled_at_ms: 3_000, generated_chars: 120 } }),
      ];
      // The playhead is T_MAX (2026) while the heartbeats above are near
      // epoch 0 — many hours stale by any measure, well past STALL_AFTER_MS.
      const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", true, T_MAX);
      expect(card.liveTokStalled).toBe(true);
      expect(card.liveTokRate).toBe(0);
    });

    it("still works from an OLDER runtime's heartbeat shape (no sampled_at_ms/generated_chars)", () => {
      const data: FlowRecord[] = [
        rec({ ts: "2026-08-08T23:59:58.000Z", machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
        rec({ ts: "2026-08-08T23:59:58.000Z", machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { cumulative_chars: 10 } }),
        rec({ ts: "2026-08-09T00:00:00.000Z", machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { cumulative_chars: 30 } }),
      ];
      // T_MAX ("2026-08-09T00:00:00.000Z") matches the second (fallback,
      // whole-second `ts`-derived) heartbeat exactly — fresh, not stalled.
      const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", true, T_MAX);
      expect(card.liveTokRate).not.toBeNull();
      expect(card.liveTokRate!).toBeGreaterThan(0);
    });
  });

  // (#1923) The gap flow presence genuinely cannot cover: a lab run's
  // NON-dispatch phases (COW sandbox clone, baseline hash, the verify
  // command, scoring — minutes of a long-agentic run), and the Redis-off
  // case. In those windows there is no dispatch in flight, so no bookends
  // and no presence key, and the card would read "idle" while a run is
  // very much live. This is a DISPLAY-layer read of `/runs` (already
  // fleet-aware, already unions lab + flow sources server-side); nothing
  // here writes a lab run into the flow stream.
  it("(#1923) a running lab run makes the card active even with zero flow presence", () => {
    const data: FlowRecord[] = [];
    const machineRuns: Run[] = [run({ id: "lab-1", kind: "lab", status: "running", machine: "u1" })];
    const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", true, T_MAX, new Map(), machineRuns);
    expect(card.stat).toBe("dispatch in flight");
    expect(card.runsCount).toBe(1);
  });

  // (#1923 review) THE regression this trio exists for. A lab run in its
  // DISPATCH phase is visible to BOTH sources at once: the lab
  // `lifecycle.json` row on `/runs` (`status: "running"`, written at start
  // and RAII-guarded) AND the flow session its provider's
  // `darkmux_crew::dispatch::dispatch` call emits contract-2 bookends and a
  // `darkmux:session-presence:<sid>` key for. Summing the two counted that
  // one run twice.
  //
  // Real production shapes, not synthetic ones: the lab run id is
  // `{workload}-{profile}-{epoch_secs}-{i}` (`lab/run.rs`), the dispatch
  // session id `darkmux-coding-{workload}-{epoch_millis}`
  // (`providers/coding_task.rs`, via `session_id::session_id`). They share
  // no join key, and the lab dispatch carries NO `mission_id`, so
  // `topLevelRunSessionIds` treats it as standalone and has nothing to
  // collapse it into.
  it("(#1923) a lab run in its DISPATCH phase counts ONCE, not once per source", () => {
    const labSession = "darkmux-coding-long-agentic-1756000000000";
    const data: FlowRecord[] = [rec({ machine_uid: "u1", session_id: labSession, action: "dispatch.start" })];
    const machineRuns: Run[] = [run({ id: "long-agentic-balanced-1756000000-1", kind: "lab", status: "running", machine: "u1" })];
    const card = buildFleetCard(data, new Map(), null, new Set([labSession]), false, "u1", true, T_MAX, new Map(), machineRuns);
    expect(card.runsCount).toBe(1);
    expect(card.stat).toBe("dispatch in flight");
  });

  // The other side of the same `Math.max`: flow work beyond the lab run's
  // own dispatch must still be counted. Two live flow sessions beside one
  // lab run reads 2 — a merge that clamped to the lab count would pass the
  // test above and be wrong here.
  it("(#1923) flow work beyond the lab run's own dispatch still counts", () => {
    const labSession = "darkmux-coding-long-agentic-1756000000000";
    const data: FlowRecord[] = [
      rec({ machine_uid: "u1", session_id: labSession, action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "solo-1", action: "dispatch.start" }),
    ];
    const machineRuns: Run[] = [run({ id: "long-agentic-balanced-1756000000-1", kind: "lab", status: "running", machine: "u1" })];
    const card = buildFleetCard(data, new Map(), null, new Set([labSession, "solo-1"]), false, "u1", true, T_MAX, new Map(), machineRuns);
    expect(card.runsCount).toBe(2);
  });

  // TWO concurrent lab runs, both between dispatches (no flow presence at
  // all): the lab side is what the max has to yield to here. A merge that
  // read only the flow side would report 0.
  it("(#1923) two lab runs with no live dispatch between them still count as two", () => {
    const machineRuns: Run[] = [
      run({ id: "lab-a", kind: "lab", status: "running", machine: "u1" }),
      run({ id: "lab-b", kind: "lab", status: "running", machine: "u1" }),
    ];
    const card = buildFleetCard([], new Map(), null, new Set(), false, "u1", true, T_MAX, new Map(), machineRuns);
    expect(card.runsCount).toBe(2);
  });

  // Only a RUNNING lab run counts — a completed or errored one is history,
  // not current activity, same rule flow presence already applies.
  it("(#1923) a completed lab run does not count as active", () => {
    const data: FlowRecord[] = [];
    const machineRuns: Run[] = [run({ id: "lab-1", kind: "lab", status: "complete", machine: "u1" })];
    const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", true, T_MAX, new Map(), machineRuns);
    expect(card.stat).toBe("idle");
    expect(card.runsCount).toBe(0);
  });

  // A running MISSION/DISPATCH row in `/runs` must NOT be double-counted —
  // that machine's activity is already fully accounted for by flow
  // presence (post-#2060). Only `kind === "lab"` rows are net-new signal.
  it("(#1923) a running mission row in /runs is not double-counted against flow presence", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" })];
    const machineRuns: Run[] = [run({ id: "s1", kind: "dispatch", status: "running", machine: "u1" })];
    const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", true, T_MAX, new Map(), machineRuns);
    expect(card.runsCount).toBe(1);
  });

  // (#1855) A rostered-but-never-seen machine must render the SAME "offline"
  // stat a machine that WAS seen and has since gone quiet already uses — the
  // shared indicator vocabulary this project's "no snowflakes" rule asks
  // for, rather than a new "silent"/"unknown" state. `entry.id` stands in
  // for `m` directly, matching how `FleetLens.tsx` calls this for a roster
  // entry with no known uid.
  it("(#1855) a rostered entry with no known identity reads 'offline', not 'idle'", () => {
    const card = buildFleetCard([], new Map(), null, new Set(), /* machAbsent */ true, "studio", true, T_MAX);
    expect(card.stat).toBe("offline");
    expect(card.active).toBe(false);
    expect(card.runsCount).toBe(0);
    expect(card.name).toBe("studio");
    // Never having reported hardware is honest, not a bug — darkmux has
    // genuinely never heard from this machine.
    expect(card.spec).toBe("");
  });

  // (#2814) The whole card for THIS machine on an empty window. `nameOf`
  // answers with the raw uid when the window holds no record naming it —
  // correct for a uid nothing is known about, and wrong for the one uid the
  // daemon can name from its own config. A card titled with a 36-character
  // UUID is the display half of "self is unknown".
  it("(#2814) this machine's own card carries its name and hardware on an empty window", () => {
    const uid = "F9ACF59C-0E8B-5092-A6B4-7C07070737D2";
    const specs = machineSpecs({
      machine_id: "MacBook-Pro",
      machine_uid: uid,
      cpu_brand: "Apple M5 Max",
      ram_total_bytes: 137438953472,
    });
    const card = buildFleetCard([], new Map(), specs, new Set(), /* machAbsent */ false, uid, true, T_MAX);
    expect(card.name).toBe("MacBook-Pro");
    expect(card.spec).toBe("Apple M5 Max · 128 GB");
    expect(card.specUnknown).toBeNull();
  });

  // Inverted: an OBSERVED name still wins the title. The specs name is a
  // floor for the gap `nameOf` cannot fill, never an override of live
  // observation (#2030 — a value that cannot be outvoted is the defect, not
  // the fix).
  it("(#2814) a name the window actually observed still outranks the specs name", () => {
    const uid = "F9ACF59C-0E8B-5092-A6B4-7C07070737D2";
    const specs = machineSpecs({ machine_id: "MacBook-Pro", machine_uid: uid, cpu_brand: "Apple M5 Max" });
    const data: FlowRecord[] = [rec({ machine_uid: uid, machine_id: "MacBook-Pro.local" })];
    const card = buildFleetCard(data, new Map(), specs, new Set(), false, uid, true, T_MAX);
    expect(card.name).toBe("MacBook-Pro.local");
  });
});

describe("rosterOnlyEntries", () => {
  // (#1855) THE defect this closes: `machineUids` only ever unions
  // flow-derived uids with CURRENTLY-beating presence keys, so a roster
  // entry with neither produced no uid for the card list to fall back on —
  // the machine vanished from the dashboard entirely, indistinguishable
  // from never having been added.
  it("a roster entry with no flow record and no presence beat is reported roster-only", () => {
    const roster = [rosterEntry({ id: "studio" })];
    expect(rosterOnlyEntries([], new Map(), roster)).toEqual(roster);
  });

  // The INVERTED case, and the one that proves this doesn't just echo the
  // roster back unfiltered: a machine that IS live (or has flow history)
  // under the exact name the roster declares must NOT be reported here too
  // — reporting it would draw a duplicate "offline" card next to its real,
  // live one for the same machine.
  it("a roster entry already covered by a live presence beat under the same name is excluded", () => {
    const roster = [rosterEntry({ id: "studio" })];
    const live = new Map([["u1", beat({ machine_uid: "u1", display_name: "studio" })]]);
    expect(rosterOnlyEntries([], live, roster)).toEqual([]);
  });

  // Same exclusion, but via flow history rather than live presence — a
  // machine that has previously reported under this name (and might simply
  // be between beats right now) is already covered by the ordinary
  // `machineUids` union and must not ALSO get a roster-only phantom card.
  it("a roster entry already covered by flow history under the same name is excluded", () => {
    const roster = [rosterEntry({ id: "studio" })];
    const data: FlowRecord[] = [rec({ machine_uid: "u1", machine_id: "studio" })];
    expect(rosterOnlyEntries(data, new Map(), roster)).toEqual([]);
  });

  // A mixed roster: one entry covered, one genuinely silent — only the
  // silent one comes back. Proves the filter is per-entry, not all-or-none.
  it("filters a mixed roster down to only the genuinely-unaccounted entries", () => {
    const roster = [rosterEntry({ id: "studio" }), rosterEntry({ id: "mini-1" })];
    const live = new Map([["u1", beat({ machine_uid: "u1", display_name: "studio" })]]);
    expect(rosterOnlyEntries([], live, roster)).toEqual([rosterEntry({ id: "mini-1" })]);
  });

  it("an empty roster reports nothing, on an otherwise busy fleet", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "u1", machine_id: "studio" })];
    expect(rosterOnlyEntries(data, new Map(), [])).toEqual([]);
  });

  // (#1855 follow-up, F1) The self-machine phantom: presence self-disables
  // when Redis is unset, so a quiet window can carry NO flow record and NO
  // beat for the daemon serving the page, even though its own roster entry
  // exists (`darkmux-add-machine`'s own step 7). Without consulting
  // `/machine/specs` this used to fall straight through `knownNames`,
  // reporting the daemon's own roster entry as a phantom "offline" card —
  // served by the very machine it calls offline.
  it("excludes a roster entry matching THIS machine's own /machine/specs identity, even with zero flow/presence history", () => {
    const roster = [rosterEntry({ id: "studio" })];
    const specs = machineSpecs({ machine_id: "studio" });
    expect(rosterOnlyEntries([], new Map(), roster, specs)).toEqual([]);
  });

  // The inverted case pinned again at THIS call site (not just `specOf`'s):
  // a `specs.machine_id` that doesn't match the roster entry must not
  // suppress it — this is a targeted self-check, not a blanket "specs
  // present, trust everything" escape hatch.
  it("does NOT exclude a roster entry that specs.machine_id doesn't match", () => {
    const roster = [rosterEntry({ id: "studio" })];
    const specs = machineSpecs({ machine_id: "some-other-machine" });
    expect(rosterOnlyEntries([], new Map(), roster, specs)).toEqual(roster);
  });

  // No specs at all (the default, pre-existing call sites, or a static
  // build) behaves exactly as before — `specs` is optional and additive.
  it("with no specs argument, behaves exactly as before (backward compatible)", () => {
    const roster = [rosterEntry({ id: "studio" })];
    expect(rosterOnlyEntries([], new Map(), roster)).toEqual(roster);
  });

  // (#1855 follow-up, F2) The mismatched-name duplicate: a live peer beating
  // under one alias and rostered under a near-miss of it (case, stray
  // whitespace, or the mDNS `.local` suffix) used to render BOTH a live
  // card and a phantom "offline" roster-only card for the same machine.
  it("excludes a roster entry that differs from a live beat only by case", () => {
    const roster = [rosterEntry({ id: "Studio" })];
    const live = new Map([["u1", beat({ machine_uid: "u1", display_name: "studio" })]]);
    expect(rosterOnlyEntries([], live, roster)).toEqual([]);
  });

  it("excludes a roster entry that differs from flow history only by the mDNS .local suffix", () => {
    const roster = [rosterEntry({ id: "MacBook-Pro" })];
    const data: FlowRecord[] = [rec({ machine_uid: "u1", machine_id: "MacBook-Pro.local" })];
    expect(rosterOnlyEntries(data, new Map(), roster)).toEqual([]);
  });

  it("excludes a roster entry with stray leading/trailing whitespace around an otherwise-matching name", () => {
    const roster = [rosterEntry({ id: "  studio  " })];
    const live = new Map([["u1", beat({ machine_uid: "u1", display_name: "studio" })]]);
    expect(rosterOnlyEntries([], live, roster)).toEqual([]);
  });

  // The inverted case for F2: a roster id sharing no normalized substring
  // with any known alias is still reported — widening the match must not
  // become "everything on the roster is presumed accounted for."
  it("still reports a roster entry whose name shares nothing with any known alias", () => {
    const roster = [rosterEntry({ id: "mini-1" })];
    const live = new Map([["u1", beat({ machine_uid: "u1", display_name: "studio" })]]);
    expect(rosterOnlyEntries([], live, roster)).toEqual(roster);
  });

  // (#2768) THE defect this closes: three generations of rename share no
  // substring at all — `laptop` and `MacBook-Pro` fail every name-based
  // check above (exact, case-fold, `.local`-strip, whitespace-trim). A
  // same-name fixture would pass against the bug this is meant to catch;
  // this one is deliberately shaped so ONLY the uid join can exclude it.
  it("excludes a roster entry whose machine_uid matches a live beat reporting under a WHOLLY DIFFERENT name", () => {
    const roster = [rosterEntry({ id: "laptop", machine_uid: "F9ACF59C-UID" })];
    const live = new Map([["F9ACF59C-UID", beat({ machine_uid: "F9ACF59C-UID", display_name: "MacBook-Pro" })]]);
    expect(rosterOnlyEntries([], live, roster)).toEqual([]);
  });

  // Same shape, via flow history under the new name rather than a live beat
  // — the uid join must work off `machineUids`'s flow-derived half too, not
  // only the presence-beat half.
  it("excludes a roster entry whose machine_uid matches flow history under a different name", () => {
    const roster = [rosterEntry({ id: "laptop", machine_uid: "F9ACF59C-UID" })];
    const data: FlowRecord[] = [rec({ machine_uid: "F9ACF59C-UID", machine_id: "MacBook-Pro" })];
    expect(rosterOnlyEntries(data, new Map(), roster)).toEqual([]);
  });

  // The inverted case, red-proving the join is keyed on the VALUE, not
  // merely on the field's presence: a roster entry CARRYING a machine_uid
  // that does not match anything currently known is still a genuinely
  // silent machine and must still be reported — "rostered, never seen"
  // stays a real, renderable state (issue #2768's own constraint).
  it("still reports a roster entry with a machine_uid that matches no known uid", () => {
    const roster = [rosterEntry({ id: "mini-1", machine_uid: "UNSEEN-UID" })];
    const live = new Map([["F9ACF59C-UID", beat({ machine_uid: "F9ACF59C-UID", display_name: "MacBook-Pro" })]]);
    expect(rosterOnlyEntries([], live, roster)).toEqual(roster);
  });

  // An entry with NO machine_uid at all (every pre-#2768 roster, and every
  // remote peer added by network address) behaves exactly as before — the
  // uid branch never fires, so the pre-existing name-based checks are the
  // only thing that can exclude it. Constraint 1 from #2768: absence must
  // never fall back to a uid guess.
  it("a roster entry with no machine_uid falls through to the pre-existing name-matching behavior unchanged", () => {
    const roster = [rosterEntry({ id: "laptop" })];
    const live = new Map([["F9ACF59C-UID", beat({ machine_uid: "F9ACF59C-UID", display_name: "MacBook-Pro" })]]);
    // No uid to join on, and the names share nothing — still reported.
    expect(rosterOnlyEntries([], live, roster)).toEqual(roster);
  });

  // (#2814) The F1 self-check above already suppresses a roster entry whose
  // id equals `specs.machine_id`. It cannot suppress one the operator
  // declared under an OLD name — `laptop` for a machine that now calls
  // itself `MacBook-Pro` — and since #2814 puts the self uid into the card
  // list unconditionally, that entry would draw a second, "offline" card
  // beside the machine's own live one. The uid is the join that survives
  // the rename.
  it("(#2814) excludes a roster entry declaring THIS machine's uid under a stale name, on an empty window", () => {
    const roster = [rosterEntry({ id: "laptop", machine_uid: "F9ACF59C-UID" })];
    const specs = machineSpecs({ machine_id: "MacBook-Pro", machine_uid: "F9ACF59C-UID" });
    expect(rosterOnlyEntries([], new Map(), roster, specs)).toEqual([]);
  });

  it("(#2814) still reports a roster entry whose uid is NOT this machine's, on the same empty window", () => {
    // Inverted: the self uid must suppress only the entry that names it.
    const roster = [rosterEntry({ id: "studio", machine_uid: "OTHER-UID" })];
    const specs = machineSpecs({ machine_id: "MacBook-Pro", machine_uid: "F9ACF59C-UID" });
    expect(rosterOnlyEntries([], new Map(), roster, specs)).toEqual(roster);
  });
});

describe("rosterAliasFor", () => {
  // (#2802 regression fix) This replaces `rosterLabelFor`, which returned the
  // same string to OVERRIDE a card's title. That override was inert while
  // roster entries carried no uid; once #2802 began back-filling uids from
  // flow history it started firing on entries nobody had aliased on purpose,
  // and the card for this machine rendered as `laptop` while the activity
  // lane beneath it said `MacBook-Pro`.
  it("returns the operator's alias when it differs from the machine's own name", () => {
    const roster = [rosterEntry({ id: "laptop", machine_uid: "F9ACF59C-UID" })];
    expect(rosterAliasFor("F9ACF59C-UID", roster, "MacBook-Pro")).toBe("laptop");
  });

  // THE REGRESSION, pinned: the alias must never become the title. A caller
  // applies this as secondary text; the machine's own name is the title.
  it("does not return an alias equal to the machine's own name", () => {
    const roster = [rosterEntry({ id: "MacBook-Pro", machine_uid: "F9ACF59C-UID" })];
    expect(
      rosterAliasFor("F9ACF59C-UID", roster, "MacBook-Pro"),
    ).toBeUndefined();
  });

  // Same machine, same name, different spelling — `.local` and case are the
  // two aliases a single machine legitimately carries (see `nameOf`'s #2030
  // doc), and showing either beside the other is noise, not provenance.
  it("folds a .local or case variant rather than showing the name twice", () => {
    const roster = [rosterEntry({ id: "macbook-pro.local", machine_uid: "F9ACF59C-UID" })];
    expect(
      rosterAliasFor("F9ACF59C-UID", roster, "MacBook-Pro"),
    ).toBeUndefined();
  });

  // Inverted case: no roster entry names this uid — never invent an alias.
  it("returns undefined when no roster entry names this uid", () => {
    const roster = [rosterEntry({ id: "laptop", machine_uid: "F9ACF59C-UID" })];
    expect(rosterAliasFor("some-other-uid", roster, "MacBook-Pro")).toBeUndefined();
  });

  // An entry with no machine_uid at all never matches any uid.
  it("returns undefined for a roster with no resolved uids", () => {
    const roster = [rosterEntry({ id: "laptop" })];
    expect(rosterAliasFor("F9ACF59C-UID", roster, "MacBook-Pro")).toBeUndefined();
  });
});

/**
 * (#1855) A card with no hardware line used to say ONE thing for two facts.
 *
 * The issue's own wire dump is the `not-reported` case: both real machines
 * beat with no `specs` key at all, because the emitter hardcoded
 * `specs: None` until #2083. That peer ANSWERED and said nothing about its
 * hardware, and "hardware not reported" is the honest sentence for it.
 *
 * The `not-seen` case is the one #1855's roster cards created. A machine the
 * operator declared with `darkmux machine add` that is down, or has never
 * started its daemon, now renders a card instead of vanishing — and that card
 * was asserting the machine had reported and withheld its hardware, when
 * nothing had ever been received from it at all. Same class as the vanishing
 * itself: a confident answer the page cannot back up.
 */
describe("(#1855) the spec line says WHICH kind of unknown", () => {
  const T = 9e15;

  it("a machine that beat WITHOUT specs reads 'not-reported' — it answered and said nothing", () => {
    const live = new Map([["u1", beat({ machine_uid: "u1" })]]);
    const card = buildFleetCard([], live, null, new Set(), false, "u1", true, T, live);
    expect(card.spec).toBe("");
    expect(card.specUnknown).toBe("not-reported");
    expect(specUnknownLabel(card.specUnknown!)).toBe("hardware not reported");
  });

  it("a machine nothing has been received from reads 'not-seen'", () => {
    // The rostered-but-silent card: forced absent, no beat, no snapshot entry.
    const card = buildFleetCard([], new Map(), null, new Set(), /* machAbsent */ true, "studio-2", true, T);
    expect(card.spec).toBe("");
    expect(card.specUnknown).toBe("not-seen");
    expect(specUnknownLabel(card.specUnknown!)).toBe("hardware unknown — nothing received");
  });

  it("a machine WITH hardware reports no unknown at all", () => {
    // Inverted case 1 — the healthy card must carry no marker of any kind.
    const live = new Map([["u1", beat({ machine_uid: "u1", specs: "Apple M5 Max · 128 GB" })]]);
    const card = buildFleetCard([], live, null, new Set(), false, "u1", true, T, live);
    expect(card.spec).toBe("Apple M5 Max · 128 GB");
    expect(card.specUnknown).toBeNull();
  });

  it("THIS machine's own /machine/specs read also clears the unknown", () => {
    // Inverted case 2: the local card resolves its hardware from the specs
    // probe, not from a beat, and must not be pushed into either unknown
    // bucket just because presence is switched off.
    const data = [rec({ machine_uid: "u1", machine_id: "MacBook-Pro", action: "dispatch.start" })];
    const specs = machineSpecs({ machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max", ram_total_bytes: 137438953472 });
    const card = buildFleetCard(data, new Map(), specs, new Set(), false, "u1", true, T);
    expect(card.spec).toBe("Apple M5 Max · 128 GB");
    expect(card.specUnknown).toBeNull();
  });

  it("the two labels are different sentences — a regression that collapsed them would be invisible otherwise", () => {
    expect(specUnknownLabel("not-seen")).not.toBe(specUnknownLabel("not-reported"));
  });
});
