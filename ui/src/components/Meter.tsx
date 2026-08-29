/**
 * (#2107, #1833) A generic 0-100% semicircle needle meter — CPU/GPU/MEM
 * utilization, anywhere a plain percent gauge is needed.
 *
 * This is a NEW, small component, not an extraction of
 * `lenses/machine/MachineHealthRegion.tsx`'s `<Gauge>` (the VRAM pressure
 * dial). That dial is genuinely bespoke to VRAM: stacked darkmux/other/
 * committed BANDS, hatched growth extension, a color ramp keyed to a
 * machine-specific scale (`resolveGaugeScale`), a seven-segment odometer
 * readout — none of which has a meaning for "what percent of the CPU is
 * busy". Forcing reuse would mean either dragging VRAM-shaped generality
 * into every CPU/GPU/MEM reading, or literally copying ~130 lines of
 * band/hatch/odometer SVG under a "shared" label that would immediately
 * drift the moment either dial changed. The one piece that IS genuinely
 * shared is the geometry primitive both dials reduce to — a percent maps
 * to an angle across a 180° sweep (`machineGauge.ts`'s own
 * `needleAngleDeg = pct * 1.8`, reproduced here as `angleForPct` so the
 * two dials draw from the same formula without either importing the
 * other's VRAM-specific code). `MachineHealthRegion.tsx` is untouched by
 * this file — its own goldens are unaffected.
 *
 * Three readings render on one arc: `now` (the needle), `avg` and `max`
 * (two static tick marks on the arc), plus the three numbers spelled out
 * underneath — the sighted glance and the numeric fallback answer the same
 * question two ways, same discipline `MachineHealthRegion`'s own aria rules
 * follow (color/position is never the ONLY channel).
 */

/** `pct * 1.8` — the same 180°-sweep angle formula
 * `lenses/machine/machineGauge.ts`'s `computeGaugeGeometry` uses
 * (`needleAngleDeg = pct * 1.8`), reproduced rather than imported so this
 * component has no dependency on that VRAM-specific module. */
function angleForPct(pct: number): number {
  return Math.max(0, Math.min(100, pct)) * 1.8;
}

const CX = 60;
const CY = 58;
const R = 46;
// A flat half-circle track, drawn once and shared by every tick/needle
// rotation below — same `pathLength=100` convention `MachineHealthRegion`'s
// own arc uses, so `stroke-dasharray`/`strokeDashoffset` percentages read
// the same way here.
const HALF_ARC_D = `M ${CX - R} ${CY} A ${R} ${R} 0 0 1 ${CX + R} ${CY}`;

export interface MeterProps {
  label: string;
  /** Latest reading, 0-100. `null` draws no needle (never measured yet). */
  now: number | null;
  /** Mean over the meter's window. `null` draws no avg mark. */
  avg: number | null;
  /** Max over the meter's window. `null` draws no max mark. */
  max: number | null;
  /** Shown under the numbers, e.g. "last 10 min" or "this mission". */
  scopeLabel?: string;
}

function fmtPct(v: number | null): string {
  return v === null ? "—" : `${Math.round(v)}%`;
}

/** One meter — CPU, GPU, or MEM. Pure presentational; the caller resolves
 * which window (mission/dispatch samples vs a rolling 10-minute tail) the
 * `now`/`avg`/`max` numbers describe. */
export function Meter({ label, now, avg, max, scopeLabel }: MeterProps) {
  const ariaLabel = [
    `${label}: `,
    now === null ? "no reading yet" : `now ${fmtPct(now)}`,
    avg === null ? "" : `, average ${fmtPct(avg)}`,
    max === null ? "" : `, max ${fmtPct(max)}`,
    scopeLabel ? `, over ${scopeLabel}` : "",
  ]
    .filter(Boolean)
    .join("");

  return (
    <div className="meter" data-meter={label.toLowerCase()}>
      <svg width="120" height="70" viewBox="0 0 120 70" role="img" aria-label={ariaLabel}>
        <path className="meter-track" d={HALF_ARC_D} fill="none" strokeWidth={7} pathLength={100} />
        {now !== null && (
          <path
            className="meter-fill"
            d={HALF_ARC_D}
            fill="none"
            strokeWidth={7}
            pathLength={100}
            strokeDasharray={`${Math.max(0, Math.min(100, now))} 100`}
          />
        )}
        {avg !== null && (
          <line
            className="meter-mark meter-mark-avg"
            x1={CX - R - 3}
            y1={CY}
            x2={CX - R + 3}
            y2={CY}
            transform={`rotate(${angleForPct(avg)} ${CX} ${CY})`}
          />
        )}
        {max !== null && (
          <line
            className="meter-mark meter-mark-max"
            x1={CX - R - 3}
            y1={CY}
            x2={CX - R + 3}
            y2={CY}
            transform={`rotate(${angleForPct(max)} ${CX} ${CY})`}
          />
        )}
        {now !== null && (
          <line
            className="meter-needle"
            x1={CX}
            y1={CY}
            x2={CX - R + 8}
            y2={CY}
            transform={`rotate(${angleForPct(now)} ${CX} ${CY})`}
          />
        )}
        <circle className="meter-hub" cx={CX} cy={CY} r={3} />
      </svg>
      <div className="meter-label">{label}</div>
      <div className="meter-numbers">
        <span className="meter-now">{fmtPct(now)}</span>
        <span className="meter-sep">·</span>
        <span className="meter-avg">{fmtPct(avg)} avg</span>
        <span className="meter-sep">·</span>
        <span className="meter-max">{fmtPct(max)} max</span>
      </div>
      {scopeLabel && <div className="meter-scope">{scopeLabel}</div>}
    </div>
  );
}
