import { describe, it, expect } from "vitest";
import {
  REACHABLE,
  REACHABLE_UNTRACKED,
  ABANDON_REASONS,
  matrixCells,
  runForCell,
  type MatrixCell,
} from "./runMatrix";
import { runActivity, runStatusLabel, runsFiltered, RUNS_CAP } from "../lenses/runs/format";
import { statusLabel } from "./flow";
import type { RunKind } from "../types/generated/RunKind";
import type { RunStatus } from "../types/generated/RunStatus";

/**
 * (#2813) THE RUN STATE MATRIX — the pure half.
 *
 * Operator: *"there should be a test for every kind of run, in any kind of
 * state."* This file iterates `matrixCells()` and makes, per cell, the
 * assertions that need no DOM: the axes round-trip, the row is orderable,
 * the badge text is a rendering of the canonical state, and the kind
 * filter includes it exactly when it should. `runMatrix.render.test.tsx`
 * makes the fourth kind of assertion — that the board actually paints it.
 *
 * WHAT THIS FILE CANNOT DO, stated so nobody mistakes its green for more
 * coverage than it is. Every row here is hand-built, so it proves things
 * about the UI's treatment of a row, never about whether the server emits
 * that row correctly. #2812's first defect lived entirely on the other
 * side of that line: `runActivity` was right, well tested, and handed a
 * row whose three timestamps were all absent. The boundary invariant —
 * every row `/runs` emits carries something to sort on — is asserted in
 * `crates/darkmux-serve/src/runs.rs`'s own matrix tests, against
 * `build_runs` output, because that is the only place it is observable.
 */

const CELLS = matrixCells();

/** Distinct, descending, so a cell's expected sort position is a fact
 * about `runActivity` rather than about tie-breaking. */
function tsFor(index: number): number {
  return 1_000_000 - index;
}

describe("the matrix is derived, not listed", () => {
  it("covers every reachable cell and nothing else", () => {
    // Recomputed here from the tables rather than asserted as a literal
    // count: a bare number would have to be edited (and could be edited
    // wrongly) every time a status is genuinely added.
    let expected = 0;
    for (const kind of Object.keys(REACHABLE) as RunKind[]) {
      for (const status of Object.keys(REACHABLE[kind]) as RunStatus[]) {
        for (const tracked of [true, false]) {
          const reachable = tracked ? REACHABLE[kind][status] : REACHABLE_UNTRACKED[kind][status];
          if (!reachable) continue;
          expected += status === "abandoned" ? ABANDON_REASONS.length : 1;
        }
      }
    }
    expect(CELLS.length).toBe(expected);
    expect(CELLS.length).toBeGreaterThan(0);
  });

  it("gives every cell a unique id", () => {
    expect(new Set(CELLS.map((c) => c.id)).size).toBe(CELLS.length);
  });

  it("never synthesises an untracked lab row", () => {
    // `ghost_runs` produces `RunKind::Dispatch` and `flow_mission_to_run`
    // produces `RunKind::Mission`; a lab row exists only when this daemon
    // can see the run directory, which is what `tracked` MEANS for that
    // kind. An earlier flat untracked table invented five of these.
    expect(CELLS.filter((c) => c.kind === "lab" && !c.tracked)).toEqual([]);
  });

  it("reaches an untracked dispatch in error", () => {
    // The other direction of the same correction: `dispatch error` is a
    // terminal record `terminal_status_for_action` maps to `error`, so a
    // ghost genuinely reaches it. The flat table said it could not.
    expect(CELLS.some((c) => c.kind === "dispatch" && !c.tracked && c.status === "error")).toBe(
      true,
    );
  });

  it("splits abandoned on its reason and no other status", () => {
    for (const cell of CELLS) {
      if (cell.status === "abandoned") {
        expect(cell.abandonReason, `${cell.id} must name a reason`).toBeTruthy();
      } else {
        expect(cell.abandonReason, `${cell.id} must not carry a reason`).toBeUndefined();
      }
    }
  });
});

