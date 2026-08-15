import { describe, it, expect } from "vitest";
import {
  advanceResidency,
  computeGaugeGeometry,
  computeRingGeometry,
  deriveLamps,
  digitCells,
  gaugeFaceCaption,
  gaugeFillSeverity,
  gaugeScaleWord,
  gaugeTickLabel,
  gaugeValueParts,
  groupResidencyRows,
  isEstimatedRow,
  isOverLimit,
  machineStateWord,
  rowStateDiffers,
  isUtilityTierRow,
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
    pool: { capacity_bytes: 137438953472, used_bytes: 69300000000, available_bytes: 72000000000, free_bytes: 3738599424 },
    pressure: { swap_used_bytes: 5453843005, compressor_bytes: 890290176, margin_percent: 88, red: false },
    models: [],
    machine: { potential_bytes: 24565385183, unpriced_models: 0, estimated_models: 0, current_bytes: 19506757632, state: "green" },
    attribution: "per_process",
    messages: [],
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
  it("formats GiB at one decimal", () => {
    // Binary since #1811. The same byte count used to read "32.4 GB", on an
    // arc whose max tick read 137 — see `gaugeTickLabel` below.
    expect(gaugeValueParts(32378306560)).toEqual({ num: "30.2", unit: "GiB" });
  });
  it("formats MiB/KiB/B without a decimal", () => {
    expect(gaugeValueParts(728000000)).toEqual({ num: "694", unit: "MiB" });
    expect(gaugeValueParts(5000)).toEqual({ num: "5", unit: "KiB" });
    expect(gaugeValueParts(5)).toEqual({ num: "5", unit: "B" });
  });
  it("renders — for null/undefined/non-finite, never a fabricated 0", () => {
    expect(gaugeValueParts(null)).toEqual({ num: "—", unit: "" });
    expect(gaugeValueParts(undefined)).toEqual({ num: "—", unit: "" });
    expect(gaugeValueParts(NaN)).toEqual({ num: "—", unit: "" });
  });
});

describe("gaugeTickLabel — bare on-arc numbers", () => {
  it("rounds to whole GiB with no unit — the power of two the machine is sold as", () => {
    // #1811's headline case: `hw.memsize` on a 128 GB MacBook Pro. The decimal
    // convention labeled this arc's end 137, a number matching nothing the
    // operator can look up about their own hardware.
    expect(gaugeTickLabel(137438953472)).toBe("128");
    expect(gaugeTickLabel(137438953472 * 0.75)).toBe("96");
  });
  it("floors to 0 for non-positive/non-finite input, never negative", () => {
    expect(gaugeTickLabel(0)).toBe("0");
    expect(gaugeTickLabel(-5)).toBe("0");
    expect(gaugeTickLabel(NaN)).toBe("0");
  });
});

/**
 * The fill ramp. Two things are being pinned, and the second is the one that
 * actually matters:
 *
 * 1. The thresholds are the operator's (green to half, amber past half, red
 *    approaching the line), and they are inclusive-lower at each boundary —
 *    an off-by-one here shifts a whole zone.
 * 2. It is a function of the NEEDLE POSITION and nothing else. The bug this
 *    replaced keyed the fill to `machine.state`, which on a machine with any
 *    unpriceable resident is permanently `unknown`, so a gauge on a real
 *    desk swept from empty to full without ever changing color. Nothing in
 *    this function can see a state string, which is what makes that
 *    unrepeatable — asserted at the component level too, where the payload
 *    carrying `state:"unknown"` is actually available to get it wrong.
 */
