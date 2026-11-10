import { describe, expect, it } from "vitest";
import type { EngineServer } from "../types/generated/EngineServer";
import { serverLabel, serverStateWord, serversLine, serversTitle } from "./engineServers";

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

  it("says a server that does not answer yet is starting its model", () => {
    expect(serverStateWord(server({ answering: false }))).toBe("starting the model");
    expect(serverStateWord(server())).toBe("resident");
  });

  it("leads with counts a narrow card cannot cut off, then the names, starting ones first", () => {
    const flint = server({ endpoint: "flint", engine: "flint", model: "flint-int4", served_name: "flint", answering: false });
    expect(serversLine([server(), flint])).toBe("2 servers · 1 starting · flint-int4, Qwen3.8-27B");
    expect(serversLine([server()])).toBe("1 server · Qwen3.8-27B");
    expect(serversLine([])).toBe("");
    expect(serversLine(undefined)).toBe("");
    expect(serversTitle([server(), flint])).toBe(
      "TensorFold/Qwen3.8-27B (tensorfold on tensorfold): resident\nflint-int4 (flint on flint): starting the model",
    );
  });
});
