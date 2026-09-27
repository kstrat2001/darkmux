import { useRef } from "react";

/** (#2958) `true` from the first render `value` is true, for the rest of
 *  this mount: "has this source ever answered". A source's FIRST answer is
 *  what a card waits for; a later pending read (a refetch, or the flow
 *  window's new day key at UTC midnight) must not send it back to "no
 *  signal". Written during render, like the fleet lens's other
 *  render-pass memories: it never needs to schedule a render of its own. */
export function useLatch(value: boolean): boolean {
  const latched = useRef(false);
  if (value) latched.current = true;
  return latched.current;
}
