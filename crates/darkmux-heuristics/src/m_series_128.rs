//! Heuristics provider for Apple Silicon at the 128GB+ tier.
//!
//! These rules are the *validated* canonical shapes from PERFORMANCE.md
//! and the lab notebook — bigctx (262K + 120K compactor) for medium-bucket
//! MoE on long-agentic, v15.5 (101K + 68K compactor + the v15.5 knobs)
//! for medium mid, etc. They are the empirical floor of darkmux: every
//! other provider is either an extrapolation from these or a fallback.
//!
//! Hardware match: Apple Silicon AND RAM tier ≥ Large (i.e. 65 GB+).
//! Below 65 GB the bigctx shapes won't fit; defer to `m_series_64`.

use darkmux_hardware::{HardwareSpec, Platform, RamTier};
use crate::{HeuristicsProvider, Rule, RulesTable};

pub struct Provider;
pub static PROVIDER: Provider = Provider;

/// Rows are size buckets, columns Fast / Mid / Long.
static RULES: RulesTable = RulesTable([
    // Tiny — no compactor at any task class.
    [Rule::solo(32_000), Rule::solo(64_000), Rule::solo(131_072)],
    // Small — no compactor; small models are fast enough that compaction
    // overhead beats compaction savings.
    [Rule::solo(32_000), Rule::solo(64_000), Rule::solo(131_072)],
    // Medium — the v15.5 / bigctx sweet spot. Article 2 reference shapes.
    [Rule::solo(32_000), Rule::paired(101_000, 68_000), Rule::paired(262_144, 120_000)],
    // Large (50-100B) — RAM tighter even at 128 GB.
    [Rule::solo(32_000), Rule::paired(64_000, 32_000), Rule::paired(101_000, 64_000)],
    // XL (100B+) — barely fits at any context.
    [Rule::solo(32_000), Rule::paired(50_000, 32_000), Rule::paired(101_000, 64_000)],
]);

impl HeuristicsProvider for Provider {
    fn id(&self) -> &'static str {
        "m-series-128"
    }

    fn matches(&self, hw: &HardwareSpec) -> bool {
        matches!(hw.platform, Platform::AppleSilicon)
            && matches!(hw.ram_tier(), RamTier::Xl | RamTier::Large)
    }

    fn rules(&self) -> &'static RulesTable {
        &RULES
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SizeBucket, TaskClass};

    fn hw_at(ram_gb: u32) -> HardwareSpec {
        HardwareSpec {
            platform: Platform::AppleSilicon,
            arch: "aarch64".into(),
            total_ram_gb: ram_gb,
            physical_cores: 16,
            performance_cores: Some(12),
            efficiency_cores: Some(4),
            has_unified_memory: true,
        }
    }

    #[test]
    fn matches_apple_silicon_at_large_or_xl_ram() {
        assert!(PROVIDER.matches(&hw_at(128)));
        assert!(PROVIDER.matches(&hw_at(96)));
        // 64GB is medium tier — should NOT match (m_series_64 picks it up).
        assert!(!PROVIDER.matches(&hw_at(64)));
        // Below medium definitely no match.
        assert!(!PROVIDER.matches(&hw_at(16)));
    }

    #[test]
    fn does_not_match_non_apple_silicon() {
        let mut hw = hw_at(128);
        hw.platform = Platform::Linux;
        assert!(!PROVIDER.matches(&hw));
    }

    #[test]
    fn medium_long_is_bigctx() {
        let r = PROVIDER.suggest(SizeBucket::Medium, TaskClass::Long, 262_144);
        assert_eq!(r.primary_n_ctx, 262_144);
        assert_eq!(r.compactor.as_ref().unwrap().n_ctx, 120_000);
    }

    #[test]
    fn medium_mid_is_v15_5() {
        let r = PROVIDER.suggest(SizeBucket::Medium, TaskClass::Mid, 262_144);
        assert_eq!(r.primary_n_ctx, 101_000);
        assert_eq!(r.compactor.as_ref().unwrap().n_ctx, 68_000);
    }

    #[test]
    fn fast_never_pairs_compactor() {
        for bucket in [
            SizeBucket::Tiny,
            SizeBucket::Small,
            SizeBucket::Medium,
            SizeBucket::Large,
            SizeBucket::Xl,
        ] {
            let r = PROVIDER.suggest(bucket, TaskClass::Fast, 100_000);
            assert!(r.compactor.is_none(), "fast bucket {bucket:?} got compactor");
        }
    }
}
