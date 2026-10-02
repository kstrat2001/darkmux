import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { machActive, specOf, busiestExecution, isStrictlyBusier, specUnknownLabel, cardFace, notStreamedNames, servesLine, servesParts, executionCountText, shownExecution } from "./cards";
import { CardStatus, STATUS_WORD, lampOf, secondLineOf, statusReason } from "./cardStatus";
import { buildFleetCard, faceOf } from "../../testing/fleetCard";
import { LampForm } from "../../lib/lamp";
import { utilityReading, UtilityResidency } from "../../lib/utilityJobs";
import { outcomeLine, rowStanding, type RowFacts } from "./viewRows";
import { machineAvailability } from "../../lib/machineAvailability";
import type { FleetMachine } from "../../types/generated/FleetMachine";
import type { MachineSpecsResponse } from "../../types/generated/MachineSpecsResponse";
import type { PresenceBeat } from "../../types/generated/PresenceBeat";
import type { ExecutionTokenReading } from "../../lib/tokenRate";
import { norm, type RawRecord } from "../../testing/records";
import type { Run } from "../../types/generated/Run";
import type { NormRecord } from "../../lib/ingest";
import { liveSampleToRecord, type LiveOverlay } from "../../lib/liveChannel";

function run(overrides: Partial<Run> & Pick<Run, "id" | "kind" | "status">): Run {
  return { tracked: true, receive_key: 0, ...overrides };
}

function rec(overrides: RawRecord): NormRecord {
  return norm({ ts: "2026-08-08T00:00:00.000Z", ...overrides });
}

function beat(overrides: Partial<PresenceBeat>): PresenceBeat {
  return { machine_uid: "u1", display_name: "studio", schema_version: "1.18.0", beat_ts_ms: 1, ...overrides };
}

/** A fleet-view row's facts, as `viewRows.ts::rowFacts` would hand them to a card. */
function rowFactsFor(overrides: Partial<RowFacts> = {}): RowFacts {
  return { uid: "u1", known: true, name: null, names: [], spec: "", note: null, standing: "online", liveness: "live", isSelf: false, hub: false, servesRadio: false, servesProfiles: 0, utility: utilityReading(false, null), ...overrides };
}

