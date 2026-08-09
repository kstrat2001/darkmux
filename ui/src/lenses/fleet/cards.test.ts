import { describe, it, expect } from "vitest";
import { machActive, specOf, buildFleetCard } from "./cards";
import type { FlowRecord, MachineSpecs, PresenceBeat } from "../../types/handwritten";

function rec(overrides: Partial<FlowRecord>): FlowRecord {
  return { ts: "2026-08-08T00:00:00.000Z", ...overrides };
}

function beat(overrides: Partial<PresenceBeat>): PresenceBeat {
  return { machine_uid: "u1", display_name: "studio", schema_version: "1.18.0", beat_ts_ms: 1, ...overrides };
}

describe("machActive", () => {
  it("is true when a dispatch.start on the machine belongs to a live session", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(["s1"]), "m1")).toBe(true);
  });

  it("is false when the session isn't in the live set", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "m1", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(), "m1")).toBe(false);
  });

  it("is false for a different machine's live session", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "m2", session_id: "s1", action: "dispatch.start" })];
    expect(machActive(data, new Set(["s1"]), "m1")).toBe(false);
  });
});

describe("specOf", () => {
  const specs: MachineSpecs = {
    darkmux_version: "2.5.0",
    flow_schema_version: "1.18.0",
    machine_id: "MacBook-Pro",
    os: "macos",
    ram_total_bytes: 137438953472, // 128 GiB
    ram_free_for_ai_bytes: null,
    cpu_brand: "Apple M5 Max",
    loaded_models: [],
    lms_unreachable: false,
    utility_model: null,
    redis_url_redacted: null,
    generated_at_ms: 0,
  };

  it("prefers the live /machine/specs probe for THIS machine", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "u1", machine_id: "MacBook-Pro" })];
    expect(specOf(data, new Map(), specs, "u1")).toBe("Apple M5 Max · 128 GB");
  });

  it("falls back to the presence beat's own spec string for a remote machine", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "u2", machine_id: "studio" })];
    const live = new Map([["u2", beat({ machine_uid: "u2", display_name: "studio", specs: "M1 Max · 32 GB" })]]);
    expect(specOf(data, live, specs, "u2")).toBe("M1 Max · 32 GB");
  });

  it("returns '' (renders the specdim fallback) for a remote machine with no reported hardware", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "u2", machine_id: "studio" })];
    const live = new Map([["u2", beat({ machine_uid: "u2", display_name: "studio" })]]);
    expect(specOf(data, live, specs, "u2")).toBe("");
  });

  it("the unknown bucket names any claimed-but-unverified machine_ids", () => {
    const data: FlowRecord[] = [rec({ machine_id: "someones-laptop" })]; // no machine_uid -> uidOf() = "unknown"
    expect(specOf(data, new Map(), null, "unknown")).toBe("unverified · claimed: someones-laptop");
  });

  it("the unknown bucket with no claimed names reads 'unidentified'", () => {
    expect(specOf([], new Map(), null, "unknown")).toBe("unidentified (no hardware uid)");
  });
});

describe("buildFleetCard", () => {
  it("an absent machine reads 'offline' regardless of activity", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" })];
    const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), /* machAbsent */ true, "u1");
    expect(card.stat).toBe("offline");
  });

  it("a present machine with a live dispatch reads 'dispatch in flight'", () => {
    const data: FlowRecord[] = [rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" })];
    const card = buildFleetCard(data, new Map(), null, new Set(["s1"]), false, "u1");
    expect(card.stat).toBe("dispatch in flight");
    expect(card.runsCount).toBe(1);
  });

  it("a present machine with no live dispatch reads 'idle', even with completed history", () => {
    const data: FlowRecord[] = [
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.start" }),
      rec({ machine_uid: "u1", session_id: "s1", action: "dispatch.complete" }),
    ];
    const card = buildFleetCard(data, new Map(), null, new Set(), false, "u1");
    expect(card.stat).toBe("idle");
    // runsCount is LIVE-only (this port has no playback), never the day's
    // whole history — a completed dispatch from earlier shouldn't inflate it.
    expect(card.runsCount).toBe(0);
  });
});