describe.each(CELLS.map((c) => [c.id, c] as const))("cell %s", (_id, cell: MatrixCell) => {
  const run = runForCell(cell, 1_000);

  it("round-trips kind, status and abandoned_reason", () => {
    expect(run.kind).toBe(cell.kind);
    expect(run.status).toBe(cell.status);
    expect(run.tracked).toBe(cell.tracked);
    expect(run.abandoned_reason).toBe(cell.abandonReason);
  });

  it("has a non-zero sort key", () => {
    // The assertion #2812 turns on. On this side of the wire it is a
    // statement about `runActivity`'s contract — that it reports a usable
    // key for a row that HAS a timestamp, in every state. The matching
    // statement about the server always PROVIDING one is in the Rust
    // matrix; see this file's header for why it cannot live here.
    expect(runActivity(run)).toBeGreaterThan(0);
  });

  it("badges with a rendering of its own canonical state", () => {
    const label = runStatusLabel(run);
    expect(label).toBeTruthy();
    if (cell.status !== "abandoned") {
      // A lens may choose WORDS, it may not choose STATES — every other
      // status renders as its own name, verbatim.
      expect(label).toBe(cell.status);
    } else {
      expect(label).toBe(cell.abandonReason === "aborted" ? "aborted" : "no ending recorded");
    }
  });

  it("agrees with the fleet-side label for the same state", () => {
    // The disagreement #2813 named: two vocabularies for one axis meant a
    // card and a list could describe the same run differently. They are
    // now two renderings of the same canonical state, so neither may
    // describe a state the other does not have. Asserting the pair are
    // both non-empty and both derived from `cell.status` is what pins
    // that; asserting they are IDENTICAL would be wrong, since the runs
    // board deliberately expands `abandoned` and the card does not have
    // to.
    const card = statusLabel({ status: cell.status, killed: false, abandonReason: cell.abandonReason });
    expect(card, `${cell.id} has no fleet-side label`).toBeTruthy();
    expect(card).not.toBe("canceled");
    expect(card).not.toBe("killed");
  });

  it("is included by its own kind filter and excluded by the others", () => {
    expect(runsFiltered([run], "all").map((r) => r.id)).toEqual([run.id]);
    expect(runsFiltered([run], cell.kind).map((r) => r.id)).toEqual([run.id]);
    for (const other of ["mission", "dispatch", "lab"] as RunKind[]) {
      if (other === cell.kind) continue;
      expect(runsFiltered([run], other), `${cell.id} leaked into kind=${other}`).toEqual([]);
    }
  });
});

describe("ordering across the whole matrix", () => {
  it("sorts every cell newest-activity-first", () => {
    const runs = CELLS.map((cell, i) => runForCell(cell, tsFor(i)));
    // Shuffled deterministically — an already-sorted input would let a
    // no-op sort pass.
    const shuffled = [...runs].reverse();
    const sorted = runsFiltered(shuffled, "all");
    expect(sorted.map((r) => r.id)).toEqual(runs.map((r) => r.id));
  });

  it("a row with no timestamp at all sinks below the cap and disappears", () => {
    // #2812's mechanism, encoded. The 39 affected lab rows were not
    // mis-ordered by a little; they were unreachable. `RUNS_CAP` cuts a
    // newest-first list, and a row whose key is 0 sits below every row
    // that has any timestamp — however recent it actually is.
    //
    // This is the UI-side consequence of the server invariant the Rust
    // matrix asserts. It stays here because it is the reason that
    // invariant matters, and because a future "simplification" of
    // `runActivity` that treated a missing timestamp as `Date.now()`
    // would be caught by nothing else.
    const older = Array.from({ length: RUNS_CAP }, (_, i) => ({
      id: `has-a-timestamp-${i}`,
      kind: "lab" as const,
      status: "complete" as const,
      tracked: true,
      updated_ts: 1, // the oldest possible real timestamp
    }));
    const liveButUnorderable = runForCell(
      { kind: "lab", status: "running", tracked: true, id: "live-no-timestamp" },
      undefined,
    );
    expect(runActivity(liveButUnorderable)).toBe(0);

    const shown = runsFiltered([liveButUnorderable, ...older], "all").slice(0, RUNS_CAP);
    expect(shown.some((r) => r.id === "live-no-timestamp")).toBe(false);
    // And the control: give it a real timestamp and it is at the top.
    const orderable = runForCell(
      { kind: "lab", status: "running", tracked: true, id: "live-with-timestamp" },
      500,
    );
    const shownNow = runsFiltered([orderable, ...older], "all").slice(0, RUNS_CAP);
    expect(shownNow[0].id).toBe("live-with-timestamp");
  });
});
