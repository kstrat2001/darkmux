import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { InlineOrCells, inlineText, type InlineOrCellsItem } from "./InlineOrCells";

const POWER_ITEMS: InlineOrCellsItem[] = [
  { cellLabel: "now", cellValue: "11.5 W", inline: "11.5 W now" },
  { cellLabel: "avg", cellValue: "11.1 W", inline: "11.1 W avg" },
  { cellLabel: "p95", cellValue: "15.0 W", inline: "15.0 W p95" },
  { cellLabel: "max", cellValue: "15.1 W", inline: "15.1 W max" },
];

describe("InlineOrCells (#2108, operator finding)", () => {
  it("desktop: renders the items' inline fragments joined with ' · ', as a single span", () => {
    const { container } = render(<InlineOrCells items={POWER_ITEMS} isMobile={false} />);
    const span = container.querySelector("span")!;
    expect(span).not.toBeNull();
    expect(span.textContent).toBe("11.5 W now · 11.1 W avg · 15.0 W p95 · 15.1 W max");
    expect(container.querySelector('[data-act="inline-or-cells"]')).toBeNull();
  });

  it("mobile: renders a cell grid, one cell per item, label above value, each nowrap", () => {
    const { container } = render(<InlineOrCells items={POWER_ITEMS} isMobile />);
    const grid = container.querySelector('[data-act="inline-or-cells"]')!;
    expect(grid).not.toBeNull();
    const cells = grid.querySelectorAll(".inline-or-cells__cell");
    expect(cells.length).toBe(4);
    // No mid-item wrap: label and value are TWO separate elements inside
    // one cell (never a flat text run the browser could break anywhere).
    const first = cells[0];
    expect(first.querySelector(".inline-or-cells__cell-label")!.textContent).toBe("now");
    expect(first.querySelector(".inline-or-cells__cell-value")!.textContent).toBe("11.5 W");
    // No desktop span rendered at all on this branch.
    expect(container.querySelector("span")).toBeNull();
  });

  it("mobile: every cell value carries the nowrap class the stylesheet keys nowrap off of", () => {
    const { container } = render(<InlineOrCells items={POWER_ITEMS} isMobile />);
    const values = container.querySelectorAll(".inline-or-cells__cell-value");
    expect(values.length).toBe(4);
    values.forEach((v) => expect(v.className).toContain("inline-or-cells__cell-value"));
  });

  it("renders nothing at all for an empty item list, on either branch", () => {
    const desktop = render(<InlineOrCells items={[]} isMobile={false} />);
    expect(desktop.container.firstChild).toBeNull();
    const mobile = render(<InlineOrCells items={[]} isMobile />);
    expect(mobile.container.firstChild).toBeNull();
  });

  it("a 3-item list (channels row shape) renders 3 cells on mobile, joined text on desktop", () => {
    const items: InlineOrCellsItem[] = [
      { cellLabel: "CPU", cellValue: "11.3 W", inline: "CPU 11.3 W" },
      { cellLabel: "GPU", cellValue: "206 mW", inline: "GPU 206 mW" },
      { cellLabel: "ANE", cellValue: "0 mW", inline: "ANE 0 mW" },
    ];
    const desktop = render(<InlineOrCells items={items} isMobile={false} />);
    expect(desktop.container.querySelector("span")!.textContent).toBe(
      "CPU 11.3 W · GPU 206 mW · ANE 0 mW",
    );
    const mobile = render(<InlineOrCells items={items} isMobile />);
    expect(mobile.container.querySelectorAll(".inline-or-cells__cell").length).toBe(3);
  });

  it("className is applied on whichever branch actually renders", () => {
    const desktop = render(<InlineOrCells items={POWER_ITEMS} isMobile={false} className="my-hook" />);
    expect(desktop.container.querySelector("span.my-hook")).not.toBeNull();
    const mobile = render(<InlineOrCells items={POWER_ITEMS} isMobile className="my-hook" />);
    expect(mobile.container.querySelector('[data-act="inline-or-cells"].my-hook')).not.toBeNull();
  });

  it("inlineText joins items the same way the desktop branch does, for a caller building its own string", () => {
    expect(inlineText(POWER_ITEMS)).toBe("11.5 W now · 11.1 W avg · 15.0 W p95 · 15.1 W max");
    expect(inlineText([])).toBe("");
  });
});

// Keep `screen` imported meaningful — a smoke test that the component
// integrates with RTL's global queries, not just container queries.
describe("InlineOrCells — screen queries", () => {
  it("a mobile cell's value is queryable by text", () => {
    render(<InlineOrCells items={POWER_ITEMS} isMobile />);
    expect(screen.getByText("11.5 W")).toBeInTheDocument();
  });
});
