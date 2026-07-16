//! darkmux-profiles — profile registry + LMStudio stack-swap orchestration.
//!
//! Extracted from the binary in #463 (PR2). Holds the profile loader/lookup
//! (`profiles`), the stack-swap orchestrator (`swap`), and the `lms` CLI
//! wrapper. Internal `crate::{lms,swap}` paths keep resolving as sibling
//! modules. `gestalt_host` (#1274 packet 2b) adds the gestalt port adapters
//! (`LmsHost`/`MacProbe`/`ArchFactsReader`), which pull in `darkmux-gestalt`
//! for the port traits — no cycle (gestalt depends only on darkmux-types).
//! `model_ledger` (#1286) composes those adapters into the potential-vs-
//! current memory ledger — ONE implementation consumed by both the
//! `darkmux model ledger` CLI verb and the serve daemon's `/machine/memory`.
//!
//! (2.0, #1405: the `runtime` module — the legacy `openclaw` shell-out
//! runtime's config-file patcher — was removed along with that runtime.)

pub mod crews;
pub mod envelope;
pub mod gestalt_host;
pub mod lms;
pub mod model_ledger;
pub mod profiles;
pub mod swap;
