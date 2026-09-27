import { ACTION, type NormAction, type NormRecord } from "../lib/ingest";
import { norm } from "./records";

/**
 * (#2926) A real coder execution's first two minutes, as flow records.
 *
 * Reproduced from the operator's 50K pepper-grinder run on 2026-09-26
 * (10:51:01-10:53:00 UTC, `handle: coder`), projected to the fields the
 * live scope reads: each heartbeat's `turn_seq`, `sampled_at_ms`,
 * `generated_chars`, `cumulative_chars`, `phase`/`tool_name` and
 * `prompt_chars`; each turn's `tool_calls_count`/`generation_ms`; each tool's
 * `tool_name`; each turn's billed `completion_tokens`. Everything else
 * (arguments, results, prompts, machine identity) is left out. The values
 * are the real ones, so the shapes #2926 is about are exactly as observed:
 *
 * - every turn opens with a stream-open chunk: `generated_chars` 0 -> 2..3
 *   about 0.3-3 s after the turn's first heartbeat (turn 7: 0 -> 2 in 970 ms,
 *   which read as ~1 tok/s under a lit THINK lamp);
 * - turn 2 then reasons at ~450 chars/s with `cumulative_chars` still 0;
 * - turn 8 writes an `edit` call (10:51:43-10:52:03, count held at 2,234,
 *   then 8,580 when the arguments land), and turn 10 writes a `write` call
 *   from 10:52:30 on, the count held at 402 for the rest of the window.
 */
