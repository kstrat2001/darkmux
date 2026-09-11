import { describe, it, expect } from "vitest";
import { machActive, specOf, buildFleetCard, rosterOnlyEntries } from "./cards";
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
    expect(machActive(data, new Set(), "m1", false, playhead)).toBe(false);
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
});
