/**
 * The fleet default view's machine-card row — `renderFleet()`'s `cards`
 * build (viewer.html:1675-1687), plus the two helpers it leans on:
 * `machActive()` (viewer.html:1315-1322) and `specOf()`
 * (viewer.html:1120-1125).
 *
 * (Playback parity, Change A, 2026-09-24) `liveMode` used to select which of
 * TWO implementations ran: live counted sessions against a live set that
 * describes NOW; replay counted ALL of the day's sessions and labeled them
 * "specialist(s)" regardless of whether anything was actually running at
 * the playhead — `goldens/playback-date.txt` used to read "48 specialists"
 * at 5% into the day with zero sessions started, where `goldens/fleet.txt`
 * read "0 running" for the live arm of the identical instant (findings #3,
 * #4 in the parity audit). `runsCount`/`runsLabel`/`runningSessionIds`/
 * `liveTokRate`/`liveTokStalled` are now ONE derivation, run over records
 * up to `t` through `runningRuns()` (the lifecycle of each run, with
 * presence as an ADDITIVE input, empty in every real replay call) in BOTH
 * modes: "N running" at the instant the playhead sits on, live or
 * replayed. `liveMode` is kept as a parameter for now (dozens of existing
 * call sites), but nothing in this file's returned `FleetCard` fields reads
 * it any more — it decides nothing here. `machActive` needs the playhead
 * honored for its own "hasn't started yet" guard; see its own doc.
 */

import {
  aggregateLiveState,
  aggregateTokenRate,
  executionTokenReading,
  lastHeartbeatMs,
  liveExecutions,
  liveStatePriority,
  liveStateWhileConnected,
} from "../../lib/tokenRate";
import type { ExecutionTokenReading, LiveState } from "../../lib/tokenRate";
import type { MachineSpecsResponse } from "../../types/generated/MachineSpecsResponse";
import { grantWords, specsLine, type Grant, type RowFacts, type Standing } from "./viewRows";
import type { PresenceBeat } from "../../types/generated/PresenceBeat";
// (#2814) `isSelfMachine`/`displayNameOf` live in `lib/flow.ts` beside
// `nameOf`/`machineNames`/`localMachineUid` rather than here, because the
// machine lens and the app shell need the identical self-identity rule and a
// second copy of it is how the two surfaces disagree about which machine
// they are on.
import { displayNameOf, isSelfMachine, uidOf } from "../../lib/machineIdentity";
import type { RosterName } from "../../lib/machineIdentity";
import type { Run } from "../../types/generated/Run";
import { utilityStrip, type UtilityStrip } from "../../lib/utilityJobs";
import { mergeLive, type LiveOverlay } from "../../lib/liveChannel";
import { recordsAsOf, type NormRecord } from "../../lib/ingest";
import { DEFAULT_POLICY, isRunning, lifecycleAt, type LifecyclePolicy, type Presence } from "../../lib/lifecycle";
import { currentRun, runIndex, type RunGroup } from "../../lib/runRef";
import { machineAvailability, type MachineAvailability } from "../../lib/machineAvailability";

/** A machine's runs in flight as of `t`: its run- and execution-grain runs
 *  (`runRef.ts`) whose lifecycle (`lifecycle.ts`) is open or waiting. One
 *  entry per `(session, mission)` run, so two missions sharing a session id
 *  are two runs, and one mission's end never closes the other's (#2125).
 *  `presence` only adds: it holds a silent run open, never a closed one. */
function runningRuns(data: NormRecord[], presence: Presence, m: string, t: number, policy: LifecyclePolicy = DEFAULT_POLICY): RunGroup[] {
  return runIndex(data)
    .groupsOn(m)
    .filter((g) => g.grain !== "lifecycle" && isRunning(lifecycleAt(currentRun(g, t), t, policy, presence)));
}

/** `machActive()` — viewer.html:1342-1349. A machine is "in flight" iff one
 * of its runs is running as of `t` (`runningRuns`). */
export function machActive(data: NormRecord[], presence: Presence, m: string, t: number, policy: LifecyclePolicy = DEFAULT_POLICY): boolean {
  return runningRuns(data, presence, m, t, policy).length > 0;
}

/** `specOf()` — viewer.html:1120-1125. Returns a RAW string (JSX escapes at
 * render time, same "escape at the template edge" discipline the legacy
 * comment names). `MACH_SPEC` (a static hardcoded lookup) is empty in the
 * live viewer — dropped here entirely, matching that source comment. */
export function specOf(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: MachineSpecsResponse | null,
  m: string,
): string {
  if (m === "unknown") {
    const ns = [...new Set(data.filter((r) => uidOf(r) === "unknown" && r.machine_id).map((r) => r.machine_id as string))];
    return ns.length ? `unverified · claimed: ${ns.join(", ")}` : "unidentified (no hardware uid)";
  }
  // (#1008) THIS machine: prefer the live `/machine/specs` probe (cpu + RAM)
  // over a static lookup. Remote machines fall through to their presence
  // beat's specs.
  // "Is this uid the machine `/machine/specs` describes?" — asked against
  // EVERY alias the uid has used, not just the one `nameOf` returns. A
  // machine logging as both `MacBook-Pro` and `MacBook-Pro.local` under one
  // uid would otherwise show "hardware not reported" for its own hardware,
  // because `nameOf` answers with whichever alias it finds first while specs
  // reports the current one. Same identity rule as
  // `lib/flow.ts::localMachineUid`; see `machineNames` for why one machine
  // accumulates several names.
  // (#2814) The identity half of this condition moved into `isSelfMachine`,
  // which joins on `specs.machine_uid` when the daemon reports one — no
  // `data`, no `liveMachines`, no window. See that function's own doc for
  // why the alias set could not answer this question durably. The
  // `cpu_brand` half stays here: it is about whether there is anything to
  // SHOW, not about who this is.
  if (isSelfMachine(data, liveMachines, specs, m) && specs?.cpu_brand) return specsLine(specs);
  const beat = liveMachines.get(m);
  return beat?.specs || "";
}

