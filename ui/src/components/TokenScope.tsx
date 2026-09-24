import { useEffect, useMemo, useRef } from "react";
import { PHOSPHOR_FALLBACK, toneRgb, type Rgb, type ScopeTone } from "../lib/scopeTone";
import {
  advanceMorph,
  createMorph,
  settleMorph,
  stateTone,
  toolIconKind,
  type ScopeMorph,
  type ScopeParams,
  type ScopeState,
} from "../lib/scopeMorph";
import { useCountUp } from "../hooks/useCountUp";
import { ToolIcon } from "./ToolIcon";
import { BrainGlyph } from "./ActivityIcon";

/**
 * (#2877) Live token-rate scope — a CRT oscilloscope, ported from the
 * operator-approved concept (`token-scope-concept.html`, private artifact
 * linked from the issue). Phosphor green, glow, an afterglow trail (the
 * per-frame fill uses a low-alpha fill rather than a hard clear, so the
 * previous frame bleeds through), scanlines (CSS, see `styles.css`'s
 * `.token-scope-screen::after`), and easing between heartbeats so a ~2s
 * data cadence reads as continuous motion.
 *
 * **Never rendered while idle.** This component does not know or check
 * "is the machine/run idle" — the CALLER decides whether to mount it at
 * all (`FleetLens`/`SessionReplay` render plain "idle" text instead of this
 * component when nothing is generating). That is what makes "idle machines
 * never animate" true by construction: an idle scope has zero instances,
 * not one instance quietly doing nothing.
 *
 * **Cost discipline (CLAUDE.md "the observer must not join the observed" +
 * this issue's "Observer cost" constraint):**
 * - Zero model work — this only reads a number a parent already computed
 *   from flow records already fetched. No dispatch, no extra network call.
 * - The rAF loop is torn down (not merely skipped) on `visibilitychange`
 *   when the tab goes hidden, and only re-started when it becomes visible
 *   again — a hidden tab schedules literally zero frames, not a per-frame
 *   no-op check.
 * - `prefers-reduced-motion: reduce` draws exactly one frame and never
 *   starts the loop — "a static readout" per the issue's constraint.
 * - Sized from its own box via `ResizeObserver`, not `window resize` alone
 *   — a hidden card becoming visible (grid reflow) resizes correctly
 *   without a window-level event to key off.
 *
 * (#2890) **One trace, morphing.** The per-state drawing is gone: every
 * state is a set of targets on ONE trace (`lib/scopeMorph.ts`), and a state
 * change glides the parameters toward them, so GEN, PROMPT, TOOLS, REST,
 * STALL, no signal and finished morph into each other and never pop. The
 * drawing below is the operator-approved prototype's
 * (`scope-states-prototype.html`) ported as-is. The center shows the rate
 * (easing between values) while generating, the average when finished, a
 * glowing icon for the tool while in TOOLS, and nothing otherwise.
 */

export type TokenScopeSize = "mini" | "card" | "tile";

export interface TokenScopeProps {
  /** Current tok/s reading. Callers pass whatever `tokenRate.ts`'s
   *  `currentTokenRate`/`aggregateTokenRate` last produced. Drives the GEN
   *  wave only; it is never read as "is it running" (that is `state`). */
  tokensPerSec: number | null;
  /** (#2890) The scope's state, which picks the trace's targets. When
   *  omitted it is derived from the older props below (`stalled`,
   *  `resting`, `tone`), so an existing caller keeps working. */
  state?: ScopeState;
  /** (#2890) While `state` is `"tools"`: the tool being run (the latest
   *  completed call's `tool_name`), drawn as an icon in the center. Unknown
   *  or absent reads as the gear. */
  toolName?: string | null;
  /** No fresh heartbeat recently. Used only when `state` is omitted. */
  stalled?: boolean;
  /** A rest/pause (e.g. thermal). Used only when `state` is omitted. */
  resting?: boolean;
  /** `"mini"`, `"card"` = the fleet machine card, `"tile"` = the run page's
   *  MODEL hero. Each maps to a sizing rule in `styles.css`; see that
   *  file's own comment on why this is never `aspect-ratio` + percentage
   *  padding. */
  size: TokenScopeSize;
  /** The number centered inside the tube: the live rate while generating,
   *  the average once finished. Ignored in TOOLS (the icon is the whole
   *  message). A plain integer eases between values. */
  centerLabel?: string | null;
  /** (#2890) A quiet unit under the number (`tok/s`, `avg tok/s`). The run
   *  page passes it; the fleet card does not. */
  centerUnit?: string | null;
  /** (#2885) `true` when `centerLabel` is a rate carried forward from an
   *  earlier turn rather than freshly measured — dims the number
   *  (`data-carried` on `.token-scope-n`, see `styles.css`) so it reads as
   *  "last known", not a fresh sample. No effect when `centerLabel` is
   *  unset. */
  centerCarried?: boolean;
  className?: string;
  /** The live state as a lamp tone (`lib/scopeTone.ts`). Used to pick the
   *  state when `state` is omitted; the trace's color always follows the
   *  state (`stateTone`). */
  tone?: ScopeTone;
}