type Kind = "start" | "beat" | "turn" | "tool" | "tokens";
const ROWS: Array<[string, Kind, Record<string, unknown>]> = [
  ["10:51:01", "start", { prompt_chars: 3192 }],
  ["10:51:01", "beat", { turn_seq: 1, sampled_at_ms: 1790419861847, generated_chars: 0, cumulative_chars: 0, prompt_chars: 16361 }],
  ["10:51:04", "beat", { turn_seq: 1, sampled_at_ms: 1790419864803, generated_chars: 3, cumulative_chars: 0 }],
  ["10:51:06", "beat", { turn_seq: 1, sampled_at_ms: 1790419866764, generated_chars: 681, cumulative_chars: 3 }],
  ["10:51:08", "turn", { turn_seq: 1, tool_calls_count: 5, generation_ms: 6267 }],
  ["10:51:08", "tokens", { turn_seq: 1, completion_tokens: 383 }],
  ["10:51:08", "tool", { tool_name: "read" }],
  ["10:51:08", "tool", { tool_name: "read" }],
  ["10:51:08", "tool", { tool_name: "read" }],
  ["10:51:08", "tool", { tool_name: "read" }],
  ["10:51:09", "tool", { tool_name: "read" }],
  ["10:51:09", "beat", { turn_seq: 2, sampled_at_ms: 1790419868125, generated_chars: 0, cumulative_chars: 0, prompt_chars: 35836 }],
  ["10:51:10", "beat", { turn_seq: 2, sampled_at_ms: 1790419870720, generated_chars: 3, cumulative_chars: 0 }],
  ["10:51:12", "beat", { turn_seq: 2, sampled_at_ms: 1790419872628, generated_chars: 887, cumulative_chars: 0 }],
  ["10:51:14", "beat", { turn_seq: 2, sampled_at_ms: 1790419874766, generated_chars: 1797, cumulative_chars: 0 }],
  ["10:51:17", "beat", { turn_seq: 2, sampled_at_ms: 1790419876861, generated_chars: 2719, cumulative_chars: 0 }],
  ["10:51:19", "beat", { turn_seq: 2, sampled_at_ms: 1790419878976, generated_chars: 3643, cumulative_chars: 0 }],
  ["10:51:19", "turn", { turn_seq: 2, tool_calls_count: 1, generation_ms: 11219 }],
  ["10:51:19", "tokens", { turn_seq: 2, completion_tokens: 952 }],
  ["10:51:19", "tool", { tool_name: "read" }],
  ["10:51:20", "beat", { turn_seq: 3, sampled_at_ms: 1790419879349, generated_chars: 0, cumulative_chars: 0, prompt_chars: 32152 }],
  ["10:51:21", "beat", { turn_seq: 3, sampled_at_ms: 1790419881368, generated_chars: 2, cumulative_chars: 2 }],
  ["10:51:22", "turn", { turn_seq: 3, tool_calls_count: 1, generation_ms: 2474 }],
  ["10:51:22", "tokens", { turn_seq: 3, completion_tokens: 53 }],
  ["10:51:22", "tool", { tool_name: "bash" }],
  ["10:51:22", "beat", { turn_seq: 4, sampled_at_ms: 1790419881883, generated_chars: 0, cumulative_chars: 0, prompt_chars: 32341 }],
  ["10:51:22", "beat", { turn_seq: 4, sampled_at_ms: 1790419882216, generated_chars: 2, cumulative_chars: 2 }],
  ["10:51:23", "turn", { turn_seq: 4, tool_calls_count: 1, generation_ms: 786 }],
  ["10:51:23", "tokens", { turn_seq: 4, completion_tokens: 52 }],
  ["10:51:23", "tool", { tool_name: "read" }],
  ["10:51:23", "beat", { turn_seq: 5, sampled_at_ms: 1790419882674, generated_chars: 0, cumulative_chars: 0, prompt_chars: 31990 }],
  ["10:51:25", "beat", { turn_seq: 5, sampled_at_ms: 1790419884890, generated_chars: 3, cumulative_chars: 0 }],
  ["10:51:25", "turn", { turn_seq: 5, tool_calls_count: 1, generation_ms: 2934 }],
  ["10:51:25", "tokens", { turn_seq: 5, completion_tokens: 71 }],
  ["10:51:25", "tool", { tool_name: "read" }],
  ["10:51:26", "beat", { turn_seq: 6, sampled_at_ms: 1790419885613, generated_chars: 0, cumulative_chars: 0, prompt_chars: 39095 }],
  ["10:51:26", "beat", { turn_seq: 6, sampled_at_ms: 1790419886877, generated_chars: 2, cumulative_chars: 0 }],
  ["10:51:29", "beat", { turn_seq: 6, sampled_at_ms: 1790419888768, generated_chars: 764, cumulative_chars: 762 }],
  ["10:51:31", "beat", { turn_seq: 6, sampled_at_ms: 1790419891068, generated_chars: 1530, cumulative_chars: 1455 }],
  ["10:51:31", "turn", { turn_seq: 6, tool_calls_count: 1, generation_ms: 5470 }],
  ["10:51:31", "tokens", { turn_seq: 6, completion_tokens: 420 }],
  ["10:51:31", "tool", { tool_name: "read" }],
  ["10:51:31", "beat", { turn_seq: 7, sampled_at_ms: 1790419891088, generated_chars: 0, cumulative_chars: 0, prompt_chars: 44682 }],
  ["10:51:32", "beat", { turn_seq: 7, sampled_at_ms: 1790419892058, generated_chars: 2, cumulative_chars: 0 }],
  ["10:51:34", "beat", { turn_seq: 7, sampled_at_ms: 1790419894376, generated_chars: 81, cumulative_chars: 75, phase: "writing_tool_call", tool_name: "edit" }],
  ["10:51:35", "beat", { turn_seq: 7, sampled_at_ms: 1790419894976, generated_chars: 946, cumulative_chars: 75 }],
  ["10:51:35", "turn", { turn_seq: 7, tool_calls_count: 1, generation_ms: 3907 }],
  ["10:51:35", "tokens", { turn_seq: 7, completion_tokens: 273 }],
  ["10:51:35", "tool", { tool_name: "edit" }],
  ["10:51:35", "beat", { turn_seq: 8, sampled_at_ms: 1790419895000, generated_chars: 0, cumulative_chars: 0, prompt_chars: 45708 }],
  ["10:51:36", "beat", { turn_seq: 8, sampled_at_ms: 1790419895577, generated_chars: 3, cumulative_chars: 0 }],
  ["10:51:38", "beat", { turn_seq: 8, sampled_at_ms: 1790419897876, generated_chars: 697, cumulative_chars: 0 }],
  ["10:51:40", "beat", { turn_seq: 8, sampled_at_ms: 1790419900007, generated_chars: 1531, cumulative_chars: 0 }],
  ["10:51:43", "beat", { turn_seq: 8, sampled_at_ms: 1790419902899, generated_chars: 2234, cumulative_chars: 82, phase: "writing_tool_call", tool_name: "edit" }],
  ["10:51:46", "beat", { turn_seq: 8, sampled_at_ms: 1790419905905, generated_chars: 2234, cumulative_chars: 82, phase: "writing_tool_call", tool_name: "edit" }],
  ["10:51:48", "beat", { turn_seq: 8, sampled_at_ms: 1790419907907, generated_chars: 2234, cumulative_chars: 82, phase: "writing_tool_call", tool_name: "edit" }],
  ["10:51:50", "beat", { turn_seq: 8, sampled_at_ms: 1790419910912, generated_chars: 2234, cumulative_chars: 82, phase: "writing_tool_call", tool_name: "edit" }],
  ["10:51:53", "beat", { turn_seq: 8, sampled_at_ms: 1790419912914, generated_chars: 2234, cumulative_chars: 82, phase: "writing_tool_call", tool_name: "edit" }],
  ["10:51:55", "beat", { turn_seq: 8, sampled_at_ms: 1790419915920, generated_chars: 2234, cumulative_chars: 82, phase: "writing_tool_call", tool_name: "edit" }],
  ["10:51:58", "beat", { turn_seq: 8, sampled_at_ms: 1790419917923, generated_chars: 2234, cumulative_chars: 82, phase: "writing_tool_call", tool_name: "edit" }],
  ["10:52:00", "beat", { turn_seq: 8, sampled_at_ms: 1790419920926, generated_chars: 2234, cumulative_chars: 82, phase: "writing_tool_call", tool_name: "edit" }],
  ["10:52:03", "beat", { turn_seq: 8, sampled_at_ms: 1790419922929, generated_chars: 2234, cumulative_chars: 82, phase: "writing_tool_call", tool_name: "edit" }],
  ["10:52:04", "beat", { turn_seq: 8, sampled_at_ms: 1790419924547, generated_chars: 8580, cumulative_chars: 82 }],
  ["10:52:05", "turn", { turn_seq: 8, tool_calls_count: 1, generation_ms: 29563 }],
  ["10:52:05", "tokens", { turn_seq: 8, completion_tokens: 2354 }],
  ["10:52:05", "tool", { tool_name: "edit" }],
  ["10:52:05", "beat", { turn_seq: 9, sampled_at_ms: 1790419924568, generated_chars: 0, cumulative_chars: 0, prompt_chars: 48508 }],
  ["10:52:07", "beat", { turn_seq: 9, sampled_at_ms: 1790419927497, generated_chars: 3, cumulative_chars: 0 }],
  ["10:52:09", "turn", { turn_seq: 9, tool_calls_count: 1, generation_ms: 4423 }],
  ["10:52:09", "tokens", { turn_seq: 9, completion_tokens: 143 }],
  ["10:52:09", "tool", { tool_name: "read" }],
  ["10:52:22", "beat", { turn_seq: 10, sampled_at_ms: 1790419941861, generated_chars: 0, cumulative_chars: 0, prompt_chars: 30287 }],
  ["10:52:28", "beat", { turn_seq: 10, sampled_at_ms: 1790419948199, generated_chars: 3, cumulative_chars: 0 }],
  ["10:52:30", "beat", { turn_seq: 10, sampled_at_ms: 1790419950290, generated_chars: 402, cumulative_chars: 104, phase: "writing_tool_call", tool_name: "write" }],
  ["10:52:32", "beat", { turn_seq: 10, sampled_at_ms: 1790419952293, generated_chars: 402, cumulative_chars: 104, phase: "writing_tool_call", tool_name: "write" }],
  ["10:52:35", "beat", { turn_seq: 10, sampled_at_ms: 1790419955299, generated_chars: 402, cumulative_chars: 104, phase: "writing_tool_call", tool_name: "write" }],
  ["10:52:37", "beat", { turn_seq: 10, sampled_at_ms: 1790419957301, generated_chars: 402, cumulative_chars: 104, phase: "writing_tool_call", tool_name: "write" }],
  ["10:52:40", "beat", { turn_seq: 10, sampled_at_ms: 1790419960308, generated_chars: 402, cumulative_chars: 104, phase: "writing_tool_call", tool_name: "write" }],
  ["10:52:42", "beat", { turn_seq: 10, sampled_at_ms: 1790419962311, generated_chars: 402, cumulative_chars: 104, phase: "writing_tool_call", tool_name: "write" }],
  ["10:52:45", "beat", { turn_seq: 10, sampled_at_ms: 1790419965318, generated_chars: 402, cumulative_chars: 104, phase: "writing_tool_call", tool_name: "write" }],
  ["10:52:47", "beat", { turn_seq: 10, sampled_at_ms: 1790419967320, generated_chars: 402, cumulative_chars: 104, phase: "writing_tool_call", tool_name: "write" }],
  ["10:52:50", "beat", { turn_seq: 10, sampled_at_ms: 1790419970324, generated_chars: 402, cumulative_chars: 104, phase: "writing_tool_call", tool_name: "write" }],
  ["10:52:52", "beat", { turn_seq: 10, sampled_at_ms: 1790419972327, generated_chars: 402, cumulative_chars: 104, phase: "writing_tool_call", tool_name: "write" }],
  ["10:52:55", "beat", { turn_seq: 10, sampled_at_ms: 1790419975329, generated_chars: 402, cumulative_chars: 104, phase: "writing_tool_call", tool_name: "write" }],
  ["10:52:58", "beat", { turn_seq: 10, sampled_at_ms: 1790419978335, generated_chars: 402, cumulative_chars: 104, phase: "writing_tool_call", tool_name: "write" }],
  ["10:53:00", "beat", { turn_seq: 10, sampled_at_ms: 1790419980339, generated_chars: 402, cumulative_chars: 104, phase: "writing_tool_call", tool_name: "write" }],
];

