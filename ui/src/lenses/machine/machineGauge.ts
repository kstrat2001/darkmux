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

/** The arc fill's hue — the ONE color on this face derived from client
 * arithmetic rather than a server verdict, and the deliberate exception to
 * this module's otherwise strict "the server decides, we render" rule.
 *
 * Why it has to be an exception (docs/design/machine-lens/provenance.md
 * finding 1): the fill used to key on `machine.state`, and on a real machine
 * `machine.state` is almost always `unknown` — the ledger honestly refuses to
 * promise a fit whenever ANY resident model is unpriceable, which is the
 * normal case. A fill keyed to that verdict is therefore a permanently grey
 * fill, and a gauge whose needle sweeps a grey arc from empty to full has
 * thrown away the one thing a gauge is for: showing you, without reading a
 * number, that the tank is filling.
 *
 * So the two questions are separated, and each is answered by the channel that
 * can actually answer it:
 * - **"how full is it?"** — this ramp, off the needle's own position. Pure
 *   arithmetic on two server numbers (`current / scale`), no verdict implied.
 * - **"is the machine in trouble?"** — unchanged and still server-only: the
 *   state chip, the seven lamps, the redline (`redlineLit`, one field), and
 *   the face caption. An amber fill NEVER means the arbiter said amber.
 *
 * Thresholds are the operator's own (2026-08-14): green to half, amber past
 * half, red approaching the line. They are fill-level marks, not the server's
 * cascade, and nothing else on the page reads them. */
export function gaugeFillSeverity(pct: number): "green" | "amber" | "red" {
  if (!Number.isFinite(pct) || pct < 50) return "green";
  if (pct < 85) return "amber";
  return "red";
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

/** The machine chip's word, with its REASON attached when the verdict is
 * `unknown`.
 *
 * A bare `UNKNOWN` is jargon that comes out of nowhere: nothing else on the
 * page defines it, and — per docs/design/machine-lens/provenance.md finding
 * 1 — it is the PERMANENT state on any machine with an unpriceable resident,
 * i.e. most real ones. So the one word a first-time reader most needs
 * explained is also the one they will see every single time.
 *
 * The cascade has exactly two `Unknown` arms and they are distinguishable
 * from the SAME fields the server branched on, so this names which one fired
 * rather than guessing:
 *   - `limit_bytes == null` → the server's `None` arm, "no limit readable".
 *   - otherwise (a limit exists, the priced sum fits, but residents are
 *     unpriceable) → the `Some(_)` arm, "unpriced resident".
 * Order matters and mirrors the server's: the `Some(limit)` arms cannot fire
 * at all when the limit is absent, so a missing limit is checked FIRST.
 *
 * Green/amber/red stay bare — they are self-evident, and a reason appended
 * to a self-evident word is noise. This never invents a verdict; it only
 * annotates one the server already reached. */
export function machineStateWord(state: string | null | undefined, limitBytes: number | null | undefined, unpricedModels: number): string {
  const word = (state || "unknown").toUpperCase();
  if (word !== "UNKNOWN") return word;
  if (limitBytes == null) return "UNKNOWN · no limit readable";
  if (unpricedModels > 0) return "UNKNOWN · unpriced resident";
  return word; // defensive: unknown for neither named reason — never invent one
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
  warningsCount: number;
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
 * machine it sat UNLIT rendering the word "GREEN" in grey — a few inches from
 * the same word rendered in actual green on the machine chip. A tell-tale
 * never renames itself; the oil light says "oil pressure" whether it is lit
 * or not, and its lit-ness is the entire message.
 *
 * It was also a duplicate: the machine chip beside it already carries the
 * verdict WITH its cause and its estimated-count qualifier, so the lamp
 * offered a second, greyer, less-informed copy. The other lamps each key on a
 * CONDITION (pressure, over-limit, stale, an unpriced resident); a verdict is
 * not a condition, and it already has a home. Always all seven render (an unlit
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
      title: "resident model(s) with no readable arch facts — see the warning below",
    },
    {
      key: "pressure",
      word: "PRESSURE",
      lit: inputs.pressureRed,
      severity: "bad",
      title: "memory_free_percent trigger",
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
      word: inputs.warningsCount > 0 ? `⚠ WARN ×${inputs.warningsCount}` : "WARN",
      lit: inputs.warningsCount > 0,
      severity: "warn",
      title: "full warning text below",
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
  const freeText =
    pressure.memory_free_percent != null && Number.isFinite(Number(pressure.memory_free_percent))
      ? String(Math.round(Number(pressure.memory_free_percent)))
      : "—";
  const swap = splitFormatted(memBytes(pressure.swap_used_bytes));
  const comp = splitFormatted(memBytes(pressure.compressor_bytes));
  return [
    {
      digits: digitCells(freeText),
      unit: "% free",
      label: "memory free",
      // The one figure here that can put the machine in Red, and the one
      // most easily misread: it sits beside two BYTE COUNTS but is not one.
      note: "the only figure that can trigger RED — kern.memorystatus_level, the kernel's own 0–100 pressure headroom, not a byte count",
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

/** The per-model detail line — `ctx · weights · kv@ctx · potential ·
 * current`, the same shape the retired `modelLines()` produced as its
 * fourth element, kept verbatim because it is a well-tested, genuinely good
 * string (docs/design/machine-lens/provenance.md row ⑮'s traced identities all read off this exact
 * text). Detail-layer precision (`memBytes()`, two decimals) — this is a
 * k/v row, not the glance layer. */
export function modelKvLine(m: MachineResourcesModel): string {
  const kv = m.kv_bytes_at_ctx != null ? `kv@ctx ${memBytes(m.kv_bytes_at_ctx)}` : "kv unknown (no arch facts)";
  return `ctx ${m.loaded_ctx} · weights ${memBytes(m.weights_bytes)} · ${kv} · potential ${memBytes(m.potential_bytes)} · current ${memBytes(m.current_bytes)}`;
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
