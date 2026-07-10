//! Pluggable footprint estimation (#1274): the estimate is itself a fact
//! source, so it can later be refined from observed post-load residency
//! (#1257 load-provenance records) without changing the core's decision
//! logic.

use crate::facts::CatalogFact;
use std::collections::BTreeMap;

/// Footprint fact source for pending loads. Implementations MUST be pure
/// (no I/O) — the catalog fact is passed in.
pub trait FootprintEstimator {
    /// Estimated resident bytes for loading `model_key` at `min_ctx`.
    /// `None` = unknowable (missing catalog / size) → the planner warns
    /// ([`crate::plan::Warning::LoadEstimateUnknown`]) and budget math
    /// treats it as 0 — a documented degradation, never a panic.
    fn estimate_bytes(
        &self,
        model_key: &str,
        min_ctx: u32,
        catalog: Option<&[CatalogFact]>,
    ) -> Option<u64>;
}

/// v1 (#1274 estimation decision): catalog file size + a conservative fixed
/// KV-cache margin per requested ctx token. Refined later from #1257
/// observed post-load residency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V1Estimator {
    pub kv_bytes_per_ctx_token: u64,
}

impl FootprintEstimator for V1Estimator {
    fn estimate_bytes(
        &self,
        model_key: &str,
        min_ctx: u32,
        catalog: Option<&[CatalogFact]>,
    ) -> Option<u64> {
        let size = catalog?
            .iter()
            .find(|c| c.model_key == model_key)?
            .size_bytes?;
        Some(size + self.kv_bytes_per_ctx_token * u64::from(min_ctx))
    }
}

/// Testkit: exact per-model answers keyed by model key (ctx and catalog are
/// ignored) — budget table rows stay one line. An absent key estimates as
/// `None` (the unknowable path).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FixedEstimator(pub BTreeMap<String, u64>);

impl FootprintEstimator for FixedEstimator {
    fn estimate_bytes(
        &self,
        model_key: &str,
        _min_ctx: u32,
        _catalog: Option<&[CatalogFact]>,
    ) -> Option<u64> {
        self.0.get(model_key).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_estimator_answers_from_its_map() {
        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 42_u64)]));
        assert_eq!(est.estimate_bytes("m", 8_000, None), Some(42));
        assert_eq!(est.estimate_bytes("absent", 8_000, None), None);
    }

    #[test]
    fn v1_estimator_adds_kv_margin_to_catalog_size() {
        let catalog = vec![CatalogFact { model_key: "m".into(), size_bytes: Some(1_000_000) }];
        let est = V1Estimator { kv_bytes_per_ctx_token: 10 };
        assert_eq!(
            est.estimate_bytes("m", 8_000, Some(&catalog)),
            Some(1_000_000 + 10 * 8_000)
        );
    }

    #[test]
    fn v1_estimator_is_unknowable_without_a_catalog_size() {
        let est = V1Estimator { kv_bytes_per_ctx_token: 10 };
        // No catalog at all.
        assert_eq!(est.estimate_bytes("m", 8_000, None), None);
        // Cataloged but sizeless.
        let sizeless = vec![CatalogFact { model_key: "m".into(), size_bytes: None }];
        assert_eq!(est.estimate_bytes("m", 8_000, Some(&sizeless)), None);
        // Not cataloged.
        let other = vec![CatalogFact { model_key: "other".into(), size_bytes: Some(1) }];
        assert_eq!(est.estimate_bytes("m", 8_000, Some(&other)), None);
    }
}
