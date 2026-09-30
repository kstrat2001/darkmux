/*
 * The `plug` and `zap` icon paths below are from Lucide (https://lucide.dev), copied here
 * rather than added as a dependency.
 *
 * ISC License
 *
 * Copyright (c) 2026 Lucide Icons and Contributors
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 */
import type { BatteryIconKind } from "../lib/battery";

/** Lucide's 24x24 line icons: `zap` while charging, `plug` while on AC and not charging. */
const PATHS: Record<Exclude<BatteryIconKind, null>, readonly string[]> = {
  bolt: [
    "M15.914 4a1.5 1.5 0 00-2.474-1.561l-9 9A1.5 1.5 0 005.5 14h4.002a.5.5 0 01.471.666L8.086 20a1.5 1.5 0 002.475 1.56l9-9A1.5 1.5 0 0018.5 10h-3.997a.5.5 0 01-.472-.667z",
  ],
  plug: [
    "M12 22v-5",
    "M15 8V2",
    "M17 8a1 1 0 0 1 1 1v4a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V9a1 1 0 0 1 1-1z",
    "M9 8V2",
  ],
};

/** The power-state glyph drawn INSIDE the battery svg, centered on (`cx`, `cy`). Lucide's line
 * style (stroke 2, round caps and joins, no fill) in the page's tokens: each path is drawn
 * twice, a wider `--bg` stroke underneath as a halo and the `--fg` stroke on top, so it reads
 * over any point on the red-to-green ramp. Decorative: the battery's own accessible name
 * states the power state. */
export function PowerGlyph({ kind, cx, cy }: { kind: Exclude<BatteryIconKind, null>; cx: number; cy: number }) {
  const size = 16;
  return (
    <g
      className="battery-bar-icon"
      data-kind={kind}
      transform={`translate(${cx - size / 2} ${cy - size / 2}) scale(${size / 24})`}
      aria-hidden="true"
    >
      <title>{kind === "bolt" ? "charging" : "on AC"}</title>
      {PATHS[kind].map((d) => (
        <path key={`halo-${d}`} className="battery-bar-icon__halo" d={d} />
      ))}
      {PATHS[kind].map((d) => (
        <path key={d} className="battery-bar-icon__line" d={d} />
      ))}
    </g>
  );
}