export const PEPPER_SID = "darkmux-coding-refresh-rotation-fixture";
export const PEPPER_MACHINE = "MacBook-Pro";
const KIND_ACTION: Record<Kind, NormAction> = {
  start: ACTION.DispatchStart,
  beat: ACTION.DispatchTurnHeartbeat,
  turn: ACTION.DispatchTurn,
  tool: ACTION.DispatchTool,
  tokens: ACTION.TelemetryTokens,
};

/** Unix ms of a `HH:MM:SS(.sss)` wall time on the run's day (UTC). */
export const pepperAt = (hms: string): number => Date.parse(`2026-09-26T${hms}Z`);

/** The execution's records, oldest first, optionally only turns `<= maxTurn`
 *  or `>= minTurn` (a turn's `dispatch.tool` records follow its turn record,
 *  so they are kept with it). `extra` merges into every record (a fleet test
 *  adds `machine_uid`). */
export function pepperRecords(opts: { minTurn?: number; maxTurn?: number; extra?: Record<string, unknown> } = {}): NormRecord[] {
  const out: NormRecord[] = [];
  let turn = 0;
  for (const [hms, kind, payload] of ROWS) {
    if (typeof payload.turn_seq === "number") turn = payload.turn_seq;
    const keep = kind === "start" || ((opts.minTurn == null || turn >= opts.minTurn) && (opts.maxTurn == null || turn <= opts.maxTurn));
    if (!keep) continue;
    out.push(norm({
      ts: `2026-09-26T${hms}Z`,
      action: KIND_ACTION[kind],
      session_id: PEPPER_SID,
      machine_id: PEPPER_MACHINE,
      handle: "coder",
      ...(kind === "start" ? { source: "crew_dispatch", category: "work", model: "darkmux:qwen3.6-35b-a3b" } : {}),
      payload,
      ...opts.extra,
    }));
  }
  return out;
}
