import { describe, expect, it } from "vitest";
import { buildFlowWindow, LIVE_WINDOW_MS, mergeTailRecords } from "./flow";
import { nextFlowWindow, type FlowWindowInputs } from "./flowWindow";
import { ingestRecord, type NormRecord } from "./ingest";

/** A small deterministic PRNG, so a failing case reproduces from its seed. */
function rng(seed: number) {
  let s = seed >>> 0;
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0;
    return s / 2 ** 32;
  };
}

const EDGE = Date.UTC(2026, 10, 10, 12, 0, 0);

/** Records shaped like the live stream's: whole-second times (so ties are
 *  common), a few sessions, turns among them, the odd untimed record, the
 *  odd one older than the window, and exact duplicates (the reconcile's
 *  overlap delivering a line the stream already did). */
function stream(rand: () => number, n: number, startMs: number): NormRecord[] {
  const out: NormRecord[] = [];
  const actions = ["dispatch.start", "dispatch.turn", "dispatch.turn.heartbeat", "dispatch.complete", "telemetry.lms"];
  let t = startMs;
  for (let i = 0; i < n; i++) {
    if (rand() < 0.15 && out.length) {
      out.push({ ...out[Math.floor(rand() * out.length)] });
      continue;
    }
    t += Math.floor(rand() * 3) * 1000;
    const action = actions[Math.floor(rand() * actions.length)];
    const sid = `s${Math.floor(rand() * 4)}`;
    const ts = rand() < 0.03 ? "not a time" : rand() < 0.03 ? new Date(EDGE - LIVE_WINDOW_MS - 5_000).toISOString() : new Date(t).toISOString().replace(/\.\d+Z$/, "Z");
    const raw: Record<string, unknown> = { ts, action, session_id: sid, machine_uid: `u${Math.floor(rand() * 2)}` };
    if (action === "dispatch.turn") raw.payload = { turn_seq: 1 + Math.floor(rand() * 6) };
    if (rand() < 0.3) raw.machine_id = `m${Math.floor(rand() * 2)}`;
    const rec = ingestRecord(raw);
    if (!rec) continue;
    // A real record whose key is the synthetic runtime row this turn makes
    // (no action, source "runtime", the turn's session, machine and time),
    // landing before or after the turn: the full build keeps the real one,
    // so insertion has to as well.
    const twin = action === "dispatch.turn" && rand() < 0.3 ? ingestRecord({ ts, session_id: sid, machine_uid: raw.machine_uid, source: "runtime" }) : null;
    if (twin && rand() < 0.5) out.push(twin, rec);
    else out.push(rec, ...(twin ? [twin] : []));
  }
  return out;
}

// The inputs are compared by identity, as the hook's are: one empty array.
const NONE: NormRecord[] = [];

const full = (i: FlowWindowInputs) => buildFlowWindow([...i.yDay, ...i.yTail], [...i.tDay, ...i.tTail], i.edgeMs);

describe("nextFlowWindow: an appended tail is inserted, and equals a full rebuild", () => {
  for (let seed = 1; seed <= 40; seed++) {
    it(`seed ${seed}: every step of a growing tail matches buildFlowWindow`, () => {
      const rand = rng(seed);
      const yDay = stream(rand, 60, EDGE - 20 * 3600_000);
      const tDay = stream(rand, 80, EDGE - 2 * 3600_000);
      const incoming = stream(rand, 120, EDGE - 3600_000);
      // The tail grows in batches of 1..8, as frames deliver them.
      let tTail: NormRecord[] = [];
      let state = nextFlowWindow(null, { yDay, yTail: NONE, tDay, tTail, edgeMs: EDGE });
      while (tTail.length < incoming.length) {
        const k = 1 + Math.floor(rand() * 8);
        tTail = [...tTail, ...incoming.slice(tTail.length, tTail.length + k)];
        const inputs = { yDay, yTail: NONE, tDay, tTail, edgeMs: EDGE };
        state = nextFlowWindow(state, inputs);
        expect(state.rebuilt, "an append is inserted, not rebuilt").toBe(false);
        expect(state.data).toEqual(full(inputs));
      }
      expect(state.data.length).toBeGreaterThan(0);
    });
  }

  it("an append does not re-sort the window: it reuses the records already placed", () => {
    const rand = rng(7);
    const tDay = stream(rand, 50, EDGE - 3600_000);
    const first = nextFlowWindow(null, { yDay: NONE, yTail: NONE, tDay, tTail: [], edgeMs: EDGE });
    const add = ingestRecord({ ts: new Date(EDGE).toISOString(), action: "telemetry.lms", session_id: "s9" })!;
    const next = nextFlowWindow(first, { yDay: NONE, yTail: NONE, tDay, tTail: [add], edgeMs: EDGE });
    expect(next.rebuilt).toBe(false);
    expect(next.data).toContain(add);
    expect(next.data.length).toBe(first.data.length + 1);
    expect(next.data).not.toBe(first.data);
  });

  it("anything other than an append (a new day fetch, a moved edge, a pruned tail) is a full rebuild", () => {
    const rand = rng(9);
    const tDay = stream(rand, 30, EDGE - 3600_000);
    const tail = stream(rand, 10, EDGE - 600_000);
    const base = nextFlowWindow(null, { yDay: NONE, yTail: NONE, tDay, tTail: tail, edgeMs: EDGE });
    expect(nextFlowWindow(base, { yDay: NONE, yTail: NONE, tDay: [...tDay], tTail: tail, edgeMs: EDGE }).rebuilt).toBe(true);
    expect(nextFlowWindow(base, { yDay: NONE, yTail: NONE, tDay, tTail: tail, edgeMs: EDGE + 60_000 }).rebuilt).toBe(true);
    expect(nextFlowWindow(base, { yDay: NONE, yTail: NONE, tDay, tTail: tail.slice(1), edgeMs: EDGE }).rebuilt).toBe(true);
    // A tail that aged a record out of its front AND grew is not an append,
    // though it is longer.
    const pruned = { yDay: NONE, yTail: NONE, tDay, tTail: [...tail.slice(1), ...stream(rand, 3, EDGE - 60_000)], edgeMs: EDGE };
    const after = nextFlowWindow(base, pruned);
    expect(after.rebuilt).toBe(true);
    expect(after.data).toEqual(full(pruned));
    // The same inputs again (a re-render) are the same window.
    expect(nextFlowWindow(base, { yDay: NONE, yTail: NONE, tDay, tTail: tail, edgeMs: EDGE })).toBe(base);
  });
});

describe("mergeTailRecords: a poll that brings nothing new is no cache change", () => {
  it("returns the held array itself when every incoming record is already held and none aged out", () => {
    const held = stream(rng(3), 20, EDGE - 600_000).filter((r) => r.tMs !== null && r.tMs >= EDGE - LIVE_WINDOW_MS);
    const again = held.map((r) => ({ ...r }));
    expect(mergeTailRecords(held, again, EDGE - LIVE_WINDOW_MS)).toBe(held);
  });
});
