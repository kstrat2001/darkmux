//! darkmux-profiles — profile registry + LMStudio state helpers.
//!
//! Extracted from the binary in #463 (PR2). Holds the profile loader/lookup
//! (`profiles`), the `darkmux:` ownership contract (`ownership`: the
//! namespace helpers and the managed-resident eject), and the `lms` CLI
//! wrapper. `gestalt_host` (#1274 packet 2b) adds the gestalt port adapters
//! (`LmsHost`/`MacProbe`/`ArchFactsReader`), which pull in `darkmux-gestalt`
//! for the port traits — no cycle (gestalt depends only on darkmux-types).
//! `model_ledger` (#1286) composes those adapters into the potential-vs-
//! current memory ledger — ONE implementation consumed by both the
//! `darkmux machine resources` CLI verb and the serve daemon's `/machine/resources`.
//!
//! (2.0, #1405: the `runtime` module — the legacy `openclaw` shell-out
//! runtime's config-file patcher — was removed along with that runtime.)

pub mod envelope;
pub mod gestalt_host;
pub mod lms;
pub mod model_ledger;
pub mod ownership;
pub mod profiles;
