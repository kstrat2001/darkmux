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
import type { FlowRecord, MachineSpecs, PresenceBeat } from "../../types/handwritten";
import { nameOf, machineNames } from "../../lib/flow";
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

/** (#1923) Lab runs deliberately do NOT ride the flow stream — CLAUDE.md's
 * cross-system contract 3, the lab/fleet sink boundary: "lab runs write
 * per-run-local artifacts; the fleet flow stream carries engagement work
 * only. No crossings in either direction." So `machActive`/`sessionsOn`
 * (both flow-derived) structurally cannot see a lab run — a machine running
 * only lab work reads "idle" / "0 running" no matter how long it runs.
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
  /** (#1903) The session ids counted into `runsCount`, LIVE MODE ONLY —
   * always empty in replay, where `runsCount` counts the day's whole
   * session set rather than currently-running work (see `runsCount`'s own
   * comment above). Lets `FleetLens.tsx` build the running-count's own tap
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
  // (#1923) `+ labRunning`: lab runs are additive, never a replacement for
  // the flow-derived count — see `runningLabRunCount`'s own doc for why a
  // mission/dispatch row from the same `/runs` payload is excluded here.
  const runsCount = liveMode ? runningSessionIds.length + labRunning : all.length;
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