/** The dim line shown in place of the hardware: the view's typed status for
 * a machine it could not read, else why no hardware is known. */
export function specDimLabel(card: { note: string | null; specUnknown: SpecUnknownReason | null }): string {
  return card.note ?? specUnknownLabel(card.specUnknown ?? "not-reported");
}

/** The card's subtitle: the hardware line, then what the peer lets this
 * machine do. One line, so the card keeps its height. */
export function specLine(card: { spec: string; grant: Grant | null }): string {
  return card.grant ? `${card.spec} · ${grantWords(card.grant)}` : card.spec;
}

/** (#2060) Collapse a machine's running runs down to TOP-LEVEL runs: a
 * mission's own run session and its seat/step executions are one mission,
 * not one-run-per-seat.
 *
 * A mission's own session is its `run` grain group (opened by `run.start`,
 * `src/mission_launch.rs::run_bookend_record`); a seat/step execution the
 * mission launches carries the SAME `mission_id` on its own session. So:
 * one run per mission, preferring the run session as its representative (so
 * a single-running-item drill-in lands on the mission, not on whichever seat
 * happened to be seen first). A run with no mission (a standalone dispatch,
 * a lab run) always counts on its own. */
function topLevelRuns(runs: readonly RunGroup[]): RunGroup[] {
  const standalone: RunGroup[] = [];
  const repForMission = new Map<string, RunGroup>();
  for (const g of runs) {
    if (!g.missionId) {
      standalone.push(g);
      continue;
    }
    if (!repForMission.has(g.missionId) || g.grain === "run") repForMission.set(g.missionId, g);
  }
  return [...standalone, ...repForMission.values()];
}

/** (#1923) How many of this machine's `/runs` rows are lab runs that are
 * running right now.
 *
 * ## What flow presence DOES see (correcting this comment's own first draft)
 *
 * A lab run's DISPATCH phase rides the flow stream like any other dispatch.
 * The providers (`crates/darkmux-lab/src/providers/{prompt,coding_task,
 * tool_bench}.rs`) call `darkmux_crew::dispatch::dispatch`, whose internal
 * path emits the contract-2 liveness bookends through
 * `DispatchBookendGuard` and then spawns the
 * `darkmux:session-presence:<sid>` emitter — which is exactly what
 * `useLiveSessionIds` → the card's `presence` reads. `lib/flow.ts`'s bookend-matcher
 * doc names `darkmux-lab` as one of the two producer lineages, and
 * `crates/darkmux-lab/src/lab/lifecycle.rs`'s module doc says the same
 * thing from the producer side: the lab lifecycle record is "the missing
 * half of contract 2 (dispatch liveness) applied to the lab path."
 *
 * The first version of this comment claimed the opposite — that CLAUDE.md's
 * contract 3 (the lab/fleet sink boundary) kept lab work off the flow
 * stream entirely. It does not. Contract 3 governs where a lab run's
 * ARTIFACTS are written (per-run-local, never the fleet stream); it grants
 * no exemption from contract 2. Built on that false premise, the count
 * SUMMED the two sources and reported a single live lab run as two.
 *
 * ## The gap that is real, and all this exists to close
 *
 * A lab run's NON-dispatch phases: the COW sandbox clone, the baseline
 * hash, the verify command, scoring. On a long-agentic run those are
 * minutes with no dispatch in flight — no bookends, no presence key, so
 * flow sees nothing and the card reads "idle" while a run is live. Same for
 * the Redis-off case, where presence cannot be read at all. The lab
 * `lifecycle.json` row is the only source that stays "running" across the
 * whole span (written at start, RAII-guarded — see that module's doc).
 *
 * This does NOT cross the sink boundary: it changes what the card reads for
 * DISPLAY, never what gets WRITTEN. `machineRuns` comes from `GET /runs`,
 * which already unions lab + flow sources server-side
 * (`crates/darkmux-serve/src/runs.rs::build_runs`) — reading that union here
 * is a display-layer join, not a new writer into the flow stream.
 *
 * Counts `kind === "lab"` rows only. A running mission/dispatch row in
 * `/runs` is deliberately NOT counted here — that activity is already
 * accounted for by the flow runs (via `topLevelRuns` above,
 * post-#2060), and counting it again here would double-count it. */
function runningLabRunCount(machineRuns: Run[]): number {
  return machineRuns.filter((r) => r.kind === "lab" && r.status === "running").length;
}

/** (#1855) WHY a card has no hardware line. `""` from `specOf` is not one
 * fact, and the single legacy string said it was:
 *
 * - `not-reported` — a beat (or a static build's committed snapshot entry)
 *   EXISTS for this machine and carries no `specs`. The machine answered and
 *   told us nothing about its hardware, which is what every peer running a
 *   build older than #2083 does: that fix stopped the emitter hardcoding
 *   `specs: None`, so a peer only starts sending hardware once it upgrades
 *   past it. "hardware not reported" is the honest sentence for that, and it
 *   is unchanged.
 * - `not-seen` — there is NO beat and no snapshot entry at all. Nothing was
 *   ever received from this machine, so there is nothing that could have
 *   reported anything. This is the state #1855's roster cards land in: a
 *   machine the operator declared with `darkmux machine add` that is down or
 *   has never started its daemon now renders (it used to vanish entirely),
 *   and the old string made its card assert that the machine had answered
 *   and withheld its hardware.
 *
 * The distinction is the same one the fleet-coverage notice draws one level
 * up, and the same one `specOf`'s `unknown`-uid branch already draws for
 * identity: "we looked and it said nothing" is not "we could not look". */
