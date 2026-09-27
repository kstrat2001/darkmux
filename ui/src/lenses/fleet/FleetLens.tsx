import { NO_PRESENCE } from "../../lib/lifecycle";
import { useLifecyclePolicy } from "../../hooks/useLifecyclePolicy";
import { encodeMachineKey } from "../../lib/machineKey";
import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import { fitTubes } from "./tubeFit";
import { scopeCenter } from "../../lib/scopeCenter";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "../../lib/fetcher";
import { queryKeys, PRESENCE_POLL_MS } from "../../lib/queryKeys";
import { useFlowWindow } from "../../hooks/useFlowWindow";
import { useNowMs } from "../../lib/clock";
import { REPLAY_GRANULARITY_NOTE, useLiveOverlay } from "../../lib/liveChannel";
import { useCountUp } from "../../hooks/useCountUp";
import { useFleetRoster, useLiveMachines, useStaticFleetBeats } from "../../hooks/useLiveMachines";
import { getSource, runsSrc, runsReachable } from "../../lib/source";
import { useLiveSessionIds } from "../../hooks/useLiveSessionIds";
import { machineUids, machPresent, machineNames, LIVE_WINDOW_MS } from "../../lib/flow";
import type { FleetMachinesLiveResponse, FleetSessionsLiveResponse, RunsResponse } from "../../types/handwritten";
import { fmtN, fmtC } from "../../lib/format";
import { MachineIcon } from "../../components/MachineIcon";
import { Shimmer } from "../../components/Placeholder";
import { TokenScope } from "../../components/TokenScope";
import { usePlaybackClock } from "../../lib/pageClockRate";
import { WALL_CLOCK } from "../../lib/restHand";
import { UtilityGlyph } from "../../components/UtilityGlyph";
import { scopeStateOf } from "../../lib/scopeMorph";
import { liveStateLabel, reasonForLine } from "../../lib/tokenRate";
import { tokensOffMeter } from "./savings";
import { buildFleetCardBase, withLiveReadings, busiestExecution, isStrictlyBusier, rosterOnlyEntries, rosterAliasFor, specUnknownLabel, cardFace, NO_SIGNAL_STAT, type CardSourcesAnswered } from "./cards";
import { useLatch } from "../../hooks/useLatch";
import { buildActivityTimeline, ACTIVITY_WINDOW_PRESETS, DEFAULT_ACTIVITY_WINDOW_MIN } from "./timeline";
import type { MachineSpecs } from "../../types/handwritten";
import { runsForMachine } from "../runs/format";
import { recordsAsOf, type NormRecord } from "../../lib/ingest";
import { dispatchHash } from "../../lib/route";

/** `ICON.machine` (viewer.html:935) — the generic processor/chip glyph
 * every fleet card renders, since `MACH_ICON` (the per-machine form-factor
 * lookup) is empty in the live viewer (see that source's own comment: "real
 * machines render with no icon until /machine/specs/<id> wiring lands"). No
 * text content — contributes nothing to the parity extractor's `innerText`,
 * same as legacy's inline SVG. */
/** Every machine card drills to the RUNS lens, pinned to that machine.
 *
 * One destination, because the alternative is only ever valid for ONE
 * machine. The residency room reads `/machine/resources`, and that probe
 * answers for THIS host only — a remote machine's residency is unreadable
 * from here by construction (`MachineLens.tsx` says so in its own doc, and
 * gates `resourcesQuery` off accordingly). So the machine lens can never be
 * a correct destination for a remote card, and making it a CONDITIONAL
 * destination is what made the navigation inconsistent: the same gesture on
 * two cards went to two different kinds of page, one of which could not
 * answer.
 *
 * "What is running here" is a question every card can answer, local or
 * remote, and it is the question the card is already asking on the
 * operator's behalf — it shows a running count and an activity timeline. So
 * the runs list pinned to that machine continues what they were reading,
 * identically for every machine.
 *
 * Collapsing the branch also removes a defect it had to work around:
 * `localUid` is null until `/machine/specs` resolves, so the destination
 * CHANGED under the operator between first paint and +100ms, and #1809 added
 * a guess-toward-the-humbler-destination rule purely to make that flicker
 * harmless. With one destination there is no guess and no wrong frame.
 *
 * The residency room is not orphaned — it keeps the MACHINE tab in the nav
 * chrome, which is a bare `lens=machine` meaning "this machine", the only
 * machine it can actually report on.
 *
 * Operator call, 2026-08-23: "a remote machine's stats can't be read so
 * clicking that card ... would be an unusable result. The machine tab is
 * this machine and that makes sense ... Always going to runs would make it
 * consistent nav regardless of machine." */
function machineDrillHash(machineKey: string): string {
  // (#2929) A machine KEY (`lib/machineKey.ts`), never the hardware uid: the
  // address bar lands in screenshots and shared links.
  return `lens=runs&machine=${encodeURIComponent(machineKey)}`;
}

