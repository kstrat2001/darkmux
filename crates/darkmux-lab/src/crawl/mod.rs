//! The agentic bug crawler (#1959) — packet 1: workspace materialization,
//! rule resolution, and the mechanical `plan` pass that turns a resolved
//! workspace into a token-estimated work-unit plan. NO model dispatch
//! lives here — that's the launcher (`src/crawl_launch.rs`).
//!
//! Rules moved to `darkmux_crew::rules` (#1959 refactor) — a rule is a
//! general template kind, not crawl-specific. This module re-exports
//! nothing from it; callers import `darkmux_crew::rules` directly.

pub mod manifest;
pub mod plan;
pub mod sources;

mod glob;
mod semver;
