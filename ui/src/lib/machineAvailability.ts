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
 * - `not_reporting`: records from it once arrived and it is now down or
 *   silent. Its last readings are history.
 *
 * Unknown is never shown as a confident fact: a surface that gets anything
 * but `known` shows an explicit "not reported" state, never "idle", "0" or
 * a quiet strip.
 */
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
  if (!f.seen) return "not_streamed";
  return f.standing === "offline" ? "not_reporting" : "known";
}

/** The tooltip a warning mark carries; `null` for `known`, which warns of nothing. */
export function availabilityWarning(a: MachineAvailability): string | null {
  switch (a) {
    case "known":
      return null;
    case "not_streamed":
      return "This machine's records do not reach this viewer, so its activity cannot show here. Check that its flow stream points at this fleet's hub.";
    case "not_reporting":
      return "This machine stopped reporting. What is shown is its last known state.";
    default: {
      const unhandled: never = a;
      return unhandled;
    }
  }
}

/** The one word a per-machine surface shows in place of a figure or an
 *  "idle" it cannot back. */
export const NOT_REPORTED = "not reported";
