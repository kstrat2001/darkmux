import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { WorkStatus, workStatusKind } from "./WorkStatus";

// (operator, 2026-09-03) "the detailed run view shows pulsing RUNNING, and
// mission shows a non-pulsing green ACTIVE … meant to mean the same thing at
// a different scope level … prefer re-usable and consistent indicators." One
// chip, one vocabulary, one pulse — every scope's raw status maps into it.
describe("workStatusKind — every raw status the app has maps into seven kinds", () => {
  it.each([
    ["running", "running"],
    ["active", "running"],
    ["complete", "done"],
    ["finalized", "done"],
    ["error", "error"],
    // (#2406) A mixed terminal — real output shipped, some of it did not.
    // Its own kind, distinct from `stopped` even though they share a color.
    ["degraded", "degraded"],
    // A mission a person aborted (`mission abort`): the "stopped by a person"
    // kind, its own color, never degraded's caution amber (operator, 2026-10-07).
    ["aborted", "aborted"],
    ["abandoned", "stopped"],
    // (F2) A deliberate escalation is a caution, never the error color.
    ["escalated", "stopped"],
    ["planned", "idle"],
    ["unparseable", "idle"],
    ["waiting", "idle"],
    ["not_reporting", "idle"],
    // A mission the daemon could not classify, no status at all, and a word this
    // build has never heard: all `unknown`, never `idle`.
    ["unknown", "unknown"],
    [undefined, "unknown"],
    ["something-new", "unknown"],
  ])("%s → %s", (raw, kind) => {
    expect(workStatusKind(raw)).toBe(kind);
  });
});

describe("workStatusKind — an abandoned status reads its reason", () => {
  it("abandoned by a person is aborted; abandoned with no ending, or for no named reason, is stopped", () => {
    expect(workStatusKind("abandoned", "aborted")).toBe("aborted");
    expect(workStatusKind("abandoned", "noterminal")).toBe("stopped");
    expect(workStatusKind("abandoned", undefined)).toBe("stopped");
    // A reason rides only beside `abandoned`; any other word ignores it.
    expect(workStatusKind("error", "aborted")).toBe("error");
  });
});

describe("<WorkStatus>", () => {
  it("an abandoned chip a person stopped reads aborted in the aborted kind, unless the caller words it", () => {
    const el = render(<WorkStatus status="abandoned" abandonReason="aborted" />).container.firstElementChild!;
    expect(el.textContent).toBe("aborted");
    expect(el).toHaveAttribute("data-status-kind", "aborted");
    expect(el).toHaveClass("wstatus", "is-aborted", "s-abandoned");
    const plain = render(<WorkStatus status="abandoned" />).container.firstElementChild!;
    expect(plain.textContent).toBe("abandoned");
    expect(plain).toHaveAttribute("data-status-kind", "stopped");
  });
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
  it("its color is keyed by kind, from the one status table (`styles.status.test.ts`)", () => {
    for (const status of ["running", "complete", "error", "degraded", "abandoned", "planned", "something-new"] as const) {
      const chip = render(<WorkStatus status={status as never} />).container.firstElementChild!;
      expect(chip.getAttribute("data-status-kind"), status).toBe(workStatusKind(status));
    }
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
    const el = render(<WorkStatus status="running" label="● live" live="stale" />).container.firstElementChild!;
    expect(el.textContent).toBe("running");
    expect(el.getAttribute("data-live")).toBe("stale");
  });
  it("extra classes ride along, so a call site keeps its layout hook without a second style source", () => {
    const el = render(<WorkStatus status="error" className="labbadge" />).container.firstElementChild!;
    expect(el).toHaveClass("wstatus", "is-error", "labbadge");
  });
  it("a degraded chip is its own kind, distinct from stopped, and stays non-pulsing", () => {
    const el = render(<WorkStatus status="degraded" />).container.firstElementChild!;
    expect(el.textContent).toBe("degraded");
    expect(el).toHaveClass("wstatus", "is-degraded", "s-degraded");
    expect(el).not.toHaveClass("is-stopped", "is-running", "is-error", "is-done");
    expect(el.getAttribute("data-live")).toBeNull();
  });
  it("(#2406) the title prop carries the counts breakdown through to the DOM", () => {
    const el = render(<WorkStatus status="degraded" title="7 complete · 1 errored · 4 running" />).container
      .firstElementChild!;
    expect(el.getAttribute("title")).toBe("7 complete · 1 errored · 4 running");
  });
});
