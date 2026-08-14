import { describe, it, expect } from "vitest";
import {
  advanceResidency,
  computeGaugeGeometry,
  deriveLamps,
  digitCells,
  gaugeFaceCaption,
  gaugeScaleWord,
  gaugeTickLabel,
  gaugeValueParts,
  groupResidencyRows,
  isOverLimit,
  modelKvLine,
  odometerTiles,
  redlineLit,
  residencyChangedThisPoll,
  resolveGaugeScale,
  sortResidencyRows,
} from "./machineGauge";
import type { MachineResources, MachineResourcesModel } from "../../types/handwritten";

function resources(overrides: Partial<MachineResources> = {}): MachineResources {
  return {
    schema_version: "1.0",
    generated_at_ms: 1,
    gather_ms: 42,
    limit_bytes: 137438953472, // 128 GiB, decimal ~137.44 GB
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
    identifier: "darkmux:a",
    model_key: "a",
    owner: "darkmux",
    loaded_ctx: 65536,
    weights_bytes: 17180000000,
    kv_per_token_bytes: 20480,
    kv_bytes_at_ctx: 1342177280,
    potential_bytes: 19272177280,
    current_bytes: 18000000000,
    state: "green",
    ...overrides,
  };
}

describe("gaugeValueParts — the glance-layer figure (one decimal)", () => {
  it("formats GB at one decimal", () => {
    expect(gaugeValueParts(32378306560)).toEqual({ num: "32.4", unit: "GB" });
  });
  it("formats MB/KB/B without a decimal", () => {
    expect(gaugeValueParts(728000000)).toEqual({ num: "728", unit: "MB" });
    expect(gaugeValueParts(5000)).toEqual({ num: "5", unit: "KB" });
    expect(gaugeValueParts(5)).toEqual({ num: "5", unit: "B" });
  });
  it("renders — for null/undefined/non-finite, never a fabricated 0", () => {
    expect(gaugeValueParts(null)).toEqual({ num: "—", unit: "" });
    expect(gaugeValueParts(undefined)).toEqual({ num: "—", unit: "" });
    expect(gaugeValueParts(NaN)).toEqual({ num: "—", unit: "" });
  });
});

describe("gaugeTickLabel — bare on-arc numbers", () => {
  it("rounds to whole GB with no unit", () => {
    expect(gaugeTickLabel(137438953472)).toBe("137");
    expect(gaugeTickLabel(137438953472 * 0.75)).toBe("103");
  });
  it("floors to 0 for non-positive/non-finite input, never negative", () => {
    expect(gaugeTickLabel(0)).toBe("0");
    expect(gaugeTickLabel(-5)).toBe("0");
    expect(gaugeTickLabel(NaN)).toBe("0");
  });
});

describe("gaugeScaleWord — the max tick's own meaning", () => {
  it("names LIMIT for the physical-pool source", () => {
    expect(gaugeScaleWord("physical_pool")).toBe("LIMIT");
  });
  it("names BUDGET only when the source says so — the inverted case", () => {
    expect(gaugeScaleWord("budget")).toBe("BUDGET");
    expect(gaugeScaleWord(null)).toBe("LIMIT");
    expect(gaugeScaleWord(undefined)).toBe("LIMIT");
  });
});

describe("resolveGaugeScale — the scale is the allowance, never auto-expanded to fit an overrun", () => {
  it("uses the limit when one is readable, even if potential or current is larger", () => {
    expect(resolveGaugeScale(10_000_000_000, 10_000_000_000, 15_000_000_000, 8_000_000_000)).toBe(10_000_000_000);
  });
  it("falls back to the physical pool only when no limit is readable", () => {
    expect(resolveGaugeScale(null, 20_000_000_000, 1, 1)).toBe(20_000_000_000);
  });
  it("falls back to Σ current/potential only when NEITHER limit nor pool is readable", () => {
    expect(resolveGaugeScale(null, null, 5_000_000_000, 8_000_000_000)).toBe(8_000_000_000);
  });
  it("never returns 0 — the degenerate all-null case still yields a positive divisor", () => {
    expect(resolveGaugeScale(null, null, null, null)).toBe(1);
  });
});

