//! Heuristics provider for Apple Silicon at the 32GB tier (Mac Studio / MacBook Pro).
//!
//! These rules are **extrapolated from `m_series_64`** with further reductions
//! to fit an even tighter unified-memory budget. They are *not* independently
//! measured — treat `n_ctx` values as conservative starting points; tune down if
//! you see swap pressure or load failures.
//!
//! **Compactor pairing rationale:** At ~32 GB unified memory, a small compactor
//! (e.g. qwen3-4b-instruct in MLX4 ≈ 2–3 GB) consumes a meaningful fraction of
//! the RAM budget. KV cache pre-allocation for the compactor further reduces headroom
//! for the primary model's working set. Default policy: only pair a compactor when
//! Long agentic workloads would otherwise run out of context — Mid tasks don't need it,
//! and Fast tasks never benefit. Operators on this tier should monitor RSS; if swap
//! pressure appears, drop the compactor and reduce `n_ctx` further.
//!
//! Hardware match: Apple Silicon AND RAM tier == Small (0–32 GB).

use crate::hardware::{HardwareSpec, Platform, RamTier};
use crate::heuristics::{
    Architecture, CompactorChoice, HeuristicsProvider, RuleResult, SizeBucket, TaskClass,
    DEFAULT_COMPACTOR_ID,
};

pub struct Provider;
pub static PROVIDER: Provider = Provider;

const NOTE_EXTRAPOLATED: &str =
    "Provider `m-series-32` rules are extrapolated from the validated 64GB tier with further \
     reductions for a ~32 GB unified memory budget. Compactor pairing is conservative (only \
     Medium Long); tune down `n_ctx` if you see swap pressure.";

