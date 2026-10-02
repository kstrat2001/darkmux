import { describe, it, expect } from "vitest";
import { runStatusWord } from "./runStatusWord";
import { NOT_REPORTING_STATUS } from "./machineAvailability";
import { type RunState } from "./flow";
import { toRunState, type CloseEdge, type Lifecycle } from "./lifecycle";
import { norm } from "../testing/records";
import type { RunStatus } from "../types/generated/RunStatus";
import type { AbandonReason } from "../types/generated/AbandonReason";

/**
 * (#2813) ONE STATUS AXIS.
 *
 * The viewer used to carry a second, incompatible vocabulary: a label function
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
 * Rust gains a variant, the generated type changes, `runStatusWord`'s `never`
 * binding stops compiling, AND this list is the checklist for what the new
 * cell should say. */
const ALL_STATUSES: RunStatus[] = [
  "planned",
  "running",
  "complete",
  "degraded",
  "error",
  "escalated",
  "abandoned",
  "unparseable",
];

describe("the status axis is the canonical one", () => {
  it("gives every canonical status exactly one non-empty label", () => {
    const labels = new Map<RunStatus, string>();
    for (const status of ALL_STATUSES) {
      const label = runStatusWord(status);
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
    // `killed`, `errored` and `canceled` were words of the old vocabulary. The
    // first two are `error` now; `canceled` is gone entirely — the state it
    // described is `abandoned` with no ending recorded.
    const every: string[] = [
      ...ALL_STATUSES.map((status) => runStatusWord(status)),
      runStatusWord("abandoned", "aborted"),
      runStatusWord("abandoned", "noterminal"),
    ];
    expect(every).not.toContain("canceled");
  });

  it("renders the payload nuances the wire actually carries", () => {
    // `error` reads `error` everywhere: the board, its filter and the run page.
    expect(runStatusWord("error")).toBe("error");
    // A machine that is not reporting has a word of its own, not `unparseable`'s.
    expect(runStatusWord(NOT_REPORTING_STATUS)).toBe("not reporting");
    expect(runStatusWord("unparseable")).toBe("unparseable");
    // `abandoned` splits on the reason the server sends.
    const abandoned = (abandonReason?: AbandonReason): RunState => ({
      status: "abandoned",
      abandonReason,
    });
    for (const [reason, word] of [["aborted", "aborted"], ["noterminal", "no ending recorded"], [undefined, "no ending recorded"]] as const) {
      const state = abandoned(reason);
      expect(runStatusWord(state.status, state.abandonReason)).toBe(word);
    }
  });
});

describe("a lens may select a status, never invent one", () => {
  // `toRunState` is the ONLY lifecycle -> status map (`lib/lifecycle.ts`):
  // every phase and close edge lands on a canonical status.
  const at = (phase: Lifecycle["phase"], edge?: CloseEdge): Lifecycle => ({
    phase,
    startMs: 0,
    lastActivityMs: 0,
    close: edge ? { edge, atMs: 1, skewed: false, record: norm({ ts: "2026-09-27T10:00:00Z", action: "dispatch.complete" }) } : null,
    waitUntilMs: null,
  });
  const cases: Array<{ name: string; l: Lifecycle; status: RunStatus; label: string }> = [
    { name: "in flight", l: at("open"), status: "running", label: "running" },
    { name: "held by a budget wait", l: at("waiting"), status: "running", label: "running" },
    { name: "not started as of the instant", l: at("not_started"), status: "planned", label: "planned" },
    { name: "failed", l: at("closed", { kind: "error", killed: false, exitCode: 1 }), status: "error", label: "error" },
    { name: "killed / timed out", l: at("closed", { kind: "error", killed: true, exitCode: 137 }), status: "error", label: "error" },
    { name: "finished cleanly", l: at("closed", { kind: "complete" }), status: "complete", label: "complete" },
    { name: "closed by the presence reconciler", l: at("closed", { kind: "session_end" }), status: "abandoned", label: "no ending recorded" },
    { name: "a wait the operator stopped", l: at("closed", { kind: "budget_stop", byOperator: true }), status: "abandoned", label: "aborted" },
    { name: "a mission aborted", l: at("closed", { kind: "mission_abort" }), status: "abandoned", label: "aborted" },
    { name: "silent past the staleness window (the old `canceled`)", l: at("stale"), status: "abandoned", label: "no ending recorded" },
  ];

  for (const c of cases) {
    it(`maps ${c.name} onto ${c.status}`, () => {
      const state = toRunState(c.l);
      expect(state.status).toBe(c.status);
      expect(ALL_STATUSES).toContain(state.status);
      expect(runStatusWord(state.status, state.abandonReason)).toBe(c.label);
    });
  }
});
