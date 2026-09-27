import { createContext, useContext, useMemo, type ReactNode } from "react";
import { judgementAt, NO_PRESENCE, type Judgement, type Presence } from "../lib/lifecycle";
import { useNowMs } from "../lib/clock";
import { getSource } from "../lib/source";
import { injectedPlaybackDate } from "../lib/injectedMeta";
import { useLiveSessionIds } from "./useLiveSessionIds";

/** The page's inputs to judging a run: its playhead (`null` at the live
 *  edge) and the sessions presence reports live. Provided once by `App`,
 *  from the same transport and presence read the run page and the fleet
 *  lens are handed. */
export interface PageJudgementInputs {
  readonly playhead: number | null;
  readonly live: Presence;
}

export const PageJudgementContext = createContext<PageJudgementInputs>({ playhead: null, live: NO_PRESENCE });

/** What this page judges its runs at (`judgementAt`): the playhead when one
 *  is parked; else now, with the live sessions. Now is re-read once a second
 *  only while `ticking` (something shown can change with time alone), and
 *  otherwise whenever `refresh` changes (what is shown changed). `null` when
 *  the page has no "now" of its own and no playhead, a daemon-less build or
 *  an injected `/play/<date>` page: its recording is judged as of its newest
 *  record, as the run page judges it (`SessionReplay`'s `frozenAtRecords`). */
export function useJudgement(ticking: boolean, refresh: unknown): Judgement | null {
  const { playhead, live } = useContext(PageJudgementContext);
  const frozen = getSource().kind !== "daemon" || injectedPlaybackDate() != null;
  const clockNow = useNowMs(ticking && !frozen && playhead === null);
  return useMemo(
    () => (playhead === null && frozen ? null : judgementAt(playhead, ticking ? clockNow : Date.now(), live)),
    [playhead, frozen, ticking, clockNow, live, refresh],
  );
}

/** Provides the page's judgement inputs: `playhead` from the transport and
 *  the presence read, polled only when `live` (a replay must not ask about
 *  now). Its own component, so a presence poll re-renders only the surfaces
 *  that judge runs, never the page around them. */
export function PageJudgementProvider({ playhead, live, children }: { playhead: number | null; live: boolean; children: ReactNode }) {
  const sessions = useLiveSessionIds(live).sessions;
  const value = useMemo(() => ({ playhead, live: live ? sessions : NO_PRESENCE }), [playhead, live, sessions]);
  return <PageJudgementContext.Provider value={value}>{children}</PageJudgementContext.Provider>;
}
