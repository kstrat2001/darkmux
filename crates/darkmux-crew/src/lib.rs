//! Crew architecture — Role, Crew, Mission, Phase schema + loaders.
//!
//! The doctrine in `CLAUDE.md` names "Role + Crew (not Team)" — composition
//! is operator-defined per mission, no fixed membership. This module groups
//! the four schema types and their on-disk loaders. Future Phase B work
//! (SQLite-backed derived index, CRUD verbs) lands here as additional
//! submodules; Phase C (orchestration / dispatch) likewise.
//!
//! Module layout:
//!
//!   crew::types  — schema (Role, Crew, Mission, Phase, Skill, …)
//!   crew::loader — read JSON manifests from `~/.darkmux/crew/<entity>/`
//!                  with binary-embedded built-ins as fallback

pub mod cli;
// (#1230 Packet 1) Bounded concurrent-dispatch executor over gestalt's
// `plan_waves` — see the module doc. No production caller in this packet;
// Packet 2's `run_step_graph` scheduler is the intended first consumer.
pub mod concurrent_dispatch;
pub mod dispatch;
pub mod dispatch_internal;
pub mod index;
pub mod lessons;
pub mod lifecycle;
pub mod loader;
pub mod pins;
pub mod select;
// (#1230 Packet 2) Generic dependency-graph scheduler over `Step`/`Phase`
// — see the module doc for the DependencyNode/is_ready/reachable/
// run_step_graph shape.
pub mod scheduler;
pub mod single_shot;
// (#1230 Packet 2) Step-kind registry — the execution contract
// `scheduler::run_step_graph` dispatches through.
pub mod step_kinds;
pub mod telemetry_sampler;
pub mod types;
