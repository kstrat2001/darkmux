/**
 * Minimal ambient declarations for the handful of Node built-ins two test
 * files use to read real fixtures off disk (`sessionRun.test.ts`'s
 * byte-parity check against the real `tests/parity/goldens/
 * session-task-list.txt`, and `SessionReplay.test.tsx`'s render of the same
 * real corpus — see those files' own docs for why reading the REAL
 * recorded legacy output is the point, not a hand-rolled approximation).
 *
 * NOT a new dependency — `@types/node` is deliberately not installed (this
 * project's `tsconfig.json` scopes `types` to `["vite/client",
 * "vitest/globals"]` on purpose), and the drill-in packet's brief holds to
 * "no new dependencies." vitest itself runs tests via esbuild/vite's
 * transform, which never consults this file or cares about `@types/node`'s
 * absence — this file exists ONLY to satisfy `tsc --noEmit` (`bun run
 * typecheck`, part of `bun run build`). Node itself (the real runtime
 * vitest executes under) provides these modules regardless of what TS
 * knows about them; this is type-shape only, scoped to exactly the
 * handful of functions actually called, not a general `@types/node`
 * stand-in.
 */

declare module "node:fs" {
  export function readFileSync(path: string, encoding: string): string;
}

declare module "node:url" {
  export function fileURLToPath(url: string): string;
}

declare module "node:path" {
  function dirname(p: string): string;
  function resolve(...segments: string[]): string;
  function join(...segments: string[]): string;
  const path: { dirname: typeof dirname; resolve: typeof resolve; join: typeof join };
  export default path;
}

declare const process: { env: Record<string, string | undefined> };
