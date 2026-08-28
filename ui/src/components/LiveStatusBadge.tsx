import type { LiveTailStatus } from "../hooks/useLiveTail";

/**
 * `#modebadge` — viewer.html:807 (`<span class="pb" id="modebadge">▣
 * playback</span>`) + `setBadges()`'s `live`/`.stale` class toggle
 * (3444-3446) + `LIVE_ES.onopen`/`.onerror`'s live/reconnecting text swap
 * (3609-3625, the #1480 part 2 "a drop is visible" fix). Global chrome —
 * every lens shares the same badge, none of them own it (same status as
 * `#meta`/`#crumb`, see `App.tsx`'s own module doc) — so this mounts once at
 * the App root, driven by `useLiveTail`'s returned status.
 *
 * Legacy's badge has a SECOND arm (`▣ playback`) for the non-live case —
 * `PlaybackModeBadge` below, rendered as a sibling rather than folded in
 * here; see its own doc for why they are not one component.
 */
export function LiveStatusBadge({ status }: { status: LiveTailStatus }) {
  const live = status === "live";
  return (
    <span id="modebadge" className={`pb${live ? " live" : " stale"}`} data-state={status}>
      {/* (operator) The DOT pulses, not the whole string — a heartbeat
          rather than the entire badge breathing, which reads as noise at the
          top of every screen. Text is split so the animation has something
          small to attach to; `innerText` is unchanged either way, so the
          parity goldens do not move. */}
      <span className="pbdot">{live ? "●" : "◌"}</span>
      {live ? " live" : " reconnecting"}
    </span>
  );
}

/**
 * `mb.textContent = live ? "● live" : "▣ playback"` — viewer.html:3473, the
 * OTHER arm of the same badge. Rendered instead of `<LiveStatusBadge>` on a
 * replay, which is why they are siblings here rather than one component with
 * a mode flag: they are driven by different things entirely. The live badge
 * tracks a CONNECTION (`useLiveTail`'s `live`/`reconnecting`, the #1480
 * "a drop is visible" fix); this one states a MODE, and a replay has no
 * connection whose health could change.
 *
 * (#1800) It was previously not rendered at all on a replay — a defensible
 * cut while `PlaybackLens` was a placeholder, since a mode badge over a
 * not-ported notice describes nothing. Now that the stage is a real
 * historical render, its absence was the difference between a page that says
 * what it is showing and one that leaves you to infer it —
 * `goldens/playback-date.txt`'s topbar carries `▶ PLAYBACK` (operator, 2026-08-28: "why is the playback icon a square?" — ▣ was legacy's glyph; ▶ says playback).
 *
 * `.pb` with neither `.live` nor `.stale`, matching legacy's own
 * `classList.toggle("live", live)` leaving the class off in this arm.
 */
export function PlaybackModeBadge() {
  return (
    <span id="modebadge" className="pb" data-state="playback">
      <span className="pbdot">▶</span>
      {" playback"}
    </span>
  );
}