describe("computeGaugeGeometry", () => {
  it("scales to the limit and positions the needle at cur/scale, clamped 0-180deg", () => {
    const g = computeGaugeGeometry(
      resources({ machine: { potential_bytes: 24565385183, unpriced_models: 0, current_bytes: 32378306560, state: "unknown" } }),
    );
    expect(g.scale).toBe(137438953472);
    expect(g.pct).toBeCloseTo(23.56, 1);
    expect(g.needleAngleDeg).toBeCloseTo(42.4, 1);
  });

  it("clamps the needle at 100% (180deg) when current meets or exceeds the scale — never past it", () => {
    const g = computeGaugeGeometry(resources({ machine: { potential_bytes: 1, unpriced_models: 0, current_bytes: 999999999999, state: "red" } }));
    expect(g.pct).toBe(100);
    expect(g.needleAngleDeg).toBe(180);
  });

  it("draws the commit tick at Σ priced potential's position", () => {
    const g = computeGaugeGeometry(resources());
    expect(g.commitPct).toBeCloseTo(17.877, 2);
    expect(g.commitAngleDeg).toBeCloseTo(32.18, 1);
    expect(g.overcommitted).toBe(false);
  });

  it("the inverted case: commit tick is null when Σ potential is 0 — no models, nothing to draw", () => {
    const g = computeGaugeGeometry(resources({ machine: { potential_bytes: 0, unpriced_models: 0, current_bytes: 0, state: "unknown" } }));
    expect(g.commitPct).toBeNull();
    expect(g.commitAngleDeg).toBeNull();
  });

  it("clamps an overcommitted tick to the line and flags it — Σ potential > scale (machine-Amber's own condition)", () => {
    const g = computeGaugeGeometry(
      resources({ limit_bytes: 10000000000, pool: { capacity_bytes: 10000000000, available_bytes: 1 }, machine: { potential_bytes: 15000000000, unpriced_models: 0, current_bytes: 8000000000, state: "amber" } }),
    );
    expect(g.overcommitted).toBe(true);
    expect(g.commitPct).toBe(100); // clamped to the line, not 150
  });

  it("produces five quarter ticks with whole-GB labels", () => {
    const g = computeGaugeGeometry(resources());
    expect(g.ticks.map((t) => t.label)).toEqual(["0", "34", "69", "103", "137"]);
    expect(g.ticks.map((t) => t.pct)).toEqual([0, 25, 50, 75, 100]);
  });

  it("carries the scale word from limit_source", () => {
    expect(computeGaugeGeometry(resources({ limit_source: "budget" })).scaleWord).toBe("BUDGET");
    expect(computeGaugeGeometry(resources({ limit_source: "physical_pool" })).scaleWord).toBe("LIMIT");
  });
});

describe("redlineLit / isOverLimit / gaugeFaceCaption — the redline's provenance", () => {
  it("lights on exactly machine.state === 'red', nothing else", () => {
    expect(redlineLit("red")).toBe(true);
    expect(redlineLit("amber")).toBe(false);
    expect(redlineLit("green")).toBe(false);
    expect(redlineLit("unknown")).toBe(false);
    expect(redlineLit(null)).toBe(false);
  });

  it("isOverLimit is the server's own arm-2 comparison — true only when cur >= limit and both are readable", () => {
    expect(isOverLimit(140, 130)).toBe(true);
    expect(isOverLimit(130, 130)).toBe(true); // meets, not just exceeds
    expect(isOverLimit(100, 130)).toBe(false);
    expect(isOverLimit(null, 130)).toBe(false);
    expect(isOverLimit(100, null)).toBe(false);
  });

  it("the face caption stays 'IN USE' for every non-red state — the live mockups' own shape", () => {
    expect(gaugeFaceCaption("green", false, false)).toBe("IN USE");
    expect(gaugeFaceCaption("amber", false, false)).toBe("IN USE");
    expect(gaugeFaceCaption("unknown", false, false)).toBe("IN USE");
  });

  it("flips to the alarm word only when red, pressure checked first (the server's own cascade order)", () => {
    expect(gaugeFaceCaption("red", true, false)).toBe("RED · PRESSURE");
    expect(gaugeFaceCaption("red", true, true)).toBe("RED · PRESSURE"); // both true: pressure wins, matches cascade arm order
    expect(gaugeFaceCaption("red", false, true)).toBe("RED · OVER LIMIT");
    expect(gaugeFaceCaption("red", false, false)).toBe("RED"); // defensive: never invent a reason
  });
});

