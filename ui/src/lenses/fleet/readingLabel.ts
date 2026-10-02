/**
 * What a card's status line says while an execution it shows is running: the
 * reading takes the status word's place. One named function over the
 * execution's own fields, so the five cases are tested without a DOM.
 */

import { fmtN } from "../../lib/format";
import { liveStateLabel, reasonForLine } from "../../lib/tokenRate";
import type { ExecutionTokenReading, LiveState } from "../../lib/tokenRate";

/** The line's text. `rest` carries both forms of the reason, because the card
 *  shows one by the viewport's width (the line keeps its one-line height). */
export type ReadingLabel = { kind: "text"; text: string } | { kind: "rest"; full: string; word: string };

/** The reading for an execution in `state` (never the disconnection downgrade,
 *  where the line says `disconnected` instead). */
export function readingLabel(exec: ExecutionTokenReading, state: LiveState): ReadingLabel {
  if (state === "generating") return { kind: "text", text: generatingText(exec) };
  // (#2890) "processing ~36k", not "processing prompt · ~36k": the long form
  // ellipsized the size away on a phone.
  if (state === "prompt" && exec.promptLabel) return { kind: "text", text: `processing ${exec.promptLabel}` };
  // (#2950) Why it rests, in place of "rest Ns": the tube's center counts the seconds.
  if (state === "rest" && exec.restReason) return { kind: "rest", full: reasonForLine(exec.restReason), word: exec.restReasonWord ?? exec.restReason };
  return {
    kind: "text",
    text: liveStateLabel({
      state,
      restSecondsLeft: exec.restSecondsLeft,
      toolName: exec.toolName,
      writing: exec.writing,
      writingSeconds: exec.writingSeconds,
      compacting: exec.compacting,
      compactingSeconds: exec.compactingSeconds,
    }),
  };
}

/** A GEN lamp with no reading yet is "not yet measured", never a confident
 *  "0 tok/s" (#2886). Thinking keeps its word while unmeasured (#2911). */
function generatingText(exec: ExecutionTokenReading): string {
  const unit = exec.thinking ? "think tok/s" : "tok/s";
  return exec.tokensPerSec != null ? `${fmtN(Math.round(exec.tokensPerSec))} ${unit}` : `— ${unit}`;
}

/** The tooltip on a reading: the whole reason or size, should a narrow card ellipsize it. */
export function readingTitle(exec: ExecutionTokenReading, state: LiveState): string | undefined {
  if (state === "prompt" && exec.promptLabel) return `estimated prompt size: ${exec.promptLabel} tokens`;
  if (state === "rest" && exec.restReason) return `resting: ${exec.restReason}`;
  return undefined;
}
