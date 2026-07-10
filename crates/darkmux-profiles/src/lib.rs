//! darkmux-profiles — profile registry + LMStudio stack-swap orchestration.
//!
//! Extracted from the binary in #463 (PR2). Holds the profile loader/lookup
//! (`profiles`), the stack-swap orchestrator (`swap`), the `lms` CLI wrapper,
//! and the runtime-config patcher (`runtime`). Internal `crate::{lms,swap,
//! runtime}` paths keep resolving as sibling modules. `gestalt_host` (#1274
//! packet 2b) adds the gestalt port adapters (`LmsHost`/`MacProbe`/
//! `ArchFactsReader`), which pull in `darkmux-gestalt` for the port traits —
//! no cycle (gestalt depends only on darkmux-types).

pub mod crews;
pub mod envelope;
pub mod gestalt_host;
pub mod lms;
pub mod profiles;
pub mod runtime;
pub mod swap;
