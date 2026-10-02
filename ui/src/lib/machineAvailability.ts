/**
 * (#3012, 5.0 R3) What THIS VIEWER knows about a machine's activity: one
 * typed answer, shared by every surface that would otherwise say "idle",
 * "0 running" or "no samples" about a machine it cannot see into.
 *
 * - `known`: this viewer receives records or beats from the machine (or it is
 *   this machine), so a quiet reading is a real reading.
 * - `not_streamed`: the machine is on the roster or readable over its
 *   listener, yet nothing it writes reaches this viewer (its Redis sink is
 *   off, or it points at another hub). Quiet here proves nothing.
 * - `not_reporting`: it is down or silent (records from it may once have
 *   arrived). Its last readings are history.
 *
 * Unknown is never shown as a confident fact: a surface that gets anything
 * but `known` shows an explicit "not reported" state, never "idle", "0" or
 * a quiet strip.
 */
import type { PresenceBeat } from "../types/generated/PresenceBeat";
import type { NormRecord } from "./ingest";
import { windowHoldsMachine } from "./machineIdentity";

export type MachineAvailability = "known" | "not_streamed" | "not_reporting";

export interface AvailabilityFacts {
  /** The machine this daemon runs on: it never needs a stream to know itself. */
  self: boolean;
  /** Whether this viewer's window holds a record or a beat from the machine. */
  seen: boolean;
  /** The view's (or presence's) answer to whether the machine is up. */
  standing: "online" | "offline" | "unknown";
}

export function machineAvailability(f: AvailabilityFacts): MachineAvailability {
  if (f.self) return "known";
  // A machine that is down is not misconfigured: its status already says so,
  // so it never gets the "check its flow stream" warning, seen or not.
  if (f.standing === "offline") return "not_reporting";
  return f.seen ? "known" : "not_streamed";
}

/** The one word a per-machine surface shows in place of a figure or an
 *  "idle" it cannot back. */
export const NOT_REPORTED = "not reported";

/** (5.0 R3) The status a run reads when it is recorded as running on a machine
 *  that is not reporting: nothing says it is still running, and nothing says
 *  it stopped. The wire status is untouched; this is what THIS viewer can claim.
 *  Its own key, distinct from the run status `unparseable`; `runStatusWord` words it. */
export const NOT_REPORTING_STATUS = "not_reporting";
export const NOT_REPORTING_TITLE = "The machine this ran on is not reporting, so whether it is still running is unknown.";

/** Whether a surface must hold back its idle / zero / quiet claims. */
export const isUnseen = (a: MachineAvailability | undefined): boolean => a !== undefined && a !== "known";

/** What a page about `uid` can see of it, judged only once the flow window has
 *  answered and the page knows whose machine it is (until then `known`: the
 *  pending forms already say "checking…"). `self`: the page is about the
 *  machine serving it. */
export function windowAvailability(
  data: NormRecord[],
  liveMachines: Map<string, PresenceBeat>,
  uid: string | null,
  opts: { self: boolean; answered: boolean },
): MachineAvailability {
  if (!opts.answered || uid === null) return "known";
  return machineAvailability({ self: opts.self, seen: windowHoldsMachine(data, liveMachines, uid), standing: "unknown" });
}

/** A remote machine's live-load line when it has no samples: the page says
 *  what it cannot see before it says "idle". `answered`: the window and the
 *  page's identity are both settled. */
export function remoteIdleLine(a: MachineAvailability, answered: boolean): string {
  if (isUnseen(a)) return `${NOT_REPORTED} · no records from this machine reach this viewer`;
  return answered ? "idle · no samples in the last 10 min" : "checking…";
}
