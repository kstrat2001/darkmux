import { describe, expect, it } from "vitest";
import { batteryAriaLabel, batteryFillWidth, capacityLine, chargeCaption, conditionRow, fmtOperatingHours } from "./battery";
import type { BatteryHealth, BatterySample } from "../types/handwritten";

function sample(over: Partial<BatterySample> = {}): BatterySample {
  return { charge_pct: 78, on_ac: false, charging: false, minutes_to_empty: 130, ...over };
}

function health(over: Partial<BatteryHealth> = {}): BatteryHealth {
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
    time_at_soc_hours: null,
    total_operating_time_hours: 5368,
    ...over,
  };
}

describe("chargeCaption", () => {
  it("is absent for no battery", () => {
    expect(chargeCaption(null)).toBe("");
  });
  it("reads on AC when not charging", () => {
    expect(chargeCaption(sample({ on_ac: true, charging: false }))).toBe("on AC");
  });
  it("reads charging when on AC and charging", () => {
    expect(chargeCaption(sample({ on_ac: true, charging: true }))).toBe("charging");
  });
  it("reads a time-left estimate while discharging", () => {
    expect(chargeCaption(sample({ on_ac: false, minutes_to_empty: 130 }))).toBe("2 h 10 m left");
  });
  it("never fabricates a zero estimate", () => {
    expect(chargeCaption(sample({ on_ac: false, minutes_to_empty: null }))).toBe("on battery");
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
  // (#2821 review, item 3) The value shows ONLY the raw reading, labeled
  // inline — never a second headline percentage that reads like macOS's
  // own single "Maximum Capacity" figure.
  it("the visible value carries only the raw reading, labeled inline", () => {
    expect(capacityLine(health())?.value).toBe("5,701 of 6,249 mAh (raw, 91.2%)");
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
    expect(fmtOperatingHours(5368)).toBe("5,368 h");
  });
  it("is null when unmeasured", () => {
    expect(fmtOperatingHours(null)).toBeNull();
  });
});

describe("batteryAriaLabel", () => {
  it("states percent and on-AC", () => {
    expect(batteryAriaLabel(sample({ charge_pct: 100, on_ac: true, charging: false }))).toBe("battery 100%, on AC");
  });
  it("states percent and charging", () => {
    expect(batteryAriaLabel(sample({ charge_pct: 62, on_ac: true, charging: true }))).toBe("battery 62%, charging");
  });
  it("states percent and a time-left estimate while discharging", () => {
    expect(batteryAriaLabel(sample({ charge_pct: 35, on_ac: false, minutes_to_empty: 130 }))).toBe(
      "battery 35%, 2 h 10 m left",
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
