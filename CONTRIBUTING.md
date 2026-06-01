# Contributing to darkmux

darkmux is a Rust CLI. The single binary is `darkmux`, built with `cargo`.

## Quick start

```bash
git clone https://github.com/kstrat2001/darkmux
cd darkmux
cargo build --release
cargo test
```

The release binary lands at `target/release/darkmux`. To install it on `$PATH`:

```bash
cargo install --path .
```

This produces a self-contained binary — built-in workloads (`templates/builtin/workloads/`) are embedded at compile time, so the binary works from any directory without the source tree.

## Development loop

```bash
cargo build              # debug build (faster compile)
cargo test               # run all tests
cargo test <name>        # run a specific test by name
cargo clippy             # lint
cargo fmt                # format
```

If you modify the embedded workload manifests under `templates/builtin/workloads/`, you must rebuild — `include_str!` resolves at compile time.

## Code style

- Rust 2021 edition, MSRV 1.80
- `cargo fmt` before every commit
- `cargo clippy` clean (warnings tolerated for now in pre-1.0 dead-code paths; new warnings in changed files should be fixed)
- Single-purpose PRs
- Conventional commit messages (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`)
- New external dependencies are scrutinized — darkmux deliberately keeps the dep surface small (see `Cargo.toml`). If a 10-line inline module avoids a crate, prefer that.

## Tests

- New features should include tests
- Bug fixes should include a regression test
- Tests that mutate process-level state (`current_dir`, environment variables) must use `#[serial_test::serial]` to avoid races with parallel test runs
- Integration tests live in `tests/cli.rs`. They spawn the compiled binary via `assert_cmd` and assert on its observable surface (stdout/stderr/exit code/run-dir contents)

Tests that depend on a real `lms` binary or a real LMStudio runtime should set `DARKMUX_LMS_BIN=/usr/bin/true` (or similar) so the test runs without those dependencies.

## Issue reports

When filing a bug, please include:

- darkmux version (`darkmux --version`)
- LMStudio version (`lms --version`) if relevant
- Output of `darkmux status`
- A minimal `profiles.json` that reproduces the issue (the `profiles.example.json` in the repo is a starting point)
- The CWD where you ran the command (project-local `.darkmux/` vs. user-global `~/.darkmux/` resolution affects path-related issues)

## Project structure

darkmux is a Cargo **workspace** — most code lives in focused crates under `crates/` (`darkmux-types`, `darkmux-profiles`, `darkmux-crew`, `darkmux-flow`, `darkmux-lab`, `darkmux-serve`, `darkmux-eureka`, `darkmux-doctor`, `darkmux-fleet`, …), with the agent runtime as its own excluded crate at `runtime/`. The thin binary entrypoint and the CLI verb modules (`flow_cli.rs`, `mission_propose.rs`, `role_cli.rs`, `sprint_cli.rs`, …) live in `src/`. Embedded assets (workload / role / skill manifests + prompts) live under `templates/builtin/`; integration tests are several `*.rs` files under `tests/` that spawn the compiled binary via `assert_cmd`.

The **authoritative, kept-current** map of where each module lives is the **"Where things live"** section of [`CLAUDE.md`](CLAUDE.md) — refer to it rather than a parallel list here. A duplicated map is exactly what drifts: this section previously described a pre-workspace `src/` monolith that no longer exists.

## Releases

darkmux is pre-1.0. Versioning is informal until v1.0. The maintainer cuts releases manually:

```bash
# bump version in Cargo.toml
cargo build --release
cargo test
git tag vX.Y.Z
git push origin vX.Y.Z
```
