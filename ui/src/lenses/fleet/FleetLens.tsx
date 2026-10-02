import { judgementAt } from "../../lib/lifecycle";
import { useLifecyclePolicy } from "../../hooks/useLifecyclePolicy";
import { encodeMachineKey } from "../../lib/machineKey";
import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import { fitTubes } from "./tubeFit";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS } from "../../lib/queryKeys";
import { useFlowWindow } from "../../hooks/useFlowWindow";
import { useNowMs } from "../../lib/clock";
import { useLiveOverlay } from "../../lib/liveChannel";
import { useCountUp } from "../../hooks/useCountUp";
import { useFleetRoster, useLiveMachines } from "../../hooks/useLiveMachines";
import { useFleetView } from "../../hooks/useFleetView";
import { useFlip } from "../../hooks/useFlip";
import { orderCards } from "./cardOrder";
import { useCardOrderGate } from "./cardOrderGate";
import { getSource, runsSrc, runsReachable } from "../../lib/source";
import { useLiveSessionIds } from "../../hooks/useLiveSessionIds";
import { machPresent, LIVE_WINDOW_MS } from "../../lib/flow";
import { findUid, isSelfMachine, machineMatch, machineUids, uidForName } from "../../lib/machineIdentity";
import type { FleetMachinesLiveResponse } from "../../types/generated/FleetMachinesLiveResponse";
import type { FleetDispatchesLiveResponse } from "../../types/generated/FleetDispatchesLiveResponse";
import type { RunsResponse } from "../../types/generated/RunsResponse";
import { fmtN, fmtC } from "../../lib/format";
import { Shimmer } from "../../components/Placeholder";
import { usePlaybackClock } from "../../lib/pageClockRate";
import { tokensOffMeter } from "./savings";
import { buildFleetCardBase, withLiveReadings, shownExecution, cardFace, notStreamedNames, type CardSourcesAnswered } from "./cards";
import { MachineCard } from "./MachineCard";
import { useLatch } from "../../hooks/useLatch";
import { buildActivityTimeline, ACTIVITY_WINDOW_PRESETS, DEFAULT_ACTIVITY_WINDOW_MIN } from "./timeline";
import { rowFacts, rowSpecs } from "./viewRows";
import { runsForMachine } from "../runs/format";
import { recordsAsOf, type NormRecord } from "../../lib/ingest";

/** `sc()`. One token-class chip (value over label).
 *
 * `loading` (#2862) renders the shared `Shimmer` in place of `value` — the
 * `settled ? fmtC(...) : ""` sentinel this used to take as `value` moved to
 * the call site, which was doing the SAME masking `Shimmer` now owns, just
 * without leaving a box for the CSS overlay to sit in (see `.ph-shimmer`'s
 * own doc in `styles.css`). `.scv` is a `<div>`, matching `.savnum`'s own
 * block-fills-its-track sizing, so only a height floor (`minHeight="1.1em"`,
 * `.savc .scv`'s own line-height) is needed. */
function Chip({ value, label, cls, loading, part }: { value?: string | number; label: string; cls?: string; loading?: boolean; part?: { value: string; label: string } | null }) {
  return (
    <div className={`savc${cls ? ` ${cls}` : ""}`}>
      {loading ? <Shimmer as="div" className="scv" minHeight="1.1em" /> : <div className="scv">{value}</div>}
      {/* (#2902, layout) The part line rides on the label's own line, never
          a line of its own: the hero keeps one height whether a provider
          reported cached tokens or not (see `.savc__lbl` in `styles.css`). */}
      <div className="savc__lbl">
        <div className="scl">{label}</div>
        {part && !loading ? <div className="savpart"><span className="savpartv">{part.value}</span> {part.label}</div> : null}
      </div>
    </div>
  );
}

type OffRow = { calls: number; tokens: number };

/** (#3067) The hover text under the total: what it leaves out (machines not
 *  streaming here) and what it includes that no run's row shows (calls with no
 *  session, radio routing). A tooltip, so the hero keeps its height. */
export function totalHint(noRun: OffRow, unlisted: OffRow, notStreaming: readonly string[]): string | undefined {
  const parts: string[] = [];
  if (notStreaming.length > 0) parts.push(`Counts only machines whose records reach this viewer. Not streaming here: ${notStreaming.join(", ")}.`);
  if (noRun.calls > 0) parts.push(`Includes ${fmtC(noRun.tokens)} tokens with no run (radio routing and probes).`);
  if (unlisted.calls > 0) parts.push(`Includes ${fmtC(unlisted.tokens)} tokens on runs not listed here.`);
  return parts.length > 0 ? parts.join(" ") : undefined;
}

/**
 * `savingsHero()` (#783, #1186). Always renders,
 * even at zero — a fresh fleet with no dispatches yet shows "0", not a
 * hidden card (showing "0" that then climbs reads as a live odometer;
 * hiding it made it pop in late on the legacy mobile client — see that
 * source's own comment).
 *
 * TOKENS ONLY — no rates, no currency, no savings formula. (#2902) Facts
 * only, too: every figure is a plain sum of provider-reported counts from
 * the window's usage records (`tokensOffMeter`). One lead figure (ALL
 * TOKENS) with UTILITY as a part line under it, and three chips (INPUT with
 * CACHED as its part line, GENERATED, DISPATCHES). Nothing here classifies
 * where a token ran or what it cost, and no chip is invented to make the
 * parts add up — see `savings.ts`'s module doc for what was withdrawn and
 * why.
 */
