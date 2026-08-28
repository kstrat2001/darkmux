//! The agentic bug crawler (#1959) — packet 1: corpus manifest, rule
//! loader, source resolution into read-only worktrees, and the mechanical
//! `crawl plan` pass that turns a resolved corpus into a token-estimated
//! work-unit plan. NO model dispatch lives here — that's a later packet.

pub mod manifest;
pub mod plan;
pub mod rules;
pub mod sources;

mod glob;
mod semver;