function machineSpecs(overrides: Partial<MachineSpecsResponse> & Pick<MachineSpecsResponse, "machine_id">): MachineSpecsResponse {
  return {
    darkmux_version: "3.7.1",
    flow_schema_version: "1.18.0",
    machine_uid: null,
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
  // (#2902 step 5, 5th review MF1) A hosted call held by its budget writes
  // `budget.wait` BEFORE any `dispatch start`: the machine is in flight
  // while the wait is open, live or in playback, with or without presence.
  it("is true for a machine whose only session is an open budget wait (no dispatch start yet)", () => {
    const data: NormRecord[] = [
      rec({ ts: "2026-08-08T20:00:00.000Z", machine_uid: "m1", session_id: "s1", action: "budget.wait", payload: { endpoint_id: "azure", wait_ms: 86000000 } }),
    ];
    expect(machActive(data, new Set(), "m1", T_MAX)).toBe(true);
    expect(machActive(data, new Set(["s1"]), "m1", T_MAX)).toBe(true);
    const resumed = [...data, rec({ ts: "2026-08-08T21:00:00.000Z", machine_uid: "m1", session_id: "s1", action: "budget.resume" })];
    expect(machActive(resumed, new Set(), "m1", T_MAX)).toBe(false);
  });

  it("is true when a dispatch.start on the machine belongs to a live session", () => {
    const data: NormRecord[] = [rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(["s1"]), "m1", T_MAX)).toBe(true);
  });

  it("is false when the session isn't in the live set", () => {
    const data: NormRecord[] = [rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(), "m1", T_MAX)).toBe(false);
  });

  it("is false for a different machine's live session", () => {
    const data: NormRecord[] = [rec({ machine_uid: "m2", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(["s1"]), "m1", T_MAX)).toBe(false);
  });

  // (#1800 P2) The replay arm keys on the CLOSE-EDGE, not presence — the live
  // set is empty on a replay by construction, so a presence-keyed check would
  // report every recorded day as idle whether or not it was.
  it("replay: a session closed at or before the playhead is NOT active", () => {
    const data: NormRecord[] = [
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
  // — inside the staleness window — rather than relying on the fixture's
  // far-past default `ts`: the lifecycle never reads "no close edge" alone
  // as running forever; see the orphan case below and
  // `tests/lifecycle/cases.json` for the regression this guards.
  it("replay: a session with NO close-edge IS active, on the same empty live set", () => {
    const data: NormRecord[] = [
      rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start", ts: new Date(T_MAX - 60_000).toISOString() }),
    ];
    expect(machActive(data, new Set(), "m1", T_MAX)).toBe(true);
  });

  // (Playback parity, Change A, finding #7) The case the OLD replay
  // algorithm could not express at all: no close edge, but stale well past
  // the staleness window as of the playhead — an orphaned session the
  // container's own watchdog would already have killed. The old "no close
  // edge => active" rule read this as running forever.
  it("replay: a session with NO close-edge but stale past the TTL is NOT active", () => {
    const data: NormRecord[] = [rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(), "m1", T_MAX)).toBe(false);
  });

  // `session.end` alone closes a session (`lib/lifecycle.ts`) — an abandoned
  // or hard-killed dispatch never emits `dispatch.complete`, and reading only
  // the dispatch terminal drew such a machine active forever.
  it("replay: session.end alone closes it, with no dispatch terminal at all", () => {
    const data: NormRecord[] = [
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
  // playhead must not read "in flight": its lifecycle reads `not_started`
  // there.
  it("replay: a session that hasn't started yet as of the playhead is NOT active", () => {
    const playhead = Date.parse("2026-08-08T00:00:00.000Z"); // before the fixture's own default ts
    const data: NormRecord[] = [
      rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start", ts: "2026-08-08T00:05:00.000Z" }),
    ];
    expect(machActive(data, new Set(), "m1", playhead)).toBe(false);
  });
});

describe("specOf", () => {
  const specs: MachineSpecsResponse = {
    darkmux_version: "2.5.0",
    flow_schema_version: "1.18.0",
    machine_id: "MacBook-Pro",
    machine_uid: null,
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
    const data: NormRecord[] = [rec({ machine_uid: "u1", machine_id: "MacBook-Pro" })];
    expect(specOf(data, new Map(), specs, "u1")).toBe("Apple M5 Max · 128 GB");
  });

  it("still recognizes THIS machine when its records use a different alias than specs reports", () => {
    // One uid, two names — `machine_id` defaults to the hostname, and macOS
    // reports both the short and `.local` forms depending on how the daemon
    // started. `nameOf` answers with the first alias it finds; specs reports
    // the current one. Comparing those two directly made the machine fail to
    // recognize its own hardware and render "hardware not reported".
    const data: NormRecord[] = [
      rec({ machine_uid: "u1", machine_id: "MacBook-Pro.local" }),
      rec({ machine_uid: "u1", machine_id: "MacBook-Pro" }),
    ];
    expect(specOf(data, new Map(), specs, "u1")).toBe("Apple M5 Max · 128 GB");
  });

  it("does NOT claim this daemon's hardware for a machine that merely shares no alias", () => {
    // The inverted case: a genuinely remote machine must keep falling through
    // to its own presence beat, or the fix would credit every card with the
    // local host's CPU and RAM.
    const data: NormRecord[] = [rec({ machine_uid: "u2", machine_id: "studio" })];
    const live = new Map([["u2", beat({ machine_uid: "u2", display_name: "studio", specs: "M1 Max · 32 GB" })]]);
    expect(specOf(data, live, specs, "u2")).toBe("M1 Max · 32 GB");
  });

  it("falls back to the presence beat's own spec string for a remote machine", () => {
    const data: NormRecord[] = [rec({ machine_uid: "u2", machine_id: "studio" })];
    const live = new Map([["u2", beat({ machine_uid: "u2", display_name: "studio", specs: "M1 Max · 32 GB" })]]);
    expect(specOf(data, live, specs, "u2")).toBe("M1 Max · 32 GB");
  });

  it("returns '' (renders the specdim fallback) for a remote machine with no reported hardware", () => {
    const data: NormRecord[] = [rec({ machine_uid: "u2", machine_id: "studio" })];
    const live = new Map([["u2", beat({ machine_uid: "u2", display_name: "studio" })]]);
    expect(specOf(data, live, specs, "u2")).toBe("");
  });

  it("the unknown bucket names any claimed-but-unverified machine_ids", () => {
    const data: NormRecord[] = [rec({ machine_id: "someones-laptop" })]; // no machine_uid -> uidOf() = "unknown"
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
  const specsWithUid = { ...specs, machine_uid: "00000000-0000-4000-8000-ABCDEF000011" };

  it("(#2814) recognises THIS machine on an empty window with no beats, via the reported uid", () => {
    expect(specOf([], new Map(), specsWithUid, "00000000-0000-4000-8000-ABCDEF000011")).toBe("Apple M5 Max · 128 GB");
  });

  it("(#2814) recognises THIS machine when the window knows the uid ONLY under a stale name", () => {
    // The live #2796 shape: one uid, renamed `laptop` -> `MacBook-Pro`. Here
    // only the old name survives in the window, so the alias set holds
    // `laptop` and specs reports `MacBook-Pro` — the name join misses, the
    // uid join cannot.
    const data: NormRecord[] = [rec({ machine_uid: "00000000-0000-4000-8000-ABCDEF000011", machine_id: "laptop" })];
    expect(specOf(data, new Map(), specsWithUid, "00000000-0000-4000-8000-ABCDEF000011")).toBe("Apple M5 Max · 128 GB");
  });

  it("(#2814) a reported uid does NOT credit a different machine with this host's hardware", () => {
    // The inverted case. A remote peer that happens to log under the same
    // NAME this daemon reports would pass the old alias join; it must not
    // pass the uid join.
    const data: NormRecord[] = [rec({ machine_uid: "u2", machine_id: "MacBook-Pro" })];
    const live = new Map([["u2", beat({ machine_uid: "u2", display_name: "MacBook-Pro", specs: "M1 Max · 32 GB" })]]);
    expect(specOf(data, live, specsWithUid, "u2")).toBe("M1 Max · 32 GB");
  });

  it("(#2814) keeps the alias join when specs reports no uid at all", () => {
    // Non-macOS, a failed `ioreg`, or a peer/static fixture built before the
    // field existed. Absence degrades to the pre-#2814 behavior; it never
    // means "not this machine".
    const data: NormRecord[] = [rec({ machine_uid: "u1", machine_id: "MacBook-Pro" })];
    expect(specOf(data, new Map(), specs, "u1")).toBe("Apple M5 Max · 128 GB");
  });
});

describe("runningRuns: bookkeeping is not a run", () => {
  // A mission's own lifecycle session (`mission.start`, no close yet) and a
  // scheduler task session are open lifecycles, but no model work: the card
  // counts runs, not bookkeeping.
  it("an open mission lifecycle session alone reads idle", () => {
    const data: NormRecord[] = [rec({ machine_uid: "u1", session_id: "mission-m1", mission_id: "m1", action: "mission.start" })];
    const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", Date.parse("2026-08-08T00:01:00.000Z"));
    expect(card.runsCount).toBe(0);
    expect(faceOf(card).status).toBe(CardStatus.Idle);
  });
});

describe("buildFleetCard", () => {
  it("an absent machine reads 'offline' regardless of activity", () => {
    const data: NormRecord[] = [rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" })];
    const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), /* machAbsent */ true, "u1", T_MAX);
    expect(faceOf(card).status).toBe(CardStatus.Offline);
  });

  it("a present machine with a live dispatch reads 'dispatch in flight'", () => {
    const data: NormRecord[] = [rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" })];
    const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX);
    expect(faceOf(card).status).toBe(CardStatus.Running);
    expect(card.runsCount).toBe(1);
  });

  it("a present machine with no live dispatch reads 'idle', even with completed history", () => {
    const data: NormRecord[] = [
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.complete" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", T_MAX);
    expect(faceOf(card).status).toBe(CardStatus.Idle);
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
    const data: NormRecord[] = [
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.complete" }),
      rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.complete" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", T_MAX);
    expect(card.runsCount).toBe(0);
    expect(faceOf(card).status).toBe(CardStatus.Idle);
  });

  // The regression this pair used to guard was the OPPOSITE of parity: "live
  // and replay disagree on the same closed-out day, and that is the point."
  // Change A's whole point is that they must NOT disagree at the same
  // instant — this is the parity check that replaces it.
  it("live and replay AGREE on the same closed-out day, probed at its end (parity)", () => {
    const data: NormRecord[] = [
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.complete" }),
    ];
    expect(buildFleetCard(data, new Map(), null, new Set(), false, "u1", T_MAX).runsCount).toBe(0);
    expect(buildFleetCard(data, new Map(), null, new Set(), false, "u1", T_MAX).runsCount).toBe(0);
  });

  // (#2060) A mission's own run session (opened by `run.start`, see
  // `run_bookend_record`) and a seat step's session it launched
  // (`mission_id` set, `session_id` its own) are the SAME run at the fleet
  // card's grain — one mission dispatching one seat must read "1 running",
  // not "2 running".
  it("(#2060) a mission's own session collapses with its seat/step session into ONE running run", () => {
    const data: NormRecord[] = [
      rec({ machine_uid: "u1", session_id: "mission-1.run", mission_id: "mission-1", action: "run.start" }),
      rec({ machine_uid: "u1", session_id: "seat-1", mission_id: "mission-1", action: "dispatch.start" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(["mission-1.run", "seat-1"]), false, "u1", T_MAX);
    expect(card.runsCount).toBe(1);
    // The single "in flight" tap target must land on the MISSION's own
    // session, not whichever seat happened to be encountered first.
    expect(card.runningSessionIds).toEqual(["mission-1.run"]);
  });

  // (#2060 review) The INVERTED order, and the only case that actually pins
  // the "prefer the mission's own session" half of the drill-in preference.
  // The run index preserves record order, so when the SEAT's record comes first the
  // mission's own session arrives with a representative already recorded —
  // `!existing` alone would keep the seat and drill-in would land on it.
  // With the mission-first fixture above, `!existing` picks the mission
  // regardless, so that test passes with the preference deleted.
  it("(#2060) the mission's own session wins the drill-in even when a seat's record comes FIRST", () => {
    const data: NormRecord[] = [
      rec({ machine_uid: "u1", session_id: "seat-1", mission_id: "mission-1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "mission-1.run", mission_id: "mission-1", action: "run.start" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(["mission-1.run", "seat-1"]), false, "u1", T_MAX);
    expect(card.runsCount).toBe(1);
    expect(card.runningSessionIds).toEqual(["mission-1.run"]);
  });

  // (#2060) A concurrent STANDALONE dispatch (no `mission_id`) is genuinely
  // separate activity and must still count on its own alongside the mission.
  it("(#2060) a standalone dispatch beside a running mission still counts as a second run", () => {
    const data: NormRecord[] = [
      rec({ machine_uid: "u1", session_id: "mission-1.run", mission_id: "mission-1", action: "run.start" }),
      rec({ machine_uid: "u1", session_id: "seat-1", mission_id: "mission-1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "solo-1", action: "dispatch.start" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(["mission-1.run", "seat-1", "solo-1"]), false, "u1", T_MAX);
    expect(card.runsCount).toBe(2);
  });

  // (#2060) Two DIFFERENT missions each with their own live seat must still
  // count as two runs — the collapse is per-mission, not "any mission_id
  // present collapses everything".
  it("(#2060) two different missions' seats never collapse into each other", () => {
    const data: NormRecord[] = [
      rec({ machine_uid: "u1", session_id: "mission-1", mission_id: "mission-1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "mission-2", mission_id: "mission-2", action: "dispatch.start" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(["mission-1", "mission-2"]), false, "u1", T_MAX);
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
      const data: NormRecord[] = [
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT2, generated_chars: 120 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.complete" }),
      ];
      const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", T_MAX);
      expect(faceOf(card).status).toBe(CardStatus.Idle);
      expect(card.liveTokRate).toBeNull();
    });

    it("a running session with fewer than two heartbeats mounts the scope at 0, not no scope", () => {
      const data: NormRecord[] = [
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT2, generated_chars: 40 } }),
      ];
      const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX);
      expect(faceOf(card).status).toBe(CardStatus.Running);
      // One fresh heartbeat: generating, but not enough samples for a rate
      // yet. The scope is up at 0 rather than absent.
      expect(card.liveTokRate).toBe(0);
      expect(card.liveTokState).toBe("generating");
    });

    // The playhead cut (`buildFleetCardBase`'s durable sets, `withLiveReadings`'
    // live overlay): a heartbeat stamped AFTER the playhead must not reach
    // the card. The rate readers cut again inside, so the reading that can
    // tell is the half-open check, which reads the sets' latest heartbeat
    // directly: a silent run at the playhead reads STALLED, and a leaked
    // future heartbeat would make it read "cannot say" instead.
    const iso = (ms: number) => new Date(ms).toISOString();
    const SILENT_FROM = T_MAX - 60_000;
    const silentRun = (): NormRecord[] => [
      rec({ ts: iso(SILENT_FROM - 3_000), machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
      rec({ ts: iso(SILENT_FROM - 2_000), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: SILENT_FROM - 2_000, generated_chars: 40 } }),
      rec({ ts: iso(SILENT_FROM), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: SILENT_FROM, generated_chars: 120 } }),
    ];
    const futureBeat = { sampled_at_ms: T_MAX + 10_000, generated_chars: 4_120 };

    it("reads AS OF the playhead: a durable heartbeat stamped after it does not reach the card", () => {
      const data = [...silentRun(), rec({ ts: iso(T_MAX + 10_000), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: futureBeat })];
      const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX, undefined, [], true, T_MAX);
      expect(card.liveTokState).toBe("stalled");
    });

    it("reads a live sample AS OF the playhead too: one stamped after it does not reach the card", () => {
      const future = liveSampleToRecord({ v: 1, kind: "model", at_ms: T_MAX + 10_000, session_id: "s1", role: "coder", fields: { turn_seq: 1, generated_chars: 4_120 } });
      const live: LiveOverlay = { version: 1, bySession: new Map([["s1", [future!]]]), utility: [] };
      const card = buildFleetCard(silentRun(), new Map(), null, new Set(["s1"]), false, "u1", T_MAX, undefined, [], true, T_MAX, [], live);
      expect(card.liveTokState).toBe("stalled");
    });

    it("is a positive number once a running session has two FRESH heartbeats to derive Δchars/Δms from", () => {
      const data: NormRecord[] = [
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT2, generated_chars: 120 } }),
      ];
      const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX);
      // 80 chars / 2000ms = 40 chars/sec, DEFAULT_CHARS_PER_TOKEN (4) → 10 tok/s.
      expect(card.liveTokRate).toBeCloseTo(10, 5);
      expect(card.liveTokStalled).toBe(false);
    });

    // (#2911) The card's per-session heartbeat reads go through the window's
    // session index, not a whole-window scan. Instrumented by appending a
    // heartbeat AFTER the index is built (a deliberate break of the
    // never-mutated-after-read contract): the index cannot see it, a scan can.
    it("reads a running session's heartbeats through the window's session index", () => {
      const data: NormRecord[] = [
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
      ];
      expect(buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX).liveTokRate).toBe(0);
      data.push(rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT2, generated_chars: 120 } }));
      expect(buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX).liveTokRate).toBe(0);
      // Control: a fresh array gets a fresh index and reads the rate.
      expect(buildFleetCard([...data], new Map(), null, new Set(["s1"]), false, "u1", T_MAX).liveTokRate).toBeCloseTo(10, 5);
    });

    // (#2885) A short turn's lone first heartbeat carries the previous
    // turn's rate forward — marked `liveTokCarried` so the card can dim it.
    it("carries the last measured rate (marked liveTokCarried) into a new turn's lone first heartbeat", () => {
      const t1a = T_MAX - 22_000;
      const t1b = T_MAX - 20_000;
      const t1c = T_MAX - 18_000;
      const data: NormRecord[] = [
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
        // Turn 1 opens at 0 (every turn does — #2886 pass 4 finding 2), then
        // two real-progress intervals before turn 2's lone first heartbeat.
        // The carry must read the SECOND interval, not the 0-opening one.
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: t1a, generated_chars: 0, turn_seq: 1 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: t1b, generated_chars: 800, turn_seq: 1 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: t1c, generated_chars: 1_600, turn_seq: 1 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: T_MAX, generated_chars: 50, turn_seq: 2 } }),
      ];
      const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX);
      expect(card.liveTokState).toBe("generating");
      // Turn 1: 800 chars / 2s = 400 chars/s -> 100 tok/s at the default.
      expect(card.liveTokRate).toBeCloseTo(100, 5);
      expect(card.liveTokCarried).toBe(true);
    });

    it("is NOT carried once a turn has produced its own two fresh heartbeats", () => {
      const card = buildFleetCard(
        [
          rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
          rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
          rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT2, generated_chars: 120 } }),
        ],
        new Map(),
        null,
        new Set(["s1"]),
        false,
        "u1",
        T_MAX,
      );
      expect(card.liveTokCarried).toBe(false);
    });

    it("sums across two concurrently running sessions on the same machine", () => {
      const data: NormRecord[] = [
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT2, generated_chars: 120 } }),
        rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.start" }),
        rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
        rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT2, generated_chars: 200 } }),
      ];
      const card = buildFleetCard(data, new Map(), null, new Set(["s1", "s2"]), false, "u1", T_MAX);
      // s1: 40 chars/sec / 4 = 10 tok/s. s2: 80 chars/sec / 4 = 20 tok/s.
      expect(card.liveTokRate).toBeCloseTo(30, 5);
    });

    // (Playback parity, Change A, finding #3 — 2026-09-24) This used to be
    // "is always null in replay (liveMode=false), even with a running-shaped
    // session" — the OLD divergent behavior the audit's finding #3 named
    // directly ("live: dispatch in flight · 89 tok/s · 1 running; playback:
    // dispatch in flight · 1 specialist" for the SAME instant). A replay
    // caller's presence is empty in practice (there is no presence to read
    // about a past day), and the session's own freshness — its lifecycle's
    // staleness window, not presence — is what makes it read
    // as running, in both modes, so the tok/s scope is a fact about the
    // recorded instant rather than a live-only instrument.
    it("computes a real rate for a running-shaped session in replay too (parity)", () => {
      // Record `ts` (not just the heartbeat payload's `sampled_at_ms`) has
      // to be FRESH as of `T_MAX` too — the lifecycle measures staleness off
      // the record's own `ts`, matching a real flow
      // record where the two are close together. Both defaulted to the
      // fixture's far-past `rec()` default `ts` here would make the
      // session read as an orphan (finding #7's own fix), which is a
      // different case than the one under test.
      const data: NormRecord[] = [
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start", ts: new Date(BEAT1).toISOString() }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", ts: new Date(BEAT1).toISOString(), payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", ts: new Date(BEAT2).toISOString(), payload: { sampled_at_ms: BEAT2, generated_chars: 120 } }),
      ];
      // Empty `liveSet` — a real replay call never has presence to consult.
      const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", T_MAX);
      expect(card.liveTokRate).toBeCloseTo(10, 5);
    });

    // (#2877 dogfood finding, live daemon) A mission observed live stuck
    // `status: "running"` (no terminal record) hours after its last real
    // heartbeat. Before this fix, `aggregateTokenRate` happily reported
    // whatever its LAST two heartbeats measured — a fleet card reading a
    // confident "N tok/s" for a session that stopped producing hours ago.
    it("reads 0, not a stale historical rate, once the session's heartbeats go quiet", () => {
      const data: NormRecord[] = [
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
      const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX);
      expect(card.liveTokStalled).toBe(true);
      expect(card.liveTokRate).toBe(0);
    });

    // (#2886 pass 3, "STALL while disconnected") Same records, same stale
    // gap — the ONLY thing that changed is the page's own connection to the
    // daemon. A false STALL claim from a disconnection must not survive.
    it("reads no state (not stalled) for the SAME stale gap when the page is disconnected", () => {
      const data: NormRecord[] = [
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start", ts: new Date(1_000).toISOString() }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", ts: new Date(1_000).toISOString(), payload: { sampled_at_ms: 1_000, generated_chars: 40 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", ts: new Date(3_000).toISOString(), payload: { sampled_at_ms: 3_000, generated_chars: 120 } }),
      ];
      const connectedCard = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX, undefined, undefined, true);
      expect(connectedCard.liveTokStalled).toBe(true);
      expect(connectedCard.liveTokState).toBe("stalled");
      const disconnectedCard = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX, undefined, undefined, false);
      expect(disconnectedCard.liveTokStalled).toBe(false);
      expect(disconnectedCard.liveTokState).toBeNull();
      // Still mounted (a running session exists) — just no state to claim.
      expect(disconnectedCard.liveTokRate).not.toBeNull();
    });

    // (#2886 pass 4, do-it — fresh-reviewer finding 5, "half-open connection
    // race") Even while `connected` (the header's own status) says `true`,
    // a stall claim needs the daemon to have answered SINCE the point the
    // last heartbeat's own deadline passed. Same fixture as above (last
    // heartbeat at 3,000ms) — `STALL_AFTER_MS` past it is 33,000ms.
    it("downgrades a stall to no-signal via the half-open check even while connected=true", () => {
      const data: NormRecord[] = [
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start", ts: new Date(1_000).toISOString() }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", ts: new Date(1_000).toISOString(), payload: { sampled_at_ms: 1_000, generated_chars: 40 } }),
        rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", ts: new Date(3_000).toISOString(), payload: { sampled_at_ms: 3_000, generated_chars: 120 } }),
      ];
      // Contact confirmed BEFORE the 33,000ms deadline — the half-open gap.
      const staleContact = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX, undefined, undefined, true, 32_999);
      expect(staleContact.liveTokStalled).toBe(false);
      expect(staleContact.liveTokState).toBeNull();
      // Contact confirmed AFTER the deadline — a genuine, trustworthy stall.
      const freshContact = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX, undefined, undefined, true, 33_001);
      expect(freshContact.liveTokStalled).toBe(true);
      expect(freshContact.liveTokState).toBe("stalled");
    });

    it("a mission between model steps (only its run session beating) mounts no scope and claims no state", () => {
      // The launcher beats presence for the mission's run session during a
      // mod wait, a test gate, delivery: no model is involved, so the card
      // must not say "processing prompt" or run a scope at 0.
      const data: NormRecord[] = [
        rec({ machine_uid: "u1", session_id: "m1", action: "run.start", mission_id: "m1" }),
        rec({ machine_uid: "u1", session_id: "e1", action: "dispatch.start", mission_id: "m1" }),
        rec({ machine_uid: "u1", session_id: "e1", action: "dispatch.complete", mission_id: "m1" }),
      ];
      const card = buildFleetCard(data, new Map(), null, new Set(["m1"]), false, "u1", T_MAX);
      expect(card.liveTokRate).toBeNull();
      expect(card.liveTokState ?? null).toBeNull();
    });

    it("(#2881) a pre-#2310 review run's bookend is a run, not a pager execution", () => {
      // Archives are append-only (contract 8): the retired review launcher
      // bookended the WHOLE run with a crew-summary handle, and the daemon
      // serves that record as `run.start`. Paging it read as a second
      // execution labeled `deep+diff-review+probe-4b+probe-qwen38`.
      const data: NormRecord[] = [
        rec({ ts: "2026-08-08T23:59:50.000Z", machine_uid: "u1", session_id: "m1", action: "run.start", handle: "deep+diff-review+probe-4b", mission_id: "m1" }),
        rec({ ts: "2026-08-08T23:59:58.000Z", machine_uid: "u1", session_id: "e1", action: "dispatch.start", handle: "reviewer", mission_id: "m1" }),
        rec({ ts: "2026-08-08T23:59:58.000Z", machine_uid: "u1", session_id: "e1", action: "dispatch.turn.heartbeat", payload: { cumulative_chars: 10 } }),
        rec({ ts: "2026-08-09T00:00:00.000Z", machine_uid: "u1", session_id: "e1", action: "dispatch.turn.heartbeat", payload: { cumulative_chars: 30 } }),
      ];
      const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", T_MAX);
      expect(card.executions.map((e) => e.sessionId)).toEqual(["e1"]);
    });

    it("still works from an OLDER runtime's heartbeat shape (no sampled_at_ms/generated_chars)", () => {
      const data: NormRecord[] = [
        rec({ ts: "2026-08-08T23:59:58.000Z", machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
        rec({ ts: "2026-08-08T23:59:58.000Z", machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { cumulative_chars: 10 } }),
        rec({ ts: "2026-08-09T00:00:00.000Z", machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { cumulative_chars: 30 } }),
      ];
      // T_MAX ("2026-08-09T00:00:00.000Z") matches the second (fallback,
      // whole-second `ts`-derived) heartbeat exactly — fresh, not stalled.
      const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX);
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
    const data: NormRecord[] = [];
    const machineRuns: Run[] = [run({ id: "lab-1", kind: "lab", status: "running", machine: "u1" })];
    const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", T_MAX, null, machineRuns);
    expect(faceOf(card).status).toBe(CardStatus.Running);
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
  // `topLevelRuns` treats it as standalone and has nothing to
  // collapse it into.
  it("(#1923) a lab run in its DISPATCH phase counts ONCE, not once per source", () => {
    const labSession = "darkmux-coding-long-agentic-1756000000000";
    const data: NormRecord[] = [rec({ machine_uid: "u1", session_id: labSession, action: "dispatch.start" })];
    const machineRuns: Run[] = [run({ id: "long-agentic-balanced-1756000000-1", kind: "lab", status: "running", machine: "u1" })];
    const card = buildFleetCard(data, new Map(), null, new Set([labSession]), false, "u1", T_MAX, null, machineRuns);
    expect(card.runsCount).toBe(1);
    expect(faceOf(card).status).toBe(CardStatus.Running);
  });

  // The other side of the same `Math.max`: flow work beyond the lab run's
  // own dispatch must still be counted. Two live flow sessions beside one
  // lab run reads 2 — a merge that clamped to the lab count would pass the
  // test above and be wrong here.
  it("(#1923) flow work beyond the lab run's own dispatch still counts", () => {
    const labSession = "darkmux-coding-long-agentic-1756000000000";
    const data: NormRecord[] = [
      rec({ machine_uid: "u1", session_id: labSession, action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "solo-1", action: "dispatch.start" }),
    ];
    const machineRuns: Run[] = [run({ id: "long-agentic-balanced-1756000000-1", kind: "lab", status: "running", machine: "u1" })];
    const card = buildFleetCard(data, new Map(), null, new Set([labSession, "solo-1"]), false, "u1", T_MAX, null, machineRuns);
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
    const card = buildFleetCard([], new Map(), null, new Set(), false, "u1", T_MAX, null, machineRuns);
    expect(card.runsCount).toBe(2);
  });

  // Only a RUNNING lab run counts — a completed or errored one is history,
  // not current activity, same rule flow presence already applies.
  it("(#1923) a completed lab run does not count as active", () => {
    const data: NormRecord[] = [];
    const machineRuns: Run[] = [run({ id: "lab-1", kind: "lab", status: "complete", machine: "u1" })];
    const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1", T_MAX, null, machineRuns);
    expect(faceOf(card).status).toBe(CardStatus.Idle);
    expect(card.runsCount).toBe(0);
  });

  // A running MISSION/DISPATCH row in `/runs` must NOT be double-counted —
  // that machine's activity is already fully accounted for by flow
  // presence (post-#2060). Only `kind === "lab"` rows are net-new signal.
  it("(#1923) a running mission row in /runs is not double-counted against flow presence", () => {
    const data: NormRecord[] = [rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" })];
    const machineRuns: Run[] = [run({ id: "s1", kind: "dispatch", status: "running", machine: "u1" })];
    const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX, null, machineRuns);
    expect(card.runsCount).toBe(1);
  });

  // (#1855) A rostered-but-never-seen machine must render the SAME "offline"
  // stat a machine that WAS seen and has since gone quiet already uses — the
  // shared indicator vocabulary this project's "no snowflakes" rule asks
  // for, rather than a new "silent"/"unknown" state. `entry.id` stands in
  // for `m` directly, matching how `FleetLens.tsx` calls this for a roster
  // entry with no known uid.
  it("(#1855) a rostered entry with no known identity reads 'offline', not 'idle'", () => {
    const card = buildFleetCard([], new Map(), null, new Set(), /* machAbsent */ true, "studio", T_MAX);
    expect(faceOf(card).status).toBe(CardStatus.Offline);
    expect(card.active).toBe(false);
    expect(card.runsCount).toBe(0);
    // (#2921) The card builder never echoes an unknown `m` back as a title
    // (that echo is how a raw hardware uid became one); `FleetLens` titles a
    // roster-only card with the roster id itself.
    expect(card.name).toBe("unnamed machine");
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
    const uid = "00000000-0000-4000-8000-ABCDEF000011";
    const specs = machineSpecs({
      machine_id: "MacBook-Pro",
      machine_uid: uid,
      cpu_brand: "Apple M5 Max",
      ram_total_bytes: 137438953472,
    });
    const card = buildFleetCard([], new Map(), specs, new Set(), /* machAbsent */ false, uid, T_MAX);
    expect(card.name).toBe("MacBook-Pro");
    expect(card.spec).toBe("Apple M5 Max · 128 GB");
    expect(card.specUnknown).toBeNull();
  });

  // Inverted: an OBSERVED name still wins the title. The specs name is a
  // floor for the gap `nameOf` cannot fill, never an override of live
  // observation (#2030 — a value that cannot be outvoted is the defect, not
  // the fix).
  it("(#2814) a name the window actually observed still outranks the specs name", () => {
    const uid = "00000000-0000-4000-8000-ABCDEF000011";
    const specs = machineSpecs({ machine_id: "MacBook-Pro", machine_uid: uid, cpu_brand: "Apple M5 Max" });
    const data: NormRecord[] = [rec({ machine_uid: uid, machine_id: "MacBook-Pro.local" })];
    const card = buildFleetCard(data, new Map(), specs, new Set(), false, uid, T_MAX);
    expect(card.name).toBe("MacBook-Pro.local");
  });
});

describe("(#1855) the spec line says WHICH kind of unknown", () => {
  const T = 9e15;

  it("a machine that beat WITHOUT specs reads 'not-reported' — it answered and said nothing", () => {
    const live = new Map([["u1", beat({ machine_uid: "u1" })]]);
    const card = buildFleetCard([], live, null, new Set(), false, "u1", T);
    expect(card.spec).toBe("");
    expect(card.specUnknown).toBe("not-reported");
    expect(specUnknownLabel(card.specUnknown!)).toBe("hardware not reported");
  });

  it("a machine nothing has been received from reads 'not-seen'", () => {
    // The rostered-but-silent card: forced absent, no beat, no snapshot entry.
    const card = buildFleetCard([], new Map(), null, new Set(), /* machAbsent */ true, "studio-2", T);
    expect(card.spec).toBe("");
    expect(card.specUnknown).toBe("not-seen");
    expect(specUnknownLabel(card.specUnknown!)).toBe("hardware unknown (nothing received)");
  });

  it("a machine WITH hardware reports no unknown at all", () => {
    // Inverted case 1 — the healthy card must carry no marker of any kind.
    const live = new Map([["u1", beat({ machine_uid: "u1", specs: "Apple M5 Max · 128 GB" })]]);
    const card = buildFleetCard([], live, null, new Set(), false, "u1", T);
    expect(card.spec).toBe("Apple M5 Max · 128 GB");
    expect(card.specUnknown).toBeNull();
  });

  it("THIS machine's own /machine/specs read also clears the unknown", () => {
    // Inverted case 2: the local card resolves its hardware from the specs
    // probe, not from a beat, and must not be pushed into either unknown
    // bucket just because presence is switched off.
    const data = [rec({ machine_uid: "u1", machine_id: "MacBook-Pro", action: "dispatch.start" })];
    const specs = machineSpecs({ machine_id: "MacBook-Pro", cpu_brand: "Apple M5 Max", ram_total_bytes: 137438953472 });
    const card = buildFleetCard(data, new Map(), specs, new Set(), false, "u1", T);
    expect(card.spec).toBe("Apple M5 Max · 128 GB");
    expect(card.specUnknown).toBeNull();
  });

  it("the two labels are different sentences — a regression that collapsed them would be invisible otherwise", () => {
    expect(specUnknownLabel("not-seen")).not.toBe(specUnknownLabel("not-reported"));
  });
});

// (#2881) The pager's default-page pick.
describe("busiestExecution", () => {
  const exec = (overrides: Partial<ExecutionTokenReading> & Pick<ExecutionTokenReading, "sessionId" | "state">): ExecutionTokenReading => ({
    role: "coder",
    tokensPerSec: null,
    carried: false,
    ...overrides,
  });

  it("is null for an empty list", () => {
    expect(busiestExecution([])).toBeNull();
  });

  it("picks the sole entry when there is only one", () => {
    const e = exec({ sessionId: "a", state: "rest" });
    expect(busiestExecution([e])).toBe(e);
  });

  it("picks generating over every quieter state, regardless of array order", () => {
    const resting = exec({ sessionId: "a", state: "rest" });
    const generating = exec({ sessionId: "b", state: "generating", tokensPerSec: 10 });
    const stalled = exec({ sessionId: "c", state: "stalled" });
    expect(busiestExecution([resting, generating, stalled])?.sessionId).toBe("b");
    expect(busiestExecution([stalled, generating, resting])?.sessionId).toBe("b");
  });

  it("ranks the quiet states by the lamps' own priority: rest, then tools, then prompt, then stalled", () => {
    const tools = exec({ sessionId: "a", state: "tools" });
    const rest = exec({ sessionId: "b", state: "rest" });
    const prompt = exec({ sessionId: "c", state: "prompt" });
    const stalled = exec({ sessionId: "d", state: "stalled" });
    expect(busiestExecution([tools, prompt, stalled, rest])?.sessionId).toBe("b");
    expect(busiestExecution([prompt, stalled, tools])?.sessionId).toBe("a");
  });

  it("ranks a real state over no-signal (null), even a quiet one over a stalled no-signal", () => {
    const noSignal = exec({ sessionId: "a", state: null });
    const stalled = exec({ sessionId: "b", state: "stalled" });
    expect(busiestExecution([noSignal, stalled])?.sessionId).toBe("b");
  });

  it("ties within generating go to the HIGHER current rate", () => {
    const slower = exec({ sessionId: "a", state: "generating", tokensPerSec: 10 });
    const faster = exec({ sessionId: "b", state: "generating", tokensPerSec: 40 });
    expect(busiestExecution([slower, faster])?.sessionId).toBe("b");
  });

  it("a final tie (same state, same rate) goes to the LOWER session id — deterministic, not array order", () => {
    const first = exec({ sessionId: "b", state: "generating", tokensPerSec: 10 });
    const second = exec({ sessionId: "a", state: "generating", tokensPerSec: 10 });
    expect(busiestExecution([first, second])?.sessionId).toBe("a");
    expect(busiestExecution([second, first])?.sessionId).toBe("a");
  });
});

// (#2886 pass 5, MUST — fresh-reviewer finding F6) The pager's STICKY
// default page guard — never flaps on a tie or on rate alone.
describe("isStrictlyBusier", () => {
  const exec = (overrides: Partial<ExecutionTokenReading> & Pick<ExecutionTokenReading, "sessionId" | "state">): ExecutionTokenReading => ({
    role: "coder",
    tokensPerSec: null,
    carried: false,
    ...overrides,
  });

  it("is false when both are the SAME state class, even if the rate differs (the flap this exists to stop)", () => {
    const current = exec({ sessionId: "a", state: "generating", tokensPerSec: 10 });
    const candidate = exec({ sessionId: "b", state: "generating", tokensPerSec: 90 });
    expect(isStrictlyBusier(candidate, current)).toBe(false);
  });

  it("is false when the candidate is the SAME execution as current (a tie with itself)", () => {
    const current = exec({ sessionId: "a", state: "rest" });
    expect(isStrictlyBusier(current, current)).toBe(false);
  });

  it("is false when the candidate is a WORSE state class than current", () => {
    const current = exec({ sessionId: "a", state: "generating" });
    const candidate = exec({ sessionId: "b", state: "stalled" });
    expect(isStrictlyBusier(candidate, current)).toBe(false);
  });

  it("is true only when the candidate is a STRICTLY better state class than current", () => {
    const current = exec({ sessionId: "a", state: "rest" });
    const candidate = exec({ sessionId: "b", state: "generating" });
    expect(isStrictlyBusier(candidate, current)).toBe(true);
  });

  it("is false for no-signal (null) vs no-signal — the worst class tied with itself", () => {
    const current = exec({ sessionId: "a", state: null });
    const candidate = exec({ sessionId: "b", state: null });
    expect(isStrictlyBusier(candidate, current)).toBe(false);
  });
});

describe("buildFleetCard: executions and defaultExecutionSessionId (#2881)", () => {
  const BEAT1 = T_MAX - 2000;
  const BEAT2 = T_MAX;

  it("is empty while idle", () => {
    const card = buildFleetCard([], new Map(), null, new Set(), false, "u1", T_MAX);
    expect(card.executions).toEqual([]);
    expect(card.defaultExecutionSessionId).toBeNull();
  });

  it("has exactly one entry for a single running execution, matching the aggregate fields", () => {
    const data: NormRecord[] = [
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start", handle: "darkmux/coder" }),
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT2, generated_chars: 120 } }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1", T_MAX);
    expect(card.executions).toHaveLength(1);
    expect(card.executions[0].sessionId).toBe("s1");
    expect(card.executions[0].role).toBe("coder");
    expect(card.executions[0].tokensPerSec).toBeCloseTo(card.liveTokRate!, 5);
    expect(card.defaultExecutionSessionId).toBe("s1");
  });

  it("sorts by session id (a stable order independent of state) and defaults to the busiest", () => {
    const data: NormRecord[] = [
      // s2 is RESTING.
      rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.start", handle: "darkmux/reviewer" }),
      rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
      rec({ machine_uid: "u1", session_id: "s2", action: "dispatch.rest", ts: new Date(BEAT2).toISOString(), payload: { ms: 15_000 } }),
      // s1 is GENERATING.
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start", handle: "darkmux/coder" }),
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT1, generated_chars: 40 } }),
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { sampled_at_ms: BEAT2, generated_chars: 120 } }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(["s1", "s2"]), false, "u1", T_MAX);
    expect(card.executions.map((e) => e.sessionId)).toEqual(["s1", "s2"]);
    expect(card.executions.find((e) => e.sessionId === "s2")?.state).toBe("rest");
    // Busiest = generating, not array/session-id order.
    expect(card.defaultExecutionSessionId).toBe("s1");
  });
});

