/**
 * (#2107, #1833, extraction packet) The ONE semicircle needle gauge — the
 * shared core the machine lens's VRAM pressure dial (`MachineHealthRegion
 * .tsx`'s `Gauge`) and the CPU/GPU/MEM meters (`MachineDrawer.tsx`,
 * `MachineLens.tsx`'s live-load section) both render through.
 *
 * History: the first cut of this component (pre-extraction) was a second,
 * SMALLER gauge implementation living beside the VRAM dial's own — two
 * pieces of SVG geometry answering "draw a semicircle with a needle",
 * styled and maintained independently. The operator's own framing: "That
 * meter as a reuse component seems like a nice thing to have. Then its
 * style can be maintained in one place." A second component was the
 * opposite of that, so this packet deleted it and extracted the VRAM
 * dial's OWN geometry into this file instead — `MachineHealthRegion.tsx`'s
 * `Gauge` is now a thin caller that computes VRAM-specific data (through
 * the SAME unchanged pure functions in `machineGauge.ts`) and hands it to
 * `<Meter>`; every CPU/GPU/MEM caller hands it a much smaller prop set.
 *
 * **Literally one geometry, not two skins.** `CX`/`CY`/`R`/the arc path/
 * the tick positions/the needle length/the hub radius are the VRAM dial's
 * ORIGINAL numbers, unchanged, and every caller — VRAM included — draws
 * from them. What differs per instance is only the outer `width`/`height`
 * (SVG scales the same `viewBox` uniformly), which bands/ticks/redline/
 * gradient/center-content it supplies, and its own CSS-driven color
 * treatment via each band's own `className`. This is what makes the
 * extraction verifiable: `MachineHealthRegion.tsx`'s own DOM-structural
 * tests (exact class names, exact `transform`/`stroke`/`d` attribute
 * values) pass byte-identical against this file with ZERO changes to
 * those tests — the proof the move changed nothing for the VRAM caller.
 *
 * **The needle, arc/bands, and numerals are the shared core.** The dial
 * (needle + bands + track) is always drawn by this component; a numeral
 * READOUT is too — by DEFAULT the plain `now · avg avg · max max` row every
 * compact CPU/GPU/MEM meter needs (`numerals`/`label` props), rendered
 * below the dial inside this SAME component rather than duplicated by each
 * caller. VRAM's seven-segment odometer is the one thing that genuinely
 * ISN'T shared — a wholly different rendering technique (digit-cell
 * polygons, not text) — so it rides the `children` slot INSTEAD of
 * `numerals`/`label` (the two are mutually exclusive by convention: a
 * caller supplies one or the other, never both). Everything else VRAM-only
 * — the color-ramp `gradient`, `ticks` with drawn labels, the `scaleWord`
 * corner label, the `redline` threshold arc — is an optional prop a
 * compact caller simply omits.
 */
import type { ReactNode } from "react";

/** The dial's own local coordinate system, in `viewBox` units — the VRAM
 * dial's ORIGINAL numbers (`MachineHealthRegion.tsx`, pre-extraction),
 * unchanged. Exported because `MachineHealthRegion.tsx` still needs `CX`
 * for the odometer layout it passes as `children`. */
export const CX = 120;
export const CY = 120;
export const R = 86;

/** The half-circle track, drawn once and shared by every band/track path —
 * literally the VRAM dial's original constant. */
export const HALF_ARC_D = `M 34 120 A ${R} ${R} 0 0 1 206 120`;

/** Track/band stroke width, in viewBox units — the VRAM dial's original. */
const STROKE_W = 11;

/** The compact-meter render size (CPU/GPU/MEM, and #2108's CPU-cluster
 * tiles) — the SAME `viewBox` (`0 0 240 170`) as VRAM's 300×212, scaled
 * down so three (or, with clusters, several more) fit one row in the
 * ~340-360px content width the machine drawer/lens have at a 390px
 * viewport. Aspect-locked to the viewBox (240:170 ≈ 1.412) so nothing
 * distorts.
 *
 * Shrunk 112×79 → 100×70 in #2108 (operator finding, typography pass):
 * the `avg · max` line needs to stay on one line (`white-space: nowrap`)
 * at three tiles across on a 390px phone, and the instruction was
 * explicit — "if the grid cannot fit that at the current gauge width,
 * shrink the gauge, not the text." A few px narrower per tile buys the
 * row the margin `nowrap` needs. */
