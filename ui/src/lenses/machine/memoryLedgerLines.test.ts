import { describe, it, expect } from "vitest";
import {
  attributionLine,
  limitDescription,
  machineScale,
  machineTotalText,
  modelLines,
  perModelScale,
  pressureText,
  stampLine,
} from "./memoryLedgerLines";
import type { MachineResources, MachineResourcesModel } from "../../types/handwritten";

// #1806 Stage 1 refactored the health region's text builders from one flat
// `healthLines()` array into granular exports (`machineTotalText`,
// `pressureText`, `modelLines`, …) so `MemLedgerCards.tsx` can slot each
// fragment into real `.memcard`/`.membar` structure without re-deriving any
// text. These tests pin the exact byte-for-byte strings — the same
// guarantee `tests/parity/goldens/machine*.txt` proves end-to-end in a real
// browser, at the unit level and for the inverted cases those integration
// goldens don't parameterize over (priced vs unpriced, known vs unknown).

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

describe("machineTotalText", () => {
  it("uppercases the state and formats the full meta line, no unpriced tag when count is 0", () => {
    const { stateText, metaLine, hint } = machineTotalText(machineResources());
    expect(stateText).toBe("GREEN");
    expect(metaLine).toBe(
      "Σ potential 24.57 GB · Σ current 19.51 GB · limit 137.44 GB (physical pool (no budget configured)) · pool 137.44 GB / free 3.74 GB",
    );
    expect(hint).toBeUndefined();
  });

  // The inverted case: an unpriced resident. The undercount is STATED in the
  // text, never silently absorbed — PROVENANCE.md's central honesty rule.
  it("appends the unpriced count to the potential figure when models are unpriceable", () => {
    const { metaLine } = machineTotalText(
      machineResources({ machine: { potential_bytes: 20000000000, unpriced_models: 1, current_bytes: 10000000000, state: "unknown" } }),
    );
    expect(metaLine).toContain("(+1 unpriced)");
  });

  it("carries the RAW shrink hint, with no prefix baked in — the caller adds the arrow", () => {
    const { hint } = machineTotalText(
      machineResources({
        machine: { potential_bytes: 1, unpriced_models: 0, current_bytes: 1, state: "amber", ...({ shrink_hint: "shrink several contexts" } as object) },
      }),
    );
    expect(hint).toBe("shrink several contexts");
  });

  it("uppercases 'unknown' honestly rather than hiding a missing state", () => {
    const { stateText } = machineTotalText(machineResources({ machine: { potential_bytes: 1, unpriced_models: 0, current_bytes: 1, state: "" as string } }));
    expect(stateText).toBe("UNKNOWN");
  });
});

describe("pressureText", () => {
  it("formats all four fragments and reports red only from pressure.red", () => {
    const t = pressureText({ swap_used_bytes: 5453843005, compressor_bytes: 890290176, memory_free_percent: 88, red: false });
    expect(t).toEqual({ red: false, swapText: "5.45 GB", compressorText: "890 MB", freeText: "88%" });
  });

  it("the inverted case: red flips true only when the server says so", () => {
    const t = pressureText({ swap_used_bytes: 0, compressor_bytes: 0, memory_free_percent: 10, red: true });
    expect(t.red).toBe(true);
  });

  it("renders — for a missing percentage rather than '0%' or 'null%'", () => {
    const t = pressureText({ swap_used_bytes: 0, compressor_bytes: 0, memory_free_percent: null as unknown as number, red: false });
    expect(t.freeText).toBe("—");
  });
});

describe("modelLines", () => {
  it("orders identifier, OWNER, STATE, meta for a priced model", () => {
    const lines = modelLines(model());
    expect(lines).toEqual([
      "darkmux:qwen3.6-35b-a3b-turboquant-mlx",
      "DARKMUX",
      "GREEN",
      "ctx 262144 · weights 18.45 GB · kv@ctx 5.37 GB · potential 24.57 GB · current 19.51 GB",
    ]);
  });

  // The inverted case PROVENANCE.md names explicitly: no readable arch facts
  // means kv AND potential stay null — the meta line says so in words
  // rather than printing a misleading number.
  it("names the reason instead of a dash when kv/potential are unpriced", () => {
    const lines = modelLines(
      model({
        identifier: "microsoft/phi-4",
        owner: "user",
        loaded_ctx: 16384,
        weights_bytes: 9053136497,
        kv_per_token_bytes: null as unknown as number,
        kv_bytes_at_ctx: null as unknown as number,
        potential_bytes: null as unknown as number,
        current_bytes: 2841952256,
        state: "unknown",
      }),
    );
    expect(lines[3]).toBe("ctx 16384 · weights 9.05 GB · kv unknown (no arch facts) · potential — · current 2.84 GB");
  });

  it("appends the prefixed hint line only when shrink_hint is present", () => {
    const withHint = modelLines(model({ ...({ shrink_hint: "reload at ctx 32768" } as object) }));
    expect(withHint[4]).toBe("↳ reload at ctx 32768");
    const without = modelLines(model());
    expect(without).toHaveLength(4);
  });
});

describe("perModelScale / machineScale", () => {
  it("perModelScale is the largest single potential/current figure among models, floored at 1", () => {
    expect(perModelScale([model({ potential_bytes: 100, current_bytes: 50 }), model({ potential_bytes: 30, current_bytes: 200 })])).toBe(200);
  });
  it("perModelScale floors at 1 for an empty model list (never a 0-width track)", () => {
    expect(perModelScale([])).toBe(1);
  });
  it("machineScale takes the largest of limit/pool/potential/current, floored at 1", () => {
    expect(machineScale(100, 50, 30, 20)).toBe(100);
    expect(machineScale(null, null, null, null)).toBe(1);
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
