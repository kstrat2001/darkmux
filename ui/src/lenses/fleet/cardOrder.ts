/**
 * The order of the fleet cards, decided in ONE place.
 *
 * A fleet reads the same from whichever machine serves it, so no card's place
 * depends on who is looking: the order is by the machine's own name, then its
 * uid to settle two machines that share one. This machine is not first.
 *
 * A card can come from the daemon's `/fleet/view` (roster order) or from flow
 * and presence alone (window order). Which source has answered changes while
 * the page loads and refetches, so ordering by either source's own order moved
 * the cards. The final list is ordered here, from the cards' own names.
 */

import { canonUid } from "../../lib/machineIdentity";

/** What decides a card's position. */
export interface CardOrder {
  /** The name the card shows. */
  name: string;
  /** The card's machine uid: the tie-break between machines that share a name. */
  uid: string;
}

const nameKey = (name: string): string => name.toLowerCase();

/** By name, case-insensitively, then by canonical uid. */
export function compareCardOrder(a: CardOrder, b: CardOrder): number {
  const an = nameKey(a.name);
  const bn = nameKey(b.name);
  if (an !== bn) return an < bn ? -1 : 1;
  const au = canonUid(a.uid);
  const bu = canonUid(b.uid);
  if (au === bu) return 0;
  return au < bu ? -1 : 1;
}

/** `items` in card order. Does not mutate its input. */
export function orderCards<T>(items: readonly T[], orderOf: (item: T) => CardOrder): T[] {
  return [...items].sort((a, b) => compareCardOrder(orderOf(a), orderOf(b)));
}
