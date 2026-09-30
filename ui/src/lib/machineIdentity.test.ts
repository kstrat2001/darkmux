import { describe, it, expect } from "vitest";
import {
  canonUid,
  displayNameOf,
  findUid,
  isSelfMachine,
  localMachineUid,
  machineLabels,
  machineMatch,
  machineNames,
  machineUids,
  matchesMachine,
  nameKey,
  nameOf,
  ownMachineName,
  sameMachine,
  sameUid,
  UNNAMED_MACHINE,
} from "./machineIdentity";
import { FLEET_UID as U, lower, machineFleet } from "../testing/machineFleet";
import { norm } from "../testing/records";
import type { NormRecord } from "./ingest";
import type { PresenceBeat } from "../types/generated/PresenceBeat";

/**
 * A machine's identity is its `machine_uid`; its `machine_id` is only a label,
 * and one machine really does accumulate several. `machine_id` defaults to the
 * hostname, and macOS reports both `MacBook-Pro` and `MacBook-Pro.local`
 * depending on how the daemon was started, so one stable uid ends up with
 * records under both. Renaming a machine does the same.
 *
 * That broke the "which uid is this daemon" lookup, which compared specs'
 * name against `nameOf`'s single answer: `nameOf` returned the older alias,
 * specs reported the current one, nothing matched, and the fallback handed
 * back the NAME as though it were a uid. Downstream, the local machine
 * classified itself as remote on a fleet-card drill — no residency ledger,
 * and a note telling the operator to go view the machine they were already on.
 *
 * The multi-alias case is the point of these tests; the single-alias cases are
 * here so a fix that simply returned the first uid every time would fail.
 */
