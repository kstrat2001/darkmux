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
 *    and it overran the ring; the status line keeps "tool gen · <tool> · Ns";
 *  - PROMPT while compacting (#2915): "compacting" on its own, with the
 *    utility treatment, no timer (the status line counts);
 *  - PROMPT: nothing; `TokenScope` draws the brain for the whole phase
 *    (#2890, operator: a size estimate in the center made one phase look
 *    like two; the size is in the event detail and the run page);
 *  - IDLE: the word "idle" on its own, centered; or, while a run is in
 *    flight with no model working (#2911, `inFlight`), that phrase;
 *  - everything else: nothing. */
export interface ScopeCenterInput {
  state: ScopeState;
  tokensPerSec: number | null;
  carried?: boolean;
  restSecondsLeft?: number;
  writing?: boolean;
  writingSeconds?: number;
  thinking?: boolean;
  /** (#2911) A run is in flight on this surface (a mission between model
   *  steps, a lab run with no execution yet). Read only for `idle`: the
   *  center then says "no model working", the run page's own phrase for
   *  that state, where a bare "idle" contradicted "dispatch in flight" on
   *  the same card. A machine with nothing running keeps "idle". */
  inFlight?: boolean;
  /** (#2915) PROMPT because the execution is compacting. The center then
   *  reads "compacting" on its own (no timer: the status line counts) and
   *  the tube takes the utility treatment (`utility`) instead of the work
   *  model's brain. Read only for `prompt`. */
  compacting?: boolean;
}

export interface ScopeCenter {
  centerLabel: string | null;
  centerUnit: string | null;
  centerCarried: boolean;
  /** (#2915) Present (always `true`) only while compacting: the tube draws
   *  a utility job's work, not the work model's (`TokenScope`'s `utility`). */
  utility?: true;
}

export function scopeCenter(r: ScopeCenterInput): ScopeCenter {
  if (r.state === "prompt" && r.compacting === true) {
    return { centerLabel: null, centerUnit: "compacting", centerCarried: false, utility: true };
  }
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
            ? r.inFlight
              ? "no model working"
              : "idle"
            : null,
    centerCarried: generating && r.carried === true,
  };
}
