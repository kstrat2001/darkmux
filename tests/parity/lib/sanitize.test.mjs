// (#2821 review, item 4) Regression coverage for `sanitizeBatteryHealth`'s
// numeric policy. Run with `node --test lib/sanitize.test.mjs` (or
// `node --test lib/` for every `.test.mjs` in this directory) — Node's
// built-in test runner, zero new dependency, matching this package's own
// "no test harness wired up yet" starting point rather than importing one.
import { test } from "node:test";
import assert from "node:assert/strict";
import { sanitizeText } from "./sanitize.mjs";

function payloadWith(batteryHealth) {
  return JSON.stringify({
    load: {
      battery_health: batteryHealth,
      now: { sampled_at_ms: 1, sampler_cost_ms: 1 },
    },
  });
}

test("known battery_health numeric fields are replaced with the named synthetic value", () => {
  const { text } = sanitizeText(
    payloadWith({
      cycle_count: 913, // a real-looking value that must NOT survive
      design_capacity_mah: 6249,
      condition: "Check Battery",
      condition_word: "Normal",
    }),
  );
  const out = JSON.parse(text).load.battery_health;
  assert.equal(out.cycle_count, 42, "known numeric field -> its named synthetic value");
  assert.equal(out.design_capacity_mah, 6000);
  assert.equal(out.condition, "Check Battery", "SAFE_FIELDS string enum passes through unchanged");
  assert.equal(out.condition_word, "Normal");
});

// The defect this test exists to catch: the pre-fix policy was an
// ALLOWLIST (`Object.prototype.hasOwnProperty.call(BATTERY_HEALTH_SYNTHETIC_NUMERIC, k)`)
// — a numeric key NOT on that list fell through to the generic walker,
// which passes numbers through untouched. A future probe field
// (`max_capacity_pct`, `manufacture_date_ms` — plausible names for real
// fields this module doesn't know about yet) would leak verbatim.
test("an UNKNOWN numeric key inside battery_health is never passed through verbatim", () => {
  const { text, matched } = sanitizeText(
    payloadWith({
      cycle_count: 28,
      max_capacity_pct: 95, // unknown to this policy
      manufacture_date_ms: 1700000000000, // unknown, and a real-looking epoch ms
    }),
  );
  const out = JSON.parse(text).load.battery_health;
  assert.notEqual(out.max_capacity_pct, 95, "an unknown numeric field must not survive verbatim");
  assert.notEqual(out.manufacture_date_ms, 1700000000000, "including one that looks like a real timestamp");
  assert.equal(typeof out.max_capacity_pct, "number", "still a number — shape preserved, value scrubbed");
  assert.equal(typeof out.manufacture_date_ms, "number");
  // Recorded so a human sees it and can add an explicit synthetic mapping
  // — see sanitizeBatteryHealth's own doc.
  assert.ok(matched.unknownFields.includes("battery_health.max_capacity_pct"));
  assert.ok(matched.unknownFields.includes("battery_health.manufacture_date_ms"));
});

test("time_at_soc_hours is replaced by a synthetic ramp of the same length, never the real buckets", () => {
  const real = [0, 14, 1940, 683, 0, 13];
  const { text } = sanitizeText(payloadWith({ time_at_soc_hours: real }));
  const out = JSON.parse(text).load.battery_health.time_at_soc_hours;
  assert.equal(out.length, real.length, "structural length preserved");
  assert.notDeepEqual(out, real, "real per-bucket values must not survive");
});

test("an unknown NUMERIC ARRAY inside battery_health (not time_at_soc_hours) is also scrubbed per element", () => {
  const { text, matched } = sanitizeText(
    payloadWith({ some_future_counter_array: [1, 2, 3, 999999] }),
  );
  const out = JSON.parse(text).load.battery_health.some_future_counter_array;
  assert.equal(out.length, 4, "length preserved");
  assert.notDeepEqual(out, [1, 2, 3, 999999], "real values must not survive");
  assert.ok(matched.unknownFields.includes("battery_health.some_future_counter_array"));
});

test("health_condition (the authoritative BatteryHealthCondition signal) is a SAFE_FIELDS string — empty string survives verbatim", () => {
  const { text } = sanitizeText(
    payloadWith({ health_condition: "", condition: "Check Battery", condition_word: "Normal" }),
  );
  const out = JSON.parse(text).load.battery_health;
  assert.equal(out.health_condition, "", "the empty-string healthy reading must not be mangled into a fake identifier");
});

test("health_condition passes a non-empty verbatim word through too, e.g. Service Recommended", () => {
  const { text } = sanitizeText(payloadWith({ health_condition: "Service Recommended" }));
  assert.equal(JSON.parse(text).load.battery_health.health_condition, "Service Recommended");
});

test("battery_health: null (no battery on this machine) passes through as null, not an object", () => {
  const { text } = sanitizeText(payloadWith(null));
  assert.equal(JSON.parse(text).load.battery_health, null);
});

test("fields OUTSIDE battery_health are unaffected by this block's numeric policy", () => {
  const raw = JSON.stringify({
    load: { battery_health: { cycle_count: 28 }, now: { sampled_at_ms: 1234567890, sampler_cost_ms: 4.2 } },
  });
  const { text } = sanitizeText(raw);
  const now = JSON.parse(text).load.now;
  assert.equal(now.sampled_at_ms, 1234567890, "generic numeric fields elsewhere in the corpus pass through — unchanged scope");
  assert.equal(now.sampler_cost_ms, 4.2);
});
