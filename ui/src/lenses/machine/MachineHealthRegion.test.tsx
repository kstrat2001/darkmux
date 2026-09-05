import { describe, it, expect, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";
import { MachineHealthRegion } from "./MachineHealthRegion";
import { advanceResidency } from "./machineGauge";
import type { MachineResources, MachineResourcesModel } from "../../types/handwritten";

// #1806 Stage 2/3's structural DOM claims, at the component level — the
// browser-level proof lives in `tests/e2e/viewer-machine.spec.js` (XSS
// inertness across every field of a real payload shape) and
// `tests/parity/next-parity.spec.ts` (the narrowed byte-exact comparison —
// see that file's own doc for what still corresponds and what was
// deliberately retired). These tests pin the two honesty rules
// docs/design/machine-lens/provenance.md names as load-bearing, with BOTH sides of each inverted
// case in one file so a future edit can't quietly satisfy one and break
// the other.


/** The center readout's cells are seven-segment `<g>`s carrying a
 * `translate(x y) scale(sx sy)` into the canonical 60x100 glyph cell (or a
 * `<circle>` for the decimal point). Their drawn extent is therefore
 * `x .. x + sx*60`, which this recovers so the centering assertions can
 * stay geometric rather than being softened into "roughly centered". */
function odoCellExtents(container: HTMLElement): { left: number; right: number }[] {
  return [...container.querySelectorAll(".mm-gauge-odo-cell")].map((n) => {
    const t = n.getAttribute("transform");
    if (t) {
      const [, x, sx] = /translate\(([-\d.]+) [-\d.]+\) scale\(([-\d.]+)/.exec(t)!;
      return { left: Number(x), right: Number(x) + Number(sx) * 60 };
    }
    const cx = Number(n.getAttribute("cx")), r = Number(n.getAttribute("r"));
    return { left: cx - r, right: cx + r };
  });
}

const BASE: MachineResources = {
  schema_version: "1.0",
  generated_at_ms: 1000,
  gather_ms: 42,
  limit_bytes: 137438953472,
  limit_source: "physical_pool",
  pool: { capacity_bytes: 137438953472, used_bytes: 69300000000, available_bytes: 72000000000, free_bytes: 3738599424 },
  pressure: { swap_used_bytes: 0, compressor_bytes: 0, margin_percent: 88, red: false },
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
  machine: { potential_bytes: 19272177280, unpriced_models: 1, estimated_models: 0, current_bytes: 20841952256, state: "unknown" },
  attribution: "per_process",
  messages: [],
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

  /**
   * The commit TICK is gone — with darkmux on its own inner ring and its
   * growth as a hatched band, the tick was a third rendering of one fact on a
   * face already too crowded to read. The honesty rule it carried moves with
   * it: nothing committed beyond what is held now means NO growth band and no
   * legend entry for one, rather than a zero-width band.
   */
  it("draws NO growth band and no committed legend entry when there is nothing beyond current", () => {
    const nothingPending: MachineResources = {
      ...BASE,
      models: [],
      machine: { ...BASE.machine, potential_bytes: 0, unpriced_models: 0 },
    };
    const { container } = renderRegion(nothingPending, { residencyRows: [] });
    expect(container.querySelector(".mm-gauge-growth")).toBeNull();
    expect(container.querySelector(".mm-legend-sw.is-growth")).toBeNull();
    // ...and the tick itself never returns.
    expect(container.querySelector(".mm-gauge-commit")).toBeNull();
  });

  it("the inverted case: a commitment beyond current DOES draw the band and name it", () => {
    // BASE is a fully-materialised fixture (committed 17.95 < current 19.41),
    // so it correctly has NO growth. Give it something still to claim.
    const pending: MachineResources = {
      ...BASE,
      machine: { ...BASE.machine, potential_bytes: 60_000_000_000 },
    };
    const { container } = renderRegion(pending, { residencyRows: residencyRowsFor(pending) });
    expect(container.querySelector(".mm-gauge-growth")).not.toBeNull();
    expect(container.querySelector(".mm-legend-sw.is-growth")).not.toBeNull();
    expect(container.querySelector(".mm-legend")!.textContent).toMatch(/committed/);
  });

  it("names every band it draws — including 'other', now that it IS drawn", () => {
    const { container } = renderRegion(BASE);
    const legend = container.querySelector(".mm-legend")!;
    expect(legend.textContent).toMatch(/darkmux/);
    // The inverse of what this asserted under the rings: this segment was
    // undrawn there on the theory the gap showed it, and was effectively
    // invisible. Stacked, it is a real segment and must be named.
    expect(legend.textContent).toMatch(/\bother\b/i);
    // The readout owns the machine's own figure — the legend covers only the
    // segments the readout does not explain, so `used` is not restated here.
    expect(legend.textContent).not.toMatch(/\bmachine\b/i);
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

/**
 * The gauge face's two 2026-08-14 fixes, both raised by the operator against
 * the live page and both structural rather than cosmetic.
 *
 * The `state` on every payload here is BASE's own `"unknown"` — deliberately.
 * That is the state a real machine reports (any unpriceable resident makes the
 * ledger decline to promise a fit — provenance finding 1), and under the old
 * code it painted the fill dim gray at every fill level, which is what the
 * operator actually saw and reported. A test that only exercised green/red
 * payloads would have passed against the broken version.
 */
describe("MachineHealthRegion — the arc's color is a fixed ramp, never a verdict", () => {
  // The fill used to be `gaugeFillSeverity(usedPct)` — three buckets with
  // edges at 50% and 85%. Those edges were thresholds darkmux invented, and
  // a machine at 84% and one at 86% are not different in kind. The ramp is
  // now painted across the arc's SWEEP (green at 0, red at the scale end),
  // so the color under any point states only where that point sits.
  //
  // These tests replace the bucket-boundary ones. The claim they protected —
  // the fill answers "how full", never "what the arbiter decided" — is not
  // weakened by the change; it is now true BY CONSTRUCTION, and that is what
  // is asserted.

  it("paints the band from the arc ramp, not from a per-machine color", () => {
    const { container } = renderRegion(BASE);
    expect(container.querySelector(".mm-gauge-val")!.getAttribute("stroke")).toBe("url(#mm-gauge-ramp)");
    expect(container.querySelector("linearGradient#mm-gauge-ramp")).not.toBeNull();
  });

  it("is IDENTICAL under two different server verdicts at the same fill", () => {
    // The strongest form of "the fill is not a verdict": vary only
    // `machine.state` and the drawn arc must not differ by one character.
    const at = (state: string): string => {
      const r: MachineResources = { ...BASE, machine: { ...BASE.machine, state } };
      const { container } = renderRegion(r, { residencyRows: residencyRowsFor(r) });
      return container.querySelector(".mm-gauge-val")!.outerHTML;
    };
    expect(at("unknown")).toBe(at("green"));
    expect(at("unknown")).toBe(at("red"));
  });

  it("moves with the MACHINE's fill, not with darkmux's share of it", () => {
    // The labeling error #1821 exists to fix, expressed as geometry: a dial
    // ending at the machine's `128 LIMIT` must not be driven by a quantity
    // that is only ever a fraction of it. darkmux barely present; the
    // MACHINE nearly full.
    const busyElsewhere: MachineResources = {
      ...BASE,
      machine: { ...BASE.machine, current_bytes: 2_000_000_000 },
      pool: { ...BASE.pool!, used_bytes: 120_000_000_000 },
    };
    const { container } = renderRegion(busyElsewhere, { residencyRows: residencyRowsFor(busyElsewhere) });
    // The needle is the fill's own channel now. 120e9 = 111.8 GiB of the
    // 128 GiB scale = 87.3% -> 157.2 degrees. Driving it from darkmux's 1.6%
    // share would have put it at 2.8.
    const deg = Number(/rotate\(([-\d.]+)/.exec(container.querySelector(".mm-gauge-needle")!.getAttribute("transform")!)![1]);
    expect(deg).toBeGreaterThan(150);
  });

  it("never lands the arbiter's verdict on the fill or the needle", () => {
    const { container } = renderRegion(BASE);
    // No severity class survives on either — the classes are gone from the
    // markup entirely, not merely unset for this fixture.
    expect(container.querySelector(".mm-gauge-val")!.getAttribute("class")).toBe("mm-gauge-val");
    expect(container.querySelector(".mm-gauge-needle")!.getAttribute("class")).toBe("mm-gauge-needle");
  });

  it("states no verdict anywhere on the gauge face", () => {
    // The verdict chip is gone by operator decision: it interpreted data the
    // reader can already see. The lamp row still carries server-declared
    // CONDITIONS (pressure, over-limit, unpriced) — those are facts, not an
    // assessment of whether the machine is doing well.
    const full: MachineResources = { ...BASE, machine: { ...BASE.machine, state: "unknown", current_bytes: 130000000000 } };
    const { container } = renderRegion(full, { residencyRows: residencyRowsFor(full) });
    expect(container.querySelector(".mm-gcap")).toBeNull();
    expect(container.querySelector(".mm-chip")).toBeNull();
    expect(container.textContent).not.toMatch(/\bmachine total\b/i);
    // …while the lamp row is untouched.
    expect(container.querySelectorAll(".mm-lamp").length).toBeGreaterThan(0);
  });
});

describe("MachineHealthRegion — the pressure tiles' seven-segment readout", () => {
  it("renders a decimal point as its own dot cell, between digit cells", () => {
    // 7.22 GiB of swap: the tile figure carries a `.`, which is drawn as a
    // `.mm-odo-dot` span rather than a segment glyph. This is the FOOTER
    // odometer's own branch (the hero readout has a separate copy) — it had
    // never fired in a test before this one.
    const swapped: MachineResources = {
      ...BASE,
      pressure: { ...BASE.pressure, swap_used_bytes: 7_752_000_000 },
    };
    const { container } = renderRegion(swapped, { residencyRows: residencyRowsFor(swapped) });
    const tiles = container.querySelectorAll(".mm-odo-cells");
    expect(tiles.length).toBeGreaterThan(0);
    const dots = container.querySelectorAll(".mm-odo-cells .mm-odo-dot");
    expect(dots.length).toBeGreaterThan(0);
    // and the sr-only text carries the full figure with its point
    const sr = Array.from(container.querySelectorAll(".mm-sr-only")).map((n) => n.textContent ?? "");
    expect(sr.some((t) => /7\.2\d/.test(t))).toBe(true);
  });

  it("draws NO dot cell when no tile figure carries a decimal point", () => {
    const { container } = renderRegion(BASE);
    expect(container.querySelectorAll(".mm-odo-cells .mm-odo-dot").length).toBe(0);
  });
});

describe("MachineHealthRegion — the center readout is centered on the hub", () => {
  it("centers the digit cells on the gauge's own axis, independently of the unit beside them", () => {
    const { container } = renderRegion(BASE);
    const cells = odoCellExtents(container);
    expect(cells.length).toBe(4); // "19.4"
    const left = Math.min(...cells.map((c) => c.left));
    const right = Math.max(...cells.map((c) => c.right));
    // CX is 120. The old single <text x=120 textAnchor="middle"> centered the
    // number AND the unit as one run, so the figure itself always sat left of
    // the hub by half of " GB" — the nit this fixes. The glyphs changed from
    // boxed digits to seven-segment cells; the layout claim did not.
    expect((left + right) / 2).toBeCloseTo(120, 6);
    // The unit hangs off the right edge and takes no part in that centering.
    expect(Number(container.querySelector(".mm-gauge-center-unit")!.getAttribute("x"))).toBeGreaterThan(right);
  });

  /**
   * The aria narrative must describe the same band a sighted reader sees. Two
   * bugs lived here until the #1821 review: the percentage came from
   * darkmux's share while the figure beside it was the machine's ("87.7 GiB
   * in use … 29% full"), and it cited a dashed tick deleted long before. The
   * needle-vs-readout defect had survived in the one channel nobody looks at.
   */
  it("narrates the machine's figure and the machine's percentage — the same subject", () => {
    const { container } = renderRegion(BASE);
    const aria = container.querySelector(".mm-gauge svg")!.getAttribute("aria-label")!;
    // pool.used 69.3e9 = 64.5 GiB = 50% of the 128 GiB scale.
    expect(aria).toContain("64.5 GiB used");
    expect(aria).toMatch(/50% full/);
    // ...and NOT darkmux's 19.4 GiB / 15%, which is what it used to mix in.
    expect(aria).not.toMatch(/15% full/);
  });

  it("names darkmux's share as a part OF that total, not as the total", () => {
    const { container } = renderRegion(BASE);
    const aria = container.querySelector(".mm-gauge svg")!.getAttribute("aria-label")!;
    expect(aria).toMatch(/Of that, darkmux holds 19\.4 GiB/);
  });

  it("never cites the dashed tick — it was deleted", () => {
    const pending: MachineResources = { ...BASE, machine: { ...BASE.machine, potential_bytes: 60_000_000_000 } };
    const { container } = renderRegion(pending, { residencyRows: residencyRowsFor(pending) });
    const aria = container.querySelector(".mm-gauge svg")!.getAttribute("aria-label")!;
    expect(aria).not.toMatch(/dashed tick/);
    expect(aria).toMatch(/hatched extension beyond the needle/);
  });

  it("never announces a fabricated 0% to a screen reader when the reading is unavailable", () => {
    // `memPct(null, scale)` is 0 by design (it exists so no caller is handed
    // NaN), so an unguarded "% full" clause would tell a screen-reader user
    // the machine is 0% full for the very payload the odometer renders as a
    // single "—". Absence is never zero — including in the channel a sighted
    // reader cannot check against the dial.
    const none: MachineResources = {
      ...BASE,
      machine: { ...BASE.machine, current_bytes: null as unknown as number },
      pool: { ...BASE.pool!, used_bytes: null as unknown as number },
    };
    const { container } = renderRegion(none, { residencyRows: residencyRowsFor(none) });
    const aria = container.querySelector(".mm-gauge svg")!.getAttribute("aria-label")!;
    expect(aria).not.toContain("% full");
    expect(aria).not.toContain("0%");
    expect(aria).toContain("unreadable");

    // The inverted case: a readable reading DOES get its fullness announced.
    const ok = renderRegion(BASE);
    const okAria = ok.container.querySelector(".mm-gauge svg")!.getAttribute("aria-label")!;
    expect(okAria).toContain("% full");
  });

  it("stays centered when the figure's width changes — including the no-data case", () => {
    // The readout's source is the MACHINE's used memory: 130e9 = 121.1 GiB,
    // five cells.
    const wide: MachineResources = { ...BASE, pool: { ...BASE.pool!, used_bytes: 130000000000 } };
    const { container } = renderRegion(wide, { residencyRows: residencyRowsFor(wide) });
    const cells = odoCellExtents(container);
    expect(cells.length).toBe(5);
    expect((Math.min(...cells.map((c) => c.left)) + Math.max(...cells.map((c) => c.right))) / 2).toBeCloseTo(120, 6);

    // Both sources unreadable — the readout falls back through pool.used to
    // machine.current, and with neither present renders "—", never a 0.
    const none: MachineResources = {
      ...BASE,
      machine: { ...BASE.machine, current_bytes: null as unknown as number },
      pool: { ...BASE.pool!, used_bytes: null as unknown as number },
    };
    const r2 = renderRegion(none, { residencyRows: residencyRowsFor(none) });
    const oneCell = odoCellExtents(r2.container);
    expect(oneCell.length).toBe(1); // the "—" cell, not a fabricated 0
    expect((oneCell[0].left + oneCell[0].right) / 2).toBeCloseTo(120, 6);
    // …and it must be VISIBLE as absence. A seven-segment cell handed a
    // character it cannot draw renders empty, which would turn the honest
    // "—" into a blank hub — absence rendered as nothing at all.
    expect(r2.container.querySelectorAll(".mm-gauge-odo-cell polygon").length).toBeGreaterThan(0);
  });
});

describe("MachineHealthRegion — the redline keys on exactly machine.state === 'red'", () => {
  it("lights the redline and the center value when state is red", () => {
    const red: MachineResources = { ...BASE, machine: { ...BASE.machine, state: "red", current_bytes: 140000000000 }, pressure: { ...BASE.pressure, red: true } };
    const { container } = renderRegion(red, { residencyRows: residencyRowsFor(red) });
    expect(container.querySelector(".mm-gauge-redline.lit")).not.toBeNull();
    expect(container.querySelector(".mm-gauge-center-val.lit")).not.toBeNull();
  });

  it("renders the red REASON in the caption slot — the one job that slot has", () => {
    const red: MachineResources = { ...BASE, machine: { ...BASE.machine, state: "red", current_bytes: 140000000000 }, pressure: { ...BASE.pressure, red: true } };
    const { container } = renderRegion(red, { residencyRows: residencyRowsFor(red) });
    expect(container.querySelector(".mm-gauge-center-caption")!.textContent).toBe("RED · PRESSURE");
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
    // The reading itself is still there — never blanked. The readout shows
    // the MACHINE's used memory (what the needle points at), not darkmux's
    // share: pool.used_bytes 69.3e9 = 64.5 GiB.
    //
    // The figure is drawn as seven-segment polygons, so it carries no text
    // to assert on. Asserted through the gauge's own accessible narrative
    // instead, which states the same figure — and which therefore MUST stay
    // in step with the glyphs, making this the stronger place to pin it.
    expect(container.querySelector("svg[role='img']")!.getAttribute("aria-label")).toContain("64.5");
    expect(odoCellExtents(container).length).toBe(4); // "64.5", not blanked
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
    expect(kv!.textContent).toContain("physical pool");
    expect(kv!.textContent).not.toContain("budget configured (");
  });

  // The inverted case — without it, a hardcoded "physical pool" literal would
  // satisfy the assertion above and the mutation would go undetected again.
  it("follows limit_source when it changes, rather than hardcoding one answer", () => {
    const { container } = renderRegion({ ...BASE, limit_source: "budget" });
    const kv = container.querySelector(".mm-kv--machine")!;
    expect(kv.textContent).not.toContain("physical pool");
  });

  /**
   * `used` and `available` overlap by design, so adjacent in one row they
   * read as an addition that exceeds the machine — 152.78 GiB on a 128 GiB
   * box when first shown (#1821). The parenthetical is what refuses that
   * reading, and it must be RENDERED, not merely available: a mutation that
   * dropped it from this row passed all 688 tests while the helper stayed
   * green.
   */
  it("names the reclaimable overlap in the row, so used + available cannot read as a sum", () => {
    const { container } = renderRegion(BASE);
    const kv = container.querySelector(".mm-kv--machine")!;
    expect(kv.textContent).toMatch(/reclaimable/);
    // available 67.06 GiB - free 3.48 GiB = 63.57 GiB counted by both.
    expect(kv.textContent).toContain("63.57 GiB reclaimable");
  });

  it("omits the parenthetical when there is no overlap to explain", () => {
    const flush: MachineResources = {
      ...BASE,
      pool: { ...BASE.pool!, available_bytes: BASE.pool!.free_bytes },
    };
    const { container } = renderRegion(flush);
    expect(container.querySelector(".mm-kv--machine")!.textContent).not.toMatch(/reclaimable/);
  });

  it("renders the pool in binary GiB, agreeing with the header's own figure (#1811)", () => {
    const { container } = renderRegion(BASE);
    const kv = container.querySelector(".mm-kv--machine")!;
    // Same `hw.memsize` the stage header renders as "128 GB". It used to read
    // "137.44 GB" here — one number, two figures, on the one screen whose job
    // is telling the operator how much room they have. The units are the fix;
    // the reconciling " (128 GiB)" parenthetical that used to be asserted here
    // is gone with them.
    expect(kv.textContent).toContain("128.00 GiB");
    expect(kv.textContent).not.toContain("137.44");
  });

  it("distinguishes pool CAPACITY, USED, and AVAILABLE — three different fields (#1821)", () => {
    const { container } = renderRegion(BASE);
    const kv = container.querySelector(".mm-kv--machine")!;
    // capacity 137438953472, used 69300000000, available 72000000000 — all
    // three must render as DIFFERENT numbers, not one figure repeated.
    expect(kv.textContent).toContain("128.00 GiB"); // capacity
    expect(kv.textContent).toContain("64.54 GiB"); // used
    expect(kv.textContent).toContain("67.06 GiB"); // available (colloquial)
    // free_bytes (3738599424 -> "3.48 GiB") stays in the PAYLOAD but is
    // deliberately not given prime space in this row (operator-approved
    // #1821 addendum) — two figures both reading "how much is left" was
    // the defect the rename fixed, not something to preserve under a new
    // label.
    expect(kv.textContent).not.toContain("3.48 GiB");
  });

  it("renders the attribution footer — the observer's own cost disclosure", () => {
    const { container } = renderRegion(BASE);
    const feet = [...container.querySelectorAll(".memfoot")];
    expect(feet.length).toBeGreaterThanOrEqual(2);
    expect(feet.some((f) => /attribution:/i.test(f.textContent ?? ""))).toBe(true);
  });

  // (#2108, operator finding) On a narrow viewport the dotted inline row
  // wrapped into four ragged lines with the ` · ` separators landing
  // mid-line. Mobile renders the SAME facts as a one-item-per-row list
  // instead — proven both by a structural marker (row elements) and by
  // the values still being present, so this can't pass by accident on a
  // component that merely stopped rendering the row altogether.
  it("renders the ledger summary as a definition-style list of rows on a narrow viewport, carrying the same values", () => {
    const { container } = renderRegion(BASE, { isMobileOverride: true });
    const wrap = container.querySelector('[data-act="machine-detail-rows"]');
    expect(wrap).not.toBeNull();
    const rows = wrap!.querySelectorAll(".mm-kv-row");
    expect(rows.length).toBeGreaterThanOrEqual(5); // limit source, pool, used, available, unpriced (+ reclaim note)
    expect(wrap!.textContent).toContain("physical pool");
    expect(wrap!.textContent).toContain("128.00 GiB"); // pool
    expect(wrap!.textContent).toContain("64.54 GiB"); // used
    expect(wrap!.textContent).toContain("67.06 GiB"); // available
    expect(wrap!.textContent).toContain("63.57 GiB reclaimable");
    expect(wrap!.textContent).toContain("model");
    // Each row is label-left/value-right, not one flat text run — the
    // structural claim the mobile form is actually FOR.
    const labelValueRows = [...rows].filter(
      (r) => r.querySelector(".mm-kv-row__label") && r.querySelector(".mm-kv-row__value"),
    );
    expect(labelValueRows.length).toBeGreaterThanOrEqual(5);
  });

  it("stays the inline dotted form (no row markup) on desktop — the mobile branch is opt-in, not the new default", () => {
    const { container } = renderRegion(BASE, { isMobileOverride: false });
    expect(container.querySelector('[data-act="machine-detail-rows"]')).toBeNull();
    expect(container.querySelector(".mm-kv-row")).toBeNull();
    const kv = container.querySelector(".mm-kv--machine")!;
    expect(kv.textContent).toContain("limit source physical pool · pool 128.00 GiB");
  });
});

/**
 * (follow-up to the utility-block redesign) The `utility` row-chip stitches
 * a residency row back to the `darkmux/utility` explainer block below the
 * ledger. `renderRegion`'s BASE fixture already carries a darkmux-owned row
 * (`darkmux:priced-model`) and a user-owned one (`user/unpriced-model`),
 * which is exactly the pair needed to prove the marker is keyed on IDENTITY
 * (a matching id) and not merely on OWNERSHIP (`owner === "darkmux"`) or
 * on any state coloring — a chip with no severity class is the deliberate
 * choice (`isUtilityTierRow`'s own doc): this is an identity label, not a
 * health verdict, and the page's color-means-verified-severity doctrine
 * would be violated by spending green/amber on it.
 */
/**
 * The per-row state chip renders ONLY where the row disagrees with the
 * machine. Deleting it outright was the tempting simplification and would
 * have been wrong — `compute_ledger`'s per-model tint has real divergence
 * branches, and those rows are the only place that fact exists.
 *
 * These are the RENDERED assertions. `rowStateDiffers` is unit-tested in
 * `machineGauge.test.ts`, but a mutation that made this component render the
 * chip unconditionally passed the whole suite until these existed.
 */
/**
 * The pressure tiles' explanatory notes are revealed on demand, not rendered
 * permanently. A `title` tooltip would have been simpler and WRONG: this page
 * is read over the tailnet on a phone, where hover does not exist, so the
 * notes would silently cease to exist on the surface the operator uses most.
 * A button works for tap, hover, and keyboard alike.
 */
describe("MachineHealthRegion — the pressure tiles explain themselves on demand", () => {
  it("renders no note until asked, and every tile offers the affordance", () => {
    const { container } = renderRegion(BASE);
    expect(container.querySelectorAll(".mm-odo-n").length).toBe(0);
    expect(container.querySelectorAll(".mm-odo-i").length).toBe(3);
  });

  it("reveals THAT tile's note on click, and states the relationship for a screen reader", () => {
    const { container } = renderRegion(BASE);
    const btn = container.querySelectorAll(".mm-odo-i")[0] as HTMLButtonElement;
    expect(btn.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(btn);
    expect(btn.getAttribute("aria-expanded")).toBe("true");
    const notes = container.querySelectorAll(".mm-odo-n");
    expect(notes.length).toBe(1); // exactly one — not all three
    expect(notes[0].textContent).toMatch(/only figure that can trigger RED/i);
  });

  it("closes on a second click, and opening another tile closes the first", () => {
    const { container } = renderRegion(BASE);
    const [free, , comp] = [...container.querySelectorAll(".mm-odo-i")] as HTMLButtonElement[];
    fireEvent.click(free);
    fireEvent.click(free);
    expect(container.querySelectorAll(".mm-odo-n").length).toBe(0);

    fireEvent.click(free);
    fireEvent.click(comp);
    const notes = container.querySelectorAll(".mm-odo-n");
    expect(notes.length).toBe(1);
    expect(notes[0].textContent).toMatch(/macOS/);
    expect(free.getAttribute("aria-expanded")).toBe("false");
  });

  it("dismisses on Escape — a popover must close by the gestures every popover supports", () => {
    const { container } = renderRegion(BASE);
    fireEvent.click(container.querySelector(".mm-odo-i")!);
    expect(container.querySelectorAll(".mm-odo-n").length).toBe(1);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(container.querySelectorAll(".mm-odo-n").length).toBe(0);
  });

  it("dismisses on a click outside, but NOT on a click within the tile row", () => {
    const { container } = renderRegion(BASE);
    fireEvent.click(container.querySelector(".mm-odo-i")!);
    // Inside the row: stays open (otherwise the popover would close before
    // its own text could be selected).
    fireEvent.mouseDown(container.querySelector(".mm-odo-n")!);
    expect(container.querySelectorAll(".mm-odo-n").length).toBe(1);
    // Outside: closes.
    fireEvent.mouseDown(document.body);
    expect(container.querySelectorAll(".mm-odo-n").length).toBe(0);
  });

  it("registers no global listeners while closed — an idle page costs nothing", () => {
    const add = vi.spyOn(document, "addEventListener");
    const { container } = renderRegion(BASE);
    const before = add.mock.calls.filter(([e]) => e === "keydown" || e === "mousedown").length;
    expect(before).toBe(0);
    fireEvent.click(container.querySelector(".mm-odo-i")!);
    const after = add.mock.calls.filter(([e]) => e === "keydown" || e === "mousedown").length;
    expect(after).toBe(2);
    add.mockRestore();
  });

  it("is a real button — reachable without a pointer at all", () => {
    const { container } = renderRegion(BASE);
    const btn = container.querySelector(".mm-odo-i")!;
    expect(btn.tagName).toBe("BUTTON");
    expect(btn.getAttribute("type")).toBe("button");
    // #1821 (operator-approved rename): this tile's label is "margin", not
    // "memory free" — kern.memorystatus_level is neither free nor available
    // memory (see the tile's own note test in machineGauge.test.ts).
    expect(btn.getAttribute("aria-label")).toMatch(/margin/i);
  });
});

describe("MachineHealthRegion — the per-row state chip only speaks when it disagrees", () => {
  it("renders NO state chip on a row that agrees with the machine", () => {
    // BASE: machine `unknown`, and the unpriced row is `unknown` too.
    const { container } = renderRegion(BASE);
    const agreeing = [...container.querySelectorAll(".mm-row")].find((r) => r.textContent?.includes("unpriced-model"))!;
    expect(agreeing).toBeTruthy();
    expect(agreeing.querySelector(".mm-row-chip.is-state")).toBeNull();
  });

  it("DOES render it on a row that diverges — a materialized model under machine-amber", () => {
    // The exact `compute_ledger` branch: machine amber, but this model's
    // current has fully materialized its potential, so the row is green.
    const amber: MachineResources = {
      ...BASE,
      machine: { ...BASE.machine, state: "amber" },
      models: BASE.models.map((m) => (m.owner === "darkmux" ? { ...m, state: "green" } : m)),
    };
    const { container } = renderRegion(amber, { residencyRows: residencyRowsFor(amber) });
    const diverging = [...container.querySelectorAll(".mm-row")].find((r) => r.textContent?.includes("priced-model"))!;
    const chip = diverging.querySelector(".mm-row-chip.is-state")!;
    expect(chip).toBeTruthy();
    expect(chip.textContent).toBe("GREEN");
  });

  it("keeps the whole ledger quiet when every row agrees — the everyday case", () => {
    const uniform: MachineResources = {
      ...BASE,
      machine: { ...BASE.machine, state: "green" },
      models: BASE.models.map((m) => ({ ...m, state: "green" })),
    };
    const { container } = renderRegion(uniform, { residencyRows: residencyRowsFor(uniform) });
    expect(container.querySelectorAll(".mm-row-chip.is-state").length).toBe(0);
  });
});

describe("MachineHealthRegion — the utility row-chip (identity marker, never a severity color)", () => {
  it("marks the matching row with a neutral, unclassed chip when the utility model is resident", () => {
    const { container } = renderRegion(BASE, { utilityModelId: "darkmux:priced-model" });
    const row = [...container.querySelectorAll(".mm-row")].find((c) => c.textContent?.includes("priced-model") && !c.textContent?.includes("unpriced"))!;
    const chip = [...row.querySelectorAll(".mm-row-chip")].find((c) => c.textContent === "utility");
    expect(chip).toBeTruthy();
    // Identity marker, not a verdict — no severity class riding along.
    expect(chip!.className).toBe("mm-row-chip is-identity");
    expect(chip!.className).not.toMatch(/is-(green|amber|red|state|warn|new)\b/);
    const otherRow = [...container.querySelectorAll(".mm-row")].find((c) => c.textContent?.includes("unpriced-model"))!;
    expect([...otherRow.querySelectorAll(".mm-row-chip")].some((c) => c.textContent === "utility")).toBe(false);
  });

  it("the inverted case: no row anywhere carries the chip when utilityModelId doesn't match any row", () => {
    const { container } = renderRegion(BASE, { utilityModelId: "darkmux:some-other-model" });
    const chips = [...container.querySelectorAll(".mm-row-chip")].filter((c) => c.textContent === "utility");
    expect(chips).toHaveLength(0);
  });

  it("the inverted case: no chip anywhere when the utility tier isn't resident at all (null, the default)", () => {
    const { container } = renderRegion(BASE);
    const chips = [...container.querySelectorAll(".mm-row-chip")].filter((c) => c.textContent === "utility");
    expect(chips).toHaveLength(0);
  });

  it("never marks a departed (ghost) row even if its identifier matches — a ghost isn't resident", () => {
    const first = advanceResidency(null, BASE.models, 1000);
    const second = advanceResidency(first.state, [BASE.models[1]], 2000); // the darkmux priced model departs
    const { container } = renderRegion(BASE, { residencyRows: second.rows, utilityModelId: "darkmux:priced-model" });
    const ghostRow = [...container.querySelectorAll(".mm-row.is-ghost")].find((r) => r.textContent?.includes("priced-model"))!;
    expect(ghostRow).toBeTruthy();
    expect([...ghostRow.querySelectorAll(".mm-row-chip")].some((c) => c.textContent === "utility")).toBe(false);
  });
});

/**
 * #1819 — an estimated resident (a GGUF download with no readable
 * `config.json`, priced by the size-based fallback instead) is priced, not
 * absent: it must NOT hit the `pot == null` UNPRICED branch anywhere on the
 * page, and it must carry its own provenance disclosure everywhere the
 * verdict appears (row chip, kv line, machine chip, machine detail row).
 * These tests exist alongside — never replacing — the existing UNPRICED
 * coverage above, so a future edit can't quietly satisfy one case while
 * breaking the other.
 */
const ESTIMATED_MODEL: MachineResourcesModel = {
  identifier: "microsoft/phi-4-Q4_K_M",
  model_key: "phi-4-gguf",
  owner: "user",
  loaded_ctx: 8192,
  weights_bytes: 9053136497,
  kv_per_token_bytes: null as unknown as number,
  kv_bytes_at_ctx: null as unknown as number,
  potential_bytes: 11480858097,
  potential_source: "estimated",
  current_bytes: 9200000000,
  state: "green",
};

describe("MachineHealthRegion — #1819 the ESTIMATED resident carries its provenance everywhere the verdict appears", () => {
  function withEstimated(machineOverrides: Partial<MachineResources["machine"]> = {}): MachineResources {
    return {
      ...BASE,
      models: [BASE.models[0], ESTIMATED_MODEL],
      machine: { ...BASE.machine, unpriced_models: 0, estimated_models: 1, state: "green", ...machineOverrides },
    };
  }

  it("renders the ESTIMATED chip, not the UNPRICED one, for a row with a fallback-priced potential", () => {
    const resources = withEstimated();
    const { container } = renderRegion(resources, { residencyRows: residencyRowsFor(resources) });
    const row = [...container.querySelectorAll(".mm-row")].find((c) => c.textContent?.includes("phi-4"))!;
    expect(row.querySelector(".mm-row-chip.is-estimated")).not.toBeNull();
    expect(row.textContent).toContain("ESTIMATED");
    expect(row.querySelector(".mm-row-chip.is-warn")).toBeNull();
    expect(row.textContent).not.toContain("UNPRICED · potential unknown");
    // A priced row DOES draw the commit layer — it is not treated as absent.
    expect(row.querySelector(".mm-row-pot")).not.toBeNull();
  });

  it("the ESTIMATED chip's title states the dense-attention assumption and its consequence", () => {
    const resources = withEstimated();
    const { container } = renderRegion(resources, { residencyRows: residencyRowsFor(resources) });
    const row = [...container.querySelectorAll(".mm-row")].find((c) => c.textContent?.includes("phi-4"))!;
    const chip = row.querySelector(".mm-row-chip.is-estimated")!;
    expect(chip.getAttribute("title")?.toLowerCase()).toContain("dense");
    expect(chip.getAttribute("title")?.toLowerCase()).toContain("hybrid");
  });

  it("the row's kv line marks the estimated potential with `~` and `(estimated)`", () => {
    const resources = withEstimated();
    const { container } = renderRegion(resources, { residencyRows: residencyRowsFor(resources) });
    const row = [...container.querySelectorAll(".mm-row")].find((c) => c.textContent?.includes("phi-4"))!;
    expect(row.textContent).toContain("potential ~10.69 GiB (estimated)");
  });

  it("the row carries an explanatory hint naming the assumption, like the unpriced row's own hint", () => {
    const resources = withEstimated();
    const { container } = renderRegion(resources, { residencyRows: residencyRowsFor(resources) });
    const row = [...container.querySelectorAll(".mm-row")].find((c) => c.textContent?.includes("phi-4"))!;
    const hint = [...row.querySelectorAll(".mm-hint")].find((h) => h.textContent?.includes("estimated:"));
    expect(hint).toBeTruthy();
    expect(hint!.textContent?.toLowerCase()).toContain("dense");
  });

  it("decision 1: an estimated resident is disclosed on its own row and in the machine detail row", () => {
    // #1819 required the count to travel WITH the verdict word everywhere
    // that word appeared. The verdict word is gone from this page (the chip
    // interpreted data the reader can already see), so the requirement now
    // binds on the two channels that remain — and BOTH must carry it, or a
    // reader who glances at the machine line never learns the figure rests
    // on a guess.
    const resources = withEstimated({ state: "green" });
    const { container } = renderRegion(resources, { residencyRows: residencyRowsFor(resources) });
    expect(container.querySelector(".mm-row-chip.is-estimated")).not.toBeNull();
    expect(container.querySelector(".mm-kv--machine")!.textContent).toContain("estimated");
  });

  it("the machine detail row discloses the estimated count alongside (not instead of) the unpriced count", () => {
    const resources = withEstimated();
    const { container } = renderRegion(resources, { residencyRows: residencyRowsFor(resources) });
    const detail = container.querySelector(".mm-kv--machine")!;
    expect(detail.textContent).toContain("unpriced");
    expect(detail.textContent).toContain("estimated");
    // A NON-BREAKING space between the count and its unit, asserted as such.
    // This strip is a flat text run, so the browser may break at any space in
    // it, and at the current type scale it picked the one inside this value —
    // rendering `estimated 1` on one line and `model` alone on the next. The
    // assertion pins the glue rather than normalizing it away: written as a
    // plain space, this test passes against exactly the bug it now guards.
    expect(detail.textContent).toContain("1\u00A0model");
  });

  it("the inverted case: the estimated disclosure is entirely absent when nothing was estimated (BASE's own default)", () => {
    const { container } = renderRegion(BASE);
    expect(container.querySelector(".mm-row-chip.is-estimated")).toBeNull();
    const detail = container.querySelector(".mm-kv--machine")!;
    expect(detail.textContent).not.toContain("estimated");
  });

  it("a genuinely unpriceable resident still forces the machine to UNKNOWN even alongside an estimated one — an estimate never substitutes for a real price", () => {
    const resources: MachineResources = {
      ...BASE,
      // BASE.models[1] is the genuinely unpriced fixture. The estimated
      // row's own `state` is "unknown" here too — matching what the real
      // per-model tint would compute under a machine-Unknown verdict
      // (`compute_ledger`'s tint keys on `machine_state`, not on
      // `potential_source`), so this fixture stays honest about what the
      // server would actually send.
      models: [BASE.models[0], BASE.models[1], { ...ESTIMATED_MODEL, state: "unknown" }],
      machine: { ...BASE.machine, unpriced_models: 1, estimated_models: 1, state: "unknown" },
    };
    const { container } = renderRegion(resources, { residencyRows: residencyRowsFor(resources) });
    // The machine-level verdict has no display channel any more, so what is
    // asserted is that an UNPRICEABLE resident is still disclosed as such —
    // the fact the undecided verdict used to stand for. An estimate must not
    // paper over it.
    expect(container.querySelector(".mm-row-chip.is-warn")!.textContent).toContain("UNPRICED");
    expect(container.querySelector(".mm-kv--machine")!.textContent).toContain("unpriced");
    // The estimated row still renders its own chip even though the MACHINE
    // verdict stayed undecided — provenance is a per-row fact, independent
    // of whether the whole-machine cascade could reach a verdict.
    const estimatedRow = [...container.querySelectorAll(".mm-row")].find((c) => c.textContent?.includes("phi-4"))!;
    expect(estimatedRow.querySelector(".mm-row-chip.is-estimated")).not.toBeNull();
  });
});

describe("MachineHealthRegion — messages[] severity (#1821: an info disclosure must not look like a warning)", () => {
  function findWarnLamp(container: HTMLElement) {
    return [...container.querySelectorAll(".mm-lamp")].find((l) => l.textContent?.includes("WARN"))!;
  }

  it("an info-only message does NOT light the WARN lamp", () => {
    const infoOnly: MachineResources = {
      ...BASE,
      messages: [{ severity: "info", text: "N models priced by ESTIMATE" }],
    };
    const { container } = renderRegion(infoOnly);
    expect(findWarnLamp(container).className).not.toMatch(/is-lit-/);
  });

  it("the inverted case: a warn-severity message DOES light the WARN lamp", () => {
    const withWarn: MachineResources = {
      ...BASE,
      messages: [{ severity: "warn", text: "resident model(s) unpriceable — undercounts" }],
    };
    const { container } = renderRegion(withWarn);
    expect(findWarnLamp(container).className).toMatch(/is-lit-warn/);
  });

  it("an error-severity message also lights the WARN lamp", () => {
    const withError: MachineResources = {
      ...BASE,
      messages: [{ severity: "error", text: "`lms ps` probe failed" }],
    };
    const { container } = renderRegion(withError);
    expect(findWarnLamp(container).className).toMatch(/is-lit-warn/);
  });

  it("the message card renders each entry with a severity-keyed class, not a uniform amber treatment", () => {
    const mixed: MachineResources = {
      ...BASE,
      messages: [
        { severity: "info", text: "estimate disclosure" },
        { severity: "warn", text: "undercount warning" },
      ],
    };
    const { container } = renderRegion(mixed);
    expect(container.querySelector(".memmsg-info")).not.toBeNull();
    expect(container.querySelector(".memmsg-warn")).not.toBeNull();
    // The inverted case: an info message must not ALSO carry the warn class.
    const infoMsg = [...container.querySelectorAll(".memmsg")].find((m) => m.textContent?.includes("estimate disclosure"))!;
    expect(infoMsg.className).not.toMatch(/memmsg-warn/);
  });
});

// (#1854) A resident whose measured footprint outgrew the potential darkmux
// priced it at. Measured live on an IDLE MLX model: 28.40 GiB held against a
// priced 22.88 GiB, steady to the byte across repeated samples. The server
// clamps the projection to what is actually held and flags the row; these
// tests pin the two places the page SAYS so, because a silently-repaired
// estimate is one nobody ever fixes.
const OVER_PRICE: MachineResources = {
  ...BASE,
  models: [
    { ...BASE.models[0], potential_bytes: 24565385183, current_bytes: 30493331456, over_price_bytes: 5927946273, state: "green" },
    BASE.models[1],
  ],
  machine: { ...BASE.machine, over_price_models: 1, state: "green" },
};

describe("MachineHealthRegion — #1854 a resident holding more than darkmux priced it", () => {
  it("says it on the row it is about: the overage, and what the projection now counts", () => {
    const { container } = renderRegion(OVER_PRICE);
    const hints = [...container.querySelectorAll(".mm-hint")].map((n) => n.textContent);
    expect(hints.some((t) => /holds 5\.52 GiB more than priced/.test(t || ""))).toBe(true);
    expect(hints.some((t) => /the fit projection counts the measured 28\.40 GiB/.test(t || ""))).toBe(true);
  });


  it("does NOT flip the row's state chip — the price was wrong, the model is not unhealthy", () => {
    // The severity channel stays reserved for fit severity. A row whose only
    // distinction is an outgrown ESTIMATE must not borrow the vocabulary the
    // machine uses to say "this does not fit" — that mints a second meaning
    // for color, the same argument that rejected forcing the verdict to
    // UNKNOWN. The chip only speaks when a row DISAGREES with the machine.
    const { container } = renderRegion(OVER_PRICE);
    const row = [...container.querySelectorAll(".mm-row")].find((n) => /more than priced/.test(n.textContent || ""));
    expect(row).toBeTruthy();
    expect(row!.querySelector(".mm-row-chip.is-state")).toBeNull();
    // Scoped deliberately: BASE's genuinely-unpriced row DOES disagree with
    // the machine and keeps its UNKNOWN chip, so a page-wide assertion here
    // would pass or fail for the wrong reason.
    expect(container.querySelectorAll(".mm-row-chip.is-state").length).toBe(1);
  });

  it("the inverted case: an ordinary machine's caption and rows are untouched", () => {
    // BASE has no over-price resident, and must gain neither the
    // parenthetical nor a second hint — a marker that renders always carries
    // no information.
    const { container } = renderRegion(BASE);
    const hints = [...container.querySelectorAll(".mm-hint")].map((n) => n.textContent);
    expect(hints.some((t) => /more than priced/.test(t || ""))).toBe(false);
  });
});

// ── (operator finding, 2026-09-05) the snapshot age is READ, not counted ──
//
// The at-rest footer read "snapshot 553169s ago" on the demo fixture. Raw
// seconds stop being readable somewhere around a minute; past that the
// operator is doing arithmetic to answer "is this reading current?", which
// is the one question the line exists to answer at a glance.
//
// Fixed by REUSING `lib/format.ts`'s `relAgoFrom` — the coarse past-only
// formatter the notes dialog, the machine drawer's last-sample line and the
// fleet meta line already render their ages with — rather than minting
// another one. (An earlier version of this note claimed the RUN LIST used it
// too; it does not. The run list has its own `lenses/runs/format.ts::runsAgo`
// and the record panel a third, `components/RecordView.tsx::relTime`. Three
// age formatters, one per surface — a count `lib/format.test.ts` now pins,
// so a fourth cannot arrive quietly.) The local `relSecondsAgo` helper this
// region carried is gone; a page with two relative-time formatters
// eventually shows two different answers for the same instant.
describe("MachineHealthRegion — the snapshot age reads in human units", () => {
  /** The demo fixture's own age at the time of the finding: 553169 seconds,
   * which is 6 days. The number is the point — a bucket no seconds-only
   * formatter renders readably. */
  const SIX_DAYS_MS = 553_169_000;

  it("renders a coarse age, never a raw second count, in the at-rest footer", () => {
    const { container } = renderRegion(BASE, { nowMs: BASE.generated_at_ms + SIX_DAYS_MS });
    const foot = [...container.querySelectorAll(".memfoot")].find((f) => f.textContent?.startsWith("snapshot"))!;
    expect(foot).toBeTruthy();
    expect(foot.textContent).toBe("snapshot 6d ago");
    expect(foot.textContent).not.toContain("553169");
  });

  it("the stale banner's age uses the same formatter — one page, one answer for one instant", () => {
    const { container } = renderRegion(BASE, {
      resourcesErrored: true,
      nowMs: BASE.generated_at_ms + SIX_DAYS_MS,
    });
    expect(container.querySelector(".mm-stalebanner")?.textContent).toContain("snapshot 6d ago");
  });

  // The inverted case: a genuinely fresh reading must still say so in
  // seconds. A formatter that answered "6d ago" for everything would pass
  // the assertions above and be useless.
  it("a seconds-old snapshot still reads in seconds", () => {
    const { container } = renderRegion(BASE, { nowMs: BASE.generated_at_ms + 42_000 });
    const foot = [...container.querySelectorAll(".memfoot")].find((f) => f.textContent?.startsWith("snapshot"))!;
    expect(foot.textContent).toBe("snapshot 42s ago");
  });

  // ── two clocks, not one ──
  //
  // `nowMs` is the READING browser's clock; `generated_at_ms` is the daemon
  // HOST's. This lens is read off-box over the tailnet by design (#1286
  // constraint 2: "the display renders off-machine"), so those are two
  // machines' clocks and nothing keeps them in step. A reader whose clock
  // sits behind the host makes the delta negative, and `relAgoFrom` renders
  // a negative delta as the EMPTY STRING — which is how the footer came out
  // as a bare "snapshot" and the banner as "… — snapshot": a line that
  // answers nothing, at the exact moment the reader asked "is this current?".
  //
  // Clamped at the two call sites rather than inside `relAgoFrom` (see that
  // function's callers), so an unknowable-but-small skew reads as the
  // freshest thing it could honestly be.
  const SKEW_MS = 30_000;

  it("a client clock 30s BEHIND the host still reads an age, not a bare 'snapshot'", () => {
    const { container } = renderRegion(BASE, { nowMs: BASE.generated_at_ms - SKEW_MS });
    const foot = [...container.querySelectorAll(".memfoot")].find((f) => f.textContent?.startsWith("snapshot"))!;
    expect(foot.textContent).toBe("snapshot just now");
  });

  it("the stale banner survives the same skew", () => {
    const { container } = renderRegion(BASE, {
      resourcesErrored: true,
      nowMs: BASE.generated_at_ms - SKEW_MS,
    });
    expect(container.querySelector(".mm-stalebanner")?.textContent).toMatch(/\u2014 snapshot just now$/);
  });
});
