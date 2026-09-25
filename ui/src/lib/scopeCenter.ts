import type { ScopeState } from "./scopeMorph";

/** What one scope reading puts in the tube's center, for EVERY surface that
 *  draws a scope (the run page's hero and the fleet card alike, #2890:
 *  operator, "make the effect consistent across the app to avoid
 *  confusion"). One function, so the two can never drift apart again:
 *
 *  - GEN: the rounded rate ("—" when not yet measured) over "tok/s"
 *    (also while thinking; the shimmer and the rate line say that);
 *  - REST: the countdown ("12s") over "resting";
 *  - TOOLS: the tool's icon (drawn by `TokenScope`), with "tool gen" under
 *    it while the model generates the call (LM Studio's "tool call
 *    generation"; not "writing", which read as the edit/write tools' own
 *    action). No seconds: a growing number made the caption variable-width
 *    and it overran the ring; the status line keeps "tool gen · N s";
 *  - PROMPT: nothing; `TokenScope` draws the brain for the whole phase
 *    (#2890, operator: a size estimate in the center made one phase look
 *    like two; the size is in the event detail and the run page);
 *  - IDLE: the word "idle" on its own, centered;
 *  - everything else: nothing. */
export interface ScopeCenterInput {
  state: ScopeState;
  tokensPerSec: number | null;
  carried?: boolean;
  restSecondsLeft?: number;
  writing?: boolean;
  writingSeconds?: number;
  thinking?: boolean;
}

export interface ScopeCenter {
  centerLabel: string | null;
  centerUnit: string | null;
  centerCarried: boolean;
}

export function scopeCenter(r: ScopeCenterInput): ScopeCenter {
  const generating = r.state === "generating";
  const writing = r.state === "tools" && r.writing === true;
  const resting = r.state === "rest" && r.restSecondsLeft != null;
  return {
    centerLabel: generating
      ? r.tokensPerSec != null
        ? String(Math.round(r.tokensPerSec))
        : "—"
      : resting
        ? `${r.restSecondsLeft}s`
        : null,
    // (#2890, operator) "tok/s" while thinking too: "think tok/s" did not
    // fit inside the wave, and the violet shimmer plus the card's rate line
    // ("76 think tok/s") already say it is thinking.
    centerUnit: generating
      ? "tok/s"
      : resting
        ? "resting"
        : writing
          ? "tool gen"
          : r.state === "idle"
            ? "idle"
            : null,
    centerCarried: generating && r.carried === true,
  };
}
