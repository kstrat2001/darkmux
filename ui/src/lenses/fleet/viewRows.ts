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
import { findUid, nameKey, sameUid } from "../../lib/machineIdentity";

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
    case "pin_not_saved":
      return "pin not saved";
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

/** The role a peer grants when it answers this machine's radio questions. */
const RADIO_HOST_ROLE = "radio-host";

/** What a peer lets this machine do: its profiles and roles as one compact
 * line, with the radio seat pulled out as a flag so the card draws it as an
 * icon. `text` is `null` when the radio seat is the whole grant. */
export interface Grant {
  text: string | null;
  radio: boolean;
}

/** The grant the view holds for a peer; `null` when there is none to show
 * (this machine's own row, no entry, unknown). */
export function grantOf(accepts: AcceptsState): Grant | null {
  switch (accepts.state) {
    case "granted":
      return grantFrom(accepts.accepts);
    case "not_listed":
    case "this_machine":
    case "unknown":
      return null;
    default: {
      const unhandled: never = accepts;
      return unhandled;
    }
  }
}

function grantFrom(a: CardAccepts): Grant {
  const radio = a.roles.includes(RADIO_HOST_ROLE);
  const parts: string[] = [];
  if (a.profiles.length > 0) parts.push(`runs ${a.profiles.join(", ")}`);
  for (const role of a.roles) if (role !== RADIO_HOST_ROLE) parts.push(`${role} here`);
  if (parts.length > 0) return { text: parts.join(" · "), radio };
  return { text: radio ? null : "accepts nothing", radio };
}

/** The grant in words, the radio seat spelled out: the line's tooltip. */
export function grantWords(g: Grant): string {
  const radio = g.radio ? `${RADIO_HOST_ROLE} here` : null;
  return [g.text, radio].filter((p): p is string => p !== null).join(" · ");
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

/** Whether the row's own card declares `fleet.mode hub`. A machine whose card
 * was not read declares nothing the view can show. */
export function rowIsHub(row: FleetMachine): boolean {
  return row.card.state === "available" && row.card.card.fleet_mode === "hub";
}

/** Whether the machine a page shows is one the view's rows say declares
 * `hub`. `isLocal`: the page shows THIS machine, whose row is the view's own.
 * Any other machine is found by the hardware uid its card carries or by its
 * roster id, the two keys a machine page is reached by. */
export function machineIsHub(rows: readonly FleetMachine[] | null, targetUid: string | null, isLocal: boolean): boolean {
  return (rows ?? []).some(
    (row) =>
      rowIsHub(row) &&
      (row.is_this_machine
        ? isLocal
        : targetUid !== null && (sameUid(row.machine_uid, targetUid) || (!!row.entry && nameKey(row.entry.id) === nameKey(targetUid)))),
  );
}

export interface RowFacts {
  /** The identity the rest of the lens keys a card by (see `rowUid`). */
  uid: string;
  /** Whether the page already knows this uid from flow, presence or itself,
   * so its name comes from the same alias rules every other surface uses. */
  known: boolean;
  /** The machine's own current name when the view read its card, else the
   * roster id. */
  name: string | null;
  /** Every name the row's machine answers to: its own current name (the card)
   * and the roster id, so a run recorded under either one still matches. */
  names: string[];
  spec: string;
  /** Why there is no hardware line, as the outcome line; `null` when the
   * card was read. */
  note: string | null;
  /** What the peer lets this machine do; `null` for this machine's own row. */
  grant: Grant | null;
  standing: Standing;
  /** The view's own answer on whether the machine's presence beat is live. */
  liveness: Liveness;
  isSelf: boolean;
  /** The row's card declares `fleet.mode hub`. */
  hub: boolean;
}

/** Whether this viewer is receiving the row's machine. A view whose presence
 * says the beat stopped (`no_beat`) outranks the window: a day-old record
 * still sitting in it is history, not a stream. The window answers only when
 * presence could not. */
export function rowSeen(row: Pick<RowFacts, "known" | "liveness">): boolean {
  return row.liveness !== "no_beat" && row.known;
}

/** The uid a card is keyed by. This machine's own row, when the daemon
 * reports no hardware uid, takes `selfUid`: the flow uid the page recognizes
 * as this machine. A machine the page already knows by its
 * hardware uid (flow, presence, itself) keeps that uid; one only the view
 * knows is keyed by its roster id, the same key `lib/machineKey.ts` gives a
 * roster-only machine, so the card's link resolves. */
export function rowUid(
  row: FleetMachine,
  knownUids: ReadonlySet<string>,
  selfUid: string | null,
  flowUidOfName: (name: string) => string | null = () => null,
): string {
  // A row whose card was not read has no uid of its own; the one flow machine
  // that goes by its roster id is the same machine (`uidForName`).
  const named = row.entry ? flowUidOfName(row.entry.id) : null;
  const uid = row.machine_uid ?? (row.is_this_machine ? selfUid : null) ?? named ?? null;
  // A uid the page already knows, in the form the page spells it.
  const known = findUid(knownUids, uid);
  if (known !== null) return known;
  return row.entry?.id ?? uid ?? "unknown";
}

export function rowFacts(
  row: FleetMachine,
  knownUids: ReadonlySet<string>,
  selfUid: string | null,
  flowUidOfName: (name: string) => string | null = () => null,
): RowFacts {
  const specs = rowSpecs(row);
  const uid = rowUid(row, knownUids, selfUid, flowUidOfName);
  return {
    uid,
    known: findUid(knownUids, uid) !== null,
    // (#3028) A machine renames itself; the roster id is the label its
    // operator wrote. The card the view read is the machine speaking.
    name: specs?.machine_id ?? row.entry?.id ?? null,
    names: [...new Set([specs?.machine_id, row.entry?.id].filter((n): n is string => !!n))],
    spec: specs ? specsLine(specs) : "",
    note: outcomeLine(row.card),
    grant: row.is_this_machine ? null : grantOf(row.accepts),
    standing: rowStanding(row),
    liveness: row.liveness,
    isSelf: row.is_this_machine,
    hub: rowIsHub(row),
  };
}
