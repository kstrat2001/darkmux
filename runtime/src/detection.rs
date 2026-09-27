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
///
/// (#2947) The values name the action, and match the host's
/// `darkmux_types::config::DetectionPolicy` token for token: `off`,
/// `record`, `warn`, `cut`. `enforce`/`observe` are retired in 4.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectionPolicy {
    /// Do not run the detector at all. Cheapest, and measures nothing.
    Off,
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
    Record,
    /// Detect and never act, exactly like `Record` inside the container;
    /// the HOST surfaces each finding as a warning (a stderr line, a
    /// `dispatch.degeneracy.warning` flow record, the envelope's count),
    /// keyed on this token in the trajectory's `policy` field.
    Warn,
    /// Detect and cut. The shipped behavior.
    Cut,
}

impl DetectionPolicy {
    /// Whether the detector should run its measurement.
    pub fn measures(self) -> bool {
        !matches!(self, DetectionPolicy::Off)
    }
    /// Whether a finding may change what the dispatch does.
    pub fn acts(self) -> bool {
        matches!(self, DetectionPolicy::Cut)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            DetectionPolicy::Off => "off",
            DetectionPolicy::Record => "record",
            DetectionPolicy::Warn => "warn",
            DetectionPolicy::Cut => "cut",
        }
    }
    /// Exact parse of the token the host forwards. `None` for anything
    /// else, including the retired `enforce`/`observe`.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "off" => Some(DetectionPolicy::Off),
            "record" => Some(DetectionPolicy::Record),
            "warn" => Some(DetectionPolicy::Warn),
            "cut" => Some(DetectionPolicy::Cut),
            _ => None,
        }
    }
}

/// `env(DARKMUX_RUNTIME_DETECTION_DEGENERACY_POLICY)`, as the host forwards
/// it.
///
/// (#2947) There is no silent fallback here any more. The host ALWAYS
/// forwards the variable (`dispatch_internal::build_docker_run_argv`), and
/// always with a token it resolved through the registry, which refuses an
/// unknown or retired value at preflight before any container starts. The
/// runtime image is version-checked against the host (#2923), so the two
/// vocabularies cannot disagree in a real dispatch. An absent variable (the
/// runtime run by hand, outside darkmux) reads as the shipped `cut`, the
/// same value the host would have forwarded by default. An UNRECOGNIZED
/// token is a host/runtime mismatch that should be impossible; it reads as
/// `cut` (the armed direction) and says so on stderr every time, rather
/// than silently.
///
/// Read per call rather than cached so a test's `set_var` takes effect,
/// matching how `DARKMUX_TURN_DELAY_MS` and the other host-forwarded knobs
/// behave in this crate.
pub fn degeneracy_policy() -> DetectionPolicy {
    match std::env::var("DARKMUX_RUNTIME_DETECTION_DEGENERACY_POLICY") {
        Err(_) => DetectionPolicy::Cut,
        Ok(raw) => DetectionPolicy::parse(&raw).unwrap_or_else(|| {
            eprintln!(
                "darkmux-runtime: DARKMUX_RUNTIME_DETECTION_DEGENERACY_POLICY=`{raw}` is not a \
                 policy this runtime knows (off, record, warn, cut); running as `cut`. The host \
                 refuses such a value at preflight, so this runtime and its host are \
                 mismatched versions (#2947)."
            );
            DetectionPolicy::Cut
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_split_measuring_from_acting() {
        assert!(DetectionPolicy::Cut.measures() && DetectionPolicy::Cut.acts());
        // `record` and `warn` both measure and never act; the host tells
        // them apart (warn surfaces each finding).
        assert!(DetectionPolicy::Record.measures() && !DetectionPolicy::Record.acts());
        assert!(DetectionPolicy::Warn.measures() && !DetectionPolicy::Warn.acts());
        assert!(!DetectionPolicy::Off.measures() && !DetectionPolicy::Off.acts());
    }

    /// (#2947) The runtime's vocabulary is the host's: every token
    /// round-trips, and the retired spellings are not tokens.
    #[test]
    fn the_runtime_parses_exactly_the_host_vocabulary() {
        for p in [DetectionPolicy::Off, DetectionPolicy::Record, DetectionPolicy::Warn, DetectionPolicy::Cut] {
            assert_eq!(DetectionPolicy::parse(p.as_str()), Some(p));
        }
        assert_eq!(DetectionPolicy::parse("enforce"), None);
        assert_eq!(DetectionPolicy::parse("observe"), None);
    }
}