export const COMPACT_METER_WIDTH = 100;
export const COMPACT_METER_HEIGHT = 70;

export interface MeterGradient {
  id: string;
  stops: Array<{ offset: number | string; color: string }>;
}

export interface MeterBand {
  className: string;
  /** A literal CSS color, or `url(#<gradient.id>)` to paint from the
   * `gradient` prop. Omitted for a hatched band (its color comes from CSS
   * via `className` — matching VRAM's original `growth` band JSX, which
   * carried no `stroke` prop at all). */
  stroke?: string;
  lengthPct: number;
  /** Where along the arc this band starts, 0-100. Default 0. Ignored when
   * `hatchedDasharray` is set — the hatch pattern already encodes start. */
  startPct?: number;
  /** Precomputed via `hatchedSegmentDash(startPct, lengthPct)` — lives on
   * the band rather than being computed by `Meter` itself so this shared
   * component never has to import the VRAM-specific `machineGauge.ts`
   * (a layering inversion). Presence marks the band hatched. */
  hatchedDasharray?: string;
  /** Draw even when `lengthPct` is 0 — VRAM's darkmux band always drew,
   * empty or not, so an empty reading doesn't change the DOM shape. Every
   * other band (VRAM's `other`/`growth`, every CPU/GPU/MEM band) is
   * omitted entirely at `lengthPct <= 0`. */
  alwaysRender?: boolean;
}

export interface MeterTick {
  pct: number;
  className?: string;
  /** Present only for VRAM's hand-placed scale labels (`TICK_LABEL_XY`);
   * a compact meter's avg/max marks carry no label — the numbers are
   * spelled out in the `numerals` row instead. */
  label?: string;
  labelX?: number;
  labelY?: number;
}

/** The default, plain numeral readout every compact CPU/GPU/MEM meter
 * uses — `now`/`avg`/`max` (or `high`, the drawer's own vocabulary; the
 * label under each number is the caller's to name via `numeralLabels`). */
export interface MeterNumerals {
  now: number | null;
  avg: number | null;
  max: number | null;
}

export interface MeterProps {
  /** `"mm-gauge"` for every current caller — ONE wrapper class, so the
   * geometry-level CSS (track/needle/hub/tick color, font) lives in ONE
   * place regardless of which dial is rendering. A band's OWN color is
   * carried by that band's own `className`/`stroke`, never by this. */
  wrapperClassName: string;
  /** Rendered box size — the SAME `viewBox="0 0 240 170"` scales
   * uniformly to whatever `width`/`height` a caller asks for. Defaults to
   * the VRAM dial's own historical box (300×212) so that caller passes
   * neither and gets byte-identical output. */
  width?: number;
  height?: number;
  ariaLabel: string;
  gradient?: MeterGradient;
  bands: MeterBand[];
  ticks?: MeterTick[];
  scaleWord?: string;
  /** The threshold redline arc — VRAM only. Always drawn (never absent)
   * once this prop is present at all; `lit` toggles the `.lit` class,
   * matching the pre-extraction unconditional-path-with-toggled-class
   * shape exactly. A compact meter omits this prop and gets no redline
   * element at all. */
  redline?: { lit: boolean };
  /** Omit for "no reading yet" — draws no needle, matching this app's
   * absence-never-zero rule (a `0`-angle needle would assert a real
   * reading of 0%, not "unmeasured"). */
  needleAngleDeg?: number;
  /** A short label ABOVE the numeral row, e.g. "CPU" — compact meters
   * only; VRAM names its subject via `children`'s own text labels
   * instead. */
  label?: string;
  /** The default numeral readout, rendered BELOW the dial. Mutually
   * exclusive with `children` by convention — a caller supplies one or
   * the other. */
  numerals?: MeterNumerals;
  /** (#2108) Suppress the `avg · max` line even when `numerals` is
   * present — for a caller whose reading has no window-average concept
   * (a CPU cluster's own per-core %). Every existing caller omits this
   * and keeps the line, all-null included; see that line's own doc. */
  hideAvgMax?: boolean;
  /** Extra SVG content drawn last, before `</svg>` closes — VRAM's
   * odometer digit group + its two text labels. The one thing that isn't
   * shared core (see this module's own doc). */
  children?: ReactNode;
}

