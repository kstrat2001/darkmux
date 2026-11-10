/**
 * (#3450) The servers darkmux starts (Tensorfold, rec's Flint) as a viewer
 * surface words them: darkmux's own residents, beside its LM Studio loads.
 * Which server is darkmux's is decided by the daemon (its record matching
 * the live process); these words never decide it again.
 */

import type { EngineServer } from "../types/generated/EngineServer";

/** What a server is doing, in the words every surface uses: "starting the
 * model" while it does not answer yet, else "resident". */
export function serverStateWord(s: Pick<EngineServer, "answering">): "starting the model" | "resident" {
  return s.answering ? "resident" : "starting the model";
}

/** One server, as a card's resident line names it: its model, and its
 * engine and endpoint. */
export function serverLabel(s: Pick<EngineServer, "model" | "engine" | "endpoint">): string {
  return `${s.model} (${s.engine} on ${s.endpoint})`;
}

/** A model id's last path segment (`TensorFold/Qwen3.8-27B` → `Qwen3.8-27B`):
 * a card is narrow, and the full id is in the line's title. */
function shortModel(model: string): string {
  return model.slice(model.lastIndexOf("/") + 1);
}

/** The fleet card's line for a machine's servers: the counts first, which a
 * narrow card's ellipsis cannot cut off ("2 servers · 1 starting"), then the
 * models by short name, the ones still starting first. Empty when the
 * machine runs none (or its card is from a darkmux that lists none). */
export function serversLine(servers: readonly EngineServer[] | null | undefined): string {
  const all = servers ?? [];
  if (all.length === 0) return "";
  const starting = all.filter((s) => !s.answering);
  const names = [...starting, ...all.filter((s) => s.answering)].map((s) => shortModel(s.model));
  const counts = [`${all.length} ${all.length === 1 ? "server" : "servers"}`];
  if (starting.length > 0) counts.push(`${starting.length} starting`);
  return [...counts, names.join(", ")].join(" · ");
}

/** The servers line's title: every server in full, with what it is doing. */
export function serversTitle(servers: readonly EngineServer[] | null | undefined): string {
  return (servers ?? []).map((s) => `${serverLabel(s)}: ${serverStateWord(s)}`).join("\n");
}
