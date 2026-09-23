import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
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

  // (#2863 review, finding 6) A multi-line value under MAX_INLINE renders in
  // full with no truncation button at all — the panel's own newline
  // collapses to whitespace visually (no `white-space: pre-wrap`), so a
  // SHORT second line (a second command, an injected instruction) reads as
  // if it were never there. A char-count-only truncation is defeatable by
  // padding line 1 out to just under the cutoff; a LINE count is not.
  it("marks a short multi-line value as multi-line, not just a long one", () => {
    const cmd = "echo ok\nrm -rf /workspace";
    render(<RecordView record={{ ...REC, source: cmd }} />);
    expect(screen.getByText(/\+1 more line/)).toBeInTheDocument();
    expect(screen.queryByText(cmd)).toBeNull();
  });

  it("a padded first line cannot defeat the multi-line marker", () => {
    // Padding line 1 out past MAX_INLINE does not change the fact that
    // there are 2 lines — the marker still says "lines", not just "chars".
    const cmd = "echo " + "x".repeat(200) + "\nrm -rf /workspace";
    render(<RecordView record={{ ...REC, source: cmd }} />);
    expect(screen.getByText(/\+1 more line/)).toBeInTheDocument();
  });

  // (#2863 review, finding 7, security — Trojan-Source class) A bidi
  // override in a raw field value renders raw, so it can reorder what the
  // panel visually displays without changing what actually ran.
  it("escapes a bidi override in a plain string value so it cannot reorder the row", () => {
    render(<RecordView record={{ ...REC, source: "safe‮exe.txt" }} />);
    expect(screen.getByText(/⟨U\+202E⟩/)).toBeInTheDocument();
    expect(screen.queryByText(/‮/)).toBeNull();
  });

  // (#2863 review round 2, finding 5) `Value()`'s escaping covers the
  // rendered field rows, but "raw JSON" is a SEPARATE render path —
  // `JSON.stringify(record, null, 2)` straight into a `<pre>`, bypassing
  // `Value()` entirely. A bidi override anywhere in the record reached
  // that view raw.
  it("escapes a bidi override in the raw JSON view too, not just the rendered rows", () => {
    render(<RecordView record={{ ...REC, source: "safe‮exe.txt" }} />);
    fireEvent.click(screen.getByText("raw JSON"));
    const pre = document.querySelector(".eventlog__detailpre")!;
    expect(pre).not.toBeNull();
    expect(pre.textContent).toContain("⟨U+202E⟩");
    expect(pre.textContent).not.toContain("‮");
  });

  it("expanding a multi-line value shows the hidden line as a real line break, not run together", () => {
    // `.rv__str` has no `white-space: pre-wrap` of its own, so a real
    // newline character collapses to whitespace visually under normal CSS —
    // the expanded text renders inline-styled so the break survives without
    // depending on the stylesheet loading.
    const cmd = "echo ok\nrm -rf /workspace";
    render(<RecordView record={{ ...REC, source: cmd }} />);
    fireEvent.click(screen.getByText(/\+1 more line/));
    const val = screen.getByText(/rm -rf \/workspace/);
    expect(val.style.whiteSpace).toBe("pre-wrap");
  });
});

