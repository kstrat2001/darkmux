# darkmux viewer

React + TanStack Query workspace that builds to ONE committed, self-contained
`crates/darkmux-serve/assets/next.html`. **This is the viewer** — it serves
`GET /` and `GET /play/:date` as of the flip (#1800); `GET /next`, the route
it grew up on, is now a permanent redirect to `/` so bookmarks and phone
home-screen shortcuts keep working.

The gate for the flip was a NUMBER, not a judgement: 21 of 22 goldens recorded
from the legacy viewer asserting real byte parity in a real browser
(`tests/parity/`), with the 22nd (`mission-replay`) blocked on a missing
corpus fixture rather than on the port.

**The legacy `viewer.html` is retired (#1806)** — deleted from the tree along
with its XSS golden tests and the legacy half of `tests/parity/`. Its frozen
render output survives as `tests/parity/goldens/*.txt` (the spec this port is
graded against); its source is recoverable with
`git show v2.9.0:crates/darkmux-serve/assets/viewer.html`. Every
`viewer.html:NNNN` line-citation comment under `src/` below (and the
provenance references in this file) point at that same recoverable revision —
not a file present anywhere in the current tree.

## The stack, and why

- **bun** — package manager + script runner. Fast, text lockfile (`bun.lock`
  is diffable, unlike a binary one).
- **Vite + `vite-plugin-singlefile`** — bundler. Singlefile inlines every JS
  chunk and the stylesheet into ONE `index.html`, which is what lets the
  Rust side `include_str!` a single committed artifact instead of shipping a
  whole `dist/` tree — the release binary stays self-contained and node-free
  (`cargo build` never touches `ui/`).
- **React 18 + TypeScript strict** — the spine the port was ratified against.
- **@tanstack/react-query** — data layer. Every fetch goes through ONE typed
  wrapper (`src/lib/fetcher.ts`) returning a discriminated `FetchResult`, so
  a lens component renders its error state from real detail (status +
  message), not Query's bare `isError` boolean.
- **ts-rs** (Rust dev-dependency, `#[cfg(test)]`-gated in
  `crates/darkmux-serve/src/runs.rs`) — generates `src/types/generated/*.ts`
  straight from the `/runs` view-model structs. Endpoints that build ad-hoc
  `serde_json::json!({...})` (no typed Rust struct to derive from) are typed
  by hand in `src/types/handwritten.ts`, which names its Rust source per
  field group — see that file's own doc for which endpoints and why.
- **reactflow** (#1868) — the mission-graph lens's (`#mission=<id>`,
  `src/lenses/mission/`) canvas renderer. Pinned to 11.11.4, matching the
  version the pre-#1868 standalone `mission-graph.html` page's own vendored
  bundle used — a real dependency of this workspace now, bundled by Vite
  like everything else, rather than a separately-vendored IIFE. That
  standalone page and its vendor dir (`crates/darkmux-serve/assets/vendor/`)
  are retired (#1868's third packet); recoverable with
  `git show v2.9.0:crates/darkmux-serve/assets/vendor/README.md` if the
  pinning history is ever needed again. Its MIT notice lives in
  `vendor-licenses/LICENSE-reactflow` alongside react/react-dom/
  @tanstack/react-query's, prepended to the built artifact the same way
  (see that directory's own README).
- **vitest + @testing-library/react** — unit/component tests.
- **@playwright/test** (`ui/verify/`, NOT wired into `test`/`build`) — the
  one-shot LIVE render-proof harness against a throwaway daemon. Not a
  runtime or CI dependency; see that directory's own doc comment.

## Scripts

| Script | What |
|---|---|
| `bun run dev` | Vite dev server (not used by the daemon; local iteration only). |
| `bun run build` | typecheck → `vite build` → copies `dist/index.html` to `../crates/darkmux-serve/assets/next.html`. **Run this and commit the result** — the artifact is committed, not built by `cargo build`. |
| `bun run test` | vitest, `src/**/*.test.{ts,tsx}` only (`verify/` is excluded — see `vitest.config.ts`). |
| `bun run typecheck` | `tsc --noEmit`. |
| `bun run types:regen` | Regenerates `src/types/generated/*.ts` from the live Rust structs (`cargo test -p darkmux-serve export_bindings --lib`). |
| `bun run types:check` | Regenerates, then `git diff --exit-code` on the generated dir — the drift guard. Wire this into CI in a follow-up packet; it's a local check today. |

## The `#lens=` compatibility promise

The hash grammar (`src/lib/route.ts`) is a byte-for-byte port of
`viewer.html`'s own `catalogQuery`/`labQuery`/`consoleQuery`/`machineQuery`
functions, precedence order included — every CLI-printed deep link and
phone-bookmark against the legacy viewer must keep resolving once a lens
moves here. An unrecognized route renders a named placeholder
(`LensPlaceholder`), never a blank page.

## What's deliberately deferred to lens packets

- Every lens except the fleet-machines strip (`FleetStrip`) is a
  `LensPlaceholder` naming what still needs porting.
- `PresenceBeat` (fleet/machines/live's real Rust type, in `darkmux-flow`)
  is hand-written, not ts-rs-derived — bridging it would mean adding ts-rs
  as a dependency of a crate consumed by production code, not just
  `darkmux-serve`'s test-only surface; ledgered rather than chased (see the
  overnight runbook's FOLLOW-UPS section).
- `types:check` is a local script, not a CI job yet.
- The SSE spike (`src/lib/sse.ts`) is unit-tested against a mock
  `EventSource` only — the live proof against a real `/flow/:date/stream`
  is Packet 5's job.
- The three-state **contract** (empty-is-never-silent, enforced everywhere)
  is a later arc; this packet only proves the PATTERN on one region.