const SavingsHero = memo(function SavingsHero({
  tokens: t,
  liveMode,
  settled,
  notStreaming,
}: {
  tokens: ReturnType<typeof tokensOffMeter>;
  liveMode: boolean;
  /** (5.0 R3) The names of machines whose records never reach this viewer.
   *  The total cannot include their tokens, so it says it counts what is
   *  seen rather than "all". */
  notStreaming: readonly string[];
  /** (#2817) False while the flow window is still loading. A zero is a
   *  MEASUREMENT — "darkmux dispatched no tokens in this window" — and
   *  rendering one before the window has arrived states a fact nobody has
   *  established.
   *  The figures are silhouetted until this is true.
   *
   *  `settled && 0` stays a literal "0": a fresh fleet with no dispatches
   *  genuinely has none, and that is worth saying. Only the not-yet-known
   *  case is silhouetted. */
  settled: boolean;
}) {
  const hours = Math.round(LIVE_WINDOW_MS / 3600000);
  // (#2878) `null` while unsettled, so the first real total lands instantly;
  // a later change counts up, and only up (the 24h window slides past old
  // records on every poll with nothing running). (Playback parity, Change
  // B) NOT gated on `liveMode` any more — `useCountUp` itself reads the
  // one shared seek signal and snaps only across an actual scrub/rewind;
  // an ADVANCE (a play tick, or a live poll) tweens in both modes now,
  // which is the parity fix (finding #5: a 30s advance tweened live and
  // jumped in playback for the identical figure).
  const heroTotal = useCountUp(settled ? t.total : null, (n) => (n === null ? "" : fmtN(n)), undefined, {
    upOnly: true,
  });

  return (
    // The silhouette is applied in CSS off this one attribute so the figures
    // keep their exact geometry — same elements, same sizes, digits hidden.
    // #2068 measured CLS 1.21 from a tile that mounted and unmounted with
    // transient data; a loading state that changes the layout would
    // reintroduce exactly that. `aria-busy` tells a screen reader the region
    // is pending rather than reading placeholder digits as values.
    <div className="savings" data-settled={settled ? "true" : "false"} aria-busy={!settled}>
      {/* (operator) "tokens · last 24h" rather than "by your fleet · last 24h",
          to match the event pane's "events last 24h". Two panels counting two
          things over the same window should say so the same way; "by your
          fleet" named the SOURCE where its neighbor named the SUBJECT.

          The window suffix is LIVE-ONLY — `const win=...live-mode...?` last
          ${h}h`:''`. A replay's numbers cover the recorded
          day, not the last 24 hours, and the meta bar already states that
          day's range. (#1800 P2: the suffix was unconditional, so a replayed
          day claimed a window it had not been measured over.) */}
      {/* (#2834, operator) The section is ABOUT darkmux tokens; the figure
          is all of them over the window. So the eyebrow names the subject
          and the label under the number names what the number counts.
          Before this the two were swapped — the eyebrow said "tokens · last
          24h" and the label said "darkmux tokens", which read as a section
          called "tokens" containing a figure called "darkmux tokens", with
          the window attached to the wrong one. */}
      <div className="saveyebrow">darkmux tokens</div>
      <div className="savrow">
        {/* (#2834) ONE figure: every token darkmux dispatched in the window.
            It used to be three — local, cloud, unattributed — split on a
            predicate that cannot carry the distinction. `is_remote()` is
            `endpoint.url.is_some()`, so ANY OpenAI-compatible endpoint read
            as cloud, including a local inference server on 127.0.0.1. A
            Splash run on this machine's own GPU, at zero marginal cost, was
            counted against "cloud tokens" and the savings figure inverted.

            The split is not being fixed here, it is being WITHDRAWN. Whether
            an endpoint costs money is not derivable from its URL — a
            self-hosted model on a rented VPS is metered, a local server is
            not, and both are "an endpoint with a URL". It is a property the
            operator knows and darkmux does not, so darkmux should stop
            asserting it. #1521 tracks the real design: per-endpoint
            attribution with metering declared rather than inferred.

            "Unattributed" goes with it, and it was always a symptom of the
            same thing: a bucket for sessions whose endpoint could not be
            determined, which only needs to exist when the endpoint decides
            the bucket. With one figure there is nothing to be unattributed
            FROM — the tokens were dispatched by darkmux either way, which
            is the only claim being made. */}
        <div className="savlead">
          {/* (#2862) A shimmer while unsettled; (#2878) once settled the
              total counts up on new work. The count-up hook is hoisted above
              the return (hooks cannot sit in a conditional branch). */}
          {settled ? <div className="savnum">{heroTotal}</div> : <Shimmer as="div" className="savnum" minHeight="1em" />}
          {/* (#2902) UTILITY is a PART of all tokens, not a peer figure, so
              it sits with the total it belongs to; a peer chip read as a
              third bucket to add. (layout) It takes no line of its own: on
              the label's line under the figure on a desktop, beside the
              figure on a phone (`.savlblwrap` in `styles.css`), so the hero
              is the same height with or without it. */}
          <div className="savlblwrap">
            <div
              className="savlbl"
              title={totalHint(t.noRun, t.unlisted, notStreaming)}
            >
              {notStreaming.length > 0 ? "tokens seen" : "all tokens"}
              {liveMode ? ` · last ${hours}h` : ""}
            </div>
            {t.utility && settled ? <div className="savpart"><span className="savpartv">{fmtC(t.utility)}</span> utility</div> : null}
          </div>
        </div>
        <div className="savclasses">
          {/* (#2902 step 2a) Every chip is a sum of provider-reported
              counts from the usage records (`tokensOffMeter`). CACHED is
              absent when no record in the window reports `cached_tokens`
              (a 0 there would be an assumption, not a measurement); a
              REPORTED 0 renders "0 cached" by design — hosted providers
              report `cached_tokens: 0` on every reply, and that is a
              measurement worth showing. UTILITY (darkmux's own jobs —
              compaction and the radio router, `call_purpose`) is hidden at
              0: it is a part line, not a chip, and a "0 utility" line under
              the total would read as a third figure. CACHED and UTILITY are
              shares of INPUT and ALL TOKENS, so each renders as a part line
              under the figure it belongs to, never as a peer chip. INPUT +
              GENERATED equal ALL TOKENS whenever providers report total =
              prompt + completion; no filler chip covers a provider total
              above it. */}
          <Chip value={fmtC(t.input)} loading={!settled} label="input" part={t.cached != null ? { value: fmtC(t.cached), label: "cached" } : null} />
          <Chip value={fmtC(t.generated)} loading={!settled} label="generated" cls="gen" />
          <Chip value={t.runs} loading={!settled} label={`dispatch${t.runs === 1 ? "" : "es"}`} />
        </div>
      </div>
    </div>
  );
});

/**
 * The fleet default view — `renderFleet()`: the
 * savings hero, one card per machine, and the recent-activity timeline.
 * `/next`'s default (no-hash) route. See `savings.ts`/
 * `cards.ts`/`timeline.ts` for the ported pure logic this component
 * composes.
 *
 * Data sources: `/flow/<today>` + `/flow/<yesterday>` (the live window every
 * number here derives from — `useFlowWindow`), `/fleet/machines/live` +
 * `/fleet/dispatches/live` (presence), `/machine/specs` (this machine's own
 * hardware string).
 */