// (operator, 2026-09-23) `sampled_at_ms 1790174634.5s` — the duration
// formula (`value/1000` toFixed "s") run on an epoch millisecond value.
// Below EPOCH_MS_THRESHOLD a `_ms` number is still a duration; at or above
// it, it's a timestamp and renders in the pane's own clock format.
describe("_ms fields: epoch timestamps vs durations", () => {
  const FROZEN_NOW = new Date("2026-09-23T10:00:00Z").getTime();

  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(FROZEN_NOW);
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("a small _ms value still renders as a millisecond duration", () => {
    render(<RecordView record={{ ...REC, payload: { sampler_cost_ms: 10 } }} />);
    expect(screen.getByText("10ms")).toBeInTheDocument();
  });

  it("a multi-second _ms duration still renders in seconds", () => {
    render(<RecordView record={{ ...REC, payload: { wall_ms: 1500 } }} />);
    expect(screen.getByText("1.5s")).toBeInTheDocument();
  });

  it("an epoch-ms value on the SAME day renders as a clock time, not '<n>.<n>s'", () => {
    const sameDayMs = FROZEN_NOW - 60_000;
    render(<RecordView record={{ ...REC, payload: { sampled_at_ms: sameDayMs } }} />);
    const expected = new Date(sameDayMs).toLocaleTimeString([], { hour12: false });
    expect(screen.getByText(expected)).toBeInTheDocument();
    // The bug this replaces: a 9+ digit decimal-seconds string.
    expect(screen.queryByText(/^\d{9,}\.\d+s$/)).toBeNull();
  });

  it("an epoch-ms value on a DIFFERENT day is prefixed with the short date", () => {
    const twoDaysAgoMs = FROZEN_NOW - 2 * 24 * 3600_000;
    render(<RecordView record={{ ...REC, payload: { sampled_at_ms: twoDaysAgoMs } }} />);
    const d = new Date(twoDaysAgoMs);
    const expected = `${d.toLocaleDateString([], { month: "short", day: "numeric" })} ${d.toLocaleTimeString([], { hour12: false })}`;
    expect(screen.getByText(expected)).toBeInTheDocument();
  });
});

// (operator, 2026-09-23) `cpu_clusters [object Object],[object Object]` —
// an array of objects fell through to the generic string branch, and
// `String()` on an array of objects joins each element's own useless
// `[object Object]`. Fixture trimmed from a real `machine.telemetry`
// record fetched off the live daemon (`GET /flow/2026-09-23`), scrubbed to
// the repo's synthetic machine_uid.
const TELEMETRY_REC = {
  ts: "2026-09-23T00:00:27Z",
  level: "info",
  category: "machinery",
  tier: "local",
  stage: "dispatch",
  action: "machine.telemetry",
  handle: "MacBook-Pro",
  source: "host",
  machine_id: "MacBook-Pro",
  machine_uid: "ABFCA777-9F06-A6BF-52CB-589A5D164929",
  payload: {
    sampled_at_ms: 1790121627281,
    sampler_cost_ms: 11,
    cpu_pct: 40,
    cpu_clusters: [
      { name: "Super", cores: 6, pct: 72, mhz: 4601 },
      { name: "Performance", cores: 12, pct: 24, mhz: 3720 },
    ],
    mem_pct: 67,
  },
};

describe("arrays of objects render as nested groups, not '[object Object]'", () => {
  it("cpu_clusters becomes one sub-group per element, labeled by its own 'name'", () => {
    render(<RecordView record={TELEMETRY_REC} />);
    expect(screen.queryByText(/\[object Object\]/)).toBeNull();
    // Header text, not just present anywhere — each element's own `name`
    // field ALSO renders as an ordinary row inside its group, so "Super"
    // legitimately appears twice; the group headers are what this asserts.
    const headers = [...document.querySelectorAll(".rv__grouphd")].map((el) => el.textContent);
    expect(headers).toContain("Super");
    expect(headers).toContain("Performance");
    // A field inside one element, grouped like any other number on the panel.
    expect(screen.getByText("4,601")).toBeInTheDocument();
  });

  it("arrays of primitives keep their current (comma-joined) rendering", () => {
    render(<RecordView record={{ ...REC, payload: { tags: ["a", "b", "c"] } }} />);
    expect(screen.getByText("a,b,c")).toBeInTheDocument();
  });

  it("a TOP-LEVEL array of primitives no longer vanishes (used to match neither filter)", () => {
    render(<RecordView record={{ ...REC, tags: ["x", "y"] }} />);
    expect(screen.getByText("x,y")).toBeInTheDocument();
  });

  it("a TOP-LEVEL array of objects renders as nested groups too", () => {
    render(<RecordView record={{ ...REC, findings: [{ name: "f1" }, { name: "f2" }] }} />);
    const headers = [...document.querySelectorAll(".rv__grouphd")].map((el) => el.textContent);
    expect(headers).toContain("f1");
    expect(headers).toContain("f2");
  });
});