describe("machineNames / localMachineUid — identity is the uid, not the label", () => {
  const rec = (uid: string, name: string, ts = "2026-08-13T10:00:00Z") =>
    norm({ ts, machine_uid: uid, machine_id: name });
  const beat = (uid: string, display: string): [string, never] =>
    [uid, { machine_uid: uid, display_name: display, schema_version: "1.19.0", beat_ts_ms: 1 } as never];

  const UID = "00000000-0000-4000-8000-ABCDEF000011";
  const OTHER = "00000000-0000-4000-8000-ABCDEF000006";

  it("collects every alias a uid has used, across records and its presence beat", () => {
    const data = [rec(UID, "MacBook-Pro.local"), rec(UID, "MacBook-Pro"), rec(OTHER, "m1-max-32gb-studio")];
    const live = new Map([beat(UID, "MacBook-Pro")]);
    expect(machineNames(data, live, UID)).toEqual(new Set(["MacBook-Pro.local", "MacBook-Pro"]));
    expect(machineNames(data, live, OTHER)).toEqual(new Set(["m1-max-32gb-studio"]));
  });

  it("finds the uid when specs names an alias that is NOT the one nameOf reports", () => {
    // The point of this test is `localMachineUid`, and it needs `nameOf` and
    // specs to DISAGREE — otherwise it passes without exercising anything.
    //
    // (#2030) `nameOf` now answers with the most RECENT alias rather than the
    // first one listed, so the divergence is built the other way round: the
    // window's newest record says "MacBook-Pro.local" while `/machine/specs`
    // still reports "MacBook-Pro". Simply flipping the expected string would
    // have left both sides agreeing and quietly made this vacuous.
    const data = [rec(UID, "MacBook-Pro", "2026-08-13T09:00:00Z"), rec(UID, "MacBook-Pro.local", "2026-08-13T22:00:00Z")];
    const live = new Map([beat(UID, "MacBook-Pro")]);
    expect(nameOf(data, live, UID)).toBe("MacBook-Pro.local");
    // specs reports the OTHER alias, and identity still resolves.
    expect(localMachineUid(data, live, "MacBook-Pro")).toBe(UID);
    expect(localMachineUid(data, live, "MacBook-Pro.local")).toBe(UID);
  });

  it("still resolves the ordinary single-alias case, and does not match a different machine", () => {
    const data = [rec(UID, "MacBook-Pro"), rec(OTHER, "m1-max-32gb-studio")];
    const live = new Map([beat(UID, "MacBook-Pro"), beat(OTHER, "m1-max-32gb-studio")]);
    expect(localMachineUid(data, live, "MacBook-Pro")).toBe(UID);
    expect(localMachineUid(data, live, "m1-max-32gb-studio")).toBe(OTHER);
  });

  it("falls back to the raw name when no uid has produced a record or beat yet", () => {
    // A freshly booted daemon — the case the `?? machineId` fallback exists for.
    expect(localMachineUid([], new Map(), "MacBook-Pro")).toBe("MacBook-Pro");
    expect(localMachineUid([], new Map(), null)).toBeNull();
  });

  // (#2814) SELF IS NEVER UNKNOWN. Every assertion above resolves this
  // machine's identity through the flow window — the set of names a uid has
  // been OBSERVED under — so all of them inherit the window's lifetime. The
  // machine you are STANDING ON needs no window and no network to identify
  // itself: `/machine/specs` reads its own hardware uid directly.
  it("(#2814) answers with the uid /machine/specs reports, on an empty window with no beats", () => {
    // The state a fresh install, a quiet machine with Redis off, or a rename
    // whose old records have aged out is actually in. Pre-#2814 this returned
    // the NAME as if it were a uid, and every downstream uid comparison was
    // false from there.
    expect(localMachineUid([], new Map(), "MacBook-Pro", UID)).toBe(UID);
  });

  it("(#2814) the reported uid outranks a name that ANOTHER machine's records also carry", () => {
    // The inverted case, and the one that proves the reported uid is doing
    // the work rather than the name lookup happening to agree with it: a
    // different machine's uid holds the alias `MacBook-Pro` in this window,
    // so the name path resolves to OTHER. Self is still UID — `/machine/specs`
    // is the daemon's own probe of its own hardware, not an observation.
    const data = [rec(OTHER, "MacBook-Pro")];
    const live = new Map([beat(OTHER, "MacBook-Pro")]);
    expect(localMachineUid(data, live, "MacBook-Pro")).toBe(OTHER);
    expect(localMachineUid(data, live, "MacBook-Pro", UID)).toBe(UID);
  });

  // (#2814) The display half. `localMachineUid` now answers with a real
  // hardware uid where it used to answer with the machine's own NAME (via
  // `?? machineId`), so every label derived from `nameOf(thatUid)` would
  // start printing a 36-character UUID on an empty window. These two ship
  // together; the first without the second is a regression.
  it("(#2814) displayNameOf names THIS machine from specs when the window knows nothing", () => {
    expect(nameOf([], new Map(), UID)).toBe(UNNAMED_MACHINE); // the gap, stated (#2921: never the uid)
    expect(displayNameOf([], new Map(), { machine_id: "MacBook-Pro", machine_uid: UID }, UID)).toBe("MacBook-Pro");
  });

  it("(#2814) displayNameOf is a FLOOR — an observed name still outranks the specs name", () => {
    const data = [rec(UID, "MacBook-Pro.local")];
    expect(displayNameOf(data, new Map(), { machine_id: "MacBook-Pro", machine_uid: UID }, UID)).toBe("MacBook-Pro.local");
  });

  it("(#2814) displayNameOf does not lend this machine's name to a DIFFERENT uid", () => {
    expect(displayNameOf([], new Map(), { machine_id: "MacBook-Pro", machine_uid: UID }, OTHER)).toBe(UNNAMED_MACHINE);
  });

  it("(#2814) displayNameOf with no specs is exactly nameOf", () => {
    const data = [rec(UID, "MacBook-Pro")];
    expect(displayNameOf(data, new Map(), null, UID)).toBe(nameOf(data, new Map(), UID));
    expect(displayNameOf([], new Map(), null, UID)).toBe(UNNAMED_MACHINE);
  });

  it("(#2814) keeps the name path when specs reports no uid (non-macOS, or an older build)", () => {
    // The probe is macOS-only and best-effort, and a peer built before the
    // field existed answers without it. Absence must degrade to today's
    // behavior, never to "cannot identify myself".
    const data = [rec(UID, "MacBook-Pro")];
    const live = new Map([beat(UID, "MacBook-Pro")]);
    expect(localMachineUid(data, live, "MacBook-Pro", null)).toBe(UID);
    expect(localMachineUid(data, live, "MacBook-Pro", undefined)).toBe(UID);
  });
});

