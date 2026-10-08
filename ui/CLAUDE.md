# The viewer (`ui/`)

Claude Code loads this file when it works in this directory.

The viewer is TypeScript and React. `bun run build` compiles it into `crates/darkmux-serve/assets/next.html`, which is committed and embedded in the darkmux binary; CI fails if the committed bundle is out of date with `ui/` source.

## Verifying a viewer change

Look first. For any visible change, render it on desktop and phone widths (the served viewer, or `scripts/demo-env/`) and look at the screenshots before anything else, and put a rough build in front of the operator early. The highest-value viewer findings have come from the operator looking at the screen, not from agents.

A change only under `ui/`:
- Local: `bun run typecheck`, the vitest files for the changed modules (`bunx vitest related <files> --run`), and `bun run build` to refresh the committed bundle. No Rust tests and no clippy: nothing in Rust changed.
- CI runs the build and bundle-sync check, the full vitest suite, ESLint, knip, the parity corpus and the Playwright viewer e2e (the XSS gate).
- Review: none for layout, styling or copy. One frontier review, briefed to disprove the change's claim, when the change alters what the viewer presents as true: a run's status or color, redaction for remote viewers, or how untrusted text is rendered.

A change that also touches a wire type or an HTTP route: also `bun run types:check` and `cargo t-serve`, and it is a contract change (see the root `CLAUDE.md`'s verification table).

## Rules that live elsewhere

- A run's status and color are decided once (`crates/darkmux-serve/src/run_lifecycle.rs` and `ui/src/lib/lifecycle.ts`, judged by the same corpus `tests/lifecycle/cases.json`); views render them, never re-derive them.
- The work-unit nouns (run, mission, dispatch, role execution, step) mean one grain each on every surface: `docs/agents/CONTRACTS.md`, contract 8.
