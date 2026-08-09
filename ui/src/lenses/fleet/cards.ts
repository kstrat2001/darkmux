/**
 * The fleet default view's machine-card row — `renderFleet()`'s `cards`
 * build (viewer.html:1675-1687), plus the two helpers it leans on:
 * `machActive()` (viewer.html:1315-1322) and `specOf()`
 * (viewer.html:1120-1125).
 *
 * This port is always "live mode" (`/next` has no playback scrubber — see
 * `savings.ts`'s module doc for the same reasoning applied to the token
 * sums), so every `liveMode?...:...` branch in the legacy source collapses
 * to its live-mode arm here: `runs` counts only sessions ∈ the live set
 * (never the whole day's history), and the label is always "running" (never
 * "specialist(s)").
 */

import { uidOf, sessionsOn } from "../../lib/flow";
import type { FlowRecord, MachineSpecs, PresenceBeat } from "../../types/handwritten";
import { nameOf } from "../../lib/flow";

/** `machActive()` — viewer.html:1315-1322. A machine is "in flight" iff one
 * of its started sessions is still running (∈ the live session set — this
 * port's live-mode-only `sessionRunning()`). */
export function machActive(data: FlowRecord[], liveSet: Set<string>, m: string): boolean {
  return data.some((r) => uidOf(r) === m && r.action === "dispatch.start" && liveSet.has(r.session_id ?? ""));
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
): FleetCard {
  const active = machActive(data, liveSet, m);
  const stat = machAbsent ? "offline" : active ? "dispatch in flight" : "idle";
  const runsCount = sessionsOn(data, m).filter((sid) => liveSet.has(sid)).length;
  return {
    uid: m,
    name: nameOf(data, liveMachines, m),
    spec: specOf(data, liveMachines, specs, m),
    active,
    absent: machAbsent,
    stat,
    runsCount,
  };
}
