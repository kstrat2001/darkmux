import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./src/test-setup.ts"],
    // `verify/` is the Playwright live-render proof (a separate harness,
    // its own config — see `verify/playwright.config.ts`), not a vitest
    // suite; without this exclude, vitest's default glob picks up its
    // `*.spec.ts` files too and fails trying to load `@playwright/test`'s
    // `test()` outside a Playwright runner.
    //
    // (#2782) The root-level entry is for build-tooling modules that must NOT
    // live under `src/`, because nothing reachable from `main.tsx` may import
    // them — today just `devProxyTarget.ts`, the dev server's API proxy
    // target. Without it that rule would be unpinnable by this suite, which
    // is how it came to be a hardcoded port in the first place. Deliberately
    // `*.test.ts` and not `**/*.test.ts`: the broader glob would sweep
    // `node_modules` and `verify/` back in.
    include: ["src/**/*.test.{ts,tsx}", "*.test.ts"],
  },
});
