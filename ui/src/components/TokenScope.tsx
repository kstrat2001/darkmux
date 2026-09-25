import { useEffect, useMemo, useRef } from "react";
import { lighten, PHOSPHOR_FALLBACK, toneRgb, type Rgb, type ScopeTone } from "../lib/scopeTone";

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
 */

export type TokenScopeSize = "mini" | "card" | "tile";

export interface TokenScopeProps {
  /** Current tok/s reading. Callers pass whatever `tokenRate.ts`'s
   *  `currentTokenRate`/`aggregateTokenRate` last produced. This prop is
   *  READ, never treated as authoritative "is it running" — that's
   *  `stalled` below, kept separate because a stall's target rate is 0 but
   *  the DECAY behavior (ring flattening) differs from "genuinely producing
   *  0 tok/s" (which doesn't really happen, but the two are conceptually
   *  distinct states per the issue). */
  tokensPerSec: number | null;
  /** No fresh heartbeat recently — decays the wave toward a flat ring with
   *  a little noise jitter, per the issue's "a stall decays toward a flat
   *  ring". */
  stalled?: boolean;
  /** A rest/pause (e.g. thermal) — dims the tube per the issue's "a rest or
   *  pause dims the tube", independent of `stalled`. */
  resting?: boolean;
  /** `"mini"` = fleet machine card, `"tile"` = the run page's TOK/S MODEL
   *  tile. Each maps to a fixed `--d`/`--rim` pair in `styles.css` — see
   *  that file's own comment on why this is never `aspect-ratio` +
   *  percentage padding. */
  size: TokenScopeSize;
  /** Placement 2's "only the number centered inside the tube" — the run
   *  page passes the tok/s readout here; the fleet card leaves it unset. */
  centerLabel?: string | null;
  className?: string;
  /** The live state, which colors the trace to match its lit lamp
   *  (`lib/scopeTone.ts`). `"none"`: no live execution, phosphor green. */
  tone?: ScopeTone;
}

interface ScopeAnim {
  shown: number;
  target: number;
  phase: number;
  bright: number;
  /** (#2877 pass 2) A slow accumulator that advances regardless of rate —
   *  the "minimum ripple that breathes" a quiet-but-alive tube needs so it
   *  never reads as a dead flat ring, distinct from `stalled`'s jittery
   *  decay (see `drawFrame`'s own doc). */
  breath: number;
}

function prefersReducedMotion(): boolean {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return false;
  try {
    return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  } catch {
    return false;
  }
}

/** One frame of the phosphor trace, at the given radius fraction of
 * `min(w, h)`. Ported from the concept's `drawScope` — two passes (a soft
 * wide glow pass + a bright thin core pass), additive blending
 * ("lighter"), plus a phosphor sweep dot on top.
 *
 * (#2877 pass 2, operator phone screenshot at 88 tok/s) The original lobe
 * count (`3 + round(min(tps,200)/12)`, ~10 lobes at 88 tok/s) PLUS a
 * same-strength second harmonic (`lobes*2+1` peaks at 0.35 amplitude) PLUS
 * the afterglow trail together read as "a fuzzy glowing donut, not a
 * waveform" — too much density smeared together for the eye to resolve
 * individual peaks. Three changes, all in the direction of "speed and
 * brightness carry the busier signal, not density":
 * 1. Lobes are CAPPED (8) and grow more slowly (`tps / 22`, not `/12`) —
 *    individually countable at speed, at both the card and the grown mobile
 *    tile size.
 * 2. The second harmonic drops from an equal-weight second wave (0.35) to a
 *    faint texture (0.12) — it no longer doubles the perceived peak count.
 * 3. The afterglow trail itself fades FASTER as the rate climbs
 *    (`trailAlpha` rises with `active`), so a faster-moving trace does not
 *    smear into a longer, denser streak than a slow one already read fine
 *    at.
 * Busy still reads busier: phase velocity (rotation speed) and the sweep
 * dot's speed and brightness both still scale with the rate directly. */
