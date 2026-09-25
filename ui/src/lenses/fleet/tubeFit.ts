/**
 * (#2890) Sizes each fleet card's scope tube from the room the card has.
 *
 * Two layouts, matching `styles.css`:
 *
 * - **Desktop (over 560px):** the tube is a centered block between the
 *   card's header and its status lines, so it is bounded by the card's
 *   WIDTH only and the card grows downward to fit it (operator: with three
 *   across "there's plenty of room down to make these taller"). 55% of the
 *   inner width, clamped [`STACKED_MIN`, `STACKED_MAX`].
 * - **Phone:** the tube sits to the right of the text as a reserved square:
 *   at most `TUBE_WIDTH_SHARE` of the inner width and never into the
 *   `TEXT_RESERVE` floor the text column keeps, clamped [`TUBE_MIN`,
 *   `TUBE_MAX`]. The card grows to the square.
 *
 * Written as the card's `--tube`, which the CSS uses for the tube's size,
 * the text gutter and (phone) the card's minimum height.
 */

export const TUBE_MIN = 88;
export const TUBE_MAX = 150;
export const TUBE_WIDTH_SHARE = 0.45;
/** The text column's floor beside a phone tube: the widest lines ("dispatch
 *  in flight", the hardware line) measure up to about 180px. */
export const TEXT_RESERVE = 184;
export const STACKED_SHARE = 0.55;
export const STACKED_MIN = 120;
export const STACKED_MAX = 180;

function px(v: string): number {
  const n = parseFloat(v);
  return Number.isFinite(n) ? n : 0;
}

/** The tube beside the text (phone): its width share, never into the text
 *  floor. Pure arithmetic, exported for tests. */
export function tubeSize(innerWidth: number): number {
  const byWidth = Math.min(innerWidth * TUBE_WIDTH_SHARE, innerWidth - TEXT_RESERVE - 12);
  return Math.floor(Math.max(TUBE_MIN, Math.min(TUBE_MAX, byWidth)));
}

/** The tube stacked between header and status (desktop). */
export function stackedTubeSize(innerWidth: number): number {
  return Math.floor(Math.max(STACKED_MIN, Math.min(STACKED_MAX, innerWidth * STACKED_SHARE)));
}

/** Fits every scope-bearing card under `root`. Safe to call on every render
 *  and resize: it reads layout, then writes one custom property per card.
 *  Without layout (a test DOM) every width reads 0 and the CSS default is
 *  kept. */
export function fitTubes(root: HTMLElement | null): void {
  if (!root) return;
  const cards = [...root.querySelectorAll<HTMLElement>(".mach")].filter((c) => c.querySelector(".mach-scope"));
  if (!cards.length) return;
  const phone = typeof window !== "undefined" && typeof window.matchMedia === "function" && window.matchMedia("(max-width: 560px)").matches;
  for (const card of cards) {
    const cs = getComputedStyle(card);
    const innerW = card.clientWidth - px(cs.paddingLeft) - px(cs.paddingRight);
    if (innerW <= 0) continue;
    card.style.setProperty("--tube", `${phone ? tubeSize(innerW) : stackedTubeSize(innerW)}px`);
  }
}