/** `%d%` — every caller (the dial's own numerals, VRAM's aria narrative
 * text elsewhere) rounds identically through this one function. `null`
 * stays `—`, never coerced to 0 (absence is a different claim). */
export function fmtPct(v: number | null): string {
  return v === null ? "—" : `${Math.round(v)}%`;
}

/** `pct * 1.8` — the dial's own 180°-sweep angle formula (matches
 * `machineGauge.ts`'s `computeGaugeGeometry`'s `needleAngleDeg = pct *
 * 1.8` exactly, since both draw on the SAME geometry this file now owns).
 * Clamped: a bad reading must not swing the needle past either end of the
 * arc. Exported for compact callers building their own `needleAngleDeg`
 * from a raw percent (VRAM's own `band.needleAngleDeg` already comes
 * pre-computed off `computeBandGeometry`, so `Gauge` doesn't need this). */
export function angleForPct(pct: number): number {
  return Math.max(0, Math.min(100, pct)) * 1.8;
}

/** A compact meter's single fill band — `now` percent, 0-100, clamped.
 * `null` (no reading yet) yields NO band at all, matching the same
 * absence-never-zero rule the needle already follows. */
export function simpleBand(className: string, stroke: string, now: number | null): MeterBand[] {
  if (now === null) return [];
  return [{ className, stroke, lengthPct: Math.max(0, Math.min(100, now)) }];
}

/** A compact meter's avg/max marks — small unlabeled ticks on the arc, the
 * SAME radial-mark geometry VRAM's own scale ticks use, just without a
 * drawn label (the numbers are spelled out in the `numerals` row
 * instead). Either or both may be absent. */
export function avgMaxTicks(avg: number | null, max: number | null): MeterTick[] {
  const out: MeterTick[] = [];
  if (avg !== null) out.push({ pct: Math.max(0, Math.min(100, avg)), className: "mm-gauge-tick mm-gauge-tick-avg" });
  if (max !== null) out.push({ pct: Math.max(0, Math.min(100, max)), className: "mm-gauge-tick mm-gauge-tick-max" });
  return out;
}

/** Bundles EVERYTHING a compact CPU/GPU/MEM caller needs into one prop
 * spread — `bands`/`ticks`/`needleAngleDeg`/`numerals`/`label` — so the
 * "how do I feed a plain now/avg/max reading into `<Meter>`" logic lives
 * in exactly ONE place rather than being re-derived at each of the two
 * call sites (`MachineDrawer.tsx`, `MachineLens.tsx`'s live-load section).
 * `className`/`stroke` are the fill band's own CSS treatment — every
 * current caller passes the SAME `"mm-gauge-fill-compact"` / accent color,
 * kept as parameters rather than hardcoded here in case a future caller
 * genuinely needs its own color. */
export function compactMeterProps(
  label: string,
  className: string,
  stroke: string,
  m: { now: number | null; avg: number | null; high: number | null },
): Pick<MeterProps, "label" | "bands" | "ticks" | "needleAngleDeg" | "numerals"> {
  return {
    label,
    bands: simpleBand(className, stroke, m.now),
    ticks: avgMaxTicks(m.avg, m.high),
    needleAngleDeg: m.now == null ? undefined : angleForPct(m.now),
    numerals: { now: m.now, avg: m.avg, max: m.high },
  };
}

