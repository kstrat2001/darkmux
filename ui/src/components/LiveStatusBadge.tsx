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
 * the playback badge that used to live below (removed 2026-09-01: it was
 * redundant beside the transport and false on a mission, which has no
 * playback at all — the source chip carries the mode now)
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

