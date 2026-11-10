import { describe, expect, it } from "vitest";
import type { EngineServer } from "../types/generated/EngineServer";
import { ENGINE_START_BOUND_MS, serverLabel, serverStateWord, serversLine, serversTitle } from "./engineServers";

function server(over: Partial<EngineServer> = {}): EngineServer {
  return {
    endpoint: "tensorfold",
    engine: "tensorfold",
    model: "TensorFold/Qwen3.8-27B",
    served_name: "darkmux:TensorFold/Qwen3.8-27B",
    n_ctx: 262144,
    port: 8091,
    pid: 4242,
    started_at_ms: 0,
    answering: true,
    ...over,
  };
}

describe("engine server words (#3450)", () => {
  it("names a server by its model, engine and endpoint", () => {
    expect(serverLabel(server())).toBe("TensorFold/Qwen3.8-27B (tensorfold on tensorfold)");
  });

  // The clock is a parameter: every case states how long ago the server
  // started, never inherits it from when the suite runs.
  const NOW = 1_800_000_000_000;

  it("says a server that does not answer yet is starting its model, while its start is young", () => {
    expect(serverStateWord(server({ answering: false, started_at_ms: NOW - 60_000 }), NOW)).toBe("starting the model");
    expect(serverStateWord(server({ started_at_ms: NOW - 60_000 }), NOW)).toBe("resident");
  });

  // A resident server too busy to answer within a second was read as
  // "starting the model" hours after it started. Past the start bound no
  // start is still running (darkmux gives up and stops it), so the honest
  // word is that it does not answer.
  it("says 'not answering', not 'starting', once the start bound has passed", () => {
    expect(serverStateWord(server({ answering: false, started_at_ms: NOW - ENGINE_START_BOUND_MS - 1 }), NOW)).toBe("not answering");
    expect(serverStateWord(server({ answering: false, started_at_ms: NOW - ENGINE_START_BOUND_MS + 1 }), NOW)).toBe("starting the model");
    expect(ENGINE_START_BOUND_MS).toBe(15 * 60 * 1000);
  });

  it("leads with counts a narrow card cannot cut off, then the names, starting ones first", () => {
    const flint = server({ endpoint: "flint", engine: "flint", model: "flint-int4", served_name: "flint", answering: false, started_at_ms: NOW - 1_000 });
    expect(serversLine([server(), flint], NOW)).toBe("2 servers · 1 starting · flint-int4, Qwen3.8-27B");
    expect(serversLine([server()], NOW)).toBe("1 server · Qwen3.8-27B");
    expect(serversLine([], NOW)).toBe("");
    expect(serversLine(undefined, NOW)).toBe("");
    expect(serversTitle([server(), flint], NOW)).toBe(
      "TensorFold/Qwen3.8-27B (tensorfold on tensorfold): resident\nflint-int4 (flint on flint): starting the model",
    );
  });

  it("counts a busy resident as not answering, never as starting", () => {
    const busy = server({ answering: false, started_at_ms: NOW - 3 * 3600_000 });
    expect(serversLine([busy], NOW)).toBe("1 server · 1 not answering · Qwen3.8-27B");
    expect(serversTitle([busy], NOW)).toBe("TensorFold/Qwen3.8-27B (tensorfold on tensorfold): not answering");
  });
});
