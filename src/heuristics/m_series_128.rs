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

use crate::hardware::{HardwareSpec, Platform, RamTier};
use crate::heuristics::{
    Architecture, CompactorChoice, HeuristicsProvider, RuleResult, SizeBucket, TaskClass,
    DEFAULT_COMPACTOR_ID,
};

pub struct Provider;
pub static PROVIDER: Provider = Provider;

impl HeuristicsProvider for Provider {
    fn id(&self) -> &'static str {
        "m-series-128"
    }
    fn description(&self) -> &'static str {
        "Apple Silicon with 96+ GB unified memory (M-series Max / Studio Ultra). Validated against PERFORMANCE.md and the Article 2 lab notebook on M5 Max + 128 GB."
    }

    fn matches(&self, hw: &HardwareSpec) -> bool {
        matches!(hw.platform, Platform::AppleSilicon)
            && matches!(hw.ram_tier(), RamTier::Xl | RamTier::Large)
    }

    fn suggest(
        &self,
        bucket: SizeBucket,
        _arch: Architecture,
        task: TaskClass,
        max_ctx: u32,
    ) -> RuleResult {
        let cap = |n: u32| -> u32 { n.min(max_ctx) };
        let compactor = |n_ctx: u32| -> Option<CompactorChoice> {
            Some(CompactorChoice {
                model_id: DEFAULT_COMPACTOR_ID.to_string(),
                n_ctx,
            })
        };

        match (bucket, task) {
            // Tiny — no compactor at any task class.
            (SizeBucket::Tiny, TaskClass::Fast) => RuleResult {
                primary_n_ctx: cap(32_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Tiny, TaskClass::Mid) => RuleResult {
                primary_n_ctx: cap(64_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Tiny, TaskClass::Long) => RuleResult {
                primary_n_ctx: cap(131_072),
                compactor: None,
                include_compaction_settings: false,
            },

            // Small — no compactor; small models are fast enough that
            // compaction overhead beats compaction savings.
            (SizeBucket::Small, TaskClass::Fast) => RuleResult {
                primary_n_ctx: cap(32_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Small, TaskClass::Mid) => RuleResult {
                primary_n_ctx: cap(64_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Small, TaskClass::Long) => RuleResult {
                primary_n_ctx: cap(131_072),
                compactor: None,
                include_compaction_settings: false,
            },

            // Medium — the v15.5 / bigctx sweet spot. Article 2 reference shapes.
            (SizeBucket::Medium, TaskClass::Fast) => RuleResult {
                primary_n_ctx: cap(32_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Medium, TaskClass::Mid) => RuleResult {
                primary_n_ctx: cap(101_000),
                compactor: compactor(68_000),
                include_compaction_settings: true,
            },
            (SizeBucket::Medium, TaskClass::Long) => RuleResult {
                primary_n_ctx: cap(262_144),
                compactor: compactor(120_000),
                include_compaction_settings: true,
            },

            // Large (50-100B) — RAM tighter even at 128 GB.
            (SizeBucket::Large, TaskClass::Fast) => RuleResult {
                primary_n_ctx: cap(32_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Large, TaskClass::Mid) => RuleResult {
                primary_n_ctx: cap(64_000),
                compactor: compactor(32_000),
                include_compaction_settings: true,
            },
            (SizeBucket::Large, TaskClass::Long) => RuleResult {
                primary_n_ctx: cap(101_000),
                compactor: compactor(64_000),
                include_compaction_settings: true,
            },

            // XL (100B+) — barely fits at any context.
            (SizeBucket::Xl, TaskClass::Fast) => RuleResult {
                primary_n_ctx: cap(32_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Xl, TaskClass::Mid) => RuleResult {
                primary_n_ctx: cap(50_000),
                compactor: compactor(32_000),
                include_compaction_settings: true,
            },
            (SizeBucket::Xl, TaskClass::Long) => RuleResult {
                primary_n_ctx: cap(101_000),
                compactor: compactor(64_000),
                include_compaction_settings: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let r = PROVIDER.suggest(SizeBucket::Medium, Architecture::Moe, TaskClass::Long, 262_144);
        assert_eq!(r.primary_n_ctx, 262_144);
        assert_eq!(r.compactor.as_ref().unwrap().n_ctx, 120_000);
        assert!(r.include_compaction_settings);
    }

    #[test]
    fn medium_mid_is_v15_5() {
        let r = PROVIDER.suggest(SizeBucket::Medium, Architecture::Moe, TaskClass::Mid, 262_144);
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
            let r = PROVIDER.suggest(bucket, Architecture::Dense, TaskClass::Fast, 100_000);
            assert!(r.compactor.is_none(), "fast bucket {bucket:?} got compactor");
        }
    }
}
