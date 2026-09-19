import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RunsBoard } from "./RunsBoard";
import { matrixCells, runForCell, type MatrixCell } from "../../lib/runMatrix";
import { RUNS_CAP } from "./format";
import { RUNNING_WORD, workStatusKind } from "../../components/WorkStatus";

/**
 * (#2813) THE RUN STATE MATRIX — the rendered half.
 *
 * The pure half (`lib/runMatrix.test.ts`) proves the axes round-trip, the
 * row is orderable, and the kind filter admits it. This file proves the
 * board actually PAINTS each cell, with the right badge, in the right
 * position. Those are different failures: a row can format perfectly and
 * still never reach the DOM, which is precisely the shape of #2812 — the
 * sort function was right and the row was invisible.
 *
 * One mount per assertion rather than per cell: every cell is fed to the
 * board together, so the sweep also covers the interaction between them
 * (ordering, the kind counts, the cap) instead of testing each cell in an
 * empty board where nothing can crowd it out.
 */

const CELLS = matrixCells();

/** Distinct and descending, so each cell's expected row position is exact.
 * Deliberately NOT `Date.now()`-relative: a fixture that mixes a fixed
 * timestamp with a clock-relative assertion is the flake this repo has
 * already paid for. Nothing here reads the clock. */
function runsForAllCells() {
  return CELLS.map((cell, i) => runForCell(cell, 1_000_000 - i));
}

/**
 * The SAME rows, in an order the board must not simply preserve.
 *
 * Red-proving caught this: with the runs handed to the board already in
 * newest-first order, disabling `runsFiltered`'s sort entirely left every
 * rendered-ordering assertion here green. A fixture that arrives sorted
 * cannot tell a sort from a pass-through. Reversal rather than a random
 * shuffle so a failure is reproducible from the test name alone.
 */
function shuffled<T>(rows: T[]): T[] {
  return [...rows].reverse();
}

/**
 * The word this cell's badge must say, written out rather than computed.
 *
 * Red-proving caught this too, and it is the subtler of the two: the first
 * draft derived the expectation by calling `runStatusLabel` — the function
 * the assertion exists to check. Collapsing that function's whole
 * `abandoned` split to `return r.status` then left every render test
 * green, because both sides of the comparison moved together. A test that
 * asks the subject what the answer is cannot fail.
 *
 * So the mapping is stated here, independently, as literals. It is a
 * second place that has to change when the words change, and that is the
 * point: the change has to be deliberate in both.
 */
function expectedBadgeText(cell: MatrixCell): string {
  // A running chip always says the ONE word every running chip says,
  // whatever label the lens passes (see `WorkStatus`'s own doc).
  if (workStatusKind(cell.status) === "running") return RUNNING_WORD;
  if (cell.status !== "abandoned") return cell.status;
  return cell.abandonReason === "aborted" ? "aborted" : "no ending recorded";
}

function mockRuns(runs: unknown[]) {
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      if (url === "/runs") {
        return Promise.resolve(
          new Response(JSON.stringify({ runs, generated_at_ms: 1 }), { status: 200 }),
        );
      }
      if (url === "/lab/runs") {
        return Promise.resolve(
          new Response(JSON.stringify({ configured: true, dir: "/lab", exists: true, runs: [] }), {
            status: 200,
          }),
        );
      }
      return Promise.resolve(new Response("not found", { status: 404 }));
    }),
  );
}

function renderBoard(initialKind: "all" | "mission" | "dispatch" | "lab" = "all") {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <RunsBoard initialKind={initialKind} initialRun={null} initialMachineUid={null} />
    </QueryClientProvider>,
  );
}

/** The rendered rows, in DOM order, by the id each row prints. */
function renderedRowIds(container: HTMLElement): string[] {
  return Array.from(container.querySelectorAll(".labrunrow")).map(
    (row) => row.querySelector(".labruncrew")?.textContent ?? "",
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
  window.location.hash = "";
});

describe("the runs board renders every cell of the matrix", () => {
  it("shows a row for each of the first RUNS_CAP cells, newest-activity-first", async () => {
    const runs = runsForAllCells();
    mockRuns(shuffled(runs));
    const { container } = renderBoard();
    await waitFor(() => expect(screen.getByText(runs[0].id)).toBeInTheDocument());

    const expected = runs.slice(0, RUNS_CAP).map((r) => r.id);
    expect(renderedRowIds(container)).toEqual(expected);
  });

  it("shows every cell once the cap is lifted", async () => {
    // The matrix is larger than `RUNS_CAP`, so without this the sweep
    // above would silently be a sweep over only the first 25 cells — and
    // the tail is where the uncommon states (planned, unparseable, the
    // ghosts) live.
    const runs = runsForAllCells();
    expect(runs.length).toBeGreaterThan(RUNS_CAP);
    mockRuns(shuffled(runs));
    const { container } = renderBoard();
    await waitFor(() => expect(screen.getByText(runs[0].id)).toBeInTheDocument());

    screen.getByText(new RegExp(`show all ${runs.length}`)).click();
    await waitFor(() => expect(renderedRowIds(container).length).toBe(runs.length));
    expect(renderedRowIds(container)).toEqual(runs.map((r) => r.id));
  });
});

describe.each(CELLS.map((c) => [c.id, c] as const))("cell %s renders", (_id, cell: MatrixCell) => {
  it("with the badge its own status earns", async () => {
    const run = runForCell(cell, 1_000);
    mockRuns([run]);
    const { container } = renderBoard();
    await waitFor(() => expect(screen.getByText(run.id)).toBeInTheDocument());

    const row = container.querySelector(".labrunrow") as HTMLElement;
    expect(row, `${cell.id} produced no row at all`).toBeTruthy();

    const badge = row.querySelector(".wstatus") as HTMLElement;
    expect(badge, `${cell.id} produced no status badge`).toBeTruthy();
    expect(badge.textContent).toBe(expectedBadgeText(cell));
    // The COLOR is keyed on the raw canonical status, not on the word —
    // so two states that happen to share a look stay distinguishable in
    // the DOM (`s-aborted` vs `s-abandoned` would collide if the badge
    // keyed on the label).
    expect(badge.className).toContain(`s-${cell.status}`);

    // The kind chip on the row, and the untracked marker, are the other
    // two axes the row is supposed to carry.
    expect(within(row).getByText(cell.kind)).toBeInTheDocument();
    const untracked = row.querySelector(".rununtracked");
    if (cell.tracked) {
      expect(untracked, `${cell.id} is tracked but rendered the untracked marker`).toBeNull();
    } else {
      expect(untracked, `${cell.id} is untracked and must say so`).toBeTruthy();
    }
  });
});

describe("kind filtering over the whole matrix", () => {
  it.each(["mission", "dispatch", "lab"] as const)("kind=%s shows that kind and no other", async (kind) => {
    const runs = runsForAllCells();
    mockRuns(shuffled(runs));
    const { container } = renderBoard(kind);
    const expected = runs.filter((r) => r.kind === kind);
    expect(expected.length, `the matrix must contain ${kind} cells`).toBeGreaterThan(0);
    await waitFor(() => expect(screen.getByText(expected[0].id)).toBeInTheDocument());

    const shown = renderedRowIds(container);
    expect(shown).toEqual(expected.slice(0, RUNS_CAP).map((r) => r.id));
    // The inverted half: nothing from another kind leaked in. Without
    // this the assertion above would pass on a filter that merely
    // preserved order.
    for (const other of runs.filter((r) => r.kind !== kind)) {
      expect(shown, `${other.id} leaked into kind=${kind}`).not.toContain(other.id);
    }
  });
});
