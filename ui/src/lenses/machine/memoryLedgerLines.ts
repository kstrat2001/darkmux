/**
 * Text/data-builders for the machine lens's `darkmux/utility` node and a
 * handful of figures the Stage 2/3 redesign (`MachineHealthRegion.tsx`)
 * still reads verbatim — limit-source wording, the attribution/stamp
 * footer, the not-local/loading/daemon-unreachable placeholder sentences.
 *
 * (#1806 Stage 1 → Stage 2/3 history) Stage 1 replaced one flat `healthLines()`
 * string array with granular per-fragment exports so `MachineHealthRegion.tsx`
 * could slot legacy's own `.memcard`/`.membar` structure around unchanged
 * text. Stage 2/3 (the gauge/lamp/odometer/row redesign, `MachineHealthRegion.tsx`,
 * docs/design/machine-lens/proposal.md in the design packet) replaced that flat-ledger RENDERING
 * entirely, and with it the fragments that existed only to feed it:
 * `machineTotalText()` and `modelLines()` (the old card's meta-line/header
 * text) and `pressureText()` (superseded by `machineGauge.ts`'s
 * `odometerTiles()`) are gone — their successors live in `machineGauge.ts`
 * alongside the rest of the gauge/lamp/row math, not here. `machineScale()`
 * is also gone: the gauge's own scale is deliberately NOT that function's
 * auto-expanding max (see `machineGauge.ts::resolveGaugeScale`'s doc for
 * why the two differ). `perModelScale()` survives unchanged — the "shared
 * scale across every row" rule is the same rule at Stage 2/3 as it was at
 * Stage 1, just applied to rows instead of cards (re-exported from
 * `machineGauge.ts` for that module's own consumers).
 *
 * `.memowner`/`.memstate` are `text-transform: uppercase` in legacy CSS —
 * the TEXT content itself is lowercase (`owner`/`state` verbatim from
 * `/machine/resources`); only the rendering was uppercased there. This port
 * uppercases the STRING directly (see `format.ts`'s module doc) rather than
 * depending on a stylesheet rule the port is free to change.
 *
 * (operator-approved utility-block redesign) `utilityLines()` — the
 * `darkmux/utility` node's own flat four-element `string[]` builder,
 * unchanged since this file's introduction — is retired in favor of
 * `utilityView()`, a typed structure (see its own doc below). The old
 * function's positional-array shape forced `MachineLens.tsx`'s rendering CSS
 * into `nth-child` selectors and flattened a live probe reading and a
 * hardcoded capability list into one indistinguishable style; the new shape
 * carries a `model.isLiveData` flag specifically so the renderer can keep
 * those two visually distinct (`docs/design/machine-lens/provenance.md`'s
 * rule against hardcoded copy rendering as if it were read data). The four
 * branch conditions (`um` undefined/null/loaded/not-loaded) are unchanged —
 * only what each branch produces changed.
 */

import { MACHINE_MEM_POLL_MS } from "../../lib/queryKeys";
import type { MachineResources, MachineResourcesModel, MachineSpecs } from "../../types/handwritten";

/** Chip severities the utility block's state chip can carry. Deliberately
 * narrower than `.mm-chip`'s full green/amber/red vocabulary — nothing about
 * utility-tier residency is ever a RED (failure) condition, only a spectrum
 * from "healthy" to "can't see it" — and `"unknown"` renders with NO color
 * class (see `.mm-chip`'s own CSS comment: `unknown` is a real, common state
 * and its honest rendering is the neutral base look, never an invented
 * color). */
export type UtilityChipSeverity = "green" | "amber" | "unknown";

/** The structured shape of the `darkmux/utility` node. Replaces the old
 * `utilityLines()` fixed four-element `string[]` (see this file's git
 * history / #1806 packet doc for that shape) — a positional array forced
 * the rendering CSS to key on `nth-child`, which is exactly the kind of
 * "guesswork from shape" this project's own `lineClass()` doc (in
 * `MachineLens.tsx`) warns against for variable content, and it also
 * flattened two semantically different rows (a live probe reading and a
 * hardcoded capability list) into one identical style with no label at all.
 * `model.isLiveData` is the field that lets the renderer apply
 * `docs/design/machine-lens/provenance.md`'s rule that hardcoded UI copy
 * must never render indistinguishably from a value that came off a probe —
 * `handles` has no such flag because it is ALWAYS static copy. */
export interface UtilityView {
  /** Constant: `"darkmux/utility"`. */
  name: string;
  /** Constant gloss naming what this node IS, so a first-time reader isn't
   * left inferring it from the model id alone. */
  gloss: string;
  model: {
    /** The model id (live probe data) when one is registered/reported;
     * otherwise an explanatory sentence — never a bare dash or blank. */
    value: string;
    /** `true` only when `value` came off `/machine/specs` verbatim. Drives
     * the bright-vs-dim treatment (provenance.md's rule, above). */
    isLiveData: boolean;
  };
  /** Constant: `"compaction · mission-compile · estimate · scribe"` — the
   * fixed capability list, always hardcoded UI copy, never live data. */
  handles: string;
  chip: { text: string; severity: UtilityChipSeverity };
  /** The utility model's id when it is ACTUALLY RESIDENT, else `null` — the
   * single field the ledger's `utility` row-chip keys on
   * (`isUtilityTierRow`). A real discriminant rather than a re-derivation:
   * the alternative was for the caller to recover this state by
   * string-matching the DISPLAY text (`chip.text === "resident"`), which
   * couples a rendering decision to a copy string that exists to be
   * rewritten. Non-null in exactly the `um.loaded === true` branch, so a
   * registered-but-not-loaded tier can never mark a row. */
  residentModelId: string | null;
  /** A `↳`-prefixed-at-render hint line, present only for the
   * present-but-not-loaded state (the one state where there is something
   * actionable to tell the reader: the model isn't resident yet, but
   * dispatching still works — the FIRST dispatch just pays the load cost). */
  hint?: string;
}

