import { useEffect, useState } from "react";

/** How long the cards wait for the fleet view before laying out in key order
 *  anyway. The view is cached daemon-side and normally answers in tens of
 *  milliseconds, but it probes every peer, and a slow one held a read for
 *  seconds; a second is the point where a wait reads as the page being slow,
 *  so past it the cards show and any later move animates (`useFlip`). */
const ORDER_WAIT_MS = 1000;

/** `true` once `ms` have passed since mount. */
function useOrderWait(ms: number): boolean {
  const [waited, setWaited] = useState(false);
  useEffect(() => {
    const t = setTimeout(() => setWaited(true), ms);
    return () => clearTimeout(t);
  }, [ms]);
  return waited;
}

/**
 * Whether the cards' order is final.
 *
 * The cards are laid out only once their names are settled: when the fleet
 * view has answered, or after `ORDER_WAIT_MS`. Until then the grid keeps the
 * cards' boxes, unpainted (`.fleet[data-order="pending"]`), so nothing moves
 * under the operator's eye and the page does not change size.
 */
export function useCardOrderGate(viewAnswered: boolean): "pending" | "final" {
  const waited = useOrderWait(ORDER_WAIT_MS);
  return viewAnswered || waited ? "final" : "pending";
}
