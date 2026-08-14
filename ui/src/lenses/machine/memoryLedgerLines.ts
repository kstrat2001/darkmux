/**
 * Text/data-builders for the machine lens's health region — the
 * `darkmux/utility` node plus the `/machine/resources` memory ledger
 * (`renderMachine()`'s `util`/`health` assembly, viewer.html:1796-1901, and
 * the `renderMachineMemModel`/`memBar`/`memStampText` helpers at
 * viewer.html:4853-4897).
 *
 * (#1806 Stage 1) Before this packet, the health region flattened every
 * field into ONE string array, one `<div>` per line (`healthLines()`, since
 * removed — see `MachineLens.tsx`'s `Lines` doc for why that shape existed:
 * `innerText` line breaks that don't depend on flex behavior). This module
 * now exports the SAME text — same computations, same strings, same order —
 * but GRANULARLY, so `MemLedgerCards.tsx` can slot each fragment into real
 * structure (`.memcard`/`.membar`/`.memwarn`/`#memstamp`, legacy's own
 * class names — `crates/darkmux-serve/assets/viewer.html` CSS 274-303,
 * render 1870-1930/4946-4973) instead of re-deriving anything. Every
 * exported function here is a straight extraction of a fragment that used
 * to be one `lines.push(...)` call in the retired `healthLines()` — the
 * text a fragment produces is unchanged, which is what keeps `innerText`
 * (and therefore the parity goldens, `tests/parity/goldens/machine*.txt`)
 * byte-identical to before this restructuring. See `MemLedgerCards.tsx`'s
 * own doc for how the pieces assemble and why the DOM nesting itself cannot
 * introduce a stray line break (verified against legacy's OWN golden, which
 * already nests these exact absolutely-positioned bar elements between text
 * lines with zero blank-line drift — `.pot`/`.cur`/`.lim`/`.poolline` carry
 * no text node of their own).
 *
 * `.memowner`/`.memstate` are `text-transform: uppercase` in legacy CSS
 * (`crates/darkmux-serve/assets/viewer.html`, `.memowner`/`.memstate`
 * rules) — the TEXT content itself is lowercase (`owner`/`state` verbatim
 * from `/machine/resources`); only the rendering is uppercased. This port
 * uppercases the STRING directly (see `format.ts`'s module doc) rather than
 * depending on a stylesheet rule the port is free to change.
 */

import { memBytes } from "../../lib/format";
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

/** `limitDescription()` — viewer.html:4896's inline ternary, factored out
 * so the machine-card meta line (`machineTotalText` below) has one source
 * of truth for the wording. */
export function limitDescription(limitSource: string | null | undefined): string {
  if (limitSource === "budget") return "#1243 budget";
  if (limitSource === "physical_pool") return "physical pool (no budget configured)";
  return "no limit readable";
}

/** The per-model TEXT lines — `renderMachineMemModel()`'s header/meta text,
 * viewer.html:4963-4973. Order: identifier, OWNER, STATE, meta, [hint]. The
 * hint line (when present) already carries its `↳ ` prefix baked in, same
 * as legacy's own template — a caller that wants the RAW hint (to prefix it
 * itself, matching `machineTotalText`'s shape) should read
 * `m.shrink_hint` directly rather than parsing this array's last element. */
