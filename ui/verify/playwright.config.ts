import { defineConfig } from "@playwright/test";

// Packet 1's LIVE render-proof harness, NOT a CI-wired test suite (there is
// no `test` script pointing here — see `ui/README.md`). Talks to a
// throwaway daemon the operator/agent starts by hand — port 8790 by
// default (Packet 1's own proof; never port 8765, the operator's real
// daemon — EXCEPT via `DARKMUX_VERIFY_PORT=5273`, the vite dev server, which
// proxies API calls to 8765: `task-row-name.spec.ts` needs a REAL mission (the
// defect only reproduces on real data), so it is a local-only proof, not CI.
// #2282.
// daemon), overridable via `DARKMUX_VERIFY_PORT` so a later lens packet's
// own live-render spec (e.g. Packet 2's `machine-render.spec.ts`, port
// 8793 per the overnight runbook's "throwaway daemons on 8793+" boundary)
// can point at ITS OWN throwaway daemon without a second config file. One-
// shot verification tool, kept deliberately separate from
// `src/**/*.test.tsx` (vitest, component-level) and from the repo's
// `tests/e2e`/`tests/parity` suites (legacy-viewer harnesses this
// directory must not touch).
const port = process.env.DARKMUX_VERIFY_PORT || "8790";

export default defineConfig({
  testDir: ".",
  timeout: 30_000,
  use: {
    baseURL: `http://127.0.0.1:${port}`,
  },
});
