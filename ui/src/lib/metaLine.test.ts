import { describe, expect, it } from "vitest";
import { computeMetaLines, readyParts } from "./metaLine";
import type { NormRecord } from "./ingest";
import { norm } from "../testing/records";

const T0 = Date.parse("2026-09-26T10:00:00Z");
const beat = new Map([["u1", { machine_id: "box" } as never]]);
const start = (action: string): NormRecord => norm({ ts: new Date(T0).toISOString(), action, session_id: "s1", machine_uid: "u1" });

describe("meta line: last dispatch, dated from its START", () => {
  it("the idle headline and the ready parts agree", () => {
    const nowMs = T0 + 120_000;
    const ago = readyParts([start("dispatch.start")], beat, nowMs)?.ago;
    expect(ago).toMatch(/\S/);
    expect(computeMetaLines([start("dispatch.start")], beat, nowMs)).toEqual([`1  · last dispatch ${ago}`]);
  });

  it("no start: no 'last dispatch' suffix; no machine: the waiting line", () => {
    expect(computeMetaLines([], beat, T0)).toEqual(["1 "]);
    expect(readyParts([], beat, T0)?.ago).toBe("");
    expect(computeMetaLines([start("dispatch.start")], new Map(), T0)).toEqual(["○ waiting for a machine"]);
    expect(readyParts([start("dispatch.start")], new Map(), T0)).toBeNull();
  });

  it("a start in the future of `nowMs` is not a last dispatch", () => {
    expect(computeMetaLines([start("dispatch.start")], beat, T0 - 1)).toEqual(["1 "]);
  });

  // (5.0 R3) This machine is never unknown: with presence empty (fleet off, no
  // Redis) it still counts itself rather than "waiting for a machine".
  it("counts this machine when presence is empty and the daemon has named itself", () => {
    const recs = [start("dispatch.start")];
    expect(readyParts(recs, new Map(), T0 + 1000, "U1")?.n).toBe(1);
    expect(computeMetaLines(recs, new Map(), T0 + 1000, "U1")[0]).toMatch(/^1 /);
  });

  it("this machine and its own beat are one machine, whatever case the uid is spelled in", () => {
    expect(readyParts([], beat, T0, "U1")?.n).toBe(1);
  });

  it("a peer's beat plus this machine is two", () => {
    expect(readyParts([], new Map([["peer", { machine_id: "p" } as never]]), T0, "u1")?.n).toBe(2);
  });
});
