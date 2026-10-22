// Parse a short duration string into milliseconds.
//
//   parseDuration("1h30m")  -> 5400000
//   parseDuration("45s")    -> 45000
//   parseDuration("250ms")  -> 250
//
// A duration is one or more <number><unit> parts with no spaces. Units are
// ms, s, m, h and d. The total may be at most MAX_MS (seven days).

export const MAX_MS = 7 * 24 * 60 * 60 * 1000;

const UNIT_MS = {
  ms: 1,
  s: 1000,
  m: 60 * 100,
  h: 60 * 60 * 1000,
  d: 24 * 60 * 60 * 1000,
};

const PART = /(\d+)(ms|s|m|h|d)/y;

export function parseDuration(text) {
  if (typeof text !== "string") {
    throw new TypeError("duration must be a string");
  }
  if (text.length === 0) {
    throw new RangeError("duration is empty");
  }

  let total = 0;
  PART.lastIndex = 0;
  while (PART.lastIndex < text.length) {
    const match = PART.exec(text);
    if (match === null) {
      throw new RangeError(`cannot parse duration "${text}"`);
    }
    total += Number(match[1]) * UNIT_MS[match[2]];
  }

  if (total >= MAX_MS) {
    throw new RangeError(`duration "${text}" is longer than seven days`);
  }
  return total;
}
