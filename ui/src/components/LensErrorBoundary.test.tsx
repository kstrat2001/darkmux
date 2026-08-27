import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { LensErrorBoundary } from "./LensErrorBoundary";

// React logs a caught render error to console.error; that is expected here and
// would otherwise be mistaken for a failing test.
afterEach(() => vi.restoreAllMocks());

function Boom({ die }: { die: boolean }): React.ReactElement {
  if (die) throw new TypeError("Cannot read properties of undefined (reading 'potential_bytes')");
  return <div>lens content</div>;
}

describe("LensErrorBoundary", () => {
  it("contains a lens crash instead of blanking the page, and names what broke", () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    render(
      <div>
        <div>the rest of the app</div>
        <LensErrorBoundary name="machine">
          <Boom die />
        </LensErrorBoundary>
      </div>,
    );
    // The critical property: siblings survive. Before this, the whole tree
    // unmounted and the operator got a blank browser window.
    expect(screen.getByText("the rest of the app")).toBeInTheDocument();
    expect(screen.getByRole("alert")).toBeInTheDocument();
    expect(screen.getByText(/the machine lens stopped rendering/i)).toBeInTheDocument();
    // The message is RENDERED, not only logged — a crash an operator can read
    // is a crash they can report.
    expect(screen.getByText(/potential_bytes/)).toBeInTheDocument();
  });

  it("renders children untouched when nothing throws", () => {
    render(
      <LensErrorBoundary name="fleet">
        <Boom die={false} />
      </LensErrorBoundary>,
    );
    expect(screen.getByText("lens content")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("offers a retry control rather than leaving a dead tab", () => {
    // Deliberately asserts the CONTROL exists and re-renders children, not
    // that a component "recovers" — that would be testing React's remount
    // semantics rather than this boundary's contract.
    vi.spyOn(console, "error").mockImplementation(() => {});
    render(
      <LensErrorBoundary name="runs">
        <Boom die />
      </LensErrorBoundary>,
    );
    const retry = screen.getByRole("button", { name: /try again/i });
    expect(retry).toBeInTheDocument();
    fireEvent.click(retry);
    // Still throwing, so it lands back on the fallback rather than blanking.
    expect(screen.getByRole("alert")).toBeInTheDocument();
  });
});
