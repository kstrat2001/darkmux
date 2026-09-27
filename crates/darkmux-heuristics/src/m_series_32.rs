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

use darkmux_hardware::{HardwareSpec, Platform, RamTier};
use crate::{HeuristicsProvider, Rule, RulesTable};

pub struct Provider;
pub static PROVIDER: Provider = Provider;

const NOTE_EXTRAPOLATED: &str =
    "Provider `m-series-32` rules are extrapolated from the m-series-64 tier (itself derived from \
     the validated 128GB tier) with further reductions for a ~32 GB unified memory budget. Compactor pairing is conservative (only \
     Medium Long); tune down `n_ctx` if you see swap pressure.";

/// Rows are size buckets, columns Fast / Mid / Long.
static RULES: RulesTable = RulesTable([
    // Tiny — no compactor needed; models are fast enough.
    [Rule::solo(32_000), Rule::solo(64_000), Rule::solo(131_072)],
    // Small — no compactor; small models are fast enough that
    // compaction overhead beats compaction savings.
    [Rule::solo(32_000), Rule::solo(64_000), Rule::solo(131_072)],
    // Medium — RAM gets tight. Conservative n_ctx; compactor only on Long
    // where context accumulation matters most. Mid doesn't pair a compactor:
    // at ~32 GB, KV pre-allocation for even a small model eats enough headroom
    // that the benefit is marginal; better to keep it for Long tasks.
    [Rule::solo(32_000), Rule::solo(64_000), Rule::paired(64_000, 32_000)],
    // Large (50–100B) — RAM critical at 32 GB. Best to drop compactor;
    // tight context windows to leave headroom for model weights + OS.
    [Rule::solo(16_000), Rule::solo(32_000), Rule::solo(64_000)],
    // XL (100B+) — unlikely to fit reliably on 32 GB. Minimal context.
    [Rule::solo(8_000), Rule::solo(16_000), Rule::solo(32_000)],
]);

impl HeuristicsProvider for Provider {
    fn id(&self) -> &'static str {
        "m-series-32"
    }

    fn is_generic(&self) -> bool {
        false
    }

    fn matches(&self, hw: &HardwareSpec) -> bool {
        matches!(hw.platform, Platform::AppleSilicon) && matches!(hw.ram_tier(), RamTier::Small)
    }

    fn rules(&self) -> &'static RulesTable {
        &RULES
    }

    fn extra_notes(&self) -> &[&'static str] {
        &[NOTE_EXTRAPOLATED]
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
        let r = PROVIDER.suggest(SizeBucket::Medium, TaskClass::Long, 262_144);
        assert_eq!(r.primary_n_ctx, 64_000);
        assert!(r.compactor.is_some());
        assert_eq!(r.compactor.as_ref().unwrap().n_ctx, 32_000);
    }

    #[test]
    fn medium_mid_no_compactor() {
        // Medium + Mid: no compactor at this tier (RAM headroom too tight)
        let r = PROVIDER.suggest(SizeBucket::Medium, TaskClass::Mid, 262_144);
        assert_eq!(r.primary_n_ctx, 64_000);
        assert!(r.compactor.is_none());
    }

    #[test]
    fn xl_long_keeps_ctx_minimal() {
        // Don't recommend wide context on a 120B+ model at 32 GB.
        let r = PROVIDER.suggest(SizeBucket::Xl, TaskClass::Long, 262_144);
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
            let r = PROVIDER.suggest(bucket, TaskClass::Fast, 100_000);
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
    fn extra_notes_do_not_call_the_64gb_tier_validated() {
        // The 64GB tier is itself extrapolated from the 128GB tier (its own
        // note says "not independently measured"), so this tier's note must
        // not present it as a validated baseline.
        for n in PROVIDER.extra_notes() {
            assert!(!n.contains("validated 64GB"), "note claims a validated 64GB tier: {n}");
            assert!(n.contains("128GB"), "note should name the validated 128GB root: {n}");
        }
    }

    #[test]
    fn large_no_compactor_anywhere() {
        // Large bucket should never pair a compactor at 32 GB tier.
        for task in [TaskClass::Fast, TaskClass::Mid, TaskClass::Long] {
            let r = PROVIDER.suggest(SizeBucket::Large, task, 100_000);
            assert!(r.compactor.is_none(), "large + {task:?} got compactor");
        }
    }

    #[test]
    fn xl_also_no_compactor() {
        for task in [TaskClass::Fast, TaskClass::Mid, TaskClass::Long] {
            let r = PROVIDER.suggest(SizeBucket::Xl, task, 100_000);
            assert!(r.compactor.is_none(), "xl + {task:?} got compactor");
        }
    }

    #[test]
    fn ctx_capped_at_max_context_length() {
        // Model claims maxCtx=20K but rules suggest 64K → should cap at 20K.
        let r = PROVIDER.suggest(SizeBucket::Medium, TaskClass::Mid, 20_000);
        assert_eq!(r.primary_n_ctx, 20_000);
    }
}