export type SpecUnknownReason = "not-reported" | "not-seen";

/** The one sentence per reason, so the card and its tests cannot drift
 * apart. `not-reported` is verbatim the legacy string (viewer.html's
 * `specdim` fallback). */
export function specUnknownLabel(reason: SpecUnknownReason): string {
  return reason === "not-seen" ? "hardware unknown — nothing received" : "hardware not reported";
}

/** (#2881) The pager's default page when the operator hasn't picked one:
 *  the busiest running execution — generating first, then the lamps' own
 *  priority (`liveStatePriority`, the same ranking `aggregateLiveState`
 *  already uses to pick the card's single aggregate state). A tie within a
 *  priority band goes to the higher current rate (meaningful only among
 *  generating executions, where a real tie is otherwise plausible — two
 *  coders that both started producing at once), and a final tie goes to the
 *  lower session id so the pick is deterministic rather than depending on
 *  array order. `null` for an empty list. */
export function busiestExecution(executions: ExecutionTokenReading[]): ExecutionTokenReading | null {
  let best: ExecutionTokenReading | null = null;
  for (const e of executions) {
    if (!best) {
      best = e;
      continue;
    }
    const bestPriority = liveStatePriority(best.state);
    const ePriority = liveStatePriority(e.state);
    if (ePriority !== bestPriority) {
      if (ePriority < bestPriority) best = e;
      continue;
    }
    const bestRate = best.tokensPerSec ?? -1;
    const eRate = e.tokensPerSec ?? -1;
    if (eRate !== bestRate) {
      if (eRate > bestRate) best = e;
      continue;
    }
    if (e.sessionId < best.sessionId) best = e;
  }
  return best;
}

/** (#2886 pass 5, MUST — fresh-reviewer finding F6) Whether `candidate`
 *  should REPLACE `current` as the pager's STICKY default page. Only when
 *  `candidate` is STRICTLY busier by STATE CLASS (the same
 *  `liveStatePriority` ranking `busiestExecution` itself picks from — a
 *  candidate that only ties `current`'s priority is not strictly busier,
 *  by definition). Deliberately narrower than `busiestExecution`'s own
 *  tie-break chain: that function picks a reasonable FIRST default from
 *  nothing; this one guards against replacing an ALREADY-DISPLAYED page,
 *  where a rate-based or session-id tie-break is exactly what flapped a
 *  real fleet's default page 46 times in 863s — two generating executions
 *  trading which one currently reads the higher tok/s is not a reason to
 *  switch what the operator is looking at. `FleetLens.tsx` calls this every
 *  render with the currently-displayed execution as `current`, and only
 *  calls `busiestExecution` fresh when `current` itself is gone (its own
 *  execution ended) — see that component's own doc. */
export function isStrictlyBusier(candidate: ExecutionTokenReading, current: ExecutionTokenReading): boolean {
  return liveStatePriority(candidate.state) < liveStatePriority(current.state);
}

