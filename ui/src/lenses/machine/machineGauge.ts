/**
 * Pure builders for the Stage 2/3 machine-lens redesign (docs/design/machine-lens/proposal.md in the
 * design packet; §3 "level 3 — the works" is the chosen treatment). Nothing
 * here touches the DOM — every function is a straight number-in/shape-out
 * transform, tested without rendering (`machineGauge.test.ts`), which is
 * also where docs/design/machine-lens/provenance.md's honesty rules get pinned at the unit level:
 * absence vs zero, unknown-is-real, color-never-alone, redline keys on
 * exactly one server field.
 *
 * Two deliberate PRECISION conventions, both traced to the operator-approved
 * mockups (`level3.html`/`scaling.html`) rather than invented here:
 * - **Glance layer** (the gauge's center readout, its on-arc tick labels, the
 *   caption under it) uses ONE decimal place (`gaugeValueParts`,
 *   `gaugeTickLabel`) — the SVG face has no room for `memBytes()`'s two.
 * - **Detail layer** (per-model kv lines, the machine k/v row, the odometer
 *   tiles, the footer) keeps `memBytes()`'s existing two-decimal form — nothing
 *   about Stage 2/3 asked for that to change, and it stays the one place
 *   every figure on the page still matches docs/design/machine-lens/provenance.md's traced values.
 */

import { GIB, KIB, MIB, memBytes, memPct, memStateCls } from "../../lib/format";
import type { MachineResources, MachineResourcesModel } from "../../types/handwritten";

export type Severity = "green" | "amber" | "red" | "unknown";

// ── The gauge (the semicircle hero) ─────────────────────────────────────

export interface GaugeTick {
  pct: number; // 0-100, position along the sweep
  label: string;
}

export interface GaugeGeometry {
  scale: number;
  cur: number | null;
  pct: number; // clamped 0-100 — the needle's position
  needleAngleDeg: number; // 0-180, rotate() the needle by this
  /** Σ PRICED potential's position on the arc, or `null` when there is
   * nothing to mark (no models, or the priced sum is exactly 0 — see
   * `MachineHealthRegion`'s guard). Distinct from a model's OWN
   * `potential_bytes` being `null` (the unpriced-model case, `MemBar`'s
   * absence-not-zero rule) — the MACHINE total's potential field is never
   * itself nullable (`MachineResources.machine.potential_bytes: number`). */
  commitPct: number | null;
  commitAngleDeg: number | null;
  /** The raw (unclamped) commit percentage exceeded 100 — Σ potential >
   * scale, machine-Amber's own condition (`model_ledger.rs`'s cascade arm
   * 4). The tick still CLAMPS to the line visually (`commitPct` is
   * clamped); this flag is what turns it amber instead of the neutral
   * dashed `--fg`. */
  overcommitted: boolean;
  ticks: GaugeTick[];
  scaleWord: "LIMIT" | "BUDGET";
}

/** `gaugeValueParts()` — the glance-layer figure, one decimal for GiB, whole
 * numbers below that. `memBytes()` (two decimals) stays the detail-layer
 * convention; see this module's own doc for why the two coexist. Both are
 * **binary** since #1811 — see `format.ts::memBytes` for the units argument. */
export function gaugeValueParts(bytes: number | null | undefined): { num: string; unit: string } {
  if (bytes == null) return { num: "—", unit: "" };
  const n = Number(bytes);
  if (!Number.isFinite(n)) return { num: "—", unit: "" };
  if (n >= GIB) return { num: (n / GIB).toFixed(1), unit: "GiB" };
  if (n >= MIB) return { num: String(Math.round(n / MIB)), unit: "MiB" };
  if (n >= KIB) return { num: String(Math.round(n / KIB)), unit: "KiB" };
  return { num: String(Math.round(n)), unit: "B" };
}

/** `gaugeTickLabel()` — the bare, unit-less number painted directly on the
 * arc — five of these share one tight radius, so even the one decimal
 * `gaugeValueParts` keeps is too much. Whole GiB only; a machine with a
 * sub-GiB scale isn't a real target here.
 *
 * Binary is what makes this row legible: the same 128 GiB machine used to
 * label its arc `0 · 34 · 69 · 103 · 137` (decimal quarter-marks of
 * `hw.memsize`) and now labels it `0 · 32 · 64 · 96 · 128` — the powers of two
 * an operator can recognize as their own hardware without doing arithmetic. */
export function gaugeTickLabel(bytes: number): string {
  const n = Number(bytes);
  if (!Number.isFinite(n) || n <= 0) return "0";
  return String(Math.round(n / GIB));
}


/** The dial fill's color ramp stops — `--good`, `--warn`, `--bad` from
 * `styles.css`, duplicated here as literals because an SVG `stroke` has to
 * be a concrete value and reading a CSS custom property at render time
 * would mean a `getComputedStyle` call per frame.
 *
 * THREE stops, not two, and that is not a flourish. Interpolating this
 * palette's green (`#4ade80`) straight to its red (`#f56565`) passes through
 * `#9fa172` — a muddy olive, because both endpoints are pastels carrying a
 * lot of blue. Routing through the palette's own amber puts a real yellow at
 * the midpoint AND keeps every color the dial can show inside the
 * vocabulary the rest of the page already uses. */
