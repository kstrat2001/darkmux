/** (#316 stage 4.5) What a machine's specs say about its engine: the one
 *  reading every machine surface renders (the fleet card, the machine page,
 *  Machine info), so none of them re-derives it. The fact is decided once,
 *  by the daemon: `lms_unreachable` says the resident list is not known, and
 *  `engine` says why. */

import type { MachineEngine } from "../types/generated/MachineEngine";
import type { MachineSpecsResponse } from "../types/generated/MachineSpecsResponse";

const LMSTUDIO = "lmstudio";

type EngineSpecs = Pick<MachineSpecsResponse, "engine">;
type ResidentSpecs = Pick<MachineSpecsResponse, "engine" | "lms_unreachable">;

/** The machine's engine. A card from a darkmux before 4.5 states none; its
 *  engine was LM Studio. */
export function engineOf(specs: EngineSpecs): MachineEngine {
  return specs.engine ?? { name: LMSTUDIO, reports_residency: true, usable: true };
}

/** The engine as a person names it: "LM Studio", else its endpoint id. */
export function engineLabel(engine: MachineEngine): string {
  return engine.name === LMSTUDIO ? "LM Studio" : engine.name;
}

/** The hardware line's engine part: ` · <endpoint id>` for an engine that is
 *  not LM Studio, nothing for LM Studio (the default, and every machine
 *  before 4.5), so a card names an engine only when it says something. */
export function engineSuffix(engine: MachineEngine): string {
  return engine.name === LMSTUDIO ? "" : ` · ${engineLabel(engine)}`;
}

/** Whether the machine's resident list is its engine's answer; with no specs
 *  nothing is claimed either way, so `true` (a reading stands as stated). */
export function residentsKnown(specs: ResidentSpecs | null | undefined): boolean {
  return !specs || !specs.lms_unreachable;
}

/** Why the machine's resident list is not known, or `null` when an empty list
 *  means nothing is loaded. */
export function residentsUnknownNote(specs: ResidentSpecs | null): string | null {
  if (residentsKnown(specs)) return null;
  const engine = engineOf(specs as ResidentSpecs);
  if (!engine.usable) return `the default engine \`${engine.name}\` cannot be used; \`darkmux doctor\` names why.`;
  if (!engine.reports_residency) return `${engineLabel(engine)} does not report which models it has loaded.`;
  if (engine.name !== LMSTUDIO) return `${engine.name} could not be asked; what it holds is unknown.`;
  return "LM Studio could not be asked (`lms ps` failed); what it holds is unknown.";
}

/** (#3554) What a machine holds for darkmux, as its fleet card counts it. */
export type CardResidents = { known: true; count: number } | { known: false; why: string };

/** darkmux's namespace on a resident's identifier (contract 4): the test
 *  `darkmux_profiles::ownership::is_darkmux_owned` applies. */
const DARKMUX_NAMESPACE = "darkmux:";

/** (#3554) How many models the machine holds for darkmux: the residents in
 *  darkmux's namespace, never a model another app loaded (the fleet card is a
 *  darkmux card, not an LM Studio one); or why the list is not known, so a
 *  card never says "0 loaded" about a list nobody read. `null` with no specs:
 *  no card was read, so nothing is claimed. */
export function darkmuxResidents(specs: (ResidentSpecs & Pick<MachineSpecsResponse, "loaded_models">) | null): CardResidents | null {
  if (!specs) return null;
  const why = residentsUnknownNote(specs);
  if (why !== null) return { known: false, why };
  // A peer's card is read off the wire: one that leaves the list out claims
  // nothing, rather than "0 loaded" or taking the fleet page down with it.
  const listed: unknown = specs.loaded_models;
  if (!Array.isArray(listed)) return null;
  return { known: true, count: specs.loaded_models.filter((m) => m.identifier.startsWith(DARKMUX_NAMESPACE)).length };
}

/** Machine info's engine row: the name, and what it does not report or that
 *  it cannot be used; empty with no specs (the row then hides). */
export function engineRow(specs: EngineSpecs | null | undefined): string {
  if (!specs) return "";
  const engine = engineOf(specs);
  if (!engine.usable) return `${engine.name} · cannot be used (darkmux doctor names why)`;
  return engine.reports_residency ? engineLabel(engine) : `${engineLabel(engine)} · does not report what it holds`;
}
