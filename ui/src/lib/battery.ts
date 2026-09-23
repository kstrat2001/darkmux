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
import { gaugeFillColor } from "../components/Meter";

/** `Nh Mm` from a minute count — the same coarse shape `relAgoFrom` uses
 * for its own hour bucket, just without the "ago" framing (this is a
 * forward estimate, not a past timestamp). */
function fmtHm(totalMinutes: number): string {
  const h = Math.floor(totalMinutes / 60);
  const m = totalMinutes % 60;
  if (h <= 0) return `${m} min`;
  return `${h} h ${m} m`;
}

/** (operator, 2026-09-24: "'on AC' is unlike a lot of meters these days...
 * a lightning bolt icon works inside the battery") The glyph drawn INSIDE
 * the battery body, replacing the old plain-text "on AC"/"charging"
 * caption:
 * - `"bolt"` — current is actually flowing in (`charging`).
 * - `"plug"` — connected to power but NOT charging (topped off/held at
 *   100%, `on_ac && !charging` — the reference machine's own steady
 *   state). A bolt here would claim current is flowing when it isn't;
 *   this is the same distinction macOS's own menu-bar battery icon draws
 *   between "charging" and "power-connected, full".
 * - `null` — discharging (`!on_ac`). No icon; the time-remaining estimate
 *   (`batteryTimeLeftText`) is the information that state actually has to
 *   show, and an icon can't carry a number. */
export type BatteryIconKind = "bolt" | "plug" | null;

export function batteryIcon(b: BatterySample): BatteryIconKind {
  if (b.charging) return "bolt";
  if (b.on_ac) return "plug";
  return null;
}

/** The state fragment for the accessible name — three shapes, matching the
 * icon states above one-for-one (plus the "on battery" case a bolt/plug
 * icon never draws): `"charging"`, `"on AC, not charging"`, or
 * `"on battery"` (with `, H h M m left` appended when an estimate exists).
 * Never a fabricated "0 min left" — the estimator's absence-never-zero
 * rule already governs `minutes_to_empty` on the wire; this just states
 * what arrives, in words. */
export function batteryStateText(b: BatterySample): string {
  if (b.charging) return "charging";
  if (b.on_ac) return "on AC, not charging";
  const time = b.minutes_to_empty != null ? `, ${fmtHm(b.minutes_to_empty)} left` : "";
  return `on battery${time}`;
}

/** The ONLY visible text the battery meter still carries for its power
 * state (operator: "keep the time-remaining text... only when discharging
 * with an estimate — that's information an icon can't carry"). Neutral
 * `null` for every other state: charging/on-AC now speak entirely through
 * `batteryIcon`'s glyph, and a discharging reading with no estimate yet
 * has nothing honest to print (never a fabricated "0 min left" placeholder
 * where the OS declined to estimate). */
export function batteryTimeLeftText(b: BatterySample | null): string | null {
  if (b === null || b.on_ac || b.minutes_to_empty == null) return null;
  return `${fmtHm(b.minutes_to_empty)} left`;
}

/** "battery 100%, on AC, not charging" / "battery 62%, charging" /
 * "battery 35%, on battery, 2 h 10 m left" / "battery unmeasured" — the
 * accessible name for the battery meter's `<svg role="img">`. States the
 * SAME facts the icon + time text carry visually, in words, so a
 * screen-reader user loses nothing the icon-only sighted presentation
 * shows. `null` sample renders the caller's own absent case — this
 * function is never called for one (`BatteryLensBlock` returns early), but
 * is total anyway rather than partial. */
