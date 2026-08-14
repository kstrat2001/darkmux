import { describe, it, expect } from "vitest";
import { attributionLine, limitDescription, perModelScale, stampLine, utilityView } from "./memoryLedgerLines";
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
// Every one of the four `utilityView()` branches gets its own test, each
// pinning: the chip's TEXT and its SEVERITY class (both, not just one — a
// mutation swapping severity alone would slip past a text-only assertion),
// the model row's value AND its live-vs-copy flag, and — since `handles` is
// ALWAYS static copy, never live data — that it never varies across states.
describe("utilityView", () => {
  it("constants: name and gloss never change, handles is always the same static copy", () => {
    const states: MachineSpecs["utility_model"][] = [
      null,
      { id: "darkmux:qwen3-4b", loaded: true },
      { id: "darkmux:qwen3-4b", loaded: false },
    ];
    for (const um of states) {
      const v = utilityView({ utility_model: um } as unknown as MachineSpecs, true);
      expect(v.name).toBe("darkmux/utility");
      expect(v.gloss).toBe("the internal small-model tier");
      expect(v.handles).toBe("compaction · mission-compile · estimate · scribe");
    }
    // ...and the fourth branch (not confirmed local) too.
    const notLocal = utilityView(null, false);
    expect(notLocal.name).toBe("darkmux/utility");
    expect(notLocal.gloss).toBe("the internal small-model tier");
    expect(notLocal.handles).toBe("compaction · mission-compile · estimate · scribe");
  });

  it("'not reported': specs aren't confirmed local — chip neutral, model row explains why with no live data", () => {
    const v = utilityView(null, false);
    expect(v.chip).toEqual({ text: "not reported", severity: "unknown" });
    expect(v.model).toEqual({ value: "not visible from here — local-probe only", isLiveData: false });
    expect(v.hint).toBeUndefined();
  });

  it("'not configured': explicit null utility model — chip neutral, model row says none configured, no live data", () => {
    const specs = { utility_model: null } as unknown as MachineSpecs;
    const v = utilityView(specs, true);
    expect(v.chip).toEqual({ text: "not configured", severity: "unknown" });
    expect(v.model).toEqual({ value: "— none on this machine", isLiveData: false });
    expect(v.hint).toBeUndefined();
  });

  it("'resident': loaded true — chip green, model id is live data, no hint", () => {
    const specs = { utility_model: { id: "darkmux:qwen3-4b", loaded: true } } as unknown as MachineSpecs;
    const v = utilityView(specs, true);
    expect(v.chip).toEqual({ text: "resident", severity: "green" });
    expect(v.model).toEqual({ value: "darkmux:qwen3-4b", isLiveData: true });
    expect(v.hint).toBeUndefined();
  });

  it("'not loaded': registered but not resident — chip amber, model id is STILL live data, hint present", () => {
    const specs = { utility_model: { id: "darkmux:qwen3-4b", loaded: false } } as unknown as MachineSpecs;
    const v = utilityView(specs, true);
    expect(v.chip).toEqual({ text: "not loaded", severity: "amber" });
    expect(v.model).toEqual({ value: "darkmux:qwen3-4b", isLiveData: true });
    expect(v.hint).toBe("loads on first use — the first dispatch pays the model load");
  });

  it("'not reported' and 'not configured' are textually and semantically distinct (can't see it vs it doesn't exist)", () => {
    const notReported = utilityView(null, false);
    const notConfigured = utilityView({ utility_model: null } as unknown as MachineSpecs, true);
    expect(notReported.chip.text).not.toBe(notConfigured.chip.text);
    expect(notReported.model.value).not.toBe(notConfigured.model.value);
  });

  it("the hint line is present in exactly one of the four states", () => {
    const views = [
      utilityView(null, false),
      utilityView({ utility_model: null } as unknown as MachineSpecs, true),
      utilityView({ utility_model: { id: "darkmux:qwen3-4b", loaded: true } } as unknown as MachineSpecs, true),
      utilityView({ utility_model: { id: "darkmux:qwen3-4b", loaded: false } } as unknown as MachineSpecs, true),
    ];
    const withHint = views.filter((v) => v.hint !== undefined);
    expect(withHint).toHaveLength(1);
    expect(withHint[0].chip.text).toBe("not loaded");
  });

  /**
   * `residentModelId` is what the ledger's `utility` row-chip keys on, so a
   * non-null value in any branch OTHER than `loaded === true` would tag a
   * residency row for a model that is not resident — the page asserting
   * something it has not been told. The field's own doc promises exactly
   * this; nothing tested it until a mutation (setting it to `um.id` in the
   * not-loaded branch) passed all 643 tests. That mutation now fails here.
   */
  it("carries residentModelId in EXACTLY the resident state, never the other three", () => {
    const resident = utilityView({ utility_model: { id: "darkmux:qwen3-4b", loaded: true } } as unknown as MachineSpecs, true);
    expect(resident.residentModelId).toBe("darkmux:qwen3-4b");

    // The three inverted cases, each named — a registered-but-unloaded tier
    // is the interesting one: it HAS an id, and the id is still not a
    // licence to mark a row.
    expect(utilityView({ utility_model: { id: "darkmux:qwen3-4b", loaded: false } } as unknown as MachineSpecs, true).residentModelId).toBeNull();
    expect(utilityView({ utility_model: null } as unknown as MachineSpecs, true).residentModelId).toBeNull();
    expect(utilityView(null, false).residentModelId).toBeNull();
  });

  it("residentModelId is the id itself, not the chip's display text — the seam it replaced", () => {
    // It used to be recovered by the caller as `chip.text === "resident"`,
    // which coupled a rendering decision to a copy string that exists to be
    // rewritten. Renaming the chip copy must not disturb the marker.
    const v = utilityView({ utility_model: { id: "darkmux:qwen3-4b", loaded: true } } as unknown as MachineSpecs, true);
    expect(v.residentModelId).toBe(v.model.value);
    expect(v.residentModelId).not.toBe(v.chip.text);
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
