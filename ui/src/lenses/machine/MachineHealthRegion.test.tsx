import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { MachineHealthRegion } from "./MachineHealthRegion";
import { advanceResidency } from "./machineGauge";
import type { MachineResources } from "../../types/handwritten";

// #1806 Stage 2/3's structural DOM claims, at the component level — the
// browser-level proof lives in `tests/e2e/viewer-machine.spec.js` (XSS
// inertness across every field of a real payload shape) and
// `tests/parity/next-parity.spec.ts` (the narrowed byte-exact comparison —
// see that file's own doc for what still corresponds and what was
// deliberately retired). These tests pin the two honesty rules
// docs/design/machine-lens/provenance.md names as load-bearing, with BOTH sides of each inverted
// case in one file so a future edit can't quietly satisfy one and break
// the other.

const BASE: MachineResources = {
  schema_version: "1.0",
  generated_at_ms: 1000,
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

function residencyRowsFor(resources: MachineResources) {
  return advanceResidency(null, resources.models, resources.generated_at_ms).rows;
}

function renderRegion(resources: MachineResources | null, extra: Partial<Parameters<typeof MachineHealthRegion>[0]> = {}) {
  return render(
    <MachineHealthRegion
      isLocalMach
      resources={resources}
      resourcesErrored={false}
      machineName="MacBook-Pro"
      residencyRows={resources ? residencyRowsFor(resources) : []}
      nowMs={2000}
      {...extra}
    />,
  );
}

describe("MachineHealthRegion — absence vs zero (docs/design/machine-lens/provenance.md's central honesty rule)", () => {
  it("draws NO .mm-row-pot layer at all for an unpriced model — absence, not a zero-width bar", () => {
    const { container } = renderRegion(BASE);
    const unpricedRow = [...container.querySelectorAll(".mm-row")].find((c) => c.textContent?.includes("unpriced-model"))!;
    expect(unpricedRow).toBeTruthy();
    expect(unpricedRow.querySelector(".mm-row-pot")).toBeNull();
    // The inverted case, same row, same test: `.mm-row-cur` (the MEASURED
    // figure) still renders — an unpriced model still has an observed RSS,
    // only the commitment is unknown.
    expect(unpricedRow.querySelector(".mm-row-cur")).not.toBeNull();
    expect(unpricedRow.textContent).toContain("UNPRICED · potential unknown");
  });

  it("draws a .mm-row-pot layer for a priced model in the SAME payload", () => {
    const { container } = renderRegion(BASE);
    const pricedRow = [...container.querySelectorAll(".mm-row")].find((c) => c.textContent?.includes("priced-model") && !c.textContent?.includes("unpriced"))!;
    expect(pricedRow.querySelector(".mm-row-pot")).not.toBeNull();
  });

  it("the gauge draws NO commit tick when Σ priced potential is 0 — no models at all", () => {
    const empty: MachineResources = { ...BASE, models: [], machine: { ...BASE.machine, potential_bytes: 0, unpriced_models: 0 } };
    const { container } = renderRegion(empty, { residencyRows: [] });
    expect(container.querySelector(".mm-gauge-commit")).toBeNull();
  });

  it("the inverted case: a nonzero Σ priced potential DOES draw the commit tick", () => {
    const { container } = renderRegion(BASE);
    expect(container.querySelector(".mm-gauge-commit")).not.toBeNull();
  });
});

describe("MachineHealthRegion — hostile state strings degrade to 'unknown', never land raw", () => {
  it("maps an XSS-shaped state string to the unknown class, not a class attribute breakout", () => {
    const hostile: MachineResources = {
      ...BASE,
      models: [{ ...BASE.models[0], identifier: "hostile-model", state: 'red" onmouseover=window.__xss=1 x="' }],
      machine: { ...BASE.machine, unpriced_models: 0 },
    };
    const { container } = renderRegion(hostile, { residencyRows: residencyRowsFor(hostile) });
    const row = [...container.querySelectorAll(".mm-row")].find((c) => c.textContent?.includes("hostile-model"))!;
    const cur = row.querySelector(".mm-row-cur")!;
    expect(cur.className).toBe("mm-row-cur is-unknown");
    expect(container.querySelector("[onmouseover]")).toBeNull();
  });

  it("a recognized state string keeps its real class — the mapping is a real allowlist, not a blanket degrade", () => {
    const known: MachineResources = {
      ...BASE,
      models: [{ ...BASE.models[0], identifier: "amber-model", state: "amber" }],
      machine: { ...BASE.machine, unpriced_models: 0 },
    };
    const { container } = renderRegion(known, { residencyRows: residencyRowsFor(known) });
    const row = [...container.querySelectorAll(".mm-row")].find((c) => c.textContent?.includes("amber-model"))!;
    expect(row.querySelector(".mm-row-cur")!.className).toBe("mm-row-cur is-amber");
  });
});

describe("MachineHealthRegion — the redline keys on exactly machine.state === 'red'", () => {
  it("lights the redline and the center value when state is red", () => {
    const red: MachineResources = { ...BASE, machine: { ...BASE.machine, state: "red", current_bytes: 140000000000 }, pressure: { ...BASE.pressure, red: true } };
    const { container } = renderRegion(red, { residencyRows: residencyRowsFor(red) });
    expect(container.querySelector(".mm-gauge-redline.lit")).not.toBeNull();
    expect(container.querySelector(".mm-gauge-center-val.lit")).not.toBeNull();
  });

  it("the inverted case: amber does NOT light the redline, even at high current", () => {
    const amber: MachineResources = { ...BASE, machine: { ...BASE.machine, state: "amber", current_bytes: 140000000000 } };
    const { container } = renderRegion(amber, { residencyRows: residencyRowsFor(amber) });
    expect(container.querySelector(".mm-gauge-redline.lit")).toBeNull();
    expect(container.querySelector(".mm-gauge-center-val.lit")).toBeNull();
  });

  it("a stale (errored, cached) snapshot never shows a lit redline — the cached read isn't grounds to alarm live", () => {
    const red: MachineResources = { ...BASE, machine: { ...BASE.machine, state: "red" }, pressure: { ...BASE.pressure, red: true } };
    const { container } = renderRegion(red, { resourcesErrored: true, residencyRows: residencyRowsFor(red) });
    expect(container.querySelector(".mm-gauge-redline.lit")).toBeNull();
  });
});

describe("MachineHealthRegion — #1812: stale keeps the last-good reading, visibly marked", () => {
  it("renders the LAST GOOD figures under a stale banner + desaturation when the latest poll errored", () => {
    const { container, getByText } = renderRegion(BASE, { resourcesErrored: true });
    expect(getByText(/showing the last snapshot/i)).toBeInTheDocument();
    expect(container.querySelector(".mm-hero.is-stale")).not.toBeNull();
    // The reading itself is still there — never blanked.
    expect(container.querySelector(".mm-gauge-center-val")?.textContent).toContain("20.8");
  });

  it("the inverted case: no stale banner and no desaturation when the latest poll succeeded", () => {
    const { container, queryByText } = renderRegion(BASE, { resourcesErrored: false });
    expect(queryByText(/showing the last snapshot/i)).not.toBeInTheDocument();
    expect(container.querySelector(".mm-hero.is-stale")).toBeNull();
  });

  it("falls to the daemon-unreachable placeholder ONLY when there is no last-good payload at all", () => {
    const { getByText } = renderRegion(null, { resourcesErrored: true });
    expect(getByText(/daemon not reachable/i)).toBeInTheDocument();
  });
});

describe("MachineHealthRegion — the machine's own shrink hint", () => {
  it("renders the machine-level shrink_hint, distinct from a per-model one", () => {
    const withHint: MachineResources = { ...BASE, machine: { ...BASE.machine, ...({ shrink_hint: "shrink several contexts" } as object) } };
    const { getByText } = renderRegion(withHint);
    expect(getByText(/shrink several contexts/)).toBeInTheDocument();
  });

  it("the inverted case: only the unpriced ROW's hint renders when the machine itself has none — BASE's unpriced model already contributes one .mm-hint", () => {
    const { container } = renderRegion(BASE);
    expect(container.querySelectorAll(".mm-hint").length).toBe(1);
    expect(container.querySelector(".mm-hint")!.textContent).toContain("unpriceable");
  });
});

describe("MachineHealthRegion — structure the e2e/parity suites also check", () => {
  it("renders #memstamp with the gather figure and groups rows darkmux-first", () => {
    const { container } = renderRegion(BASE);
    expect(container.querySelector("#memstamp")?.textContent).toContain("gather 42 ms");
    const headers = [...container.querySelectorAll(".mm-grouphdr")].map((h) => h.textContent);
    expect(headers[0]).toContain("DARKMUX-MANAGED");
    expect(headers[1]).toContain("USER-LOADED");
  });

  it("places the stale banner before the hero cluster when resourcesErrored is true", () => {
    const { container } = renderRegion(BASE, { resourcesErrored: true });
    const parent = container.querySelector(".mm-stalebanner")!.parentElement!;
    const children = [...parent.children];
    const bannerIdx = children.findIndex((el) => el.className === "mm-stalebanner");
    const heroIdx = children.findIndex((el) => el.className.includes("mm-hero"));
    expect(bannerIdx).toBeGreaterThanOrEqual(0);
    expect(heroIdx).toBeGreaterThan(bannerIdx);
  });

  it("renders the not-local placeholder when isLocalMach is false, never the gauge", () => {
    const { container, getByText } = render(
      <MachineHealthRegion isLocalMach={false} resources={null} resourcesErrored={false} machineName="studio" residencyRows={[]} />,
    );
    expect(getByText(/not reported from here/i)).toBeInTheDocument();
    expect(container.querySelector(".mm-gauge")).toBeNull();
  });
});

describe("MachineHealthRegion — ghost/NEW residency rows (docs/design/machine-lens/proposal.md §8)", () => {
  it("a departed model renders a dimmed DEPARTED row with its last observed figure", () => {
    const first = advanceResidency(null, BASE.models, 1000);
    const second = advanceResidency(first.state, [BASE.models[0]], 2000); // the unpriced model departs
    const { container } = renderRegion(BASE, { residencyRows: second.rows, resourcesErrored: false });
    const ghostRow = [...container.querySelectorAll(".mm-row.is-ghost")].find((r) => r.textContent?.includes("unpriced-model"));
    expect(ghostRow).toBeTruthy();
    expect(ghostRow!.textContent).toContain("DEPARTED");
    expect(ghostRow!.textContent).toContain("last observed");
    // No fill layers on a ghost — it has nothing current to draw.
    expect(ghostRow!.querySelector(".mm-row-cur")).toBeNull();
  });

  it("an arriving model carries a NEW chip for exactly its first rendered poll", () => {
    const first = advanceResidency(null, [BASE.models[0]], 1000);
    const second = advanceResidency(first.state, BASE.models, 2000); // the unpriced model arrives
    const { container } = renderRegion(BASE, { residencyRows: second.rows });
    const newRow = [...container.querySelectorAll(".mm-row.is-new")].find((r) => r.textContent?.includes("unpriced-model"));
    expect(newRow).toBeTruthy();
    expect(newRow!.textContent).toContain("NEW · first seen");
  });

  it("the inverted case: a steady-state row carries neither chip", () => {
    const first = advanceResidency(null, BASE.models, 1000);
    const second = advanceResidency(first.state, BASE.models, 2000);
    const { container } = renderRegion(BASE, { residencyRows: second.rows });
    expect(container.querySelectorAll(".mm-row.is-new").length).toBe(0);
    expect(container.querySelectorAll(".mm-row.is-ghost").length).toBe(0);
  });
});

/**
 * (final merge gate) The three surfaces the parity retirement left at ZERO
 * coverage, found by mutation rather than by reading: deleting the machine
 * k/v row, deleting the attribution footer, and — the one that matters —
 * swapping `limitDescription(b.limit_source)` for a literal all passed the
 * entire suite. The retired golden region had been covering them by byte
 * equality, and the retirement's own justification claimed a replacement
 * that did not exist for these three.
 *
 * The limit-source case is not cosmetic. Rendering a fixed string there
 * makes the page state that a #1243 budget is configured on a machine whose
 * limit is the physical pool — a fabricated claim about where a limit came
 * from, on the surface whose entire job is provenance (#44: "the operator
 * never has to wonder where a decision came from").
 */
describe("MachineHealthRegion — the k/v row and footer the retired golden used to cover", () => {
  it("names the ACTUAL limit source, not a fixed string", () => {
    const { container } = renderRegion(BASE);
    const kv = container.querySelector(".mm-kv--machine");
    expect(kv).toBeTruthy();
    expect(kv!.textContent).toContain("physical pool (no budget configured)");
    expect(kv!.textContent).not.toContain("budget configured (");
  });

  // The inverted case — without it, a hardcoded "physical pool" literal would
  // satisfy the assertion above and the mutation would go undetected again.
  it("follows limit_source when it changes, rather than hardcoding one answer", () => {
    const { container } = renderRegion({ ...BASE, limit_source: "budget" });
    const kv = container.querySelector(".mm-kv--machine")!;
    expect(kv.textContent).not.toContain("physical pool");
  });

  it("reconciles the page's two byte conventions on the pool figure (#1811)", () => {
    const { container } = renderRegion(BASE);
    const kv = container.querySelector(".mm-kv--machine")!;
    // The header says "128 GB" (binary) and this row says "137.44 GB"
    // (decimal) for the SAME `hw.memsize`. The parenthetical is what tells a
    // reader those are one number.
    expect(kv.textContent).toContain("137.44 GB (128 GiB)");
  });

  it("distinguishes pool CAPACITY from pool FREE — they are different fields", () => {
    const { container } = renderRegion(BASE);
    const kv = container.querySelector(".mm-kv--machine")!;
    // 137438953472 vs 3738599424 — rendering `available` where `capacity`
    // belongs was the third undetected mutation.
    expect(kv.textContent).toContain("137.44 GB");
    expect(kv.textContent).toContain("3.74 GB");
  });

  it("renders the attribution footer — the observer's own cost disclosure", () => {
    const { container } = renderRegion(BASE);
    const feet = [...container.querySelectorAll(".memfoot")];
    expect(feet.length).toBeGreaterThanOrEqual(2);
    expect(feet.some((f) => /attribution:/i.test(f.textContent ?? ""))).toBe(true);
  });
});
