import { defineConfig } from "@playwright/test";

// Packet 1's LIVE render-proof harness, NOT a CI-wired test suite (there is
// no `test` script pointing here — see `ui/README.md`). Talks to a
// throwaway daemon the operator/agent starts by hand on port 8790 (never
// port 8765 — the operator's real daemon). One-shot verification tool, kept
// deliberately separate from `src/**/*.test.tsx` (vitest, component-level)
// and from the repo's `tests/e2e`/`tests/parity` suites (legacy-viewer
// harnesses this packet must not touch).
export default defineConfig({
  testDir: ".",
  timeout: 30_000,
  use: {
    baseURL: "http://127.0.0.1:8790",
  },
});