impl HeuristicsProvider for Provider {
    fn id(&self) -> &'static str {
        "m-series-32"
    }
    fn description(&self) -> &'static str {
        "Apple Silicon with 0–32 GB unified memory (Mac Studio / MacBook Pro). Rules extrapolated from m_series_64 with further reductions; compactor pairing is conservative due to tight RAM headroom. Not yet empirically validated."
    }

    fn matches(&self, hw: &HardwareSpec) -> bool {
        matches!(hw.platform, Platform::AppleSilicon) && matches!(hw.ram_tier(), RamTier::Small)
    }

    fn extra_notes(&self) -> &[&'static str] {
        &[NOTE_EXTRAPOLATED]
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
            // Tiny — no compactor needed; models are fast enough.
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

            // Medium — RAM gets tight. Conservative n_ctx; compactor only on Long
            // where context accumulation matters most. Mid doesn't pair a compactor:
            // at ~32 GB, KV pre-allocation for even a small model eats enough headroom
            // that the benefit is marginal; better to keep it for Long tasks.
            (SizeBucket::Medium, TaskClass::Fast) => RuleResult {
                primary_n_ctx: cap(32_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Medium, TaskClass::Mid) => RuleResult {
                primary_n_ctx: cap(64_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Medium, TaskClass::Long) => RuleResult {
                primary_n_ctx: cap(64_000),
                compactor: compactor(32_000),
                include_compaction_settings: true,
            },

            // Large (50–100B) — RAM critical at 32 GB. Best to drop compactor;
            // tight context windows to leave headroom for model weights + OS.
            (SizeBucket::Large, TaskClass::Fast) => RuleResult {
                primary_n_ctx: cap(16_000),
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
                compactor: None,
                include_compaction_settings: false,
            },

            // XL (100B+) — unlikely to fit reliably on 32 GB. Minimal context.
            (SizeBucket::Xl, TaskClass::Fast) => RuleResult {
                primary_n_ctx: cap(8_000),
                compactor: None,
                include_compaction_settings: false,
            },
            (SizeBucket::Xl, TaskClass::Mid) => RuleResult {
                primary_n_ctx: cap(16_000),
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
            physical_cores: 10,
            performance_cores: Some(6),
            efficiency_cores: Some(4),
            has_unified_memory: true,
        }
    }

    #[test]
    fn matches_apple_silicon_at_small_ram_only() {
        // Small tier: 0–32 GB
        assert!(PROVIDER.matches(&hw_at(8)));
        assert!(PROVIDER.matches(&hw_at(16)));
        assert!(PROVIDER.matches(&hw_at(32)));

        // Medium tier: 33–64 GB — should NOT match (m_series_64 claims this)
        assert!(!PROVIDER.matches(&hw_at(33)));
        assert!(!PROVIDER.matches(&hw_at(48)));
        assert!(!PROVIDER.matches(&hw_at(64)));

        // Large/Xl tiers
        assert!(!PROVIDER.matches(&hw_at(96))); // Large tier — m_series_128 claims this
        assert!(!PROVIDER.matches(&hw_at(128))); // Xl tier

        // Non-Apple Silicon
        let mut non_as = hw_at(16);
        non_as.platform = Platform::Linux;
        assert!(!PROVIDER.matches(&non_as));

        let mut mac_intel = hw_at(16);
        mac_intel.platform = Platform::MacIntel;
        assert!(!PROVIDER.matches(&mac_intel));
    }

    #[test]
    fn medium_long_has_compactor_with_reduced_ctx() {
        // Medium + Long: pairs a compactor (conservative), ctx capped at 64K
        let r = PROVIDER.suggest(
            SizeBucket::Medium,
            Architecture::Moe,
            TaskClass::Long,
            262_144,
        );
        assert_eq!(r.primary_n_ctx, 64_000);
        assert!(r.compactor.is_some());
        assert_eq!(r.compactor.as_ref().unwrap().n_ctx, 32_000);
        assert!(r.include_compaction_settings);
    }

    #[test]
    fn medium_mid_no_compactor() {
        // Medium + Mid: no compactor at this tier (RAM headroom too tight)
        let r = PROVIDER.suggest(
            SizeBucket::Medium,
            Architecture::Moe,
            TaskClass::Mid,
            262_144,
        );
        assert_eq!(r.primary_n_ctx, 64_000);
        assert!(r.compactor.is_none());
        assert!(!r.include_compaction_settings);
    }

    #[test]
    fn xl_long_keeps_ctx_minimal() {
        // Don't recommend wide context on a 120B+ model at 32 GB.
        let r = PROVIDER.suggest(SizeBucket::Xl, Architecture::Dense, TaskClass::Long, 262_144);
        assert!(r.primary_n_ctx <= 32_000);
        assert!(r.compactor.is_none());
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

    #[test]
    fn extra_notes_warn_about_extrapolation() {
        let n = PROVIDER.extra_notes();
        assert!(!n.is_empty());
        assert!(
            n.iter().any(|s| s.contains("extrapolated")),
            "expected extrapolation warning: {n:?}"
        );
    }

    #[test]
    fn large_no_compactor_anywhere() {
        // Large bucket should never pair a compactor at 32 GB tier.
        for task in [TaskClass::Fast, TaskClass::Mid, TaskClass::Long] {
            let r = PROVIDER.suggest(SizeBucket::Large, Architecture::Moe, task, 100_000);
            assert!(r.compactor.is_none(), "large + {task:?} got compactor");
        }
    }

    #[test]
    fn xl_also_no_compactor() {
        for task in [TaskClass::Fast, TaskClass::Mid, TaskClass::Long] {
            let r = PROVIDER.suggest(SizeBucket::Xl, Architecture::Moe, task, 100_000);
            assert!(r.compactor.is_none(), "xl + {task:?} got compactor");
        }
    }

    #[test]
    fn ctx_capped_at_max_context_length() {
        // Model claims maxCtx=20K but rules suggest 64K → should cap at 20K.
        let r = PROVIDER.suggest(
            SizeBucket::Medium,
            Architecture::Moe,
            TaskClass::Mid,
            20_000,
        );
        assert_eq!(r.primary_n_ctx, 20_000);
    }
}