const UTILITY_HANDLES = "compaction · mission-compile · estimate · scribe";

/** The `darkmux/utility` node — viewer.html:1810-1822 (legacy shape;
 * superseded on the React port by the operator-approved kv/chip redesign,
 * `MachineLens.tsx`'s render of this view). `isLocalSpecs` is
 * `isLocalMachine(m)` (viewer.html:2628): only a specs-confirmed local
 * machine gets real utility-tier state; everything else reads "not
 * reported" (never fabricated residency for a remote/unresolved machine).
 * The four branch conditions below are unchanged from the retired
 * `utilityLines()` — only the presentation each branch produces changed. */
export function utilityView(specs: MachineSpecs | null, isLocalSpecs: boolean): UtilityView {
  const um = isLocalSpecs ? specs?.utility_model : undefined;
  const base = { name: "darkmux/utility", gloss: "the internal small-model tier", handles: UTILITY_HANDLES };
  if (um === undefined) {
    return {
      ...base,
      model: { value: "not visible from here — local-probe only", isLiveData: false },
      chip: { text: "not reported", severity: "unknown" },
      residentModelId: null,
    };
  }
  if (um === null) {
    return {
      ...base,
      model: { value: "— none on this machine", isLiveData: false },
      chip: { text: "not configured", severity: "unknown" },
      residentModelId: null,
    };
  }
  if (um.loaded) {
    return {
      ...base,
      model: { value: um.id, isLiveData: true },
      chip: { text: "resident", severity: "green" },
      residentModelId: um.id,
    };
  }
  return {
    ...base,
    model: { value: um.id, isLiveData: true },
    chip: { text: "not loaded", severity: "amber" },
    residentModelId: null,
    hint: "loads on first use — the first dispatch pays the model load",
  };
}

/** `limitDescription()` — viewer.html:4896's inline ternary, factored out so
 * every k/v surface naming the limit's source (Stage 2/3's machine detail
 * row, `MachineHealthRegion.tsx`) has one source of truth for the wording. */
export function limitDescription(limitSource: string | null | undefined): string {
  if (limitSource === "budget") return "#1243 budget";
  if (limitSource === "physical_pool") return "physical pool (no budget configured)";
  return "no limit readable";
}

/** `memStampText()` — viewer.html:4879. */
export function stampLine(b: MachineResources): string {
  const gather = b.gather_ms != null ? String(b.gather_ms) : "—";
  const cache = b.cache_ttl_ms != null ? String(b.cache_ttl_ms) : "—";
  return `gather ${gather} ms (zero model dispatches) · server cache ${cache} ms · polled every ${MACHINE_MEM_POLL_MS / 1000}s`;
}

/** The attribution footer line — viewer.html:1930. */
export function attributionLine(b: MachineResources): string {
  return `attribution: ${b.attribution_note || b.attribution || "—"}`;
}

/** `perScale` — viewer.html:1891. The common scale every PER-MODEL row's bar
 * is drawn against: the largest single potential/current figure among all
 * rendered models, so a small model's bar stays legible next to a large one.
 * Re-exported from `machineGauge.ts` for that module's own consumers — see
 * this file's own module doc for why it survived Stage 2/3 unchanged while
 * `machineTotalText`/`modelLines`/`pressureText`/`machineScale` did not. */
export function perModelScale(models: MachineResourcesModel[]): number {
  return Math.max(1, ...models.map((mm) => Math.max(Number(mm.potential_bytes) || 0, Number(mm.current_bytes) || 0)));
}

/** The not-local placeholder sentence — viewer.html:1871. Named export
 * (rather than an inline literal in the component) so the wording has
 * exactly one source. */
export function notLocalMessage(machineName: string): string {
  return `residency / RAM not reported from here — local-probe only. View the machine page on ${machineName || "that machine"} directly for live figures.`;
}

/** The daemon-unreachable-with-no-cached-data placeholder — viewer.html:1873. */
export const DAEMON_UNREACHABLE_MESSAGE =
  "daemon not reachable — the machine lens reads live probes via /machine/resources (CLI twin: darkmux machine resources).";

/** The first-fetch-in-flight placeholder — viewer.html:1875. */
export const LOADING_MESSAGE = "loading…";

/** The stale-cached-snapshot banner — viewer.html:1880. Stage 2/3 renders
 * it as `.mm-stalebanner` above the desaturated hero, not as a `.memwarn`
 * line: legacy showed it inside the ledger card because the ledger WAS the
 * page, whereas the banner now has to caption an entire instrument cluster
 * whose figures are all equally stale. */
export const STALE_BANNER_TEXT = "⚠ daemon unreachable — showing the last snapshot; the figures below are stale";