const FILL_STOPS = ["#4ade80", "#f0b429", "#f56565"] as const;

function mixHex(a: string, b: string, k: number): string {
  const ch = (h: string, i: number) => parseInt(h.slice(1 + i * 2, 3 + i * 2), 16);
  const out = [0, 1, 2].map((i) => Math.round(ch(a, i) + (ch(b, i) - ch(a, i)) * k));
  return `#${out.map((n) => n.toString(16).padStart(2, "0")).join("")}`;
}

/** (operator request) The fill color as a CONTINUOUS function of how full
 * the machine is: pure green at 0%, the palette's amber at 50%, pure red at
 * 100%.
 *
 * This replaces `gaugeFillSeverity`'s three buckets, and the reason is the
 * same one that removed the verdict chip: **the bucket edges at 50% and 85%
 * were thresholds darkmux invented.** A machine at 84% and one at 86% are
 * not different in kind, and painting them different colors asserted that
 * they were. A ramp asserts nothing — it maps a ratio the operator can
 * already read off the needle onto a hue, and every boundary in it is
 * arbitrary in the same tiny degree, which is to say not a boundary at all.
 *
 * Clamps rather than extrapolating: a machine past its limit is drawn at the
 * red end, not at some color beyond red — and that includes `+Infinity`,
 * which is what a zero limit divides out to. Only `NaN` (no figure at all)
 * falls to the green end, and it is the caller's job not to draw a band for
 * a figure it does not have. */
export function gaugeFillColor(pct: number): string {
  const t = (Number.isNaN(pct) ? 0 : Math.max(0, Math.min(100, Number(pct)))) / 100;
  return t < 0.5
    ? mixHex(FILL_STOPS[0], FILL_STOPS[1], t * 2)
    : mixHex(FILL_STOPS[1], FILL_STOPS[2], (t - 0.5) * 2);
}

/** The ramp painted ACROSS THE ARC'S SWEEP — green at 0, red at the scale's
 * end — so a band takes its color from WHERE IT SITS on the dial rather
 * than from a figure computed about it. The needle's position and the
 * color under it then carry the same information, and nothing is asserted:
 * the ramp is the same whatever the machine is doing, and the fill simply
 * reveals its own slice of it.
 *
 * Returned as `{offset, color}` stops for an SVG `linearGradient` laid
 * horizontally across the arc's bounding box.
 *
 * **The offsets are cosine-spaced, not linear, and that is the whole
 * subtlety.** A horizontal gradient interpolates along X, while the arc
 * advances by ANGLE; for a semicircle the two are related by
 * `x = cx − r·cos(pct·π)`, so evenly-spaced colors in X would bunch
 * visibly wrong against the tick marks — the 50% stop would not land at the
 * top of the dial. Placing each stop at its own `(1 − cos(pct·π)) / 2`
 * makes the gradient track the arc exactly, so the color at any tick is
 * the color that tick's percentage maps to.
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

/** A CSS `linear-gradient(...)` spanning ONE band's own slice of the arc
 * ramp — for the legend swatch that labels it. A flat swatch beside a
 * multi-colored band would break the mapping the legend exists to state. */
export function gaugeRampSwatch(startPct: number, endPct: number): string {
  const a = gaugeFillColor(startPct);
  const b = gaugeFillColor(endPct);
  return `linear-gradient(90deg, ${a}, ${b})`;
}


// ── Seven-segment glyphs ─────────────────────────────────────────────────

/** Which segments are lit for each character this readout can show, in the
 * conventional `a`–`g` naming (`a` top, clockwise to `f` upper-left, `g`
 * middle). Anything unmapped renders blank rather than throwing — a readout
 * handed an unexpected character shows an empty cell, which is honest, where
 * a crash would take the whole machine page with it. */
const SEVEN_SEG_LIT: Record<string, string> = {
  "0": "abcdef", "1": "bc", "2": "abged", "3": "abgcd", "4": "fgbc",
  "5": "afgcd", "6": "afgedc", "7": "abc", "8": "abcdefg", "9": "abfgcd",
  // Every dash this readout can be handed lights the middle bar, which is
  // exactly what a dash looks like on a seven-segment cell. The EM dash is
  // the one that matters: `gaugeValueParts` returns "—" for an unreadable
  // figure, and without it the no-data case drew an empty cell — an
  // invisible readout where the page's whole honesty rule is that absence
  // must be VISIBLE as absence and never a fabricated 0.
  "-": "g", "\u2013": "g", "\u2014": "g",
};

/** Segment polygons in a 60x100 cell — the canonical grid every consumer
 * scales into its own box, so the hero figure inside the gauge SVG and the
 * pressure tiles in HTML cannot drift into two different glyph shapes. */
const SEVEN_SEG_POLY: Record<string, string> = (() => {
  const h = (cy: number) => `12,${cy} 17,${cy - 5} 43,${cy - 5} 48,${cy} 43,${cy + 5} 17,${cy + 5}`;
  const v = (cx: number, y1: number, y2: number) =>
    `${cx},${y1} ${cx + 5},${y1 + 5} ${cx + 5},${y2 - 5} ${cx},${y2} ${cx - 5},${y2 - 5} ${cx - 5},${y1 + 5}`;
  return { a: h(6), g: h(50), d: h(94), f: v(6, 12, 44), b: v(54, 12, 44), e: v(6, 56, 88), c: v(54, 56, 88) };
})();

