import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys } from "../../lib/queryKeys";
import type { SelfIdentity } from "../../lib/machineIdentity";
import type { MachineSpecsResponse } from "../../types/generated/MachineSpecsResponse";

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
 * Who this machine is for ORDERING the cards, and whether their order is final.
 *
 * `orderSelf` is known before the fleet view answers. The view gathers every
 * peer's card (a slow peer can hold it for seconds), while `/machine/specs` is
 * the daemon's own hardware probe, read by the app shell into this cache slot;
 * a disabled observer here reads it without a second fetch. The view's own
 * self row (`viewSelf`) wins once it lands. Live only: a replay describes a
 * past day.
 *
 * The cards are laid out only once their order is final: when the view has
 * answered, when this machine is already known from the shell's specs (self is
 * first from the first paint), or after `ORDER_WAIT_MS`. Until then the grid
 * keeps the cards' boxes, unpainted (`.fleet[data-order="pending"]`), so
 * nothing moves under the operator's eye and the page does not change size.
 */
export function useCardOrderGate(
  liveMode: boolean,
  viewSelf: SelfIdentity | null,
  viewAnswered: boolean,
): { orderSelf: SelfIdentity | null; orderState: "pending" | "final" } {
  const state = useQuery({
    enabled: false,
    queryKey: queryKeys.machineSpecs(),
    queryFn: () => fetchJson<MachineSpecsResponse>("/machine/specs"),
  });
  const waited = useOrderWait(ORDER_WAIT_MS);
  const shell = liveMode && state.data?.ok ? state.data.data : null;
  return { orderSelf: viewSelf ?? shell, orderState: !viewAnswered && !shell?.machine_uid && !waited ? "pending" : "final" };
}
