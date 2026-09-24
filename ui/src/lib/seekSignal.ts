import { createContext, useContext } from "react";

/**
 * (Playback parity, Change B — one seek signal) The playback transport
 * (`hooks/usePlaybackTransport.ts`) changes its playhead `t` for two
 * different reasons: the play tick ADVANCES it by measured elapsed wall
 * clock, or a scrub/rewind/replay-from-start SEEKS it to an arbitrary new
 * instant. Only the second kind should suppress animation — a count-up
 * tween or an "arriving" row highlight makes sense across an advance (the
 * live edge always advances one tick at a time) and is actively wrong
 * across a seek (a scrub from 03:00 to 09:00 must not tween through six
 * hours of intermediate values, or mark every row in between as "just
 * arrived").
 *
 * This is ONE counter, incremented only by `usePlaybackTransport`'s own
 * `scrub`/`rewind`/replay-from-end calls, provided once near the app root
 * and read by every consumer that needs to distinguish the two
 * (`useCountUp`, `useArrivalKeys`) — rather than each caller threading its
 * own `liveMode ? x : y` gate down to them. Live mode never seeks (there is
 * no transport), so the default context value (`0`, never changing) means
 * every consumer's "was this a seek" check is permanently false there —
 * advance always animates, exactly as before.
 */
export const SeekSignalContext = createContext<number>(0);

/** The current seek generation. Consumers compare this against the value
 * they saw on their OWN previous render (a ref) to answer "did a seek
 * happen since I last rendered" — the context alone only says how many
 * seeks have ever happened, not whether THIS update is one of them. */
export function useSeekGeneration(): number {
  return useContext(SeekSignalContext);
}