export const SEVEN_SEG_CELL = { w: 60, h: 100 } as const;

/** How visible an UNLIT segment is.
 *
 * A real LCD shows its whole character cell, lit or not, and that ghosting
 * is what separates a display from a typeface — it also anchors a narrow
 * glyph like `1` in its cell, so a figure like `110.6` reads as evenly
 * spaced rather than as digits floating in their own gaps.
 *
 * Chosen by looking, not reasoning: 8% / 4.5% / 2% / 0% rendered side by
 * side at both the hero and pressure-tile sizes, then 0%, 3% and 5% in the
 * running page on the screens it is actually read on. 8% held the cell but
 * read as a period reference and smudged at tile size; 0% was clean but let
 * a narrow `1` drift in its own gap. 5% is the operator's call — enough that
 * the cell exists, not enough to date the design.
 *
 * ONE constant, because the hero figure and the pressure tiles must agree:
 * two readouts on one face ghosting differently would read as a rendering
 * bug, not a choice. */
export const SEVEN_SEG_GHOST = 0.05;

/** Every segment of one character's cell, each flagged lit or not, so a
 * consumer can render the unlit ones at [`SEVEN_SEG_GHOST`] without knowing
 * the segment naming. */
export function sevenSegmentPolygons(ch: string): { points: string; lit: boolean }[] {
  const on = SEVEN_SEG_LIT[ch] ?? "";
  return Object.entries(SEVEN_SEG_POLY).map(([k, points]) => ({ points, lit: on.includes(k) }));
}

/** Whether this character is drawn as a decimal point rather than segments —
 * named here so both consumers branch on one rule. */
export function isSevenSegDot(ch: string): boolean {
  return ch === ".";
}

/** The scale's own end-label word — `LIMIT`, or `BUDGET` once a #1243
 * budget is configured (`limit_source === "budget"`). Never a bare number;
 * docs/design/machine-lens/proposal.md §3's whole argument for moving the denominator off the face
 * was that the max tick had to carry its OWN meaning. */
export function gaugeScaleWord(limitSource: string | null | undefined): "LIMIT" | "BUDGET" {
  return limitSource === "budget" ? "BUDGET" : "LIMIT";
}

/** The gauge's scale — deliberately NOT the old flat-meter's `machineScale()`
 * (limit ∨ pool ∨ potential ∨ current, whichever is largest). That auto-
 * expanding scale was right for a linear track that had to fit everything
 * without clipping; the semicircle's whole argument (docs/design/machine-lens/proposal.md §"The
 * denominator, argued") is the opposite — the scale end IS the allowance
 * (`limit_bytes`, or the #1243 budget), full stop, so an overcommitted
 * potential CLAMPS to the line and turns amber rather than silently
 * stretching the scale to accommodate it (which would make "overcommitted"
 * unrenderable — the tick could never exceed a scale that grows to fit it).
 * Falls back to the physical pool, then to Σ current/potential, ONLY when no
 * limit is readable at all (the "no limit configured" degenerate case) —
 * never as a competing candidate against a real limit. */
export function resolveGaugeScale(limit: number | null, poolCap: number | null, pot: number | null, cur: number | null): number {
  if (limit != null && limit > 0) return limit;
  if (poolCap != null && poolCap > 0) return poolCap;
  return Math.max(pot || 0, cur || 0, 1);
}

/** Builds the whole gauge's geometry off one `MachineResources` payload —
 * the single source every element on the face (needle, ticks, commit
 * marker, redline) reads from, so none of them can disagree about what the
 * scale means. */
/** One segment of the stacked band, as percentages of the dial's scale. */
export interface BandSegment {
  startPct: number;
  lengthPct: number;
}

export interface BandGeometry {
  /** darkmux's own memory, from 0. */
  darkmux: BandSegment;
  /** Everything else on the machine, stacked on top of darkmux and ending at
   * the needle. Its derivedness is SELF-EVIDENT here in a way it never was as
   * an undrawn gap: it is visibly the span between darkmux's end and the
   * needle. */
  other: BandSegment;
  /** darkmux's committed-but-unmaterialised growth, beyond the needle. */
  growth: BandSegment;
  /** Where the machine is NOW — the end of `other`, and the needle. */
  usedPct: number;
  /** Where it lands if darkmux's models fully materialise. */
  projectedPct: number;
  needleAngleDeg: number;
}

/**
 * The dial's stacked band (#1821).
 *
 * This replaced two concentric rings. The rings were honest but not
 * glanceable: "everything else" was left undrawn on the theory that the GAP
 * between the rings showed it — but that gap is the angular difference
 * between two arc endpoints at DIFFERENT RADII, so the machine's single
 * largest consumer (~50 GiB here) required cross-radius mental subtraction
 * and was effectively unrendered. The inner ring also under-read: at r=66 vs
 * r=86 the same percentage draws a visibly shorter arc, so darkmux's share
 * looked smaller than it was. And a gauge that needs a three-item legend to
 * be read is telling you something about its own geometry.
 *
 * Stacking restores the property the rings gave up — ADDITIVITY. This page's
 * whole question is "will it fit", which is a sum, and the one thing two
 * rings could not show was the sum.
 *
 * On the "other is derived, so do not draw it" rule that kept it off the
 * rings: that was over-applied. The needle position, every percentage, and
 * the growth band are all client arithmetic on server numbers. Stacked, the
 * geometry declares `other` a remainder by construction — it is the span
 * between darkmux's end and the needle — which is more honest than absence,
 * not less.
 */
