/**
 * (#2890, operator 2026-09-25: "designate a square for the scope to utilize
 * a more full space on right") Sizes each fleet card's scope tube as a
 * reserved square on the right, from the room the card has, instead of one
 * fixed size and instead of letting the text on the left squeeze it.
 *
 * - **height room:** the tallest TEXT block of any card in the same row (a
 *   card is stretched to its row, e.g. by a neighbor's pager). Not the card's
 *   own height: that includes this card's `min-height` (tube + padding), so
 *   a tube sized from it could never shrink once the row got shorter. A card
 *   alone on its row (stacked, as on a phone) has no height bound: it grows
 *   downward to fit the square.
 * - **width share:** at most `TUBE_WIDTH_SHARE` of the card's inner width, so
 *   the text column keeps the rest. Text that does not fit ellipsizes; it
 *   never shrinks the square.
 *
 * Clamped to [`TUBE_MIN`, `TUBE_MAX`] and written as the card's `--tube`,
 * which the CSS uses for the tube, the text gutter and the card's minimum
 * height.
 */

export const TUBE_MIN = 88;
export const TUBE_MAX = 150;
export const TUBE_WIDTH_SHARE = 0.45;
/** The text column's floor: the lines beside the tube ("dispatch in
 *  flight", the hardware line) measure up to about 180px, so the square
 *  takes what is left after it, never the text's room. */
export const TEXT_RESERVE = 184;

function px(v: string): number {
  const n = parseFloat(v);
  return Number.isFinite(n) ? n : 0;
}

/** Height of a card's in-flow text: from the top of its first row to the
 *  bottom of its last, which excludes the absolutely positioned tube and
 *  this card's own `min-height`. */
function textBlockHeight(card: HTMLElement): number {
  const kids = [...card.children] as HTMLElement[];
  if (!kids.length) return 0;
  const top = kids[0].getBoundingClientRect().top;
  const bottom = Math.max(...kids.map((k) => k.getBoundingClientRect().bottom));
  return Math.max(0, bottom - top);
}

/** The tube size for one card: the row's height room (`null` = none, a card
 *  alone on its row), capped by the card's width share and by what the
 *  text column's floor leaves. Pure arithmetic,
 *  exported for tests. */
export function tubeSize(innerWidth: number, rowTextHeight: number | null): number {
  const byWidth = Math.min(innerWidth * TUBE_WIDTH_SHARE, innerWidth - TEXT_RESERVE - 12);
  const bound = rowTextHeight === null ? byWidth : Math.min(byWidth, rowTextHeight);
  return Math.floor(Math.max(TUBE_MIN, Math.min(TUBE_MAX, bound)));
}

/** Fits every scope-bearing card under `root`. Safe to call on every render
 *  and resize: it reads layout, then writes one custom property per card.
 *  Without layout (a test DOM) every size reads 0 and the CSS default is
 *  kept. */
export function fitTubes(root: HTMLElement | null): void {
  if (!root) return;
  const all = [...root.querySelectorAll<HTMLElement>(".mach")];
  const cards = all.filter((c) => c.querySelector(".mach-scope"));
  if (!cards.length) return;
  const rowOf = (c: HTMLElement) => Math.round(c.getBoundingClientRect().top);
  const rowText = new Map<number, number>();
  const rowCount = new Map<number, number>();
  for (const c of all) {
    const r = rowOf(c);
    rowText.set(r, Math.max(rowText.get(r) ?? 0, textBlockHeight(c)));
    rowCount.set(r, (rowCount.get(r) ?? 0) + 1);
  }
  for (const card of cards) {
    const cs = getComputedStyle(card);
    const innerW = card.clientWidth - px(cs.paddingLeft) - px(cs.paddingRight);
    if (innerW <= 0) continue;
    const r = rowOf(card);
    const alone = (rowCount.get(r) ?? 1) === 1;
    card.style.setProperty("--tube", `${tubeSize(innerW, alone ? null : (rowText.get(r) ?? null))}px`);
  }
}
