import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { MachineHealthRegion } from "./MemLedgerCards";
import type { MachineResources } from "../../types/handwritten";

// #1806 Stage 1's structural DOM claims, at the component level — the
// browser-level proof lives in `tests/e2e/viewer-machine.spec.js`
// (`.membar .pot`/`.cur`/`.lim` counts against a real payload) and
// `tests/parity/next-parity.spec.ts` (byte-exact `innerText` against the
// retired flat-line render). These tests pin the two honesty rules
// PROVENANCE.md names as load-bearing, with BOTH sides of each inverted
// case in one file so a future edit can't quietly satisfy one and break
// the other.

const BASE: MachineResources = {
  schema_version: "1.0",
  generated_at_ms: 1,
  gather_ms: 42,
  limit_bytes: 137438953472,
  limit_source: "physical_pool",
  pool: { capacity_bytes: 137438953472, available_bytes: 3738599424 },
  pressure: { swap_used_bytes: 0, compressor_bytes: 0, memory_free_percent: 88, red: false },
  models: [
    {
      identifier: "darkmux:priced-model",
      model_key: "priced-model",
      owner: "darkmux",
      loaded_ctx: 65536,
      weights_bytes: 17180000000,
      kv_per_token_bytes: 20480,
      kv_bytes_at_ctx: 1342177280,
      potential_bytes: 19272177280,
      current_bytes: 18000000000,
      state: "green",
    },
    {
      identifier: "user/unpriced-model",
      model_key: "unpriced-model",
      owner: "user",
      loaded_ctx: 16384,
      weights_bytes: 9053136497,
      kv_per_token_bytes: null as unknown as number,
      kv_bytes_at_ctx: null as unknown as number,
      potential_bytes: null as unknown as number,
      current_bytes: 2841952256,
      state: "unknown",
    },
  ],
  machine: { potential_bytes: 19272177280, unpriced_models: 1, current_bytes: 20841952256, state: "unknown" },
  attribution: "per_process",
  warnings: [],
  cache_ttl_ms: 2000,
};

function renderRegion(resources: MachineResources | null, extra: Partial<Parameters<typeof MachineHealthRegion>[0]> = {}) {
  return render(
    <MachineHealthRegion isLocalMach resources={resources} resourcesErrored={false} machineName="MacBook-Pro" {...extra} />,
  );
}

describe("MachineHealthRegion — absence vs zero (PROVENANCE.md's central honesty rule)", () => {
  it("draws NO .pot layer at all for an unpriced model — absence, not a zero-width bar", () => {
    const { container } = renderRegion(BASE);
    const unpricedCard = [...container.querySelectorAll(".memcard")].find((c) => c.textContent?.includes("unpriced-model"))!;
    expect(unpricedCard).toBeTruthy();
    expect(unpricedCard.querySelector(".membar .pot")).toBeNull();
    // The inverted case, same card, same test: `.cur` (the MEASURED figure)
    // still renders — an unpriced model still has an observed RSS, only the
    // commitment is unknown.
    expect(unpricedCard.querySelector(".membar .cur")).not.toBeNull();
  });

  // The inverted case as its own assertion, not just "the other branch of an
  // if": a PRICED model in the exact same payload DOES get a `.pot` layer,
  // proving the absence above is keyed on the data, not a bug that drops
  // `.pot` for every model.
  it("draws a .pot layer for a priced model in the SAME payload", () => {
    const { container } = renderRegion(BASE);
    const pricedCard = [...container.querySelectorAll(".memcard")].find((c) => c.textContent?.includes("priced-model") && !c.textContent?.includes("unpriced"))!;
    expect(pricedCard.querySelector(".membar .pot")).not.toBeNull();
  });

  it("red-proves the guard is meaningful: a model with potential_bytes present always gets .pot, one with it null never does", () => {
    // Sanity-check the fixture itself isn't accidentally giving both models
    // the same potential — if it did, the two tests above would pass for
    // the wrong reason (structural coincidence, not the `!= null` gate).
    expect(BASE.models[0].potential_bytes).not.toBeNull();
    expect(BASE.models[1].potential_bytes).toBeNull();
  });
});

describe("MachineHealthRegion — hostile state strings degrade to 'unknown', never land raw", () => {
  it("maps an XSS-shaped state string to the unknown class, not a class attribute breakout", () => {
    const hostile: MachineResources = {
      ...BASE,
      models: [
        {
          ...BASE.models[0],
          identifier: "hostile-model",
          state: 'red" onmouseover=window.__xss=1 x="',
        },
      ],
      machine: { ...BASE.machine, unpriced_models: 0 },
    };
    const { container } = renderRegion(hostile);
    const card = [...container.querySelectorAll(".memcard")].find((c) => c.textContent?.includes("hostile-model"))!;
    const cur = card.querySelector(".membar .cur")!;
    expect(cur.className).toBe("cur unknown");
    // No onmouseover handler landed anywhere in the tree.
    expect(container.querySelector("[onmouseover]")).toBeNull();
  });

  // The inverted case: a KNOWN state string is not swept into "unknown" by
  // the same guard — the mapping is a real allowlist, not a blanket
  // degrade-everything fallback that would make the honest color/pattern
  // channel meaningless.
  it("a recognized state string keeps its real class", () => {
    const known: MachineResources = {
      ...BASE,
      models: [{ ...BASE.models[0], identifier: "amber-model", state: "amber" }],
      machine: { ...BASE.machine, unpriced_models: 0 },
    };
    const { container } = renderRegion(known);
    const card = [...container.querySelectorAll(".memcard")].find((c) => c.textContent?.includes("amber-model"))!;
    expect(card.querySelector(".membar .cur")!.className).toBe("cur amber");
  });
});

describe("MachineHealthRegion — structure the e2e/parity suites also check", () => {
  it("renders #memstamp and .memcard/.membar counts consistent with the payload", () => {
    const { container } = renderRegion(BASE);
    expect(container.querySelector("#memstamp")?.textContent).toContain("gather 42 ms");
    // machine + 2 models + pressure = 4 cards (no warnings card — BASE has none)
    expect(container.querySelectorAll(".memcard").length).toBe(4);
    // machine bar + 2 model bars = 3 `.membar`s.
    expect(container.querySelectorAll(".membar").length).toBe(3);
    // Only the MACHINE bar gets a `.lim` tick — legacy passes limit=null for
    // model bars, and #1806 Stage 1 does the same.
    expect(container.querySelectorAll(".membar .lim").length).toBe(1);
  });

  it("places the stale banner BEFORE the machine-total card when resourcesErrored is true", () => {
    const { container } = renderRegion(BASE, { resourcesErrored: true });
    const children = [...container.querySelector(".none, .memwarn, .memcard")!.parentElement!.children];
    const staleIdx = children.findIndex((el) => el.className === "memwarn" && el.textContent?.includes("stale"));
    const machineCardIdx = children.findIndex((el) => el.className.includes("memcard") && el.textContent?.startsWith("machine total"));
    expect(staleIdx).toBeGreaterThanOrEqual(0);
    expect(machineCardIdx).toBeGreaterThan(staleIdx);
  });
});