export function computeBandGeometry(resources: MachineResources): BandGeometry {
  const geo = computeGaugeGeometry(resources);
  const scale = geo.scale;
  const pctOf = (b: number | null | undefined): number => (b == null || !scale ? 0 : Math.max(0, Math.min(100, (Number(b) / scale) * 100)));

  const darkmuxPct = pctOf(resources.machine.current_bytes);
  // The machine's used can never read below darkmux's own share — darkmux's
  // memory IS part of the machine's — so a degraded pool reading clamps up
  // rather than producing a negative `other`.
  const usedPct = Math.max(pctOf(resources.pool?.used_bytes), darkmuxPct);
  const committedPct = pctOf(resources.machine.potential_bytes);
  const growthPct = Math.max(0, Math.min(100 - usedPct, committedPct - darkmuxPct));

  return {
    darkmux: { startPct: 0, lengthPct: darkmuxPct },
    other: { startPct: darkmuxPct, lengthPct: Math.max(0, usedPct - darkmuxPct) },
    growth: { startPct: usedPct, lengthPct: growthPct },
    usedPct,
    projectedPct: usedPct + growthPct,
    needleAngleDeg: usedPct * 1.8,
  };
}

/**
 * A `stroke-dasharray` that draws ONE sub-arc of a `pathLength=100` path,
 * hatched, without CSS having to supply the dash pattern.
 *
 * This exists because of a real shipped bug: the growth band set its extent
 * via the `stroke-dasharray` ATTRIBUTE while `styles.css` set
 * `stroke-dasharray: 3 3` for the hatching — and a CSS declaration overrides
 * an SVG presentation attribute, so the inline extent was silently clobbered
 * and the band ran hatched to the END OF THE SCALE. The dial spent its whole
 * life claiming "projected to the limit". One dasharray cannot carry both
 * dashing and extent, so the extent and the hatch are composed here, in one
 * value, and the CSS rule is gone.
 *
 * Shape: a leading `0 <start>` gap, then `dash gap` pairs covering the
 * segment, then a long final gap swallowing the remainder.
 */
export function hatchedSegmentDash(startPct: number, lengthPct: number, dash = 2.2, gap = 2.2): string {
  if (lengthPct <= 0) return "0 100";
  const parts: number[] = [0, startPct];
  let drawn = 0;
  while (drawn < lengthPct) {
    const d = Math.min(dash, lengthPct - drawn);
    parts.push(d, gap);
    drawn += d + gap;
  }
  parts.push(0, 200); // swallow whatever is left of the path
  return parts.map((n) => Number(n.toFixed(3))).join(" ");
}

export function computeGaugeGeometry(resources: MachineResources): GaugeGeometry {
  const limit = resources.limit_bytes != null ? Number(resources.limit_bytes) : null;
  const poolCap = resources.pool?.capacity_bytes != null ? Number(resources.pool.capacity_bytes) : null;
  const pot = resources.machine.potential_bytes != null ? Number(resources.machine.potential_bytes) : null;
  const cur = resources.machine.current_bytes != null ? Number(resources.machine.current_bytes) : null;
  const scale = resolveGaugeScale(limit, poolCap, pot, cur);

  const pct = memPct(cur, scale);
  const needleAngleDeg = pct * 1.8;

  const hasCommit = pot != null && pot > 0;
  const rawCommitPct = hasCommit ? (pot! / scale) * 100 : null;
  const commitPct = rawCommitPct != null ? Math.max(0, Math.min(100, rawCommitPct)) : null;
  const commitAngleDeg = commitPct != null ? commitPct * 1.8 : null;
  const overcommitted = rawCommitPct != null && rawCommitPct > 100;

  const ticks: GaugeTick[] = [0, 0.25, 0.5, 0.75, 1].map((f) => ({
    pct: f * 100,
    label: gaugeTickLabel(scale * f),
  }));

  return {
    scale,
    cur,
    pct,
    needleAngleDeg,
    commitPct,
    commitAngleDeg,
    overcommitted,
    ticks,
    scaleWord: gaugeScaleWord(resources.limit_source),
  };
}


/** Whether a row's own state chip carries information the MACHINE chip does
 * not — i.e. whether this row disagrees with the machine's verdict.
 *
 * The tempting simplification was to delete the per-row chip outright, on the
 * grounds that unified memory is one pool with shared fate so the machine
 * state dominates. That is ALMOST true, and the exceptions are exactly the
 * interesting rows (`compute_ledger`'s per-model tint):
 *   - machine AMBER + a model whose `current >= potential` → the row is
 *     GREEN: its commitment is already materialized, so it is not part of
 *     the risk the machine is amber about.
 *   - a model with no `potential_bytes` (unpriceable) → the row stays
 *     UNKNOWN whatever the machine says.
 * A row that agrees with the machine is one fact stamped twice; a row that
 * disagrees is the only place that fact exists. So the chip renders exactly
 * when it disagrees — which on a healthy or uniformly-unknown machine means
 * it does not render at all, and the rows get quiet. */
