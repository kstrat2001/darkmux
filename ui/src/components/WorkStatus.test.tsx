import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { WorkStatus, workStatusKind } from "./WorkStatus";

// (operator, 2026-09-03) "the detailed run view shows pulsing RUNNING, and
// mission shows a non-pulsing green ACTIVE … meant to mean the same thing at
// a different scope level … prefer re-usable and consistent indicators." One
// chip, one vocabulary, one pulse — every scope's raw status maps into it.
describe("workStatusKind — every raw status the app has maps into five kinds", () => {
  it.each([
    ["running", "running"],
    ["active", "running"],
    ["live", "running"],
    ["complete", "done"],
    ["finished", "done"],
    ["finalized", "done"],
    ["closed", "done"],
    ["error", "error"],
    ["errored", "error"],
    ["killed", "error"],
    ["aborted", "stopped"],
    ["abandoned", "stopped"],
    ["canceled", "stopped"],
    ["interrupted", "stopped"],
    ["paused", "stopped"],
    ["planned", "idle"],
    ["unparseable", "idle"],
    [undefined, "idle"],
    ["something-new", "idle"],
  ])("%s → %s", (raw, kind) => {
    expect(workStatusKind(raw)).toBe(kind);
  });
});

describe("<WorkStatus>", () => {
  it("a running chip says ONE word whatever the scope's raw status — a mission's `active` and a step's `running` both read RUNNING", () => {
    const m = render(<WorkStatus status="active" />).container.firstElementChild!;
    const r = render(<WorkStatus status="running" />).container.firstElementChild!;
    expect(m.textContent).toBe("running");
    expect(r.textContent).toBe("running");
    // Same kind class (the look), different raw-word hook (`s-active` /
    // `s-running`) so a golden or a test can still tell them apart.
    expect(m).toHaveClass("wstatus", "is-running", "s-active");
    expect(r).toHaveClass("wstatus", "is-running", "s-running");
    expect(m.getAttribute("data-live")).toBe(r.getAttribute("data-live"));
  });
  it("only the running kind carries the pulse hook; a done chip is still", () => {
    const done = render(<WorkStatus status="finalized" />).container.firstElementChild!;
    expect(done).toHaveClass("wstatus", "is-done");
    expect(done).not.toHaveClass("is-running");
    expect(done.getAttribute("data-live")).toBeNull();
  });
  it("a terminal chip keeps its raw word or a caller's override; a running chip ignores the override and says RUNNING", () => {
    const done = render(<WorkStatus status="finalized" />).container.firstElementChild!;
    expect(done.textContent).toBe("finalized");
    const aborted = render(<WorkStatus status="abandoned" label="no ending recorded" />).container.firstElementChild!;
    expect(aborted.textContent).toBe("no ending recorded");
    const el = render(<WorkStatus status="live" label="● live" live="stale" />).container.firstElementChild!;
    expect(el.textContent).toBe("running");
    expect(el.getAttribute("data-live")).toBe("stale");
  });
  it("extra classes ride along, so a call site keeps its layout hook without a second style source", () => {
    const el = render(<WorkStatus status="error" className="labbadge" />).container.firstElementChild!;
    expect(el).toHaveClass("wstatus", "is-error", "labbadge");
  });
});
