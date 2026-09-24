import { useEffect, useRef, useState } from "react";

/** ~150ms slide + a ~1s highlight fade (`styles.css`'s `eventlog-arrive`/
 * `eventlog-arrive-highlight` keyframes) — held a little past the longer
 * of the two so the class never drops mid-animation. */
const DEFAULT_HOLD_MS = 1100;

/**
 * (#2878) Which of `keys` arrived AFTER this hook's own first render —
 * never the page's initial list, and never a row merely REVEALED by
 * loosening a filter or scrolling.
 *
 * That second distinction is why callers must pass the FULL, unfiltered
 * key set (every record the pane has ever received), not the filtered
 * view it renders: a record already known but hidden by a filter is not
 * "new" when the filter changes, only when the underlying data grows.
 * `EventLogColumn` calls this with `records` (its own prop, before
 * `filtered`/`visibleRecs` narrow it), then checks the returned set once
 * per row it actually renders.
 *
 * `resetKey` re-arms the hook as a fresh mount — the events pane can stay
 * mounted (`paneId`'s own doc: "two simultaneously-mounted panes") while
 * its `records` prop is swapped for an entirely different session's
 * window (switching which run's page is open). Without a reset, that
 * swap would look like every record in the new session "just arrived" at
 * once. Pass the value that identifies the underlying data set
 * (`scopeLabel`, a session id, …); changing it drops the old `seen` set
 * and adopts the new snapshot with nothing marked as arriving.
 */
export function useArrivalKeys(keys: readonly string[], resetKey: string, holdMs = DEFAULT_HOLD_MS): ReadonlySet<string> {
  const seen = useRef<Set<string> | null>(null);
  const lastResetKey = useRef<string | null>(null);
  const [arriving, setArriving] = useState<ReadonlySet<string>>(() => new Set());
  const timers = useRef(new Map<string, ReturnType<typeof setTimeout>>());

  useEffect(() => {
    if (lastResetKey.current !== resetKey) {
      // A different underlying data set (or the very first render): adopt
      // its keys as the baseline, mark nothing as arriving, and drop any
      // pending highlight timers from the scope we just left.
      lastResetKey.current = resetKey;
      seen.current = new Set(keys);
      timers.current.forEach(clearTimeout);
      timers.current.clear();
      if (arriving.size > 0) setArriving(new Set());
      return;
    }
    const prev = seen.current ?? new Set<string>();
    const fresh = keys.filter((k) => !prev.has(k));
    seen.current = new Set(keys);
    if (fresh.length === 0) return;
    setArriving((old) => {
      const next = new Set(old);
      for (const k of fresh) next.add(k);
      return next;
    });
    for (const k of fresh) {
      const existing = timers.current.get(k);
      if (existing) clearTimeout(existing);
      const t = setTimeout(() => {
        timers.current.delete(k);
        setArriving((old) => {
          if (!old.has(k)) return old;
          const next = new Set(old);
          next.delete(k);
          return next;
        });
      }, holdMs);
      timers.current.set(k, t);
    }
    // `arriving` deliberately excluded from deps: it is this effect's own
    // output, and including it would re-run the diff on every highlight
    // expiring rather than only when `keys`/`resetKey` change.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [keys, resetKey, holdMs]);

  useEffect(() => {
    const map = timers.current;
    return () => {
      map.forEach(clearTimeout);
    };
  }, []);

  return arriving;
}