// (#1729) The presence-coverage notice this lens used to own MOVED to
// `components/FleetCoverageNotice.tsx` in #2683 and now mounts once, from
// `App.tsx`, above every lens. It is not re-mounted here: the masthead makes
// the same presence-derived claim this view does, one shared indicator covers
// both, and two copies on the fleet route would have shown the identical
// banner twice. The reasoning (and the unchanged wording) lives in that
// component's own doc.

/**
 * (#1923 review) The `/runs` read failed — say so, rather than letting the
 * cards below silently assert "no lab runs".
 *
 * The cards' lab-run count comes from `GET /runs`; a failed read yields the
 * same empty list a healthy idle machine does, so the card falls back to
 * exactly the "idle while a lab run is live" reading #1923 removed. It is
 * the sibling of the app-level `FleetCoverageNotice` (presence unreadable,
 * mounted from `App.tsx` since #2683) applied
 * to the other source this lens depends on, and follows `RunsBoard`'s own
 * habit of naming what it could not read instead of rendering the absence
 * as data.
 *
 * Deliberately does NOT say the machine IS running lab work — the whole
 * point is that this page no longer knows. `role="status"`, same as the
 * coverage notice: informational, never an interruption.
 *
 * Worded to share NO phrase with `FleetCoverageNotice`'s "could not be
 * read": the two can legitimately fire together (a machine that can reach
 * neither Redis nor its own `/runs`), and a reader — or a test's
 * `getByText` — has to be able to tell which source is missing.
 */
function RunsUnreadableNotice({ unreadable, message }: { unreadable: boolean; message?: string | null }) {
  if (!unreadable) return null;
  return (
    <div className="fleetcov" data-state="runs-unreadable" role="status">
      <span className="fleetcov__icon">⚠</span>
      <span>
        Run records are unavailable{message ? ` (${message})` : ""} — lab runs are missing from the counts below, so a
        machine working through one may read idle.
      </span>
    </div>
  );
}

/**
 * (#1855 follow-up, CONSIDER 4) `GET /fleet/roster` failed to PARSE (the
 * file exists but is corrupt — an operator hand-edit gone wrong). Before
 * this, `useFleetRoster` dropped the error entirely and every consumer saw
 * exactly what a genuinely-empty roster looks like — every previously-
 * visible rostered card silently vanishing again, which is the precise
 * symptom #1855 exists to fix, now caused by this fix's own read path.
 *
 * Sibling of `RunsUnreadableNotice` above (same `.fleetcov` shape, same
 * `role="status"` — informational, never an interruption), worded to share
 * no phrase with either sibling notice so a reader — or a test's
 * `getByText` — can tell which source failed when more than one fires at
 * once. `message` is always the SERVER's fixed literal
 * (`ROSTER_READ_FAILED`), never the raw parse error — that crate's own doc
 * says why (the parse error embeds this roster file's local path, which
 * carries the operator's username on macOS).
 */
function RosterUnreadableNotice({ error }: { error: string | null }) {
  if (!error) return null;
  return (
    <div className="fleetcov" data-state="roster-unreadable" role="status">
      <span className="fleetcov__icon">⚠</span>
      <span>Fleet roster unreadable ({error}) — a rostered-but-silent machine may be missing from the cards below.</span>
    </div>
  );
}

/** (#2928 re-review, C-1) The activity lanes and axis, memoized on the
 *  timeline object: live samples re-render the fleet lens several times a
 *  second, and the timeline (rebuilt once per wall second or data change)
 *  never reads them, so its hundreds of bars are not re-diffed per sample. */
const NO_RUNS: import("../../types/generated/Run").Run[] = [];

const TimelineLanes = memo(function TimelineLanes({ timeline }: { timeline: ReturnType<typeof buildActivityTimeline> }) {
  return (
    <>
      {timeline.lanes.map((lane) => (
        <div className="lane" key={lane.uid} data-flip-key={lane.uid}>
          <div className="lname" title={lane.name}>
            {lane.name}
          </div>
          <div className="tltrack">
            {/* (#1639, drill-in packet) Session drill — click a bar, land
                on `#dispatch=<sid>`. Legacy's OWN `.sbar` bars are inert
                (no `data-act`, no click handler anywhere in
                `viewer.html`'s timeline code); legacy's only session-drill
                click was `recentRow()`'s "open →" link on the machine
                page's per-run list, which #1809 removed outright when it
                replaced that list with a link into the runs lens (see
                `MachineLens.tsx`'s own doc, and `viewer-session-url.spec.js`'s
                module doc for the full gap history). Since #1809 nothing
                ANYWHERE in this port reaches `SessionReplay` by clicking,
                even though the fetch + render it needs (`/flow-dispatch/<id>`
                → `runRegions`) has worked since Packet 4.
                This is a deliberate WIDENING beyond legacy's own address-bar
                behavior, same precedent as `machineDrillHash`'s machine key and
                the `machine=` runs-lens pin above: the activity lane already
                names every session on screen (`bar.sid`, carried into
                `bar.title`), so it is the least-surprising place to attach
                the click legacy never wired. A real `location.hash` write
                (not `writeHash`/`replaceState`) — the same mechanism every
                other cross-lens hop in this file uses — so `hashchange`
                fires, back/forward/copy-paste all behave, and `useSyncHash`
                never has to reconcile a route no navigation actually
                happened for. */}
            {lane.bars.map((bar) => (
              <div
                key={bar.key}
                className={`sbar ${bar.status}`}
                style={{ left: `${bar.leftPct}%`, width: `${bar.widthPct}%` }}
                title={bar.title}
                data-act="session"
                data-arg={bar.sid}
                role="button"
                tabIndex={0}
                onClick={() => {
                  location.hash = bar.hash;
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    location.hash = bar.hash;
                  }
                }}
              />
            ))}
            <div className="ph" style={{ left: `${timeline.playheadPct}%` }} />
          </div>
        </div>
      ))}
      <div className="tlaxis">
        <span>{timeline.axis[0]}</span>
        <span>{timeline.axis[1]}</span>
        <span>{timeline.axis[2]}</span>
      </div>
    </>
  );
});

