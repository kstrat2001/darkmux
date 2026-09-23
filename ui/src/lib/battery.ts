/**
 * (#2821) Pure battery-surface logic for the machine LENS ONLY — no DOM, so
 * every rule here is unit-testable without mounting anything, matching this
 * app's `machineGauge.ts` convention of keeping number-crunching out of the
 * component that renders it.
 *
 * Scope note (operator, 2026-09-23): battery surfaces are the live machine
 * lens ONLY — not the machine drawer, not fleet cards. Nothing here is
 * wired into `machineStatsContent.tsx`'s shared `body`/`HostExtras`; it is
 * consumed only from `MachineLens.tsx`.
 */
import type { BatteryHealth, BatterySample } from "../types/handwritten";

/** `Nh Mm` from a minute count — the same coarse shape `relAgoFrom` uses
 * for its own hour bucket, just without the "ago" framing (this is a
 * forward estimate, not a past timestamp). */
function fmtHm(totalMinutes: number): string {
  const h = Math.floor(totalMinutes / 60);
  const m = totalMinutes % 60;
  if (h <= 0) return `${m} min`;
  return `${h} h ${m} m`;
}

/** The one-line state under the charge gauge: on AC, charging, a time
 * estimate, or nothing extra when discharging with no estimate yet — never
 * a fabricated "0 min left" (the estimator's absence-never-zero rule
 * already governs `minutes_to_empty` on the wire; this just renders what
 * arrives). `null` sample (no battery) renders no caption at all. */
export function chargeCaption(b: BatterySample | null): string {
  if (b === null) return "";
  if (b.on_ac) return b.charging ? "charging" : "on AC";
  if (b.minutes_to_empty != null) return `${fmtHm(b.minutes_to_empty)} left`;
  return "on battery";
}

/** "battery 100%, on AC" / "battery 35%, 2 h 10 m left" / "battery
 * unmeasured" — the accessible name for the battery bar glyph (`role="img"`
 * on its `<svg>`, matching every other compact gauge on this page). Built
 * from the same `chargeCaption` every sighted reader sees, so the two
 * channels never disagree. `null` sample renders the caller's own absent
 * case — this function is never called for one (`BatteryLensBlock` returns
 * early), but is total anyway rather than partial. */
export function batteryAriaLabel(sample: BatterySample | null): string {
  if (sample === null) return "battery unmeasured";
  const pct = sample.charge_pct == null ? "unmeasured" : `${sample.charge_pct}%`;
  const cap = chargeCaption(sample);
  return `battery ${pct}${cap ? `, ${cap}` : ""}`;
}

/** The battery bar's fill width, in the SAME units as `maxWidth` (the
 * glyph's own inner fillable width), clamped 0-100% first so a
 * momentarily-over-100 gauge reading (the same post-full-charge overshoot
 * `charge_pct_from` already clamps server-side) can never draw past the
 * glyph's own body. `null` when unmeasured — the caller draws no fill rect
 * at all, the same absence-never-zero rule `simpleBand` follows for the
 * CPU/GPU/MEM dials. */
export function batteryFillWidth(chargePct: number | null, maxWidth: number): number | null {
  if (chargePct === null) return null;
  const clamped = Math.max(0, Math.min(100, chargePct));
  return (clamped / 100) * maxWidth;
}

/** The health row's condition value + whether it should render in the warn
 * tone. Prefers the COMPUTED `condition_word` — derived server-side from
 * `health_condition` (the authoritative `BatteryHealthCondition` signal),
 * with `permanent_failure_status` acting only as a failure override; see
 * `BatteryHealth`'s own doc for the corrected derivation and the
 * measurement backing it (the reference machine's raw `condition` string
 * read "Check Battery" while `health_condition`/`condition_word` and
 * `system_profiler` agreed "Normal"). `warn` fires for ANY non-"Normal"
 * value, including a verbatim passthrough like "Service Recommended" —
 * never just a fixed two-word enum. Falls back to labeling the raw
 * `condition` string PRECISELY (never as "the" condition) only when no
 * computed word is available at all. */
export function conditionRow(h: BatteryHealth | null): { value: string; warn: boolean } | null {
  if (h === null) return null;
  if (h.condition_word != null) {
    return { value: h.condition_word, warn: h.condition_word !== "Normal" };
  }
  if (h.condition != null) {
    // No computed verdict to trust yet (older daemon) — name the raw
    // source explicitly rather than presenting an unlabeled word as fact.
    return { value: `power source reports: ${h.condition}`, warn: false };
  }
  return null;
}

/** The battery-lens "charge capacity" row's value + a `title` disclosure —
 * split apart (#2821 review, item 3) because the ORIGINAL one-line form
 * ("5,701 of 6,249 mAh design (91.2% raw · 93.7% nominal)") reads too much
 * like macOS's own single "Maximum Capacity: 95%" figure, just with more
 * digits. Neither ratio recorded here reproduces that figure (measured on
 * the reference machine: raw 91.2%, nominal 93.7%, macOS's own figure
 * 95%), so presenting two percentages side by side invited exactly the
 * conflation the label change fixes.
 *
 * The visible `value` now carries ONLY the raw reading — `N of M mAh
 * (raw)`, parenthetical source named inline rather than implied — and the
 * nominal reading moves into `title` (a hover/long-press disclosure,
 * `Kv`'s own new prop), worded as a disclaimer rather than a second
 * headline number. `null` when the mAh pair itself is unavailable. */
export interface CapacityDisplay {
  value: string;
  title: string | null;
}

export function capacityLine(h: BatteryHealth | null): CapacityDisplay | null {
  if (h === null) return null;
  const design = h.design_capacity_mah;
  const raw = h.raw_max_capacity_mah;
  if (design == null || raw == null) return null;
  const rawPctClause = h.raw_capacity_pct != null ? `, ${h.raw_capacity_pct}%` : "";
  const value = `${raw.toLocaleString()} of ${design.toLocaleString()} mAh (raw${rawPctClause})`;

  const nominalParts: string[] = [];
  if (h.nominal_charge_capacity_mah != null) {
    nominalParts.push(`${h.nominal_charge_capacity_mah.toLocaleString()} mAh nominal`);
  }
  if (h.nominal_capacity_pct != null) nominalParts.push(`${h.nominal_capacity_pct}%`);
  const title =
    nominalParts.length > 0
      ? `Nominal reading: ${nominalParts.join(", ")}. Neither this nor the raw figure above is macOS's own "Maximum Capacity" percentage — that figure uses an undocumented Apple formula and reproduces from neither.`
      : `Not macOS's own "Maximum Capacity" percentage — that figure uses an undocumented Apple formula.`;
  return { value, title };
}

/** `5,368 h` — the lifetime cross-check total, formatted with the same
 * thousands separator the mAh figures use. `null` renders as `null`
 * (caller decides whether to hide the row). */
export function fmtOperatingHours(hours: number | null): string | null {
  if (hours == null) return null;
  return `${hours.toLocaleString()} h`;
}
