//! (#2846) Per-detector policy.
//!
//! The runtime carries several detectors that watch a dispatch and act on
//! what they find. This module says how much authority each one has.
//!
//! The type is declared HERE rather than imported from `darkmux-types`
//! because this crate is deliberately independent of the parent workspace
//! (see `runtime/Cargo.toml`). The host resolves the policy from
//! `env > config.json > built-in` and forwards it as an environment
//! variable, the same shape `pace::max_pause_ms` uses.

/// What authority one detector has over the dispatch it watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetectionPolicy {
    /// Detect and act. The shipped behavior.
    #[default]
    Enforce,
    /// Detect and RECORD, but never act. The record carries what the
    /// detector would have done.
    ///
    /// This is the setting that makes a controlled comparison possible.
    /// Widening the per-call cap to stop a gate firing also shrinks the
    /// usable prompt budget, because the endpoint requires
    /// `prompt + max_tokens <= context_window` — so a run that fails under a
    /// widened cap cannot be attributed to the missing gate. Policy changes
    /// only whether the verdict is obeyed; cadence, per-call cap and prompt
    /// budget are untouched.
    Observe,
    /// Do not run the detector at all. Cheapest, and measures nothing.
    Off,
}

impl DetectionPolicy {
    /// Whether the detector should run its measurement.
    pub fn measures(self) -> bool {
        !matches!(self, DetectionPolicy::Off)
    }
    /// Whether a finding may change what the dispatch does.
    pub fn acts(self) -> bool {
        matches!(self, DetectionPolicy::Enforce)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            DetectionPolicy::Enforce => "enforce",
            DetectionPolicy::Observe => "observe",
            DetectionPolicy::Off => "off",
        }
    }
    /// Lenient parse. An unrecognized value resolves to `Enforce`, which is
    /// the ARMED direction on purpose: a typo must never silently disarm a
    /// guard. `darkmux doctor` surfaces the unparseable value host-side.
    pub fn parse_lenient(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "observe" => DetectionPolicy::Observe,
            "off" => DetectionPolicy::Off,
            _ => DetectionPolicy::Enforce,
        }
    }
}

/// `env(DARKMUX_RUNTIME_DETECTION_DEGENERACY_POLICY) > Enforce`.
///
/// Read per call rather than cached so a test's `set_var` takes effect,
/// matching how `DARKMUX_TURN_DELAY_MS` and the other host-forwarded knobs
/// behave in this crate.
pub fn degeneracy_policy() -> DetectionPolicy {
    std::env::var("DARKMUX_RUNTIME_DETECTION_DEGENERACY_POLICY")
        .ok()
        .map(|s| DetectionPolicy::parse_lenient(&s))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_split_measuring_from_acting() {
        assert!(DetectionPolicy::Enforce.measures() && DetectionPolicy::Enforce.acts());
        // The whole point of `observe`: it still measures, it never acts.
        assert!(DetectionPolicy::Observe.measures() && !DetectionPolicy::Observe.acts());
        assert!(!DetectionPolicy::Off.measures() && !DetectionPolicy::Off.acts());
    }

    #[test]
    fn an_unrecognized_value_stays_armed() {
        // The lenient direction is deliberate. A typo that disarmed a guard
        // would be silent, and silence is how a guard stops being one.
        assert_eq!(DetectionPolicy::parse_lenient("obsrve"), DetectionPolicy::Enforce);
        assert_eq!(DetectionPolicy::parse_lenient(""), DetectionPolicy::Enforce);
        assert_eq!(DetectionPolicy::parse_lenient("  OBSERVE "), DetectionPolicy::Observe);
    }
}
