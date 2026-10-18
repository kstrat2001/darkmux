/**
 * What a card's status line says while an execution it shows is running: the
 * reading takes the status word's place. One named function over the
 * execution's own fields, so the five cases are tested without a DOM.
 */

import { compactDuration, fmtN } from "../../lib/format";
import { liveStateLabel, promptProgressLabel, reasonForLine } from "../../lib/tokenRate";
import type { ExecutionTokenReading, LiveState } from "../../lib/tokenRate";

/** The line's text. `byWidth` carries a long and a short form, because the
 *  card shows one by the viewport's width (the line keeps its one-line
 *  height): a rest's reason, and (#3210) a prefill's progress. */
export type ReadingLabel = { kind: "text"; text: string } | { kind: "byWidth"; full: string; word: string };

/** The reading for an execution in `state` (never the disconnection downgrade,
 *  where the line says `disconnected` instead). */
export function readingLabel(exec: ExecutionTokenReading, state: LiveState): ReadingLabel {
  if (state === "generating") return { kind: "text", text: generatingText(exec) };
  // (#3210) How far the engine has read the prompt, when it reports it, ahead
  // of the size estimate it makes redundant (the hover still says how much).
  // With a time left the line is "40% · ~23s left" alone: "processing" in
  // front ellipsized the time away on a three-card desktop row, and the
  // brain over its bar already says it. Before there is a rate, "processing
  // 20%".
  if (state === "prompt" && exec.promptProgress) {
    const progress = promptProgressLabel(exec.promptProgress);
    return { kind: "text", text: exec.promptProgress.secondsLeft !== undefined ? progress : `processing ${progress}` };
  }
  // (#2890) "processing ~36k", not "processing prompt · ~36k": the long form
  // ellipsized the size away on a phone.
  if (state === "prompt" && exec.promptLabel) return { kind: "text", text: `processing ${exec.promptLabel}` };
  // (#2950) Why it rests, in place of "rest Ns": the tube's center counts the seconds.
  if (state === "rest" && exec.restReason) return { kind: "byWidth", full: reasonForLine(exec.restReason), word: exec.restReasonWord ?? exec.restReason };
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
  if (state === "prompt" && exec.promptProgress) {
    const p = exec.promptProgress;
    const left = p.secondsLeft !== undefined ? `, about ${compactDuration(p.secondsLeft)} left` : "";
    // (#3210) mlx_lm reports the part of this request's prompt its cache did
    // not hold, not the whole prompt: say so.
    return `uncached prompt processed: ${fmtN(p.processed)} of ${fmtN(p.total)} tokens${left}`;
  }
  if (state === "prompt" && exec.promptLabel) return `estimated prompt size: ${exec.promptLabel} tokens`;
  if (state === "rest" && exec.restReason) return `resting: ${exec.restReason}`;
  return undefined;
}
