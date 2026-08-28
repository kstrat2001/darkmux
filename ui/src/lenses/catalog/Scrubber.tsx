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
 * Accessibility: the range is named via `aria-label` (#1869 code review;
 * this port's established pattern for naming an input without a rendered
 * node — see `EventLogColumn.tsx`'s search box) plus `aria-valuetext` — a
 * bare `0..100` means nothing to a screen reader, but "14:32 of
 * 09:00:00–18:20:00" does. A visually-hidden `<label>` (`.mm-sr-only`, this
 * component's first cut) is the WRONG tool for this: `.mm-sr-only` uses
 * `clip`, which keeps the node in the accessibility tree AND in
 * `innerText` — a real element there announces "playback position" TWICE
 * (once as loose row text, once as the range's own computed name) and
 * leaks an extra line into the parity golden that has nothing to do with
 * the transport's actual content. `aria-label` names the control with no
 * rendered node to leak. `.scrub` itself carries `role="group"` +
 * `aria-label` so its four controls announce as a named group instead of
 * ungrouped siblings. Both icon-only buttons keep a `title`/`aria-label`
 * pair that stays correct across the glyph flip (#1067's own rule: the
 * glyph carries visual state, the name carries meaning). Every control is
 * a real `<button>` or the native range input, so keyboard operation (Tab,
 * Space/Enter, arrow keys on the range) works without any extra wiring. No
 * transition is added by this component, so there is nothing here to gate
 * under `prefers-reduced-motion`.
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
  // `span` (floored to 1) feeds `onScrub`'s drag math below, so a drag on a
  // zero-span day never divides by zero. The RENDERED value is a separate
  // question — legacy's own rule is `span > 0 ? Math.round(...) : 100`
  // (viewer.html:2618, `#1640`): a zero-span day pins the thumb at the END,
  // not the start. Flooring `span` to 1 before that branch (a prior version
  // of this line did) silently took the `span > 0` arm with a floored span
  // instead of the real one, computing 0 instead of following legacy's
  // explicit `else 100` — thumb hard left while the clock read "1/1 rec"
  // and the hero showed the whole day. `realSpan` (unfloored) is what the
  // branch itself must test.
  const realSpan = tMax - tMin;
  const span = Math.max(1, realSpan);
  const raw = realSpan > 0 ? ((t - tMin) / realSpan) * 100 : 100;
  const value = Number.isFinite(raw) ? Math.min(100, Math.max(0, Math.round(raw))) : 100;

  return (
    <div className="scrub" data-testid="scrubber" role="group" aria-label="playback transport">
      {/* (operator, on the phone, 2026-08-28) Inline SVG icons in fixed
          square buttons, not text glyphs: `⏮` rendered as a 48x40 emoji on
          iOS while `▶` was a 39x32 mono-font triangle whose ink sat low in
          its box, so neither looked centered and the two never matched. A
          path is the same shape at the same place on every platform. */}
      <button type="button" className="icon" onClick={onRewind} title="jump to start" aria-label="jump to start">
        <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true" focusable="false">
          <path d="M2 2h2v12H2zM14 2v12L5 8z" fill="currentColor" />
        </svg>
      </button>
      <button
        type="button"
        className="primary icon"
        onClick={onTogglePlay}
        title={playing ? "pause" : "play"}
        aria-label={playing ? "pause" : "play"}
      >
        {playing ? (
          <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true" focusable="false">
            <path d="M3 2h4v12H3zM9 2h4v12H9z" fill="currentColor" />
          </svg>
        ) : (
          <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true" focusable="false">
            <path d="M4 2l10 6-10 6z" fill="currentColor" />
          </svg>
        )}
      </button>
      <input
        type="range"
        min={0}
        max={100}
        value={value}
        aria-label="playback position"
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
