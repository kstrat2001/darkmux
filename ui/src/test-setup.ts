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

// (#2027) Storage is per-test state, and nothing was clearing it.
//
// Once the event log began persisting filters and its collapse choice to
// `sessionStorage`, one test's picks silently applied to every later test in
// the same file — and, because these keys are module-level, to other FILES
// sharing the environment. Two unrelated suites started reporting missing
// rows, which is a filter working correctly against state the test never set.
//
// Global rather than per-file on purpose: the alternative is remembering to
// add cleanup to every suite that ever touches a persisted preference, and
// the failure mode of forgetting is a confusing pass/fail somewhere else
// entirely. This is also the miniature of the production hazard the filter
// badge exists to surface — an invisible filter looks like a quiet system.
import { afterEach } from "vitest";

afterEach(() => {
  try {
    window.sessionStorage.clear();
    window.localStorage.clear();
  } catch {
    // storage unavailable in this environment — nothing to clear
  }
});
