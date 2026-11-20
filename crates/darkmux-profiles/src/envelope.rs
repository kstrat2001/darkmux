//! Shared declared-vs-loaded context comparison (#544).
//!
//! Whether a resident instance is one a dispatch would use is not decided
//! here: the dispatch residency preflight owns that rule
//! (`darkmux_crew::dispatch_internal::decide_preflight`, contract 4: only
//! darkmux's own instance at the profile's window or more), and the lab's
//! pre-run warning and `darkmux doctor`'s residency rows read it.

/// Declared vs loaded context windows differ enough to flag?
///
/// A 5% tolerance absorbs benign rounding (a profile declaring `262000`
/// against a `262144` power-of-two load) while still catching a real
/// envelope swap (e.g. `262000` declared vs `101000` loaded). A loaded
/// context of `0` means `lms ps` didn't report one — we can't tell, so we
/// don't flag.
pub fn ctx_diverges(declared: u64, loaded: u64) -> bool {
    if loaded == 0 {
        return false;
    }
    declared.abs_diff(loaded) * 20 > declared
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctx_tolerates_power_of_two_rounding() {
        assert!(!ctx_diverges(262000, 262144));
    }

    #[test]
    fn ctx_flags_real_envelope_swap() {
        assert!(ctx_diverges(262000, 101000));
    }

    #[test]
    fn ctx_unknown_load_does_not_flag() {
        assert!(!ctx_diverges(262000, 0));
    }
}