/** The older props to a state, for a caller that does not pass `state`. */
function legacyState(stalled: boolean, resting: boolean, tone: ScopeTone): ScopeState {
  if (stalled) return "stalled";
  if (resting) return "rest";
  if (tone === "none") return "idle";
  return tone;
}

/** Free-running clocks that are not morph parameters: the wave's phase
 *  (speed follows the rate), the breath, the comet sweep and the inward
 *  rings (fixed tempos). */
interface ScopeClocks {
  phase: number;
  breathT: number;
  sweep: number;
  inwardT: number;
}

function prefersReducedMotion(): boolean {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return false;
  try {
    return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  } catch {
    return false;
  }
}

/** Mix a channel toward white by `k`, the hot core of a dot. */
function lift(c: number, k: number): number {
  return Math.round(c + (255 - c) * k);
}

function rgba(r: number, g: number, b: number, a: number): string {
  return `rgba(${Math.round(r)},${Math.round(g)},${Math.round(b)},${a > 0 ? a : 0})`;
}

/** Two passes (a soft wide glow and a bright thin core), hoisted so the
 *  per-frame loop does not rebuild them. */
const TRACE_PASSES: ReadonlyArray<readonly [number, number]> = [
  [3, 0.22],
  [1.2, 0.88],
];
const INWARD_PASSES: ReadonlyArray<readonly [number, number]> = [
  [2.4, 0.25],
  [1, 0.8],
];

/** One frame of the scope, from the current morph parameters `p`. Ported
 *  from the prototype's `draw()`: an afterglow fill (a low-alpha fill rather
 *  than a hard clear, so the previous frame bleeds through), then, each
 *  faded in by its own parameter, the main trace (the GEN wave rides on
 *  it), GEN's sweep dot, the TOOLS comet, the PROMPT inward rings, REST's
 *  drifting dot, the STALL ember and the no-signal static. `sx`/`sy` scale
 *  every layer, which is what squashes the tube to a line and a dot when it
 *  stalls. `clock` is the scope's own running time (seconds), for the
 *  ember's pulse. Additive blending ("lighter") for the glow. */
