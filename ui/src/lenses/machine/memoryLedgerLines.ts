/**
 * Line-builders for the machine lens's health region — the `darkmux/utility`
 * node plus the `/machine/resources` memory ledger (`renderMachine()`'s
 * `util`/`health` assembly, viewer.html:1796-1901, and the
 * `renderMachineMemModel`/`memBar`/`memStampText` helpers at
 * viewer.html:4853-4897). Each function returns the VISIBLE lines in
 * top-to-bottom order — see `runLines.ts`'s module doc for why this port
 * represents content as literal line arrays (one `<div>` per line) rather
 * than reproducing legacy's CSS-flex-dependent `innerText` line breaks.
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

function limitDescription(limitSource: string | null | undefined): string {
  if (limitSource === "budget") return "#1243 budget";
  if (limitSource === "physical_pool") return "physical pool (no budget configured)";
  return "no limit readable";
}

function modelLines(m: MachineResourcesModel): string[] {
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
function stampLine(b: MachineResources): string {
  const gather = b.gather_ms != null ? String(b.gather_ms) : "—";
  const cache = b.cache_ttl_ms != null ? String(b.cache_ttl_ms) : "—";
  return `gather ${gather} ms (zero model dispatches) · server cache ${cache} ms · polled every ${MACHINE_MEM_POLL_MS / 1000}s`;
}

export interface HealthLinesParams {
  isLocalMach: boolean;
  machineName: string;
  resources: MachineResources | null; // MACHINE_MEM
  resourcesErrored: boolean; // MACHINE_MEM_ERR
}

/** The residency/RAM health-region body — the `healthBody` branch chain,
 * viewer.html:1835-1900. */
export function healthLines({ isLocalMach, machineName, resources, resourcesErrored }: HealthLinesParams): string[] {
  if (!isLocalMach) {
    return [
      "residency / RAM",
      `residency / RAM not reported from here — local-probe only. View the machine page on ${machineName || "that machine"} directly for live figures.`,
    ];
  }
  if (resourcesErrored && !resources) {
    return ["daemon not reachable — the machine lens reads live probes via /machine/resources (CLI twin: darkmux machine resources)."];
  }
  if (!resources) {
    return ["loading…"];
  }

  const b = resources;
  const lines: string[] = [];
  if (resourcesErrored) {
    lines.push("⚠ daemon unreachable — showing the last snapshot; the figures below are stale");
  }

  // Machine total card.
  const machine = b.machine;
  const pool = b.pool;
  const unpriced = Number(machine.unpriced_models) || 0;
  lines.push("machine total");
  lines.push((machine.state || "unknown").toUpperCase());
  let meta = `Σ potential ${memBytes(machine.potential_bytes)}${unpriced ? ` (+${unpriced} unpriced)` : ""} · Σ current ${memBytes(machine.current_bytes)} · limit ${memBytes(b.limit_bytes)} (${limitDescription(b.limit_source)})`;
  if (pool) meta += ` · pool ${memBytes(pool.capacity_bytes)} / free ${memBytes(pool.available_bytes)}`;
  lines.push(meta);
  const shrinkHint = (machine as { shrink_hint?: string }).shrink_hint;
  if (shrinkHint) lines.push(`↳ ${shrinkHint}`);

  // Per-model cards.
  const models = Array.isArray(b.models) ? b.models : [];
  if (models.length) {
    for (const m of models) lines.push(...modelLines(m));
  } else {
    lines.push("no models loaded.");
  }

  // Pressure card.
  const pressure = b.pressure;
  lines.push("pressure");
  if (pressure.red) lines.push("RED");
  lines.push("swap used");
  lines.push(memBytes(pressure.swap_used_bytes));
  lines.push("compressor");
  lines.push(memBytes(pressure.compressor_bytes));
  lines.push("memory free");
  lines.push(pressure.memory_free_percent != null ? `${Number(pressure.memory_free_percent)}%` : "—");

  // Warnings.
  if (Array.isArray(b.warnings) && b.warnings.length) {
    lines.push("warnings");
    for (const w of b.warnings) lines.push(`⚠ ${w}`);
  }

  lines.push(`attribution: ${b.attribution_note || b.attribution || "—"}`);
  lines.push(stampLine(b));

  return lines;
}
