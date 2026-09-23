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

/** The health row's condition value + whether it should render in the warn
 * tone. Prefers the COMPUTED `condition_word` (Normal/Service Battery,
 * derived from `permanent_failure_status`) over the raw, unreliable IOKit
 * `condition` string — see `BatteryHealth`'s own doc for the measurement
 * this is based on (the reference machine's raw string read "Check
 * Battery" while `condition_word` and macOS's own Settings/system_profiler
 * agreed "Normal"). Falls back to labeling the raw string PRECISELY (never
 * as "the" condition) only when no computed word is available at all. */
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

/** `5,701 of 6,249 mAh design (91.2% raw · 93.7% nominal)` — both capacity
 * ratios, always both, never relabeled "Maximum Capacity": measured
 * against the reference machine, neither ratio reproduces macOS's own
 * "Maximum Capacity" figure (system_profiler showed 95% where raw read
 * 91.2% and nominal read 93.7%), so claiming parity with that figure would
 * be a fabrication. `null` when the mAh pair itself is unavailable. */
export function capacityLine(h: BatteryHealth | null): string | null {
  if (h === null) return null;
  const design = h.design_capacity_mah;
  const raw = h.raw_max_capacity_mah;
  if (design == null || raw == null) return null;
  const parts: string[] = [];
  if (h.raw_capacity_pct != null) parts.push(`${h.raw_capacity_pct}% raw`);
  if (h.nominal_capacity_pct != null) parts.push(`${h.nominal_capacity_pct}% nominal`);
  const pctClause = parts.length > 0 ? ` (${parts.join(" · ")})` : "";
  return `${raw.toLocaleString()} of ${design.toLocaleString()} mAh design${pctClause}`;
}

/** One scaled bar for the time-at-charge histogram — height is a PERCENT
 * of the tallest bucket (never a raw hour count as a bar length, which
 * would make the whole chart unreadable whenever one bucket dominates).
 * `null` heights are impossible here: every element of `time_at_soc_hours`
 * is a `u32` on the wire (`parse_time_at_soc`'s own doc), never absent
 * individually — only the whole array is optional. */
export interface SocBar {
  index: number;
  hours: number;
  pct: number;
}

/** Scales `time_at_soc_hours` to a 0-100 bar-height percent per bucket.
 * `null`/empty input yields `[]` — the caller renders no chart rather than
 * an empty axis. */
export function socHistogramBars(hours: number[] | null): SocBar[] {
  if (hours == null || hours.length === 0) return [];
  const max = Math.max(...hours);
  if (max <= 0) return hours.map((h, i) => ({ index: i, hours: h, pct: 0 }));
  return hours.map((h, i) => ({ index: i, hours: h, pct: (h / max) * 100 }));
}

/** `5,368 h` — the lifetime cross-check total, formatted with the same
 * thousands separator the mAh figures use. `null` renders as `null`
 * (caller decides whether to hide the row). */
export function fmtOperatingHours(hours: number | null): string | null {
  if (hours == null) return null;
  return `${hours.toLocaleString()} h`;
}
