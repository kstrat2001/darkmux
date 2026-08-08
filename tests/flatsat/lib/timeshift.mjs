// Rewrites the parity corpus's frozen "the day it was recorded" timestamps
// to real wall-clock "today" at seed time (docker-compose.yml note b of the
// packet brief) — a whole-day shift so relative ordering/spacing between
// records is preserved exactly, only the absolute date moves. Frozen
// records recorded at 2026-08-08T00:47:35Z stay at HH:47:35 on whatever day
// `bun run up` actually ran.
const DAY_MS = 86_400_000;

function utcMidnightMs(dateStr) {
  // dateStr: "YYYY-MM-DD"
  return Date.parse(`${dateStr}T00:00:00Z`);
}

/** Whole-day delta (in ms) from the corpus's captured_date to real today (UTC). */
export function deltaMsFromCapturedDate(capturedDate, now = new Date()) {
  const todayStr = now.toISOString().slice(0, 10);
  return utcMidnightMs(todayStr) - utcMidnightMs(capturedDate);
}

export function shiftIso(tsIso, deltaMs) {
  const shifted = new Date(Date.parse(tsIso) + deltaMs);
  // Match the corpus's own second-precision "Z" formatting (no
  // milliseconds) — every sampled record in flow-today.json/flow-
  // yesterday.json uses this shape.
  return shifted.toISOString().replace(/\.\d{3}Z$/, "Z");
}

export function shiftUnixSeconds(tsSeconds, deltaMs) {
  if (typeof tsSeconds !== "number") return tsSeconds;
  return tsSeconds + Math.round(deltaMs / 1000);
}

/** UTC "YYYY-MM-DD" bucket for a (possibly shifted) ISO timestamp — for
 * grouping flow records into their own day-file, same convention
 * LocalFileSink uses (`day_utc_now()`/record.ts's own date). */
export function utcDateOf(tsIso) {
  return new Date(tsIso).toISOString().slice(0, 10);
}

/** Recursively shift every `*_ts` integer (Unix seconds) field on a plain
 * object/array — used for Mission/Phase JSON (created_ts/started_ts/
 * finalized_ts/completed_ts/abandoned_ts/paused_ts). Leaves non-`_ts`
 * fields untouched; recurses into nested objects/arrays for forward
 * compatibility with fields this packet doesn't enumerate by name. */
export function shiftTsFields(value, deltaMs) {
  if (Array.isArray(value)) return value.map((v) => shiftTsFields(v, deltaMs));
  if (value && typeof value === "object") {
    const out = {};
    for (const [k, v] of Object.entries(value)) {
      if (k.endsWith("_ts") && typeof v === "number") {
        out[k] = shiftUnixSeconds(v, deltaMs);
      } else {
        out[k] = shiftTsFields(v, deltaMs);
      }
    }
    return out;
  }
  return value;
}
