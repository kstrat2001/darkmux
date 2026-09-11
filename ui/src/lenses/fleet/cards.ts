/**
 * The fleet default view's machine-card row — `renderFleet()`'s `cards`
 * build (viewer.html:1675-1687), plus the two helpers it leans on:
 * `machActive()` (viewer.html:1315-1322) and `specOf()`
 * (viewer.html:1120-1125).
 *
 * (#1800 P2) `liveMode` is REAL here now. It used to be assumed true, because
 * `/next` had no historical route — so both of legacy's `liveMode?...:...`
 * branches collapsed to their live arm. `PlaybackLens` reaches this code with
 * a recorded day, where the live arm is wrong twice over: `runs` counted
 * sessions against a live set that describes NOW (a replayed day reads "0
 * running"), and the label said "running" for work that finished hours ago.
 * Legacy's replay arm counts ALL of the day's sessions and labels them
 * "specialist(s)" — `goldens/playback-date.txt` reads "48 specialists" where
 * `goldens/fleet.txt` reads "0 running", from this one branch.
 *
 * (#1869) `t` (named `tMax` at some call sites, but it is the PLAYHEAD, see
 * `buildFleetCard`'s own doc) is now genuinely scrubbable — `PlaybackLens`
 * owns a `t` state and can hand this module anything from `tMin` to the
 * day's true max, not just the max. `runsCount` deliberately still counts
 * the WHOLE day's sessions in replay mode regardless of the playhead
 * (`all.length`, unchanged — legacy's own `runs=liveMode?...:all.length`
 * reads the same unfiltered `sessionsOn(m)`); `machActive` is the one
 * derivation that DOES need the playhead honored, and its own doc explains
 * why.
 */

import { uidOf, sessionsOn, sessionRunning, T } from "../../lib/flow";
import type { FlowRecord, MachineSpecs, PresenceBeat, RosterMachineEntry } from "../../types/handwritten";
import { nameOf, machineNames, machineUids } from "../../lib/flow";
import type { Run } from "../../types/generated/Run";

/** `machActive()` — viewer.html:1342-1349. A machine is "in flight" iff one
 * of its started sessions is still running — routed through the shared
 * `sessionRunning()` (live = presence, replay = close-edge at the playhead)
 * so the running-forever bug class can't be fixed at one site and linger at
 * another.
 *
 * (#1869) `T(r.ts) <= t` restores legacy's own `visible()` gate — legacy's
 * `machActive` reads `visible().some(...)`, `visible = () =>
 * DATA.filter(r=>T(r.ts)<=state.t)`. This port dropped the gate because,
 * before the playback transport existed, `t` was always the day's true max
 * (`computeTMax`), making it an unconditional no-op. Now that `PlaybackLens`
 * can hand this a playhead BEFORE the day's end, a `dispatch.start` that
 * hasn't happened yet as of that playhead must not read as "in flight" —
 * without this guard, scrubbing to before a machine's first session of the
 * day still rendered it active, because `sessionRunning`'s close-edge check
 * finds no close (there's nothing to close yet) and defaults to "running". */
export function machActive(
  data: FlowRecord[],
  liveSet: Set<string>,
  m: string,
  liveMode: boolean,
  t: number,
): boolean {
  return data.some(
    (r) =>
      T(r.ts) <= t &&
      uidOf(r) === m &&
      r.action === "dispatch.start" &&
      sessionRunning(data, liveSet, r.session_id ?? "", liveMode, t),
  );
}

/** `specOf()` — viewer.html:1120-1125. Returns a RAW string (JSX escapes at
 * render time, same "escape at the template edge" discipline the legacy
 * comment names). `MACH_SPEC` (a static hardcoded lookup) is empty in the
 * live viewer — dropped here entirely, matching that source comment. */
export function specOf(
  data: FlowRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: MachineSpecs | null,
  m: string,
  /** (#2067) Where a REMOTE card's hardware line comes from. Defaults to the
   * presence beats; a static build passes its committed fleet snapshot
   * instead, since it cannot poll presence at all. */
  specBeats: Map<string, PresenceBeat> = liveMachines,
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
  // (#2067) Keyed off `liveMachines`, not `specBeats`, on purpose: this
  // branch answers "is `m` the machine `/machine/specs` describes", a
  // liveness-side identity question, and `specs` is null on a static build
  // (the query is live-only) so the branch never runs there. If a static
  // machine-specs source is ever wired in, this alias lookup must read the
  // snapshot too.
  if (specs && specs.machine_id && machineNames(data, liveMachines, m).has(specs.machine_id) && specs.cpu_brand) {
    const gb = specs.ram_total_bytes ? ` · ${Math.round(specs.ram_total_bytes / 1073741824)} GB` : "";
    return specs.cpu_brand + gb;
  }
  const beat = specBeats.get(m);
  return beat?.specs || "";
}