function drawFrame(ctx: CanvasRenderingContext2D, w: number, h: number, anim: ScopeAnim, stalled: boolean, resting: boolean, rgb: Rgb) {
  const [cr, cg, cb] = rgb;
  const [hr, hg, hb] = lighten(rgb, 0.55);
  const cx = w / 2;
  const cy = h / 2;
  const R = Math.min(w, h) * 0.34;
  ctx.globalCompositeOperation = "source-over";

  const tps = anim.shown;
  const active = stalled ? 0 : Math.min(1, tps / 25);
  // Low-alpha fill (not a hard clear) is the afterglow trail: the previous
  // frame's trace fades rather than vanishing outright. Faster at higher
  // rates — see this function's own doc, point 3.
  const trailAlpha = 0.22 + 0.14 * active;
  ctx.fillStyle = `rgba(5,8,6,${trailAlpha})`;
  ctx.fillRect(0, 0, w, h);

  // Rest and stall both slow the rotation — a rest is a deliberate pause,
  // not merely "zero rate", so it reads calmer than an ordinary quiet tube.
  const rate = resting ? 0.4 : 1;
  anim.phase += (0.6 + tps * 0.09) * rate;
  anim.breath += (resting ? 0.012 : 0.028) * rate;
  const lobes = Math.min(8, 3 + Math.floor(tps / 22));
  // The minimum-ripple floor breathes slowly on its own clock (`anim.breath`,
  // independent of `tps`) so a quiet-but-not-stalled tube stays visibly
  // alive rather than a dead flat ring — calmer than a busy one (the floor
  // does not grow with `active`), but never perfectly still.
  const breathe = 0.5 + 0.5 * Math.sin(anim.breath);
  // (#2877 pass 2, second look) `anim.bright` alone (0.22) measured only a
  // ~13% average-brightness drop against tools/prompt on the real
  // screenshots — additive "lighter" blending plus the glow's own bloom
  // keeps a thin bright line looking nearly as present as a thicker dim
  // one. Resting now also shrinks the ring itself (smaller amplitude, a
  // measured ~30% fewer lit pixels), so the dim reads as "a smaller, calmer
  // trace" and not just "the same trace, slightly faded".
  const restDamp = resting ? 0.55 : 1;
  const amp = R * (0.03 + 0.012 * breathe + 0.16 * active) * restDamp;
  const noise = stalled ? R * 0.004 : 0;

  ctx.globalCompositeOperation = "lighter";
  const passes: Array<{ w: number; a: number }> = [
    { w: 3, a: 0.22 },
    { w: 1.2, a: 0.88 },
  ];
  for (const p of passes) {
    ctx.beginPath();
    for (let i = 0; i <= 240; i++) {
      const t = (i / 240) * Math.PI * 2;
      const wave = Math.sin(lobes * t - anim.phase) + 0.12 * Math.sin((lobes * 2 + 1) * t + anim.phase * 1.7);
      const r = R + amp * wave + noise * (Math.random() - 0.5);
      const x = cx + Math.cos(t) * r;
      const y = cy + Math.sin(t) * r;
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.closePath();
    ctx.strokeStyle = `rgba(${cr},${cg},${cb},${p.a * anim.bright})`;
    ctx.lineWidth = p.w;
    ctx.shadowColor = `rgba(${cr},${cg},${cb},0.8)`;
    ctx.shadowBlur = p.w * 3 * anim.bright;
    ctx.stroke();
  }

  // Phosphor sweep — a bright leading dot travelling the ring, radar-trace
  // style. Its angular speed and brightness both follow the rate directly,
  // the clearest "busier" signal at a glance since it doesn't depend on
  // resolving individual wave peaks the way lobe density did.
  if (!stalled) {
    const sweepAngle = -anim.phase * 0.5;
    const sweepR = R + amp * Math.sin(lobes * sweepAngle - anim.phase);
    const sx = cx + Math.cos(sweepAngle) * sweepR;
    const sy = cy + Math.sin(sweepAngle) * sweepR;
    ctx.beginPath();
    ctx.arc(sx, sy, Math.max(1.4, R * 0.035), 0, Math.PI * 2);
    ctx.fillStyle = `rgba(${hr},${hg},${hb},${(0.5 + 0.45 * active) * anim.bright})`;
    ctx.shadowColor = `rgba(${cr},${cg},${cb},0.9)`;
    ctx.shadowBlur = 10 * anim.bright;
    ctx.fill();
  }
  ctx.shadowBlur = 0;
}

export function TokenScope({ tokensPerSec, stalled = false, resting = false, size, centerLabel, className, tone = "generating" }: TokenScopeProps) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const animRef = useRef<ScopeAnim>({ shown: 0, target: 0, phase: Math.random() * 6, bright: 1, breath: Math.random() * 6 });
  const targetRef = useRef({ target: 0, stalled: false, resting: false, rgb: PHOSPHOR_FALLBACK as Rgb });

  // Live values the rAF loop reads without needing to restart the effect
  // below on every rate update (a heartbeat every ~2s would otherwise tear
  // down and rebuild the canvas/observer/listener on that same cadence).
  // The trace takes the lit lamp's color, read from the same :root token.
  const rgb = useMemo(() => toneRgb(tone), [tone]);
  targetRef.current = { target: stalled ? 0 : Math.max(0, tokensPerSec ?? 0), stalled, resting, rgb };

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return undefined;
    const ctx = canvas.getContext("2d");
    if (!ctx) return undefined;

    function size2() {
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      const rect = canvas!.getBoundingClientRect();
      canvas!.width = Math.max(1, Math.round(rect.width * dpr));
      canvas!.height = Math.max(1, Math.round(rect.height * dpr));
      ctx!.setTransform(dpr, 0, 0, dpr, 0, 0);
    }
    size2();

    const ro = typeof ResizeObserver !== "undefined" ? new ResizeObserver(size2) : null;
    ro?.observe(canvas);

    const reduce = prefersReducedMotion();
    let rafId: number | null = null;
    let last = 0;

    function frame(now: number) {
      const w = canvas!.clientWidth;
      const h = canvas!.clientHeight;
      const dtSec = Math.min(0.05, Math.max(0, (now - last) / 1000)) || 0;
      last = now;
      const anim = animRef.current;
      const { target, resting: isResting } = targetRef.current;
      // Easing tuned so the ~2s heartbeat cadence reads as continuous
      // motion rather than a visible step every beat (ported verbatim from
      // the concept: k = 1 - e^(-dt*2.2)).
      const k = 1 - Math.exp(-dtSec * 2.2);
      anim.shown += (target - anim.shown) * k;
      // (#2877 pass 2) 0.22, not 0.35 — the operator's note was that the
      // existing dim wasn't legible; lower still reads the sweep/wave (never
      // fully dark) while being unmistakably dimmer than an active tube.
      anim.bright += ((isResting ? 0.22 : 1) - anim.bright) * k;
      if (w && h) drawFrame(ctx!, w, h, anim, targetRef.current.stalled, isResting, targetRef.current.rgb);
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

    if (reduce) {
      // Static readout: draw once at the target value, never loop.
      animRef.current.shown = targetRef.current.target;
      animRef.current.bright = targetRef.current.resting ? 0.22 : 1;
      const w = canvas.clientWidth;
      const h = canvas.clientHeight;
      if (w && h) drawFrame(ctx, w, h, animRef.current, targetRef.current.stalled, targetRef.current.resting, targetRef.current.rgb);
    } else {
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
    }

    return () => {
      ro?.disconnect();
    };
    // Intentionally NOT depending on tokensPerSec/stalled/resting — those
    // ride `targetRef` so a heartbeat's rate update never tears down and
    // re-creates the canvas/observer/listener.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const cls = ["token-scope-bezel", `token-scope-bezel--${size}`, className].filter(Boolean).join(" ");
  return (
    <div className={cls} data-tone={tone}>
      <div className="token-scope-screen">
        <canvas ref={canvasRef} aria-hidden="true" />
        {centerLabel != null && (
          <div className="token-scope-center">
            <span className="token-scope-n">{centerLabel}</span>
          </div>
        )}
      </div>
    </div>
  );
}
