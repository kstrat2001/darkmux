import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen } from "@testing-library/react";
import { BootErrorBoundary } from "./BootErrorBoundary";

// React logs a caught render error to console.error; that is expected here
// and would otherwise be mistaken for a failing test (same convention as
// LensErrorBoundary.test.tsx).
afterEach(() => vi.restoreAllMocks());

function Boom(): React.ReactElement {
  throw new TypeError("Cannot read properties of undefined (reading 'potential_bytes')");
}

describe("BootErrorBoundary", () => {
  it("shows the boot-error surface instead of an empty document when the shell throws during render", () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    render(
      <BootErrorBoundary>
        <Boom />
      </BootErrorBoundary>,
    );
    // The critical property (#1709): before this boundary, a throw anywhere
    // in the shell (Masthead, NavChrome, MachineDrawer, or App itself) left
    // nothing painted at all — no role, no text, nothing to screenshot or
    // report. Something must render.
    expect(screen.getByRole("alert")).toBeInTheDocument();
    // The message appears once as its own line and again inside the
    // selectable stack trace — both are expected, so assert on the count
    // rather than a single match.
    expect(screen.getAllByText(/potential_bytes/).length).toBeGreaterThan(0);
  });

  it("does not swallow the error — it still reaches the console for an operator with devtools open", () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    render(
      <BootErrorBoundary>
        <Boom />
      </BootErrorBoundary>,
    );
    expect(spy).toHaveBeenCalled();
  });

  it("renders children untouched when nothing throws", () => {
    render(
      <BootErrorBoundary>
        <div>app shell content</div>
      </BootErrorBoundary>,
    );
    expect(screen.getByText("app shell content")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("offers a reload affordance", () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    render(
      <BootErrorBoundary>
        <Boom />
      </BootErrorBoundary>,
    );
    expect(screen.getByRole("button", { name: /reload/i })).toBeInTheDocument();
  });
});
