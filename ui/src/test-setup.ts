import "@testing-library/jest-dom/vitest";

// jsdom has no `ResizeObserver` — real browsers do, so this is a
// test-environment gap, not a missing app dependency. `reactflow` (#1868's
// mission-graph lens) calls it unconditionally on mount to track pane size,
// so any test that renders `MissionCanvas`/`MissionGraphLens` crashes
// without this stub. A no-op is sufficient: no test in this suite asserts
// on a resize-driven React Flow layout change.
if (typeof globalThis.ResizeObserver === "undefined") {
  globalThis.ResizeObserver = class ResizeObserver {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
}