// (#2915) The fleet card's utility strip: the machine's utility model, whether
// it is resident, and its live utility job (quiet when none). A radio routing
// job has no session, so it reaches the card only through the machine.
describe("(#2915) buildFleetCard's utility strip", () => {
  const at = (s: number) => new Date(Date.parse("2026-08-08T00:00:00.000Z") + s * 1000).toISOString();
  const tAt = (s: number) => Date.parse(at(s));
  const routeStart = (s: number, uid = "u1") =>
    rec({ ts: at(s), machine_uid: uid, action: "utility.start", category: "telemetry", source: "utility", handle: "radio-router", model: "util-4b", payload: { job: "radio_routing", model: "util-4b", stall_after_ms: 30000 } });
  const routeEnd = (s: number) =>
    rec({ ts: at(s), machine_uid: "u1", action: "telemetry.tokens", category: "telemetry", source: "tokens", handle: "radio-router", payload: { purpose: "utility", call_kind: "single_shot", job: "radio_routing", requested_model: "util-4b", total_tokens: 9 } });
  const card = (data: NormRecord[], t: number, specs: MachineSpecsResponse | null = null) =>
    buildFleetCard(data, new Map(), specs, new Set(), false, "u1", t, specs ? rowFactsFor({ isSelf: true }) : null);

  it("shows the routing job while it runs, and is quiet once its usage record lands", () => {
    expect(card([routeStart(0)], tAt(2)).utility.job).toMatchObject({ job: "radio_routing", visual: "radio", stalled: false });
    expect(card([routeStart(0), routeEnd(1)], tAt(2)).utility.job).toBeNull();
  });

  it("another machine's job is not this card's", () => {
    expect(card([routeStart(0, "u2")], tAt(2)).utility.job).toBeNull();
  });

  it("a routing job with no end past its bound reads stalled", () => {
    expect(card([routeStart(0)], tAt(31)).utility.job?.stalled).toBe(true);
  });

  it("a job this build does not know gets the generic visual, never none", () => {
    const r = routeStart(0);
    (r as unknown as { payload: Record<string, unknown> }).payload.job = "dream_job";
    expect(card([r], tAt(1)).utility.job).toMatchObject({ job: "dream_job", visual: "generic" });
  });

  it("the model and residency are the row's own card's statement, on any machine", () => {
    const utility = utilityReading(true, { id: "util-4b", loaded: true });
    const specs = machineSpecs({ machine_id: "studio", machine_uid: "u1" });
    for (const isSelf of [true, false]) {
      const c = buildFleetCard([], new Map(), specs, new Set(), false, "u1", T_MAX, rowFactsFor({ isSelf, utility }));
      expect(c.utility, `isSelf ${isSelf}`).toMatchObject({ model: "util-4b", residency: UtilityResidency.Resident, job: null });
    }
  });

  it("a card that registers none has no model, whatever its old records name", () => {
    const c = buildFleetCard([routeStart(0), routeEnd(1)], new Map(), null, new Set(), false, "u1", tAt(2), rowFactsFor({ utility: utilityReading(true, null) }));
    expect(c.utility).toMatchObject({ model: null, residency: UtilityResidency.None });
  });

  it("with no card read, residency is unknown and the model comes from its utility records", () => {
    expect(card([routeStart(0), routeEnd(1)], tAt(2)).utility).toMatchObject({ model: "util-4b", residency: UtilityResidency.Unknown });
    expect(card([], T_MAX).utility).toMatchObject({ model: null, residency: UtilityResidency.Unknown, job: null });
  });

  it("a compaction on this machine shows as compacting, ended by its usage record", () => {
    const start = rec({ ts: at(0), machine_uid: "u1", session_id: "s1", action: "utility.start", payload: { job: "compaction", model: "util-4b", serves: "s1", stall_after_ms: 600000 } });
    const end = rec({ ts: at(4), machine_uid: "u1", session_id: "s1", action: "telemetry.tokens", category: "telemetry", source: "tokens", payload: { purpose: "utility", call_kind: "compaction", job: "compaction", total_tokens: 3 } });
    expect(card([start], tAt(2)).utility.job).toMatchObject({ job: "compaction", visual: "compacting" });
    expect(card([start, end], tAt(5)).utility.job).toBeNull();
  });
});

