import { describe, it, expect } from "vitest";
import { attributionLine, limitDescription, perModelScale, stampLine, utilityModelId } from "./memoryLedgerLines";
import type { MachineResources, MachineResourcesModel, MachineSpecs } from "../../types/handwritten";

// #1806 Stage 1 refactored the health region's text builders from one flat
// `healthLines()` array into granular exports; #1806 Stage 2/3 (the
// gauge/lamp/odometer/row redesign) then retired the fragments that existed
// only to feed the flattened `.memcard` render (`machineTotalText`,
// `modelLines`, `pressureText`, `machineScale` — see this module's own doc)
// in favor of `machineGauge.ts`'s equivalents, which carry their OWN test
// coverage (`machineGauge.test.ts`). What remains here is what Stage 2/3
// still reads verbatim: the utility-tier lines, the limit-source wording,
// the attribution/stamp footer, and the shared per-model scale.

function machineResources(overrides: Partial<MachineResources> = {}): MachineResources {
  return {
    schema_version: "1.0",
    generated_at_ms: 1,
    gather_ms: 42,
    limit_bytes: 137438953472,
    limit_source: "physical_pool",
    pool: { capacity_bytes: 137438953472, available_bytes: 3738599424 },
    pressure: { swap_used_bytes: 5453843005, compressor_bytes: 890290176, memory_free_percent: 88, red: false },
    models: [],
    machine: { potential_bytes: 24565385183, unpriced_models: 0, current_bytes: 19506757632, state: "green" },
    attribution: "per_process",
    warnings: [],
    cache_ttl_ms: 2000,
    ...overrides,
  };
}

function model(overrides: Partial<MachineResourcesModel> = {}): MachineResourcesModel {
  return {
    identifier: "darkmux:qwen3.6-35b-a3b-turboquant-mlx",
    model_key: "qwen3.6-35b-a3b-turboquant-mlx",
    owner: "darkmux",
    loaded_ctx: 262144,
    weights_bytes: 18446676063,
    kv_per_token_bytes: 20480,
    kv_bytes_at_ctx: 5368709120,
    potential_bytes: 24565385183,
    current_bytes: 19506757632,
    state: "green",
    ...overrides,
  };
}

// Replacement coverage for the operator-approved utility-block redesign
// (`tests/parity/next-parity.spec.ts`'s narrowing doc names this file as
// where the retired byte-exact utility-block assertion's coverage lives).
/**
 * `utilityModelId` is what remains of a whole `utilityView()` four-state
 * structure (and, before that, `utilityLines()`'s four-element array) that
 * rendered as its own card on the machine page. The operator's cut: that
 * card was CONFIG, not machine state — it described what the tier is
 * responsible for, not how it relates to this machine.
 *
 * So the tests shrink with it. The states did not need re-homing: `resident`
 * was already proven by the ledger row's existence, `not loaded` and `not
 * configured` are answered by `darkmux doctor` (with a fix hint), and `not
 * reported` duplicated the page-level not-local placeholder. What is left to
 * pin is the locality guard — the one honesty rule that survived — and that
 * a missing binding yields `null` rather than a fabricated id.
 */
describe("utilityModelId", () => {
  const specs = (utility_model: unknown) => ({ utility_model }) as unknown as MachineSpecs;

  it("returns the configured id for a specs-confirmed local machine", () => {
    expect(utilityModelId(specs({ id: "darkmux:qwen3-4b", loaded: true }), true)).toBe("darkmux:qwen3-4b");
  });

  it("ignores `loaded` entirely — a row exists iff the model is resident, so identity is the only question", () => {
    // Same id either way. A configured-but-unloaded tier simply has no row
    // to match downstream, so it badges nothing without a state machine.
    expect(utilityModelId(specs({ id: "darkmux:qwen3-4b", loaded: false }), true)).toBe("darkmux:qwen3-4b");
  });

  it("never reports a utility tier for a machine it cannot see — the locality guard", () => {
    // The inverted case that matters: the SAME payload, not confirmed local.
    expect(utilityModelId(specs({ id: "darkmux:qwen3-4b", loaded: true }), false)).toBeNull();
  });

  it("yields null rather than a fabricated id when nothing is configured", () => {
    expect(utilityModelId(specs(null), true)).toBeNull();
    expect(utilityModelId(null, true)).toBeNull();
  });
});

describe("limitDescription", () => {
  it("names the #1243 budget source", () => {
    expect(limitDescription("budget")).toBe("#1243 budget");
  });
  it("names the physical-pool fallback", () => {
    expect(limitDescription("physical_pool")).toBe("physical pool (no budget configured)");
  });
  it("falls back to 'no limit readable' for anything else, including absence", () => {
    expect(limitDescription("something-unrecognized")).toBe("no limit readable");
    expect(limitDescription(null)).toBe("no limit readable");
    expect(limitDescription(undefined)).toBe("no limit readable");
  });
});

describe("perModelScale", () => {
  it("is the largest single potential/current figure among models, floored at 1", () => {
    expect(perModelScale([model({ potential_bytes: 100, current_bytes: 50 }), model({ potential_bytes: 30, current_bytes: 200 })])).toBe(200);
  });
  it("floors at 1 for an empty model list (never a 0-width track)", () => {
    expect(perModelScale([])).toBe(1);
  });
});

describe("stampLine / attributionLine", () => {
  it("formats the observer-cost stamp with the fixed poll cadence", () => {
    expect(stampLine(machineResources({ gather_ms: 541, cache_ttl_ms: 2000 }))).toBe(
      "gather 541 ms (zero model dispatches) · server cache 2000 ms · polled every 5s",
    );
  });
  it("falls back to — for missing gather/cache figures", () => {
    expect(stampLine(machineResources({ gather_ms: null as unknown as number, cache_ttl_ms: null as unknown as number }))).toBe(
      "gather — ms (zero model dispatches) · server cache — ms · polled every 5s",
    );
  });
  it("prefers attribution_note over the bare attribution code", () => {
    expect(attributionLine(machineResources({ attribution: "per_process", attribution_note: "3 worker(s) rank-matched" }))).toBe(
      "attribution: 3 worker(s) rank-matched",
    );
  });
  it("falls back to the bare code, then —, when the note is absent", () => {
    expect(attributionLine(machineResources({ attribution: "per_process" }))).toBe("attribution: per_process");
    expect(attributionLine(machineResources({ attribution: "" }))).toBe("attribution: —");
  });
});
