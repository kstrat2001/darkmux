import { describe, expect, test } from "vitest";
import type { FlowRecord, MachineResourcesModel, MachineSpecs } from "../../types/handwritten";
import { utilitySectionView } from "./utilitySectionView";

const U = "m1";
const at = (s: number) => new Date(Date.UTC(2026, 8, 27, 9, 0, s)).toISOString();
const ms = (s: number) => Date.parse(at(s));
const specs = (over: Partial<MachineSpecs["utility_model"]> | null): MachineSpecs =>
  ({ machine_id: "studio", machine_uid: U, utility_model: over === null ? null : { id: "darkmux:util-4b", loaded: true, n_ctx: 120000, ...over } }) as unknown as MachineSpecs;
const usage = (s: number, job: string | null, tokens: number, extra: Record<string, unknown> = {}) =>
  ({ ts: at(s), action: "telemetry.tokens", category: "telemetry", source: "tokens", machine_uid: U, payload: { purpose: "utility", call_kind: job === "compaction" ? "compaction" : "single_shot", ...(job ? { job } : {}), total_tokens: tokens, requested_model: "darkmux:util-4b" }, ...extra }) as unknown as FlowRecord;
const start = (s: number, job: string) =>
  ({ ts: at(s), action: "utility.start", machine_uid: U, payload: { job, model: "darkmux:util-4b", stall_after_seconds: 30 } }) as unknown as FlowRecord;
const row = { current_bytes: 2 ** 30 * 14.61 } as unknown as MachineResourcesModel;

describe("(#2915) the machine page's Utility section", () => {
  test("this machine: model, declared window, residency with its footprint, idle", () => {
    const v = utilitySectionView({ data: [], uid: U, nowMs: ms(60), specs: specs({}), isLocal: true, residentRow: row });
    expect(v.modelLine).toBe("darkmux:util-4b");
    expect(v.factsLine).toBe("window 120,000 · resident · 14.61 GiB");
    expect(v.liveLine).toBe("idle");
  });

  test("not loaded, and an undeclared window, are said plainly", () => {
    const v = utilitySectionView({ data: [], uid: U, nowMs: ms(60), specs: specs({ loaded: false, n_ctx: null }), isLocal: true, residentRow: null });
    expect(v.factsLine).toBe("window — · not loaded");
  });

  test("another machine: its model from its own records, residency unknown", () => {
    const v = utilitySectionView({ data: [usage(1, "radio_routing", 40)], uid: U, nowMs: ms(60), specs: null, isLocal: false, residentRow: null });
    expect(v.modelLine).toBe("darkmux:util-4b");
    expect(v.factsLine).toBe("window — · residency unknown (another machine)");
  });

  test("the live job counts its seconds, and reads stalled past its bound", () => {
    expect(utilitySectionView({ data: [start(0, "radio_routing")], uid: U, nowMs: ms(3), specs: specs({}), isLocal: true, residentRow: null }).liveLine).toBe("radio routing · 3s");
    expect(utilitySectionView({ data: [start(0, "radio_routing")], uid: U, nowMs: ms(40), specs: specs({}), isLocal: true, residentRow: null }).liveLine).toBe("radio routing · stalled");
  });

  test("each job's calls and tokens, every known job listed even at zero, an unknown job after them", () => {
    const v = utilitySectionView({
      data: [usage(1, "radio_routing", 40), usage(2, "radio_routing", 60), usage(3, "dream_job", 5), usage(4, "radio_routing", 999, { machine_uid: "other" })],
      uid: U,
      nowMs: ms(60),
      specs: specs({}),
      isLocal: true,
      residentRow: null,
    });
    expect(v.jobs).toEqual([
      { word: "compacting", calls: "0 calls", tokens: "0 tokens", known: true },
      { word: "radio routing", calls: "2 calls", tokens: "100 tokens", known: true },
      { word: "other", calls: "1 call", tokens: "5 tokens", known: false },
    ]);
  });

  test("(#2915 review, C7) a fixed set of rows: unknown and unnamed jobs fold into ONE 'other' row, always present", () => {
    const rows = (data: FlowRecord[]) => utilitySectionView({ data, uid: U, nowMs: ms(60), specs: specs({}), isLocal: true, residentRow: null }).jobs;
    const none = rows([]);
    const many = rows([usage(1, "dream_job", 5), usage(2, "nightmare_job", 6), usage(3, null, 7)]);
    expect(none.map((r) => r.word)).toEqual(["compacting", "radio routing", "other"]);
    expect(many.map((r) => r.word)).toEqual(["compacting", "radio routing", "other"]);
    expect(many[2]).toMatchObject({ calls: "3 calls", tokens: "18 tokens" });
    expect(none[2]).toMatchObject({ calls: "0 calls", tokens: "0 tokens" });
  });
});
