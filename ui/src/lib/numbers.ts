/** The largest of `xs`, or undefined when there are none. A loop, never a
 *  spread into `Math.max`: a spread passes every element as an argument, and
 *  past roughly 100k of them the engine throws a RangeError. For record
 *  times, `latestByTime`/`earliestByTime` in `ingest.ts` are the rule. */
export function maxOf(xs: Iterable<number>): number | undefined {
  let best: number | undefined;
  for (const x of xs) if (best === undefined || x > best) best = x;
  return best;
}

/** The smallest of `xs`, or undefined when there are none. See `maxOf`. */
export function minOf(xs: Iterable<number>): number | undefined {
  let best: number | undefined;
  for (const x of xs) if (best === undefined || x < best) best = x;
  return best;
}
