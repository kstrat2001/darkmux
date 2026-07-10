//! Pluggable footprint estimation (#1274): the estimate is itself a fact
//! source, so it can later be refined from observed post-load residency
//! (#1257 load-provenance records) without changing the core's decision
//! logic.

use crate::facts::CatalogFact;
use serde::{Deserialize, Serialize};
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

/// The probe-measured post-load overhead the architecture-aware estimate
/// adds on top of weights + KV: runtime buffers, Metal/MLX working set, and
/// the other transients observed between the arithmetic and the live
/// footprint (2026-07-10 M5 Max probes, #1286). Refined per model by #1257
/// observed residency later.
pub const DEFAULT_TRANSIENT_MARGIN_BYTES: u64 = 750_000_000;

/// Per-model architecture facts (#1286), read from the model's OWN
/// config.json. The reader is I/O and ships with the packet-2 LmsHost
/// adapter — arch facts arrive HERE as data, keeping the crate pure.
///
/// Hybrid linear-attention models (the Qwen 3.5/3.6 generation) need no
/// special casing: only their full-attention layers hold a KV cache, so
/// they simply carry a small `full_attention_layers` count. That is the
/// whole '4B doesn't mean 4GB' inversion (#1286): the probed 35B MoE judge
/// is 10 full-attention layers of 40 and costs 20 KB/token, while the
/// mid-size dense devstral (all 40 layers full) costs 160 KB/token — 8× —
/// and even the 4B costs 144 KB/token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchFacts {
    /// Total transformer layers — provenance for the "x of y full-attention"
    /// reading; not a term in the KV arithmetic.
    pub total_layers: u32,
    /// Layers that hold a KV cache: `== total_layers` on dense models, a
    /// small fraction on hybrid linear-attention models.
    pub full_attention_layers: u32,
    pub kv_heads: u32,
    pub head_dim: u32,
    /// Bytes per cached element (2 = fp16 cache).
    pub kv_bytes_per_element: u32,
}

impl ArchFacts {
    /// KV-cache bytes per context token:
    /// `2 (K and V) × full_attention_layers × kv_heads × head_dim ×
    /// kv_bytes_per_element`. Probed values (#1286): 35B judge 20 KB/token,
    /// devstral 160 KB/token, 4B 144 KB/token.
    pub fn kv_per_token(&self) -> u64 {
        2 * u64::from(self.full_attention_layers)
            * u64::from(self.kv_heads)
            * u64::from(self.head_dim)
            * u64::from(self.kv_bytes_per_element)
    }
}

/// Architecture-aware estimator (#1286):
/// `catalog size_bytes + kv_per_token(arch) × min_ctx +
/// transient_margin_bytes` — the potential-commitment number the memory
/// ledger charts and the #1285 wave scheduler packs against.
///
/// A model with no arch facts (or no catalog size) estimates as `None` —
/// unknowable, deliberately NOT a [`V1Estimator`]-style fallback: silently
/// degrading to the coarse fixed margin would hide exactly the
/// '4B ≠ 4GB' error class this estimator exists to close, so the planner's
/// [`crate::plan::Warning::LoadEstimateUnknown`] fires instead. Callers
/// wanting a fallback compose one explicitly (a chain estimator trying Arch
/// then V1 is a legitimate composition — if built, it gets its own tests) —
/// the fallback is then visible in the wiring, never implicit in the math.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchEstimator {
    /// model key → arch facts, for the models the adapter could read.
    pub arch: BTreeMap<String, ArchFacts>,
    /// Post-load overhead added to every estimate; defaults to the
    /// probe-measured [`DEFAULT_TRANSIENT_MARGIN_BYTES`].
    pub transient_margin_bytes: u64,
}

impl ArchEstimator {
    pub fn new(arch: BTreeMap<String, ArchFacts>) -> Self {
        ArchEstimator { arch, transient_margin_bytes: DEFAULT_TRANSIENT_MARGIN_BYTES }
    }
}

