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
  it("renders #logscope inside the log header with the scope label passed in", () => {
    render(<EventLogColumn records={[]} scopeLabel="FLEET" visible />);
    expect(document.getElementById("logscope")?.textContent).toBe("FLEET");
  });

  it("renders every record (up to the cap) as a row, newest first", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-old" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", session_id: "s-new" }),
    ];
    render(<EventLogColumn records={records} scopeLabel="fleet" visible />);
    const rows = document.querySelectorAll('[data-act="rec"]');
    expect(rows.length).toBe(2);
    // newest first (viewer.html:2443's `slice(-50).reverse()`)
    expect(rows[0].textContent).toContain("s-new");
    expect(rows[1].textContent).toContain("s-old");
  });

  it("shows the empty-log message when there are no records", () => {
    render(<EventLogColumn records={[]} scopeLabel="fleet" visible />);
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
    render(<EventLogColumn records={records} scopeLabel="fleet" visible />);
    fireEvent.change(screen.getByPlaceholderText("filter the stream…"), { target: { value: "s-alpha" } });
    const rows = document.querySelectorAll('[data-act="rec"]');
    expect(rows.length).toBe(1);
    expect(rows[0].textContent).toContain("s-alpha");
  });

  it("shows 'no match' in the query count when the search matches nothing", () => {
    render(<EventLogColumn records={[rec({})]} scopeLabel="fleet" visible />);
    fireEvent.change(screen.getByPlaceholderText("filter the stream…"), { target: { value: "nothing-matches-this" } });
    expect(screen.getByText("no match")).toBeInTheDocument();
  });

  it("clicking a row selects it (turns follow off) and shows it in the detail panel", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", session_id: "s-old" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", session_id: "s-new" }),
    ];
    render(<EventLogColumn records={records} scopeLabel="fleet" visible />);
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
    render(<EventLogColumn records={records} scopeLabel="fleet" visible />);
    fireEvent.click(document.querySelectorAll('[data-act="rec"]')[1]); // select the older one
    expect(document.getElementById("detailbody")!.textContent).toContain("s-old");

    fireEvent.click(document.getElementById("follow")!);
    expect(document.getElementById("follow")!.className).toMatch(/\bon\b/);
    expect(document.getElementById("detailbody")!.textContent).toContain("s-new");
  });

  it("shows the 'select an event' placeholder when nothing is selected and there are no records", () => {
    render(<EventLogColumn records={[]} scopeLabel="fleet" visible />);
    expect(screen.getByText("select an event from the log to inspect it")).toBeInTheDocument();
  });

  it("the 'model only' quick filter keeps reasoning/tool-call/turn rows and drops others", () => {
    const records = [
      rec({ ts: "2026-08-08T12:00:00.000Z", action: "dispatch.reasoning", session_id: "s-reasoning" }),
      rec({ ts: "2026-08-08T12:05:00.000Z", action: "machine.online", session_id: "s-machine" }),
    ];
    render(<EventLogColumn records={records} scopeLabel="fleet" visible />);
    fireEvent.click(document.getElementById("fbtn")!);
    const rows = document.querySelectorAll('[data-act="rec"]');
    expect(rows.length).toBe(1);
    expect(rows[0].textContent).toContain("s-reasoning");
  });
});
