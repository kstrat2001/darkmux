import { describe, it, expect } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { RecordView } from "./RecordView";

// The rules here are the ones that replace 26 per-action templates, so they
// carry the whole design. Each assertion pins a decision made from the
// 801-record survey, not a preference.

const REC = {
  ts: "2026-08-09T05:14:06Z",
  level: "info",
  tier: "local",
  stage: "dispatch",
  action: "dispatch.turn",
  handle: "coder",
  session_id: "crew-dispatch-coder-1786251936375019-0",
  source: "crew_dispatch",
  machine_uid: "F9ACF59C-0E8B-5092-A6B4-7C07070737D2",
  payload: { turn_seq: 73, finish_reason: "stop", tool_calls_count: 0, total_tokens: 33543 },
};

describe("RecordView", () => {
  it("leads with the verb and its subject, not with field names", () => {
    render(<RecordView record={REC} />);
    expect(screen.getByText("dispatch.turn")).toBeInTheDocument();
    expect(screen.getByText("coder")).toBeInTheDocument();
  });

  it("hides the fields the survey proved never vary, and says how many", () => {
    // level/tier/stage/machine_uid measured at <=2 distinct values across 801
    // records — machine_uid at exactly 1. Rendering them at full weight is
    // noise wearing signal's clothes.
    render(<RecordView record={REC} />);
    expect(screen.queryByText("F9ACF59C-0E8B-5092-A6B4-7C07070737D2")).toBeNull();
    expect(screen.getByText(/4 unchanging fields/)).toBeInTheDocument();
  });

  it("reveals them on request — hidden is not gone", () => {
    render(<RecordView record={REC} />);
    fireEvent.click(screen.getByText(/4 unchanging fields/));
    expect(screen.getByText("dispatch")).toBeInTheDocument();
  });

  it("groups numbers so a token count is readable at a glance", () => {
    render(<RecordView record={REC} />);
    expect(screen.getByText("33,543")).toBeInTheDocument();
  });

  it("truncates ids in the MIDDLE, keeping the part that distinguishes them", () => {
    // These ids share long prefixes; cutting the tail would delete exactly
    // the characters that tell two of them apart.
    render(<RecordView record={REC} />);
    const id = screen.getByTitle("crew-dispatch-coder-1786251936375019-0");
    expect(id.textContent).toContain("…");
    expect(id.textContent!.endsWith("0")).toBe(true);
    expect(id.textContent!.startsWith("crew-dispatch")).toBe(true);
  });

  it("renders an absent value as a dash, not as the word null", () => {
    render(<RecordView record={{ ...REC, source: null }} />);
    expect(screen.getByText("—")).toBeInTheDocument();
    expect(screen.queryByText("null")).toBeNull();
  });

  it("keeps the raw JSON one click away", () => {
    render(<RecordView record={REC} />);
    expect(screen.queryByText(/"machine_uid"/)).toBeNull();
    fireEvent.click(screen.getByText("raw JSON"));
    expect(screen.getByText(/"machine_uid"/)).toBeInTheDocument();
  });

  it("truncates a huge string rather than flooding the column", () => {
    // The median record is 463B and the largest is 46KB — that outlier is a
    // single long string, and it is exactly when the panel matters most.
    const long = "x".repeat(5000);
    render(<RecordView record={{ ...REC, source: long }} />);
    expect(screen.getByText(/\+4,840 more/)).toBeInTheDocument();
  });
});
