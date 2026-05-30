//! darkmux-profiles — profile registry + LMStudio stack-swap orchestration.
//!
//! Extracted from the binary in #463 (PR2). Holds the profile loader/lookup
//! (`profiles`), the stack-swap orchestrator (`swap`), the `lms` CLI wrapper,
//! and the runtime-config patcher (`runtime`). Internal `crate::{lms,swap,
//! runtime}` paths keep resolving as sibling modules; the only outward
//! dependency is `darkmux-types` for the Profile schema.

pub mod envelope;
pub mod lms;
pub mod profiles;
pub mod runtime;
pub mod swap;
