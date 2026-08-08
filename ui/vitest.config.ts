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
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
