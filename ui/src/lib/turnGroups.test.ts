import { describe, expect, it } from "vitest";
import { groupsByTurn, turnItems } from "./turnGroups";
import type { FlowRecord } from "../types/handwritten";

/** One session's records in the order the host emits them (measured on
 * `refresh-rotation-deep-1790125783-1`): reasoning(N), turn(N), N's tools,
 * a rest; heartbeats stream during the turn; a context reading per turn. */
const S = "darkmux-coding-refresh-rotation-1790125784225";
const at = (sec: number) => new Date(Date.UTC(2026, 8, 23, 1, 9, 0) + sec * 1000).toISOString();
const r = (sec: number, action: string, payload: Record<string, unknown> = {}, session: string | undefined = S): FlowRecord =>
  ({ ts: at(sec), action, session_id: session, machine_id: "MacBook-Pro", payload }) as unknown as FlowRecord;

const start = r(0, "dispatch start");
const beat1 = r(1, "dispatch.turn.heartbeat", { turn_seq: 1 });
const think1 = r(8, "dispatch.reasoning", { turn_seq: 1, reasoning_text: "Let me read the files." });
const turn1 = r(9, "dispatch.turn", {
  turn_seq: 1,
  finish_reason: "tool_calls",
  tool_calls_count: 1,
  usage: { prompt_tokens: 6366, completion_tokens: 392, reasoning_tokens: 126 },
});
const ctx1 = r(9, "telemetry.context", { used: 6366, max: 262144, threshold: 131072 });
const tool1 = r(10, "dispatch.tool", { tool_name: "read", args: '{"path":"/workspace/a.js"}' });
const rest1 = r(11, "dispatch.rest", { ms: 15000, reason: "thermal-duty-cycle", state: "fair" });
const beat2 = r(27, "dispatch.turn.heartbeat", { turn_seq: 2 });
const turn2 = r(41, "dispatch.turn", {
  turn_seq: 2,
  finish_reason: "stop",
  tool_calls_count: 0,
  generation_ms: 14217,
  usage: { prompt_tokens: 18926, completion_tokens: 1110, reasoning_tokens: 378 },
});

const ALL = [start, beat1, think1, turn1, ctx1, tool1, rest1, beat2, turn2];
// What the list shows by default: newest first, heartbeats and context hidden.
const VISIBLE = [turn2, rest1, tool1, turn1, think1, start];

describe("turnItems (#2863)", () => {
  it("puts each turn's events under it, newest turn first, with the rest between turns", () => {
    const items = turnItems(VISIBLE, ALL);
    expect(items.map((i) => (i.kind === "turn" ? `turn ${i.turn.seq}` : `${i.kind}:${i.rec.action}`))).toEqual([
      "turn 2",
      "rest:dispatch.rest",
      "turn 1",
      "rec:dispatch.tool",
      "rec:dispatch.reasoning",
      "rec:dispatch start",
    ]);
  });

  it("a turn header carries in, out, thinking, the window and the compaction threshold", () => {
    const t1 = turnItems(VISIBLE, ALL).find((i) => i.kind === "turn" && i.turn.seq === 1);
    expect(t1 && t1.kind === "turn" && t1.turn).toMatchObject({
      why: "1 tool",
      inTok: 6366,
      outTok: 392,
      thinkTok: 126,
      window: 262144,
      threshold: 131072,
    });
  });

  it("the final turn reads as the answer", () => {
    const t2 = turnItems(VISIBLE, ALL)[0];
    expect(t2.kind === "turn" && t2.turn.why).toBe("answered");
  });

  it("duration is exact when the host recorded it, approximate from timestamps otherwise", () => {
    const items = turnItems(VISIBLE, ALL);
    const t2 = items.find((i) => i.kind === "turn" && i.turn.seq === 2);
    const t1 = items.find((i) => i.kind === "turn" && i.turn.seq === 1);
    expect(t2 && t2.kind === "turn" && t2.turn).toMatchObject({ durationMs: 14217, approx: false });
    // First heartbeat at 1 s, turn record at 9 s: whole-second timestamps.
    expect(t1 && t1.kind === "turn" && t1.turn).toMatchObject({ durationMs: 8000, approx: true });
  });

  it("a hidden record (a heartbeat, a context reading) still informs the header", () => {
    // Neither is in VISIBLE; the window and the approximate duration came from them.
    const t1 = turnItems(VISIBLE, ALL).find((i) => i.kind === "turn" && i.turn.seq === 1);
    expect(t1 && t1.kind === "turn" && t1.turn.window).toBe(262144);
    expect(t1 && t1.kind === "turn" && t1.turn.durationMs).not.toBeNull();
  });

  it("stays flat for a list mixing sessions, where a turn number means nothing", () => {
    const other = r(50, "dispatch.turn", { turn_seq: 1 }, "some-other-session");
    const all = [...ALL, other];
    expect(groupsByTurn(all)).toBe(false);
    expect(turnItems([other, ...VISIBLE], all).every((i) => i.kind === "rec")).toBe(true);
  });

  it("stays flat for one session with no turns (a mission's step records)", () => {
    const steps = [r(0, "step start"), r(5, "step complete")];
    expect(groupsByTurn(steps)).toBe(false);
  });

  it("records with no session (telemetry) do not make a one-session list mixed", () => {
    // Built without the helper: its default parameter would put the session
    // back and make this test vacuous (it was, until a mutation showed it).
    const telem = { ts: at(20), action: "machine.telemetry", machine_id: "MacBook-Pro", payload: {} } as unknown as FlowRecord;
    expect(telem.session_id).toBeUndefined();
    expect(groupsByTurn([...ALL, telem])).toBe(true);
  });

  it("a record that names its own turn joins that turn, even before the turn's record arrives", () => {
    // Heartbeats for turn 2 stream BEFORE turn 2's record, while turn 1 is
    // still the latest turn seen. Shown (the filter can include them), they
    // belong under turn 2.
    const items = turnItems([turn2, beat2, rest1, tool1, turn1, think1, start], ALL);
    const seq = items.map((i) => (i.kind === "turn" ? `turn ${i.turn.seq}` : i.rec.action));
    expect(seq.indexOf("dispatch.turn.heartbeat")).toBeLessThan(seq.indexOf("turn 1"));
    expect(seq.indexOf("dispatch.turn.heartbeat")).toBeGreaterThan(seq.indexOf("turn 2"));
  });

  it("shows every visible row exactly once", () => {
    expect(turnItems(VISIBLE, ALL).map((i) => i.rec)).toHaveLength(VISIBLE.length);
  });
});
