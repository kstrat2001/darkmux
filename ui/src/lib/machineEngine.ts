/** (#316 stage 4.5) What a machine's specs say about its engine: the one
 *  reading every machine surface renders (the fleet card, the machine page,
 *  Machine info), so none of them re-derives it. */

import type { MachineEngine } from "../types/generated/MachineEngine";
import type { MachineSpecsResponse } from "../types/generated/MachineSpecsResponse";

const LMSTUDIO = "lmstudio";

/** The machine's engine. A card from a darkmux before 4.5 states none; its
 *  engine was LM Studio. */
export function engineOf(specs: Pick<MachineSpecsResponse, "engine">): MachineEngine {
  return specs.engine ?? { name: LMSTUDIO, reports_residency: true };
}

/** The engine as a person names it: "LM Studio", else its endpoint id. */
export function engineLabel(engine: MachineEngine): string {
  return engine.name === LMSTUDIO ? "LM Studio" : engine.name;
}

/** Whether the machine's engine says what it holds; with no specs, nothing
 *  is claimed either way, so `true` (a reading stands as the card states it). */
export function reportsResidency(specs: Pick<MachineSpecsResponse, "engine"> | null | undefined): boolean {
  return !specs || engineOf(specs).reports_residency;
}

/** The hardware line's engine part: ` · <endpoint id>` for an engine that is
 *  not LM Studio, nothing for LM Studio (the default, and every machine
 *  before 4.5), so a card names an engine only when it says something. */
export function engineSuffix(engine: MachineEngine): string {
  return engine.name === LMSTUDIO ? "" : ` · ${engineLabel(engine)}`;
}

/** Why the machine's resident list is empty for a reason other than "nothing
 *  is loaded", or `null` when an empty list means exactly that. */
export function residentsUnknownNote(specs: Pick<MachineSpecsResponse, "engine" | "lms_unreachable"> | null): string | null {
  if (specs === null) return null;
  const engine = engineOf(specs);
  if (specs.lms_unreachable) {
    return engine.name === LMSTUDIO
      ? "LM Studio could not be asked (`lms ps` failed); what it holds is unknown."
      : `the default engine \`${engine.name}\` cannot be used; \`darkmux doctor\` names why.`;
  }
  if (!engine.reports_residency) return `${engineLabel(engine)} does not report which models it has loaded.`;
  return null;
}

/** Machine info's engine row: the name, and what it does not report; empty
 *  with no specs (the row then hides). */
export function engineRow(specs: Pick<MachineSpecsResponse, "engine"> | null | undefined): string {
  if (!specs) return "";
  const engine = engineOf(specs);
  return engine.reports_residency ? engineLabel(engine) : `${engineLabel(engine)} · does not report what it holds`;
}