export function rowStateDiffers(rowState: string | null | undefined, machineState: string | null | undefined): boolean {
  return memStateCls(rowState) !== memStateCls(machineState);
}

/** The redline's lit state keys on exactly ONE server field —
 * `machine.state === "red"` — zero client arithmetic (docs/design/machine-lens/proposal.md §"The
 * redline"). Whatever put the machine in Red (pressure, or the over-limit
 * disjunct) is a `stateWordSuffix()` question, never this one's. */
export function redlineLit(state: string | null | undefined): boolean {
  return state === "red";
}

/** The one piece of client arithmetic docs/design/machine-lens/proposal.md explicitly sanctions: `cur
 * >= limit` is the server's OWN published over-limit rule
 * (`model_ledger.rs`'s cascade arm 2) applied to two server-supplied
 * numbers — the exact comparison that already clamps the needle at 100%,
 * not an invented threshold. Returns `false` (never a guess) when either
 * figure is unreadable. */
export function isOverLimit(cur: number | null | undefined, limit: number | null | undefined): boolean {
  return cur != null && limit != null && Number(cur) >= Number(limit);
}

/** The gauge face's caption — the RED REASON when the redline is lit, and
 * `null` (nothing rendered) otherwise.
 *
 * It used to read `IN USE` in the normal case, carried over verbatim from
 * the level-3 mockups. The operator called it on the live page, and he is
 * right: it is noise. A semicircle with a needle, a filled arc, a scale
 * running 0→128 and a max tick labeled LIMIT is *self-evidently* a gauge of
 * how much is in use — the caption restates the one thing the instrument
 * cannot fail to communicate, in the most valuable pixels on the page,
 * directly beneath the reading.
 *
 * What the slot is actually FOR is the other branch: naming which disjunct
 * put the machine in Red, right where the eye already is. So it now renders
 * only when it has that to say — the same rule the per-row state chip
 * follows (`rowStateDiffers`), and the same one that deleted the
 * `darkmux/utility` card: an element that always says the same thing is
 * furniture, and this page has been shedding furniture all day.
 *
 * `pressureRed` is checked first because the server's own cascade checks it
 * first (arm 1 before arm 2) — a machine red for BOTH reasons reports the
 * one the server would name first. */
export function gaugeFaceCaption(state: string | null | undefined, pressureRed: boolean, overLimit: boolean): string | null {
  if (!redlineLit(state)) return null;
  if (pressureRed) return "RED · PRESSURE";
  if (overLimit) return "RED · OVER LIMIT";
  return "RED"; // defensive: red for neither known disjunct — never invent a reason
}

// ── The tell-tale lamp row ───────────────────────────────────────────────

export type LampKey = "residency" | "unpriced" | "pressure" | "overLimit" | "stale" | "warn";
export type LampSeverity = "dim" | "warn" | "bad";

export interface LampView {
  key: LampKey;
  word: string;
  lit: boolean;
  severity: LampSeverity;
  title: string;
}

export interface LampInputs {
  state: string | null | undefined;
  pressureRed: boolean;
  overLimit: boolean;
  unprivedCount: number;
  /** #1821 — count of `warn` + `error` severity messages ONLY. An `info`
   * disclosure (the #1819 estimate note) must NOT light this lamp — that
   * was the whole defect: a working-as-designed disclosure lit the same
   * amber WARN lamp as a real degradation. */
  alarmMessagesCount: number;
  resourcesErrored: boolean;
  residencyChanged: boolean;
}

/** Every lamp keys on exactly ONE named CONDITION (docs/design/machine-lens/provenance.md row ⑨) —
 listed here in the mockup's own
 * order.
 *
 * There is deliberately NO `STATE` lamp. One rendered here until the operator
 * caught what it was doing (2026-08-15): it relabelled ITSELF with the state
 * (`STATE GREEN` / `STATE AMBER`) *and* changed its lit-ness, so on a healthy
 * machine it sat UNLIT rendering the word "GREEN" in gray — a few inches from
 * the same word rendered in actual green on the machine chip. A tell-tale
 * never renames itself; the oil light says "oil pressure" whether it is lit
 * or not, and its lit-ness is the entire message.
 *
 * It was also a duplicate: the machine chip beside it already carries the
 * verdict WITH its cause and its estimated-count qualifier, so the lamp
 * offered a second, greyer, less-informed copy. The other lamps each key on a
 * CONDITION (pressure, over-limit, stale, an unpriced resident); a verdict is
 * not a condition, and it already has a home. Always all six render (an unlit
 * lamp still carries a visible outline — accessibility rule: presence is
 * never color-alone), so the row's SHAPE never changes with the payload,
 * only which ones glow. */
