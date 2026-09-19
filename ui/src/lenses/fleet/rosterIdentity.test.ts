import { describe, it, expect } from "vitest";
import { rosterOnlyEntries } from "./cards";
import type { FlowRecord, PresenceBeat } from "../../types/handwritten";
import type { RosterMachineEntry } from "../../types/handwritten";

/**
 * (#2796) ROSTER-vs-IDENTITY CONSOLIDATION, as a fixture rather than as
 * production wreckage.
 *
 * The live fleet currently sits in the good case only by accident. This
 * machine called itself `laptop` until June 2026 and `MacBook-Pro` after,
 * under one unchanging hardware uid, and `darkmux machine add laptop` left a
 * self-entry behind. That entry is suppressed today because May–June flow
 * files are still on disk and carry `machine_id: "laptop"` for that uid, so
 * the ALIAS branch of `rosterOnlyEntries` folds it away.
 *
 * Age those files out and none of the three branches fire — `laptop` and
 * `macbookpro` share no substring for `normalizeMachineAlias`, and the entry
 * has no `machine_uid` for the first branch — and the phantom card returns.
 * That is a state nobody can reproduce on demand in production; it arrives
 * silently, months later, when retention rolls past a rename.
 *
 * So it is fixtured. The rows below are the whole space: which of the three
 * fallbacks is load-bearing, and what happens when each is the only one left.
 */

const UID_A = "F9ACF59C-0E8B-5092-A6B4-7C07070737D2";
const UID_B = "382A2016-41FD-5729-BF22-9C1A91F1BEDD";

function beat(uid: string, name: string): [string, PresenceBeat] {
  return [uid, { machine_uid: uid, display_name: name, ts: Date.now() } as PresenceBeat];
}

function record(uid: string, machineId: string): FlowRecord {
  return {
    ts: "2026-09-19T10:00:00Z",
    machine_uid: uid,
    machine_id: machineId,
    session_id: `s-${machineId}`,
    action: "dispatch.start",
  } as FlowRecord;
}

function entry(id: string, machineUid?: string): RosterMachineEntry {
  return { id, address: "127.0.0.1:8765", added_unix_ms: 1, machine_uid: machineUid } as RosterMachineEntry;
}

describe("a roster entry is folded onto the machine it actually names", () => {
  it("row 1 — the name still matches: folded by the NAME branch", () => {
    const left = rosterOnlyEntries(
      [record(UID_A, "MacBook-Pro")],
      new Map([beat(UID_A, "MacBook-Pro")]),
      [entry("MacBook-Pro", UID_A)],
    );
    expect(left, "the machine's own entry is not a separate machine").toEqual([]);
  });

  it("row 2 — renamed, uid backfilled: folded by the UID branch even with no alias history", () => {
    // The backfill resolved the uid, so this must hold with NO record ever
    // carrying the old name — that is the whole point of #2802.
    const left = rosterOnlyEntries(
      [record(UID_A, "MacBook-Pro")],
      new Map([beat(UID_A, "MacBook-Pro")]),
      [entry("laptop", UID_A)],
    );
    expect(left, "one uid is one machine, whatever either side calls it").toEqual([]);
  });

  it("row 3 — renamed, uid absent, alias history STILL PRESENT: folded by the ALIAS branch", () => {
    // This is the live fleet today, and it is the weakest of the three: it
    // holds only while a record naming the old id survives retention.
    const left = rosterOnlyEntries(
      [record(UID_A, "MacBook-Pro"), record(UID_A, "laptop")],
      new Map([beat(UID_A, "MacBook-Pro")]),
      [entry("laptop")],
    );
    expect(left, "the alias branch carries it while history remains").toEqual([]);
  });

  // `it.fails` — this asserts the test DOES fail, so CI stays green on a known
  // defect AND breaks loudly the moment #2814 fixes it. A skip would go quiet
  // forever; a plain failing test would train everyone to ignore red.
  it.fails("row 4 — renamed, uid absent, alias history AGED OUT: the phantom returns (open, #2814)", () => {
    // THE BAD STATE. Nothing here is corrupt: one machine, one uid, a stale
    // roster row, and a retention window that finally rolled past the rename.
    // All three fallbacks miss, and a machine that does not exist appears in
    // the fleet.
    const left = rosterOnlyEntries(
      [record(UID_A, "MacBook-Pro")],
      new Map([beat(UID_A, "MacBook-Pro")]),
      [entry("laptop")],
    );

    expect(
      left.map((e) => e.id),
      "a uid-less roster row whose name has aged out of history is reported as \
a machine of its own — one physical machine, two cards. Fixtured because it \
cannot be produced on demand in production: it arrives months after a rename, \
when retention rolls past the last record carrying the old name.",
    ).toEqual([]);
  });

  it("row 5 — two genuinely different machines stay two", () => {
    const left = rosterOnlyEntries(
      [record(UID_A, "MacBook-Pro"), record(UID_B, "m1-max-32gb-studio")],
      new Map([beat(UID_A, "MacBook-Pro"), beat(UID_B, "m1-max-32gb-studio")]),
      [entry("MacBook-Pro", UID_A), entry("m1-max-32gb-studio", UID_B)],
    );
    expect(left, "consolidation must not swallow a real peer").toEqual([]);
  });

  it("row 6 — a declared machine with no history at all is still a machine", () => {
    // The inverse failure: folding away an entry for a peer that simply has
    // not beaten yet would hide a machine the operator added on purpose.
    const left = rosterOnlyEntries([], new Map(), [entry("never-seen", "UID-UNKNOWN")]);
    expect(left.map((e) => e.id)).toEqual(["never-seen"]);
  });
});
