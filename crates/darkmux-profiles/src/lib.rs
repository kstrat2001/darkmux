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

/// (#2902 step 5) THE preflight for an entry point that starts work: every
/// registered `config.json` enum setting its scope consumes
/// (`darkmux_types::config_enum::preflight`), plus, for a scope that
/// dispatches, every endpoint budget in the profile registry: an
/// unregistered `policy` (`config_enum::bad_endpoint_budget_policies`) and
/// `limits` that cannot be used as written
/// (`config_enum::invalid_endpoint_limits`). A per-endpoint enum lives
/// in `profiles.json`, which `darkmux-types` cannot locate on its own, so
/// this crate (the registry's loader) adds that pass. Every endpoint is
/// checked, not only the ones this run would call: bad config is bad config
/// (#2947). A registry that cannot be loaded adds nothing here; the entry
/// point reports that itself when it resolves a profile.
pub fn preflight(
    scope: darkmux_types::config_enum::Scope,
) -> Result<(), darkmux_types::config_enum::PreflightRefusal> {
    preflight_with(scope, None)
}

/// [`preflight`] against the registry the command itself uses:
/// `profiles_file` is its `--profiles-file` (or equivalent), `None` for the
/// default search (`DARKMUX_PROFILES`, then the default locations). A
/// registry that cannot be loaded adds nothing here.
pub fn preflight_with(
    scope: darkmux_types::config_enum::Scope,
    profiles_file: Option<&str>,
) -> Result<(), darkmux_types::config_enum::PreflightRefusal> {
    use darkmux_types::config_enum::{self, PreflightRefusal, Scope};
    let mut bad = match config_enum::preflight(scope) {
        Ok(()) => Vec::new(),
        Err(r) => r.bad,
    };
    let mut invalid = Vec::new();
    if matches!(scope, Scope::Dispatch | Scope::MissionLaunch | Scope::LabRun) {
        warn_renamed_leftovers_once();
        if let Ok(loaded) = profiles::load_registry_quiet(profiles_file) {
            bad.extend(config_enum::bad_endpoint_budget_policies(&loaded.registry));
            invalid.extend(config_enum::invalid_endpoint_limits(&loaded.registry));
        }
    }
    if bad.is_empty() && invalid.is_empty() {
        Ok(())
    } else {
        Err(PreflightRefusal { scope, bad, invalid })
    }
}

/// (#2902 step 5) A leftover RENAMED setting (the pre-4.0
/// `remote.max_tokens_per_execution`, in config.json or the env) is read by
/// nothing. It is never refused, but it must not silently do nothing: every
/// entry point that dispatches says so, once per process, on stderr.
fn warn_renamed_leftovers_once() {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let leftovers = darkmux_types::config_access::renamed_setting_leftovers();
    if leftovers.is_empty() || WARNED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    for l in leftovers {
        eprintln!("darkmux: ⚠ {}", l.line);
    }
}