export function deriveLamps(inputs: LampInputs): LampView[] {
  return [
    {
      key: "residency",
      word: "Δ RESIDENCY",
      lit: inputs.residencyChanged,
      severity: "warn",
      title: "the resident set changed within the last poll or two",
    },
    {
      key: "unpriced",
      word: inputs.unprivedCount > 0 ? `⚠ UNPRICED ×${inputs.unprivedCount}` : "UNPRICED",
      lit: inputs.unprivedCount > 0,
      severity: "warn",
      title: "resident model(s) genuinely unpriceable — no readable arch facts AND no catalog size either (#1819) — see the warning below",
    },
    {
      key: "pressure",
      word: "PRESSURE",
      lit: inputs.pressureRed,
      severity: "bad",
      title: "margin_percent trigger",
    },
    {
      key: "overLimit",
      word: "OVER LIMIT",
      lit: inputs.overLimit,
      severity: "bad",
      title: "current_bytes has reached or passed the limit — the redline's own condition",
    },
    {
      key: "stale",
      word: inputs.resourcesErrored ? "⚠ STALE" : "STALE",
      lit: inputs.resourcesErrored,
      severity: "warn",
      title: "the last poll failed — figures below are the last good snapshot",
    },
    {
      key: "warn",
      word: inputs.alarmMessagesCount > 0 ? `⚠ WARN ×${inputs.alarmMessagesCount}` : "WARN",
      lit: inputs.alarmMessagesCount > 0,
      severity: "warn",
      title: "full message text below (warn/error severity only — a disclosure does not light this)",
    },
  ];
}

// ── Odometer tiles (the two high-water marks + memory free) ────────────

export interface OdometerView {
  digits: string[];
  unit: string;
  label: string;
  note: string;
}

function splitFormatted(formatted: string): { num: string; unit: string } {
  if (formatted === "—") return { num: "—", unit: "" };
  const sp = formatted.lastIndexOf(" ");
  if (sp === -1) return { num: formatted, unit: "" };
  return { num: formatted.slice(0, sp), unit: formatted.slice(sp + 1) };
}

/** Splits a formatted string into individual characters for the odometer's
 * digit cells (`level3.html`'s `.odo .c`). Works on ANY string — the "—"
 * no-data case renders as one cell, honest rather than a fabricated "0". */
export function digitCells(s: string): string[] {
  return s.split("");
}

/** The three pressure instruments as odometer tiles, in the mockup's own
 * order. Memory free is NOT a high-water mark (it can rise as well as
 * fall — it is the sole pressure TRIGGER); swap/compressor are, and their
 * note says so (`reports, never alarms` — the row-colored-by-its-own-
 * condition lesson carried into copy, docs/design/machine-lens/proposal.md §2). Reuses `memBytes()`
 * (the detail-layer's two-decimal convention) rather than the gauge's own
 * one-decimal `gaugeValueParts` — these are k/v figures, not the glance
 * layer. */
export function odometerTiles(pressure: MachineResources["pressure"]): OdometerView[] {
  const marginText =
    pressure.margin_percent != null && Number.isFinite(Number(pressure.margin_percent))
      ? String(Math.round(Number(pressure.margin_percent)))
      : "—";
  const swap = splitFormatted(memBytes(pressure.swap_used_bytes));
  const comp = splitFormatted(memBytes(pressure.compressor_bytes));
  return [
    {
      digits: digitCells(marginText),
      // Bare `%`, not `% margin`: the label below already says MARGIN, and
      // the tile was printing the word twice. A leftover from the #1821
      // rename — the unit read `% free` against a `MARGIN` label, which did
      // not collide, and correcting the unit made it a duplicate that
      // nobody re-read the pair for. Now it matches its two siblings, where
      // the unit is a pure unit (`GiB`) and the label is the subject.
      unit: "%",
      label: "margin",
      // #1821 (operator-approved rename): this tile used to read "% free"
      // — measured live, the SAME instant, this figure read 82% while
      // truly-free pages read 30.8%. Neither "free" nor "available"
      // belongs on it; `margin` (this project's own NASA register — mass
      // margin, power margin, propellant margin) is honest: it is
      // headroom before the kernel sheds load, not a byte count, and it
      // is still the only figure that can trigger RED.
      note: "the only figure that can trigger RED — kern.memorystatus_level = (capacity − wired − compressor) / capacity, the kernel's own 0–100 pressure headroom. Not free memory and not a byte count — it read 82% margin here while truly-free pages read 30.8%",
    },
    {
      digits: digitCells(swap.num),
      unit: swap.unit,
      label: "swap used",
      note: "vm.swapusage — a monotonic high-water mark since boot. Reports, never alarms: it does not fall when pressure eases",
    },
    {
      digits: digitCells(comp.num),
      unit: comp.unit,
      label: "compressor",
      // The disambiguation the operator asked for on 2026-08-15, after
      // reasonably wondering whether this was the utility tier. It is not —
      // and the collision is ours: darkmux's utility model does COMPACTION,
      // macOS has a COMPRESSOR, one letter apart, three rows apart on this
      // page. The label stays (it is the term the CLI and JSON use); the
      // note is where the two get told apart.
      note: "macOS's own memory compressor (vm_stat) — RAM holding compressed inactive pages. Nothing to do with darkmux's compactor, which is a model",
    },
  ];
}

// ── Model rows: the scaling rule + residency diffing ────────────────────

export type RowStatus = "live" | "new" | "ghost";
// `"expected"` is a RESERVED, not-yet-buildable fourth status — docs/design/machine-lens/proposal.md
// §8 names the `EXPECTED · not yet resident` row explicitly as blocked on a
// server-side `expected[]` set (staffing-derived) that does not exist today.
// The slot is named here so a future packet extends this union instead of
// inventing a parallel vocabulary; nothing in this module produces it.