export interface FleetCard {
  uid: string;
  name: string;
  /** "" means the `specdim` fallback — `specUnknown` below says which one. */
  spec: string;
  /** (#1855) `null` iff `spec` is non-empty. See `SpecUnknownReason`. */
  specUnknown: SpecUnknownReason | null;
  /** The view's typed status line for a machine whose card it could not
   *  read ("listener off"); shown in place of the hardware line. `null` for
   *  a machine the view read, and for one the view does not hold. */
  note: string | null;
  /** What this peer lets this machine do (`viewRows.Grant`); `null` for
   *  this machine's own card and for any peer without a grant to show. */
  grant: Grant | null;
  /** The machine's own card declares `fleet.mode hub`. */
  hub: boolean;
  /** Whether the machine is up: the view's own `liveness` (a card it read
   *  is proof of life), or, for a machine the view does not hold, the flow
   *  window's online/offline edges. */
  standing: Standing;
  /** (5.0 R3, #3012) Whether this viewer can see this machine's activity at
   *  all. Anything but `known` means a quiet card proves nothing. */
  availability: MachineAvailability;
  active: boolean;
  absent: boolean;
  stat: string;
  runsCount: number;
  /** Always `"running"` (Playback parity, Change A) — the same word at the
   * same instant, live or replayed. Used to be `liveMode?'running':
   * 'specialist'+(runs===1?'':'s')` (viewer.html:1713); see this file's own
   * module doc for why that branch was a parity defect, not a feature. */
  runsLabel: string;
  /** (#1903) The machine's currently-running FLOW sessions, collapsed to
   * top-level runs — as of `t`, in BOTH modes now (Playback parity, Change
   * A): `runningRuns()` is one algorithm over records up to `t`, with
   * presence as an optional additive input that a replay caller simply
   * never has. This used to be live-mode-only,
   * always empty in replay — see `runsCount`'s own comment for the defect
   * that produced.
   *
   * (#1923 review) This is the flow HALF of `runsCount`, no longer
   * necessarily its whole basis: `runsCount` merges this with the lab-row
   * count via `Math.max`, so a machine whose only activity is a lab run
   * between dispatches reads `runsCount: 1` with this list empty. The tap
   * target below degrades correctly on its own in that case — an empty
   * list is not "exactly one", so the card drills to the runs lens pinned
   * to the machine, which is where a lab run is listed anyway.
   *
   * Lets `FleetLens.tsx` build the running-count's own tap
   * target — the session drill directly when there's exactly one, the runs
   * lens pinned to this machine otherwise — without re-deriving the
   * running session set from raw flow data a second time. */
  runningSessionIds: string[];
  /** (#2877) This machine's total tok/s across whatever is running as of
   * `t` — `null` when there is nothing running (idle/absent) OR when
   * running sessions exist but none has produced two heartbeats yet to
   * derive a rate from. (Playback parity, Change A) Computed the same way
   * in replay now too — a replayed instant where something was genuinely
   * generating shows the same rate a live viewer saw at that instant; see
   * `buildFleetCard`'s own note. The card must render plain "idle" text and
   * never mount a scope when this is `null` — an idle machine has zero
   * `TokenScope` instances, not one sitting at 0. */
  liveTokRate: number | null;
  /** (#2886 pass 5, MUST — fresh-reviewer finding F4) `liveTokStalled`,
   *  `liveTokState`, `liveTokRestSecondsLeft`, `liveTokCarried` below, and
   *  `defaultExecutionSessionId` further down, are NOT read by
   *  `FleetLens.tsx` any more — the pager reads the equivalent per-PAGE data
   *  off `executions` instead (one entry per execution) once #2881 landed,
   *  including for a single running execution (verified:
   *  `card.executions[0]` matches these aggregate fields exactly in that
   *  case, so no separate rendering path was ever needed for it).
   *
   *  Kept anyway, deliberately, rather than deleted:
   *  1. They are still a genuine part of `buildFleetCard`'s PURE snapshot —
   *     the machine-wide stall/state/carry answer, independent of which
   *     execution a pager happens to be showing, which is a reasonable
   *     thing for a card snapshot to expose even to a consumer that never
   *     renders a pager (a future export, a different summary view).
   *  2. The render-level gap the finding actually named — the per-page
   *     half-open evidence and per-page `lastHeartbeatMs` were asserted
   *     only on `card.executions[i]` fields, never on what reaches the
   *     screen — is closed by NEW tests in `FleetLens.test.tsx` that pin
   *     the rendered rate-line text and the mocked `TokenScope`'s own
   *     props, not by these fields regaining a consumer.
   *  3. Deleting five fields with a decade of pre-existing #2877/#2885/
   *     #2886 unit coverage (stall detection, the half-open race, carried
   *     detection) to chase a render-path gap that's already closed here
   *     would be churn for its own sake, not a fix. */
  /** (#2877) No fresh heartbeat from anything running on this machine —
   * the scope should decay to its flat-ring stall state. `liveTokRate` is
   * already forced to `0` in this case (see `buildFleetCard`), so this is
   * purely the VISUAL flag; the number is already honest either way.
   * (#2877 pass 2) Now DERIVED from `liveTokState` (`=== "stalled"`) — one
   * rule, not two that could disagree. */
  liveTokStalled: boolean;
  /** (#2877 pass 2, "is this resting? can't tell") The same legible
   * between-heartbeats state `sessionRun.ts`'s `liveTokScope` derives, here
   * aggregated across every running session on this machine
   * (`aggregateLiveState`) — see `lib/tokenRate.ts`'s own doc. `null`
   * exactly when `liveTokRate` is `null` (nothing running to have a state
   * for). */
  liveTokState: LiveState | null;
  /** Present only when `liveTokState === "rest"`. */
  liveTokRestSecondsLeft?: number;
  /** (#2885) `true` when `liveTokRate` is carried forward from an earlier
   *  turn on at least one contributing session rather than freshly measured
   *  — the card dims the rate line. See
   *  `lib/tokenRate.ts::AggregatedTokenRate`. */
  liveTokCarried: boolean;
  /** (#2881) One entry per currently-running execution on this machine, as
   *  of `t` — the pager's per-page data. Sorted by session id, a STABLE
   *  order independent of state/rate, so a pager's page numbers do not
   *  reshuffle tick to tick while the operator is looking at one page (see
   *  `FleetLens.tsx`'s sticky-pick doc). Empty when nothing is running,
   *  same condition as `liveTokRate === null`. The single machine-wide
   *  `liveTokRate` above is unchanged — it is still the card's TOTAL (moved
   *  to the count line once there are 2+ executions, #2881); this is each
   *  execution's OWN reading. */
  executions: ExecutionTokenReading[];
  /** (#2881) The pager's default page's session id — the busiest of
   *  `executions` (`busiestExecution`). `null` when `executions` is empty. */
  defaultExecutionSessionId: string | null;
  /** (#2915) The machine's utility strip: its utility model, residency, and
   *  live utility job (compaction, radio routing, or any newer job), quiet
   *  when none. Machine-level, separate from the work model's scope: a radio
   *  routing job has no session, so it reaches the card only here. */
  utility: UtilityStrip;
}

/** `machPresent()`'s boolean-or-null result, narrowed to "definitely
 * absent" — the only value the fleet card's `stat`/CSS branch reads
 * (`unknown` presence renders the same as "present" for this purpose,
 * matching `absent?'offline':(act?...)`'s two-way branch). */
