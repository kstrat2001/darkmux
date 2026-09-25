import type { Rgb, ScopeTone } from "./scopeTone";
import type { LiveState } from "./tokenRate";

/**
 * (#2890) The scope's parameter-morph engine, ported from the
 * operator-approved prototype (`scope-states-prototype.html`, kept with the
 * lab record). There are no per-state drawings: every state is a set of
 * TARGETS on one trace, and a state change glides the parameters toward the
 * new targets with an exponential approach per frame, so states morph into
 * each other and never pop.
 *
 * Pure and clock-free on purpose. Time enters ONLY as `dt` (seconds since the
 * previous frame), so a test drives the morph with a fixed step and asserts
 * where it lands, and the component's rAF loop drives it with real frame
 * deltas. Nothing here reads `Date.now()`/`performance.now()`.
 *
 * Allocation-light on purpose too: this runs every animation frame on every
 * mounted scope. `scopeTargets` writes into a caller-owned object and color
 * is three numbers rather than an array that would be re-created per frame.
 */

/** The scope's visible states. The five live ones are `LiveState`'s own;
 *  `nosignal` is a stall reading the page could not trust (connection lost),
 *  `finished` is a run that has ended (calm dimmed ring, the average in the
 *  center), and `idle` is no live execution (a mission between model steps). */
export type ScopeState = LiveState | "nosignal" | "finished" | "idle";

/** One frame's worth of trace parameters.
 *  - `ring`: brightness of the main trace
 *  - `wave`: the rate driving the wave (GEN)
 *  - `comet`: the TOOLS sweep
 *  - `inward`: the PROMPT rings
 *  - `breath`: the REST breathing
 *  - `fuzz`: no-signal static
 *  - `sx`/`sy`: horizontal and vertical scale (the CRT collapse and power-on)
 *  - `ember`: the STALL dot
 *  - `rscale`: ring radius scale
 *  - `tempo`: how fast the wave turns (1 live; slow for a finished run's echo)
 *  - `scribe`: (#2889) TOOLS while the model WRITES the call — the comet's
 *    tail stretches and its head scribbles, same color and icon as running
 *  - `drift`: REST's slow dot drifting on the breathing circle (idle breathes
 *    without it)
 *  - `iris`: (#2890) GEN while the model is THINKING (reasoning, not visible
 *    text): a violet-to-pink shimmer blended into the GEN color
 *  - `r`/`g`/`b`: color */
export interface ScopeParams {
  ring: number;
  wave: number;
  comet: number;
  inward: number;
  breath: number;
  fuzz: number;
  sx: number;
  sy: number;
  ember: number;
  rscale: number;
  tempo: number;
  scribe: number;
  iris: number;
  drift: number;
  r: number;
  g: number;
  b: number;
}

/** How fast each parameter chases its target, per second. The CRT collapse
 *  (`sx`/`sy`) runs fast; everything else settles in about half a second.
 *  Verbatim from the prototype. */
export const SCOPE_SPEED = {
  ring: 6,
  wave: 4,
  comet: 5,
  inward: 5,
  breath: 4,
  fuzz: 6,
  sx: 16,
  sy: 16,
  ember: 5,
  rscale: 5,
  tempo: 4,
  scribe: 4,
  iris: 3,
  drift: 4,
  rgb: 5,
} as const;

/** The morphing keys, in a fixed array so the per-frame loop does not call
 *  `Object.keys` (which allocates). Color is handled separately. */
const MORPH_KEYS = ["ring", "wave", "comet", "inward", "breath", "fuzz", "sx", "sy", "ember", "rscale", "tempo", "scribe", "iris", "drift"] as const;

function blankParams(): ScopeParams {
  return { ring: 1, wave: 0, comet: 0, inward: 0, breath: 0, fuzz: 0, sx: 1, sy: 1, ember: 0, rscale: 1, tempo: 1, scribe: 0, iris: 0, drift: 0, r: 0, g: 0, b: 0 };
}

/** The targets for `state`, `sinceSec` seconds after entering it, written
 *  into `out` (a fresh object when omitted). `rate` only matters for GEN
 *  and FINISHED (its average);
 *  `sinceSec` only for STALL, whose collapse runs in three phases: squash to
 *  a line, shrink the line to a dot, then leave an ember. `writing` (#2889)
 *  only matters for TOOLS: the model is generating the call's arguments
 *  rather than darkmux running it, a variant of the same state. */
export function scopeTargets(
  state: ScopeState,
  sinceSec: number,
  rate: number,
  rgb: Rgb,
  out: ScopeParams = blankParams(),
  writing = false,
  thinking = false,
): ScopeParams {
  out.ring = 1;
  out.wave = 0;
  out.comet = 0;
  out.inward = 0;
  out.breath = 0;
  out.fuzz = 0;
  out.sx = 1;
  out.sy = 1;
  out.ember = 0;
  out.rscale = 1;
  out.tempo = 1;
  out.scribe = 0;
  out.iris = 0;
  out.drift = 0;
  out.r = rgb[0];
  out.g = rgb[1];
  out.b = rgb[2];
  switch (state) {
    case "generating":
      out.wave = Math.max(0, rate);
      if (thinking) out.iris = 1;
      break;
    case "prompt":
      out.ring = 0.35;
      out.inward = 1;
      break;
    case "tools":
      out.ring = 0.3;
      out.comet = 1;
      if (writing) out.scribe = 1;
      break;
    case "rest":
      out.ring = 0.8;
      out.breath = 1;
      out.drift = 1;
      out.rscale = 0.92;
      break;
    case "nosignal":
      out.ring = 0;
      out.fuzz = 1;
      break;
    case "finished":
      // (#2890 operator review) The ended run's echo: the wave its AVERAGE
      // rate draws, so the shape matches the number in the center, turning
      // slowly and dimmer than live GEN. A flat ring under "192 avg tok/s"
      // read as a run that never generated.
      out.ring = 0.55;
      out.rscale = 0.96;
      out.wave = Math.max(0, rate);
      out.tempo = 0.12;
      break;
    case "idle":
      // No live execution: a machine with nothing running, or a mission
      // between model steps. (#2890, operator: "similar to rest") It breathes
      // like REST, dimmer, in the neutral color, with no drifting dot: on and
      // waiting, not cooling down.
      out.ring = 0.45;
      out.breath = 0.7;
      out.rscale = 0.94;
      break;
    case "stalled":
      if (sinceSec < 0.3) {
        out.ring = 1.1;
        out.sy = 0.02;
      } else if (sinceSec < 0.6) {
        out.ring = 1.2;
        out.sy = 0.02;
        out.sx = 0.02;
      } else {
        out.ring = 0;
        out.sy = 0.02;
        out.sx = 0.02;
        out.ember = 1;
      }
      break;
  }
  return out;
}