export function modelLines(m: MachineResourcesModel): string[] {
  const kv = m.kv_bytes_at_ctx != null ? `kv@ctx ${memBytes(m.kv_bytes_at_ctx)}` : "kv unknown (no arch facts)";
  const lines = [
    m.identifier || m.model_key,
    (m.owner || "").toUpperCase(),
    (m.state || "unknown").toUpperCase(),
    `ctx ${m.loaded_ctx} · weights ${memBytes(m.weights_bytes)} · ${kv} · potential ${memBytes(m.potential_bytes)} · current ${memBytes(m.current_bytes)}`,
  ];
  const hint = (m as { shrink_hint?: string }).shrink_hint;
  if (hint) lines.push(`↳ ${hint}`);
  return lines;
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

/** The machine-total card's STATE chip text + the combined meta line +
 * optional shrink hint (RAW, no `↳ ` prefix — the caller renders that, same
 * convention as `MemLedgerCards.tsx`'s `ModelCard` reading `m.shrink_hint`
 * directly) — the three text fragments the retired `healthLines()` used to
 * push right after the "machine total" label (viewer.html:1898-1902's
 * `machineCard` template, minus the label itself and the `memBar` markup,
 * which `MemLedgerCards.tsx` draws separately from the same raw numbers). */
export interface MachineTotalText {
  stateText: string;
  metaLine: string;
  hint?: string;
}
export function machineTotalText(b: MachineResources): MachineTotalText {
  const machine = b.machine;
  const pool = b.pool;
  const unpriced = Number(machine.unpriced_models) || 0;
  const stateText = (machine.state || "unknown").toUpperCase();
  let metaLine = `Σ potential ${memBytes(machine.potential_bytes)}${unpriced ? ` (+${unpriced} unpriced)` : ""} · Σ current ${memBytes(machine.current_bytes)} · limit ${memBytes(b.limit_bytes)} (${limitDescription(b.limit_source)})`;
  if (pool) metaLine += ` · pool ${memBytes(pool.capacity_bytes)} / free ${memBytes(pool.available_bytes)}`;
  const shrinkHint = (machine as { shrink_hint?: string }).shrink_hint;
  return { stateText, metaLine, hint: shrinkHint || undefined };
}

/** The pressure card's four k/v text fragments — viewer.html:1921-1925's
 * `pressureCard` template, minus markup. `red` gates the "RED" state chip
 * the same way it always has (`pressure.red` alone — see that render's own
 * comment on why swap/compressor never alarm on their own condition). Pairing
 * these into real `.memrow`/`.k`/`.v` structure is explicitly OUT of #1806
 * Stage 1's scope (it would re-pair "swap used" and its value onto one line,
 * changing `innerText` — see `MemLedgerCards.tsx`'s doc); this stays four
 * flat text fragments on purpose. */
export interface PressureText {
  red: boolean;
  swapText: string;
  compressorText: string;
  freeText: string;
}
export function pressureText(pressure: MachineResources["pressure"]): PressureText {
  return {
    red: !!pressure.red,
    swapText: memBytes(pressure.swap_used_bytes),
    compressorText: memBytes(pressure.compressor_bytes),
    freeText: pressure.memory_free_percent != null ? `${Number(pressure.memory_free_percent)}%` : "—",
  };
}

/** `perScale` — viewer.html:1891. The common scale every PER-MODEL bar is
 * drawn against: the largest single potential/current figure among all
 * models, so a small model's bar stays legible next to a large one. */
export function perModelScale(models: MachineResourcesModel[]): number {
  return Math.max(1, ...models.map((mm) => Math.max(Number(mm.potential_bytes) || 0, Number(mm.current_bytes) || 0)));
}

/** `mScale` — viewer.html:1894. The MACHINE bar's own scale: the largest of
 * the limit, the physical pool, and the machine's own potential/current —
 * so both the limit tick and the pool tick land somewhere on the track. */
export function machineScale(limit: number | null, poolCap: number | null, pot: number | null, cur: number | null): number {
  return Math.max(limit || 0, poolCap || 0, pot || 0, cur || 0, 1);
}

/** The not-local placeholder sentence — viewer.html:1871. Named export
 * (rather than an inline literal in `MemLedgerCards.tsx`) so the wording has
 * exactly one source. */
export function notLocalMessage(machineName: string): string {
  return `residency / RAM not reported from here — local-probe only. View the machine page on ${machineName || "that machine"} directly for live figures.`;
}

/** The daemon-unreachable-with-no-cached-data placeholder — viewer.html:1873. */
export const DAEMON_UNREACHABLE_MESSAGE =
  "daemon not reachable — the machine lens reads live probes via /machine/resources (CLI twin: darkmux machine resources).";

/** The first-fetch-in-flight placeholder — viewer.html:1875. */
export const LOADING_MESSAGE = "loading…";

/** The stale-cached-snapshot banner — viewer.html:1880. Rendered as a
 * `.memwarn` line (matching legacy), the same class the warnings card's own
 * lines use — both are "read this, something is off" text. */
export const STALE_BANNER_TEXT = "⚠ daemon unreachable — showing the last snapshot; the figures below are stale";
