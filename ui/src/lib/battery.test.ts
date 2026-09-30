import { describe, expect, it } from "vitest";
import {
  batteryAriaLabel,
  batteryFillWidth,
  batteryHeldExplanation,
  batteryHeldTitle,
  batteryIcon,
  batteryRampStops,
  batteryStateText,
  batteryTimeLeftText,
  capacityLine,
  conditionRow,
  fmtOperatingHours,
} from "./battery";
import { gaugeFillColor } from "../components/Meter";
import type { BatteryHealthNow } from "../types/generated/BatteryHealthNow";
import type { BatteryCharge } from "../types/generated/BatteryCharge";

function sample(over: Partial<BatteryCharge> = {}): BatteryCharge {
  return { charge_pct: 78, on_ac: false, state: "discharging", minutes_to_empty: 130, ...over };
}

function health(over: Partial<BatteryHealthNow> = {}): BatteryHealthNow {
  return {
    cycle_count: 28,
    design_capacity_mah: 6249,
    raw_max_capacity_mah: 5701,
    nominal_charge_capacity_mah: 5853,
    raw_capacity_pct: 91.2,
    nominal_capacity_pct: 93.7,
    condition: "Check Battery",
    health_condition: "",
    condition_word: "Normal",
    permanent_failure_status: 0,
    temperature_c: 31.01,
    time_at_soc_ms: null,
    total_operating_ms: 19324800000,
    ...over,
  };
}

// ── (operator, 2026-09-24) "'on AC' is unlike a lot of meters these days
//    ... a lightning bolt icon works inside the battery" — icon replaces
//    the old plain-text caption for the two AC-connected states; the third
//    state (discharging) draws none, since `batteryTimeLeftText` is what
//    that state has to say instead. ─────────────────────────────────────
describe("batteryIcon", () => {
  it("bolt while charging (current actually flowing)", () => {
    expect(batteryIcon(sample({ on_ac: true, state: "charging" }))).toBe("bolt");
  });
  it("plug when on AC but NOT charging — topped off/held, never a false bolt", () => {
    expect(batteryIcon(sample({ on_ac: true, state: "unknown" }))).toBe("plug");
  });
  it("no icon while discharging", () => {
    expect(batteryIcon(sample({ on_ac: false }))).toBeNull();
  });
});

describe("held state (macOS holding the charge level)", () => {
  const held = sample({ charge_pct: 80, on_ac: true, state: "held", minutes_to_empty: null });

  it("names the held level in the tooltip, and never states a limit it did not observe", () => {
    expect(batteryHeldTitle(held)).toBe("Held at 80% by macOS (charge limit): plugged in, not charging");
  });
  it("the (i) explanation states the level, that the hold is deliberate, and that it is not draining", () => {
    expect(batteryHeldExplanation(held)).toBe(
      "Held at 80% by macOS's charge limit. It is plugged in and not charging on purpose, which protects the battery. It is not draining.",
    );
    expect(batteryHeldExplanation(sample({ on_ac: true, state: "full" }))).toBeNull();
  });
  it("is null for every other state, so nothing claims a hold it did not see", () => {
    for (const state of ["charging", "discharging", "full", "unknown"] as const) {
      expect(batteryHeldTitle(sample({ on_ac: true, state }))).toBeNull();
    }
  });
  it("the accessible name says held, and keeps the plug icon", () => {
    expect(batteryAriaLabel(held)).toBe("battery 80%, held at 80% by macOS, on AC, not charging");
    expect(batteryIcon(held)).toBe("plug");
  });
  it("an on-AC reading that is not held keeps the old wording", () => {
    expect(batteryStateText(sample({ on_ac: true, state: "unknown" }))).toBe("on AC, not charging");
  });
});

describe("batteryStateText", () => {
  it('"charging" while current is flowing', () => {
    expect(batteryStateText(sample({ on_ac: true, state: "charging" }))).toBe("charging");
  });
  it('"on AC, not charging" when connected but topped off', () => {
    expect(batteryStateText(sample({ on_ac: true, state: "unknown" }))).toBe("on AC, not charging");
  });
  it('"on battery, H h M m left" while discharging with an estimate', () => {
    expect(batteryStateText(sample({ on_ac: false, minutes_to_empty: 130 }))).toBe("on battery, 2 h 10 m left");
  });
  it('bare "on battery" while discharging with no estimate yet — never a fabricated zero', () => {
    expect(batteryStateText(sample({ on_ac: false, minutes_to_empty: null }))).toBe("on battery");
  });
});

