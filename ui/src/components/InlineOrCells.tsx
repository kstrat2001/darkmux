/**
 * (#2108, operator finding — phone review) A "dotted list that becomes a
 * cell grid on phones" — the shared fix for a class of rows that read fine
 * as one inline `label value · label value · …` run on desktop but wrap
 * MID-ITEM at ~390px ("15.0 W / p95" split across two lines, "ANE" pushed
 * onto its own broken line). The rule this exists to enforce: no phrase
 * may ever break mid-item. A cell either fits, or the grid drops to fewer
 * columns per row — never a mid-item wrap.
 *
 * Two renderings of the SAME facts, chosen by `isMobile`:
 * - **Desktop:** the items' own `inline` fragments joined with " · ",
 *   exactly the pre-existing dotted-list text — byte-identical to what
 *   every caller rendered before this component existed.
 * - **Mobile:** a CSS grid of cells, each showing `cellLabel` (small,
 *   muted, above) and `cellValue` (bold, below), every cell
 *   `white-space: nowrap` + tabular-nums (`styles.css`'s
 *   `.inline-or-cells__cell-value`) so a value can never itself wrap; the
 *   grid's `repeat(auto-fit, minmax(...))` lets whole CELLS reflow to the
 *   next row as a unit when they don't fit, which is what keeps a phrase
 *   from ever breaking in the middle.
 *
 * `inline` and the `cellLabel`/`cellValue` pair are NOT derived from each
 * other — callers give both because the two forms often order label and
 * value differently ("11.5 W now" value-first vs "CPU 11.3 W"
 * label-first vs "worst Nominal" label-first paired with "0s above
 * nominal" value-first-with-suffix). Trying to normalize one wording into
 * the other would either read wrong on desktop or wrong on mobile; this
 * component doesn't guess, it takes both.
 */
export interface InlineOrCellsItem {
  /** The small caption shown ABOVE the value in the mobile grid cell. */
  cellLabel: string;
  /** The bold value shown BELOW the label in the mobile grid cell. */
  cellValue: string;
  /** The desktop dotted-list fragment for this item — e.g. `"11.5 W now"`
   * or `"CPU 11.3 W"`. Items are joined with `" · "` on desktop. */
  inline: string;
}

export interface InlineOrCellsProps {
  items: InlineOrCellsItem[];
  isMobile: boolean;
  /** Applied to the rendered element on EITHER branch (the desktop
   * `<span>` or the mobile grid `<div>`) — a caller styling hook, not a
   * mobile/desktop switch of its own. */
  className?: string;
}

/** The desktop dotted-list text alone — exported so a caller building its
 * OWN desktop-only string (rather than mounting `<InlineOrCells>` in
 * place of an existing `<span>`) can still share the join logic. */
export function inlineText(items: InlineOrCellsItem[]): string {
  return items.map((item) => item.inline).join(" · ");
}

export function InlineOrCells({ items, isMobile, className }: InlineOrCellsProps) {
  if (items.length === 0) return null;

  if (!isMobile) {
    return <span className={className}>{inlineText(items)}</span>;
  }

  return (
    <div
      className={`inline-or-cells${className ? ` ${className}` : ""}`}
      data-act="inline-or-cells"
    >
      {items.map((item) => (
        <div className="inline-or-cells__cell" key={item.cellLabel}>
          <div className="inline-or-cells__cell-label">{item.cellLabel}</div>
          <div className="inline-or-cells__cell-value">{item.cellValue}</div>
        </div>
      ))}
    </div>
  );
}