export function buildFleetCard(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: MachineSpecsResponse | null,
  presence: Presence,
  machAbsent: boolean,
  m: string,
  /** (Playback parity, Change A) No longer read by anything this function
   * RETURNS — kept as a parameter only so the many existing call sites
   * (live and replay alike) don't all need a positional-argument rewrite.
   * `runsCount`/`runsLabel`/`runningSessionIds`/`liveTokRate`/
   * `liveTokStalled` are now ONE derivation over records up to `t` in both
   * modes; see this module's own doc. */
  _liveMode: boolean,
  /** The playhead — `PlaybackLens`'s scrubbable `t` (#1869), pinned to the
   * day's true max in live mode (there is no scrubber on `/next`'s default
   * route). Every run's lifecycle is read as of it. */
  t: number,
  /** The fleet view's row for this machine, when the view holds one: its
   * hardware line, status note, grant and standing replace the flow-derived
   * ones. */
  row: RowFacts | null = null,
  /** (#1923) This machine's rows from `GET /runs` — see `runningLabRunCount`'s
   * own doc for why reading this here is a display-layer join, not a sink
   * crossing. Defaults to `[]` so every pre-#1923 call site (none of which
   * has `/runs` data to hand) keeps behaving exactly as before. A replay
   * caller has no `/runs` fetch to hand either, so this is naturally `[]`
   * there too — nothing here branches on mode; the data simply isn't
   * fetched (the one thing mode is still allowed to decide). */
  machineRuns: Run[] = [],
  /** (#2886 pass 3, "STALL while disconnected") Whether the PAGE has a
   * working connection to the daemon right now — read by the caller from
   * the same liveness source the header renders (`hooks/useLiveTail.ts`'s
   * `LiveTailStatus`). Defaults to `true` so every existing call site
   * (tests, and a replay call — see `liveStateWhileConnected`'s own doc for
   * why disconnection is meaningless there) keeps behaving exactly as
   * before; `FleetLens.tsx`'s live-mode render is the one caller that
   * passes the real value. */
  connected = true,
  /** (#2886 pass 4, do-it — fresh-reviewer finding 5, "half-open connection
   * race") The last moment the page confirmed contact with the daemon
   * (`App.tsx`'s `lastContactRef`, sourced from `useLiveTail`'s
   * `onContact`) — `null` when unknown (tests, a replay call, or a
   * genuinely never-live route), in which case the half-open check inside
   * `liveStateWhileConnected` is skipped and only `connected` governs, same
   * as before this parameter existed. */
  lastContactMs: number | null = null,
  /** (#2921 follow-up) The declared roster, so a machine nothing else names
   *  takes its roster id — the same `displayNameOf` its activity lane uses. */
  roster: readonly RosterName[] = [],
  /** (#2928) The live channel's overlay (`lib/liveChannel.ts`), passed by the
   *  live fleet lens at the live edge only. `null` (every replay, every test
   *  that does not opt in) derives from durable records exactly as before.
   *  Merged per execution into the scope's record sets and, for this
   *  machine only (the channel is local-daemon only), into the utility
   *  strip. */
  live: LiveOverlay | null = null,
  /** The daemon's lifecycle policy (`/runs.policy`). */
  policy: LifecyclePolicy = DEFAULT_POLICY,
): FleetCard {
  return withLiveReadings(
    buildFleetCardBase(data, liveMachines, specs, presence, machAbsent, m, _liveMode, t, row, machineRuns, roster, policy),
    t,
    connected,
    lastContactMs,
    live,
  );
}

/** What a card says about WHO the machine is and whether it is up. A machine
 * the view holds reads its hardware line, status note, grant and standing
 * from its row; any other (an unverified source, a beating machine nobody
 * rostered, a replay) reads presence and the flow window. */
function cardIdentity(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: MachineSpecsResponse | null,
  machAbsent: boolean,
  m: string,
  roster: readonly RosterName[],
  row: RowFacts | null,
) {
  if (!row) {
    const spec = specOf(data, liveMachines, specs, m);
    return {
      name: displayNameOf(data, liveMachines, specs, m, roster),
      spec,
      // (#1855) `specUnknown` says whether a beat existed to carry hardware.
      specUnknown: spec ? null : liveMachines.has(m) ? ("not-reported" as const) : ("not-seen" as const),
      note: null,
      grant: null,
      hub: false,
      standing: machAbsent ? ("offline" as const) : ("online" as const),
      availability: "known" as const,
      self: false,
    };
  }
  return {
    // (#2814) `nameOf` plus the self-identity floor: see `displayNameOf`. A
    // machine only the view knows is named by its own card's name when the
    // view read one, else its roster id (`rowFacts`).
    name: row.known ? displayNameOf(data, liveMachines, specs, m, roster) : (row.name ?? m),
    spec: row.spec,
    specUnknown: row.spec ? null : ("not-reported" as const),
    note: row.note,
    grant: row.grant,
    hub: row.hub,
    standing: row.standing,
    availability: machineAvailability({ self: row.isSelf, seen: row.known, standing: row.standing }),
    // (#2915) The view says which row is this machine; a peer's model is
    // read off its own utility records and its residency is unknown.
    self: row.isSelf,
  };
}

/** (#2928 re-review, C-1) Everything on a card that reads the WINDOW of
 *  durable records (activity, running sessions, names, hardware, counts),
 *  plus the per-session record sets its live readings start from. The fleet
 *  lens builds this once per data change and per wall second, never per live
 *  sample: a live sample changes only the readings `withLiveReadings`
 *  derives, over the running sessions alone. */
export interface FleetCardBase extends Omit<FleetCard, "liveTokRate" | "liveTokStalled" | "liveTokState" | "liveTokRestSecondsLeft" | "liveTokCarried" | "executions" | "defaultExecutionSessionId" | "utility"> {
  /** @internal The inputs the live readings need. */
  liveInputs: {
    data: NormRecord[];
    /** Each running run's session id, in `durableSets`' order. */
    runningSids: string[];
    /** Each running run's durable records, cut at the base's `t`. */
    durableSets: NormRecord[][];
    policy: LifecyclePolicy;
    presence: Presence;
    self: boolean;
    binding: { id: string; loaded: boolean } | null;
  };
}