describe("batteryTimeLeftText — the ONLY visible power-state text left (icon carries the rest)", () => {
  it("is null on AC, charging or not — the icon carries that state now", () => {
    expect(batteryTimeLeftText(sample({ on_ac: true, state: "charging" }))).toBeNull();
    expect(batteryTimeLeftText(sample({ on_ac: true, state: "unknown" }))).toBeNull();
  });
  it("reads the time-left estimate while discharging", () => {
    expect(batteryTimeLeftText(sample({ on_ac: false, minutes_to_empty: 130 }))).toBe("2 h 10 m left");
  });
  it("is null while discharging with no estimate yet — never a fabricated 0 min", () => {
    expect(batteryTimeLeftText(sample({ on_ac: false, minutes_to_empty: null }))).toBeNull();
  });
  it("is null for a null sample", () => {
    expect(batteryTimeLeftText(null)).toBeNull();
  });
});

describe("conditionRow", () => {
  it("prefers the computed condition_word over the unreliable raw string", () => {
    // The Step-0 regression case: raw IOKit condition read "Check Battery"
    // while the computed word (from permanent_failure_status) is "Normal".
    expect(conditionRow(health())).toEqual({ value: "Normal", warn: false });
  });
  it("warns on Service Battery", () => {
    expect(conditionRow(health({ condition_word: "Service Battery" }))).toEqual({
      value: "Service Battery",
      warn: true,
    });
  });
  // (#2821 review, MUST-FIX 1's UI-facing counterpart) A verbatim
  // passthrough condition word (not one of the two fixed literals) must
  // still warn — the false all-clear this review found was exactly a case
  // where a non-"Normal" word was being silently read as healthy.
  it("warns on any non-Normal verbatim passthrough, not just the fixed Service Battery literal", () => {
    expect(conditionRow(health({ condition_word: "Service Recommended" }))).toEqual({
      value: "Service Recommended",
      warn: true,
    });
  });
  it("falls back to a precisely-labeled raw string, never colored, when no computed word exists", () => {
    expect(conditionRow(health({ condition_word: null, condition: "Good" }))).toEqual({
      value: "power source reports: Good",
      warn: false,
    });
  });
  it("is null with no battery", () => {
    expect(conditionRow(null)).toBeNull();
  });
});

describe("capacityLine", () => {
  // (#2821 review, item 3 / operator, 2026-09-24) The value shows ONLY the
  // raw reading; the "(raw, X%)" parenthetical is now a SEPARATE field so
  // the caller can wrap it in `white-space: nowrap` — it must move to the
  // next line whole, never split mid-parenthesis at a narrow viewport.
  // (operator, 2026-09-24) "raw", "design" and "when new" all read as
  // jargon. Two rows: MAX CHARGE (what the pack holds now, and that as a
  // percent of the original) and ORIGINAL CAPACITY on its own line.
  it("max charge is what the pack holds now, with its percent of the original", () => {
    expect(capacityLine(health())?.value).toBe("5,701 mAh · 91.2%");
  });
  it("original capacity is its own figure", () => {
    expect(capacityLine(health())?.original).toBe("6,249 mAh");
  });
  it("without a percent, max charge is the mAh alone", () => {
    expect(capacityLine(health({ raw_capacity_pct: null }))?.value).toBe("5,701 mAh");
  });
  it("no jargon in what renders", () => {
    const c = capacityLine(health())!;
    expect(`${c.value} ${c.original}`).not.toMatch(/raw|design|when new|nominal/i);
  });
  it("the nominal reading and the disclaimer move into title", () => {
    const title = capacityLine(health())?.title;
    expect(title).toContain("5,853 mAh nominal");
    expect(title).toContain("93.7%");
    expect(title).toContain('macOS\'s own "Maximum Capacity"');
  });
  it("title still carries the disclaimer even with no nominal figures at all", () => {
    const title = capacityLine(health({ nominal_charge_capacity_mah: null, nominal_capacity_pct: null }))?.title;
    expect(title).toContain('macOS\'s own "Maximum Capacity"');
    expect(title).not.toContain("nominal");
  });
  it("is null without the mAh pair", () => {
    expect(capacityLine(health({ raw_max_capacity_mah: null }))).toBeNull();
  });
});

