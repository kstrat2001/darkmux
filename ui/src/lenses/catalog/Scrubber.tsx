import { useId } from "react";
import { clkhm, clkrange } from "../../lib/format";

/**
 * The playback transport — restored from the legacy viewer (#1869;
 * `git show v2.9.0:crates/darkmux-serve/assets/viewer.html`, markup around
 * line 854, CSS around line 379, behavior around line 2842): rewind, play/
 * pause, a scrubber range, a speed cycle, and a clock readout.
 *
 * PLAYBACK-ONLY by construction — this component only ever mounts inside
 * `PlaybackLens` (the `#<date>` route and the static demo build). Legacy
 * gated the equivalent bar with `body.live-mode .scrub{display:none}`
 * because its `.scrub` markup was global chrome, always in the DOM; this
 * port has no such element to hide because it is never rendered for a live
 * route in the first place — `FleetLens`'s own default (no-hash) route
 * never reaches this file. See `PlaybackLens.tsx` for the state this
 * component is purely a controlled view over (`t`/`playing`/`speed` all
 * live there, not here).
 *
 * Accessibility: the range carries a real `<label>` (visually hidden, same
 * `.mm-sr-only` pattern the rest of this port uses) plus `aria-valuetext`
 * — a bare `0..100` means nothing to a screen reader, but "14:32 of
 * 09:00:00–18:20:00" does. Both icon-only buttons keep a `title`/
 * `aria-label` pair that stays correct across the glyph flip (#1067's own
 * rule: the glyph carries visual state, the name carries meaning). Every
 * control is a real `<button>` or the native range input, so keyboard
 * operation (Tab, Space/Enter, arrow keys on the range) works without any
 * extra wiring. No transition is added by this component, so there is
 * nothing here to gate under `prefers-reduced-motion`.
 */
export interface ScrubberProps {
  /** The playhead — `tMin <= t <= tMax`. */
  t: number;
  tMin: number;
  tMax: number;
  playing: boolean;
  speed: number;
  onScrub: (t: number) => void;
  onRewind: () => void;
  onTogglePlay: () => void;
  onCycleSpeed: () => void;
  /** Records at or before the playhead, out of the day's total — the same
   * "N/M rec" readout legacy's own clock chip carries
   * (`visible().length+"/"+DATA.length+" rec"`, viewer.html:2619). */
  visibleCount: number;
  totalCount: number;
}

export function Scrubber({
  t,
  tMin,
  tMax,
  playing,
  speed,
  onScrub,
  onRewind,
  onTogglePlay,
  onCycleSpeed,
  visibleCount,
  totalCount,
}: ScrubberProps) {
  const rangeId = useId();
  const span = Math.max(1, tMax - tMin);
  const raw = ((t - tMin) / span) * 100;
  const value = Number.isFinite(raw) ? Math.min(100, Math.max(0, Math.round(raw))) : 100;

  return (
    <div className="scrub" data-testid="scrubber">
      <button type="button" onClick={onRewind} title="jump to start" aria-label="jump to start">
        ⏮
      </button>
      <button
        type="button"
        className="primary"
        onClick={onTogglePlay}
        title={playing ? "pause" : "play"}
        aria-label={playing ? "pause" : "play"}
      >
        {playing ? "⏸" : "▶"}
      </button>
      <label htmlFor={rangeId} className="mm-sr-only">
        playback position
      </label>
      <input
        id={rangeId}
        type="range"
        min={0}
        max={100}
        value={value}
        onChange={(e) => onScrub(tMin + (span * Number(e.target.value)) / 100)}
        aria-valuetext={`${clkhm(t)} of ${clkrange(tMin, tMax)}`}
      />
      <button type="button" onClick={onCycleSpeed} title="playback speed" aria-label={`playback speed, ${speed}×`}>
        {speed}×
      </button>
      <span className="clock" data-testid="scrubber-clock">
        {clkhm(t)} · {visibleCount}/{totalCount} rec
      </span>
    </div>
  );
}