// (#2928) The live channel's overlay on the fleet card.
describe("(#2928) buildFleetCard with the live overlay", () => {
  const T = Date.parse("2026-08-09T00:00:00.000Z");
  const durable: NormRecord[] = [
    rec({ ts: new Date(T - 60_000).toISOString(), machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
    rec({ ts: new Date(T - 2000).toISOString(), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { turn_seq: 1, sampled_at_ms: T - 2000, generated_chars: 40, cumulative_chars: 40 } }),
    rec({ ts: new Date(T).toISOString(), machine_uid: "u1", session_id: "s1", action: "dispatch.turn.heartbeat", payload: { turn_seq: 1, sampled_at_ms: T, generated_chars: 120, cumulative_chars: 120 } }),
  ];
  const liveModel = (at: number, gen: number, vis: number) =>
    JSON.stringify({ v: 1, kind: "model", session_id: "s1", at_ms: at, cadence_ms: 250, fields: { turn_seq: 1, sampled_at_ms: at, generated_chars: gen, cumulative_chars: vis } });
  const selfSpecs = machineSpecs({ machine_id: "studio", machine_uid: "u1", utility_model: { id: "u4b", loaded: true } } as Partial<MachineSpecsResponse> & Pick<MachineSpecsResponse, "machine_id">);

  it("no overlay: exactly the durable reading (playback and every existing caller)", () => {
    const a = buildFleetCard(durable, new Map(), null, new Set(["s1"]), false, "u1", T);
    const b = buildFleetCard(durable, new Map(), null, new Set(["s1"]), false, "u1", T, undefined, [], true, null, [], null);
    expect(b.liveTokRate).toBeCloseTo(10, 5);
    expect(b).toEqual(a);
  });

  it("live samples drive the rate and the THINK reading between durable heartbeats", async () => {
    const { LiveStore } = await import("../../lib/liveChannel");
    const store = new LiveStore();
    store.ingest(liveModel(T + 250, 200, 200), T + 250);
    store.ingest(liveModel(T + 500, 400, 200), T + 500); // reasoning: the text holds
    const card = buildFleetCard(durable, new Map(), null, new Set(["s1"]), false, "u1", T + 500, undefined, [], true, null, [], store.snapshot());
    // 200 chars / 250 ms = 800 chars/s at 4 chars/token = 200 tok/s.
    expect(card.liveTokRate).toBeCloseTo(200, 5);
    expect(card.liveTokState).toBe("generating");
    expect(card.executions[0]?.thinking).toBe(true);
    // The same instant from durable records alone: the last pair (40 -> 120,
    // both visible) reads GEN, not THINK.
    const durableOnly = buildFleetCard(durable, new Map(), null, new Set(["s1"]), false, "u1", T + 500);
    expect(durableOnly.executions[0]?.thinking).toBeUndefined();
  });

  it("a sub-second routing job lights this machine's utility glyph live, and only this machine's", async () => {
    const { LiveStore } = await import("../../lib/liveChannel");
    const { UTILITY_JOB } = await import("../../lib/utilityJobs");
    const store = new LiveStore();
    store.ingest(JSON.stringify({ v: 1, kind: "utility", role: "radio-router", model: "u4b", at_ms: T, cadence_ms: 250, fields: { event: "start", job: UTILITY_JOB.radio_routing, job_id: "r1", stall_after_ms: 30000 } }), T);
    const self = buildFleetCard(durable, new Map(), selfSpecs, new Set(["s1"]), false, "u1", T + 100, rowFactsFor({ isSelf: true }), [], true, null, [], store.snapshot());
    expect(self.utility.job?.visual).toBe("radio");
    const peer = buildFleetCard(durable, new Map(), selfSpecs, new Set(["s1"]), false, "u2", T + 100, rowFactsFor({ uid: "u2" }), [], true, null, [], store.snapshot());
    expect(peer.utility.job).toBeNull();
  });
});


// (#2958) Positive readings at once; negative claims once every source that
// could contradict them has answered.
describe("card availability when the view's liveness says the stream stopped (5.0 R3)", () => {
  const T = Date.parse("2026-08-09T00:00:00.000Z");
  const DAY_AGO = new Date(T - 23 * 3_600_000).toISOString();
  const stale = [{ ts: DAY_AGO, machine_uid: "darkbook", machine_id: "darkbook", session_id: "s1", action: "dispatch.complete" }].map((r) => norm(r));

  it("a peer whose card was read but whose beat stopped is not_streamed, though the window holds a day-old record", () => {
    const row = rowFactsFor({ uid: "darkbook", known: true, standing: "online", liveness: "no_beat" });
    const c = buildFleetCard(stale, new Map(), null, new Set(), false, "darkbook", T, row);
    expect(c.availability).toBe("not_streamed");
    expect(cardFace({ active: false, runsCount: 0, standing: c.standing, availability: c.availability, note: c.note }, false, { flow: true, presence: true, sessions: true, runs: true })).toMatchObject({ status: CardStatus.OnlineNotStreaming, noSignal: true });
  });

  it("an unknown liveness falls back to what the window holds", () => {
    const build = (known: boolean) => buildFleetCard([], new Map(), null, new Set(), false, "darkbook", T, rowFactsFor({ uid: "darkbook", known, liveness: "unknown" })).availability;
    expect(build(true)).toBe("known");
    expect(build(false)).toBe("not_streamed");
  });

  it("a live beat is known", () => {
    expect(buildFleetCard(stale, new Map(), null, new Set(), false, "darkbook", T, rowFactsFor({ uid: "darkbook", known: true, liveness: "live" })).availability).toBe("known");
  });

  it("a powered-off machine nothing was seen from is not_reporting, and the hero does not list it as not streaming", () => {
    const row = rowFactsFor({ uid: "darkbook", known: false, standing: "offline", liveness: "no_beat" });
    const c = buildFleetCard([], new Map(), null, new Set(), false, "darkbook", T, row);
    expect(c.availability).toBe("not_reporting");
    expect(notStreamedNames([c])).toEqual([]);
  });
});
describe("the status line's word and reason follow the availability (5.0 R3)", () => {
  const answered = { flow: true, presence: true, sessions: true, runs: true };
  const face = (availability: "known" | "not_streamed" | "not_reporting", standing: "online" | "offline", note: string | null = null) =>
    cardFace({ active: false, runsCount: 0, standing, availability, note }, false, answered);

  it("not_streamed with the card read says online, and not streaming", () => {
    const f = face("not_streamed", "online");
    expect(STATUS_WORD[f.status]).toBe("online");
    expect(secondLineOf(f.status)).toBe("not streaming");
    expect(statusReason(null, f.status)).toBe("online · not streaming: its flow stream doesn't reach this hub, so its activity can't be shown here.");
  });

  it("not_streamed with the card unread claims no evidence it is up", () => {
    const f = face("not_streamed", "online", "not listening");
    expect(STATUS_WORD[f.status]).toBe("not streaming");
    expect(secondLineOf(f.status)).toBeNull();
    expect(statusReason("not listening", f.status)).toBe("not streaming: not listening");
  });

  it("not_reporting reads offline, with the card's reason when it has one", () => {
    const f = face("not_reporting", "offline");
    expect(STATUS_WORD[f.status]).toBe("offline");
    expect(statusReason("not listening", f.status)).toBe("offline: not listening");
    expect(statusReason(null, f.status)).toBe("offline: its presence beat stopped");
  });

  it("a known, quiet machine has no reason to give", () => {
    expect(statusReason(null, face("known", "online").status)).toBeUndefined();
  });
});
describe("cardFace (#2958)", () => {
  const none = { flow: false, presence: false, sessions: false, runs: false };
  const all = { flow: true, presence: true, sessions: true, runs: true };
  const quiet = { active: false, runsCount: 0, standing: "online" as const, availability: "known" as const, note: null };

  it("a peer whose records never reach this viewer never reads idle, whatever answered (5.0 R3)", () => {
    const silent = { ...quiet, availability: "not_streamed" as const };
    expect(cardFace(silent, false, all)).toMatchObject({ status: CardStatus.OnlineNotStreaming, noSignal: true, tube: "nosignal", countShown: false, utilityQuietKnown: false });
  });

  it("a not-streamed peer still shows work a /runs row proves (a positive reading)", () => {
    const silent = { ...quiet, availability: "not_streamed" as const, active: true, runsCount: 1 };
    expect(cardFace(silent, false, all)).toMatchObject({ status: CardStatus.Running, countShown: true });
  });

  it("nothing answered: an idle card says checking…, with no-signal static and no count", () => {
    expect(cardFace(quiet, false, none)).toMatchObject({ status: CardStatus.Checking, noSignal: true, tube: "nosignal", countShown: false, utilityQuietKnown: false, active: false });
  });

  it("everything answered: idle is a reading", () => {
    expect(cardFace(quiet, false, all)).toMatchObject({ status: CardStatus.Idle, noSignal: false, tube: "idle", countShown: true, utilityQuietKnown: true });
  });

  it("idle waits on EVERY source, one at a time", () => {
    for (const k of ["flow", "presence", "sessions", "runs"] as const) {
      expect(cardFace(quiet, false, { ...all, [k]: false }).status, k).toBe(CardStatus.Checking);
    }
  });

  it("a live reading and 'dispatch in flight' show before anything else answers", () => {
    const f = cardFace({ ...quiet, active: true, runsCount: 2 }, true, { ...none, flow: true });
    expect(f).toMatchObject({ status: CardStatus.Running, active: true, noSignal: false, tube: "reading", countShown: true });
  });

  it("in flight with no model working: the tube's 'no model working' waits for every source", () => {
    expect(cardFace({ ...quiet, active: true, runsCount: 1 }, false, { ...all, sessions: false }).tube).toBe("nosignal");
    expect(cardFace({ ...quiet, active: true, runsCount: 1 }, false, all).tube).toBe("idle");
  });

  it("offline waits on presence and the flow window, not on /runs; it keeps the tube's box, powered off", () => {
    const gone = { active: false, runsCount: 0, standing: "offline" as const, availability: "known" as const, note: null };
    expect(cardFace(gone, false, { ...all, presence: false })).toMatchObject({ status: CardStatus.Checking, tube: "nosignal" });
    expect(cardFace(gone, false, { ...all, flow: false })).toMatchObject({ status: CardStatus.Checking });
    expect(cardFace(gone, false, { flow: true, presence: true, sessions: false, runs: false })).toMatchObject({ status: CardStatus.Offline, tube: "off", noSignal: false, countShown: false });
    expect(cardFace(gone, false, all)).toMatchObject({ status: CardStatus.Offline, tube: "off", countShown: true });
  });

  it("offline wins over a reading: an offline card's tube is powered off", () => {
    const gone = { active: true, runsCount: 1, standing: "offline" as const, availability: "known" as const, note: null };
    expect(cardFace(gone, true, all)).toMatchObject({ status: CardStatus.Offline, active: false, tube: "off" });
    // Before presence has answered, the reading stays: it is a positive one.
    expect(cardFace(gone, true, { ...all, presence: false })).toMatchObject({ status: CardStatus.Running, tube: "reading" });
  });

  it("a quiet utility strip waits only on the flow window", () => {
    expect(cardFace(quiet, false, { ...none, flow: true }).utilityQuietKnown).toBe(true);
    expect(cardFace(quiet, false, { ...all, flow: false }).utilityQuietKnown).toBe(false);
  });
});

describe("buildFleetCard: a machine the fleet view holds", () => {
  it("reads its hardware line and standing from its row, not from presence", () => {
    // No beat, no records: the reported case. Presence says nothing about
    // this peer; the view read its card.
    const row = rowFactsFor({ uid: "studio", known: false, name: "studio", spec: "Apple M1 Max · 32 GB", standing: "online" });
    const card = buildFleetCard([], new Map(), null, new Set(), false, "studio", T_MAX, row);
    expect(card).toMatchObject({ name: "studio", spec: "Apple M1 Max · 32 GB", standing: "online" });
  });

  it("an unreachable peer carries the view's status note and no hardware", () => {
    const row = rowFactsFor({ uid: "studio", known: false, name: "studio", note: "listener off", standing: "offline" });
    const card = buildFleetCard([], new Map(), null, new Set(), false, "studio", T_MAX, row);
    expect(card).toMatchObject({ spec: "", note: "listener off", standing: "offline" });
    expect(faceOf(card).status).toBe(CardStatus.Offline);
  });

  it("a standing the view could not decide says checking…, never idle", () => {
    const row = rowFactsFor({ uid: "studio", known: false, name: "studio", note: "listener unavailable", standing: "unknown" });
    const card = buildFleetCard([], new Map(), null, new Set(), false, "studio", T_MAX, row);
    expect(card.standing).toBe("unknown");
    // Never idle, whatever has answered.
    expect(faceOf(card).status).not.toBe(CardStatus.Idle);
  });

  it("a machine outside the view keeps the flow-derived standing", () => {
    expect(buildFleetCard([], new Map(), null, new Set(), true, "u9", T_MAX).standing).toBe("offline");
    expect(buildFleetCard([], new Map(), null, new Set(), false, "u9", T_MAX).standing).toBe("online");
  });
});

describe("cardFace: an undecided standing is not idle", () => {
  const all = { flow: true, presence: true, sessions: true, runs: true };
  const none = { flow: false, presence: false, sessions: false, runs: false };
  it("says checking… until presence answers, then not streaming; offline only when the standing is offline", () => {
    const base = { active: false, runsCount: 0, availability: "known" as const, note: "not listening" as string | null };
    expect(cardFace({ ...base, standing: "unknown" }, false, none)).toMatchObject({ status: CardStatus.Checking, tube: "nosignal" });
    expect(cardFace({ ...base, standing: "unknown" }, false, all)).toMatchObject({ status: CardStatus.NotStreaming, tube: "nosignal" });
    expect(cardFace({ ...base, note: null, standing: "online" }, false, all)).toMatchObject({ status: CardStatus.Idle, tube: "idle" });
    expect(cardFace({ ...base, standing: "offline" }, false, all)).toMatchObject({ status: CardStatus.Offline, tube: "off" });
  });

  // 5.0 UI packet: a card nothing has read has no count to claim. It said
  // "not streaming" over a confident "0 running" (proven by probe).
  it("an unread card of unknown standing holds the no-reading line, not '0 running'", () => {
    const unread = { active: false, runsCount: 0, availability: "known" as const, note: "not listening", standing: "unknown" as const };
    const f = cardFace(unread, false, all);
    expect(f.status).toBe(CardStatus.NotStreaming);
    expect(f.countShown).toBe(false);
    // Work a /runs row proves is still a positive reading.
    expect(cardFace({ ...unread, active: true, runsCount: 1 }, false, all).countShown).toBe(true);
  });
});

describe("card availability (5.0 R3, #3012)", () => {
  const T = Date.parse("2026-08-09T00:00:00.000Z");
  it("a view row the window holds nothing from is not_streamed; its own row and a flow-only card are known", () => {
    const build = (row: RowFacts | null) => buildFleetCard([], new Map(), null, new Set(), false, "darkbook", T, row).availability;
    expect(build(rowFactsFor({ uid: "darkbook", known: false }))).toBe("not_streamed");
    expect(build(rowFactsFor({ uid: "darkbook", known: true }))).toBe("known");
    expect(build(rowFactsFor({ uid: "darkbook", known: false, isSelf: true }))).toBe("known");
    expect(build(null)).toBe("known");
  });
  it("a seen peer whose card says offline is not_reporting", () => {
    const c = buildFleetCard([], new Map(), null, new Set(), false, "darkbook", T, rowFactsFor({ uid: "darkbook", standing: "offline" }));
    expect(c.availability).toBe("not_reporting");
  });
});

describe("the status lamp's form (rec 1)", () => {
  const none = { flow: false, presence: false, sessions: false, runs: false };
  const all = { flow: true, presence: true, sessions: true, runs: true };
  const quiet = { active: false, runsCount: 0, standing: "online" as const, availability: "known" as const, note: null };

  it("proven work is filled", () => {
    expect(lampOf(cardFace({ ...quiet, active: true, runsCount: 1 }, false, all).status)).toBe(LampForm.Filled);
    expect(lampOf(cardFace({ ...quiet, active: true, runsCount: 2 }, true, all).status)).toBe(LampForm.Filled);
  });
  it("proven quiet is hollow", () => {
    expect(lampOf(cardFace(quiet, false, all).status)).toBe(LampForm.Hollow);
  });
  it("no reading is dashed: checking, not streaming (read or unread card)", () => {
    expect(lampOf(cardFace(quiet, false, none).status)).toBe(LampForm.Dashed);
    expect(lampOf(cardFace({ ...quiet, availability: "not_streamed" as const }, false, all).status)).toBe(LampForm.Dashed);
    expect(lampOf(cardFace({ ...quiet, availability: "not_streamed" as const, note: "not listening" }, false, all).status)).toBe(LampForm.Dashed);
    expect(lampOf(CardStatus.Disconnected)).toBe(LampForm.Dashed);
  });
  it("offline is dim filled", () => {
    expect(lampOf(cardFace({ ...quiet, standing: "offline" as const }, false, all).status)).toBe(LampForm.Off);
  });
  it("offline wins over work, as the status word does", () => {
    expect(lampOf(cardFace({ ...quiet, active: true, standing: "offline" as const }, true, all).status)).toBe(LampForm.Off);
  });
});

describe("what the machine serves, in words (rec 2)", () => {
  it("names the profile count and radio", () => {
    expect(servesLine(3, true)).toBe("serves 3 profiles · radio");
    expect(servesLine(0, true)).toBe("serves radio");
    expect(servesLine(1, false)).toBe("serves 1 profile");
    expect(servesLine(12, false)).toBe("serves 12 profiles");
  });
  it("is empty when the machine serves nothing or does not say", () => {
    expect(servesLine(0, false)).toBe("");
    expect(servesParts(0, false)).toEqual([]);
  });
  it("parts carry the tooltips' subject", () => {
    expect(servesParts(1, true)).toEqual([{ kind: "profiles", text: "1 profile" }, { kind: "radio", text: "radio" }]);
  });
});

describe("the count line with several executions (W3)", () => {
  it("reads the position after the running count", () => {
    expect(executionCountText({ runsCount: 2, executions: 2, position: 1 })).toBe("2 running · 1/2");
    expect(executionCountText({ runsCount: 3, executions: 3, position: 3 })).toBe("3 running · 3/3");
  });
  it("names the executions when a mission folds them into fewer runs", () => {
    expect(executionCountText({ runsCount: 1, executions: 9, position: 5 })).toBe("1 run · 5/9 executions");
  });
  it("a single execution is the plain count", () => {
    expect(executionCountText({ runsCount: 1, executions: 1, position: 1 })).toBe("1 running");
  });
});

/** The rows `src/card_status.rs` reads too (`tests/fixtures/card-status-rows.json`):
 * `darkmux machine list` words a machine's status from the view's own row, and
 * must give the answer this card gives for the same row. */
const __dirname = path.dirname(fileURLToPath(import.meta.url));

/** The input Rust reads (`a_model_is_working` in `src/card_status.rs`): a card the
 * view read, with a loaded model in a busy LM Studio status. */
const BUSY = ["processingPrompt", "generating", "computingEmbedding"];
function aModelIsWorking(row: FleetMachine): boolean {
  return row.card.state === "available" && row.card.card.specs.loaded_models.some((m) => BUSY.some((b) => b.toLowerCase() === m.status.toLowerCase()));
}

describe("the card's status, for the rows the Rust twin also answers", () => {
  interface SharedCase {
    name: string;
    row: FleetMachine;
    expect: { status: string; word: string; second_line: string | null; reason: string | null };
  }
  const cases = JSON.parse(
    readFileSync(path.join(__dirname, "../../../../tests/fixtures/card-status-rows.json"), "utf8"),
  ) as SharedCase[];
  const answered = { flow: true, presence: true, sessions: true, runs: true };

  // The VISIBLE words, from the one shared fixture: the status line's word,
  // the count line's second line and the tooltip. No translation table.
  it.each(cases.map((c) => [c.name, c] as const))("%s", (_name, c) => {
    const standing = rowStanding(c.row);
    // The view-only stand-in for "records reach this viewer": a live beat, or this machine.
    const seen = c.row.is_this_machine || c.row.liveness === "live";
    const card = {
      active: aModelIsWorking(c.row),
      runsCount: 0,
      standing,
      availability: machineAvailability({ self: c.row.is_this_machine, seen, standing }),
      note: outcomeLine(c.row.card),
    };
    const face = cardFace(card, false, answered);
    expect(face.status).toBe(c.expect.status);
    expect(STATUS_WORD[face.status]).toBe(c.expect.word);
    expect(secondLineOf(face.status)).toBe(c.expect.second_line);
    expect(statusReason(card.note, face.status) ?? null).toBe(c.expect.reason);
  });

  it("the shared rows cover every status the console shows", () => {
    expect(new Set(cases.map((c) => c.expect.status))).toEqual(
      new Set(["idle", "running", "online_not_streaming", "not_streaming", "offline"]),
    );
  });
});

describe("shownExecution (#2881, #2886 F6)", () => {
  const ex = (sessionId: string, state: ExecutionTokenReading["state"], tokensPerSec: number | null = null) =>
    ({ sessionId, role: "coder", state, tokensPerSec, carried: false }) as ExecutionTokenReading;
  const execs = [ex("a", "rest"), ex("b", "generating", 40), ex("c", "generating", 10)];

  it("shows the busiest execution when nothing is remembered or pinned", () => {
    expect(shownExecution(execs, undefined, undefined)).toEqual({ selectedIdx: 1, defaultSid: "b" });
  });
  it("keeps the remembered default against a tie, replaces it only when another is strictly busier", () => {
    expect(shownExecution(execs, "c", undefined)).toEqual({ selectedIdx: 2, defaultSid: "c" });
    expect(shownExecution(execs, "a", undefined)).toEqual({ selectedIdx: 1, defaultSid: "b" });
  });
  it("a pick holds while its execution runs, and falls back to the default once it ends", () => {
    expect(shownExecution(execs, "b", "a")).toEqual({ selectedIdx: 0, defaultSid: "b" });
    expect(shownExecution(execs, "b", "gone")).toEqual({ selectedIdx: 1, defaultSid: "b" });
  });
  it("shows nothing when nothing runs", () => {
    expect(shownExecution([], "b", "a")).toEqual({ selectedIdx: -1, defaultSid: null });
  });
});
