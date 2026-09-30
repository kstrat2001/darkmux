/**
 * The fleet lens's reading of `GET /fleet/view`: one machine row in, the
 * facts a card shows out. Every closed set the view sends (card outcome,
 * unreachable reason, unavailable reason, liveness, grant) is matched
 * exhaustively here, once, so a card never guesses what a row means.
 *
 * Nothing in this file renders a node address, a machine uid, a token or the
 * peer's own `detail` sentence: an outcome line is a fixed phrase per variant.
 */

import type { AcceptsState } from "../../types/generated/AcceptsState";
import type { CardAccepts } from "../../types/generated/CardAccepts";
import type { CardOutcome } from "../../types/generated/CardOutcome";
import type { FleetMachine } from "../../types/generated/FleetMachine";
import type { Liveness } from "../../types/generated/Liveness";
import type { MachineSpecsResponse } from "../../types/generated/MachineSpecsResponse";
import type { UnavailableWhy } from "../../types/generated/UnavailableWhy";
import type { UnreachableReason } from "../../types/generated/UnreachableReason";

/** Whether a machine is up, as the card should say it. `unknown` is a real
 * third answer: presence could not say, and nothing else did either. */
export type Standing = "online" | "offline" | "unknown";

function unreachableLine(reason: UnreachableReason): string {
  switch (reason) {
    case "bad_address":
      return "bad address";
    case "dns_failed":
      return "address not found";
    case "identity_unavailable":
      return "identity unavailable";
    case "not_on_overlay":
      return "not on the overlay network";
    case "pin_mismatch":
      return "identity mismatch";
    case "listener_off":
      return "listener off";
    case "auth_required":
      return "auth required";
    case "refused_by_peer":
      return "refused by peer";
    case "listener_unavailable":
      return "listener unavailable";
    case "bad_answer":
      return "bad answer";
    case "unknown":
      return "unreachable";
    default: {
      const unhandled: never = reason;
      return unhandled;
    }
  }
}

function unavailableLine(why: UnavailableWhy): string {
  switch (why) {
    case "no_card_route":
      return "no card (older peer)";
    case "other_schema_major":
      return "card schema differs";
    case "unparseable":
      return "card unreadable";
    case "unknown":
      return "card unavailable";
    default: {
      const unhandled: never = why;
      return unhandled;
    }
  }
}

/** The short status line for a card the view could not read, or `null` for a
 * card it did read. One fixed phrase per variant, `unknown` included. */
export function outcomeLine(outcome: CardOutcome): string | null {
  switch (outcome.state) {
    case "available":
      return null;
    case "unavailable":
      return unavailableLine(outcome.why);
    case "mismatch":
      return "another machine answered";
    case "unreachable":
      return unreachableLine(outcome.reason);
    case "unknown":
      return "card state unknown";
    default: {
      const unhandled: never = outcome;
      return unhandled;
    }
  }
}

/** What a peer lets this machine do, as one compact line; `null` when the
 * view holds no grant to show (this machine's own row, no entry, withheld,
 * unknown). */
export function grantLine(accepts: AcceptsState): string | null {
  switch (accepts.state) {
    case "granted":
      return acceptsLine(accepts.accepts);
    case "not_listed":
    case "this_machine":
    case "withheld":
    case "unknown":
      return null;
    default: {
      const unhandled: never = accepts;
      return unhandled;
    }
  }
}

function acceptsLine(a: CardAccepts): string {
  const parts: string[] = [];
  if (a.profiles.length > 0) parts.push(`runs ${a.profiles.join(", ")}`);
  for (const role of a.roles) parts.push(`${role} here`);
  return parts.length > 0 ? parts.join(" · ") : "accepts nothing";
}

function livenessStanding(liveness: Liveness): Standing {
  switch (liveness) {
    case "live":
      return "online";
    case "no_beat":
      return "offline";
    case "unknown":
      return "unknown";
    default: {
      const unhandled: never = liveness;
      return unhandled;
    }
  }
}

/** A card the view read is proof the machine is up, whatever presence says:
 * a peer with Redis off has no beat and still answers. Otherwise the view's
 * own `liveness` decides; it is not recomputed from any other source. */
export function rowStanding(row: FleetMachine): Standing {
  if (row.is_this_machine || row.card.state === "available") return "online";
  return livenessStanding(row.liveness);
}

/** A machine's hardware line from its specs: chip and RAM. Empty when the
 * specs name no chip. */
export function specsLine(specs: MachineSpecsResponse): string {
  const chip = specs.cpu_brand ?? "";
  if (!chip) return "";
  const gb = specs.ram_total_bytes ? ` · ${Math.round(specs.ram_total_bytes / 1073741824)} GB` : "";
  return chip + gb;
}

/** The card's own specs, when the view read a card. */
export function rowSpecs(row: FleetMachine): MachineSpecsResponse | null {
  return row.card.state === "available" ? row.card.card.specs : null;
}

export interface RowFacts {
  /** The identity the rest of the lens keys a card by (see `rowUid`). */
  uid: string;
  /** Whether the page already knows this uid from flow, presence or itself,
   * so its name comes from the same alias rules every other surface uses. */
  known: boolean;
  /** The machine's own name for itself when the view has one. */
  name: string | null;
  spec: string;
  /** Why there is no hardware line, as the outcome line; `null` when the
   * card was read. */
  note: string | null;
  /** What the peer lets this machine do; `null` for this machine's own row. */
  grant: string | null;
  standing: Standing;
  isSelf: boolean;
}

/** The uid a card is keyed by. This machine's own row, when the daemon
 * reports no hardware uid, takes `selfUid`: the flow uid the page recognizes
 * as this machine. A machine the page already knows by its
 * hardware uid (flow, presence, itself) keeps that uid; one only the view
 * knows is keyed by its roster id, the same key `lib/machineKey.ts` gives a
 * roster-only machine, so the card's link resolves. */
export function rowUid(row: FleetMachine, knownUids: ReadonlySet<string>, selfUid: string | null): string {
  const uid = row.machine_uid ?? (row.is_this_machine ? selfUid : null);
  if (uid && knownUids.has(uid)) return uid;
  return row.entry?.id ?? uid ?? "unknown";
}

export function rowFacts(row: FleetMachine, knownUids: ReadonlySet<string>, selfUid: string | null): RowFacts {
  const specs = rowSpecs(row);
  const uid = rowUid(row, knownUids, selfUid);
  return {
    uid,
    known: knownUids.has(uid),
    name: row.entry?.id ?? specs?.machine_id ?? null,
    spec: specs ? specsLine(specs) : "",
    note: outcomeLine(row.card),
    grant: row.is_this_machine ? null : grantLine(row.accepts),
    standing: rowStanding(row),
    isSelf: row.is_this_machine,
  };
}