export function buildFleetCardBase(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: MachineSpecsResponse | null,
  presence: Presence,
  machAbsent: boolean,
  m: string,
  _liveMode: boolean,
  t: number,
  row: RowFacts | null = null,
  machineRuns: Run[] = [],
  roster: readonly RosterName[] = [],
  policy: LifecyclePolicy = DEFAULT_POLICY,
): FleetCardBase {
  // (Playback parity, Change A, findings #3/#4) ONE question, asked the
  // same way in both modes: which of this machine's runs are RUNNING as of
  // `t` (`runningRuns`). This used to be the day's whole session roster in
  // replay — the "48 specialists at 5% into the day with zero sessions
  // started" defect.
  const running = runningRuns(data, presence, m, t, policy);
  const flowActive = running.length > 0;
  const labRunning = runningLabRunCount(machineRuns);
  const active = flowActive || labRunning > 0;
  const id = cardIdentity(data, liveMachines, specs, machAbsent, m, roster, row);
  const standing = id.standing;
  const stat = standing === "offline" ? "offline" : active ? "dispatch in flight" : standing === "unknown" ? NO_SIGNAL_STAT : "idle";
  // (#2060) `topLevelRuns` collapses a mission's own session together with
  // any of its seat/step dispatches into ONE entry — a mission with one seat
  // running reads "1 running", not "2 running", in both modes.
  const runningSessionIds = topLevelRuns(running).map((g) => g.sessionId);
  // (#1923) The two sources OVERLAP — they are not disjoint, and summing
  // them double-counts. A lab run in its dispatch phase appears on BOTH:
  // once as its `/runs` lab row, once as the flow session its provider's
  // `dispatch` call emits presence for (see `runningLabRunCount`'s doc).
  // Nothing joins the two ids — the lab run id carries epoch SECONDS from
  // `lab/run.rs`, the dispatch session id epoch MILLIS minted later inside
  // the provider, and the lab dispatch carries no `mission_id` for
  // `topLevelRuns` to collapse on — so the merge here is
  // `Math.max`, the cheapest rule that is never wrong in the direction that
  // matters:
  //
  // - lab run mid-dispatch, alone   → max(1, 1) = 1  (was 2: the defect)
  // - lab run between dispatches    → max(0, 1) = 1  (the gap #1923 closes)
  // - two lab runs, both quiescent  → max(0, 2) = 2
  // - lab run + a standalone crew dispatch → max(2, 1) = 2
  //
  // What it gives up: a lab run in a NON-dispatch phase running alongside
  // unrelated flow work undercounts — one lab run scoring while one mission
  // dispatches reads "1 running", not 2. That is a strictly smaller lie
  // than the systematic double-count it replaces, and `active` below is
  // unaffected either way (a live lab run always lights the card).
  //
  // The durable fix is a real join key, and (#2511) it now EXISTS: the lab
  // dispatch's session id is carried on the start-time `lifecycle.json` as
  // soon as a single-dispatch provider mints it — no longer recorded only
  // in the run manifest `providers/coding_task.rs` writes AFTER the
  // dispatch returns, and `Run.dispatch_id` is populated for a lab row too
  // (its own doc covers exactly when). This card has not been updated to
  // USE that join yet — `runningSessionIds`/`labRunning` still merge by
  // `Math.max` rather than collapsing on the shared session id the way
  // `topLevelRuns` collapses a mission's seats — so the arithmetic
  // above is unchanged for now; that collapse is a follow-up to this
  // card specifically, not a producer-side gap any more.
  const runsCount = Math.max(runningSessionIds.length, labRunning);
  // (#2877) Scoped by the RAW running runs (`running`), not the
  // mission-collapsed `runningSessionIds` above: a mission's own
  // top-level session never carries heartbeats (its inner role executions
  // do — same fact `sessionRun.ts::rollUpMissionModelWork`'s doc names),
  // and `topLevelRuns` picks EITHER representative depending on
  // which happened to land in the running set. Reading every running
  // session's own heartbeats sidesteps that ambiguity entirely: a session
  // with no heartbeats (a mission's top-level session, or one between
  // turns) contributes nothing, `aggregateTokenRate` sums what real
  // generation IS producing as of `t`. (Playback parity, Change A) Computed
  // the same way in replay now too — a replayed instant where something was
  // genuinely generating reads the same "N tok/s" a live viewer saw at that
  // instant (finding #3): the tok/s scope is a fact about the recorded
  // instant, not a live-only instrument.
  // The as-of cut is load-bearing here, not redundant with the LIVE
  // caller's window already being time-bounded: a REPLAY caller hands this
  // function the WHOLE day's records (presence is empty, so the running
  // verdict comes from records up to `t`), and
  // without this filter `currentTokenRate`'s "two most recent heartbeats"
  // would read heartbeats from AFTER the playhead too, inflating/changing
  // the rate a live viewer actually saw at `t` (measured: 122 tok/s off a
  // heartbeat 6h in the day's future vs the correct 95 tok/s as of `t`).
  const runningSids = running.map((g) => g.sessionId);
  const durableSets = running.map((g) => recordsAsOf(g.records, t));
  return {
    uid: m,
    name: id.name,
    spec: id.spec,
    specUnknown: id.specUnknown,
    note: id.note,
    grant: id.grant,
    hub: id.hub,
    standing,
    availability: id.availability,
    active,
    absent: standing === "offline",
    stat,
    runsCount,
    // (Playback parity, Change A, finding #3) Always "running" now — a
    // replayed instant with genuinely running sessions reads the same word
    // a live viewer would have seen. `liveMode` no longer changes this, and
    // "running" (a gerund, not a count noun) never pluralizes.
    runsLabel: "running",
    runningSessionIds,
    liveInputs: { data, runningSids, durableSets, policy, presence, self: id.self, binding: id.self ? (specs?.utility_model ?? null) : null },
  };
}

/** (#2958) The word a card shows in place of its status until the first
 *  data it is derived from has arrived: the same "no signal" a running
 *  execution's line says when the page loses the daemon (#2886), so a card
 *  that knows nothing yet reuses the existing word for "no information"
 *  rather than a new indicator. */
export const NO_SIGNAL_STAT = "no signal";

