/**
 * The order of the fleet cards, decided in ONE place.
 *
 * A fleet reads the same from whichever machine serves it, so no card's place
 * depends on who is looking: this machine is not first. A card can come from
 * the daemon's `/fleet/view` (roster order) or from flow and presence alone
 * (window order). Which source has answered changes while the page loads and
 * refetches, so ordering by either source's own order moved the cards. The
 * final list is ordered here, by a key that reads the same for a machine
 * whichever source it came from.
 */

import { canonUid } from "../../lib/machineIdentity";

/** What decides a card's position. */
export interface CardOrder {
  /** The machine's hardware uid when anything reported one (a view row's
   *  `machine_uid`, or the flow uid); `null` while it is still unknown. */
  uid: string | null;
  /** A stable name to settle machines whose uid is unknown: the roster id. */
  fallback: string;
}

/**
 * Cards are ordered by the machine's hardware uid, not its display name: a
 * name is read off whichever source answered (the roster id
 * `m1-max-32gb-studio`, a presence name `Mac-Studio`, a record's
 * `MacBook-Pro`) and changes as flow and presence load, which reordered cards
 * after they were painted; a uid is the same string from the view, from flow
 * and from presence, and the same on every serving machine. Lower-cased,
 * because uids are upper-case in flow and a roster id is not. A card whose uid
 * is not known yet sorts after every known one, by its roster id, so it holds
 * still until the view reads its card.
 */
export function compareCardOrder(a: CardOrder, b: CardOrder): number {
  if ((a.uid === null) !== (b.uid === null)) return a.uid === null ? 1 : -1;
  const ak = a.uid !== null ? canonUid(a.uid) : a.fallback.toLowerCase();
  const bk = b.uid !== null ? canonUid(b.uid) : b.fallback.toLowerCase();
  if (ak === bk) return 0;
  return ak < bk ? -1 : 1;
}

/** `items` in card order. Does not mutate its input. */
export function orderCards<T>(items: readonly T[], orderOf: (item: T) => CardOrder): T[] {
  return [...items].sort((a, b) => compareCardOrder(orderOf(a), orderOf(b)));
}