describe("deriveLamps — every lamp keys on exactly one field", () => {
  const base = { state: "green", pressureRed: false, overLimit: false, unprivedCount: 0, warningsCount: 0, resourcesErrored: false, residencyChanged: false };

  it("all seven lamps render, unlit, on a clean green payload", () => {
    const lamps = deriveLamps(base);
    expect(lamps.map((l) => l.key)).toEqual(["state", "residency", "unpriced", "pressure", "overLimit", "stale", "warn"]);
    expect(lamps.every((l) => !l.lit)).toBe(true);
  });

  it("the STATE lamp lights for any non-green state — the inverted case is green staying unlit", () => {
    expect(deriveLamps({ ...base, state: "unknown" }).find((l) => l.key === "state")!.lit).toBe(true);
    expect(deriveLamps({ ...base, state: "amber" }).find((l) => l.key === "state")!.lit).toBe(true);
    expect(deriveLamps({ ...base, state: "green" }).find((l) => l.key === "state")!.lit).toBe(false);
  });

  it("UNPRICED lights only when the count is > 0 and names the count", () => {
    const off = deriveLamps({ ...base, unprivedCount: 0 }).find((l) => l.key === "unpriced")!;
    const on = deriveLamps({ ...base, unprivedCount: 2 }).find((l) => l.key === "unpriced")!;
    expect(off.lit).toBe(false);
    expect(on.lit).toBe(true);
    expect(on.word).toBe("⚠ UNPRICED ×2");
  });

  it("PRESSURE keys only on pressure.red, not on the state word", () => {
    expect(deriveLamps({ ...base, state: "red", pressureRed: false }).find((l) => l.key === "pressure")!.lit).toBe(false);
    expect(deriveLamps({ ...base, state: "green", pressureRed: true }).find((l) => l.key === "pressure")!.lit).toBe(true);
  });

  it("OVER LIMIT keys only on the overLimit flag", () => {
    expect(deriveLamps({ ...base, overLimit: true }).find((l) => l.key === "overLimit")!.lit).toBe(true);
  });

  it("STALE keys only on resourcesErrored", () => {
    expect(deriveLamps({ ...base, resourcesErrored: true }).find((l) => l.key === "stale")!.lit).toBe(true);
  });

  it("WARN keys only on warningsCount, names the count", () => {
    const on = deriveLamps({ ...base, warningsCount: 3 }).find((l) => l.key === "warn")!;
    expect(on.lit).toBe(true);
    expect(on.word).toBe("⚠ WARN ×3");
  });

  it("Δ RESIDENCY keys only on residencyChanged", () => {
    expect(deriveLamps({ ...base, residencyChanged: true }).find((l) => l.key === "residency")!.lit).toBe(true);
  });
});

describe("odometerTiles", () => {
  it("splits memory free / swap / compressor into digit cells with detail-layer (two-decimal) precision", () => {
    const tiles = odometerTiles({ swap_used_bytes: 5453843005, compressor_bytes: 727711744, memory_free_percent: 87, red: false });
    expect(tiles[0]).toEqual({ digits: ["8", "7"], unit: "% free", label: "memory free", note: "sole pressure trigger" });
    expect(tiles[1].digits.join("")).toBe("5.45");
    expect(tiles[1].unit).toBe("GB");
    expect(tiles[2].digits.join("")).toBe("728");
    expect(tiles[2].unit).toBe("MB");
  });

  it("renders a single — cell rather than a fabricated 0% when the percent is missing", () => {
    const tiles = odometerTiles({ swap_used_bytes: 0, compressor_bytes: 0, memory_free_percent: null as unknown as number, red: false });
    expect(tiles[0].digits).toEqual(["—"]);
  });
});

describe("digitCells", () => {
  it("splits any string into one-character cells", () => {
    expect(digitCells("5.46")).toEqual(["5", ".", "4", "6"]);
    expect(digitCells("—")).toEqual(["—"]);
  });
});

