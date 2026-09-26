import { describe, expect, it } from "vitest";
import { UID_SHAPED, decodeMachineKey, encodeMachineKey, type MachineKeyContext } from "./machineKey";
import { displayNameOf } from "./flow";
import type { FlowRecord, PresenceBeat } from "../types/handwritten";

// FAKE hardware uids — UUID-shaped so the no-uid assertions below have
// something real-looking to catch. Mixed case on purpose.
const UID_A = "0A1B2C3D-4E5F-4071-8293-A4B5C6D7E8F9";
const UID_B = "1b2c3d4e-5f60-4172-9384-b5c6d7e8f9a0";
const UID_C = "2C3D4E5F-6071-4283-A495-C6D7E8F9A0B1";

const rec = (uid: string, ts: string, machine_id?: string): FlowRecord =>
  ({ ts, machine_uid: uid, ...(machine_id ? { machine_id } : {}) }) as FlowRecord;

function ctx(data: FlowRecord[], extra: Partial<MachineKeyContext> = {}): MachineKeyContext {
  return { data, liveMachines: new Map<string, PresenceBeat>(), specs: null, roster: [], ...extra };
}

describe("(#2929) machine keys — what the URL hash carries instead of the hardware uid", () => {
  it("UID_SHAPED matches a uuid in either case, and not a name", () => {
    expect(UID_SHAPED.test(UID_A)).toBe(true);
    expect(UID_SHAPED.test(UID_A.toLowerCase())).toBe(true);
    expect(UID_SHAPED.test("MacBook-Pro")).toBe(false);
    expect(UID_SHAPED.test("unnamed-2")).toBe(false);
  });

  it("a named machine is keyed by its name, and the key resolves back to its uid", () => {
    const c = ctx([rec(UID_A, "2026-09-27T01:00:00Z", "studio")]);
    expect(encodeMachineKey(c, UID_A)).toBe("studio");
    expect(decodeMachineKey(c, "studio")).toEqual({ uid: UID_A, key: "studio", legacy: false });
  });

  it("two unnamed machines get distinct keys matching their labels' ordinals", () => {
    const c = ctx([rec(UID_A, "2026-09-27T01:00:00Z"), rec(UID_B, "2026-09-27T02:00:00Z")]);
    const ka = encodeMachineKey(c, UID_A);
    const kb = encodeMachineKey(c, UID_B);
    expect(ka).toBe("unnamed-1");
    expect(kb).toBe("unnamed-2");
    // The key mirrors what the page shows: "unnamed machine" / "unnamed machine 2".
    expect(displayNameOf(c.data, c.liveMachines, null, UID_B)).toBe("unnamed machine 2");
    expect(decodeMachineKey(c, ka).uid).toBe(UID_A);
    expect(decodeMachineKey(c, kb).uid).toBe(UID_B);
  });

  it("two machines sharing a name get distinct keys, first-seen keeps the bare name", () => {
    const c = ctx([rec(UID_B, "2026-09-27T02:00:00Z", "mac"), rec(UID_A, "2026-09-27T01:00:00Z", "mac")]);
    expect(encodeMachineKey(c, UID_A)).toBe("mac");
    expect(encodeMachineKey(c, UID_B)).toBe("mac~2");
    expect(decodeMachineKey(c, "mac").uid).toBe(UID_A);
    expect(decodeMachineKey(c, "mac~2").uid).toBe(UID_B);
  });

  it("a uid-only machine is keyed by its roster id, or by this daemon's specs name", () => {
    const data = [rec(UID_A, "2026-09-27T01:00:00Z"), rec(UID_B, "2026-09-27T02:00:00Z")];
    const c = ctx(data, {
      roster: [{ id: "studio", machine_uid: UID_A }],
      specs: { machine_id: "laptop", machine_uid: UID_B },
    });
    expect(encodeMachineKey(c, UID_A)).toBe("studio");
    expect(encodeMachineKey(c, UID_B)).toBe("laptop");
    expect(decodeMachineKey(c, "laptop").uid).toBe(UID_B);
  });

  it("a presence-only machine is keyed by its beat's display name", () => {
    const beats = new Map<string, PresenceBeat>([[UID_C, { machine_uid: UID_C, display_name: "mini" } as PresenceBeat]]);
    const c = ctx([], { liveMachines: beats });
    expect(encodeMachineKey(c, UID_C)).toBe("mini");
    expect(decodeMachineKey(c, "mini").uid).toBe(UID_C);
  });

  it("a roster-only card (declared, never seen) is keyed by, and resolves to, its roster id", () => {
    const c = ctx([], { roster: [{ id: "garage-mac" }] });
    expect(encodeMachineKey(c, "garage-mac")).toBe("garage-mac");
    expect(decodeMachineKey(c, "garage-mac")).toEqual({ uid: "garage-mac", key: "garage-mac", legacy: false });
  });

  it("an old link carrying the uid still resolves, in any case, and names the key to rewrite to", () => {
    const c = ctx([rec(UID_A, "2026-09-27T01:00:00Z", "studio"), rec(UID_B, "2026-09-27T02:00:00Z")]);
    expect(decodeMachineKey(c, UID_A)).toEqual({ uid: UID_A, key: "studio", legacy: true });
    expect(decodeMachineKey(c, UID_A.toLowerCase())).toEqual({ uid: UID_A, key: "studio", legacy: true });
    expect(decodeMachineKey(c, UID_B.toUpperCase())).toEqual({ uid: UID_B, key: "unnamed-1", legacy: true });
  });

  it("a key nothing in the window matches resolves to nothing (the caller's not-found state)", () => {
    const c = ctx([rec(UID_A, "2026-09-27T01:00:00Z", "studio")]);
    expect(decodeMachineKey(c, "gone-machine")).toEqual({ uid: null, key: null, legacy: false });
    expect(decodeMachineKey(c, "unnamed-3")).toEqual({ uid: null, key: null, legacy: false });
    expect(decodeMachineKey(c, UID_C)).toEqual({ uid: null, key: null, legacy: false });
  });

  it("never encodes a uid — every machine in a mixed window, and one outside it", () => {
    const beats = new Map<string, PresenceBeat>([[UID_C, { machine_uid: UID_C } as PresenceBeat]]);
    const c = ctx([rec(UID_A, "2026-09-27T01:00:00Z"), rec(UID_B, "2026-09-27T02:00:00Z", "b")], { liveMachines: beats });
    const outside = "3D4E5F60-7182-4394-A5B6-D7E8F9A0B1C2";
    const keys = [UID_A, UID_B, UID_C, outside].map((u) => encodeMachineKey(c, u));
    for (const k of keys) expect(UID_SHAPED.test(k), k).toBe(false);
    expect(new Set(keys.slice(0, 3)).size).toBe(3);
  });
});