/** (#2060) Collapse a machine's set of currently-running session ids down to
 * TOP-LEVEL runs: a mission's own whole-run session and its seat/step
 * dispatches are one mission, not one-run-per-seat.
 *
 * The distinguishing shape (`src/mission_launch.rs::mission_bookend_record`):
 * a mission's OWN bookend stamps `session_id === mission_id` (the mission id
 * doubles as its own top-level session). A seat/step dispatch the mission
 * launches carries the SAME `mission_id` but its OWN, different
 * `session_id` (`launch_session_id`/`scope_to_run`/`dispatch.map`'s per-item
 * scoping). So: group by `mission_id` when present, one run per group,
 * preferring the mission's own top-level session as the group's
 * representative id (so a single-running-item drill-in lands on the
 * mission, not on whichever seat happened to be seen first). A session with
 * no `mission_id` at all (a standalone dispatch, a lab run) always counts on
 * its own — nothing to collapse into.
 */
export function topLevelRunSessionIds(data: FlowRecord[], sessionIds: string[]): string[] {
  const missionIdOf = new Map<string, string | undefined>();
  for (const r of data) {
    if (!r.session_id || missionIdOf.has(r.session_id)) continue;
    if (r.mission_id) missionIdOf.set(r.session_id, r.mission_id);
  }
  const standalone: string[] = [];
  const repForMission = new Map<string, string>();
  for (const sid of sessionIds) {
    const missionId = missionIdOf.get(sid);
    if (!missionId) {
      standalone.push(sid);
      continue;
    }
    const isTopLevel = missionId === sid;
    const existing = repForMission.get(missionId);
    if (!existing || isTopLevel) repForMission.set(missionId, sid);
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
 * `useLiveSessionIds` → `liveSet` reads. `lib/flow.ts`'s bookend-matcher
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
 * accounted for by flow presence (via `topLevelRunSessionIds` above,
 * post-#2060), and counting it again here would double-count it. */
export function runningLabRunCount(machineRuns: Run[]): number {
  return machineRuns.filter((r) => r.kind === "lab" && r.status === "running").length;
}

/** (#1855) Which of the operator's DECLARED roster entries have NO known
 * identity in this window at all — no flow record under that name, no live
 * presence beat under it either. These are the entries the card list would
 * otherwise drop silently: `machineUids` only ever unions flow-derived uids
 * with CURRENTLY-beating presence keys, so a machine the operator added via
 * `darkmux machine add` and which has never once started its daemon (or is
 * down right now, with zero history) produces no uid for it to fall back
 * on — the exact "rostered-but-silent machine vanishes entirely" defect.
 *
 * A roster entry IS excluded here — deliberately NOT double-reported —
 * when its `id` matches any alias (`machineNames`) any known uid has ever
 * used, whether that uid is currently beating or only has past flow
 * history. Matching is exact-string against `machine_id`/`display_name`,
 * the same identity contract `roster.rs::MachineEntry.id`'s own doc
 * states ("what flow records carry as `machine_id`") — an operator who set
 * `DARKMUX_MACHINE_ID` to match their roster entry's `id` gets no
 * duplicate; the burden is naming the entry to match, not on this filter
 * to guess at aliases it has no evidence for. */
export function rosterOnlyEntries(
  data: FlowRecord[],
  liveMachines: Map<string, PresenceBeat>,
  roster: RosterMachineEntry[],
): RosterMachineEntry[] {
  const knownNames = new Set<string>();
  for (const uid of machineUids(data, liveMachines)) {
    for (const name of machineNames(data, liveMachines, uid)) knownNames.add(name);
  }
  return roster.filter((entry) => !knownNames.has(entry.id));
}

export interface FleetCard {
  uid: string;
  name: string;
  /** "" means legacy's `specdim` fallback ("hardware not reported"). */
  spec: string;
  active: boolean;
  absent: boolean;
  stat: string;
  runsCount: number;
  /** `${runs} ${liveMode?'running':'specialist'+(runs===1?'':'s')}` —
   * viewer.html:1713. The whole label, not just the noun, so the pluralization
   * rule lives beside the count it describes. */
  runsLabel: string;
  /** (#1903) The machine's currently-running FLOW sessions, collapsed to
   * top-level runs — LIVE MODE ONLY, always empty in replay, where
   * `runsCount` counts the day's whole session set rather than
   * currently-running work (see `runsCount`'s own comment above).
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
}

/** `machPresent()`'s boolean-or-null result, narrowed to "definitely
 * absent" — the only value the fleet card's `stat`/CSS branch reads
 * (`unknown` presence renders the same as "present" for this purpose,
 * matching `absent?'offline':(act?...)`'s two-way branch). */
export function buildFleetCard(
  data: FlowRecord[],
  liveMachines: Map<string, PresenceBeat>,
  specs: MachineSpecs | null,
  liveSet: Set<string>,
  machAbsent: boolean,
  m: string,
  liveMode: boolean,
  /** The playhead — `PlaybackLens`'s scrubbable `t` (#1869), pinned to the
   * day's true max in live mode (there is no scrubber on `/next`'s default
   * route). `sessionRunning`'s replay arm and `machActive`'s `T(r.ts) <= t`
   * gate are both defined against it. */
  t: number,
  /** (#2067) See `specOf`'s own doc — the spec source, when it is not the
   * presence beats (a static build). */
  specBeats: Map<string, PresenceBeat> = liveMachines,
  /** (#1923) This machine's rows from `GET /runs` — see `runningLabRunCount`'s
   * own doc for why reading this here is a display-layer join, not a sink
   * crossing. Defaults to `[]` so every pre-#1923 call site (none of which
   * has `/runs` data to hand) keeps behaving exactly as before. LIVE MODE
   * ONLY, same as `runningSessionIds` — replay's "specialists" tally is a
   * different, already-flow-complete question (see that field's own
   * comment). */
  machineRuns: Run[] = [],
): FleetCard {
  const flowActive = machActive(data, liveSet, m, liveMode, t);
  const labRunning = liveMode ? runningLabRunCount(machineRuns) : 0;
  const active = flowActive || labRunning > 0;
  const stat = machAbsent ? "offline" : active ? "dispatch in flight" : "idle";
  const all = sessionsOn(data, m);
  // (#691 Slice 2 / viewer.html:1704) Live counts only RUNNING sessions —
  // completed dispatches from earlier today must not read as current crew.
  // A replay counts the whole window: that IS the day's work.
  //
  // (#2060) `topLevelRunSessionIds` then collapses a mission's own session
  // together with any of its seat/step dispatches into ONE entry — a
  // mission with one seat running must read "1 running", not "2 running".
  // Replay's `all` stays UNCOLLAPSED on purpose: it tallies the day's whole
  // specialist roster (`runsCount`'s own module doc), which is a different
  // question from "how many things are running right now."
  const runningSessionIds = liveMode ? topLevelRunSessionIds(data, all.filter((sid) => liveSet.has(sid))) : [];
  // (#1923) The two sources OVERLAP — they are not disjoint, and summing
  // them double-counts. A lab run in its dispatch phase appears on BOTH:
  // once as its `/runs` lab row, once as the flow session its provider's
  // `dispatch` call emits presence for (see `runningLabRunCount`'s doc).
  // Nothing joins the two ids — the lab run id carries epoch SECONDS from
  // `lab/run.rs`, the dispatch session id epoch MILLIS minted later inside
  // the provider, and the lab dispatch carries no `mission_id` for
  // `topLevelRunSessionIds` to collapse on — so the merge here is
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
  // dispatch returns — and `Run.session_id` is populated for a lab row too
  // (its own doc covers exactly when). This card has not been updated to
  // USE that join yet — `runningSessionIds`/`labRunning` still merge by
  // `Math.max` rather than collapsing on the shared session id the way
  // `topLevelRunSessionIds` collapses a mission's seats — so the arithmetic
  // above is unchanged for now; that collapse is a follow-up to this
  // card specifically, not a producer-side gap any more.
  const runsCount = liveMode ? Math.max(runningSessionIds.length, labRunning) : all.length;
  return {
    uid: m,
    name: nameOf(data, liveMachines, m),
    spec: specOf(data, liveMachines, specs, m, specBeats),
    active,
    absent: machAbsent,
    stat,
    runsCount,
    runsLabel: liveMode ? "running" : `specialist${runsCount === 1 ? "" : "s"}`,
    runningSessionIds,
  };
}
