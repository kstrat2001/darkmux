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
 */

import { uidOf, sessionsOn, sessionRunning } from "../../lib/flow";
import type { FlowRecord, MachineSpecs, PresenceBeat } from "../../types/handwritten";
import { nameOf } from "../../lib/flow";

/** `machActive()` — viewer.html:1342-1349. A machine is "in flight" iff one
 * of its started sessions is still running — routed through the shared
 * `sessionRunning()` (live = presence, replay = close-edge at the playhead)
 * so the running-forever bug class can't be fixed at one site and linger at
 * another. */
export function machActive(
  data: FlowRecord[],
  liveSet: Set<string>,
  m: string,
  liveMode: boolean,
  t: number,
): boolean {
  return data.some(
    (r) => uidOf(r) === m && r.action === "dispatch.start" && sessionRunning(data, liveSet, r.session_id ?? "", liveMode, t),
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
): string {
  if (m === "unknown") {
    const ns = [...new Set(data.filter((r) => uidOf(r) === "unknown" && r.machine_id).map((r) => r.machine_id as string))];
    return ns.length ? `unverified · claimed: ${ns.join(", ")}` : "unidentified (no hardware uid)";
  }
  // (#1008) THIS machine: prefer the live `/machine/specs` probe (cpu + RAM)
  // over a static lookup. Remote machines fall through to their presence
  // beat's specs.
  const name = nameOf(data, liveMachines, m);
  if (specs && name === specs.machine_id && specs.cpu_brand) {
    const gb = specs.ram_total_bytes ? ` · ${Math.round(specs.ram_total_bytes / 1073741824)} GB` : "";
    return specs.cpu_brand + gb;
  }
  const beat = liveMachines.get(m);
  return beat?.specs || "";
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
  /** The playhead. `tMax` in both modes today (`/next` has no scrubber), but
   * named rather than assumed — `sessionRunning`'s replay arm is defined
   * against it, and a scrubber would change only this argument. */
  t: number,
): FleetCard {
  const active = machActive(data, liveSet, m, liveMode, t);
  const stat = machAbsent ? "offline" : active ? "dispatch in flight" : "idle";
  const all = sessionsOn(data, m);
  // (#691 Slice 2 / viewer.html:1704) Live counts only RUNNING sessions —
  // completed dispatches from earlier today must not read as current crew.
  // A replay counts the whole window: that IS the day's work.
  const runsCount = liveMode ? all.filter((sid) => liveSet.has(sid)).length : all.length;
  return {
    uid: m,
    name: nameOf(data, liveMachines, m),
    spec: specOf(data, liveMachines, specs, m),
    active,
    absent: machAbsent,
    stat,
    runsCount,
    runsLabel: liveMode ? "running" : `specialist${runsCount === 1 ? "" : "s"}`,
  };
}
