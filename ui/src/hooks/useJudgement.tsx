import { createContext, useContext, useMemo, type ReactNode } from "react";
import { judgementAt, NO_PRESENCE, type Judgement, type Presence } from "../lib/lifecycle";
import { useNowMs } from "../lib/clock";
import { getSource } from "../lib/source";
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
 *  is parked; else now, re-read once a second, with the live sessions. `null`
 *  on a daemon-less build at rest, which has no "now" of its own: its
 *  recording is judged as of its newest record. */
export function useJudgement(): Judgement | null {
  const { playhead, live } = useContext(PageJudgementContext);
  const daemon = getSource().kind === "daemon";
  const now = useNowMs(daemon && playhead === null);
  return useMemo(() => (playhead === null && !daemon ? null : judgementAt(playhead, now, live)), [playhead, daemon, now, live]);
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
