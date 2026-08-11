import { describe, it, expect } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { EventLogColumn } from "./EventLogColumn";
import type { FlowRecord } from "../types/handwritten";

function rec(overrides: Partial<FlowRecord>): FlowRecord {
  return {
    ts: "2026-08-08T12:00:00.000Z",
    category: "dispatch",
    action: "dispatch.reasoning",
    machine_id: "MacBook-Pro",
    ...overrides,
  };
}

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

  it("the 'model only' quick filter keeps reasoning/tool-call/turn rows and drops others", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", action: "dispatch.reasoning", session_id: "s-reasoning" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", action: "machine.online", session_id: "s-machine" }),
    ];
    render(<EventLogColumn scopeLabel="fleet" records={records} visible />);
    fireEvent.click(document.getElementById("fbtn")!);
    const rows = document.querySelectorAll('[data-act="rec"]');
    expect(rows.length).toBe(1);
    expect(rows[0].textContent).toContain("s-reasoning");
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
