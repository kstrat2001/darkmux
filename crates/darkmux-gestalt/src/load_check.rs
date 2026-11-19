//! A load is checked against the context it was asked for (#1135 class).
//!
//! `n_ctx` is a minimum ([`ctx_sufficient`]): the same predicate decides
//! whether a resident is reused, so a load that passes here is reused by
//! the next dispatch, and one below it would only be reloaded every time.
//! A host that reports more context than asked (rounding up) passes; one
//! that reports less (rounding down, a cap, an ignored flag) fails.
//!
//! Two checks, one before and one after:
//! - [`refuse_beyond_limit`]: the model's own maximum, as the host's catalog
//!   states it, is below what was asked. Refused before anything is
//!   unloaded or loaded.
//! - [`load_verified`]: the host's own report of the context it loaded at
//!   ([`LoadReport::resolved_ctx`]) is below what was asked. A report that
//!   cannot say (`None`) is not a shortfall: the next residency read
//!   (`lms ps`) still compares the resident's context before reusing it.

use crate::ownership::ctx_sufficient;
use crate::ports::{Deadline, HostError, LoadReport, ModelHost};

/// [`HostError::ContextBeyondModel`] when `limit` (the model's maximum
/// context) is known and below `asked`.
pub fn refuse_beyond_limit(model_key: &str, asked: u32, limit: Option<u64>) -> Result<(), HostError> {
    match limit {
        Some(limit) if !ctx_sufficient(limit, asked) => {
            Err(HostError::ContextBeyondModel { model_key: model_key.to_string(), asked, limit })
        }
        _ => Ok(()),
    }
}

/// [`refuse_beyond_limit`] against `host`'s catalog.
pub fn check_context_limit(host: &mut dyn ModelHost, model_key: &str, asked: u32) -> Result<(), HostError> {
    refuse_beyond_limit(model_key, asked, host.context_limit(model_key))
}

/// `host.load`, then the context it reports against `min_ctx`: a load that
/// landed below it is [`HostError::ContextShort`], naming the model's
/// maximum when the catalog states one. Every production load goes through
/// here, never `ModelHost::load` alone.
pub fn load_verified(
    host: &mut dyn ModelHost,
    model_key: &str,
    identifier: &str,
    min_ctx: u32,
    deadline: Deadline,
) -> Result<LoadReport, HostError> {
    let report = host.load(model_key, identifier, min_ctx, deadline)?;
    match report.resolved_ctx {
        Some(got) if !ctx_sufficient(got, min_ctx) => Err(HostError::ContextShort {
            model_key: model_key.to_string(),
            identifier: identifier.to_string(),
            asked: min_ctx,
            got,
            limit: host.context_limit(model_key),
        }),
        _ => Ok(report),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{HostOp, MockHost};

    const D: Deadline = Deadline(std::time::Duration::from_secs(1));

    #[test]
    fn a_limit_below_the_asked_context_is_refused_and_anything_else_is_not() {
        assert_eq!(
            refuse_beyond_limit("m", 32_768, Some(4_096)),
            Err(HostError::ContextBeyondModel { model_key: "m".into(), asked: 32_768, limit: 4_096 })
        );
        for limit in [Some(32_768), Some(131_072), None] {
            assert_eq!(refuse_beyond_limit("m", 32_768, limit), Ok(()), "limit {limit:?}");
        }
    }

    #[test]
    fn the_limit_is_read_from_the_hosts_catalog() {
        let mut host = MockHost::new().context_limited("m", 4_096);
        assert!(matches!(check_context_limit(&mut host, "m", 8_192), Err(HostError::ContextBeyondModel { limit: 4_096, .. })));
        assert_eq!(check_context_limit(&mut host, "m", 4_096), Ok(()));
        assert_eq!(check_context_limit(&mut host, "other", 1_000_000), Ok(()), "no stated limit, no refusal");
        assert!(host.ops.is_empty(), "reading the limit mutates nothing: {:?}", host.ops);
    }

    #[test]
    fn a_load_the_host_reports_below_the_asked_context_is_a_shortfall() {
        let mut host = MockHost::new().context_limited("m", 131_072);
        host.loads_at = Some(4_096);
        let err = load_verified(&mut host, "m", "darkmux:m", 32_768, D).unwrap_err();
        assert_eq!(
            err,
            HostError::ContextShort { model_key: "m".into(), identifier: "darkmux:m".into(), asked: 32_768, got: 4_096, limit: Some(131_072) }
        );
        assert!(matches!(host.ops.as_slice(), [HostOp::Load { .. }]), "the load ran once: {:?}", host.ops);
    }

    #[test]
    fn a_load_at_or_above_the_asked_context_or_unreported_is_ok() {
        for (loads_at, asked) in [(None, 32_768), (Some(32_768), 32_768), (Some(40_960), 32_768)] {
            let mut host = MockHost::new();
            host.loads_at = loads_at;
            assert!(load_verified(&mut host, "m", "darkmux:m", asked, D).is_ok(), "loads at {loads_at:?}");
        }
        let mut unreported = Unreported;
        assert_eq!(load_verified(&mut unreported, "m", "darkmux:m", 32_768, D), Ok(LoadReport::default()));
    }

    #[test]
    fn a_failed_load_is_its_own_error() {
        let mut host = MockHost::new();
        host.fail_next_load = Some(HostError::UnknownModel { model_key: "m".into() });
        assert_eq!(load_verified(&mut host, "m", "darkmux:m", 4_096, D), Err(HostError::UnknownModel { model_key: "m".into() }));
    }

    /// A host whose load cannot say what context it landed at.
    struct Unreported;

    impl ModelHost for Unreported {
        fn list_resident(&mut self) -> Result<Vec<crate::ResidentFact>, HostError> {
            Ok(Vec::new())
        }
        fn list_catalog(&mut self) -> Result<Vec<crate::CatalogFact>, HostError> {
            Ok(Vec::new())
        }
        fn load(&mut self, _: &str, _: &str, _: u32, _: Deadline) -> Result<LoadReport, HostError> {
            Ok(LoadReport::default())
        }
        fn unload(&mut self, _: &crate::OwnedTarget, _: Deadline) -> Result<(), HostError> {
            Ok(())
        }
    }

    #[test]
    fn each_shortfall_says_what_was_asked_what_it_got_and_what_to_change() {
        let beyond = HostError::ContextBeyondModel { model_key: "m".into(), asked: 32_768, limit: 4_096 }.to_string();
        for part in ["`m`", "at most 4096", "n_ctx=32768", "not loaded", "`n_ctx` to 4096 or less"] {
            assert!(beyond.contains(part), "{part}: {beyond}");
        }
        let short = |limit| {
            HostError::ContextShort { model_key: "m".into(), identifier: "darkmux:m".into(), asked: 32_768, got: 4_096, limit }.to_string()
        };
        for limit in [None, Some(4_096), Some(131_072)] {
            let text = short(limit);
            for part in ["`darkmux:m`", "context 4096", "n_ctx=32768", "left loaded", "machine eject", "`n_ctx` to 4096 or less"] {
                assert!(text.contains(part), "{limit:?} {part}: {text}");
            }
        }
        assert!(short(Some(4_096)).contains("takes at most 4096"));
        assert!(short(Some(131_072)).contains("states a maximum of 131072: the engine capped the load"));
        assert!(short(None).contains("may take less than was asked"));
    }
}