/** (#1800 P2) `records`/`tMax`/`tMin` OPTIONAL so playback can render this
 * same hero over a historical day. Omitted = the live rolling window, exactly
 * as before, so every existing caller is unchanged.
 *
 * `historical` is legacy's `liveMode`, inverted — and it is NOT merely a
 * presence switch. Legacy branches on it in FOUR places inside `renderFleet()`
 * + `savingsHero()`, and the port had collapsed all four to their live arm
 * because `/next` had no route that reached the other one:
 *
 * | surface | live | replay |
 * |---|---|---|
 * | hero eyebrow | `tokens · last 24h` | `tokens` |
 * | card count | running sessions, "N running" | the day's sessions, "N specialists" |
 * | timeline span | `max(tMax, now) - window` | `tMin..tMax` |
 * | window control | 10m/1h/4h/24h | absent |
 *
 * Presence is the fifth: `liveMachines`/`liveSessionIds` are LIVE endpoints
 * describing NOW, so a replay must neither fetch nor consult them. Asserting
 * today's presence over a past day is exactly the "confidently WRONG:
 * machines read idle, running work reads zero" failure this file's own
 * coverage notice exists to warn about.
 *
 * (#1869) `tMax` was a fixed ceiling (`computeTMax(records)`, the day's true
 * max) AND the de facto playhead until this packet — the two were always
 * the same number, because nothing before `PlaybackLens`'s own transport
 * ever scrubbed. `PlaybackLens` now owns a real `t` (playhead) state and can
 * pass anything from `tMin` up to that ceiling, driven by its `Scrubber`.
 * That makes `tMax`-as-playhead a real conflation instead of a harmless one
 * — measured live: rewinding to a day's start collapsed the activity axis
 * itself (`tlMin..tlMax`) down to a single instant instead of staying fixed
 * while the playhead marker swept back across it, because the SAME number
 * was feeding both roles. This component now takes a separate `playhead`
 * prop (below) and threads TWO numbers where it used to thread one:
 * `flowWindow.tMax` stays the fixed axis ceiling everywhere it already fed
 * `cards.ts`/`timeline.ts`'s ceiling-shaped arguments; `playheadT` (derived
 * below) is the actual bracketing value — `machPresent`, `buildFleetCardBase`'s
 * `t`, `buildActivityTimeline`'s new `playheadT` argument, and `scopedData`
 * (the token hero has no playhead argument of its own, so its "as of the
 * playhead" gate is applied to the array it's handed instead). See
 * `timeline.ts`'s own module doc for the fuller account of the bug this
 * split fixes. */
