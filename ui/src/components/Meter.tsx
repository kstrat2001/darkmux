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
  /** (#2122) Per-metric override for where a `banded` band (and the
   * caption numeral) turn amber/red. Defaults to `DEFAULT_WARN_AT`/
   * `DEFAULT_CRITICAL_AT` (80/95, CPU/GPU's own numbers) — MEM passes a
   * gentler 90/97 so its own gauge doesn't fire on the same margin the
   * VRAM pressure ledger already alarms on (see `MachineLens.tsx`'s call
   * site for the full reasoning: the ledger's kernel-pressure trigger
   * stays the authority for memory red, this is a softer heads-up). A
   * caller with no `banded` bands (VRAM, any future non-compact use)
   * never reads these — nothing to threshold. */
  warnAt?: number;
  criticalAt?: number;
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

/** (#2122) `quiet` / `warn` / `critical` — the three-band severity a
 * compact gauge's fill and caption both key on. `null` (no reading yet)
 * always reads `quiet`: an unmeasured gauge has nothing to alarm about,
 * matching this file's absence-never-zero convention everywhere else. */
export type MeterBandLevel = "quiet" | "warn" | "critical";

/** Default thresholds — CPU/GPU and the mockup's own numbers (#2122's
 * ask): quiet below 80, warning from 80, critical from 95. Matches the
 * host sampler's own `above_80_ms` reduction threshold, so the gauge's
 * color band and the "time above 80%" figure elsewhere on the panel never
 * disagree about where "elevated" starts. */
export const DEFAULT_WARN_AT = 80;
export const DEFAULT_CRITICAL_AT = 95;

/** (#2122) MEM's own gentler pair — the compact MEM gauge draws from raw
 * `mem_pct` (system memory in use), a DIFFERENT signal than the VRAM
 * pressure ledger's `pressure.margin_percent` (kernel headroom,
 * `machineGauge.ts`'s own note: "the only figure that can trigger RED").
 * The ledger stays the authority for a genuine memory alarm; this gauge's
 * job is a quieter heads-up on raw usage, so it gets a higher bar (90/97
 * instead of the 80/95 default) rather than co-firing on the same margin
 * the ledger already covers — two reds for one condition reads as a
 * louder alarm than the data supports. */
export const MEM_WARN_AT = 90;
export const MEM_CRITICAL_AT = 97;

/** Pure threshold lookup — `warnAt`/`criticalAt` are per-metric props
 * (MEM wants 90/97, see `MachineLens.tsx`'s own note on why) rather than
 * hardcoded here, so this stays one function for every caller. `>=` at
 * each edge: a reading AT the threshold is already the next band up,
 * matching the thermal pill's own `>= 80` / `>= 95` wording (#2122).
 * Still used for the compact dials' PERCENTAGE TEXT color (`nowLevelCls`,
 * below) — the arc itself no longer uses this at all, see `gradientBand`'s
 * own doc for why.
 *
 * **History: a `lowIsBad` 4th parameter lived here briefly** (#2821
 * review item 5, for the battery bar's own low-charge severity) and was
 * removed again in the SAME arc of work once the battery switched to a
 * reversed color ramp (item 2, "gradient... reversed") that carries the
 * same meaning continuously — a discrete low/high threshold on TOP of a
 * ramp that already reddens toward empty would have doubled up on the one
 * thing this whole pass exists to stop doing. Mentioned here so a future
 * reader who finds this comment via `git blame` isn't left guessing why a
 * parameter came and went in what looks like one feature. */
export function meterBandLevel(now: number | null, warnAt: number, criticalAt: number): MeterBandLevel {
  if (now === null) return "quiet";
  if (now >= criticalAt) return "critical";
  if (now >= warnAt) return "warn";
  return "quiet";
}

/** The CSS class a `banded` band's SVG path (and the caption's
 * `.meter-now`) picks up for a given level — `""` for `quiet` leaves the
 * element's existing class/color untouched entirely (the caller's own
 * accent stroke, `--fg` for the numeral), so a never-loaded gauge is
 * byte-identical to before this packet. */
