//! darkmux-profiles — profile registry + LMStudio state helpers.
//!
//! Extracted from the binary in #463 (PR2). Holds the profile loader/lookup
//! (`profiles`), the `darkmux:` namespace helpers (`swap` — the stack-swap
//! orchestrator it once held retired with the `swap` verb, #1426), and the
//! `lms` CLI wrapper. Internal `crate::{lms,swap}` paths keep resolving as
//! sibling modules. `gestalt_host` (#1274 packet 2b) adds the gestalt port adapters
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
pub mod profiles;
pub mod swap;

/// (#2902 step 5) THE preflight for an entry point that starts work: every
/// registered `config.json` enum setting its scope consumes
/// (`darkmux_types::config_enum::preflight`), plus, for a scope that
/// dispatches, every endpoint budget `policy` in the profile registry
/// (`config_enum::bad_endpoint_budget_policies`). A per-endpoint enum lives
/// in `profiles.json`, which `darkmux-types` cannot locate on its own, so
/// this crate (the registry's loader) adds that pass. Every endpoint is
/// checked, not only the ones this run would call: bad config is bad config
/// (#2947). A registry that cannot be loaded adds nothing here; the entry
/// point reports that itself when it resolves a profile.
pub fn preflight(
    scope: darkmux_types::config_enum::Scope,
) -> Result<(), darkmux_types::config_enum::PreflightRefusal> {
    use darkmux_types::config_enum::{self, PreflightRefusal, Scope};
    let mut bad = match config_enum::preflight(scope) {
        Ok(()) => Vec::new(),
        Err(r) => r.bad,
    };
    if matches!(scope, Scope::Dispatch | Scope::MissionLaunch | Scope::LabRun) {
        if let Ok(loaded) = profiles::load_registry_quiet(None) {
            bad.extend(config_enum::bad_endpoint_budget_policies(&loaded.registry));
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(PreflightRefusal { scope, bad })
    }
}
