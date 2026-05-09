//! Heuristics provider for Apple Silicon at the 64GB tier (popular MBP M4
//! Pro / equivalent).
//!
//! These rules are **derived from the validated 128GB tier**, *not*
//! independently measured. They reduce the canonical n_ctx values to fit a
//! tighter unified-memory budget — bigctx 262K + 120K compactor doesn't
//! fit reliably alongside macOS reserve + apps on a 64GB box. Treat as
//! conservative starting points.
//!
//! Hardware match: Apple Silicon AND RAM tier == Medium (33–64 GB). The
//! lower bound matters — at 32GB even 13B models are tight; users on that
//! tier fall through to `generic`.

use crate::hardware::{HardwareSpec, Platform, RamTier};
use crate::heuristics::{
    Architecture, CompactorChoice, HeuristicsProvider, RuleResult, SizeBucket, TaskClass,
    DEFAULT_COMPACTOR_ID,
};

pub struct Provider;
pub static PROVIDER: Provider = Provider;

const NOTE_DERIVED: &str =
    "Provider `m-series-64` rules are extrapolated from the validated 128GB tier — \
     not independently measured. Treat n_ctx values as starting points; tune down \
     if you see swap pressure or load failures.";

impl HeuristicsProvider for Provider {
    fn id(&self) -> &'static str {
        "m-series-64"
    }
    fn description(&self) -> &'static str {
        "Apple Silicon with 32-64 GB unified memory (M-series Pro tier). Rules derived from the 128GB tier; tighter n_ctx and smaller compactors to fit RAM."
    }

    fn matches(&self, hw: &HardwareSpec) -> bool {
        matches!(hw.platform, Platform::AppleSilicon) && matches!(hw.ram_tier(), RamTier::Medium)
    }

    fn extra_notes(&self) -> &[&'static str] {
        &[NOTE_DERIVED]
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
            // Tiny — same as 128 tier; ctx isn't RAM-bound here.
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

            // Small — same as 128 tier (fits cleanly).
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

            // Medium — RAM gets tight. Drop bigctx; smaller compactor.
            // 35B-A3B at MXFP4 ≈18GB; 4B compactor ≈2.5GB; KV cache scales
            // with n_ctx. At 131K + 64K compactor we're roughly the same
            // RAM envelope as 128-tier's 101K + 68K — fits 64GB safely.
            (SizeBucket::Medium, TaskClass::Fast) => RuleResult {
                primary_n_ctx: cap(32_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Medium, TaskClass::Mid) => RuleResult {
                primary_n_ctx: cap(64_000),
                compactor: compactor(32_000),
                include_compaction_settings: true,
            },
            (SizeBucket::Medium, TaskClass::Long) => RuleResult {
                primary_n_ctx: cap(131_072),
                compactor: compactor(64_000),
                include_compaction_settings: true,
            },

            // Large (50-100B) — RAM critical at 64GB. Best to drop the
            // compactor entirely on Mid; on Long expect tightness.
            (SizeBucket::Large, TaskClass::Fast) => RuleResult {
                primary_n_ctx: cap(32_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Large, TaskClass::Mid) => RuleResult {
                primary_n_ctx: cap(32_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Large, TaskClass::Long) => RuleResult {
                primary_n_ctx: cap(64_000),
                compactor: compactor(32_000),
                include_compaction_settings: true,
            },

            // XL (100B+) — does not fit reliably on 64GB. Allow Fast with
            // a tight ctx; Mid/Long are likely OOM. The dispatcher's
            // Large/XL warning note will fire on top of these.
            (SizeBucket::Xl, TaskClass::Fast) => RuleResult {
                primary_n_ctx: cap(16_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Xl, TaskClass::Mid) => RuleResult {
                primary_n_ctx: cap(32_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Xl, TaskClass::Long) => RuleResult {
                primary_n_ctx: cap(32_000),
                compactor: None,
                include_compaction_settings: false,
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
            physical_cores: 12,
            performance_cores: Some(8),
            efficiency_cores: Some(4),
            has_unified_memory: true,
        }
    }

    #[test]
    fn matches_apple_silicon_at_medium_ram_only() {
        assert!(PROVIDER.matches(&hw_at(64)));
        assert!(PROVIDER.matches(&hw_at(48)));
        assert!(!PROVIDER.matches(&hw_at(96))); // Large tier — 128 provider claims this
        assert!(!PROVIDER.matches(&hw_at(16))); // Small tier — generic
    }

    #[test]
    fn medium_long_is_smaller_than_128_tier_bigctx() {
        let r = PROVIDER.suggest(
            SizeBucket::Medium,
            Architecture::Moe,
            TaskClass::Long,
            262_144,
        );
        // 131K (vs 262K on 128 tier) — RAM-conservative.
        assert_eq!(r.primary_n_ctx, 131_072);
        assert!(r.compactor.is_some());
        assert!(r.compactor.as_ref().unwrap().n_ctx < 120_000);
    }

    #[test]
    fn xl_long_keeps_ctx_minimal() {
        // Don't recommend big context on a 122B model at 64GB.
        let r = PROVIDER.suggest(SizeBucket::Xl, Architecture::Moe, TaskClass::Long, 262_144);
        assert!(r.primary_n_ctx <= 32_000);
        assert!(r.compactor.is_none());
    }

    #[test]
    fn extra_notes_warn_about_extrapolation() {
        let n = PROVIDER.extra_notes();
        assert!(!n.is_empty());
        assert!(
            n.iter().any(|s| s.contains("extrapolated")),
            "expected extrapolation warning: {n:?}"
        );
    }
}
