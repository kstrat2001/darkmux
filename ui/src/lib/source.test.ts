import { describe, it, expect, afterEach } from "vitest";
import { getSource, runsSrc, labRunsSrc } from "./source";

function injectMeta(name: string, content: string) {
  const el = document.createElement("meta");
  el.setAttribute("name", name);
  el.setAttribute("content", content);
  document.head.appendChild(el);
}

/** (#2086) The one place the build type is decided. */
describe("getSource", () => {
  afterEach(() => {
    document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((m) => m.remove());
  });

  it("is the daemon with no flow-src meta, and the daemon endpoints for runs", () => {
    expect(getSource()).toEqual({ kind: "daemon", flow: null, date: null, graphs: null, machine: null, panels: null, fleet: null, runs: "/runs", labRuns: "/lab/runs" });
    expect(runsSrc()).toBe("/runs");
    expect(labRunsSrc()).toBe("/lab/runs");
  });

  it("is static with the flow-src meta, carrying every fixture it names and daemon-route fallbacks for the rest", () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    injectMeta("darkmux-flow-date", "2026-08-26");
    injectMeta("darkmux-graphs-src", "./demo-graphs.json");
    injectMeta("darkmux-runs-src", "./demo-runs.json");
    expect(getSource()).toEqual({
      kind: "static",
      flow: "./demo-flow.jsonl",
      date: "2026-08-26",
      graphs: "./demo-graphs.json",
      machine: null,
      panels: null,
      fleet: null,
      runs: "./demo-runs.json",
      labRuns: "/lab/runs",
    });
    expect(runsSrc()).toBe("./demo-runs.json");
  });

  it("rejects a malformed flow-date rather than naming a day that is not one", () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    injectMeta("darkmux-flow-date", "yesterday");
    const s = getSource();
    expect(s.kind).toBe("static");
    expect(s.date).toBeNull();
  });

  it("a single fixture without a flow file is honored on its own — a harness page may ship just one", () => {
    injectMeta("darkmux-panels-src", "./demo-panels.json");
    injectMeta("darkmux-runs-src", "./demo-runs.json");
    const s = getSource();
    expect(s.kind).toBe("daemon");
    expect(s.panels).toBe("./demo-panels.json");
    expect(runsSrc()).toBe("./demo-runs.json");
  });

  it("is read fresh: removing the meta flips the answer without any reset", () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    expect(getSource().kind).toBe("static");
    document.head.querySelector('meta[name="darkmux-flow-src"]')!.remove();
    expect(getSource().kind).toBe("daemon");
  });
});