export function bandLevelClass(level: MeterBandLevel): string {
  if (level === "warn") return "mm-band-warn";
  if (level === "critical") return "mm-band-critical";
  return "";
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

/** (gradient-everywhere pass) The dial fill's color ramp stops — `--good`,
 * `--warn`, `--bad` from `styles.css`, duplicated here as literals because
 * an SVG `stroke`/gradient `stop-color` has to be a concrete value and
 * reading a CSS custom property at render time would mean a
 * `getComputedStyle` call per frame.
 *
 * THREE stops, not two, and that is not a flourish. Interpolating this
 * palette's green (`#4ade80`) straight to its red (`#f56565`) passes
 * through `#9fa172` — a muddy olive, because both endpoints are pastels
 * carrying a lot of blue. Routing through the palette's own amber puts a
 * real yellow at the midpoint AND keeps every color the dial can show
 * inside the vocabulary the rest of the page already uses.
 *
 * This constant, `mixHex`, `gaugeFillColor` and `gaugeRampStops` below
 * used to live in `lenses/machine/machineGauge.ts`, built for the big
 * "MACHINE USED" gauge alone. Moved HERE (operator: "give every small
 * meter the same gradient treatment the big memory gauge uses... reuse
 * THAT mechanism") because every compact `<Meter>` — CPU/GPU/MEM,
 * CPU-cluster tiles, the remote-machine fallback dials — now paints from
 * the SAME ramp, and `Meter.tsx` (not a lens file) is the shared home
 * every one of those callers already imports from. `machineGauge.ts`
 * re-exports `gaugeFillColor`/`gaugeRampStops` unchanged so its own
 * callers and tests kept working without edits. */
const FILL_STOPS = ["#4ade80", "#f0b429", "#f56565"] as const;

function mixHex(a: string, b: string, k: number): string {
  const ch = (h: string, i: number) => parseInt(h.slice(1 + i * 2, 3 + i * 2), 16);
  const out = [0, 1, 2].map((i) => Math.round(ch(a, i) + (ch(b, i) - ch(a, i)) * k));
  return `#${out.map((n) => n.toString(16).padStart(2, "0")).join("")}`;
}

/** (operator request) The fill color as a CONTINUOUS function of how full
 * a 0-100 reading is: pure green at 0%, the palette's amber at 50%, pure
 * red at 100%. Every dial's ramp (the big gauge AND every compact meter)
 * draws from this one function — a gauge at 84% and one at 86% are not
 * different in kind, and painting them different colors would assert that
 * they were; a continuous ramp asserts nothing, it just maps the ratio
 * already readable off the fill/needle onto a hue.
 *
 * Clamps rather than extrapolating: a reading past 100 is drawn at the red
 * end, not at some color beyond red — and that includes `+Infinity`. Only
 * `NaN` (no figure at all) falls to the green end, and it is the caller's
 * job not to draw a band for a figure it does not have. */
export function gaugeFillColor(pct: number): string {
  const t = (Number.isNaN(pct) ? 0 : Math.max(0, Math.min(100, Number(pct)))) / 100;
  return t < 0.5
    ? mixHex(FILL_STOPS[0], FILL_STOPS[1], t * 2)
    : mixHex(FILL_STOPS[1], FILL_STOPS[2], (t - 0.5) * 2);
}

/** The ramp painted ACROSS THE ARC'S SWEEP — green at 0, red at the scale's
 * end — so a band takes its color from WHERE IT SITS on the dial rather
 * than from a figure computed about it. The needle's position (or, for a
 * compact meter, the fill's own leading edge) and the color under it then
 * carry the same information.
 *
 * Returned as `{offset, color}` stops for an SVG `linearGradient` laid
 * horizontally across the arc's bounding box.
 *
 * **The offsets are cosine-spaced, not linear, and that is the whole
 * subtlety.** A horizontal gradient interpolates along X, while the arc
 * advances by ANGLE; for a semicircle the two are related by
 * `x = cx − r·cos(pct·π)`, so evenly-spaced colors in X would bunch
 * visibly wrong against the tick marks — the 50% stop would not land at
 * the top of the dial. Placing each stop at its own
 * `(1 − cos(pct·π)) / 2` makes the gradient track the arc exactly, so the
 * color at any tick is the color that tick's percentage maps to. Every
 * `<Meter>` — big or compact — draws the SAME half-circle arc geometry
 * (`HALF_ARC_D`, this file's own doc), so this one spacing rule is correct
 * for all of them; a compact meter needs no linear variant.
 *
 * 24 segments is a legibility choice, not a precision one: the ramp is
 * piecewise-linear between stops and the eye cannot resolve the banding
 * past roughly this density at the dial's rendered size. */
export function gaugeRampStops(segments = 24): { offset: number; color: string }[] {
  return Array.from({ length: segments + 1 }, (_, i) => {
    const pct = i / segments;
    return { offset: (1 - Math.cos(pct * Math.PI)) / 2, color: gaugeFillColor(pct * 100) };
  });
}

/** The one gradient id every compact meter's own `<linearGradient>` uses.
 * Reused verbatim across every simultaneously-rendered compact `<Meter>`
 * (CPU/GPU/MEM, several CPU-cluster tiles, the remote-machine fallback
 * dials can all be on screen together) — safe because each `<Meter>`
 * renders its OWN `<svg>` root with its OWN `<defs>`, and an SVG
 * `url(#id)` paint reference resolves within that same `<svg>` fragment,
 * not globally across the DOM (the same assumption the big gauge's fixed
 * `RAMP_ID` already relies on, just exercised here with many instances at
 * once instead of one). */
export const COMPACT_RAMP_ID = "mm-compact-ramp";

/** A compact meter's single fill band — `now` percent, 0-100, clamped,
 * painted from the shared ramp (`COMPACT_RAMP_ID`) rather than a literal
 * color. `null` (no reading yet) yields NO band at all, matching the same
 * absence-never-zero rule the needle already follows.
 *
 * **Replaces `simpleBand` (deleted) and the `banded`/threshold-override
 * mechanism it opted into** (operator: "give every small meter the same
 * gradient treatment the big gauge uses... the fill should reveal the
 * gradient up to the value"). The fill's own color now depends on WHERE
 * its leading edge sits on the ramp — a gauge at 10% shows only green, one
 * at 90% reaches into red — so a SEPARATE class-driven flat-color override
 * at a warn/critical threshold would have fought the gradient it sits on
 * top of (the exact "doubling up" the operator flagged). The PERCENTAGE
 * TEXT keeps its own threshold-based severity color unchanged
 * (`nowLevelCls`, computed independently of `bands`) — that signal is
 * still real and non-redundant with the ramp. */
export function gradientBand(className: string, now: number | null): MeterBand[] {
  if (now === null) return [];
  return [{ className, stroke: `url(#${COMPACT_RAMP_ID})`, lengthPct: Math.max(0, Math.min(100, now)) }];
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
 * spread — `gradient`/`bands`/`ticks`/`needleAngleDeg`/`numerals`/`label`
 * — so the "how do I feed a plain now/avg/max reading into `<Meter>`"
 * logic lives in exactly ONE place rather than being re-derived at each
 * call site (`MachineDrawer.tsx`/`machineStatsContent.tsx`'s live-load
 * section, `MachineLens.tsx`'s remote-machine fallback). `className` is
 * the fill band's own `className` (every current caller passes
 * `"mm-gauge-fill-compact"`) — kept as a parameter rather than hardcoded
 * in case a future caller genuinely needs its own.
 *
 * **No `stroke` parameter any more** (gradient-everywhere pass): every
 * caller used to pass the SAME literal `"var(--accent, var(--good))"`
 * here, which this function is what turned into a real color via
 * `simpleBand`. Now the fill paints from the shared ramp
 * (`gradientBand`/`COMPACT_RAMP_ID`) instead, so there is no literal color
 * left for a caller to supply — passing one would be dead input. */
export function compactMeterProps(
  label: string,
  className: string,
  m: { now: number | null; avg: number | null; high: number | null },
): Pick<MeterProps, "label" | "gradient" | "bands" | "ticks" | "needleAngleDeg" | "numerals"> {
  return {
    label,
    gradient: { id: COMPACT_RAMP_ID, stops: gaugeRampStops() },
    bands: gradientBand(className, m.now),
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
  warnAt = DEFAULT_WARN_AT,
  criticalAt = DEFAULT_CRITICAL_AT,
  children,
}: MeterProps) {
  // (#2122) The caption numeral colors itself off its OWN value — every
  // `banded` band's `lengthPct` is drawn from the same reading, so the two
  // never disagree, but computing this independently means a caller with
  // `numerals` and no `banded` band (none exist today) simply gets no
  // caption color rather than crashing on a band lookup.
  const nowLevelCls = numerals ? bandLevelClass(meterBandLevel(numerals.now, warnAt, criticalAt)) : "";
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
            // (gradient-everywhere pass) Every band paints from its OWN
            // `stroke` — a literal color, a hatch pattern via `className`,
            // or (every compact-meter fill, via `gradientBand`) a
            // `url(#COMPACT_RAMP_ID)` reference into the ramp `<defs>`
            // above. No class-driven threshold override reaches the arc
            // any more (`.mm-band-warn`/`.mm-band-critical`'s STROKE rules
            // are retired with `simpleBand`/`banded` — see `gradientBand`'s
            // own doc for why); the percentage TEXT still gets its own
            // threshold color independently, below.
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
          {numerals && (
            <div className={nowLevelCls ? `meter-now ${nowLevelCls}` : "meter-now"}>{fmtPct(numerals.now)}</div>
          )}
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
