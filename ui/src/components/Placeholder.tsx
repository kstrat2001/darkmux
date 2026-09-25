import type { CSSProperties, ElementType } from "react";

/**
 * `Shimmer` — the shared loading placeholder (#2862).
 *
 * Lifted out of the fleet hero, where it shipped first (#2817, `SavingsHero`
 * in `lenses/fleet/FleetLens.tsx`): a shimmer drawn OVER the real layout, at
 * the SAME size the settled value will occupy, so data arriving never moves
 * anything. #2068 measured a layout-shift score of 1.21 from a loading state
 * that mounted/unmounted elements instead — the geometry has to be identical
 * in both states, which is why this renders the caller's own element (`as`)
 * with no text, rather than swapping in a second "skeleton" element with its
 * own (guessed) size.
 *
 * The caller still owns sizing: a block element (a `<div>` value that already
 * fills its flex/grid track, like `.savnum`/`.mv`/`.brief-value`) needs only
 * `minHeight` — width comes from the block box the same way it always did.
 * An inline element (a `<span>` badge or id) collapses to zero width with no
 * text, so it needs an explicit `minWidth` too; passing one switches the
 * element to `inline-block` automatically; see `style` fallthrough below.
 *
 * Decorative by construction (`aria-hidden`) — the PAGE says what's loading
 * (`role="status"` + `aria-label` on the pending container, e.g. "Loading
 * runs"), the same division the hero already used (`aria-busy` on `.savings`,
 * nothing on `.savnum` itself). A second aria-label per shimmered value would
 * be a screen reader announcing "loading" once per tile.
 */
export function Shimmer({
  as: As = "div",
  className = "",
  minWidth,
  minHeight = "1em",
  style,
}: {
  as?: ElementType;
  className?: string;
  minWidth?: string | number;
  minHeight?: string | number;
  style?: CSSProperties;
}) {
  const mergedStyle: CSSProperties = {
    minHeight,
    ...(minWidth != null ? { minWidth, display: "inline-block" } : null),
    ...style,
  };
  return (
    <As
      className={className ? `ph-shimmer ${className}` : "ph-shimmer"}
      style={mergedStyle}
      aria-hidden="true"
    />
  );
}