describe("fmtOperatingHours", () => {
  it("formats with a thousands separator", () => {
    expect(fmtOperatingHours(19_324_800_000)).toBe("5,368 h");
  });
  it("is null when unmeasured", () => {
    expect(fmtOperatingHours(null)).toBeNull();
  });
});

describe("batteryAriaLabel", () => {
  it("states percent, on AC, not charging", () => {
    expect(batteryAriaLabel(sample({ charge_pct: 100, on_ac: true, state: "unknown" }))).toBe(
      "battery 100%, on AC, not charging",
    );
  });
  it("states percent and charging", () => {
    expect(batteryAriaLabel(sample({ charge_pct: 62, on_ac: true, state: "charging" }))).toBe("battery 62%, charging");
  });
  it("states percent, on battery, and a time-left estimate while discharging", () => {
    expect(batteryAriaLabel(sample({ charge_pct: 35, on_ac: false, minutes_to_empty: 130 }))).toBe(
      "battery 35%, on battery, 2 h 10 m left",
    );
  });
  it("is unmeasured for a null sample", () => {
    expect(batteryAriaLabel(null)).toBe("battery unmeasured");
  });
});

describe("batteryFillWidth", () => {
  it("scales linearly to the given max width", () => {
    expect(batteryFillWidth(100, 44)).toBe(44);
    expect(batteryFillWidth(50, 44)).toBe(22);
    expect(batteryFillWidth(0, 44)).toBe(0);
  });
  it("clamps a >100 reading (the post-full-charge overshoot) to the max width, never past it", () => {
    expect(batteryFillWidth(103, 44)).toBe(44);
  });
  it("clamps a negative reading to 0", () => {
    expect(batteryFillWidth(-5, 44)).toBe(0);
  });
  it("is null when unmeasured — the caller draws no fill rect at all", () => {
    expect(batteryFillWidth(null, 44)).toBeNull();
  });
});

// ── (operator, 2026-09-24, reversing an earlier "no gradient" amendment)
//    "give every small meter the same gradient treatment the big memory
//    gauge uses"; for the battery, REVERSED — red at empty, green at full.
describe("batteryRampStops — the REVERSED ramp (red empty -> green full)", () => {
  it("the empty edge (offset 0%) is red; the full edge (offset 100%) is green", () => {
    const stops = batteryRampStops(4);
    expect(stops[0].offset).toBe("0.0000%");
    expect(stops[0].color).toBe(gaugeFillColor(100)); // red
    expect(stops[stops.length - 1].offset).toBe("100.0000%");
    expect(stops[stops.length - 1].color).toBe(gaugeFillColor(0)); // green
  });

  it("red is confined to the low end: fully green from 50% up, amber at a quarter", () => {
    const stops = batteryRampStops(20); // a stop every 5%
    const at = (pct: number) => stops[pct / 5].color;
    expect(at(0)).toBe(gaugeFillColor(100)); // red
    expect(at(25)).toBe(gaugeFillColor(50)); // the palette's amber
    for (const pct of [50, 80, 100]) expect(at(pct), `${pct}%`).toBe(gaugeFillColor(0)); // green
  });

  it("the color at a level moves monotonically toward green up to 50%, never back", () => {
    const stops = batteryRampStops(20);
    const seen = new Set<string>();
    for (const pct of [10, 25, 50]) seen.add(stops[pct / 5].color);
    expect(seen.size).toBe(3);
    expect(stops[10].color).not.toBe(stops[4].color); // 50% differs from 20%
  });

  it("offsets are evenly (linearly) spaced, unlike the arc's cosine-spaced stops — the bar is a straight rectangle", () => {
    const stops = batteryRampStops(4);
    const offsets = stops.map((s) => parseFloat(s.offset));
    expect(offsets).toEqual([0, 25, 50, 75, 100]);
  });
});