describe("advanceResidency — the scaling rule's residency state machine", () => {
  const a = model({ identifier: "darkmux:a", model_key: "a", owner: "darkmux" });
  const b = model({ identifier: "user/b", model_key: "b", owner: "user" });

  it("a first-ever poll marks everything live, nothing new — a cold boot is not an arrival", () => {
    const { state, rows } = advanceResidency(null, [a, b], 1000);
    expect(rows.every((r) => r.status === "live")).toBe(true);
    expect(rows.map((r) => r.identifier).sort()).toEqual(["darkmux:a", "user/b"]);
    expect(state.known.size).toBe(2);
    expect(state.shownGhosts.size).toBe(0);
  });

  it("a model present in both polls stays live — the steady-state case", () => {
    const first = advanceResidency(null, [a], 1000);
    const second = advanceResidency(first.state, [a], 2000);
    expect(second.rows).toEqual([{ identifier: "darkmux:a", owner: "darkmux", model: a, status: "live", lastSeenMs: 2000 }]);
  });

  it("a model that arrives after the first poll is NEW", () => {
    const first = advanceResidency(null, [a], 1000);
    const second = advanceResidency(first.state, [a, b], 2000);
    const bRow = second.rows.find((r) => r.identifier === "user/b")!;
    expect(bRow.status).toBe("new");
    expect(bRow.firstSeenMs).toBe(2000);
    const aRow = second.rows.find((r) => r.identifier === "darkmux:a")!;
    expect(aRow.status).toBe("live");
  });

  it("a model that departs becomes a ghost for exactly one more poll, then retires", () => {
    const p1 = advanceResidency(null, [a, b], 1000);
    const p2 = advanceResidency(p1.state, [a], 2000); // b departs
    const bGhost = p2.rows.find((r) => r.identifier === "user/b")!;
    expect(bGhost.status).toBe("ghost");
    expect(bGhost.lastSeenMs).toBe(1000); // when it was actually last seen, not this poll

    const p3 = advanceResidency(p2.state, [a], 3000); // b still absent — the ghost's one extra cycle is over
    expect(p3.rows.find((r) => r.identifier === "user/b")).toBeUndefined();
    expect(p3.state.known.has("user/b")).toBe(false);
  });

  it("a model that reappears while shown as a ghost renders NEW, not silently reconciled back to live", () => {
    const p1 = advanceResidency(null, [a, b], 1000);
    const p2 = advanceResidency(p1.state, [a], 2000); // b departs (ghost, cycle 1)
    const p3 = advanceResidency(p2.state, [a, b], 3000); // b reappears before retiring
    const bRow = p3.rows.find((r) => r.identifier === "user/b")!;
    expect(bRow.status).toBe("new");
  });

  it("residencyChangedThisPoll is false for an all-live poll, true the moment any row isn't", () => {
    const p1 = advanceResidency(null, [a], 1000);
    const p2 = advanceResidency(p1.state, [a], 2000);
    expect(residencyChangedThisPoll(p2.rows)).toBe(false);
    const p3 = advanceResidency(p2.state, [a, b], 3000);
    expect(residencyChangedThisPoll(p3.rows)).toBe(true);
  });

  it("a ghost's model snapshot is its LAST OBSERVED figures, not zeroed out", () => {
    const p1 = advanceResidency(null, [b], 1000);
    const p2 = advanceResidency(p1.state, [], 2000);
    const ghost = p2.rows[0];
    expect(ghost.model.current_bytes).toBe(b.current_bytes);
  });
});

describe("sortResidencyRows / groupResidencyRows — stable, never keyed on a live figure", () => {
  function row(identifier: string, owner: string, current: number): ReturnType<typeof advanceResidency>["rows"][number] {
    return { identifier, owner, model: model({ identifier, owner, current_bytes: current }), status: "live", lastSeenMs: 0 };
  }

  it("darkmux rows sort before user rows regardless of arrival order", () => {
    const rows = [row("user/z", "user", 1), row("darkmux:a", "darkmux", 999)];
    expect(sortResidencyRows(rows).map((r) => r.identifier)).toEqual(["darkmux:a", "user/z"]);
  });

  it("within a group, sorts alphabetically by identifier — NOT by current_bytes (the sort-stability rule)", () => {
    const rows = [row("darkmux:zeta", "darkmux", 999999), row("darkmux:alpha", "darkmux", 1)];
    expect(sortResidencyRows(rows).map((r) => r.identifier)).toEqual(["darkmux:alpha", "darkmux:zeta"]);
  });

  it("groups omit a header entirely when empty — the common single-group case adds no chrome", () => {
    const groups = groupResidencyRows([row("darkmux:a", "darkmux", 1)]);
    expect(groups.map((g) => g.key)).toEqual(["darkmux"]);
  });

  it("both groups render, in darkmux-first order, when both are populated", () => {
    const groups = groupResidencyRows([row("user/b", "user", 1), row("darkmux:a", "darkmux", 1)]);
    expect(groups.map((g) => g.key)).toEqual(["darkmux", "user"]);
  });
});

describe("modelKvLine — unchanged from the retired modelLines()'s fourth element", () => {
  it("formats the priced case", () => {
    expect(modelKvLine(model({ loaded_ctx: 262144, weights_bytes: 18446676063, kv_bytes_at_ctx: 5368709120, potential_bytes: 24565385183, current_bytes: 19351355392 }))).toBe(
      "ctx 262144 · weights 18.45 GB · kv@ctx 5.37 GB · potential 24.57 GB · current 19.35 GB",
    );
  });

  it("names the unpriced reason instead of a dash — the inverted case", () => {
    const line = modelKvLine(model({ kv_bytes_at_ctx: null as unknown as number, potential_bytes: null as unknown as number }));
    expect(line).toContain("kv unknown (no arch facts)");
    expect(line).toContain("potential —");
  });
});