/** (#2958) Which of a fleet card's sources have answered at least once on
 *  this mount (success or failure; a source this mount never reads counts
 *  as answered). A failed read counts: it has its own notice
 *  (`RunsUnreadableNotice`, `FleetCoverageNotice`), and waiting on it would
 *  hold "no signal" forever. The flow window is the exception (#2965): its
 *  failed read yields an empty window, which every negative claim here would
 *  read as "nothing happened", so `flow` is false while a day's read is
 *  failing (and `FlowReadNotice` names it). `/fleet/roster` is not a
 *  source here: it names machines, never says what one is doing. The caller latches each one, so only the
 *  FIRST answer counts: a later pending read (a refetch, or the flow
 *  window's new day key at UTC midnight) never re-enters "no signal". */
export interface CardSourcesAnswered {
  /** The flow window: sessions, activity, `machine.online/offline` edges,
   *  utility jobs. */
  flow: boolean;
  /** Who is up: `/fleet/view` (each row's liveness) and `/fleet/machines/live`
   *  (a machine the view does not hold). */
  presence: boolean;
  /** `/fleet/dispatches/live`: which sessions are running. */
  sessions: boolean;
  /** `/runs`: a lab run in flight, which never rides the flow stream (#1923). */
  runs: boolean;
}

/** (#2958) What a card may say, given what has answered so far. */
export interface CardFace {
  /** The status word: "offline", "dispatch in flight", "idle", or "no signal". */
  stat: string;
  /** Drawn as offline (dimmed card, powered-off tube). */
  absent: boolean;
  /** Drawn as active. */
  active: boolean;
  /** The status word is "no signal" (the dot takes the no-reading gray). */
  noSignal: boolean;
  /** The tube: a live execution's reading, the idle tube, the powered-off
   *  screen of an offline machine, or no-signal static. */
  tube: "reading" | "idle" | "off" | "nosignal";
  /** The running count is shown; otherwise "—" holds its line. */
  countShown: boolean;
  /** The utility strip may call a quiet strip "idle"; otherwise its words
   *  say "no signal". A running utility job always shows. */
  utilityQuietKnown: boolean;
}

/** (#2958) A POSITIVE reading shows as soon as the source that produced it
 *  has it; a NEGATIVE claim waits until every source that could contradict
 *  it has answered. The operator watched every card say "idle" for the
 *  3.3 s `/runs` took to answer while a run was live: "idle" was the value a
 *  card falls back to when no record says otherwise, a claim nobody had
 *  read yet.
 *
 *  - Positive, shown at once: a live execution (its tube, rate line and
 *    pager), "dispatch in flight" (`active` is only ever set by a record or
 *    a `/runs` row that says so), a running count of one or more, a running
 *    utility job. A count read before `/runs` answers is a lower bound: the
 *    lab count can only raise it (`Math.max`, #1923).
 *  - "offline": waits on presence, the view and the flow window (a beat, or
 *    a `machine.online` edge, contradicts it). It wins over a reading: an
 *    offline card's tube is powered off.
 *  - A machine whose standing is unknown (presence could not say and its card
 *    was not read) never says "idle": it says "no signal".
 *  - "idle", "no model working", "0 running": wait on every source.
 *  - A quiet utility strip's "idle": waits on the flow window, the only
 *    source of utility jobs.
 *  Until then the card says "no signal", the same word a running
 *  execution's line says when the page loses the daemon (#2886), in the
 *  same boxes. */
export function cardFace(
  card: { absent: boolean; active: boolean; runsCount: number; standing: Standing; availability: MachineAvailability },
  hasReading: boolean,
  answered: CardSourcesAnswered,
): CardFace {
  const all = answered.flow && answered.presence && answered.sessions && answered.runs;
  const offlineKnown = answered.flow && answered.presence;
  const absent = card.absent && offlineKnown;
  const active = card.active && !absent;
  // (5.0 R3) A machine whose records never reach this viewer proves nothing by
  // being quiet: only a `known` one may read idle, 0 running or a quiet strip.
  const seen = card.availability === "known";
  const idleKnown = all && seen && card.standing === "online";
  const stat = absent ? "offline" : card.active ? "dispatch in flight" : idleKnown ? "idle" : NO_SIGNAL_STAT;
  // Offline wins: a machine said to be gone draws the powered-off screen,
  // even over a reading its last records left behind.
  const tube = absent ? "off" : hasReading ? "reading" : idleKnown ? "idle" : "nosignal";
  return {
    stat,
    absent,
    active,
    noSignal: stat === NO_SIGNAL_STAT,
    tube,
    countShown: card.runsCount > 0 || (all && seen),
    utilityQuietKnown: answered.flow && seen,
  };
}

/** (#2928 re-review, C-1) A card's live readings (the scope's rate and
 *  state, the per-execution pages, the utility strip) from its base, the
 *  page clock `t`, and the live overlay. Touches only the running sessions'
 *  records and the overlay. */