/** A scope's morph state, owned by one mounted component. `p` is `null`
 *  until the first frame, which adopts the targets outright. `clock` is the
 *  scope's own running time (seconds, advanced by `dt` only) for the tempos
 *  that are not per-state, such as the ember's pulse. */
export interface ScopeMorph {
  p: ScopeParams | null;
  tgt: ScopeParams;
  state: ScopeState | null;
  sinceSec: number;
  /** Seconds since leaving a STALL, or `null` when not powering back on. */
  leftStall: number | null;
  clock: number;
}

export function createMorph(): ScopeMorph {
  return { p: null, tgt: blankParams(), state: null, sinceSec: 0, leftStall: null, clock: 0 };
}

/** Advance one frame: track how long we have been in `state`, compute its
 *  targets, and move every parameter toward them by `1 - exp(-speed * dt)`.
 *  Leaving a STALL powers the tube back on: for the first quarter second the
 *  trace is held as a line (the dot stretches out), then it opens into the
 *  next state's ring. Returns the (mutated, reused) current parameters.
 *  `writing` is TOOLS' writing variant (#2889); a change of it alone is not a
 *  state change, so it glides like every other parameter. */
export function advanceMorph(m: ScopeMorph, state: ScopeState, rate: number, rgb: Rgb, dt: number, writing = false, thinking = false): ScopeParams {
  const step = Number.isFinite(dt) && dt > 0 ? dt : 0;
  m.clock += step;
  if (state !== m.state) {
    if (m.state === "stalled") m.leftStall = 0;
    m.state = state;
    m.sinceSec = 0;
  } else {
    m.sinceSec += step;
  }
  const tgt = scopeTargets(state, m.sinceSec, rate, rgb, m.tgt, writing, thinking);
  if (m.leftStall !== null) {
    m.leftStall += step;
    if (m.leftStall < 0.25) tgt.sy = 0.02;
    if (m.leftStall > 1) m.leftStall = null;
  }
  if (!m.p) {
    m.p = { ...tgt };
    return m.p;
  }
  const p = m.p;
  for (const key of MORPH_KEYS) {
    const k = 1 - Math.exp(-SCOPE_SPEED[key] * step);
    p[key] += (tgt[key] - p[key]) * k;
  }
  const kc = 1 - Math.exp(-SCOPE_SPEED.rgb * step);
  p.r += (tgt.r - p.r) * kc;
  p.g += (tgt.g - p.g) * kc;
  p.b += (tgt.b - p.b) * kc;
  return p;
}

/** Jump straight to `state`'s settled look, with no glide: the static frame
 *  `prefers-reduced-motion: reduce` draws. A stall settles on its ember. */
export function settleMorph(m: ScopeMorph, state: ScopeState, rate: number, rgb: Rgb, writing = false, thinking = false): ScopeParams {
  m.state = state;
  m.sinceSec = state === "stalled" ? 1 : 0;
  m.leftStall = null;
  const tgt = scopeTargets(state, m.sinceSec, rate, rgb, m.tgt, writing, thinking);
  m.p = { ...tgt };
  return m.p;
}

/** The tool icons the TOOLS center shows. `other` is the gear. */
export type ToolIconKind = "read" | "edit" | "write" | "bash" | "search" | "other";

const KNOWN_TOOL_ICONS: ReadonlySet<string> = new Set(["read", "edit", "write", "bash", "search"]);

/** A tool name, as the runtime records it (`dispatch.tool`'s `tool_name`),
 *  to its icon. Anything else, or no name at all, is the gear. */
export function toolIconKind(name: string | null | undefined): ToolIconKind {
  const n = (name ?? "").trim().toLowerCase();
  return KNOWN_TOOL_ICONS.has(n) ? (n as ToolIconKind) : "other";
}

/** One scope state from the readings callers already have: the live state
 *  (`null` when there is no live execution), whether that `null` came from a
 *  lost connection, and whether the run has finished. */
export function scopeStateOf(reading: { state: LiveState | null; noSignal?: boolean; finished?: boolean }): ScopeState {
  if (reading.finished) return "finished";
  if (reading.state !== null) return reading.state;
  return reading.noSignal ? "nosignal" : "idle";
}

/** The color a scope state draws in, as a `ScopeTone` (`lib/scopeTone.ts`
 *  resolves it from the live stylesheet). */
export function stateTone(state: ScopeState): ScopeTone {
  switch (state) {
    case "finished":
    case "idle":
      return "none";
    default:
      return state;
  }
}
