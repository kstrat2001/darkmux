//! Generic fallback heuristics provider.
//!
//! Matches *anything* — this is the catch-all that fires when no
//! platform-specific provider claims the user's hardware. The rules here
//! are deliberately conservative because the empirical lab work was done
//! on Apple Silicon; on x86 + GPU, on AMD ROCm, on CPU-only systems, the
//! optimal n_ctx and compactor pairings are different and unmeasured.
//!
//! The provider attaches a prominent warning note so the user knows the
//! suggestion is unvalidated for their platform.

use darkmux_hardware::HardwareSpec;
use crate::{HeuristicsProvider, Rule, RulesTable};

pub struct Provider;
pub static PROVIDER: Provider = Provider;

const NOTE_UNVALIDATED: &str =
    "Provider `generic` — your hardware doesn't match a validated darkmux provider. \
     The suggestion below uses conservative defaults and is NOT empirically validated. \
     Please tune n_ctx down if you see swap/OOM, and consider PR'ing a hardware-specific \
     provider with measured results back to https://github.com/kstrat2001/darkmux.";

/// Rows are size buckets, columns Fast / Mid / Long. Conservative defaults
/// — single-turn-only for big models, no compactor pairing
/// (compactor-offload is an Apple-Silicon-shaped pattern and may not pay
/// off on other platforms).
static RULES: RulesTable = RulesTable([
    [Rule::solo(32_000), Rule::solo(32_000), Rule::solo(64_000)],
    [Rule::solo(32_000), Rule::solo(32_000), Rule::solo(64_000)],
    [Rule::solo(16_000), Rule::solo(32_000), Rule::solo(64_000)],
    [Rule::solo(8_000), Rule::solo(16_000), Rule::solo(32_000)],
    [Rule::solo(8_000), Rule::solo(16_000), Rule::solo(32_000)],
]);

impl HeuristicsProvider for Provider {
    fn id(&self) -> &'static str {
        "generic"
    }

    fn is_generic(&self) -> bool {
        true
    }

    fn matches(&self, _hw: &HardwareSpec) -> bool {
        true
    }

    fn rules(&self) -> &'static RulesTable {
        &RULES
    }

    fn extra_notes(&self) -> &[&'static str] {
        &[NOTE_UNVALIDATED]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SizeBucket, TaskClass};
    use darkmux_hardware::Platform;

    fn hw(plat: Platform, ram: u32) -> HardwareSpec {
        HardwareSpec {
            platform: plat,
            arch: "x86_64".into(),
            total_ram_gb: ram,
            physical_cores: 8,
            performance_cores: None,
            efficiency_cores: None,
            has_unified_memory: false,
        }
    }

    #[test]
    fn matches_anything() {
        assert!(PROVIDER.matches(&hw(Platform::Linux, 32)));
        assert!(PROVIDER.matches(&hw(Platform::Windows, 16)));
        assert!(PROVIDER.matches(&hw(Platform::AppleSilicon, 8)));
        assert!(PROVIDER.matches(&hw(Platform::Other, 0)));
    }

    #[test]
    fn never_pairs_a_compactor() {
        for bucket in [
            SizeBucket::Tiny,
            SizeBucket::Small,
            SizeBucket::Medium,
            SizeBucket::Large,
            SizeBucket::Xl,
        ] {
            for task in [TaskClass::Fast, TaskClass::Mid, TaskClass::Long] {
                let r = PROVIDER.suggest(bucket, task, 100_000);
                assert!(
                    r.compactor.is_none(),
                    "generic must not pair compactor; got one for {bucket:?}/{task:?}"
                );
            }
        }
    }

    #[test]
    fn extra_notes_announce_unvalidated() {
        let n = PROVIDER.extra_notes();
        assert!(n.iter().any(|s| s.contains("unvalidated") || s.contains("Provider `generic`")));
    }

    #[test]
    fn xl_long_is_minimal_ctx() {
        let r = PROVIDER.suggest(SizeBucket::Xl, TaskClass::Long, 100_000);
        // Generic shouldn't recommend a wide ctx on a 122B model unmeasured.
        assert!(r.primary_n_ctx <= 32_000);
    }
}
