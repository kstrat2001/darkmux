import { describe, expect, it } from "vitest";
import {
  MACHINE_NOT_FOUND_KEY,
  UID_SHAPED,
  decodeMachineKey,
  encodeMachineKey,
  machineKeyHash,
  machineLabel,
  type MachineKeyContext,
} from "./machineKey";
import type { PresenceBeat } from "../types/handwritten";
import type { NormRecord } from "./ingest";
import { norm } from "../testing/records";

// FAKE hardware uids — UUID-shaped so the no-uid assertions below have
// something uid-shaped to catch. All in the repo's fake form (#2957), with hex
// LETTERS in the tail and mixed case across the set (UID_B lowercase), so
// every case-insensitivity assertion below has a letter to fold.
const UID_A = "00000000-0000-4000-8000-ABCDEF000001";
const UID_B = "00000000-0000-4000-8000-abcdef000002";
const UID_C = "00000000-0000-4000-8000-ABCDEF000004";
const UID_D = "00000000-0000-4000-8000-ABCDEF000007";

const rec = (uid: string, ts: string, machine_id?: string): NormRecord =>
  norm({ ts, machine_uid: uid, ...(machine_id ? { machine_id } : {}) });

function ctx(data: NormRecord[], extra: Partial<MachineKeyContext> = {}): MachineKeyContext {
  return { data, liveMachines: new Map<string, PresenceBeat>(), specs: null, roster: [], ...extra };
}

const h = (uid: string) => machineKeyHash(uid).slice(0, 6);
const NOT_FOUND = { uid: null, key: null, stale: false };

