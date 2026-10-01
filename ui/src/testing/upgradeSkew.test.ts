import { describe, it, expect } from "vitest";
import {
  buildScenario,
  UPGRADE_SKEW,
  UPGRADE_IN_PROGRESS,
} from "./fleetScenario";
import { machineUids, nameOf, machineNames } from "../lib/machineIdentity";

/**
 * (#2818) THE UPGRADE, AS A TEST.
 *
 * Cutting this release puts the operator's fleet into a version-skew state
 * nothing has ever exercised: one machine on the new build, one still on the
 * released one, sharing a Redis hub and a flow stream. Every fix on this
 * release board touches those surfaces, and all of them were validated on a
 * single machine. Without this, the release IS the test.
 *
 * The assertion these make that nothing else can: given ONE scenario, every
 * surface that reports on it must agree. Today each surface is tested against
 * its own hand-built fixture, so two of them contradicting each other — which
 * is #2812 and #2813 — is unobservable.
 */
describe("(#2818) a fleet mid-upgrade, on two versions", () => {
  it("counts two machines, not three, while both versions are live", () => {
    const s = buildScenario(UPGRADE_SKEW);

    const uids = machineUids(s.data, s.liveMachines);
    expect(uids.length, "one uid per physical machine, whatever version it runs").toBe(2);
  });

  it("names each machine by what it calls itself NOW, not by an older alias", () => {
    const s = buildScenario(UPGRADE_SKEW);
    const uid = UPGRADE_SKEW.machines[0].uid;

    expect(nameOf(s.data, s.liveMachines, uid)).toBe("MacBook-Pro");
    // The older alias is still KNOWN — that is what folds the roster row —
    // but it is not what the machine is called.
    expect(machineNames(s.data, s.liveMachines, uid).has("laptop")).toBe(true);
  });

  it("a peer whose daemon is restarting mid-upgrade does not vanish", () => {
    // It has records in the window but no live beat. A machine being briefly
    // absent during its own upgrade must not remove it from the fleet.
    const s = buildScenario(UPGRADE_IN_PROGRESS);
    expect(machineUids(s.data, s.liveMachines).length).toBe(2);
  });
});