export interface ResidencyRowView {
  identifier: string;
  owner: string;
  model: MachineResourcesModel;
  status: RowStatus;
  /** For a ghost row: when it was last actually resident. For a live/new
   * row: the current poll's timestamp (unused by live rows, carried for
   * uniformity). */
  lastSeenMs: number;
  /** Only meaningful for `status:"new"` — when this identifier was first
   * observed, for the "first seen Ns ago" chip. */
  firstSeenMs?: number;
}

interface KnownEntry {
  model: MachineResourcesModel;
  lastSeenMs: number;
  firstSeenMs: number;
}

export interface ResidencyState {
  known: Map<string, KnownEntry>;
  /** Ghosts already shown for one poll cycle — present here means "about to
   * retire," not "about to render." See `advanceResidency`'s own doc. */
  shownGhosts: Set<string>;
}

function rowIdentifier(m: MachineResourcesModel): string {
  return m.identifier || m.model_key;
}

/**
 * Advances the residency state machine by one poll (docs/design/machine-lens/proposal.md §8,
 * Scenario 2 — "a swap mid-glance"). Pure and total: same inputs, same
 * outputs, no timers, no DOM — `MachineLens.tsx` holds the returned `state`
 * in a ref and calls this again on the NEXT successful poll (never on an
 * errored one — a failed poll carries no model list to diff against).
 *
 * The rule, in order:
 * 1. A model present now that was ALREADY known → `"live"`.
 * 2. A model present now that was NOT known (first-ever poll, OR it just
 *    arrived, OR it's a ghost reappearing) → `"new"`. First-ever poll is
 *    the one exception: nothing is "new" relative to a poll that never
 *    happened, so `prev === null` marks everything `"live"` instead — see
 *    below.
 * 3. A model known last poll but absent now → becomes a ghost, rendered
 *    ONE more cycle (`"ghost"`).
 * 4. A model that was ALREADY a ghost (shown once) and is STILL absent →
 *    retires. No row, not carried into the next state.
 */
export function advanceResidency(
  prev: ResidencyState | null,
  currentModels: MachineResourcesModel[],
  nowMs: number,
): { state: ResidencyState; rows: ResidencyRowView[] } {
  const known = new Map<string, KnownEntry>();
  const shownGhosts = new Set<string>();
  const rows: ResidencyRowView[] = [];

  // First-ever successful poll: nothing to diff against, so nothing is
  // "new" — a cold boot is not an arrival.
  if (prev === null) {
    for (const m of currentModels) {
      const id = rowIdentifier(m);
      known.set(id, { model: m, lastSeenMs: nowMs, firstSeenMs: nowMs });
      rows.push({ identifier: id, owner: m.owner, model: m, status: "live", lastSeenMs: nowMs });
    }
    return { state: { known, shownGhosts }, rows };
  }

  const currentIds = new Set(currentModels.map(rowIdentifier));

  for (const m of currentModels) {
    const id = rowIdentifier(m);
    const prevKnown = prev.known.get(id);
    if (prevKnown) {
      known.set(id, { model: m, lastSeenMs: nowMs, firstSeenMs: prevKnown.firstSeenMs });
      rows.push({ identifier: id, owner: m.owner, model: m, status: "live", lastSeenMs: nowMs });
    } else {
      // Not known last poll — either a genuine arrival, or a ghost from
      // last poll reappearing (either way, honestly "new": the operator's
      // eye should be told the ground shifted, not silently reconciled).
      known.set(id, { model: m, lastSeenMs: nowMs, firstSeenMs: nowMs });
      rows.push({ identifier: id, owner: m.owner, model: m, status: "new", lastSeenMs: nowMs, firstSeenMs: nowMs });
    }
  }

  for (const [id, entry] of prev.known) {
    if (currentIds.has(id)) continue; // still resident, handled above
    if (prev.shownGhosts.has(id)) continue; // already shown once — retires
    shownGhosts.add(id);
    rows.push({ identifier: id, owner: entry.model.owner, model: entry.model, status: "ghost", lastSeenMs: entry.lastSeenMs });
  }

  return { state: { known, shownGhosts }, rows };
}

/** True when this poll's rows carry any residency change — drives the Δ
 * RESIDENCY lamp. A `prev === null` first poll never counts (nothing
 * "changed" relative to no prior poll). */
export function residencyChangedThisPoll(rows: ResidencyRowView[]): boolean {
  return rows.some((r) => r.status !== "live");
}

/** The scaling rule (docs/design/machine-lens/proposal.md §8): darkmux-owned rows first, then
 * alphabetical by identifier WITHIN each group — never by a live figure.
 * `owner` here is the same namespace test the server already computed
 * (docs/design/machine-lens/provenance.md row ⑭ — `owner==="darkmux"` IS the `darkmux:` prefix
 * test), so this never re-derives ownership from the identifier string. */
export function sortResidencyRows(rows: ResidencyRowView[]): ResidencyRowView[] {
  return [...rows].sort((a, b) => {
    const aDark = a.owner === "darkmux" ? 0 : 1;
    const bDark = b.owner === "darkmux" ? 0 : 1;
    if (aDark !== bDark) return aDark - bDark;
    return a.identifier.localeCompare(b.identifier);
  });
}

export interface RowGroup {
  key: "darkmux" | "user";
  label: string;
  rows: ResidencyRowView[];
}