// ── nameOf resolves the CURRENT name, not the first one seen (#2030) ──────
describe("nameOf recency", () => {
  const UID = "00000000-0000-4000-8000-ABCDEF000011";
  const rec = (ts: string, machine_id: string): NormRecord => norm({ ts, machine_id, machine_uid: UID, action: "machine.online" });

  it("a single stale record cannot outvote every later one", () => {
    // The operator's actual case: one stray record naming a different machine
    // landed in their flow directory, and their machine page showed that name
    // from then on. Hundreds of correct records arrived afterwards and none
    // displaced it, because the lookup took the FIRST match in the window.
    const data = [
      rec("2026-08-26T13:42:41Z", "m5-ultra-256gb"), // the stray, listed first
      rec("2026-08-27T09:00:00Z", "MacBook-Pro"),
      rec("2026-08-27T21:19:40Z", "MacBook-Pro"),
    ];
    expect(nameOf(data, new Map(), UID)).toBe("MacBook-Pro");
  });

  it("still tracks a genuine rename, in either listing order", () => {
    // The behaviour the old code was reaching for — a machine really does
    // change names — must survive. Newest wins regardless of array order.
    const older = rec("2026-08-01T00:00:00Z", "MacBook-Pro.local");
    const newer = rec("2026-08-27T00:00:00Z", "MacBook-Pro");
    expect(nameOf([older, newer], new Map(), UID)).toBe("MacBook-Pro");
    expect(nameOf([newer, older], new Map(), UID)).toBe("MacBook-Pro");
  });

  it("falls back to the presence beat, then UNNAMED_MACHINE (never the uid), when no record names it", () => {
    expect(nameOf([], new Map([[UID, { display_name: "studio" } as never]]), UID)).toBe("studio");
    expect(nameOf([], new Map(), UID)).toBe(UNNAMED_MACHINE);
    expect(nameOf([], new Map(), UID)).toBe("unnamed machine");
  });

  it("keeps the first match when timestamps are unusable, rather than picking arbitrarily", () => {
    const a = rec("not-a-date", "first");
    const b = rec("also-bad", "second");
    expect(nameOf([a, b], new Map(), UID)).toBe("first");
  });
});


// (#2921 follow-up) The fallbacks after an observed name: this daemon's own
// specs name, then the roster id declared for the uid, then an ordinal that
// tells two unnamed machines apart without identifying either.
describe("displayNameOf: roster and unnamed ordinals", () => {
  // Fixture uids, not any real machine's.
  const A = "00000000-0000-4000-8000-ABCDEF000001";
  const B = "00000000-0000-4000-8000-ABCDEF000003";
  const C = "00000000-0000-4000-8000-ABCDEF000005";
  const uidOnly = (uid: string, ts: string) => norm({ ts, action: "dispatch.turn", machine_uid: uid });
  const none = new Map<string, PresenceBeat>();

  it("a roster entry declared for the uid names a uid-only machine", () => {
    const data = [uidOnly(A, "2026-09-26T10:00:00Z")];
    expect(displayNameOf(data, none, null, A, [{ id: "studio", machine_uid: A }])).toBe("studio");
    // An entry for a different uid lends nothing.
    expect(displayNameOf(data, none, null, A, [{ id: "studio", machine_uid: B }])).toBe(UNNAMED_MACHINE);
  });

  it("an observed name and this daemon's own name both outrank the roster id", () => {
    const named = [norm({ ts: "2026-09-26T10:00:00Z", machine_uid: A, machine_id: "box" })];
    expect(displayNameOf(named, none, null, A, [{ id: "studio", machine_uid: A }])).toBe("box");
    const self = { machine_id: "laptop", machine_uid: A };
    expect(displayNameOf([uidOnly(A, "2026-09-26T10:00:00Z")], none, self, A, [{ id: "studio", machine_uid: A }])).toBe("laptop");
  });

  it("two unnamed machines get distinct ordinals by first-seen order, whatever the array order", () => {
    const data = [uidOnly(B, "2026-09-26T10:05:00Z"), uidOnly(A, "2026-09-26T10:00:00Z"), uidOnly(B, "2026-09-26T09:59:00Z")];
    // B was seen first (09:59), A second.
    expect(displayNameOf(data, none, null, B)).toBe("unnamed machine");
    expect(displayNameOf(data, none, null, A)).toBe("unnamed machine 2");
    const reversed = [...data].reverse();
    expect(displayNameOf(reversed, none, null, B)).toBe("unnamed machine");
    expect(displayNameOf(reversed, none, null, A)).toBe("unnamed machine 2");
  });

  it("named, self and rostered machines take no ordinal; a presence-only uid comes after every recorded one", () => {
    const data = [
      norm({ ts: "2026-09-26T09:00:00Z", machine_uid: C, machine_id: "box" }),
      uidOnly(A, "2026-09-26T09:10:00Z"),
      uidOnly(B, "2026-09-26T09:20:00Z"),
    ];
    const roster = [{ id: "studio", machine_uid: A }];
    expect(displayNameOf(data, none, null, B, roster)).toBe("unnamed machine");
    const live = new Map([["00000000-0000-4000-8000-ABCDEF000010", {} as never]]);
    expect(displayNameOf(data, live, null, B)).toBe("unnamed machine 2");
    expect(displayNameOf(data, live, null, "00000000-0000-4000-8000-ABCDEF000010")).toBe("unnamed machine 3");
  });
});

