import type { ScopeState } from "./scopeMorph";

/** What one scope reading puts in the tube's center, for EVERY surface that
 *  draws a scope (the run page's hero and the fleet card alike, #2890:
 *  operator, "make the effect consistent across the app to avoid
 *  confusion"). One function, so the two can never drift apart again:
 *
 *  - GEN: the rounded rate ("—" when not yet measured) over "tok/s", or
 *    "think tok/s" while the model reasons;
 *  - REST: the countdown ("12s") over "resting";
 *  - TOOLS: the tool's icon (drawn by `TokenScope`), with "writing · N s"
 *    under it while the model writes the call;
 *  - PROMPT: the prompt's size ("~18k") over "reading" when the turn's
 *    opening heartbeat reported it; otherwise `TokenScope` draws the brain;
 *  - everything else: nothing. */
export interface ScopeCenterInput {
  state: ScopeState;
  tokensPerSec: number | null;
  carried?: boolean;
  restSecondsLeft?: number;
  writing?: boolean;
  writingSeconds?: number;
  thinking?: boolean;
  promptLabel?: string | null;
}

export interface ScopeCenter {
  centerLabel: string | null;
  centerUnit: string | null;
  centerCarried: boolean;
}

export function scopeCenter(r: ScopeCenterInput): ScopeCenter {
  const generating = r.state === "generating";
  const writing = r.state === "tools" && r.writing === true;
  const promptLabel = r.state === "prompt" ? (r.promptLabel ?? null) : null;
  const resting = r.state === "rest" && r.restSecondsLeft != null;
  return {
    centerLabel: generating
      ? r.tokensPerSec != null
        ? String(Math.round(r.tokensPerSec))
        : "—"
      : resting
        ? `${r.restSecondsLeft}s`
        : promptLabel,
    centerUnit: generating
      ? r.thinking
        ? "think tok/s"
        : "tok/s"
      : resting
        ? "resting"
        : writing
          ? `writing · ${r.writingSeconds ?? 0} s`
          : promptLabel !== null
            ? "reading"
            : null,
    centerCarried: generating && r.carried === true,
  };
}