function drawFrame(ctx: CanvasRenderingContext2D, w: number, h: number, p: ScopeParams, c: ScopeClocks, clock: number, dt: number) {
  const cx = w / 2;
  const cy = h / 2;
  const R = Math.min(w, h) * 0.34;
  const { r: cr, g: cg, b: cb } = p;
  const tps = p.wave;
  const active = Math.min(1, tps / 25);
  ctx.globalCompositeOperation = "source-over";
  ctx.fillStyle = `rgba(5,8,6,${0.24 + 0.14 * active + 0.25 * p.fuzz})`;
  ctx.fillRect(0, 0, w, h);

  // Clocks: the wave's phase speed follows the rate; the rest are fixed tempos.
  // (#2890) `tempo` slows a finished run's echo; every live state runs at 1.
  c.phase += (0.6 + tps * 0.09) * dt * 60 * p.tempo;
  c.breathT += dt * 1.1;
  c.sweep += dt * Math.PI * 1.6;
  c.inwardT = (c.inwardT + dt * 0.45) % 1;

  ctx.globalCompositeOperation = "lighter";
  const lobes = Math.min(8, 3 + tps / 22);
  const breathe = 0.5 + 0.5 * Math.sin(c.breathT);
  const rBase = R * p.rscale * (1 + 0.06 * p.breath * (breathe - 0.5));
  const amp = R * (0.03 + 0.16 * active) * (1 - 0.8 * p.breath);

  // The main trace.
  if (p.ring > 0.01) {
    const bright = Math.min(1, p.ring * (1 - 0.3 * p.breath * (1 - breathe)));
    for (const [lw, a] of TRACE_PASSES) {
      ctx.beginPath();
      for (let i = 0; i <= 240; i++) {
        const t = (i / 240) * Math.PI * 2;
        const wave = Math.sin(lobes * t - c.phase) + 0.12 * Math.sin((lobes * 2 + 1) * t + c.phase * 1.7);
        const r = rBase + amp * wave;
        const x = cx + Math.cos(t) * r * p.sx;
        const y = cy + Math.sin(t) * r * p.sy;
        if (i === 0) ctx.moveTo(x, y);
        else ctx.lineTo(x, y);
      }
      ctx.closePath();
      ctx.strokeStyle = rgba(cr, cg, cb, a * bright);
      ctx.lineWidth = lw;
      ctx.shadowColor = rgba(cr, cg, cb, 0.8);
      ctx.shadowBlur = lw * 3 * bright;
      ctx.stroke();
    }
    ctx.shadowBlur = 0;
  }
  // GEN's sweep dot fades in with the rate, and out with tempo: a racing dot
  // reads as live, so a finished run's slow echo has none (#2890).
  const dotLive = Math.max(0, Math.min(1, (p.tempo - 0.3) / 0.7));
  if (active > 0.05 && p.sx > 0.5 && dotLive > 0.02) {
    const ang = -c.phase * 0.5;
    const sr = rBase + amp * Math.sin(lobes * ang - c.phase);
    ctx.beginPath();
    ctx.arc(cx + Math.cos(ang) * sr * p.sx, cy + Math.sin(ang) * sr * p.sy, Math.max(1.4, R * 0.035), 0, Math.PI * 2);
    ctx.fillStyle = rgba(lift(cr, 0.55), lift(cg, 0.55), lift(cb, 0.55), (0.5 + 0.45 * active) * active * dotLive);
    ctx.shadowColor = rgba(cr, cg, cb, 0.9);
    ctx.shadowBlur = 10;
    ctx.fill();
    ctx.shadowBlur = 0;
  }
  // TOOLS: a comet sweeping at a constant tempo. The prototype blurred each
  // of the 36 tail segments; measured in a headless render that cost ~16 ms
  // per frame (5x any other state) because canvas shadow blur is paid per
  // stroke. The glow is now ONE blurred stroke under the leading half of the
  // tail, and the segments that taper it are drawn unblurred on top, which
  // reads the same at a fraction of the cost (#2890).
  if (p.comet > 0.01) {
    const head = c.sweep;
    const len = Math.PI * 0.55;
    const n = 36;
    ctx.beginPath();
    ctx.ellipse(cx, cy, rBase * p.sx, rBase * p.sy, 0, head - len * 0.5, head);
    ctx.strokeStyle = rgba(cr, cg, cb, 0.45 * p.comet);
    ctx.lineWidth = 3;
    ctx.shadowColor = rgba(cr, cg, cb, 0.8);
    ctx.shadowBlur = 8 * p.comet;
    ctx.stroke();
    ctx.shadowBlur = 0;
    for (let i = 0; i < n; i++) {
      const f = 1 - i / n;
      ctx.beginPath();
      ctx.ellipse(cx, cy, rBase * p.sx, rBase * p.sy, 0, head - len * ((i + 1) / n), head - len * (i / n));
      ctx.strokeStyle = rgba(cr, cg, cb, 0.9 * f * p.comet);
      ctx.lineWidth = 1.4 + 2.2 * f;
      ctx.stroke();
    }
    ctx.beginPath();
    ctx.arc(cx + Math.cos(head) * rBase * p.sx, cy + Math.sin(head) * rBase * p.sy, Math.max(1.6, R * 0.045), 0, Math.PI * 2);
    ctx.fillStyle = rgba(lift(cr, 0.6), lift(cg, 0.6), lift(cb, 0.6), 0.95 * p.comet);
    ctx.shadowBlur = 12 * p.comet;
    ctx.fill();
    ctx.shadowBlur = 0;
  }
  // PROMPT: rings peel off the base ring and sink toward the brain, the
  // prompt being absorbed.
  if (p.inward > 0.01) {
    for (let k = 0; k < 3; k++) {
      const q = (c.inwardT + k / 3) % 1;
      // (#2890 operator review) Each ring is born ON the base ring, wave and
      // all, then shrinks toward the brain, its wave flattening as it goes,
      // and fades out at half the base radius so the center stays clear.
      const shrink = 1 - 0.5 * q;
      const waveAmp = amp * (1 - q);
      const a = Math.min(1, q / 0.12) * Math.pow(1 - q, 1.1) * 0.9 * p.inward;
      for (const [lw, al] of INWARD_PASSES) {
        ctx.beginPath();
        for (let i = 0; i <= 120; i++) {
          const t = (i / 120) * Math.PI * 2;
          const wave = Math.sin(lobes * t - c.phase) + 0.12 * Math.sin((lobes * 2 + 1) * t + c.phase * 1.7);
          const r = (rBase + waveAmp * wave) * shrink;
          const x = cx + Math.cos(t) * r * p.sx;
          const y = cy + Math.sin(t) * r * p.sy;
          if (i === 0) ctx.moveTo(x, y);
          else ctx.lineTo(x, y);
        }
        ctx.closePath();
        ctx.strokeStyle = rgba(cr, cg, cb, al * a);
        ctx.lineWidth = lw;
        ctx.shadowColor = rgba(cr, cg, cb, 0.8);
        ctx.shadowBlur = lw * 3 * a;
        ctx.stroke();
      }
    }
    ctx.shadowBlur = 0;
  }
  // REST: a slow drifting dot on the breathing circle.
  if (p.breath > 0.05) {
    const a = c.breathT * 0.32;
    ctx.beginPath();
    ctx.arc(cx + Math.cos(a) * rBase * p.sx, cy + Math.sin(a) * rBase * p.sy, Math.max(1.2, R * 0.03), 0, Math.PI * 2);
    ctx.fillStyle = rgba(lift(cr, 0.5), lift(cg, 0.5), lift(cb, 0.5), 0.55 * p.breath);
    ctx.fill();
  }
  // STALL: the ember left after the collapse, pulsing slowly.
  if (p.ember > 0.01) {
    const pulse = 0.5 + 0.5 * Math.sin(clock * 2);
    const a = (0.35 + 0.25 * pulse) * p.ember;
    const r = R * (0.055 + 0.01 * pulse);
    ctx.beginPath();
    ctx.arc(cx, cy, Math.max(1.2, r), 0, Math.PI * 2);
    ctx.fillStyle = rgba(lift(cr, 0.6), lift(cg, 0.6), lift(cb, 0.6), a);
    ctx.shadowColor = rgba(cr, cg, cb, a);
    ctx.shadowBlur = 16 * a + 4;
    ctx.fill();
    ctx.shadowBlur = 0;
  }
  // NO SIGNAL: static fading in.
  if (p.fuzz > 0.02) {
    const rr = Math.min(w, h) / 2;
    const n = Math.round(((w * h) / 18) * p.fuzz);
    ctx.globalCompositeOperation = "source-over";
    for (let i = 0; i < n; i++) {
      const x = Math.random() * w;
      const y = Math.random() * h;
      if ((x - cx) ** 2 + (y - cy) ** 2 > rr * rr) continue;
      const v = 90 + Math.random() * 120;
      ctx.fillStyle = rgba(v, v, v + 8, (0.18 + Math.random() * 0.3) * p.fuzz);
      ctx.fillRect(x, y, 1.3, 1.3);
    }
  }
}

