import { describe, it, expect, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { EventLogColumn } from "./EventLogColumn";
import type { FlowRecord } from "../types/handwritten";
import { closeOpenModal } from "../lib/dialogManager";

function rec(overrides: Partial<FlowRecord>): FlowRecord {
  return {
    ts: "2026-08-08T12:00:00.000Z",
    category: "dispatch",
    action: "dispatch.reasoning",
    machine_id: "MacBook-Pro",
    ...overrides,
  };
}

// `dialogManager`'s "which dialog is open" state is a module-level
// singleton, not React state — it survives across `render()` calls within
// this file (unmounting a component does not reset it). Without this, a
// test that opens the Filters modal and doesn't explicitly close it would
// leave it open for the NEXT test's freshly-rendered instance too.
afterEach(() => {
  closeOpenModal({ restore: false });
});

describe("EventLogColumn", () => {
  it("names the WINDOW in the header, and keeps #logscope present but empty", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    // (operator) The outer UI owns context now: the active tab or the crumb
    // already establishes it, and `#logscope` repeated that in six of its
    // eight legacy states. The element stays in the DOM, empty, so this
    // port's parity extraction agrees with legacy's; it dies with legacy at
    // the flip.
    // Present, HIDDEN, and still carrying its text: legacy's own span keeps
    // its text and `innerText` falls back to `textContent` when unrendered,
    // so emitting nothing here would make the two disagree in the parity
    // extraction. What changed is that it is no longer SHOWN.
    const scope = document.getElementById("logscope")!;
    expect(scope.hasAttribute("hidden")).toBe(true);
    expect(scope.textContent).toBe("fleet");
    expect(document.querySelector(".eventlog__head h3")?.textContent).toMatch(/events last \d+h/i);
  });

  it("renders every record (up to the cap) as a row, newest first", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-old" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", session_id: "s-new" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    const rows = document.querySelectorAll('[data-act="rec"]');
    expect(rows.length).toBe(2);
    // newest first (viewer.html:2443's `slice(-50).reverse()`)
    expect(rows[0].textContent).toContain("s-new");
    expect(rows[1].textContent).toContain("s-old");
  });

  it("shows the empty-log message when there are no records", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    expect(screen.getByText("no events yet")).toBeInTheDocument();
  });

  // RED-PROVED: with the search filter removed (query never applied), this
  // assertion fails because both rows would still be present after typing
  // "reasoning" — verified by temporarily deleting the `if (q && ...)`
  // guard in EventLogColumn.tsx and re-running this test, which then failed
  // on the `expect(rows.length).toBe(1)` line below; restored afterward.
  it("the search box filters the visible rows by substring", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", action: "dispatch.reasoning", session_id: "s-alpha" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", action: "dispatch.tool", session_id: "s-beta" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.change(screen.getByPlaceholderText("filter the stream…"), { target: { value: "s-alpha" } });
    const rows = document.querySelectorAll('[data-act="rec"]');
    expect(rows.length).toBe(1);
    expect(rows[0].textContent).toContain("s-alpha");
  });

  it("shows 'no match' in the query count when the search matches nothing", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[rec({})]} visible />);
    fireEvent.change(screen.getByPlaceholderText("filter the stream…"), { target: { value: "nothing-matches-this" } });
    expect(screen.getByText("no match")).toBeInTheDocument();
  });

  it("clicking a row selects it (turns follow off) and shows it in the detail panel", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-old" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", session_id: "s-new" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    // Default (follow=on) shows the newest record in the detail panel.
    expect(document.getElementById("detailbody")!.textContent).toContain("s-new");

    const rows = document.querySelectorAll('[data-act="rec"]');
    fireEvent.click(rows[1]); // the older row
    expect(document.getElementById("detailbody")!.textContent).toContain("s-old");
    // Clicking turned follow off.
    expect(document.getElementById("follow")!.className).not.toMatch(/\bon\b/);
  });

  it("the follow toggle re-enables auto-selecting the newest record", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-old" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", session_id: "s-new" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.click(document.querySelectorAll('[data-act="rec"]')[1]); // select the older one
    expect(document.getElementById("detailbody")!.textContent).toContain("s-old");

    fireEvent.click(document.getElementById("follow")!);
    expect(document.getElementById("follow")!.className).toMatch(/\bon\b/);
    expect(document.getElementById("detailbody")!.textContent).toContain("s-new");
  });

  it("shows the 'select an event' placeholder when nothing is selected and there are no records", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    expect(screen.getByText("select an event from the log to inspect it")).toBeInTheDocument();
  });

  it("#fbtn opens the filters modal (#1640) — matching legacy's data-act=\"filters\" trigger", () => {
    render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    expect(document.getElementById("modalbg")!.style.display).toBe("none");
    fireEvent.click(document.getElementById("fbtn")!);
    expect(document.getElementById("modalbg")!.style.display).toBe("flex");
    expect(document.querySelector('[aria-labelledby="filters-title"]')).toBeInTheDocument();
  });

  it("the modal's 'model only' quick action keeps reasoning/tool-call/turn rows and drops others", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", action: "dispatch.reasoning", session_id: "s-reasoning" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", action: "machine.online", session_id: "s-machine" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.click(document.getElementById("fbtn")!);
    fireEvent.click(screen.getByText("model only"));
    const rows = document.querySelectorAll('[data-act="rec"]');
    expect(rows.length).toBe(1);
    expect(rows[0].textContent).toContain("s-reasoning");
  });

  it("the modal's checkbox grid filters by category/tier/source, not just activity", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", action: "dispatch.reasoning", session_id: "s-local", tier: "local" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", action: "dispatch.reasoning", session_id: "s-cloud", tier: "cloud" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.click(document.getElementById("fbtn")!);
    // Uncheck the "cloud" tier checkbox — its own accessible label is the
    // literal facet value text (see FiltersDialog.tsx).
    fireEvent.click(screen.getByLabelText("cloud"));
    const rows = document.querySelectorAll('[data-act="rec"]');
    expect(rows.length).toBe(1);
    expect(rows[0].textContent).toContain("s-local");
  });

  it("'clear all' restores every facet and empties the search text", () => {
    const records = [rec({ ts: "2026-08-08T12:00:00.000Z", action: "dispatch.reasoning", session_id: "s-1", tier: "local" })];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.click(document.getElementById("fbtn")!);
    fireEvent.click(screen.getByLabelText("local"));
    expect(document.querySelectorAll('[data-act="rec"]').length).toBe(0);
    fireEvent.click(screen.getByText("clear all"));
    expect(document.querySelectorAll('[data-act="rec"]').length).toBe(1);
  });

  // RED-PROVED (real regression, caught by
  // tests/parity/next-parity-live.spec.ts, not invented for this test): a
  // first draft seeded `filters` via a plain `useState(() =>
  // defaultFilterState(facets))` lazy initializer, which only ever runs
  // ONCE — at mount, when `records` is still `[]` (the shape every real
  // caller passes before its fetch resolves; `App.tsx` always mounts this
  // component before `useRouteRecords`/`useFlowWindow` have data). That
  // locked every facet Set to EMPTY forever, so `matchesFilters` rejected
  // every record once real data arrived — the log stayed permanently
  // empty. Confirmed by temporarily reverting the `useEffect` reseed in
  // `EventLogColumn.tsx` back to the bare lazy initializer and re-running
  // this test, which then failed with 0 rows instead of 1; restored
  // afterward.
  it("records that arrive AFTER the initial (empty) mount still render — filters must not lock onto an empty facet snapshot", async () => {
    const { rerender } = render(<EventLogColumn scopeLabel="fleet" records={[]} visible />);
    expect(document.querySelectorAll('[data-act="rec"]').length).toBe(0);

    const records = [rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-late" })];
    rerender(<EventLogColumn scopeLabel="fleet" records={records} visible />);

    await waitFor(() => expect(document.querySelectorAll('[data-act="rec"]').length).toBe(1));
    expect(document.querySelector('[data-act="rec"]')!.textContent).toContain("s-late");
  });

  // `.eventlog__rec` was a click-only div — no `role`, no `tabIndex`, no
  // key handler — so a keyboard user could not even TAB to a record, let
  // alone select one. Text-only assertions can't see this: the click
  // handler already worked and already produced the right text.
  describe("keyboard operability of a log row", () => {
    it("every row is a real role=button reachable by Tab", () => {
      const records = [rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-1" })];
      render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
      const row = document.querySelector('[data-act="rec"]')!;
      expect(row).toHaveAttribute("role", "button");
      expect(row).toHaveAttribute("tabIndex", "0");
    });

    it("Enter selects the row, the same as a click", () => {
      const records = [
        rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-old" }),
        rec({ ts: "2026-08-08T12:05:00.000Z", session_id: "s-new" }),
      ];
      render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
      const rows = document.querySelectorAll('[data-act="rec"]');
      fireEvent.keyDown(rows[1], { key: "Enter" }); // the older row
      expect(document.getElementById("detailbody")!.textContent).toContain("s-old");
      expect(document.getElementById("follow")!.className).not.toMatch(/\bon\b/);
    });

    it("Space selects the row, the same as a click", () => {
      const records = [
        rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-old" }),
        rec({ ts: "2026-08-08T12:05:00.000Z", session_id: "s-new" }),
      ];
      render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
      const rows = document.querySelectorAll('[data-act="rec"]');
      fireEvent.keyDown(rows[1], { key: " " });
      expect(document.getElementById("detailbody")!.textContent).toContain("s-old");
    });
  });

  // Structural coverage for the two classNames that shipped matching
  // nothing in styles.css (rendered as default sans-serif text, invisible
  // to the innerText-based parity goldens).
  it("renders the machine/session meta spans with their styling classes", () => {
    const records = [rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-1", machine_id: "MacBook-Pro" })];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    expect(document.querySelector(".eventlog__recmachine")).toBeInTheDocument();
    expect(document.querySelector(".eventlog__recsession")).toBeInTheDocument();
    expect(document.querySelector(".eventlog__recmachine")!.textContent).toContain("MacBook-Pro");
    expect(document.querySelector(".eventlog__recsession")!.textContent).toContain("s-1");
  });
});