describe("gaugeFillSeverity — how full, NOT what the arbiter decided", () => {
  it("ramps green → amber → red across the operator's own thresholds", () => {
    expect(gaugeFillSeverity(0)).toBe("green");
    expect(gaugeFillSeverity(49.9)).toBe("green");
    expect(gaugeFillSeverity(50)).toBe("amber"); // inclusive at the boundary
    expect(gaugeFillSeverity(84.9)).toBe("amber");
    expect(gaugeFillSeverity(85)).toBe("red"); // inclusive at the boundary
    expect(gaugeFillSeverity(100)).toBe("red");
  });

  it("is total — a non-finite percentage degrades to green, never to a missing class", () => {
    expect(gaugeFillSeverity(Number.NaN)).toBe("green");
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
      resources({ machine: { potential_bytes: 24565385183, unpriced_models: 0, estimated_models: 0, current_bytes: 32378306560, state: "unknown" } }),
    );
    expect(g.scale).toBe(137438953472);
    expect(g.pct).toBeCloseTo(23.56, 1);
    expect(g.needleAngleDeg).toBeCloseTo(42.4, 1);
  });

  it("clamps the needle at 100% (180deg) when current meets or exceeds the scale — never past it", () => {
    const g = computeGaugeGeometry(resources({ machine: { potential_bytes: 1, unpriced_models: 0, estimated_models: 0, current_bytes: 999999999999, state: "red" } }));
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
    const g = computeGaugeGeometry(resources({ machine: { potential_bytes: 0, unpriced_models: 0, estimated_models: 0, current_bytes: 0, state: "unknown" } }));
    expect(g.commitPct).toBeNull();
    expect(g.commitAngleDeg).toBeNull();
  });

  it("clamps an overcommitted tick to the line and flags it — Σ potential > scale (machine-Amber's own condition)", () => {
    const g = computeGaugeGeometry(
      resources({
        limit_bytes: 10000000000,
        pool: { capacity_bytes: 10000000000, used_bytes: 8000000000, available_bytes: 1, free_bytes: 1 },
        machine: { potential_bytes: 15000000000, unpriced_models: 0, estimated_models: 0, current_bytes: 8000000000, state: "amber" },
      }),
    );
    expect(g.overcommitted).toBe(true);
    expect(g.commitPct).toBe(100); // clamped to the line, not 150
  });

  it("produces five quarter ticks with whole-GiB labels", () => {
    const g = computeGaugeGeometry(resources());
    expect(g.ticks.map((t) => t.label)).toEqual(["0", "32", "64", "96", "128"]);
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

  it("is SILENT for every non-red state — the slot exists to name a red reason, not to restate the instrument", () => {
    // It used to read `IN USE` here, carried from the level-3 mockups. That
    // restated the one thing a needle over a 0→LIMIT scale cannot fail to
    // communicate, in the most valuable pixels on the page.
    expect(gaugeFaceCaption("green", false, false)).toBeNull();
    expect(gaugeFaceCaption("amber", false, false)).toBeNull();
    expect(gaugeFaceCaption("unknown", false, false)).toBeNull();
    // ...including the case where the CAUSE flags are set but the server has
    // not actually declared red: the caption follows the verdict, never the
    // client's own reading of the disjuncts.
    expect(gaugeFaceCaption("amber", true, true)).toBeNull();
  });

  it("flips to the alarm word only when red, pressure checked first (the server's own cascade order)", () => {
    expect(gaugeFaceCaption("red", true, false)).toBe("RED · PRESSURE");
    expect(gaugeFaceCaption("red", true, true)).toBe("RED · PRESSURE"); // both true: pressure wins, matches cascade arm order
    expect(gaugeFaceCaption("red", false, true)).toBe("RED · OVER LIMIT");
    expect(gaugeFaceCaption("red", false, false)).toBe("RED"); // defensive: never invent a reason
  });
});

describe("deriveLamps — every lamp keys on exactly one field", () => {
  const base = { state: "green", pressureRed: false, overLimit: false, unprivedCount: 0, alarmMessagesCount: 0, resourcesErrored: false, residencyChanged: false };

  it("all six lamps render, unlit, on a clean green payload", () => {
    const lamps = deriveLamps(base);
    expect(lamps.map((l) => l.key)).toEqual(["residency", "unpriced", "pressure", "overLimit", "stale", "warn"]);
    expect(lamps.every((l) => !l.lit)).toBe(true);
  });

  /**
   * There is no STATE lamp, and its absence is the assertion. One rendered
   * here until the operator caught what it did: it relabelled ITSELF with the
   * state (`STATE GREEN`) *and* changed its lit-ness, so a healthy machine
   * showed the word "GREEN" in grey, beside the same word in actual green on
   * the machine chip. A tell-tale never renames itself — its lit-ness IS the
   * message — and the verdict already has a home that carries its cause and
   * its estimated-count qualifier too.
   */
  it("has NO state lamp — a verdict is not a condition, and it is already rendered as a chip", () => {
    for (const st of ["green", "amber", "red", "unknown", null]) {
      const lamps = deriveLamps({ ...base, state: st });
      expect(lamps.some((l) => l.key === ("state" as unknown as typeof l.key))).toBe(false);
      // ...and no lamp anywhere restates the bare verdict word.
      expect(lamps.some((l) => /^STATE\b/.test(l.word))).toBe(false);
    }
  });

  it("every remaining lamp keys on a CONDITION, so a state change alone lights nothing", () => {
    // The inverted case for the deletion: an amber/unknown machine with no
    // actual condition present leaves the whole row dark.
    expect(deriveLamps({ ...base, state: "amber" }).every((l) => !l.lit)).toBe(true);
    expect(deriveLamps({ ...base, state: "unknown" }).every((l) => !l.lit)).toBe(true);
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

  it("WARN keys only on alarmMessagesCount (warn + error severities), names the count", () => {
    const on = deriveLamps({ ...base, alarmMessagesCount: 3 }).find((l) => l.key === "warn")!;
    expect(on.lit).toBe(true);
    expect(on.word).toBe("⚠ WARN ×3");
  });

  it("Δ RESIDENCY keys only on residencyChanged", () => {
    expect(deriveLamps({ ...base, residencyChanged: true }).find((l) => l.key === "residency")!.lit).toBe(true);
  });
});

describe("odometerTiles", () => {
  it("splits margin / swap / compressor into digit cells with detail-layer (two-decimal) precision", () => {
    const tiles = odometerTiles({ swap_used_bytes: 5453843005, compressor_bytes: 727711744, margin_percent: 87, red: false });
    expect(tiles[0].digits).toEqual(["8", "7"]);
    expect(tiles[0].unit).toBe("% margin");
    expect(tiles[0].label).toBe("margin");
    expect(tiles[1].digits.join("")).toBe("5.08");
    expect(tiles[1].unit).toBe("GiB");
    expect(tiles[2].digits.join("")).toBe("694");
    expect(tiles[2].unit).toBe("MiB");
  });

  /**
   * The notes moved behind an `(i)` toggle, so they can afford to actually
   * SAY something — the permanent 8.5px line they replaced could not. These
   * assert the two facts a reader most needs and most easily gets wrong.
   */
  it("the margin note says it is the sole red trigger AND that it is not a byte count", () => {
    const tiles = odometerTiles({ swap_used_bytes: 1, compressor_bytes: 1, margin_percent: 87, red: false });
    expect(tiles[0].note).toMatch(/only figure that can trigger RED/i);
    expect(tiles[0].note).toMatch(/not a byte count/i);
    // The inverted case: the two byte-count tiles must NOT claim to be triggers.
    expect(tiles[1].note).not.toMatch(/trigger/i);
    expect(tiles[2].note).not.toMatch(/trigger/i);
  });

  it("the compressor note disambiguates macOS's compressor from darkmux's compactor", () => {
    // The operator hit this exactly: "Is compressor the 'utility' model?"
    // One letter apart, three rows apart on the page. The label stays (the
    // CLI and JSON use it); the note is where they get told apart.
    const tiles = odometerTiles({ swap_used_bytes: 1, compressor_bytes: 1, margin_percent: 50, red: false });
    expect(tiles[2].note).toMatch(/macOS/);
    expect(tiles[2].note).toMatch(/compactor/);
  });

  it("renders a single — cell rather than a fabricated 0% when the percent is missing", () => {
    const tiles = odometerTiles({ swap_used_bytes: 0, compressor_bytes: 0, margin_percent: null as unknown as number, red: false });
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
      "ctx 262144 · weights 17.18 GiB · kv@ctx 5.00 GiB · potential 22.88 GiB · current 18.02 GiB",
    );
  });

  it("names the unpriced reason instead of a dash — the inverted case", () => {
    const line = modelKvLine(model({ kv_bytes_at_ctx: null as unknown as number, potential_bytes: null as unknown as number }));
    expect(line).toContain("kv unknown (no arch facts)");
    expect(line).toContain("potential —");
  });

  // #1819: an estimated row's potential is a labeled guess, not a
  // measurement — the figure itself carries `~` and `(estimated)`, distinct
  // from BOTH the priced case above (a bare number) and the unpriced case
  // (no number at all).
  it("marks an ESTIMATED potential with `~` and `(estimated)` — distinct from both the priced and unpriced cases", () => {
    const line = modelKvLine(
      model({
        kv_per_token_bytes: null as unknown as number,
        kv_bytes_at_ctx: null as unknown as number,
        potential_bytes: 11480858097,
        potential_source: "estimated",
      }),
    );
    expect(line).toContain("kv unknown (no arch facts)");
    expect(line).toContain("potential ~10.69 GiB (estimated)");
    expect(line).not.toContain("potential 10.69 GiB ·"); // never the bare, unqualified form
  });
});

describe("isEstimatedRow — the #1819 provenance predicate", () => {
  it("true only when potential_source is exactly 'estimated'", () => {
    expect(isEstimatedRow(model({ potential_source: "estimated" }))).toBe(true);
    expect(isEstimatedRow(model({ potential_source: "arch" }))).toBe(false);
    expect(isEstimatedRow(model({ potential_source: undefined }))).toBe(false);
  });
});

describe("isUtilityTierRow — the row-chip identity marker (follow-up to the utility-block redesign)", () => {
  const ID = "darkmux:qwen3-4b-instruct-2507";
  const KEY = "qwen3-4b-instruct-2507";

  it("matches when the row's identifier equals the resident utility model's id", () => {
    expect(isUtilityTierRow(ID, KEY, ID)).toBe(true);
  });

  /**
   * The server decides `utility_model.loaded` with
   * `m.identifier == id || m.model == id` (`machine_specs_handler`) — the
   * profiles registry may store the utility model id EITHER namespaced or
   * bare. A client that matched only `identifier` would leave a registry
   * holding the bare key with a block honestly reading `resident` and no
   * ledger row carrying the chip: the stitch failing silently in a config
   * the server explicitly supports. This is the mirror of that rule.
   */
  it("also matches on the BARE model_key, mirroring the server's own two-field residency test", () => {
    expect(isUtilityTierRow(ID, KEY, KEY)).toBe(true);
  });

  it("the inverted case: does NOT match a different resident row on either field", () => {
    expect(isUtilityTierRow("darkmux:qwen3.6-35b-a3b", "qwen3.6-35b-a3b", ID)).toBe(false);
  });

  it("never matches when there is no resident utility model (null — not configured/not reported/not loaded, all collapse to null upstream)", () => {
    expect(isUtilityTierRow(ID, KEY, null)).toBe(false);
  });

  it("never fabricates a match when the row itself has no identifier or key", () => {
    expect(isUtilityTierRow(null, null, ID)).toBe(false);
    expect(isUtilityTierRow(undefined, undefined, ID)).toBe(false);
    // ...and a null on ONE side must not match a null resident id either —
    // the null-resident guard runs first, so this can never be `true`.
    expect(isUtilityTierRow(null, null, null)).toBe(false);
  });
});

/**
 * `UNKNOWN` was the one word on this page a first-time reader could not
 * resolve from anything else on it — and, per provenance finding 1, the one
 * they see permanently, because the ledger declines to promise a fit
 * whenever any resident is unpriceable. Naming which of the cascade's two
 * unknown arms fired turns jargon into a sentence, using the same fields the
 * server branched on.
 */
describe("machineStateWord — UNKNOWN carries its reason", () => {
  it("names the unpriced-resident arm — the permanent-normal case on a real machine", () => {
    expect(machineStateWord("unknown", 137438953472, 1)).toBe("UNKNOWN · unpriced resident");
  });

  it("names the no-limit arm, and checks it FIRST — the server's own arm order", () => {
    // With no limit, none of the server's `Some(limit)` arms can fire, so a
    // missing limit dominates even when unpriced residents also exist.
    expect(machineStateWord("unknown", null, 3)).toBe("UNKNOWN · no limit readable");
  });

  it("leaves green/amber/red bare — a reason on a self-evident word is noise", () => {
    expect(machineStateWord("green", 100, 0)).toBe("GREEN");
    expect(machineStateWord("amber", 100, 0)).toBe("AMBER");
    expect(machineStateWord("red", 100, 2)).toBe("RED");
  });

  it("never invents a reason it cannot name", () => {
    // Unknown for neither named cause: a limit exists and nothing is
    // unpriced. Degrades to the bare word rather than guessing.
    expect(machineStateWord("unknown", 100, 0)).toBe("UNKNOWN");
    expect(machineStateWord(null, 100, 0)).toBe("UNKNOWN");
  });

  // #1819 decision 1: a DECIDED verdict (green/amber/red) may rest partly on
  // an estimate — the count travels with the word.
  it("appends the estimate disclosure to a decided verdict", () => {
    expect(machineStateWord("green", 100, 0, 1)).toBe("GREEN · 1 estimated");
    expect(machineStateWord("amber", 100, 0, 2)).toBe("AMBER · 2 estimated");
    expect(machineStateWord("red", 100, 0, 1)).toBe("RED · 1 estimated");
  });

  it("does NOT prepend a 'fit ' word — that prefix is a separate, not-yet-built decision", () => {
    expect(machineStateWord("green", 100, 0, 1)).not.toContain("fit");
    expect(machineStateWord("green", 100, 0, 1).toLowerCase()).not.toContain("fit ");
  });

  it("omits the disclosure entirely when nothing was estimated (the default, and every pre-#1819 call site)", () => {
    expect(machineStateWord("green", 100, 0, 0)).toBe("GREEN");
    expect(machineStateWord("green", 100, 0)).toBe("GREEN"); // no 4th arg at all
  });
});

/**
 * The per-row state chip renders only where it disagrees with the machine.
 *
 * Deleting it outright was tempting and would have been WRONG: unified
 * memory means the machine state dominates, but `compute_ledger`'s per-model
 * tint has two real divergence branches — a model whose `current >=
 * potential` shows GREEN under a machine-AMBER (its commitment is already
 * paid), and an unpriceable model stays UNKNOWN whatever the machine says.
 * Those rows are the only place that fact exists.
 */
describe("rowStateDiffers — the per-row chip's whole condition", () => {
  it("is false when the row agrees with the machine — the quiet common case", () => {
    expect(rowStateDiffers("unknown", "unknown")).toBe(false);
    expect(rowStateDiffers("green", "green")).toBe(false);
  });

  it("is true for a materialized model under machine-amber — the divergence that made deletion wrong", () => {
    expect(rowStateDiffers("green", "amber")).toBe(true);
  });

  it("is true for an unpriceable row under a decided machine", () => {
    expect(rowStateDiffers("unknown", "red")).toBe(true);
  });

  it("normalizes both sides before comparing, so a hostile string never reads as a difference", () => {
    // Both degrade to "unknown" through memStateCls — not two distinct values.
    expect(rowStateDiffers("bogus", null)).toBe(false);
    expect(rowStateDiffers(undefined, "not-a-state")).toBe(false);
  });
});

/**
 * The two concentric rings (#1821). The dial used to fill from darkmux's
 * `current` alone against a scale ending at the MACHINE's whole RAM — so it
 * read as "how full is this machine" while measuring a fraction of it. The
 * operator misread it for hours and was right to.
 */
describe("computeRingGeometry — the machine outside, darkmux inside", () => {
  const G = 1073741824;
  const res = (over: Record<string, unknown>): MachineResources =>
    resources({
      limit_bytes: 128 * G,
      pool: { capacity_bytes: 128 * G, used_bytes: 64 * G, available_bytes: 40 * G, free_bytes: 20 * G },
      machine: { potential_bytes: 48 * G, unpriced_models: 0, current_bytes: 32 * G, state: "green" },
      ...over,
    } as Partial<MachineResources>);

  it("puts the machine on the outer ring and darkmux on the inner, on one scale", () => {
    const r = computeRingGeometry(res({}));
    expect(r.outer.solidPct).toBeCloseTo(50, 5); // 64 of 128
    expect(r.inner.solidPct).toBeCloseTo(25, 5); // 32 of 128
  });

  it("draws darkmux's growth as an extension of the OUTER ring only, never twice", () => {
    const r = computeRingGeometry(res({}));
    // committed 48 - current 32 = 16 GiB of growth = 12.5% of the scale.
    expect(r.outer.hatchedPct).toBeCloseTo(12.5, 5);
    // The inner ring carries no hatched band: drawing the same quantity on
    // both rings is duplication, so it appears once, where "will it fit" is
    // answered.
    expect(r.inner.hatchedPct).toBe(0);
  });

  /**
   * The needle reads NOW, and must agree with the centre readout, which shows
   * the same figure. An earlier cut pointed it at the projected total while
   * the readout still showed darkmux's share — a needle at ~82% beside a
   * readout of 36.8 GiB, two subjects on one instrument. The projection is a
   * REGION (the hatched band), never a pointer.
   */
  it("lands the needle at what the machine uses NOW, not at the projection", () => {
    const r = computeRingGeometry(res({}));
    expect(r.needleAngleDeg).toBeCloseTo(50 * 1.8, 4); // used 64 of 128
    // ...and strictly short of the ring's hatched end.
    expect(r.needleAngleDeg).toBeLessThan((r.outer.solidPct + r.outer.hatchedPct) * 1.8);
  });

  it("reports everything-else as the gap, which is never a band of its own", () => {
    const r = computeRingGeometry(res({}));
    expect(r.otherPct).toBeCloseTo(25, 5); // used 64 - darkmux 32
    // The three quantities compose the outer ring exactly.
    expect(r.inner.solidPct + r.otherPct).toBeCloseTo(r.outer.solidPct, 5);
  });

  /**
   * The degenerate case that DEFINED the outer ring. Small residents, huge
   * commitment, quiet machine: if the outer ring were "used" alone, darkmux's
   * inner ring could exceed it and the dial would look broken. Defining outer
   * as the PROJECTED total keeps outer >= inner in every case.
   */
  it("never lets the inner ring overshoot the outer, however large the commitment", () => {
    const r = computeRingGeometry(
      res({
        pool: { capacity_bytes: 128 * G, used_bytes: 20 * G, available_bytes: 100 * G, free_bytes: 100 * G },
        machine: { potential_bytes: 100 * G, unpriced_models: 0, current_bytes: 5 * G, state: "green" },
      }),
    );
    const outerEnd = r.outer.solidPct + r.outer.hatchedPct;
    expect(outerEnd).toBeGreaterThanOrEqual(r.inner.solidPct);
    expect(outerEnd).toBeCloseTo(20 / 128 * 100 + 95 / 128 * 100, 4);
  });

  it("clamps at the scale rather than sweeping past it when over-committed", () => {
    const r = computeRingGeometry(
      res({ machine: { potential_bytes: 400 * G, unpriced_models: 0, current_bytes: 100 * G, state: "amber" } }),
    );
    expect(r.outer.solidPct + r.outer.hatchedPct).toBeLessThanOrEqual(100);
    expect(r.needleAngleDeg).toBeLessThanOrEqual(180);
  });

  it("never reports negative growth for a fully materialised model", () => {
    const r = computeRingGeometry(
      res({ machine: { potential_bytes: 10 * G, unpriced_models: 0, current_bytes: 30 * G, state: "green" } }),
    );
    expect(r.outer.hatchedPct).toBe(0);
  });

  it("never lets the machine read as less full than darkmux's own share", () => {
    // A degraded/absent pool reading must not produce an outer ring shorter
    // than the inner one — darkmux's memory IS part of the machine's.
    const r = computeRingGeometry(res({ pool: { capacity_bytes: 128 * G, used_bytes: 1 * G, available_bytes: 1, free_bytes: 1 } }));
    expect(r.outer.solidPct).toBeGreaterThanOrEqual(r.inner.solidPct);
    expect(r.otherPct).toBe(0);
  });
});