/** Groups the sorted rows into darkmux-managed / user-loaded buckets, each
 * with a live-only count (ghosts/new don't inflate the "N RESIDENT" header
 * the way a raw `.length` would — matching `scaling.html`'s
 * `1 RESIDENT (+1 EXPECTED)` / `2 RESIDENT (+1 DEPARTED)` phrasing, minus
 * the EXPECTED half this packet doesn't build). A group with zero rows is
 * omitted entirely — headers render only when there's something under
 * them, matching docs/design/machine-lens/proposal.md §8's "today's common case adds no chrome". */
export function groupResidencyRows(rows: ResidencyRowView[]): RowGroup[] {
  const sorted = sortResidencyRows(rows);
  const darkmux = sorted.filter((r) => r.owner === "darkmux");
  const user = sorted.filter((r) => r.owner !== "darkmux");
  const groups: RowGroup[] = [];
  if (darkmux.length) groups.push({ key: "darkmux", label: "DARKMUX-MANAGED", rows: darkmux });
  if (user.length) groups.push({ key: "user", label: "USER-LOADED", rows: user });
  return groups;
}

/** `perModelScale` — the shared track scale every row's bar draws against
 * (the scaling rule's own footnote: "scale shared across all rows"), reused
 * from `memoryLedgerLines.ts` rather than re-derived. Ghost rows keep their
 * LAST observed model in the row, so they still contribute to the shared
 * scale for the one cycle they render — a departed model's bar doesn't
 * suddenly need a different scale than the live rows beside it. */
export { perModelScale } from "./memoryLedgerLines";

/** `memStateCls` re-export point of use — kept here as a single named import
 * so `MachineHealthRegion.tsx` doesn't need to reach into `lib/format`
 * directly for this one mapping; see `format.ts` for the honesty rationale
 * (a hostile/unrecognized state string degrades to "unknown", never lands
 * raw in a class attribute). */
export { memStateCls };

/** Whether a row's potential was priced by the #1819 size-based fallback
 * rather than measured arch facts — the ESTIMATED marker's own condition,
 * named once here rather than an inline string compare repeated at every
 * call site (matching this module's convention of naming every condition a
 * marker renders on: `rowStateDiffers`, `isOverLimit`, `redlineLit`). */
export function isEstimatedRow(m: Pick<MachineResourcesModel, "potential_source">): boolean {
  return m.potential_source === "estimated";
}

/** The per-model detail line — `ctx · weights · kv@ctx · potential ·
 * current`, the same shape the retired `modelLines()` produced as its
 * fourth element, kept verbatim because it is a well-tested, genuinely good
 * string (docs/design/machine-lens/provenance.md row ⑮'s traced identities all read off this exact
 * text). Detail-layer precision (`memBytes()`, two decimals) — this is a
 * k/v row, not the glance layer. */
export function modelKvLine(m: MachineResourcesModel): string {
  const kv = m.kv_bytes_at_ctx != null ? `kv@ctx ${memBytes(m.kv_bytes_at_ctx)}` : "kv unknown (no arch facts)";
  // #1819: an ESTIMATED row's potential is a labeled guess, not a
  // measurement — the `~` and `(estimated)` suffix travel with the FIGURE
  // itself, not just a separate badge, so the number can never be read out
  // of context (e.g. copy-pasted into a bug report without its caveat).
  const potential = isEstimatedRow(m)
    ? `potential ~${memBytes(m.potential_bytes)} (estimated)`
    : `potential ${memBytes(m.potential_bytes)}`;
  return `ctx ${m.loaded_ctx} · weights ${memBytes(m.weights_bytes)} · ${kv} · ${potential} · current ${memBytes(m.current_bytes)}`;
}

/** Whether this residency row IS the machine's configured utility-tier
 * model — the badge that says "this resident is darkmux's own small-model
 * tier" (compaction, mission-compile, estimate, scribe).
 *
 * It takes only the configured ID and needs no `loaded` flag, which is the
 * whole point: a row exists in this ledger if and only if `lms ps` lists the
 * model (`gather_with_bin` builds `residents` from `ps` rows alone), so the
 * ROW is the residency claim and this function only has to answer identity.
 * A configured-but-unloaded tier simply has no row to match, and badges
 * nothing, without any state machine to get wrong. That collapse is what
 * replaced a four-state explainer card on this page: the card was config,
 * not machine state, and `darkmux doctor`'s `check_utility_model_binding`
 * already answers the config question — with a fix hint.
 *
 * Matches the namespaced `identifier` OR the bare `model_key`, and that is
 * not belt-and-braces — it MIRRORS THE SERVER. `machine_specs_handler`
 * resolves the same binding with `m.identifier == id || m.model == id`,
 * because the profiles registry may store the utility model id in either
 * form. Matching a single field would leave the badge missing on a machine
 * whose registry happens to hold the other one — a configuration the server
 * explicitly supports. Widening it cannot produce a FABRICATED badge: any
 * row this tags matched the operator's own configured id exactly, on one of
 * the two fields the server itself compares. Never matches on a
 * `null`/`undefined` on either side. */
export function isUtilityTierRow(
  identifier: string | null | undefined,
  modelKey: string | null | undefined,
  utilityModelId: string | null,
): boolean {
  if (utilityModelId == null) return false;
  return identifier === utilityModelId || modelKey === utilityModelId;
}