describe("(#2929) machine keys — what the URL hash carries instead of the hardware uid", () => {
  it("UID_SHAPED matches a uuid in either case, and not a key", () => {
    expect(UID_SHAPED.test(UID_A)).toBe(true);
    expect(UID_SHAPED.test(UID_A.toLowerCase())).toBe(true);
    expect(UID_SHAPED.test("MacBook-Pro")).toBe(false);
    expect(UID_SHAPED.test(`unnamed-${h(UID_A)}`)).toBe(false);
  });

  it("the uid hash is short, case-insensitive, distinct per uid, and not a piece of the uid", () => {
    expect(machineKeyHash(UID_A)).toBe(machineKeyHash(UID_A.toLowerCase()));
    expect(machineKeyHash(UID_A)).not.toBe(machineKeyHash(UID_B));
    expect(UID_A.toLowerCase()).not.toContain(h(UID_A));
  });

  it("a uniquely named machine is keyed by its bare name, and resolves back", () => {
    const c = ctx([rec(UID_A, "2026-09-27T01:00:00Z", "studio")]);
    expect(encodeMachineKey(c, UID_A)).toBe("studio");
    expect(decodeMachineKey(c, "studio")).toEqual({ uid: UID_A, key: "studio", stale: false });
  });

  it("an unnamed machine's key carries a hash of its uid, not its ordinal; the label keeps the ordinal", () => {
    const c = ctx([rec(UID_A, "2026-09-27T01:00:00Z"), rec(UID_B, "2026-09-27T02:00:00Z")]);
    expect(encodeMachineKey(c, UID_A)).toBe(`unnamed-${h(UID_A)}`);
    expect(encodeMachineKey(c, UID_B)).toBe(`unnamed-${h(UID_B)}`);
    expect(decodeMachineKey(c, `unnamed-${h(UID_B)}`).uid).toBe(UID_B);
    expect(machineLabel(c, UID_B)).toBe("unnamed machine 2");
  });

  it("machines sharing a name are all disambiguated, and the bare name then opens neither", () => {
    const c = ctx([rec(UID_A, "2026-09-27T01:00:00Z", "mac"), rec(UID_B, "2026-09-27T02:00:00Z", "mac")]);
    expect(encodeMachineKey(c, UID_A)).toBe(`mac_${h(UID_A)}`);
    expect(encodeMachineKey(c, UID_B)).toBe(`mac_${h(UID_B)}`);
    expect(decodeMachineKey(c, `mac_${h(UID_B)}`).uid).toBe(UID_B);
    // The separator survives URLSearchParams unencoded, so a link reads as typed.
    expect(new URLSearchParams({ machine: encodeMachineKey(c, UID_B) }).toString()).toBe(`machine=mac_${h(UID_B)}`);
    expect(decodeMachineKey(c, "mac")).toEqual(NOT_FOUND);
  });

  describe("MUST 1: no two machines ever share a key", () => {
    function assertBijective(c: MachineKeyContext, uids: string[]) {
      const keys = uids.map((u) => encodeMachineKey(c, u));
      expect(new Set(keys).size, keys.join(" ")).toBe(uids.length);
      uids.forEach((u, i) => expect(decodeMachineKey(c, keys[i]).uid, keys[i]).toBe(u));
    }
    it("a machine literally named like another's unnamed key — either order", () => {
      const fake = `unnamed-${h(UID_B)}`;
      assertBijective(ctx([rec(UID_A, "2026-09-27T01:00:00Z", fake), rec(UID_B, "2026-09-27T02:00:00Z")]), [UID_A, UID_B]);
      assertBijective(ctx([rec(UID_B, "2026-09-27T01:00:00Z"), rec(UID_A, "2026-09-27T02:00:00Z", fake)]), [UID_A, UID_B]);
    });
    for (const sep of ["_", "~"]) it(`a machine literally named like another's disambiguated key ("${sep}") — either order`, () => {
      const fake = `mac${sep}${h(UID_B)}`;
      const three = (fakeFirst: boolean) =>
        ctx(
          fakeFirst
            ? [rec(UID_C, "2026-09-27T00:00:00Z", fake), rec(UID_A, "2026-09-27T01:00:00Z", "mac"), rec(UID_B, "2026-09-27T02:00:00Z", "mac")]
            : [rec(UID_A, "2026-09-27T01:00:00Z", "mac"), rec(UID_B, "2026-09-27T02:00:00Z", "mac"), rec(UID_C, "2026-09-27T03:00:00Z", fake)],
        );
      assertBijective(three(true), [UID_A, UID_B, UID_C]);
      assertBijective(three(false), [UID_A, UID_B, UID_C]);
    });
    it("a roster-only id that matches another machine's generated key", () => {
      const c = ctx([rec(UID_B, "2026-09-27T02:00:00Z")], { roster: [{ id: `unnamed-${h(UID_B)}` }] });
      assertBijective(c, [UID_B, `unnamed-${h(UID_B)}`]);
    });
    it("a machine named the not-found marker never takes the marker", () => {
      const c = ctx([rec(UID_A, "2026-09-27T01:00:00Z", MACHINE_NOT_FOUND_KEY)]);
      expect(encodeMachineKey(c, UID_A)).not.toBe(MACHINE_NOT_FOUND_KEY);
      expect(decodeMachineKey(c, encodeMachineKey(c, UID_A)).uid).toBe(UID_A);
      expect(decodeMachineKey(c, MACHINE_NOT_FOUND_KEY)).toEqual(NOT_FOUND);
    });
  });

  describe("C2: a saved key opens the SAME machine or not-found — never another", () => {
    it("the machine gained a name since the key was minted", () => {
      const before = ctx([rec(UID_A, "2026-09-27T01:00:00Z"), rec(UID_B, "2026-09-27T02:00:00Z")]);
      const key = encodeMachineKey(before, UID_B);
      const after = ctx([rec(UID_A, "2026-09-27T01:00:00Z"), rec(UID_B, "2026-09-27T02:00:00Z", "studio")]);
      expect(decodeMachineKey(after, key)).toEqual({ uid: UID_B, key: "studio", stale: true });
    });
    it("an earlier unnamed machine entered the window (or another viewer numbers differently)", () => {
      const key = encodeMachineKey(ctx([rec(UID_B, "2026-09-27T02:00:00Z")]), UID_B);
      const after = ctx([rec(UID_A, "2026-09-27T01:00:00Z"), rec(UID_B, "2026-09-27T02:00:00Z")]);
      expect(decodeMachineKey(after, key).uid).toBe(UID_B);
    });
    it("duplicate-name order flipped (a day rollover)", () => {
      const day1 = ctx([rec(UID_A, "2026-09-27T01:00:00Z", "mac"), rec(UID_B, "2026-09-27T02:00:00Z", "mac")]);
      const kb = encodeMachineKey(day1, UID_B);
      const day2 = ctx([rec(UID_B, "2026-09-28T01:00:00Z", "mac"), rec(UID_A, "2026-09-28T02:00:00Z", "mac")]);
      expect(decodeMachineKey(day2, kb).uid).toBe(UID_B);
    });
    it("a disambiguated machine whose twin left: the key still opens it", () => {
      const day1 = ctx([rec(UID_A, "2026-09-27T01:00:00Z", "mac"), rec(UID_B, "2026-09-27T02:00:00Z", "mac")]);
      const kb = encodeMachineKey(day1, UID_B);
      expect(decodeMachineKey(ctx([rec(UID_B, "2026-09-28T01:00:00Z", "mac")]), kb)).toEqual({ uid: UID_B, key: "mac", stale: true });
    });
    it("the machine left the window: not-found, not the machine that took its place", () => {
      const ka = encodeMachineKey(ctx([rec(UID_A, "2026-09-27T01:00:00Z"), rec(UID_B, "2026-09-27T02:00:00Z")]), UID_A);
      const after = ctx([rec(UID_B, "2026-09-27T02:00:00Z"), rec(UID_C, "2026-09-27T03:00:00Z")]);
      expect(decodeMachineKey(after, ka)).toEqual(NOT_FOUND);
    });
    // Two FAKE uids whose hashes share their first 6 hex digits (found by
    // search): the page lengthens the hash to tell them apart, and a 6-hex
    // key minted while only one was known no longer names exactly one.
    const COLL_1 = "00000000-0000-4000-8000-ABCDEF002282";
    const COLL_2 = "00000000-0000-4000-8000-ABCDEF002416";
    it("two machines whose short hashes collide get distinct, longer keys", () => {
      expect(h(COLL_1)).toBe(h(COLL_2));
      const c = ctx([rec(COLL_1, "2026-09-27T01:00:00Z"), rec(COLL_2, "2026-09-27T02:00:00Z")]);
      const [k1, k2] = [encodeMachineKey(c, COLL_1), encodeMachineKey(c, COLL_2)];
      expect(k1).not.toBe(k2);
      expect(decodeMachineKey(c, k1).uid).toBe(COLL_1);
      expect(decodeMachineKey(c, k2).uid).toBe(COLL_2);
    });
    it("a short key that now matches two machines' hashes: not-found, not the first match", () => {
      const k = encodeMachineKey(ctx([rec(COLL_1, "2026-09-27T01:00:00Z")]), COLL_1);
      expect(k).toBe(`unnamed-${h(COLL_1)}`);
      const after = ctx([rec(COLL_2, "2026-09-27T00:00:00Z"), rec(COLL_1, "2026-09-27T01:00:00Z")]);
      expect(decodeMachineKey(after, k)).toEqual(NOT_FOUND);
    });
    it("a uniquely-named key whose name another machine now shares: not-found", () => {
      const ka = encodeMachineKey(ctx([rec(UID_A, "2026-09-27T01:00:00Z", "mac")]), UID_A);
      const after = ctx([rec(UID_A, "2026-09-27T01:00:00Z", "mac"), rec(UID_D, "2026-09-27T00:00:00Z", "mac")]);
      expect(decodeMachineKey(after, ka)).toEqual(NOT_FOUND);
    });
  });

  it("an already-shared `~` key (the earlier separator) still opens its machine, and is rewritten to the `_` form", () => {
    const c = ctx([rec(UID_A, "2026-09-27T01:00:00Z", "mac"), rec(UID_B, "2026-09-27T02:00:00Z", "mac")]);
    expect(decodeMachineKey(c, `mac~${h(UID_B)}`)).toEqual({ uid: UID_B, key: `mac_${h(UID_B)}`, stale: true });
  });

  it("a presence-only machine is keyed by its beat's display name", () => {
    const beats = new Map<string, PresenceBeat>([[UID_C, { machine_uid: UID_C, display_name: "mini" } as PresenceBeat]]);
    const c = ctx([], { liveMachines: beats });
    expect(encodeMachineKey(c, UID_C)).toBe("mini");
    expect(decodeMachineKey(c, "mini").uid).toBe(UID_C);
  });

  it("a uid-only machine is keyed by its roster id, or by this daemon's specs name", () => {
    const c = ctx([rec(UID_A, "2026-09-27T01:00:00Z"), rec(UID_B, "2026-09-27T02:00:00Z")], {
      roster: [{ id: "studio", machine_uid: UID_A }],
      specs: { machine_id: "laptop", machine_uid: UID_B },
    });
    expect(encodeMachineKey(c, UID_A)).toBe("studio");
    expect(encodeMachineKey(c, UID_B)).toBe("laptop");
  });

  it("C3: a roster-only card is keyed by, resolves to, and is labeled with its roster id", () => {
    const c = ctx([], { roster: [{ id: "garage-mac" }] });
    expect(encodeMachineKey(c, "garage-mac")).toBe("garage-mac");
    expect(decodeMachineKey(c, "garage-mac")).toEqual({ uid: "garage-mac", key: "garage-mac", stale: false });
    expect(machineLabel(c, "garage-mac")).toBe("garage-mac");
  });

  it("an old uid link resolves, in any case, and names the key to rewrite to", () => {
    const c = ctx([rec(UID_A, "2026-09-27T01:00:00Z", "studio"), rec(UID_B, "2026-09-27T02:00:00Z")]);
    expect(decodeMachineKey(c, UID_A)).toEqual({ uid: UID_A, key: "studio", stale: true });
    expect(decodeMachineKey(c, UID_A.toLowerCase())).toEqual({ uid: UID_A, key: "studio", stale: true });
    expect(decodeMachineKey(c, UID_B.toUpperCase())).toEqual({ uid: UID_B, key: `unnamed-${h(UID_B)}`, stale: true });
  });

  it("C4: an old uid link to a roster-declared machine never seen opens its roster card", () => {
    const c = ctx([], { roster: [{ id: "garage-mac", machine_uid: UID_D }] });
    expect(decodeMachineKey(c, UID_D.toLowerCase())).toEqual({ uid: "garage-mac", key: "garage-mac", stale: true });
  });

  it("keys nothing matches resolve to nothing (the caller's not-found state)", () => {
    const c = ctx([rec(UID_A, "2026-09-27T01:00:00Z", "studio")]);
    expect(decodeMachineKey(c, "gone-machine")).toEqual(NOT_FOUND);
    expect(decodeMachineKey(c, "unnamed-3")).toEqual(NOT_FOUND);
    expect(decodeMachineKey(c, `unnamed-${h(UID_C)}`)).toEqual(NOT_FOUND);
    expect(decodeMachineKey(c, UID_C)).toEqual(NOT_FOUND);
    expect(decodeMachineKey(c, MACHINE_NOT_FOUND_KEY)).toEqual(NOT_FOUND);
  });

  it("never encodes a uid — every machine in a mixed window, and one outside it", () => {
    const beats = new Map<string, PresenceBeat>([[UID_C, { machine_uid: UID_C } as PresenceBeat]]);
    const c = ctx([rec(UID_A, "2026-09-27T01:00:00Z"), rec(UID_B, "2026-09-27T02:00:00Z", "b")], { liveMachines: beats });
    const keys = [UID_A, UID_B, UID_C, UID_D].map((u) => encodeMachineKey(c, u));
    for (const k of keys) expect(UID_SHAPED.test(k), k).toBe(false);
    expect(new Set(keys).size).toBe(4);
  });
});
