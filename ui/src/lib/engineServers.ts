/**
 * (#3450) The servers darkmux starts (Tensorfold, rec's Flint) as a viewer
 * surface words them: darkmux's own residents, beside its LM Studio loads.
 * Which server is darkmux's is decided by the daemon (its record matching
 * the live process); these words never decide it again.
 */

import type { EngineServer } from "../types/generated/EngineServer";

/** How long a server's start may run before darkmux gives up and stops it:
 * `darkmux_profiles::engine::server::START_BOUND` (15 minutes), mirrored
 * here because the wire carries the start time, not the bound. */
export const ENGINE_START_BOUND_MS = 15 * 60 * 1000;

type StateWord = "starting the model" | "not answering" | "resident";

/** What a server is doing, in the words every surface uses: "resident" when
 * it answers; "starting the model" when it does not and its start is still
 * inside the start bound; otherwise "not answering" (a resident too busy to
 * answer within the listing's bound, or one that stopped answering). */
export function serverStateWord(s: Pick<EngineServer, "answering" | "started_at_ms">, nowMs: number): StateWord {
  if (s.answering) return "resident";
  return nowMs - s.started_at_ms < ENGINE_START_BOUND_MS ? "starting the model" : "not answering";
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
 * models by short name, the ones still starting first and the ones not
 * answering next. Empty when the machine runs none (or its card is from a
 * darkmux that lists none). */
export function serversLine(servers: readonly EngineServer[] | null | undefined, nowMs: number): string {
  const all = servers ?? [];
  if (all.length === 0) return "";
  const of = (word: StateWord) => all.filter((s) => serverStateWord(s, nowMs) === word);
  const starting = of("starting the model");
  const silent = of("not answering");
  const names = [...starting, ...silent, ...of("resident")].map((s) => shortModel(s.model));
  const counts = [`${all.length} ${all.length === 1 ? "server" : "servers"}`];
  if (starting.length > 0) counts.push(`${starting.length} starting`);
  if (silent.length > 0) counts.push(`${silent.length} not answering`);
  return [...counts, names.join(", ")].join(" · ");
}

/** The servers line's title: every server in full, with what it is doing. */
export function serversTitle(servers: readonly EngineServer[] | null | undefined, nowMs: number): string {
  return (servers ?? []).map((s) => `${serverLabel(s)}: ${serverStateWord(s, nowMs)}`).join("\n");
}