export function withLiveReadings(
  base: FleetCardBase,
  t: number,
  connected = true,
  lastContactMs: number | null = null,
  live: LiveOverlay | null = null,
): FleetCard {
  const { data, runningSids, durableSets, policy, presence, self, binding } = base.liveInputs;
  const active = base.active;
  const m = base.uid;
  const liveTokRecordSets = runningSids.map((sid, i) => {
    const liveRecs = live?.bySession.get(sid);
    return liveRecs ? mergeLive(durableSets[i], recordsAsOf(liveRecs, t)) : durableSets[i];
  });
  // (#2877 dogfood finding) A session can be `active` (no terminal record
  // yet — a mission genuinely stuck open, observed live: `status: "running"`
  // hours after its last real heartbeat) while its heartbeat stream has long
  // gone quiet. Without this, `aggregateTokenRate` happily reports whatever
  // its LAST two heartbeats measured, however old — a fleet card reading
  // "42 tok/s" for a session that stopped producing hours ago. Zeroing the
  // NUMBER (not hiding the tile — a stalled scope still mounts and shows the
  // decaying-ring visual, per the issue's own state list) keeps the readout
  // honest about what "right now" means. `t`, not `Date.now()`: this
  // function is otherwise pure over its inputs (`machActive` above uses the
  // same playhead), and a wall-clock read here would make an identical call
  // non-deterministic and untestable.
  // (#2877 pass 2) One state derivation shared with the run page
  // (`sessionRun.ts`'s `liveTokScope`) — see `lib/tokenRate.ts::
  // deriveLiveState`'s own doc. `liveTokStalled` is now DERIVED from it
  // (`=== "stalled"`) rather than a second, separately-computed "every
  // running session's heartbeats are stale" check.
  // (#2886 pass 3, "STALL while disconnected"; pass 4 finding 5, "half-open
  // connection race") Downgraded the same way the run page's
  // `sessionRun.ts` downgrades it — see
  // `lib/tokenRate.ts::liveStateWhileConnected`'s own doc. `connected`
  // defaults to `true`, so this is a no-op for every caller that doesn't
  // pass it (tests, and a replay call, where disconnection is meaningless).
  // `lastHeartbeatMs` is this MACHINE's most recent heartbeat across its
  // running sessions — the deadline the half-open check compares
  // `lastContactMs` against; `null` when unknown skips that check too.
  const liveTokLiveState =
    liveTokRecordSets.length > 0
      ? liveStateWhileConnected(
          aggregateLiveState(liveTokRecordSets, t, policy, presence),
          connected,
          // Only construct the half-open evidence when this caller actually
          // HAS it — `lastContactMs === null` means "not wired for this
          // route" (App.tsx's own fold), not "confirmed no contact ever",
          // and must skip the check entirely rather than distrust every
          // stall on principle.
          lastContactMs != null ? { lastContactMs, lastHeartbeatMs: lastHeartbeatMs(liveTokRecordSets) } : undefined,
        )
      : null;
  const liveTokStalled = liveTokLiveState?.state === "stalled";
  // While the machine has a running execution the scope stays mounted: at 0
  // with its state word when nothing is generating (resting, tools, reading
  // prompt, stalled), rather than vanishing. Only a machine with nothing
  // running mounts no scope.
  // Only when a live EXECUTION exists: a mission between model steps (only
  // its run session beating) has no model working, so no scope and no state.
  const hasLiveExecution = liveExecutions(liveTokRecordSets, t, policy, presence).length > 0;
  // (#2885) `aggregateTokenRate` now returns `{tokensPerSec, carried}` —
  // `rawTokReading` is `null` exactly when there is nothing running or no
  // execution has a reading yet, same as before.
  const rawTokReading = active && hasLiveExecution ? aggregateTokenRate(liveTokRecordSets, t, policy, presence) : null;
  const rawLiveTokRate = active && hasLiveExecution ? (rawTokReading?.tokensPerSec ?? 0) : null;
  const liveTokRate = rawLiveTokRate != null && liveTokStalled ? 0 : rawLiveTokRate;
  const liveTokState = liveTokRate !== null ? (liveTokLiveState?.state ?? null) : null;
  const liveTokRestSecondsLeft = liveTokState === "rest" ? liveTokLiveState?.restSecondsLeft : undefined;
  // Only meaningful while `liveTokState === "generating"` — that's the one
  // state whose rate line shows the NUMBER (`FleetLens.tsx`'s rate line
  // shows a state word otherwise), so a carried reading during rest/tools/
  // prompt/stalled would dim text that isn't the rate at all.
  const liveTokCarried = liveTokState === "generating" ? (rawTokReading?.carried ?? false) : false;
  // (#2881) Per-execution readings for the pager — ONE derivation, live or
  // replay, over the SAME `liveTokRecordSets` the aggregate reading above
  // already narrowed to this machine's running sessions as of `t` and
  // `liveExecutions` already filtered down to genuine execution evidence
  // (see that function's own doc). Sorted by session id — see `executions`'
  // own field doc on `FleetCard` for why the order must be stable rather
  // than resorted by business every tick.
  // (#2886 pass 4 parity) Same half-open-connection evidence the aggregate
  // reading above threads into `liveStateWhileConnected` — but per
  // EXECUTION, `lastHeartbeatMs` is THIS execution's own last heartbeat
  // (`lastHeartbeatMs([recs])`), not the machine-wide max the aggregate
  // uses. Sharing the machine-wide max here would let one execution's
  // fresher heartbeat wrongly excuse another, quieter execution's own
  // genuine stall — the whole point of per-page state is that each page
  // answers for its OWN run, not the busiest one on the card.
  const executions: ExecutionTokenReading[] = liveExecutions(liveTokRecordSets, t, policy, presence)
    .map((recs) =>
      executionTokenReading(
        recs,
        t,
        connected,
        lastContactMs != null ? { lastContactMs, lastHeartbeatMs: lastHeartbeatMs([recs]) } : undefined,
      ),
    )
    .sort((a, b) => (a.sessionId < b.sessionId ? -1 : a.sessionId > b.sessionId ? 1 : 0));
  const defaultExecutionSessionId = busiestExecution(executions)?.sessionId ?? null;
  const utility = utilityStrip(data, m, t, binding, self && live ? live.utility : []);
  const { liveInputs: _inputs, ...rest } = base;
  return {
    ...rest,
    liveTokRate,
    liveTokStalled,
    liveTokState,
    liveTokRestSecondsLeft,
    liveTokCarried,
    executions,
    defaultExecutionSessionId,
    utility,
  };
}
