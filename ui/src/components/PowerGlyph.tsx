import type { BatteryIconKind } from "../lib/battery";

/** The power-state glyph drawn INSIDE the battery svg, centered on (`cx`, `cy`): a bolt while
 * charging, a plug while on AC and not charging. Solid single-color paths in the page's own
 * tokens (`.battery-bar-icon`: `--fg` fill, `--bg` outline), so it reads over any point on the
 * red-to-green ramp and is never an emoji or an image. Decorative: the battery's own
 * accessible name states the power state. */
export function PowerGlyph({ kind, cx, cy }: { kind: Exclude<BatteryIconKind, null>; cx: number; cy: number }) {
  const size = 14;
  return (
    <g
      className="battery-bar-icon"
      data-kind={kind}
      transform={`translate(${cx - size / 2} ${cy - size / 2}) scale(${size / 10})`}
      aria-hidden="true"
    >
      <title>{kind === "bolt" ? "charging" : "on AC"}</title>
      {kind === "bolt" ? (
        <path d="M6.2 0 L1.6 5.6 H4.5 L3.7 10 L8.4 4.2 H5.5 Z" />
      ) : (
        <>
          <rect x="2.5" y="0" width="1.3" height="3" rx="0.4" />
          <rect x="6.2" y="0" width="1.3" height="3" rx="0.4" />
          <rect x="1.6" y="2.4" width="6.8" height="3.8" rx="1.3" />
          <rect x="4.3" y="6" width="1.4" height="4" rx="0.4" />
        </>
      )}
    </g>
  );
}
