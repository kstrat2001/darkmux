import { describe, expect, it } from "vitest";
import { capacityLine, chargeCaption, conditionRow, fmtOperatingHours } from "./battery";
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
  it("shows both ratios, never as macOS's own Maximum Capacity figure", () => {
    expect(capacityLine(health())).toBe("5,701 of 6,249 mAh design (91.2% raw · 93.7% nominal)");
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