export function FleetLens({
  records,
  tMax,
  tMin,
  playhead,
  historical = false,
  connected = true,
  lastContactMs = null,
}: {
  records?: NormRecord[];
  tMax?: number;
  tMin?: number;
  /** (#1869) The scrub PLAYHEAD — a genuinely separate value from `tMax`
   * once `PlaybackLens` has a real transport. Defaults to `tMax` (the old,
   * pre-transport behavior: playhead == ceiling, always). `tMax` itself
   * stays the FIXED axis ceiling — `PlaybackLens` passes the day's true
   * `computeTMax(records)` there, unmoved by scrubbing, and its scrubbable
   * `t` state here instead. See `timeline.ts`'s own module doc for the bug
   * this split fixes: collapsing both into one number made the activity
   * axis itself shrink as the playhead scrubbed back, instead of staying
   * fixed while a marker sweeps across it. */
  playhead?: number;
  historical?: boolean;
  /** (#2886 pass 3, "STALL while disconnected") Whether the PAGE has a
   * working connection to the daemon right now — `App.tsx`'s live render
   * passes `useLiveTail`'s status. Defaults to `true`, so a `historical`
   * (playback) render — which never passes this — always reads as
   * connected: disconnection is meaningless there, since `useLiveTail`
   * never even runs on a playback route (`isLiveRoute` excludes it) and
   * would otherwise report a permanent, misleading "reconnecting". See
   * `lib/tokenRate.ts::liveStateWhileConnected`'s own doc. */
  connected?: boolean;
  /** (#2886 pass 4, do-it — fresh-reviewer finding 5, "half-open connection
   * race") `App.tsx`'s `lastContactRef.current` — the last moment
   * `useLiveTail` confirmed contact with the daemon. `null` (the default)
   * on every call that doesn't pass it (a `historical` render, or a test),
   * which skips the half-open check in `withLiveReadings` entirely and falls
   * back to the plain `connected` boolean — see that function's own doc. */
  lastContactMs?: number | null;
} = {}) {
  // (Playback parity, Change A) `wallNow` feeds ONLY the live fetch window
  // below (`useFlowWindow`) — "what is fetched" is the one thing liveMode
  // is still allowed to decide. Every RENDER-TIME derivation reads
  // `playheadT` instead (defined below as `playhead ?? wallNow` — a
  // literal `Date.now()`, not this frozen value, so a replay's `wallNow` is
  // never mistaken for its own clock).
  const wallNow = Date.now();
  const liveMode = !historical;
  /** (U5-1) Whether a daemon exists to ASK — a different question from
   * `liveMode`, which is the caller's intent ("this mount is a replay").
   * `App.tsx` renders `<FleetLens />` propless, so `historical` defaults to
   * `false` on the daemon-less static demo too, and the three live-only
   * endpoints below fired there: measured on the served build, `#lens=fleet`
   * produced 404s for `/fleet/machines/live`, `/fleet/dispatches/live` and
   * `/machine/specs` plus their console errors. Gating on the BUILD is the
   * #1801 rule `MachineLens`, `useFlowWindow` and `route.ts::isLiveRoute`
   * already follow: a daemon-less build is never live, on any lens, whatever
   * a caller passed.
   *
   * Deliberately a SEPARATE constant from `liveMode` rather than folded into
   * it: `liveMode` also drives DISPLAY (the hero's "last Nh" eyebrow, the
   * activity-window control), and
   * those already render correctly on the demo. This changes what is
   * REQUESTED and nothing else — every one of these three endpoints 404s on
   * a static build today, so their gated-off results were already the empty
   * values the consumers below receive. */
  const livePolling = liveMode && getSource().kind === "daemon";
  // (#2890) A replay's timeline defaults to "all": the recording's own span,
  // edge to edge, so a short recording (the demo is about half an hour)
  // fills the lanes instead of sitting as a sliver at the right edge of a
  // preset. "all" is offered only in a replay; a preset the operator picks
  // replaces it. Live keeps the 24h default and has no "all".
  const recordingRange: [number, number] | null =
    historical && tMin != null && tMax != null && tMax > tMin ? [tMin, tMax] : null;
  const [windowMinutes, setWindowMinutes] = useState<number | "all">(() =>
    recordingRange ? "all" : DEFAULT_ACTIVITY_WINDOW_MIN,
  );
  const fixedRange = windowMinutes === "all" ? (recordingRange ?? undefined) : undefined;
  const windowMinutesNum = windowMinutes === "all" ? DEFAULT_ACTIVITY_WINDOW_MIN : windowMinutes;
  // (#2881) The pager's sticky PICK, per machine uid — the session id the
  // operator last chose with an arrow, if any. `FleetCard.executions` no
  // longer including it (that execution ended) falls back to the AUTO
  // default (`effectiveDefaultSid`, computed per card below) on the very
  // next render, which is the whole of "sticks until that execution ends,
  // then moves to the next [busiest]" — there is no separate cleanup step,
  // and no live/playback branch: the same fallback rule applies to a
  // replayed instant too.
  const [pinnedPageByUid, setPinnedPageByUid] = useState<Record<string, string>>({});
  const showExecution = useCallback((uid: string, sessionId: string) => setPinnedPageByUid((m) => ({ ...m, [uid]: sessionId })), []);
  // (#2886 pass 5, MUST — fresh-reviewer finding F6) The pager's AUTO
  // (unpicked) default, per machine uid — the session id currently shown as
  // page 1 when the operator hasn't picked one. A `useRef`, not `useState`:
  // it's read and written in the SAME render pass, purely to remember what
  // was shown last render so `isStrictlyBusier` has something to compare
  // against — it never itself needs to SCHEDULE a re-render (new flow data
  // arriving already does that). Recomputing the default from scratch every
  // tick (`busiestExecution` alone, with no memory) flapped a real fleet's
  // page 46 times in 863s, because two executions' fluctuating rates kept
  // trading the tie-break; see the `cards.map` callback below for the
  // guarded update.
  const stickyDefaultByUidRef = useRef<Record<string, string>>({});

  // (#2890) Size each card's tube to the room it has (see `tubeFit.ts`):
  // after every render, since a rate or a pager changes the text, and on
  // every resize of the card grid.
  const fleetRef = useRef<HTMLDivElement | null>(null);
  const tlRef = useRef<HTMLDivElement | null>(null);
  useFlip(fleetRef);
  useFlip(tlRef);
  // (#2928 re-review, C-1) A tube's size reads only its card's width, so it
  // is refitted when the set of scope-bearing cards (or their pagers)
  // changes, and on resize below: not on every render, where each call
  // forced a synchronous layout and a live sample re-renders several times
  // a second. `fitSignature` is defined once the cards are (below); a ref
  // carries it up to this effect, which runs after that render commits.
  const fitSignatureRef = useRef("");
  const lastFitRef = useRef<string | null>(null);
  useLayoutEffect(() => {
    if (lastFitRef.current === fitSignatureRef.current) return;
    lastFitRef.current = fitSignatureRef.current;
    fitTubes(fleetRef.current);
  });
  useEffect(() => {
    const el = fleetRef.current;
    if (!el || typeof ResizeObserver === "undefined") return undefined;
    const ro = new ResizeObserver(() => fitTubes(el));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const liveWindow = useFlowWindow(wallNow);
  const flowWindow = records !== undefined
    ? { data: records, tMax: tMax ?? 0, settled: true, failure: null }
    : liveWindow;
  // (Playback parity, Change A — "one clock") The clock every bracketing
  // derivation below reads: the playhead when scrubbed, the real wall
  // clock at the live edge. This USED to be `playhead ?? flowWindow.tMax`
  // — the newest RECORD's timestamp, not now — which is the Side finding
  // in the parity spec: the live fleet card judged staleness against
  // whenever a record last happened to arrive, so the stalled ring only
  // appeared once another record showed up, sometimes long after the run
  // actually went quiet. `flowWindow.tMax` is still used as the FIXED axis
  // ceiling it always was (see `timeline.ts`'s own doc) — this is the
  // separate, moving "now" value.
  const playheadT = playhead ?? wallNow;
  // (#2961) The playhead's clock, for REST's seconds hand.
  const playbackClock = usePlaybackClock();
  // `enabled: false` stops the REQUEST, not just the result: an earlier draft
  // discarded the data while the hook kept polling `/fleet/machines/live`
  // every few seconds behind a replay.
  //
  // It is NOT sufficient on its own, and the QA gate proved it: a disabled
  // TanStack observer still READS the shared cache slot, so as long as ANY
  // enabled observer of the same key exists anywhere in the tree, this one
  // keeps returning live beats and the poll never stops. `App.tsx` held
  // exactly such an observer. Gating the fetch AND the consumer is what makes
  // the property true in the composed app rather than only in this lens's own
  // isolated test.
  const liveMachines = useLiveMachines(livePolling);
  // (#2725) `coverage` is deliberately NOT read here: this lens sits under
  // the app-wide `FleetCoverageNotice` (`App.tsx` mounts it for every route),
  // which already says the one sentence this app has about a degraded
  // presence read — and it derives that from the machines half of the same
  // substrate, so a sessions read that failed means the machines read failed
  // too. A second marker on this page would be the duplicate wording #2683
  // removed. Destructured explicitly rather than ignored implicitly so the
  // choice is visible: the hook no longer discards the signal, this caller
  // does, on the record, for a stated reason.
  const { sessions: liveSessionIds } = useLiveSessionIds(livePolling);
  // (#1855) The operator's DECLARED roster, gated the same way as presence
  // above — a replay must not assert the CURRENT roster over a past day.
  // See `rosterOnlyEntries`'s own doc for how this is reconciled with the
  // presence/flow-derived uids so a machine that's already accounted for
  // (beating, or with flow history under this name) is never duplicated.
  //
  // (#1855 follow-up, CONSIDER 4) `error` renders via `RosterUnreadableNotice`
  // below — a corrupt roster file used to read as an empty one with no
  // signal anywhere that the read had failed, silently reproducing the
  // exact "machine vanishes" symptom #1855 exists to fix.
  const { machines: roster, error: rosterError } = useFleetRoster(livePolling);
  // The fleet view (`GET /fleet/view`, or a static build's committed
  // snapshot of it): the machine list, and for each machine its card, its
  // liveness and what it lets this machine do. A replay never asks (a view
  // describes now); the machine serving the page is the row that says so, and
  // its card's specs are the self identity the naming and key rules read.
  const fleetView = useFleetView(liveMode);
  const viewRows = fleetView.rows;
  const specs = useMemo(() => {
    const self = viewRows?.find((r) => r.is_this_machine);
    return self ? rowSpecs(self) : null;
  }, [viewRows]);
  // (#1923) `GET /runs` — already fleet-aware, already unions lab + flow
  // sources server-side (`build_runs`) — read here ONLY to fill the gap
  // flow presence structurally cannot: a lab run in flight, which
  // deliberately never rides the flow stream (the lab/fleet sink boundary,
  // CLAUDE.md contract 3). This is a display-layer read, not a new writer
  // into that stream — see `cards.ts::runningLabRunCount`'s own doc.
  //
  // Fetched on BOTH build kinds (unlike `specsQuery` above, which is
  // genuinely live-only) — `runsSrc()` resolves to a committed fixture on a
  // static build, same as `RunsBoard.tsx`'s own `runsQuery`, whose
  // `queryKey` this intentionally reuses so the two lenses share one cache
  // entry instead of two independent polls of the same data. Replay mode
  // still gets a value here (there is no reason to withhold it), but
  // the card's lab count (`buildFleetCardBase`'s `machineRuns`) reads it.
  // `enabled` on `runsReachable()`: a daemon-less static build that ships no
  // committed runs fixture has nothing to answer this, and `runsSrc()`'s
  // daemon fallback would fetch `/runs` off a page with no daemon (see that
  // predicate's own doc — the 404 the static-build gate catches).
  const runsQuery = useQuery({
    queryKey: queryKeys.runs(),
    queryFn: () => fetchJson<RunsResponse>(runsSrc()),
    enabled: runsReachable(),
    refetchInterval: livePolling ? PRESENCE_POLL_MS : false,
  });
  // `?? []` guards a malformed/shape-mismatched 200 (a test double, or a
  // future API drift) the same way every OTHER field on this response is
  // already optional-safe — `fetchJson`'s `ok: true` only proves the body
  // parsed as JSON, not that it matches `RunsResponse`.
  // (#2928 re-review, C-1) One stable empty list: a fresh `[]` each render
  // was a new input to the card bases, rebuilding them on every render.
  const runs = (runsQuery.data?.ok ? runsQuery.data.data.runs : NO_RUNS) ?? NO_RUNS;
  // (#1923 review) …but an empty list from a FAILED read is not the same
  // claim as an empty list from a healthy daemon, and the cards cannot tell
  // them apart: both render "idle" / "0 running". That is the exact lie
  // #1923 exists to remove, restored by a `/runs` that 500s or times out.
  // So the failure is named, the way `RunsBoard` names its own
  // (`labSourceNotice`) and `specOf` distinguishes "no specs" from
  // "hardware not reported" — the `?? []` fallback keeps the lens rendering,
  // this says what it is missing. `enabled: false` (a static build with no
  // committed fixture) is NOT a failure: nothing was asked, so `isError` is
  // false and `data` is undefined, and both arms below stay quiet.
  //
  // LIVE MODE ONLY, for the same reason `FleetCoverageNotice` is historical-
  // gated: a replayed day is drawn from its records, so a failed `/runs` costs
  // it nothing, and warning about it there would be the bug.
  const runsUnreadable = liveMode && (runsQuery.isError || runsQuery.data?.ok === false);
  const runsErrorMessage = runsQuery.data && !runsQuery.data.ok ? runsQuery.data.message : null;

  // (#2958) Which of the cards' sources have answered once (see
  // `cardFace`'s doc for what each one gates, and why). The two
  // presence hooks above return data, not status, so these are observers of
  // their shared cache slots for the settle state only: disabled, the hooks
  // above own the fetch (the same pattern as `useMachineKeyContext`).
  // `/runs` counts only in live mode: a replay never reads it.
  const presenceState = useQuery({
    enabled: false,
    queryKey: queryKeys.fleetMachinesLive(),
    queryFn: () => fetchJson<FleetMachinesLiveResponse>("/fleet/machines/live"),
  });
  const sessionsState = useQuery({
    enabled: false,
    queryKey: queryKeys.fleetSessionsLive(),
    queryFn: () => fetchJson<FleetDispatchesLiveResponse>("/fleet/dispatches/live"),
  });
  // Each latched: the FIRST answer counts (`useLatch`), so the flow
  // window's new day key at UTC midnight does not blink every card back to
  // "checking…".
  const flowAnswered = useLatch(flowWindow.settled);
  const presenceAnswered = useLatch(!livePolling || presenceState.status !== "pending");
  const sessionsAnswered = useLatch(!livePolling || sessionsState.status !== "pending");
  const runsAnswered = useLatch(!(liveMode && runsReachable()) || runsQuery.status !== "pending");
  // The view says who is up for every machine it holds, so "offline" waits on
  // it as well as on presence.
  const viewAnswered = useLatch(fleetView.answered);
  const orderState = useCardOrderGate(viewAnswered);
  // (#2965) A failed flow read settles the window with no records, which
  // is what a quiet window looks like: the flow source has not answered
  // while its read is failing, so the claims it backs hold "checking…".
  // `FlowReadNotice` (app-level) says why. The latch still covers the
  // midnight rollover: a new day's PENDING key is not a failure.
  const flowKnown = flowAnswered && flowWindow.failure === null;
  const answered = useMemo<CardSourcesAnswered>(
    () => ({ flow: flowKnown, presence: presenceAnswered && viewAnswered, sessions: sessionsAnswered, runs: runsAnswered }),
    [flowKnown, presenceAnswered, viewAnswered, sessionsAnswered, runsAnswered],
  );


  // (#1869) The token hero is "as of the playhead" — legacy's
  // own `visible()` gate (`DATA.filter(r=>T(r.ts)<=state.t)`), restored at
  // this call site rather than inside `tokensOffMeter`
  // itself (see `savings.ts`'s module doc for the full reasoning). In
  // replay, `playheadT` is the scrubbable
  // position `PlaybackLens` passes as its `playhead` prop, so this is what
  // makes scrubbing before a session's completion drop that session's
  // tokens out of "local" and into "unattributed" — the token half of the
  // issue's own acceptance test.
  //
  // (#1869 code review) This scopes only what THIS component owns — the
  // hero + timeline + fleet cards below. It does NOT reach the event log:
  // that's App-level chrome, a DOM SIBLING of this whole lens (mounted by
  // `App.tsx` beside `#stage`, not inside it), so it was never in scope for
  // a fix made from in here. That was a real, separate gap (the log kept
  // listing the whole day regardless of where the scrubber sat, while this
  // hero already tracked it) — closed at the App level instead, via
  // `PlaybackLens`'s `onPlayheadChange` reporting the same `playheadT` this
  // line reads up to `App`, which threads it into `EventLogColumn`. See
  // `App.tsx`'s own `eventLogRecords` doc for that half.
  //
  // (#2911) Live, the gate is "as of now" (`wallNow`): a record stamped after
  // the viewer's own clock (a peer whose clock runs ahead) is not counted
  // until the clock reaches it, the same rule the fleet cards apply
  // (`cards.ts`). `recordsAsOf` keeps that from costing a whole-window filter
  // on every 1 Hz tick: with nothing ahead of now it returns the window
  // itself, the same reference each tick, so the hero's token sums do not
  // recompute; with a record ahead, it filters once and re-filters
  // only when the window changes or now crosses that record. A replay cuts
  // the same way at the playhead.
  const scopedData = useMemo(
    () => recordsAsOf(flowWindow.data, playhead ?? wallNow),
    [flowWindow.data, playhead, wallNow],
  );
  const runIds = useMemo(() => (runsQuery.data?.ok ? new Set(runs.map((r) => r.id)) : undefined), [runsQuery.data, runs]);
  const tokens = useMemo(() => tokensOffMeter(scopedData, runIds), [scopedData, runIds]);

  // (#2928) The live channel's overlay: at the live edge of a live route
  // only (`livePolling` is false on a static build, `playhead` is set on a
  // replay), so playback and a scrubbed view derive from durable records
  // alone. Subscribing re-renders this lens on each live sample.
  const liveOverlay = useLiveOverlay(livePolling && playhead == null);
  // (#2928) What moves at the 1 s clock's pace, not at the live channel's:
  // live samples re-render this lens up to 4 times a second per execution,
  // and only the cards read them. At the live edge the running set and the
  // activity timeline are recomputed when the wall second changes (or their
  // inputs do), exactly as often as before the live channel existed.
  // Measured on the busy-day fixture, recomputing both on every sample was
  // most of the feed's cost. A replay keys on the playhead itself.
  const liveEdgeClock = playhead == null ? Math.floor(playheadT / 1000) : playheadT;
  // Session presence: an ADDITIVE input to each run's lifecycle
  // (`lib/lifecycle.ts`), never a subtraction. It is a fact about NOW: a
  // replay reads none (the presence hook is disabled there), and a scrubbed
  // playhead on a live day judges from records up to the playhead alone.
  const presence = judgementAt(playhead ?? null, playheadT, liveSessionIds).presence;
  const policy = useLifecyclePolicy();
  // (#2814) SELF IS NEVER UNKNOWN. `machineUids` unions flow-derived uids
  // with currently-beating presence keys, both empty on a machine whose
  // Redis is off or whose last record aged out of the window. The uid the
  // daemon reports for itself is not an observation and does not belong to
  // the window, so it is appended when the view's self row carries one.
  // Deduped against the derived set. `specs` is null in a replay.
  const uids = useMemo(() => {
    const derived = machineUids(flowWindow.data, liveMachines);
    const selfUid = specs?.machine_uid;
    return selfUid && findUid(derived, selfUid) === null ? [...derived, selfUid] : derived;
  }, [flowWindow.data, liveMachines, specs]);
  // The machine list. Every row of the view is a card, whether or not
  // anything was ever recorded about it: a peer with Redis off, a machine
  // down, a peer whose card could not be read. A machine the view does not
  // hold (an unverified source, a beating machine nobody rostered, or every
  // machine when there is no view: a replay, a failed read) is drawn from
  // flow and presence alone.
  const viewCards = useMemo(() => {
    const known = new Set(uids);
    const selfUid = specs ? (uids.find((u) => isSelfMachine(flowWindow.data, liveMachines, specs, u)) ?? null) : null;
    const flowUidOfName = (name: string) => uidForName(flowWindow.data, liveMachines, name);
    return (viewRows ?? []).map((row) => ({ row, facts: rowFacts(row, known, selfUid, flowUidOfName) }));
  }, [viewRows, uids, specs, flowWindow.data, liveMachines]);
  const flowOnlyUids = useMemo(() => {
    // `facts.uid` is already in the flow window's spelling of the uid
    // (`rowUid`), so a view row that spells it differently covers that
    // machine's flow card instead of adding a second one.
    const covered = new Set(viewCards.flatMap(({ row, facts }) => [facts.uid, row.machine_uid ?? facts.uid]));
    return uids.filter((m) => !covered.has(m));
  }, [viewCards, uids]);
  // (#2929) What a card's link names its machine by: the key the runs board
  // resolves back from the same window, beats, specs and roster.
  const machineKeyCtx = useMemo(
    () => ({ data: flowWindow.data, liveMachines, specs, roster }),
    [flowWindow.data, liveMachines, specs, roster],
  );

  // (#2928 re-review, C-1) Two stages. The BASE of each card reads the whole
  // window (activity, running sessions, names, hardware, counts): built once
  // per data change and per wall second (`liveEdgeClock`), exactly as often
  // as before the live channel existed. The live READINGS (scope rate and
  // state, pages, utility strip) are then derived per render from the base,
  // touching only the running sessions, so a live sample never rescans the
  // window. A replay keys the base on the playhead itself.
  const baseCards = useMemo(() => {
    const viewBases = viewCards.map(({ row, facts }) => ({
      order: { uid: row.machine_uid ?? null, fallback: row.entry?.id ?? facts.uid },
      base: buildFleetCardBase(
        flowWindow.data,
        liveMachines,
        specs,
        presence,
        false,
        facts.uid,
        playheadT,
        facts,
        // (#1923) A run belongs to the card's machine by uid
        // (`runsForMachine`'s own doc); the name the view gives the machine
        // is one more spelling it answers to.
        runsForMachine(runs, machineMatch(flowWindow.data, liveMachines, specs, roster, facts.uid, facts.names)),
        roster,
        policy,
      ),
    }));
    const flowBases = flowOnlyUids.map((m) => ({
      order: { uid: m, fallback: m },
      base: buildFleetCardBase(
        flowWindow.data,
        liveMachines,
        specs,
        presence,
        machPresent(flowWindow.data, liveMachines, playheadT, m) === false,
        m,
        playheadT,
        null,
        runsForMachine(runs, machineMatch(flowWindow.data, liveMachines, specs, roster, m)),
        roster,
        policy,
      ),
    }));
    // The ONE place the card order is decided (`cardOrder.ts`): neither the
    // view's order nor the flow window's, so which source answered first
    // never moves a card.
    return orderCards([...viewBases, ...flowBases], (b) => b.order).map((b) => b.base);
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `playheadT` is read through `liveEdgeClock` on purpose (#2928, above).
  }, [viewCards, flowOnlyUids, flowWindow.data, liveEdgeClock, liveMachines, specs, presence, runs, roster, policy]);
  const cards = useMemo(
    () => baseCards.map((b) => withLiveReadings(b, playheadT, connected, lastContactMs, liveOverlay)),
    [baseCards, playheadT, connected, lastContactMs, liveOverlay],
  );
  // (#2911) The card ticks while an execution is live. Nothing above
  // re-rendered this lens between records: SSE contact is a ref, presence
  // re-renders only on a changed payload, and a resting runtime emits no
  // heartbeats, so a REST countdown froze for the 5 s host-sampler cadence
  // (or 20 s with the sampler off) and then jumped, and the rest -> prompt
  // flip and stall detection waited the same way. Subscribing to the shared
  // 1 s clock re-renders this component every second; `wallNow` above is a
  // fresh `Date.now()` on each of those renders, which is what every card
  // derivation reads. The same gate as `SessionReplay`'s `ticking`: live
  // edge only (a replay's clock is the transport's), and only while a card
  // has a live execution, so an idle fleet page runs no timer at all
  // (`useNowMs` subscribes to nothing when inactive).
  fitSignatureRef.current = cards.map((c) => `${c.uid}:${c.liveTokRate !== null ? 1 : 0}:${c.executions.length}:${c.active ? 1 : 0}`).join("|");
  const ticking = livePolling && playhead == null && cards.some((c) => c.liveTokRate !== null);
  useNowMs(ticking);

  // The activity lanes follow the cards' order (`cardOrder.ts`), not the flow
  // window's, so the two lists cannot disagree or reshuffle as records land.
  // Runs the daemon marks `not_reporting`: their bars say so (one owner, on the row).
  const notReportingIds = useMemo(
    () => new Set(runs.filter((r) => r.not_reporting === true).flatMap((r) => [r.id, ...(r.dispatch_id ? [r.dispatch_id] : [])])),
    [runs],
  );
  const laneUids = useMemo(() => {
    const place = new Map(baseCards.map((c, i) => [c.uid, i]));
    return [...uids].sort((a, b) => (place.get(a) ?? Infinity) - (place.get(b) ?? Infinity));
  }, [uids, baseCards]);
  const timeline = useMemo(
    () =>
      buildActivityTimeline(
        flowWindow.data,
        liveMachines,
        laneUids,
        presence,
        // The FIXED axis ceiling — never the playhead. See timeline.ts's own
        // doc + this component's `playhead` prop doc for why the two must
        // stay separate arguments once a replay can scrub.
        flowWindow.tMax,
        playheadT,
        windowMinutesNum,
        liveMode,
        tMin ?? 0,
        playheadT,
        fixedRange,
        specs,
        roster,
        policy,
        notReportingIds,
      ),
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `playheadT` is read through `liveEdgeClock` on purpose (#2928, above).
    [flowWindow.data, liveMachines, laneUids, notReportingIds, presence, flowWindow.tMax, windowMinutesNum, liveMode, tMin, liveEdgeClock, fixedRange?.[0], fixedRange?.[1], specs, roster, policy],
  );

  // (5.0 R3) Names, not a count: the hero's tooltip says WHICH machines its
  // total leaves out. Keyed by the joined names so the memoized hero keeps its
  // identity across renders that change nothing.
  const notStreamingKey = notStreamedNames(cards).join("\u0000");
  const notStreaming = useMemo(() => (notStreamingKey ? notStreamingKey.split("\u0000") : []), [notStreamingKey]);

  return (
    <div className="fleet-lens" data-state={flowWindow.settled ? "loaded" : "loading"}>
      <SavingsHero
        tokens={tokens}
        liveMode={liveMode}
        // (#2965) Its zeros are a negative claim off the same read: a failed
        // one keeps the loading silhouette rather than counting up to "0".
        settled={flowWindow.settled && flowWindow.failure === null}
        notStreaming={notStreaming}
      />
      <RunsUnreadableNotice unreadable={runsUnreadable} message={runsErrorMessage} />
      <RosterUnreadableNotice error={rosterError} />
      <div className="fleet" ref={fleetRef} data-order={orderState}>
        {cards.map((card) => {
          // (#2881) Pager selection: see `shownExecution`.
          const { selectedIdx, defaultSid } = shownExecution(card.executions, stickyDefaultByUidRef.current[card.uid], pinnedPageByUid[card.uid]);
          if (defaultSid != null) stickyDefaultByUidRef.current[card.uid] = defaultSid;
          else delete stickyDefaultByUidRef.current[card.uid];
          const selectedExec = selectedIdx >= 0 ? card.executions[selectedIdx] : null;
          const showsReading = card.liveTokRate !== null && selectedExec != null;
          // (#2958) What this card may say before every source has
          // answered: positive readings at once, negative claims later.
          const face = cardFace(card, showsReading, answered);
          return (
            <MachineCard
              key={card.uid}
              card={card}
              face={face}
              selectedIdx={selectedIdx}
              machineKey={encodeMachineKey(machineKeyCtx, card.uid)}
              clock={{ playhead: playhead ?? null, livePolling, playbackClock, playheadT }}
              onShowExecution={showExecution}
            />
          );
        })}
      </div>
      {uids.length ? (
        <div className="fleettl" ref={tlRef} style={{ "--lname-w": `${timeline.labelWidthPx}px` } as CSSProperties}>
          <div className="tlhdr">
            <span>{timeline.headerText}</span>
            {/* (Playback parity, Change A, finding #8) Shown in BOTH modes
                now — a replay draws the same rolling window as live,
                anchored at the playhead, so the window control is a live
                knob there too, not a dead one. Used to be LIVE-ONLY
                (`const winCtl=liveMode?...:''`), back when
                a replay drew the whole recorded day with nothing to slide
                over. */}
            <span className="twin">
              {recordingRange && (
                <button className={`twinb${windowMinutes === "all" ? " on" : ""}`} onClick={() => setWindowMinutes("all")}>
                  all
                </button>
              )}
              {ACTIVITY_WINDOW_PRESETS.map((p) => (
                <button
                  key={p.minutes}
                  className={`twinb${windowMinutes === p.minutes ? " on" : ""}`}
                  onClick={() => setWindowMinutes(p.minutes)}
                >
                  {p.label}
                </button>
              ))}
            </span>
          </div>
          <TimelineLanes timeline={timeline} />
        </div>
      ) : (
        <div className="fleettl">
          <div className="tlempty">waiting for the first flow record…</div>
        </div>
      )}
    </div>
  );
}
