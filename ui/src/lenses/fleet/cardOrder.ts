/**
 * The order of the fleet cards, decided in ONE place.
 *
 * A card can come from the daemon's `/fleet/view` (roster order, this machine
 * first) or from flow and presence alone (window order). Which source has
 * answered changes while the page loads and refetches, so ordering by either
 * source's own order moved the cards. The final list is ordered here, by a
 * key that reads the same for a machine whichever source it came from.
 */

import { canonUid } from "../../lib/machineIdentity";

/** What decides a card's position. */
export interface CardOrder {
  /** Whether this is the machine serving the page. */
  self: boolean;
  /** The machine's stable identity: see `cardOrderKey`. */
  key: string;
}

/**
 * The stable identity a card is ordered by: the machine's hardware uid when
 * anything reported one (a view row's `machine_uid`, or the flow uid), else
 * the roster id of a machine only the roster knows. The uid, not the display
 * name: a name is read off whichever source answered (the roster id
 * `m1-max-32gb-studio`, a presence name `Mac-Studio`, a record's `MacBook-Pro`)
 * and changes as sources load, while a uid is the same string from the view,
 * from flow and from presence. Lower-cased, because uids are upper-case in
 * flow and a roster id is not, so no source's casing can reorder a machine.
 */
export function cardOrderKey(machineUid: string | null | undefined, fallbackId: string): string {
  return canonUid(machineUid || fallbackId);
}

/** This machine first, then every other machine by `key`. */
export function compareCardOrder(a: CardOrder, b: CardOrder): number {
  if (a.self !== b.self) return a.self ? -1 : 1;
  if (a.key === b.key) return 0;
  return a.key < b.key ? -1 : 1;
}

/** `items` in card order. Does not mutate its input. */
export function orderCards<T>(items: readonly T[], orderOf: (item: T) => CardOrder): T[] {
  return [...items].sort((a, b) => compareCardOrder(orderOf(a), orderOf(b)));
}
