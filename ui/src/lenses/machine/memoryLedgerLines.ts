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
 */

import { MACHINE_MEM_POLL_MS } from "../../lib/queryKeys";
import type { MachineResources, MachineResourcesModel, MachineSpecs } from "../../types/handwritten";

/** The `darkmux/utility` node — viewer.html:1810-1822. `isLocalSpecs` is
 * `isLocalMachine(m)` (viewer.html:2628): only a specs-confirmed local
 * machine gets real utility-tier state; everything else reads "not
 * reported" (never fabricated residency for a remote/unresolved machine). */
export function utilityLines(specs: MachineSpecs | null, isLocalSpecs: boolean): string[] {
  const um = isLocalSpecs ? specs?.utility_model : undefined;
  let utilState: string;
  let pillText: string;
  if (um === undefined) {
    utilState = "utility tier";
    pillText = "not reported";
  } else if (um === null) {
    utilState = "utility tier · not configured";
    pillText = "not configured";
  } else if (um.loaded) {
    utilState = `utility tier · ${um.id}`;
    pillText = "resident";
  } else {
    utilState = `utility tier · ${um.id}`;
    pillText = "registered · not loaded";
  }
  return ["darkmux/utility", utilState, "compaction · mission-compile · estimate · scribe", pillText];
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
