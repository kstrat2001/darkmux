//! Crew architecture — Role, Crew, Mission, Sprint schema + loaders.
//!
//! The doctrine in `CLAUDE.md` names "Role + Crew (not Team)" — composition
//! is operator-defined per mission, no fixed membership. This module groups
//! the four schema types and their on-disk loaders. Future Phase B work
//! (SQLite-backed derived index, CRUD verbs) lands here as additional
//! submodules; Phase C (orchestration / dispatch) likewise.
//!
//! Module layout:
//!
//!   crew::types  — schema (Role, Crew, Mission, Sprint, Capability, …)
//!   crew::loader — read JSON manifests from `~/.darkmux/crew/<entity>/`
//!                  with binary-embedded built-ins as fallback

pub mod cli;
pub mod dispatch;
pub mod index;
pub mod loader;
pub mod types;
