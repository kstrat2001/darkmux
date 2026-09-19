import type { FlowRecord, PresenceBeat, RosterMachineEntry } from "../types/handwritten";

/**
 * (#2818) FLEET SCENARIOS AS FIXTURES.
 *
 * A real fleet can only be in one configuration at a time. The states that
 * produce defects are COMBINATIONS — a renamed machine whose alias has aged
 * out, a peer declared but never seen, two daemons on different versions
 * sharing one flow stream — and several of them cannot be produced on demand
 * at all. #2796's phantom card needs three months of retention drift to
 * appear; a fixture reproduces it in 700ms.
 *
 * This module is the gating increment for the RELEASE, not the general
 * builder the issue describes. Cutting the release puts this fleet into a
 * version-skew state nothing has ever tested: studio upgrades by `brew` to
 * meet a machine already running newer code, against one shared Redis hub and
 * one shared flow stream. The test and the launch would otherwise be the same
 * event.
 *
 * Deliberately NOT here yet: run directories, the `/runs` payload, and a
 * definition shared with the Rust side. Those are the right end state and
 * none of them is needed to cover the upgrade.
 */

export interface ScenarioMachine {
  uid: string;
  /** What the machine calls itself NOW. */
  name: string;
  darkmuxVersion: string;
  schemaVersion: string;
  /** Is a presence key live for this machine? `false` models a peer that is
   *  off, or mid-upgrade with its daemon restarting. */
  beating?: boolean;
  /** Names this uid used EARLIER and that are still in the flow window. An
   *  empty array models the alias having aged out — which is the state that
   *  turns a stale roster row into a phantom card, and the one nobody can
   *  reproduce on demand. */
  formerNames?: string[];
  /** How many records this machine has in the window. Zero models a machine
   *  that is beating but has done no work. */
  records?: number;
}

export interface ScenarioSpec {
  machines: ScenarioMachine[];
  /** Roster rows as the operator declared them. Omit `machine_uid` to model
   *  an entry written before uids were recorded — the shape that has to be
   *  joined by name. */
  roster?: Array<{ id: string; machine_uid?: string; address?: string }>;
  /** Anchors the generated timestamps. */
  nowMs?: number;
}

export interface Scenario {
  data: FlowRecord[];
  liveMachines: Map<string, PresenceBeat>;
  roster: RosterMachineEntry[];
  nowMs: number;
}

function iso(ms: number): string {
  return new Date(ms).toISOString();
}

export function buildScenario(spec: ScenarioSpec): Scenario {
  const nowMs = spec.nowMs ?? Date.UTC(2026, 8, 19, 12, 0, 0);
  const data: FlowRecord[] = [];
  const liveMachines = new Map<string, PresenceBeat>();

  spec.machines.forEach((m, mi) => {
    // Former names land OLDER than the current one, so "most recent name
    // wins" (`nameOf`, #2030) has something real to resolve against rather
    // than depending on array order.
    (m.formerNames ?? []).forEach((old, i) => {
      data.push({
        ts: iso(nowMs - (90 - i) * 86_400_000),
        machine_uid: m.uid,
        machine_id: old,
        session_id: `${m.uid}-historic-${i}`,
        action: "dispatch.start",
      } as FlowRecord);
    });

    const n = m.records ?? 1;
    for (let i = 0; i < n; i++) {
      data.push({
        ts: iso(nowMs - (n - i) * 60_000),
        machine_uid: m.uid,
        machine_id: m.name,
        session_id: `${m.uid}-s${i}`,
        action: "dispatch.start",
      } as FlowRecord);
    }

    if (m.beating ?? true) {
      liveMachines.set(m.uid, {
        machine_uid: m.uid,
        display_name: m.name,
        schema_version: m.schemaVersion,
        beat_ts_ms: nowMs - mi * 1_000,
      } as PresenceBeat);
    }
  });

  const roster: RosterMachineEntry[] = (spec.roster ?? []).map((r, i) => ({
    id: r.id,
    address: r.address ?? "127.0.0.1:8765",
    added_unix_ms: nowMs - (200 - i) * 86_400_000,
    machine_uid: r.machine_uid,
  })) as RosterMachineEntry[];

  return { data, liveMachines, roster, nowMs };
}

/**
 * THE UPGRADE. The state this release creates on the operator's own fleet:
 * one machine already on the new build, one still on the released one, both
 * writing into a shared flow stream and a shared presence namespace.
 *
 * Measured from the live fleet 2026-09-19 rather than invented — studio was
 * on darkmux 3.7.0 / flow schema 1.42.0 while this machine ran 3.7.1 /
 * 1.51.0, and both were beating on one hub.
 */
export const UPGRADE_SKEW: ScenarioSpec = {
  machines: [
    {
      uid: "F9ACF59C-0E8B-5092-A6B4-7C07070737D2",
      name: "MacBook-Pro",
      darkmuxVersion: "3.7.1",
      schemaVersion: "1.51.0",
      formerNames: ["laptop"],
      records: 40,
    },
    {
      uid: "382A2016-41FD-5729-BF22-9C1A91F1BEDD",
      name: "m1-max-32gb-studio",
      darkmuxVersion: "3.7.0",
      schemaVersion: "1.42.0",
      records: 1,
    },
  ],
  roster: [{ id: "laptop" }, { id: "m1-max-32gb-studio" }],
};

/** The same fleet AFTER retention rolls past the rename — the alias that is
 *  currently holding the roster join together is gone. Nothing else changed. */
export const UPGRADE_SKEW_ALIAS_EXPIRED: ScenarioSpec = {
  ...UPGRADE_SKEW,
  machines: UPGRADE_SKEW.machines.map((m) => ({ ...m, formerNames: [] })),
};

/** Mid-upgrade: the older peer's daemon is restarting, so it has records in
 *  the window but no live beat. */
export const UPGRADE_IN_PROGRESS: ScenarioSpec = {
  ...UPGRADE_SKEW,
  machines: UPGRADE_SKEW.machines.map((m) =>
    m.darkmuxVersion === "3.7.0" ? { ...m, beating: false } : m,
  ),
};