impl FootprintEstimator for ArchEstimator {
    fn estimate_bytes(
        &self,
        model_key: &str,
        min_ctx: u32,
        catalog: Option<&[CatalogFact]>,
    ) -> Option<u64> {
        let arch = self.arch.get(model_key)?;
        let size = catalog?
            .iter()
            .find(|c| c.model_key == model_key)?
            .size_bytes?;
        Some(size + arch.kv_per_token() * u64::from(min_ctx) + self.transient_margin_bytes)
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

    // ── #1286 architecture-aware rows ────────────────────────────────────
    // The 2026-07-10 M5 Max config.json probes behind #1286, as ArchFacts.

    /// 35B MoE judge: 40 layers, 10 full-attention (hybrid linear-attention,
    /// Qwen 3.6 generation), kv_heads 2, head_dim 256, fp16 cache.
    fn judge_arch() -> ArchFacts {
        ArchFacts {
            total_layers: 40,
            full_attention_layers: 10,
            kv_heads: 2,
            head_dim: 256,
            kv_bytes_per_element: 2,
        }
    }

    /// devstral 24B dense: all 40 layers full attention, kv_heads 8,
    /// head_dim 128, fp16 cache.
    fn devstral_arch() -> ArchFacts {
        ArchFacts {
            total_layers: 40,
            full_attention_layers: 40,
            kv_heads: 8,
            head_dim: 128,
            kv_bytes_per_element: 2,
        }
    }

    /// 4B dense: all 36 layers full attention, kv_heads 8, head_dim 128,
    /// fp16 cache.
    fn qwen_4b_arch() -> ArchFacts {
        ArchFacts {
            total_layers: 36,
            full_attention_layers: 36,
            kv_heads: 8,
            head_dim: 128,
            kv_bytes_per_element: 2,
        }
    }

    #[test]
    fn arch_kv_per_token_matches_probed_configs() {
        // The '4B doesn't mean 4GB' inversion (#1286): the hybrid 35B is
        // the CHEAP KV — the mid-size dense model is the hog at 8×, and
        // even the 4B costs 7× the 35B per token. No special casing for
        // the hybrid: a small full_attention_layers count IS the model.
        assert_eq!(judge_arch().kv_per_token(), 20_480); // 20 KB/token
        assert_eq!(devstral_arch().kv_per_token(), 163_840); // 160 KB/token
        assert_eq!(qwen_4b_arch().kv_per_token(), 147_456); // 144 KB/token
    }

    #[test]
    fn arch_estimator_exact_bytes_from_probed_configs() {
        let est = ArchEstimator::new(BTreeMap::from([
            ("judge".to_string(), judge_arch()),
            ("devstral".to_string(), devstral_arch()),
            ("qwen-4b".to_string(), qwen_4b_arch()),
        ]));
        let catalog = vec![
            CatalogFact { model_key: "judge".into(), size_bytes: Some(17_180_000_000) },
            CatalogFact { model_key: "devstral".into(), size_bytes: Some(13_000_000_000) },
            CatalogFact { model_key: "qwen-4b".into(), size_bytes: Some(2_300_000_000) },
        ];
        // judge @ 65536 (the profile ctx probed on #1286): 17.18GB weights
        // + 20480 B/token × 65536 = 1,342,177,280 KV (the probe's "1.25GB"
        // — a GiB reading) + the 750MB margin.
        assert_eq!(
            est.estimate_bytes("judge", 65_536, Some(&catalog)),
            Some(17_180_000_000 + 1_342_177_280 + 750_000_000)
        );
        // devstral @ 32768: 163840 × 32768 = 5,368,709,120 KV (the probe's
        // "5.0GB" — GiB) atop the weights + margin.
        assert_eq!(
            est.estimate_bytes("devstral", 32_768, Some(&catalog)),
            Some(13_000_000_000 + 5_368_709_120 + 750_000_000)
        );
        // 4B @ 32768: 147456 × 32768 = 4,831,838,208 — the little dense
        // model's KV alone is ~4.8GB at 32k ('4B doesn't mean 4GB').
        assert_eq!(
            est.estimate_bytes("qwen-4b", 32_768, Some(&catalog)),
            Some(2_300_000_000 + 4_831_838_208 + 750_000_000)
        );
    }

    #[test]
    fn arch_estimator_default_margin_is_probe_measured() {
        let est = ArchEstimator::new(BTreeMap::new());
        assert_eq!(est.transient_margin_bytes, DEFAULT_TRANSIENT_MARGIN_BYTES);
        // Freeze the documented probe-measured value (#1286).
        assert_eq!(DEFAULT_TRANSIENT_MARGIN_BYTES, 750_000_000);
    }

    #[test]
    fn arch_estimator_unknowable_without_arch_facts() {
        // No arch facts for the key ⇒ None — deliberately NOT a silent
        // V1-style fallback (see the type docs): the planner's
        // LoadEstimateUnknown warning fires instead, and any Arch→V1
        // fallback is composed explicitly by the caller.
        let catalog = vec![CatalogFact { model_key: "judge".into(), size_bytes: Some(1) }];
        let est = ArchEstimator::new(BTreeMap::new());
        assert_eq!(est.estimate_bytes("judge", 65_536, Some(&catalog)), None);
    }

    #[test]
    fn arch_estimator_unknowable_without_a_catalog_size() {
        let est = ArchEstimator::new(BTreeMap::from([("judge".to_string(), judge_arch())]));
        // No catalog at all.
        assert_eq!(est.estimate_bytes("judge", 65_536, None), None);
        // Cataloged but sizeless.
        let sizeless = vec![CatalogFact { model_key: "judge".into(), size_bytes: None }];
        assert_eq!(est.estimate_bytes("judge", 65_536, Some(&sizeless)), None);
    }
}