describe("one machine identity: the uid, compared case-normalized", () => {
  it("sameUid ignores case and never matches a missing uid", () => {
    expect(sameUid(U.mbp, lower(U.mbp))).toBe(true);
    expect(sameUid(U.mbp, U.studio)).toBe(false);
    expect(sameUid(null, null)).toBe(false);
    expect(sameUid(U.mbp, undefined)).toBe(false);
    expect(canonUid("AbC")).toBe("abc");
  });

  it("findUid answers in the form the list spells the uid", () => {
    expect(findUid([U.mbp, U.studio], lower(U.studio))).toBe(U.studio);
    expect(findUid([U.mbp], U.studio)).toBeNull();
  });

  it("nameKey folds case and the .local suffix, nothing else", () => {
    expect(nameKey("MacBook-Pro.local")).toBe(nameKey("macbook-pro"));
    expect(nameKey("darkbook")).not.toBe(nameKey("darkbook-2"));
  });

  it("sameMachine decides by uid when both carry one, even if the names agree", () => {
    expect(sameMachine({ uid: U.macA, name: "Mac" }, { uid: U.macB, name: "Mac" })).toBe(false);
    expect(sameMachine({ uid: U.mbp, name: "MacBook-Pro" }, { uid: lower(U.mbp), name: "laptop" })).toBe(true);
  });

  it("sameMachine falls back to the name spelling only when a side has no uid", () => {
    expect(sameMachine({ name: "MacBook-Pro.local" }, { uid: U.mbp, name: "macbook-pro" })).toBe(true);
    expect(sameMachine({ name: "darkbook" }, { uid: U.mbp, name: "MacBook-Pro" })).toBe(false);
    expect(sameMachine({}, {})).toBe(false);
  });
});

describe("the window's machines are counted by uid, whatever its case", () => {
  it("machineUids lists a machine once when a beat spells its uid differently", () => {
    const f = machineFleet();
    const live = new Map([[lower(U.studio), f.liveMachines.get(U.studio) as never]]);
    const uids = machineUids(f.data, live);
    expect(uids.filter((u) => sameUid(u, U.studio))).toHaveLength(1);
    expect(uids).toHaveLength(3);
  });

  it("nameOf and machineNames read a beat keyed in the other case", () => {
    const f = machineFleet();
    const live = new Map([[lower(U.darkbook), { machine_uid: lower(U.darkbook), display_name: "Darkbook", schema_version: "1", beat_ts_ms: 1 } as never]]);
    const data = f.data.filter((r) => !sameUid(r.machine_uid, U.darkbook));
    expect(nameOf(data, live, U.darkbook)).toBe("Darkbook");
    expect(machineNames(data, live, U.darkbook)).toEqual(new Set(["Darkbook"]));
  });

  it("the roster names a machine whose uid it spells in the other case", () => {
    const f = machineFleet({ streamsHere: false });
    expect(ownMachineName([], new Map(), null, f.roster, U.darkbook)).toBe("darkbook");
  });
});

