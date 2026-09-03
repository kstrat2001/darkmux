//! The agentic bug crawler (#1959) — the mechanical `plan` pass that
//! turns a resolved (materialized) workspace into a token-estimated
//! work-unit plan. NO model dispatch lives here — that's the launcher
//! (`src/crawl_launch.rs`).
//!
//! Rules moved to `darkmux_crew::rules` (#1959 refactor) — a rule is a
//! general template kind, not crawl-specific. This module re-exports
//! nothing from it; callers import `darkmux_crew::rules` directly.
//!
//! `manifest.rs`/`sources.rs` (the crawl-specific corpus manifest + git
//! resolution) retired in the same refactor — `plan::plan` now takes a
//! `darkmux_crew::workspace_spec::Materialized` (materialized by the
//! generic `workspace_spec::materialize`) instead of a `CorpusManifest`
//! resolved by this module's own `sources::resolve`. See `plan.rs`'s own
//! module doc.

pub mod plan;

mod semver;

pub mod plan_step;
