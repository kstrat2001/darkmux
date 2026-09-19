import { describe, it, expect } from "vitest";
import { statusLabel, runStateFrom, type RunState } from "./flow";
import type { RunStatus } from "../types/generated/RunStatus";
import type { AbandonReason } from "../types/generated/AbandonReason";

/**
 * (#2813) ONE STATUS AXIS.
 *
 * The viewer used to carry a second, incompatible vocabulary: `statusLabel`
 * took four booleans and returned `running | killed | errored | complete |
 * canceled`, which overlapped the server's `RunStatus` in exactly two values.
 * A fleet card and a runs list could therefore disagree about whether the
 * same run was happening, which is #2812.
 *
 * These tests pin the axis itself rather than any one lens: every canonical
 * status has exactly one label, and a lens's local predicates can only ever
 * SELECT a canonical status, never invent one.
 */

/** Every value of the generated union. Listing them here is deliberate: if
 * Rust gains a variant, the generated type changes, `statusLabel`'s `never`
 * binding stops compiling, AND this list is the checklist for what the new
 * cell should say. */
const ALL_STATUSES: RunStatus[] = [
  "planned",
  "running",
  "complete",
  "error",
  "abandoned",
  "unparseable",
];

describe("the status axis is the canonical one", () => {
  it("gives every canonical status exactly one non-empty label", () => {
    const labels = new Map<RunStatus, string>();
    for (const status of ALL_STATUSES) {
      const label = statusLabel({ status, killed: false });
      expect(label, `${status} must have a label`).toBeTruthy();
      labels.set(status, label);
    }
    // No two statuses may share a word — that is how two states become
    // indistinguishable on screen.
    const distinct = new Set(labels.values());
    expect(distinct.size, `labels collide: ${JSON.stringify([...labels])}`).toBe(
      ALL_STATUSES.length,
    );
  });

  it("never emits a word that is not a rendering of a canonical state", () => {
    // `killed` and `canceled` were states in the old vocabulary. `killed` is
    // now a rendering of `error`; `canceled` is gone entirely — the state it
    // described is `abandoned` with no ending recorded.
    const every: string[] = [
      ...ALL_STATUSES.map((status) => statusLabel({ status, killed: false })),
      statusLabel({ status: "error", killed: true }),
      statusLabel({ status: "abandoned", killed: false, abandonReason: "aborted" }),
      statusLabel({ status: "abandoned", killed: false, abandonReason: "noterminal" }),
    ];
    expect(every).not.toContain("canceled");
  });

  it("renders the payload nuances the wire actually carries", () => {
    // `killed` is a nuance WITHIN error, not a peer of abandoned — which is
    // what the legacy `killed ? "killed" : "errored"` meant.
    expect(statusLabel({ status: "error", killed: true })).toBe("killed");
    expect(statusLabel({ status: "error", killed: false })).toBe("errored");
    // `abandoned` splits on the reason the server sends, matching the runs
    // lens's own `runStatusLabel`.
    const abandoned = (abandonReason?: AbandonReason): RunState => ({
      status: "abandoned",
      killed: false,
      abandonReason,
    });
    expect(statusLabel(abandoned("aborted"))).toBe("aborted");
    expect(statusLabel(abandoned("noterminal"))).toBe("no ending recorded");
    expect(statusLabel(abandoned(undefined))).toBe("no ending recorded");
  });
});

describe("a lens may select a status, never invent one", () => {
  const cases: Array<{
    name: string;
    predicates: { open: boolean; errored: boolean; killed: boolean; clean: boolean };
    status: RunStatus;
    label: string;
  }> = [
    {
      name: "in flight",
      predicates: { open: true, errored: false, killed: false, clean: false },
      status: "running",
      label: "running",
    },
    {
      name: "failed",
      predicates: { open: false, errored: true, killed: false, clean: false },
      status: "error",
      label: "errored",
    },
    {
      name: "killed / timed out",
      predicates: { open: false, errored: true, killed: true, clean: false },
      status: "error",
      label: "killed",
    },
    {
      name: "finished cleanly",
      predicates: { open: false, errored: false, killed: false, clean: true },
      status: "complete",
      label: "complete",
    },
    {
      name: "done with no terminal record (the old `canceled`)",
      predicates: { open: false, errored: false, killed: false, clean: false },
      status: "abandoned",
      label: "no ending recorded",
    },
  ];

  for (const c of cases) {
    it(`maps ${c.name} onto ${c.status}`, () => {
      const state = runStateFrom(c.predicates);
      expect(state.status).toBe(c.status);
      expect(ALL_STATUSES).toContain(state.status);
      expect(statusLabel(state)).toBe(c.label);
    });
  }

  it("`open` wins over every other predicate — a live run is running", () => {
    // Guards the ordering inside `runStateFrom`: a run that is still open can
    // carry a stale terminal record from an earlier attempt.
    const state = runStateFrom({ open: true, errored: true, killed: true, clean: true });
    expect(state.status).toBe("running");
  });
});
