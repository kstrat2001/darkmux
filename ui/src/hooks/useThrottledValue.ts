import { useEffect, useRef, useState } from "react";

/** (#2068) A value that changes at most once per `holdMs`. The first change
 * lands immediately (leading edge); changes inside the hold window are
 * coalesced and the LATEST one lands when the window closes (trailing edge),
 * so nothing is ever lost, only intermediate states nobody could have read.
 *
 * `same` decides whether two values count as the same thing — pass a key
 * comparison when the value is an object that is re-created per render but
 * describes the same record, or the hold would restart on every render.
 *
 * Built for the event inspector while it FOLLOWS a stream: at playback speed
 * the followed record changed several times a second and the card swapped its
 * whole body each time — the "flickers around" the operator saw on a phone.
 * The list still streams every record; only the card holds still long enough
 * to be read. */
export function useThrottledValue<T>(value: T, holdMs: number, same: (a: T, b: T) => boolean = Object.is): T {
  const [shown, setShown] = useState(value);
  const shownRef = useRef(value);
  const lastAt = useRef(0);
  const pending = useRef<T>(value);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    pending.current = value;
    if (same(shownRef.current, value)) {
      if (timer.current) {
        clearTimeout(timer.current);
        timer.current = null;
      }
      return;
    }
    const now = Date.now();
    const elapsed = now - lastAt.current;
    const commit = () => {
      timer.current = null;
      lastAt.current = Date.now();
      shownRef.current = pending.current;
      setShown(pending.current);
    };
    if (elapsed >= holdMs) {
      if (timer.current) clearTimeout(timer.current);
      commit();
      return;
    }
    if (!timer.current) timer.current = setTimeout(commit, holdMs - elapsed);
  }, [value, holdMs, same]);

  useEffect(() => () => { if (timer.current) clearTimeout(timer.current); }, []);

  return shown;
}
