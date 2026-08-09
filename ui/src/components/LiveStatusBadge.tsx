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
 * Legacy's badge also renders `▣ playback` for the non-live (playback/
 * catalog-replay) case — out of scope here: this app has no real playback
 * render pipeline yet (`PlaybackLens`'s own doc), so there is nothing this
 * badge would be describing for that state. Only the two states
 * `useLiveTail` can actually report — `live`/`reconnecting` — are rendered.
 */
export function LiveStatusBadge({ status }: { status: LiveTailStatus }) {
  const live = status === "live";
  return (
    <span id="modebadge" className={`pb${live ? " live" : " stale"}`} data-state={status}>
      {live ? "● live" : "◌ reconnecting"}
    </span>
  );
}
