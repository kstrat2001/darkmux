//! Gestalt host adapters (#1274 packet 2b) — the impure edge of the
//! ports-and-adapters lifecycle core.
//!
//! `darkmux-gestalt` is the pure planning core: facts in, plans out, zero
//! I/O. This module tree is where the I/O actually happens — the concrete
//! implementations of the gestalt ports over the systems darkmux already
//! talks to:
//!
//! - [`LmsHost`] — [`darkmux_gestalt::ModelHost`] over the `lms` CLI, with
//!   the #1276 fix made real: every mutating call runs under an enforced
//!   [`darkmux_gestalt::Deadline`] (spawn + poll + kill), so a wrong model
//!   id can never again hang a dispatch until the workflow's outer kill.
//! - [`MacProbe`] — [`darkmux_gestalt::ResourceProbe`] presenting Apple
//!   Silicon's unified memory as ONE `"unified"` pool (the #1274
//!   pools-as-data decision: the core never branches on platform, only on
//!   pool math).
//! - [`ArchFactsReader`] — the #1286 architecture-facts source: reads a
//!   catalog model's own `config.json` under the LMStudio models root
//!   (layers / kv heads / head_dim / layer_types / quantization bits), the
//!   inputs to the KV-cache "potential" arithmetic. On-disk location is
//!   resolved from the model's `lms ls --json` entry (`path` /
//!   `indexedModelIdentifier`) — the modelKey is NOT the directory for most
//!   real models; catalog-alias models with no matching directory are a
//!   named `None` limitation (see the module docs).
//!
//! These adapters are NEW surface: `swap.rs` / `lms.rs` call paths are
//! untouched (cutover is packet 3). Nothing here mutates host state except
//! through the port methods, and unload targets are claim-checked
//! [`darkmux_gestalt::OwnedTarget`]s — the namespace contract stays
//! structural at the adapter layer too.

mod arch_facts;
mod lms_host;
mod mac_probe;

pub use arch_facts::{ArchFactsRaw, ArchFactsReader};
pub use lms_host::LmsHost;
pub use mac_probe::MacProbe;

use darkmux_gestalt::Deadline;

/// The resolved load/unload deadline for host-port calls — the packet-3
/// executor's one-liner. Resolves `env(DARKMUX_MODEL_LOAD_TIMEOUT_SECONDS) >
/// config.runtime.model_load_timeout_seconds > 600` through
/// `darkmux_types::config_access` (the single precedence home, #661), then
/// carries the bound as a [`Deadline`] — which never reads a clock itself
/// (#1276: the bound is mandatory at the port, resolved visibly here).
pub fn resolved_load_deadline() -> Deadline {
    Deadline::from_secs(darkmux_types::config_access::model_load_timeout_seconds())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[serial_test::serial]
    #[test]
    fn resolved_load_deadline_honors_env_then_default() {
        let k = "DARKMUX_MODEL_LOAD_TIMEOUT_SECONDS";
        let prev = std::env::var(k).ok();
        unsafe { std::env::remove_var(k) };
        // Empty config tier in test builds (#811) → the built-in 600 default.
        assert_eq!(resolved_load_deadline(), Deadline::from_secs(600));
        unsafe { std::env::set_var(k, "45") };
        assert_eq!(resolved_load_deadline(), Deadline::from_secs(45));
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}