/** (#1903) The running COUNT's own tap target — distinct from
 * `machineDrillHash` above (the card BODY's destination, unchanged by this
 * function or its caller). The count is the thing an operator is actually
 * reading on the card ("N running"), and it had no destination of its own:
 * a tap anywhere on the card, count included, fell through to
 * `machineDrillHash` and landed on the residency room, a machine drill the
 * operator did not ask for. See #1903's own issue text: "the running count
 * is the thing the operator is reading, and it has no affordance of its
 * own."
 *
 * `null` (render the plain, non-interactive count, same as before this
 * packet) when there's nothing running to open — `runningSessionIds` is
 * only ever non-empty in LIVE mode (see `FleetCard.runningSessionIds`'s own
 * doc), so this is naturally a no-op in replay, where the count means "the
 * day's whole session set" rather than "currently running" — a different
 * question, one the card body's own drill-in already answers honestly.
 *
 * Exactly one running session goes straight to that run's own session
 * drill (`#dispatch=<sid>`, same mechanism the activity-timeline bars below
 * already use) rather than the runs lens — the single-run case has one
 * obvious destination, and naming it directly saves a hop. Two or more
 * goes to the runs lens pinned to this machine (`lens=runs&machine=<uid>`,
 * the SAME hash `machineDrillHash` already constructs for a confirmed-
 * remote card body) — a list, not a single run, is the honest surface for
 * "several things running here". */
function machineRunsHash(machineKey: string, runningSessionIds: string[]): string | null {
  if (runningSessionIds.length === 0) return null;
  if (runningSessionIds.length === 1) return dispatchHash(runningSessionIds[0], null);
  return machineDrillHash(machineKey);
}