describe("self identity is a uid comparison", () => {
  it("isSelfMachine accepts specs' uid in either case", () => {
    const f = machineFleet();
    expect(isSelfMachine(f.data, f.liveMachines, f.specs, U.mbp)).toBe(true);
    expect(isSelfMachine(f.data, f.liveMachines, f.specs, U.studio)).toBe(false);
  });

  it("a peer with the same NAME is not this machine when specs names a uid", () => {
    const f = machineFleet({ twinMacs: true });
    const specs = { machine_id: "Mac", machine_uid: lower(U.macA) };
    expect(isSelfMachine(f.data, f.liveMachines, specs, U.macA)).toBe(true);
    expect(isSelfMachine(f.data, f.liveMachines, specs, U.macB)).toBe(false);
  });

  it("without a specs uid, a name two machines share identifies neither as self", () => {
    const f = machineFleet({ twinMacs: true });
    const specs = { machine_id: "Mac", machine_uid: null };
    expect(isSelfMachine(f.data, f.liveMachines, specs, U.macA)).toBe(false);
    expect(isSelfMachine(f.data, f.liveMachines, specs, U.macB)).toBe(false);
  });

  it("without a specs uid, a name only one machine carries still finds self, .local or not", () => {
    const f = machineFleet();
    const specs = { machine_id: "macbook-pro.local", machine_uid: null };
    expect(isSelfMachine(f.data, f.liveMachines, specs, U.mbp)).toBe(true);
    expect(isSelfMachine(f.data, f.liveMachines, specs, U.studio)).toBe(false);
  });

  it("localMachineUid returns the flow's spelling of the reported uid", () => {
    const f = machineFleet();
    expect(localMachineUid(f.data, f.liveMachines, "MacBook-Pro", lower(U.mbp))).toBe(U.mbp);
  });
});

describe("machineMatch: what one machine answers to", () => {
  it("carries every alias of a machine, including the .local spelling", () => {
    const f = machineFleet();
    const m = machineMatch(f.data, f.liveMachines, f.specs, f.roster, U.mbp);
    expect(matchesMachine({ name: "MacBook-Pro" }, m)).toBe(true);
    expect(matchesMachine({ name: "macbook-pro.local" }, m)).toBe(true);
    expect(matchesMachine({ name: "darkbook" }, m)).toBe(false);
  });

  it("a run that carries a uid is decided by the uid alone", () => {
    const f = machineFleet();
    const m = machineMatch(f.data, f.liveMachines, f.specs, f.roster, U.mbp);
    expect(matchesMachine({ uid: lower(U.mbp), name: "someone-else" }, m)).toBe(true);
    expect(matchesMachine({ uid: U.studio, name: "MacBook-Pro" }, m)).toBe(false);
  });

  it("a name two machines share matches neither, by name", () => {
    const f = machineFleet({ twinMacs: true });
    const a = machineMatch(f.data, f.liveMachines, f.specs, f.roster, U.macA);
    const b = machineMatch(f.data, f.liveMachines, f.specs, f.roster, U.macB);
    expect(matchesMachine({ name: "Mac" }, a)).toBe(false);
    expect(matchesMachine({ name: "Mac" }, b)).toBe(false);
    expect(matchesMachine({ uid: U.macA, name: "Mac" }, a)).toBe(true);
    expect(matchesMachine({ uid: U.macA, name: "Mac" }, b)).toBe(false);
  });

  it("a roster-only peer answers to its roster id and its declared uid", () => {
    const f = machineFleet({ streamsHere: false });
    const m = machineMatch(f.data, f.liveMachines, f.specs, f.roster, "darkbook");
    expect(m.uid).toBe(lower(U.darkbook));
    expect(matchesMachine({ name: "Darkbook" }, m)).toBe(true);
    expect(matchesMachine({ uid: U.darkbook, name: "whatever" }, m)).toBe(true);
    expect(matchesMachine({ name: "m1-max-32gb-studio" }, m)).toBe(false);
  });

  it("this machine answers to the name its own daemon reports", () => {
    const f = machineFleet();
    const data = f.data.filter((r) => r.machine_id !== "MacBook-Pro");
    const m = machineMatch(data, f.liveMachines, f.specs, f.roster, U.mbp);
    expect(matchesMachine({ name: "MacBook-Pro" }, m)).toBe(true);
  });
});

describe("machineLabels tells machines that read alike apart", () => {
  it("numbers the second of two machines named Mac, never showing a uid", () => {
    const f = machineFleet({ twinMacs: true });
    const labels = machineLabels(f.data, f.liveMachines, f.specs, f.roster, [U.macA, U.macB, U.studio]);
    expect(labels.get(U.macA)).toBe("Mac");
    expect(labels.get(U.macB)).toBe("Mac 2");
    expect(labels.get(U.studio)).toBe("m1-max-32gb-studio");
    expect(displayNameOf(f.data, f.liveMachines, f.specs, U.mbp)).toBe("MacBook-Pro.local");
  });
});