export function batteryAriaLabel(sample: BatterySample | null): string {
  if (sample === null) return "battery unmeasured";
  const pct = sample.charge_pct == null ? "unmeasured" : `${sample.charge_pct}%`;
  return `battery ${pct}, ${batteryStateText(sample)}`;
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

/** The gradient id the battery bar's own `<linearGradient>` uses — a
 * separate constant from `COMPACT_RAMP_ID` (Meter.tsx) even though only
 * one `BatteryBar` ever renders per page (so reuse-safety across multiple
 * instances is moot here) — named for what it is, not borrowed just
 * because collision isn't a risk. */
export const BATTERY_RAMP_ID = "mm-battery-ramp";

/** (operator, 2026-09-24, reversing an earlier "no gradient" amendment:
 * "the solid meters do not indicate when things are getting tight... give
 * every small meter the same gradient treatment the big gauge uses") The
 * battery's OWN ramp — the SAME three-stop palette every other gauge on
 * this page draws from (`gaugeFillColor`, `components/Meter.tsx`), but
 * REVERSED: red at the EMPTY end (offset 0%), green at the FULL end
 * (offset 100%) — "empty is bad, full is good," the opposite direction of
 * every "high is bad" CPU/GPU/MEM dial, matching the battery's own
 * `lowIsBad` semantics from a discrete-threshold era of this same file
 * without needing a discrete threshold: the fill simply reveals less of
 * the ramp's green end the lower the charge.
 *
 * Linear, not cosine-spaced: `gaugeRampStops` (Meter.tsx) cosine-warps its
 * offsets because a horizontal gradient has to track a SEMICIRCULAR arc's
 * angle-vs-x mismatch — the battery bar is a plain rectangle, advancing
 * linearly, so evenly-spaced stops are already correct and a warp would
 * introduce a mismatch that isn't there to fix. */
export function batteryRampStops(segments = 12): Array<{ offset: string; color: string }> {
  return Array.from({ length: segments + 1 }, (_, i) => {
    const t = i / segments; // 0 = empty edge, 1 = full edge
    return { offset: `${(t * 100).toFixed(4)}%`, color: gaugeFillColor((1 - t) * 100) };
  });
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
  /** "5,701 mAh · 91.2%": what a full charge holds now
   * (`AppleRawMaxCapacity`, the measured figure) and that as a percent of
   * the original capacity. Operator, 2026-09-24: "raw", "design" and "when
   * new" all read as jargon. */
  value: string;
  /** "6,249 mAh": the original capacity, its own row. */
  original: string;
  /** Hover text for the max charge row: the nominal reading, and that
   * neither figure is macOS's own "Maximum Capacity" percentage. */
  title: string | null;
}

/** The MAX CHARGE and ORIGINAL CAPACITY rows. `null` when either figure is
 * missing. */
export function capacityLine(h: BatteryHealth | null): CapacityDisplay | null {
  if (h === null) return null;
  const design = h.design_capacity_mah;
  const raw = h.raw_max_capacity_mah;
  if (design == null || raw == null) return null;
  const value = `${raw.toLocaleString()} mAh${h.raw_capacity_pct != null ? ` · ${h.raw_capacity_pct}%` : ""}`;
  const original = `${design.toLocaleString()} mAh`;

  const nominalParts: string[] = [];
  if (h.nominal_charge_capacity_mah != null) {
    nominalParts.push(`${h.nominal_charge_capacity_mah.toLocaleString()} mAh nominal`);
  }
  if (h.nominal_capacity_pct != null) nominalParts.push(`${h.nominal_capacity_pct}%`);
  const title =
    nominalParts.length > 0
      ? `Measured full charge. Nominal reading: ${nominalParts.join(", ")}. Neither is macOS's own "Maximum Capacity" percentage — that figure uses an undocumented Apple formula and reproduces from neither.`
      : `Measured full charge. Not macOS's own "Maximum Capacity" percentage — that figure uses an undocumented Apple formula.`;
  return { value, original, title };
}

/** `5,368 h` — the lifetime cross-check total, formatted with the same
 * thousands separator the mAh figures use. `null` renders as `null`
 * (caller decides whether to hide the row). */
export function fmtOperatingHours(hours: number | null): string | null {
  if (hours == null) return null;
  return `${hours.toLocaleString()} h`;
}
