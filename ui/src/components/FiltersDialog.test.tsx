import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { FiltersBody } from "./FiltersDialog";
import type { Facets, FilterState } from "../lib/eventFilters";

/** (operator, 2026-09-06) `FiltersBody` is the shared panel body rendered
 * both inline (phone drawer, `EventLogColumn.tsx`) and inside the desktop
 * `Dialog` (`FiltersDialog`) — these tests exercise it directly, matching
 * how `EventLogColumn.test.tsx` already tests filtering behavior through
 * the mounted column rather than reaching into `Dialog`'s portal. */

function facets(overrides: Partial<Facets> = {}): Facets {
  return {
    act: ["reasoning", "tool call", "dispatch start", "machine online", "note"],
    cat: [],
    tier: [],
    src: [],
    ...overrides,
  };
}

function allOnState(f: Facets): FilterState {
  return {
    act: new Set(f.act),
    cat: new Set(f.cat),
    tier: new Set(f.tier),
    src: new Set(f.src),
    q: "",
  };
}

function renderBody(f: Facets, filters: FilterState, extra: Partial<Parameters<typeof FiltersBody>[0]> = {}) {
  const onToggle = vi.fn();
  const onToggleMany = vi.fn();
  const onSetQuery = vi.fn();
  const utils = render(
    <FiltersBody
      facets={f}
      filters={filters}
      onToggle={onToggle}
      onToggleMany={onToggleMany}
      onSetQuery={onSetQuery}
      {...extra}
    />,
  );
  return { ...utils, onToggle, onToggleMany, onSetQuery };
}

describe("FiltersBody — search field placement (operator finding 1 & 2)", () => {
  it("renders the search field as the first focusable element in the panel", () => {
    const f = facets();
    const { container } = renderBody(f, allOnState(f));
    const focusable = container.querySelectorAll("input, button, select, textarea");
    expect(focusable.length).toBeGreaterThan(0);
    expect(focusable[0].tagName).toBe("INPUT");
    expect((focusable[0] as HTMLInputElement).id).toBe("fsearch");
  });

  it("the search field is its own full-width row (not sharing a row with buttons)", () => {
    const f = facets();
    renderBody(f, allOnState(f));
    const search = screen.getByPlaceholderText(/search text/i);
    // No longer inside the old shared footer with the quick-action buttons.
    expect(search.closest(".dialog__filterfoot")).toBeNull();
  });
});

describe("FiltersBody — quick actions removed (operator finding 3)", () => {
  it("'model only' and 'clear all' buttons are gone", () => {
    const f = facets();
    renderBody(f, allOnState(f));
    expect(screen.queryByText("model only")).toBeNull();
    expect(screen.queryByText("clear all")).toBeNull();
  });
});

describe("FiltersBody — section header toggles (operator finding 3)", () => {
  it("a section header with every value on reads checked, not indeterminate", () => {
    const f = facets({ act: ["reasoning", "tool call"] }); // both MODEL
    renderBody(f, allOnState(f));
    const header = screen.getByLabelText(/model: 2 of 2 on/i) as HTMLInputElement;
    expect(header.checked).toBe(true);
    expect(header.indeterminate).toBe(false);
  });

  it("a section header with none on reads unchecked, not indeterminate", () => {
    const f = facets({ act: ["reasoning", "tool call"] });
    const filters: FilterState = { act: new Set(), cat: new Set(), tier: new Set(), src: new Set(), q: "" };
    renderBody(f, filters);
    const header = screen.getByLabelText(/model: 0 of 2 on/i) as HTMLInputElement;
    expect(header.checked).toBe(false);
    expect(header.indeterminate).toBe(false);
  });

  it("a section header with SOME values on reads indeterminate", () => {
    const f = facets({ act: ["reasoning", "tool call"] });
    const filters: FilterState = { act: new Set(["reasoning"]), cat: new Set(), tier: new Set(), src: new Set(), q: "" };
    renderBody(f, filters);
    const header = screen.getByLabelText(/model: 1 of 2 on/i) as HTMLInputElement;
    expect(header.indeterminate).toBe(true);
  });

  it("clicking a mixed header calls onToggleMany to turn every section value ON", () => {
    const f = facets({ act: ["reasoning", "tool call"] });
    const filters: FilterState = { act: new Set(["reasoning"]), cat: new Set(), tier: new Set(), src: new Set(), q: "" };
    const { onToggleMany } = renderBody(f, filters);
    const header = screen.getByLabelText(/model: 1 of 2 on/i);
    header.click();
    expect(onToggleMany).toHaveBeenCalledWith("act", ["reasoning", "tool call"], true);
  });

  it("clicking an all-on header calls onToggleMany to turn every section value OFF", () => {
    const f = facets({ act: ["reasoning", "tool call"] });
    const { onToggleMany } = renderBody(f, allOnState(f));
    const header = screen.getByLabelText(/model: 2 of 2 on/i);
    header.click();
    expect(onToggleMany).toHaveBeenCalledWith("act", ["reasoning", "tool call"], false);
  });

  it("a section with zero present values is not rendered", () => {
    // No MACHINE-section value present at all.
    const f = facets({ act: ["reasoning", "tool call", "dispatch start", "note"] });
    renderBody(f, allOnState(f));
    expect(screen.queryByLabelText(/machine: \d+ of \d+ on/i)).toBeNull();
  });

  it("renders section headers for MODEL, DISPATCH, MISSION, MACHINE when all are present", () => {
    const f = facets(); // reasoning(MODEL), tool call(MODEL), dispatch start(DISPATCH), machine online(MACHINE), note(MISSION)
    renderBody(f, allOnState(f));
    expect(screen.getByLabelText(/^model: /i)).toBeInTheDocument();
    expect(screen.getByLabelText(/^dispatch: /i)).toBeInTheDocument();
    expect(screen.getByLabelText(/^mission: /i)).toBeInTheDocument();
    expect(screen.getByLabelText(/^machine: /i)).toBeInTheDocument();
  });

  it("every other facet (category/tier/telemetry source) also gets a header toggle", () => {
    const f = facets({ cat: ["work", "compaction"] });
    renderBody(f, allOnState(f));
    expect(screen.getByLabelText(/category: 2 of 2 on/i)).toBeInTheDocument();
  });

  it("per-value checkboxes under a section still toggle individually via onToggle", () => {
    const f = facets({ act: ["reasoning", "tool call"] });
    const { onToggle } = renderBody(f, allOnState(f));
    screen.getByLabelText("reasoning").click();
    expect(onToggle).toHaveBeenCalledWith("act", "reasoning");
  });
});