export function Meter({
  wrapperClassName,
  width = 300,
  height = 212,
  ariaLabel,
  gradient,
  bands,
  ticks = [],
  scaleWord,
  redline,
  needleAngleDeg,
  label,
  numerals,
  hideAvgMax,
  children,
}: MeterProps) {
  return (
    <div className={wrapperClassName} data-meter={label ? label.toLowerCase() : undefined}>
      <svg width={width} height={height} viewBox="0 0 240 170" role="img" aria-label={ariaLabel}>
        {gradient && (
          <defs>
            <linearGradient id={gradient.id} gradientUnits="userSpaceOnUse" x1={CX - R} y1={0} x2={CX + R} y2={0}>
              {gradient.stops.map((s) => (
                <stop key={s.offset} offset={s.offset} stopColor={s.color} />
              ))}
            </linearGradient>
          </defs>
        )}
        <path className="mm-gauge-track" d={HALF_ARC_D} fill="none" strokeWidth={STROKE_W} pathLength={100} />
        {bands
          .filter((b) => b.alwaysRender || b.lengthPct > 0)
          .map((b) => (
            <path
              key={b.className}
              className={b.className}
              stroke={b.stroke}
              d={HALF_ARC_D}
              fill="none"
              strokeWidth={STROKE_W}
              pathLength={100}
              strokeDasharray={b.hatchedDasharray ?? `${b.lengthPct} 100`}
              strokeDashoffset={b.hatchedDasharray ? undefined : b.startPct ? -b.startPct : undefined}
            />
          ))}
        {ticks.map((t, i) => (
          <line
            // Index, not `t.pct` — a compact meter's avg/max ticks can
            // legitimately land on the SAME percent (e.g. a single-sample
            // window, where avg === max), which collided as a duplicate
            // React key when keyed by value alone.
            key={i}
            className={t.className ?? "mm-gauge-tick"}
            x1={30}
            y1={CY}
            x2={38}
            y2={CY}
            transform={`rotate(${t.pct * 1.8} ${CX} ${CY})`}
          />
        ))}
        {ticks
          .map((t, i) => ({ t, i }))
          .filter(({ t }) => t.label != null && t.labelX != null && t.labelY != null)
          .map(({ t, i }) => (
            <text key={i} className="mm-gauge-scale-label" x={t.labelX} y={t.labelY} textAnchor="middle">
              {t.label}
            </text>
          ))}
        {scaleWord && (
          <text className="mm-gauge-scale-word" x={220} y={146} textAnchor="middle">
            {scaleWord}
          </text>
        )}
        {redline && (
          <path
            className={`mm-gauge-redline${redline.lit ? " lit" : ""}`}
            d={HALF_ARC_D}
            fill="none"
            strokeWidth={STROKE_W}
            pathLength={100}
            strokeDasharray="2.5 100"
            strokeDashoffset="-97.5"
          />
        )}
        {needleAngleDeg != null && (
          <line className="mm-gauge-needle" x1={CX} y1={CY} x2={42} y2={CY} transform={`rotate(${needleAngleDeg} ${CX} ${CY})`} />
        )}
        <circle className="mm-gauge-hub" cx={CX} cy={CY} r={5} />
        {children}
      </svg>
      {/* (#2108, operator finding — typography pass) Label + the CURRENT
          value share ONE row directly under the gauge ("CPU 17%"), a few
          px below the arc — not the ~50px gap the previous stacked
          label-then-numerals layout left (the arc's own drawn track ends
          well above the SVG's bottom edge; `.meter-caption`'s negative
          `margin-top`, in `styles.css`, pulls the row up into that empty
          space rather than reserving it as visual gap). `.meter-label` and
          `.meter-now` keep their OWN pre-existing class names — only the
          wrapping row changed — so every existing query against either
          class (`.meter-now`'s textContent, `getByText("CPU")`, …) still
          resolves the same node it always did. */}
      {(label || numerals) && (
        <div className="meter-caption">
          {label && <div className="meter-label">{label}</div>}
          {numerals && <div className="meter-now">{fmtPct(numerals.now)}</div>}
        </div>
      )}
      {/* (phone feedback, 2026-08-29; nowrap restored #2108) `avg · max`
          on its OWN short line below the caption row. `white-space:
          nowrap` + `font-variant-numeric: tabular-nums` (styles.css) keep
          it from ever wrapping — the 2026-08-29 fix that dropped `nowrap`
          traded a wrap for an overflow collision; #2108 fixes the actual
          cause (the compact gauge/tile were too wide for three side by
          side on a 390px phone) by SHRINKING the gauge
          (`COMPACT_METER_WIDTH`/`COMPACT_METER_HEIGHT`) rather than
          re-introducing wrapping text. `hideAvgMax` (#2108, new — CPU
          clusters) opts a caller with no such window concept out of the
          line entirely, rather than rendering a bare "— avg · — max" no
          caller wants; every EXISTING numerals caller (CPU/GPU/MEM) omits
          the prop and keeps this line exactly as before, including the
          all-null "— avg · — max" case. */}
      {numerals && !hideAvgMax && (
        <div className="meter-avgmax">
          {fmtPct(numerals.avg)} avg <span className="meter-sep">·</span> {fmtPct(numerals.max)} max
        </div>
      )}
    </div>
  );
}
