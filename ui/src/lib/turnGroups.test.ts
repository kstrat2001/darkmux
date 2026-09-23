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

  it("a turn's output is summed over its calls, not read from its last one", () => {
    // A checkpointed turn takes several calls under one seq; the turn
    // record's usage is only the LAST call's (measured producer behavior).
    // The per-call telemetry.tokens records carry each call's share.
    const calls = [
      r(3, "telemetry.tokens", { turn_seq: 1, prompt_tokens: 6000, completion_tokens: 32000, reasoning_tokens: 31000 }),
      r(9, "telemetry.tokens", { turn_seq: 1, prompt_tokens: 6366, completion_tokens: 392, reasoning_tokens: 126 }),
    ];
    const all = [...ALL, ...calls];
    const t1 = turnItems(VISIBLE, all).find((i) => i.kind === "turn" && i.turn.seq === 1);
    expect(t1 && t1.kind === "turn" && t1.turn).toMatchObject({ outTok: 32392, thinkTok: 31126, inTok: 6366 });
  });

  it("shows every visible row exactly once", () => {
    expect(turnItems(VISIBLE, ALL).map((i) => i.rec)).toHaveLength(VISIBLE.length);
  });

  it("two role executions in one session keep their own turn 1 (keyed per execution, not by bare seq)", () => {
    // (#2863 review) A second `dispatch start` restarts turn_seq at 1; keyed
    // by the bare seq, one turn header overwrote the other and its output
    // summed both executions' calls.
    const e1 = r(0, "dispatch start");
    const t1a = r(5, "dispatch.turn", { turn_seq: 1, finish_reason: "stop", usage: { prompt_tokens: 100 } });
    const k1a = r(5, "telemetry.tokens", { turn_seq: 1, completion_tokens: 10 });
    const e2 = r(20, "dispatch start");
    const t1b = r(25, "dispatch.turn", { turn_seq: 1, finish_reason: "stop", usage: { prompt_tokens: 200 } });
    const k1b = r(25, "telemetry.tokens", { turn_seq: 1, completion_tokens: 30 });
    const all = [e1, t1a, k1a, e2, t1b, k1b];
    const items = turnItems([t1b, e2, t1a, e1], all);
    const heads = items.filter((i) => i.kind === "turn");
    expect(heads.map((h) => (h.kind === "turn" ? h.turn.outTok : null))).toEqual([30, 10]);
    expect(items).toHaveLength(4);
  });

  // (#2863 review, finding 3) A turn that never completed (measured on
  // `crew-dispatch-code-reviewer-1789963273339920-0`: turn 4 left a
  // `dispatch.checkpoint` with its own `turn_seq` but no `dispatch.turn`)
  // used to leave `current` pointed at the PRIOR completed turn, since only
  // `dispatch.turn`/`dispatch.reasoning` advanced it — so the checkpoint's
  // own group rendered headerless and out of time order, and the run's
  // terminal error (which carries no `turn_seq` of its own) was misfiled
  // under the wrong turn.
  it("a turn that never finished still gets a header and its error, in time order", () => {
    const e1 = r(0, "dispatch start");
    const t1 = r(5, "dispatch.turn", { turn_seq: 1, finish_reason: "tool_calls", tool_calls_count: 1, usage: { prompt_tokens: 100 } });
    const t2 = r(10, "dispatch.turn", { turn_seq: 2, finish_reason: "tool_calls", tool_calls_count: 1, usage: { prompt_tokens: 200 } });
    const t3 = r(15, "dispatch.turn", { turn_seq: 3, finish_reason: "tool_calls", tool_calls_count: 1, usage: { prompt_tokens: 300 } });
    const checkpoint4 = r(20, "dispatch.checkpoint", { turn_seq: 4 });
    const err = r(21, "dispatch error", {}); // no turn_seq of its own
    const all = [e1, t1, t2, t3, checkpoint4, err];
    const visible = [err, checkpoint4, t3, t2, t1, e1];
    const items = turnItems(visible, all);
    const labels = items.map((i) => (i.kind === "turn" ? `turn ${i.turn.seq}` : `${i.kind}:${i.rec.action}`));
    // Turn 4's synthesized header leads (it is the newest), the error and
    // checkpoint sit under it, THEN turn 3 — not the other way around.
    expect(labels).toEqual([
      "turn 4",
      "rec:dispatch error",
      "rec:dispatch.checkpoint",
      "turn 3",
      "turn 2",
      "turn 1",
      "rec:dispatch start",
    ]);
    const head4 = items.find((i) => i.kind === "turn" && i.turn.seq === 4);
    expect(head4 && head4.kind === "turn" && head4.turn.why).toBe("did not finish");
    expect(head4 && head4.kind === "turn" && head4.turn.durationMs).toBeNull();
    // (#2863 review round 2, finding 1) The synthesized header must carry
    // an identity distinct from the anchor record it borrows a timestamp
    // from — reusing the anchor's OWN identity is what produced the
    // duplicate-React-key/co-highlight bug (`EventLogColumn.test.tsx`
    // proves the render-level consequence against a real session).
    expect(head4 && head4.kind === "turn" ? head4.id : undefined).toBeTruthy();
    expect(items.some((i) => i.kind !== "turn" && "id" in i)).toBe(false);
  });

  // (#2863 review round 2, finding 2) A checkpoint with no terminal record
  // YET does not mean the turn never will finish — it means the run is
  // still going. "did not finish" is a claim about the PAST (the run
  // ended and this turn has no closing record); "in progress" is the
  // honest word while the execution has no terminal at all.
  it("a checkpoint with no terminal record yet reads as in progress, not did not finish", () => {
    const e1 = r(0, "dispatch start");
    const t1 = r(5, "dispatch.turn", { turn_seq: 1, finish_reason: "tool_calls", tool_calls_count: 1, usage: { prompt_tokens: 100 } });
    const checkpoint2 = r(10, "dispatch.checkpoint", { turn_seq: 2, verdict: "continue" });
    const all = [e1, t1, checkpoint2];
    const items = turnItems([checkpoint2, t1, e1], all);
    const head2 = items.find((i) => i.kind === "turn" && i.turn.seq === 2);
    expect(head2 && head2.kind === "turn" && head2.turn.why).toBe("in progress");
  });

  // (#2863 review round 2, finding 8) A record naming an OLDER turn_seq
  // than the one already reached (a late-arriving per-call token record
  // for turn 3, delivered AFTER turn 4's checkpoint) must not yank
  // `current` backward — an un-seq'd record arriving after it (the
  // terminal error) belongs with the FURTHEST turn reached, not the stale
  // one.
  it("a stale (lower) turn_seq does not pull `current` backward", () => {
    const e1 = r(0, "dispatch start");
    const turn3 = r(5, "dispatch.turn", { turn_seq: 3, finish_reason: "tool_calls", tool_calls_count: 1, usage: { prompt_tokens: 100 } });
    const checkpoint4 = r(7, "dispatch.checkpoint", { turn_seq: 4 });
    const staleTokens = r(7, "telemetry.tokens", { turn_seq: 3, completion_tokens: 5 });
    const err = r(8, "dispatch error", {}); // no turn_seq of its own
    const all = [e1, turn3, checkpoint4, staleTokens, err];
    const items = turnItems([err, staleTokens, checkpoint4, turn3, e1], all);
    const labels = items.map((i) => (i.kind === "turn" ? `turn ${i.turn.seq}` : `${i.kind}:${i.rec.action}`));
    const errIdx = labels.indexOf("rec:dispatch error");
    const turn4Idx = labels.indexOf("turn 4");
    const turn3Idx = labels.indexOf("turn 3");
    expect(turn4Idx).toBeLessThan(turn3Idx);
    expect(errIdx).toBeGreaterThan(turn4Idx);
    expect(errIdx).toBeLessThan(turn3Idx);
  });

  // (#2863 review, finding 3, the missing test the general reviewer found)
  // A second `dispatch start` must reset `current` to null — deleting that
  // reset leaves a record with no `turn_seq` of its own, right after the
  // second start, joining the FIRST execution's last turn instead of
  // starting fresh under the new execution.
  it("a second dispatch start resets which turn an un-seq'd record joins", () => {
    const e1 = r(0, "dispatch start");
    const t1 = r(5, "dispatch.turn", { turn_seq: 1, finish_reason: "stop", usage: { prompt_tokens: 100 } });
    const e2 = r(20, "dispatch start");
    const stray = r(21, "dispatch.tool", { tool_name: "read" }); // no turn_seq, right after the second start
    const all = [e1, t1, e2, stray];
    const items = turnItems([stray, e2, t1, e1], all);
    // `stray` must NOT join execution 1's turn 1 — it belongs to no turn yet
    // (execution 2 hasn't produced one), so it sits in the `null` group
    // alongside `e2`, not folded into turn 1's group.
    const turn1 = items.find((i) => i.kind === "turn" && i.turn.seq === 1)!;
    const turn1Idx = items.indexOf(turn1);
    const strayIdx = items.findIndex((i) => i.kind === "rec" && i.rec === stray);
    expect(strayIdx).toBeLessThan(turn1Idx);
  });

  it("an approximate duration that would be negative is no duration", () => {
    // The turn's first heartbeat stamped AFTER its turn record (whole-second
    // clocks, out-of-order delivery) must not render as a negative time.
    const late = r(12, "dispatch.turn.heartbeat", { turn_seq: 1 });
    const t = r(10, "dispatch.turn", { turn_seq: 1, finish_reason: "stop", usage: {} });
    const [head] = turnItems([t], [r(0, "dispatch start"), t, late]);
    expect(head.kind === "turn" ? head.turn.durationMs : "not a turn").toBeNull();
  });
});