/** `sc()` — viewer.html:1633. One token-class chip (value over label).
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

/**
 * `savingsHero()` — viewer.html:1619-1666 (#783, #1186). Always renders,
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
}: {
  tokens: ReturnType<typeof tokensOffMeter>;
  liveMode: boolean;
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
          ${h}h`:''` (viewer.html:1660). A replay's numbers cover the recorded
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
            <div className="savlbl">all tokens{liveMode ? ` · last ${hours}h` : ""}</div>
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
 * The fleet default view — `renderFleet()` (viewer.html:1667-1741): the
 * savings hero, one card per machine, and the recent-activity timeline.
 * `/next`'s default (no-hash) route. See `savings.ts`/
 * `cards.ts`/`timeline.ts` for the ported pure logic this component
 * composes.
 *
 * Data sources: `/flow/<today>` + `/flow/<yesterday>` (the live window every
 * number here derives from — `useFlowWindow`), `/fleet/machines/live` +
 * `/fleet/sessions/live` (presence), `/machine/specs` (this machine's own
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
        <div className="lane" key={lane.uid}>
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
                even though the fetch + render it needs (`/flow-session/<id>`
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
 * below) is the actual bracketing value — `machPresent`, `buildFleetCard`'s
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
   * which skips the half-open check in `buildFleetCard` entirely and falls
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
   * produced 404s for `/fleet/machines/live`, `/fleet/sessions/live` and
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
  // (#2067) A static build cannot poll presence, so its cards' hardware line
  // comes from the committed fleet snapshot instead — spec lookup ONLY;
  // presence at the playhead still derives from the records.
  const staticBeats = useStaticFleetBeats();
  const specBeats = getSource().kind === "static" ? staticBeats : liveMachines;
  // `/machine/specs` is the THIRD live-only endpoint on this screen, and the
  // one that got away in the first pass. It describes the hardware of the
  // machine serving the page RIGHT NOW — `pollMachineSpecs` is the live-only
  // 5s poll, and legacy states outright that "playback mode never starts that
  // poll" (viewer.html:2696), leaving `MACHINE_SPECS` null so `specOf` returns
  // "" and the card reads "hardware not reported". Rendering today's CPU and
  // RAM against a replayed day is the same confidently-wrong claim as
  // rendering today's presence.
  //
  // It also made the parity test genuinely FLAKY rather than merely wrong:
  // whether the specs response landed before the assertion was a race, so a
  // local run passed and CI failed on the identical commit. Gating it removes
  // the race at its source — the request never happens — instead of waiting
  // harder for a value that should not be read.
  const specsQuery = useQuery({
    enabled: livePolling,
    queryKey: queryKeys.machineSpecs(),
    queryFn: () => fetchJson<MachineSpecs>("/machine/specs"),
  });
  const specs = livePolling && specsQuery.data?.ok ? specsQuery.data.data : null;

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
  // `buildFleetCard` only ever reads it in `liveMode` — see that
  // parameter's own doc.
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
  // gated: replay never reads `machineRuns` at all (`buildFleetCard` gates
  // the lab count on `liveMode`), so a failed `/runs` costs a replayed day
  // nothing, and warning about it there would be the bug.
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
    queryFn: () => fetchJson<FleetSessionsLiveResponse>("/fleet/sessions/live"),
  });
  // Each latched: the FIRST answer counts (`useLatch`), so the flow
  // window's new day key at UTC midnight does not blink every card back to
  // "no signal".
  const flowAnswered = useLatch(flowWindow.settled);
  const presenceAnswered = useLatch(!livePolling || presenceState.status !== "pending");
  const sessionsAnswered = useLatch(!livePolling || sessionsState.status !== "pending");
  const runsAnswered = useLatch(!(liveMode && runsReachable()) || runsQuery.status !== "pending");
  // `/machine/specs` is live-only (see `specsQuery`); a replay has no self
  // identity to wait for.
  const specsAnswered = useLatch(!livePolling || specsQuery.status !== "pending");
  // (#2965) A failed flow read settles the window with no records, which
  // is what a quiet window looks like: the flow source has not answered
  // while its read is failing, so the claims it backs hold "no signal".
  // `FlowReadNotice` (app-level) says why. The latch still covers the
  // midnight rollover: a new day's PENDING key is not a failure.
  const flowKnown = flowAnswered && flowWindow.failure === null;
  const answered = useMemo<CardSourcesAnswered>(
    () => ({ flow: flowKnown, presence: presenceAnswered, sessions: sessionsAnswered, runs: runsAnswered, specs: specsAnswered }),
    [flowKnown, presenceAnswered, sessionsAnswered, runsAnswered, specsAnswered],
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
  const tokens = useMemo(() => tokensOffMeter(scopedData), [scopedData]);

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
  const presence = playhead == null ? liveSessionIds : NO_PRESENCE;
  const policy = useLifecyclePolicy();
  // (#2814) SELF IS NEVER UNKNOWN — and before this, self could be ABSENT.
  //
  // `machineUids` unions flow-derived uids with currently-beating presence
  // keys. Both are empty on a fresh install, on a machine whose Redis is off
  // (presence self-disables — `darkmux-flow/src/presence.rs`), and on any
  // machine whose last record has aged out of the retained window. The
  // roster could not cover the gap either: `rosterOnlyEntries`' F1
  // self-check correctly suppresses this machine's own entry as
  // already-accounted-for, so in that state NOTHING accounted for it and the
  // daemon answering the request rendered no card about itself at all.
  //
  // The uid the daemon probes for itself is not an observation and does not
  // belong to the window, so it is appended unconditionally when specs
  // report one. Deduped against the derived set, so the ordinary case — this
  // machine has records, as it does whenever anything has run — is unchanged.
  // No replay concern: `specs` is null unless `livePolling`.
  const uids = useMemo(() => {
    const derived = machineUids(flowWindow.data, liveMachines);
    const selfUid = specs?.machine_uid;
    return selfUid && !derived.includes(selfUid) ? [...derived, selfUid] : derived;
  }, [flowWindow.data, liveMachines, specs]);
  // (#1855) The roster entries with NO known identity anywhere in this
  // window — not beating, no flow history under this name either, not this
  // machine's own `/machine/specs` identity, not a normalized near-miss of
  // any of those. See `rosterOnlyEntries`'s own doc (F1/F2 in its comment)
  // for why `specs` has to be threaded through here: it is the one
  // confirmed-local identity that survives a quiet flow window with no
  // beats, which is exactly the state a Redis-off self-machine card can be
  // rendered in.
  const rosterOnly = useMemo(
    () => rosterOnlyEntries(flowWindow.data, liveMachines, roster, specs),
    [flowWindow.data, liveMachines, roster, specs],
  );
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
  const baseCards = useMemo(
    () => [
      ...uids.map((m) => {
        const card = buildFleetCardBase(
          flowWindow.data,
          liveMachines,
          specs,
          presence,
          machPresent(flowWindow.data, liveMachines, playheadT, m) === false,
          m,
          liveMode,
          playheadT,
          specBeats,
          // (#1923) `machineNames`, not a bare uid match — `Run.machine`
          // carries only a display NAME (`runsForMachine`'s own doc), same
          // alias-set lookup `specOf`/`nameOf` already use for this uid.
          runsForMachine(runs, machineNames(flowWindow.data, liveMachines, m)),
          roster,
          policy,
        );
        // (#2768, corrected by the #2802 regression fix) A roster entry
        // whose declared hardware identity matches this uid still prevents a
        // SECOND card — that is `rosterOnlyEntries` below — but it no longer
        // overrides this card's TITLE. The machine's own name wins; the
        // operator's alias rides along as secondary text. See
        // `rosterAliasFor` for why.
        const rosterAlias = rosterAliasFor(m, roster, card.name);
        return rosterAlias ? { ...card, rosterAlias } : card;
      }),
      // (#1855) A rostered entry with no known identity is, by definition,
      // not currently beating — `machAbsent` is forced `true` rather than
      // derived through `machPresent` (which would answer `null`/"unknown"
      // for a uid it has never heard of, not `false`/"absent"). Forcing it
      // is what renders these on the SAME "offline" stat/CSS branch a
      // machine that WAS seen and has since gone quiet already uses — the
      // shared indicator this project's "no snowflakes" rule asks for,
      // rather than a new vocabulary for "silent". `entry.id` doubles as
      // the card's `uid`: a roster entry carries no hardware uid (it is
      // declared before the machine has ever proven one), and `id` is
      // already the identity `specOf` falls back to for an unknown `m`.
      // (#2921) Its TITLE is the roster id, set here: `nameOf` no longer
      // echoes an unknown `m` back (that echo was how a raw hardware uid
      // reached a card title), so the operator's declared name is passed
      // explicitly rather than inherited from the fallback.
      ...rosterOnly.map((entry) => ({
        ...buildFleetCardBase(
          flowWindow.data,
          liveMachines,
          specs,
          presence,
          /* machAbsent */ true,
          entry.id,
          liveMode,
          playheadT,
          specBeats,
          undefined,
          undefined,
          policy,
        ),
        name: entry.id,
        rosterOnly: true,
      })),
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `playheadT` is read through `liveEdgeClock` on purpose (#2928, above).
    [uids, rosterOnly, flowWindow.data, liveEdgeClock, liveMachines, specs, presence, liveMode, specBeats, runs, roster, policy],
  );
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

  const timeline = useMemo(
    () =>
      buildActivityTimeline(
        flowWindow.data,
        liveMachines,
        uids,
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
      ),
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `playheadT` is read through `liveEdgeClock` on purpose (#2928, above).
    [flowWindow.data, liveMachines, uids, presence, flowWindow.tMax, windowMinutesNum, liveMode, tMin, liveEdgeClock, fixedRange?.[0], fixedRange?.[1], specs, roster, policy],
  );

  return (
    <div className="fleet-lens" data-state={flowWindow.settled ? "loaded" : "loading"}>
      <SavingsHero
        tokens={tokens}
        liveMode={liveMode}
        // (#2965) Its zeros are a negative claim off the same read: a failed
        // one keeps the loading silhouette rather than counting up to "0".
        settled={flowWindow.settled && flowWindow.failure === null}
      />
      <RunsUnreadableNotice unreadable={runsUnreadable} message={runsErrorMessage} />
      <RosterUnreadableNotice error={rosterError} />
      <div className="fleet" ref={fleetRef}>
        {cards.map((card) => {
          // (#2881) Pager selection for this card. `execs` is already
          // sorted by session id (`cards.ts::buildFleetCard`'s own doc) —
          // that sort order IS the pager's page order, so page numbers stay
          // put tick to tick. The USER'S pick lives in `pinnedPageByUid`
          // (component state, above): it applies only while the picked
          // session is still among `execs`; the moment it isn't (the
          // execution ended), this falls straight through to the AUTO
          // default computed just below (`effectiveDefaultSid`) with no
          // separate cleanup step, and no live/playback branch — the same
          // fallback rule for a replayed instant too.
          const execs = card.executions;
          const pagerActive = execs.length >= 2;
          // (#2886 pass 5, MUST — fresh-reviewer finding F6) The AUTO
          // default is sticky against flapping: keep whatever was shown as
          // the default last render (`stickyDefaultByUidRef`) unless that
          // execution is gone (falls straight to the fresh busiest — same
          // "no separate cleanup step" shape the user's OWN pin already
          // uses below) or another execution is now STRICTLY busier by
          // state class. `card.defaultExecutionSessionId` (the from-scratch
          // busiest `cards.ts` computes) is deliberately NOT read directly
          // here any more — it's what flapped, since it has no memory of
          // what was on screen a moment ago.
          const stickyDefaultSid = stickyDefaultByUidRef.current[card.uid];
          const stickyDefaultExec = stickyDefaultSid != null ? execs.find((e) => e.sessionId === stickyDefaultSid) : undefined;
          const freshBusiest = busiestExecution(execs);
          const effectiveDefaultSid =
            stickyDefaultExec && freshBusiest
              ? isStrictlyBusier(freshBusiest, stickyDefaultExec)
                ? freshBusiest.sessionId
                : stickyDefaultExec.sessionId
              : (freshBusiest?.sessionId ?? null);
          if (effectiveDefaultSid != null) stickyDefaultByUidRef.current[card.uid] = effectiveDefaultSid;
          else delete stickyDefaultByUidRef.current[card.uid];
          const pinnedSid = pinnedPageByUid[card.uid];
          const selectedSid = pinnedSid != null && execs.some((e) => e.sessionId === pinnedSid) ? pinnedSid : effectiveDefaultSid;
          const selectedIdx = selectedSid != null ? execs.findIndex((e) => e.sessionId === selectedSid) : -1;
          const selectedExec = selectedIdx >= 0 ? execs[selectedIdx] : null;
          const showsReading = card.liveTokRate !== null && selectedExec != null;
          // (#2958) What this card may say before every source has
          // answered: positive readings at once, negative claims later. The
          // live readouts (tube, rate line, pager) draw on `face.tube`, so an
          // offline card shows its powered-off screen and nothing live.
          const face = cardFace(card, showsReading, answered);
          // (#2955 review) The page's execution has no live state: the
          // page lost the daemon (`liveStateWhileConnected`'s downgrade).
          // Its status line is the plain "no signal" every other card
          // shows, with the dim dot, not a lit reading; the tube shows
          // static and the card stays active (its machine IS running).
          const readingNoSignal = face.tube === "reading" && selectedExec != null && selectedExec.state === null;
          // `card.liveTokRate !== null` (the scope's mount gate below) only
          // ever holds when at least one execution is running, so
          // `selectedExec` is non-null everywhere it's read below — this is
          // the ONE per-execution reading that N=1 and N=2+ both render
          // from; there is no separate "aggregate" rendering path left for
          // N=1 to keep in sync with this one.
          const selectPage = (e: { stopPropagation: () => void }, dir: 1 | -1) => {
            e.stopPropagation();
            if (execs.length < 2 || selectedIdx < 0) return;
            const next = execs[(selectedIdx + dir + execs.length) % execs.length];
            setPinnedPageByUid((m) => ({ ...m, [card.uid]: next.sessionId }));
          };
          return (
          // `<div class="mach ..." data-act="machine" data-arg="${uid}">`
          // (viewer.html:1711) — the fleet-card drill-in: `ACTIONS.machine`
          // (viewer.html:2991) calls `drillMachine(uid)` for an explicit
          // arg. Ported as a real cross-lens navigation (a literal
          // `location.hash` write, firing `hashchange` so `useHashRoute`
          // actually swaps the rendered component — the SAME mechanism
          // `NavChrome`'s tab clicks use, see that component's own doc for
          // why replaceState alone can't do this), not a
          // `history.replaceState`. `data-act`/`data-arg` themselves carry
          // no behavior here (the click goes through the `onClick` below,
          // not a delegated listener reading these attrs) — they're
          // restored purely as the DOM inspection hook e2e specs drill
          // through (`viewer-lifecycle.spec.js`, `viewer-xss.spec.js`),
          // same contract legacy's markup gave them.
          //
          // Every card drills to the runs lens pinned to that machine —
          // see `machineDrillHash`'s own doc for why there is one
          // destination and not a locality split. The short version:
          // residency is local-probe-only by construction (#1286's
          // "observer must not join the observed" — `/machine/resources`
          // always describes THIS daemon's own host), so the residency room
          // can only ever answer for one machine, and a destination valid
          // for one machine made the same gesture mean two different things.
          <div
            key={card.uid}
            // (#2958) `face`, not the card's raw flags: "offline" waits on
            // the sources that could contradict it. `nosignal` gives the dot
            // the absent dot's no-reading gray, without dimming the card.
            className={`mach${face.active ? " active" : ""}${face.absent ? " absent" : ""}${face.noSignal || readingNoSignal ? " nosignal" : ""}`}
            data-act="machine"
            data-arg={encodeMachineKey(machineKeyCtx, card.uid)}
            role="button"
            tabIndex={0}
            // (#1903 QA fix) Explicit, so the card's computed accessible
            // name is DETERMINISTIC rather than folding in whatever the
            // nested running-count button's own `aria-label` happens to
            // say (per ARIA's presentational-children rule, a `button`
            // descendant's content — including its own name — isn't
            // exposed separately; without this, the outer card's name
            // absorbed the inner one's text, e.g. "MacBook-Pro Apple M5
            // Max dispatch in flight open the 2 running dispatches on
            // MacBook-Pro"). Nesting one interactive control inside
            // another is itself an accepted, documented exception here —
            // not an oversight — because the count needed its own tap
            // target (#1903) without moving or restructuring the card
            // body's own destination, which the issue is explicit must
            // stay unchanged. `stopPropagation` on the inner control (see
            // the running-count block below) keeps the two handlers from
            // double-firing; this `aria-label` is the remaining a11y-tree
            // cleanup that nesting still needs.
            aria-label={card.name}
            onClick={() => {
              location.hash = machineDrillHash(encodeMachineKey(machineKeyCtx, card.uid));
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                location.hash = machineDrillHash(encodeMachineKey(machineKeyCtx, card.uid));
              }
            }}
          >
            <div className="name">
              <span className="mico">
                <MachineIcon />
              </span>
              {/* (#2890) Its own box, so a long name ellipsizes beside the
                  tube (a flex row's bare text cannot); the full name is the
                  tooltip. */}
              <span className="mach-name" title={card.name}>
                {card.name}
              </span>
              {/* (#2915) The utility strip: a fixed box at the end of the
                  name row, always present, so a job starting or ending never
                  changes the card's layout. See `UtilityGlyph`. */}
              <UtilityGlyph strip={card.utility} noSignal={!face.utilityQuietKnown} />
            </div>
            {/* (#1855) The dim fallback says WHICH kind of unknown this is —
                a machine that beat and carried no hardware, vs one nothing
                has ever been received from (a rostered-but-silent peer, the
                cards this issue made visible in the first place). The
                wording lives in `cards.ts::specUnknownLabel` so the card and
                its tests read the same string. */}
            <div className="spec" title={card.spec || undefined}>
              {card.spec ? (
                card.spec
              ) : (
                <span className="specdim">{specUnknownLabel(card.specUnknown ?? "not-reported")}</span>
              )}
            </div>
            {/* (#2877) Status, rate and running count on the left; while the
                machine generates, the scope sits to their right at the
                concept's card size, spanning those rows, so the card does
                not grow taller and an idle card reserves no empty slot. */}
            {/* (#2890) Every ONLINE card carries the tube: running work
                drives it; a machine with nothing running shows it idle
                (breathing, like rest). A machine that is off shows none. */}
            {/* (#2958) Every card carries the tube's box, offline included
                (a powered-off screen), so no card changes size when its
                data arrives or its machine comes and goes. */}
            <div className="mach-body mach-body--scope">
              {/* (#2877) Live token-rate scope. Rendered ONLY when the card
                  computed a reading (`liveTokRate !== null` — live mode,
                  active, and at least one running session has produced two
                  heartbeats) — an idle machine mounts zero `TokenScope`
                  instances, never one sitting at 0, which is what makes "idle
                  machines keep plain text and never animate" true by
                  construction rather than by a prop the component has to
                  honor internally. */}
              {/* (#2877 pass 2, "is this resting? can't tell") No center
                  label exists on this card, so the rate line itself carries
                  the word: `N tok/s` while generating, else the same state
                  word the run page's tile shows (`liveStateLabel`, one
                  derivation, no mode branch). (#2881) Reads the PAGE's own
                  execution now (`selectedExec` — the sole one when there's
                  only one running), not a machine-wide aggregate: the tube,
                  its color and this word all belong to one run. */}
              {/* (#2955, operator 2026-09-27: the card is one height in
                  every state) The reading takes the STATUS line's place, dot
                  included, instead of a line of its own under it: with a
                  reading, "dispatch in flight" says less than "42 tok/s" or
                  "processing ~36k" does, and a second line grew the desktop
                  card 23px whenever a model ran. Same slot either way, so
                  one execution's text beside the tube is two rows, as an
                  idle card's is. (A second execution still adds the pager
                  row below; the layout spec's fixme records it.) */}
              {face.tube === "reading" && selectedExec && !readingNoSignal ? (
                <div
                  className="stat mach-scope__rate"
                  data-tone={selectedExec.state ?? "none"}
                  data-carried={selectedExec.carried ? "true" : "false"}
                  data-thinking={selectedExec.state === "generating" && selectedExec.thinking === true ? "true" : undefined}
                  title={
                    selectedExec.state === "prompt" && selectedExec.promptLabel
                      ? `estimated prompt size: ${selectedExec.promptLabel} tokens`
                      : // (#2950) The whole reason on hover, should a narrow
                        // card ellipsize it.
                        selectedExec.state === "rest" && selectedExec.restReason
                        ? `resting: ${selectedExec.restReason}`
                        : undefined
                  }
                >
                  <span className="dot" />
                  {selectedExec.state === "generating"
                    ? // (#2886 pass 5, MUST — fresh-reviewer finding F3) A GEN
                      // lamp with no reading yet (fewer than two same-turn
                      // heartbeats, or an untrusted opener pair) is "not yet
                      // measured", not "measured zero" — same "—" the run
                      // page's tile already shows for the identical case
                      // (`SessionReplay.tsx`'s `centerLabel`). `Math.round(...
                      // ?? 0)` used to print a confident "0 tok/s" here.
                      // (#2911) Thinking keeps its word while unmeasured:
                      // "— think tok/s", as the run page's lamp already
                      // says "think" for the same opening seconds.
                      selectedExec.tokensPerSec != null
                      ? `${fmtN(Math.round(selectedExec.tokensPerSec))} ${selectedExec.thinking ? "think tok/s" : "tok/s"}`
                      : // (#2955 review) The whole status line now, so the
                        // unit stays beside the "not yet measured" mark.
                        selectedExec.thinking
                        ? "— think tok/s"
                        : "— tok/s"
                    : // (#2886 pass 3) `state: null` (the disconnection
                      // downgrade) renders the plain status line below
                      // (`readingNoSignal`), so this arm only narrows the
                      // type for `liveStateLabel`.
                      selectedExec.state === null
                      ? NO_SIGNAL_STAT
                      : // (#2890, operator) The prompt's estimated size lives
                        // here, not in the tube (whose center is the brain for
                        // all of PROMPT). "processing ~36k", not "processing
                        // prompt · ~36k": measured, the long form ellipsized
                        // the size away on a phone and a 1000px desktop.
                        selectedExec.state === "prompt" && selectedExec.promptLabel
                        ? `processing ${selectedExec.promptLabel}`
                        : // (#2950) Why it rests, from the rest's own record
                          // ("thermal · serious"), in place of "rest Ns": the
                          // tube's center already counts the seconds down,
                          // and the card's line has room for one of the two.
                          // (Operator, 2026-09-27) At phone width the state
                          // is dropped ("thermal"): both are in the DOM and
                          // CSS shows one by the viewport's width, so the
                          // line keeps its one-line height either way. The
                          // hover title keeps the state.
                          selectedExec.state === "rest" && selectedExec.restReason
                          ? (
                              <>
                                <span className="mach-scope__why mach-scope__why--full">{reasonForLine(selectedExec.restReason)}</span>
                                <span className="mach-scope__why mach-scope__why--word">{selectedExec.restReasonWord ?? selectedExec.restReason}</span>
                              </>
                            )
                          : liveStateLabel({
                            state: selectedExec.state,
                            restSecondsLeft: selectedExec.restSecondsLeft,
                            // (#2926) "tool gen · write · 18s": the tool
                            // being written, on this line, never in the tube.
                            toolName: selectedExec.toolName,
                            writing: selectedExec.writing,
                            writingSeconds: selectedExec.writingSeconds,
                            // (#2915) "compacting · 12s" on this line.
                            compacting: selectedExec.compacting,
                            compactingSeconds: selectedExec.compactingSeconds,
                          })}
                </div>
              ) : (
                <div className="stat">
                  <span className="dot" />
                  {/* (#2958) "idle" before its sources answer is a default,
                      not a reading: see `cardFace`. */}
                  {readingNoSignal ? NO_SIGNAL_STAT : face.stat}
                </div>
              )}
              {/* (#2881) The pager: shown only with 2+ running executions —
                  "no pager with one execution" is `pagerActive`'s own
                  `execs.length >= 2` gate. The arrows are their own tap
                  targets, matching the running-count control directly below
                  (`.runs--live`, #1903) — same nested-interactive-control
                  shape, same reason: a click here must not ALSO fire the
                  card body's `machineDrillHash` handler underneath it. */}
              {pagerActive && face.tube === "reading" && selectedExec && (
                <div className="mach-scope__pager" data-testid="fleet-pager">
                  <div
                    className="mach-scope__pager-btn"
                    role="button"
                    tabIndex={0}
                    aria-label="previous execution"
                    onClick={(e) => selectPage(e, -1)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        selectPage(e, -1);
                      }
                    }}
                  >
                    ‹
                  </div>
                  <span className="mach-scope__pager-n">
                    {selectedIdx + 1}/{execs.length}
                  </span>
                  {/* (#2881) Always rendered, even empty, so the right arrow's
                      column never moves between pages; one line, ellipsized,
                      with the full label in the tooltip. */}
                  <span className="mach-scope__pager-role" title={selectedExec.role ?? undefined}>
                    {selectedExec.role ?? ""}
                  </span>
                  <div
                    className="mach-scope__pager-btn"
                    role="button"
                    tabIndex={0}
                    aria-label="next execution"
                    onClick={(e) => selectPage(e, 1)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        selectPage(e, 1);
                      }
                    }}
                  >
                    ›
                  </div>
                </div>
              )}
              {/* (#1903) The running count's own tap target — a SIBLING
                  affordance to the card body's `machineDrillHash` click
                  above, not a replacement for it. `runsHash` is `null`
                  (falls through to the old plain, non-interactive count)
                  whenever there's nothing running to open — see
                  `machineRunsHash`'s own doc. `stopPropagation` on both
                  handlers keeps a click/Enter on the count from ALSO firing
                  the card body's own handler underneath it (this is a
                  nested interactive control by necessity — the issue is
                  explicit that the card body's destination must stay
                  unchanged, which rules out restructuring the card to avoid
                  the nesting). `.runs--live`'s own CSS is what makes it LOOK
                  interactive, matching #1900's lesson in the other
                  direction: a clickable-but-inert-looking control is as
                  dishonest as an inert-looking one that's secretly a broken
                  link.
                  (#2881) With the pager active, the machine TOTAL moves
                  here ("3 running · 180 tok/s") — there is no separate
                  "all" page; `card.liveTokRate` is still the machine-wide
                  aggregate this line always showed before, just no longer
                  the rate line's own number once there's more than one
                  execution to attribute it to.
                  (#2886 pass 5, MUST — fresh-reviewer finding F2) `runsCount`
                  and `execs.length` (`card.executions`) are DIFFERENT counts
                  for a mission/crawl: `runsCount` is post-collapse
                  (`topLevelRuns` folds every seat sharing one
                  `mission_id` into its ONE top-level run — a mission with 9
                  crawler seats reads "1 running"), while `execs` is the
                  per-execution pager data, uncollapsed on purpose (each seat
                  IS its own page). Showing "1 running · 200 tok/s" under a
                  "‹ 5/9 crawler ›" pager reads as a bug (nine pages under
                  one run?), so when the two counts disagree the line names
                  BOTH: "1 run · 9 executions · 200 tok/s". They agree for a
                  standalone card's several plain dispatches (no mission to
                  collapse), which is the common case — that keeps the
                  original "N running · X tok/s" wording unchanged. */}
              {(() => {
                // (#2958) "0 running" before every source has answered is a
                // default, not a count: the app's "not yet measured" mark,
                // in the same one-line slot. One or more is a reading.
                if (!face.countShown) return <div className="runs">—</div>;
                const runsHash = machineRunsHash(encodeMachineKey(machineKeyCtx, card.uid), card.runningSessionIds);
                const rateText = `${fmtN(Math.round(card.liveTokRate ?? 0))} tok/s`;
                const countText = pagerActive
                  ? card.runsCount === execs.length
                    ? `${card.runsCount} ${card.runsLabel} · ${rateText}`
                    : `${card.runsCount} ${card.runsCount === 1 ? "run" : "runs"} · ${execs.length} ${execs.length === 1 ? "execution" : "executions"} · ${rateText}`
                  : `${card.runsCount} ${card.runsLabel}`;
                if (!runsHash) {
                  return <div className="runs">{countText}</div>;
                }
                const activate = (e: { stopPropagation: () => void }) => {
                  e.stopPropagation();
                  location.hash = runsHash;
                };
                return (
                  <div
                    className="runs runs--live"
                    role="button"
                    tabIndex={0}
                    aria-label={`open the ${card.runsCount} running ${card.runsCount === 1 ? "dispatch" : "dispatches"} on ${card.name}`}
                    onClick={activate}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        activate(e);
                      }
                    }}
                  >
                    {countText}
                  </div>
                );
              })()}
              {face.tube === "reading" && selectedExec && (
                <div
                  className="mach-scope"
                  data-testid="fleet-token-scope"
                  // (#2928) Replay has only the 2 s heartbeats; said on hover.
                  title={playhead != null || !livePolling ? REPLAY_GRANULARITY_NOTE : undefined}
                >
                  <TokenScope
                    // Same rule as the run page's tile — a stale rate from
                    // the last generating stretch must not still drive the
                    // wave once the state has moved on (only `stalled` used
                    // to zero this). (#2881) The PAGE's own execution, not
                    // the machine aggregate. (#2886 pass 5, finding F3) The
                    // RAW nullable reading, not `?? 0` — matches
                    // `SessionReplay.tsx`'s identical prop exactly (a `null`
                    // reading is "not yet measured", never coerced into a
                    // confident zero before it reaches the component).
                    tokensPerSec={selectedExec.state === "generating" ? selectedExec.tokensPerSec : 0}
                    // (#2890) The same morphing states as the run page's hero,
                    // sized for the card. `state: null` here is the per-
                    // execution disconnection downgrade (a running machine is
                    // guaranteed by the gate above), the same "no signal" the
                    // rate line prints, so the tube shows static.
                    state={scopeStateOf({ state: selectedExec.state, noSignal: selectedExec.state === null })}
                    toolName={selectedExec.toolName}
                    // (#2889) The writing cue; the status line under the
                    // tube carries the live "tool gen · <tool> · Ns" (#2926).
                    toolWriting={selectedExec.writing === true}
                    // (#2890) Thinking tints the ring and shimmers the rate;
                    // the words and number stay as they are.
                    thinking={selectedExec.state === "generating" && selectedExec.thinking === true}
                    // (#2890, operator 2026-09-25) With the tube now sized to
                    // the card, the live rate sits in its center while
                    // generating, as on the run page. Other states keep the
                    // center's own content (tool icon, brain) or none.
                    // (#2890) The same center as every scope in the app
                    // (`lib/scopeCenter.ts`): rate over "tok/s", the REST
                    // countdown, "tool gen" (no seconds), the prompt size.
                    {...scopeCenter({
                      state: scopeStateOf({ state: selectedExec.state, noSignal: selectedExec.state === null }),
                      tokensPerSec: selectedExec.tokensPerSec,
                      carried: selectedExec.carried,
                      restSecondsLeft: selectedExec.restSecondsLeft,
                      writing: selectedExec.writing === true,
                      writingSeconds: selectedExec.writingSeconds,
                      thinking: selectedExec.thinking === true,
                      compacting: selectedExec.compacting === true,
                    })}
                    // (#2961) REST's seconds hand follows the page clock:
                    // the playhead in playback (at the transport's speed
                    // while it plays, still otherwise), the wall clock live.
                    restEndMs={selectedExec.state === "rest" ? selectedExec.restEndMs : undefined}
                    clock={playhead != null ? (playbackClock ?? { kind: "frozen", tMs: playheadT }) : WALL_CLOCK}
                    size="card"
                  />
                </div>
              )}
              {face.tube === "nosignal" && (
                <div className="mach-scope" data-testid="fleet-token-scope">
                  {/* (#2958) Nothing read yet: the same static a running
                      execution's tube shows once the page loses the daemon,
                      in the idle tube's slot and size. */}
                  <TokenScope tokensPerSec={0} state="nosignal" size="card" {...scopeCenter({ state: "nosignal", tokensPerSec: 0 })} />
                </div>
              )}
              {face.tube === "off" && (
                <div className="mach-scope" data-testid="fleet-token-scope">
                  {/* (#2958) An offline machine keeps the tube's box, its
                      screen powered off: the scope's own dark screen with no
                      trace drawn, no static, no center. Plain markup, not a
                      `TokenScope`, so an offline card runs no canvas at all. */}
                  <div className="token-scope-bezel token-scope-bezel--card" data-state="off" aria-hidden="true">
                    <div className="token-scope-screen" />
                  </div>
                </div>
              )}
              {face.tube === "idle" && (
                <div className="mach-scope" data-testid="fleet-token-scope">
                  {/* (#2911) A card whose stat reads "dispatch in flight"
                      (a mission between model steps, a lab run with no
                      execution) says "no model working" in the tube, not
                      "idle": the two words contradicted each other on one
                      card. The run page uses the same phrase. */}
                  <TokenScope tokensPerSec={0} state="idle" size="card" {...scopeCenter({ state: "idle", tokensPerSec: 0, inFlight: card.active })} />
                </div>
              )}
            </div>
          </div>
          );
        })}
      </div>
      {uids.length ? (
        <div className="fleettl" style={{ "--lname-w": `${timeline.labelWidthPx}px` } as CSSProperties}>
          <div className="tlhdr">
            <span>{timeline.headerText}</span>
            {/* (Playback parity, Change A, finding #8) Shown in BOTH modes
                now — a replay draws the same rolling window as live,
                anchored at the playhead, so the window control is a live
                knob there too, not a dead one. Used to be LIVE-ONLY
                (`const winCtl=liveMode?...:''`, viewer.html:1764), back when
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