/** A plain whole number eases between values; anything else ("—") is shown
 *  as-is. */
function parseWhole(label: string | null | undefined): number | null {
  return label != null && /^\d+$/.test(label) ? Number(label) : null;
}

export function TokenScope({
  tokensPerSec,
  state: stateProp,
  toolName,
  stalled = false,
  resting = false,
  size,
  centerLabel,
  centerUnit,
  centerCarried = false,
  className,
  tone = "generating",
}: TokenScopeProps) {
  const state: ScopeState = stateProp ?? legacyState(stalled, resting, tone);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const morphRef = useRef<ScopeMorph>(createMorph());
  const clocksRef = useRef<ScopeClocks>({ phase: Math.random() * 6, breathT: Math.random() * 6, sweep: Math.random() * 6, inwardT: Math.random() });
  // GEN's live rate, or a finished run's average for its echo (#2890).
  const rate = state === "generating" || state === "finished" ? Math.max(0, tokensPerSec ?? 0) : 0;
  // The trace takes the state's color, read once per state change from the
  // same :root token its lamp uses (never per frame).
  const rgb = useMemo(() => toneRgb(stateTone(state)), [state]);
  // Live values the rAF loop reads without restarting the effect below on
  // every update (a heartbeat every ~2s would otherwise tear down and
  // rebuild the canvas/observer/listener on that same cadence).
  const targetRef = useRef<{ state: ScopeState; rate: number; rgb: Rgb }>({ state, rate, rgb: PHOSPHOR_FALLBACK });
  targetRef.current = { state, rate, rgb };
  // Set by the effect: redraws one settled frame when motion is reduced.
  const staticRedrawRef = useRef<(() => void) | null>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return undefined;
    const ctx = canvas.getContext("2d");
    if (!ctx) return undefined;

    const reduce = prefersReducedMotion();
    function drawStatic() {
      const w = canvas!.clientWidth;
      const h = canvas!.clientHeight;
      const t = targetRef.current;
      const p = settleMorph(morphRef.current, t.state, t.rate, t.rgb);
      if (w && h) drawFrame(ctx!, w, h, p, clocksRef.current, morphRef.current.clock, 0);
    }

    function size2() {
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      const rect = canvas!.getBoundingClientRect();
      canvas!.width = Math.max(1, Math.round(rect.width * dpr));
      canvas!.height = Math.max(1, Math.round(rect.height * dpr));
      ctx!.setTransform(dpr, 0, 0, dpr, 0, 0);
      if (reduce) drawStatic();
    }
    size2();

    const ro = typeof ResizeObserver !== "undefined" ? new ResizeObserver(size2) : null;
    ro?.observe(canvas);

    if (reduce) {
      // Static readout: one settled frame, redrawn only when the state or
      // rate changes (the effect below), never a loop.
      staticRedrawRef.current = drawStatic;
      drawStatic();
      return () => {
        staticRedrawRef.current = null;
        ro?.disconnect();
      };
    }

    let rafId: number | null = null;
    let last = 0;
    function frame(now: number) {
      const w = canvas!.clientWidth;
      const h = canvas!.clientHeight;
      const dt = Math.min(0.1, Math.max(0, (now - last) / 1000)) || 0;
      last = now;
      const t = targetRef.current;
      const p = advanceMorph(morphRef.current, t.state, t.rate, t.rgb, dt);
      if (w && h) drawFrame(ctx!, w, h, p, clocksRef.current, morphRef.current.clock, dt);
      rafId = requestAnimationFrame(frame);
    }
    function start() {
      if (rafId !== null) return;
      last = performance.now();
      rafId = requestAnimationFrame(frame);
    }
    function stop() {
      if (rafId !== null) {
        cancelAnimationFrame(rafId);
        rafId = null;
      }
    }
    const onVisibility = () => {
      if (document.hidden) stop();
      else start();
    };
    document.addEventListener("visibilitychange", onVisibility);
    if (!document.hidden) start();
    return () => {
      stop();
      document.removeEventListener("visibilitychange", onVisibility);
      ro?.disconnect();
    };
    // Intentionally NOT depending on the state/rate — those ride
    // `targetRef` so a heartbeat never re-creates the canvas/observer/listener.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    staticRedrawRef.current?.();
  }, [state, rate, rgb]);

  const whole = parseWhole(centerLabel);
  const eased = useCountUp(whole, (n) => (n === null ? "" : String(Math.round(n))));
  const shownLabel = whole !== null ? eased : centerLabel;
  const showIcon = state === "tools";
  // PROMPT shows a pulsing brain until the runtime reports the prompt size
  // mid-turn (#2889); then the count takes the center like any other number.
  const showBrain = state === "prompt" && centerLabel == null;
  const showNumber = !showIcon && !showBrain && centerLabel != null;

  const cls = ["token-scope-bezel", `token-scope-bezel--${size}`, className].filter(Boolean).join(" ");
  return (
    <div className={cls} data-tone={stateTone(state)} data-state={state}>
      <div className="token-scope-screen">
        <canvas ref={canvasRef} aria-hidden="true" />
        {showIcon && (
          <div className="token-scope-center token-scope-center--icon">
            <ToolIcon kind={toolIconKind(toolName)} className="token-scope-ico" />
          </div>
        )}
        {showBrain && (
          <div className="token-scope-center token-scope-center--icon token-scope-center--brain">
            <BrainGlyph className="token-scope-ico" />
          </div>
        )}
        {showNumber && (
          <div className="token-scope-center">
            <span className="token-scope-n" data-carried={centerCarried ? "true" : "false"}>
              {shownLabel}
            </span>
            {centerUnit ? <span className="token-scope-u">{centerUnit}</span> : null}
          </div>
        )}
      </div>
    </div>
  );
}
