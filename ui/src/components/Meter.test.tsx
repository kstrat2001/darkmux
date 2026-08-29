import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { Meter } from "./Meter";

describe("Meter (#2107, #1833)", () => {
  it("renders now/avg/max as text, rounded", () => {
    render(<Meter label="CPU" now={62.4} avg={41.6} max={88.2} />);
    expect(screen.getByText("62%")).toBeInTheDocument();
    expect(screen.getByText("42% avg")).toBeInTheDocument();
    expect(screen.getByText("88% max")).toBeInTheDocument();
    expect(screen.getByText("CPU")).toBeInTheDocument();
  });

  it("draws no needle/marks and shows em-dash when a reading is null, not zero", () => {
    const { container } = render(<Meter label="GPU" now={null} avg={null} max={null} />);
    expect(container.querySelector(".meter-needle")).toBeNull();
    expect(container.querySelector(".meter-mark-avg")).toBeNull();
    expect(container.querySelector(".meter-mark-max")).toBeNull();
    expect(screen.getByText("—")).toBeInTheDocument();
    expect(screen.getByText("— avg")).toBeInTheDocument();
    expect(screen.getByText("— max")).toBeInTheDocument();
  });

  it("shows a scope label when given one", () => {
    render(<Meter label="MEM" now={50} avg={40} max={60} scopeLabel="last 10 min" />);
    expect(screen.getByText("last 10 min")).toBeInTheDocument();
  });

  it("clamps an out-of-range reading into the arc's 0-100 sweep rather than drawing past it", () => {
    const { container } = render(<Meter label="CPU" now={150} avg={0} max={-10} />);
    const fill = container.querySelector(".meter-fill");
    expect(fill?.getAttribute("stroke-dasharray")).toBe("100 100");
  });

  it("carries a spoken summary in the svg's aria-label", () => {
    render(<Meter label="CPU" now={62} avg={41} max={88} scopeLabel="last 10 min" />);
    const svg = screen.getByRole("img", { name: /CPU:.*now 62%.*average 41%.*max 88%.*last 10 min/ });
    expect(svg).toBeInTheDocument();
  });
});
