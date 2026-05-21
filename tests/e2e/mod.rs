//! End-to-end harness for cross-machine darkmux fleet testing.
//!
//! Boots an in-process mock LMStudio + an external `redis-server` +
//! N `darkmux serve` daemons with distinct `DARKMUX_MACHINE_ID` /
//! `DARKMUX_MACHINE_TIER` / `DARKMUX_REDIS_URL` env vars. Provides
//! helpers to dispatch CLI commands "from" any of the nodes (sets
//! the env vars on the child process) and assert against flow records.
//!
//! Purpose (#255): give every deferred-bug item a TDD home + permanent
//! regression coverage for cross-machine semantics. The harness itself
//! validates the substrate (PR-A through PR-D.1 arc) end-to-end as a
//! reproducible measurement; later scenarios under `tests/e2e/scenarios/`
//! TDD each open MEDIUM into a green test.
//!
//! ## Requirements
//!
//! - `redis-server` on PATH (e.g. `brew install redis`)
//! - `cargo build --release` of darkmux completed (the harness shells
//!   out to `target/release/darkmux`)
//!
//! ## Not yet wired
//!
//! - Auth-protected Redis (the harness boots an open instance on a
//!   loopback port; production uses Tailscale-protected Redis with
//!   requirepass)
//! - Containerization (Docker compose would give cleaner isolation;
//!   process-based is the v1 since it works with `cargo test` directly)

pub mod mock_lmstudio;
pub mod harness;
