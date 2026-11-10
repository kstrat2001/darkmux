//! Memory pressure as a memory decision reads it: refuse a start while the
//! machine is already in trouble, and stop darkmux's own start if it gets
//! there while the start commits its memory.
//!
//! The arithmetic of a memory decision ([`crate::residency_lock`]: a
//! model's price against reclaimable memory) is only as good as the price.
//! A price that was too low panicked a 128 GB machine: a server priced at
//! 18.2 GB committed 58 GiB onto a machine already swapping, and the
//! kernel's watchdog fired with the compressor at 97% of its segments
//! limit, 45 swap files and swap space low. So the reading itself is a
//! gate, whatever the arithmetic says: a wrong price then ends as a refused
//! dispatch, not a panic.
//!
//! # What is read
//!
//! [`darkmux_platform::Platform::memory_pressure`]: the kernel's own
//! pressure level, how full the compressor is against the kernel's limits,
//! and the swap in use. Two of them decide:
//!
//! - **The kernel's level** (normal, warning, critical). It is the
//!   kernel's own verdict, it is live, and it falls again when the pressure
//!   passes.
//! - **The compressor against its limit**, at [`COMPRESSOR_NEAR_LIMIT_PCT`]
//!   or more, read as critical whatever the level says.
//!
//! Swap is named in every refusal and decides nothing. On macOS swap grows
//! on demand, so the swap left in what is allocated is not headroom, and
//! swap in use outlives the pressure that caused it: a threshold on either
//! would refuse on a machine that has long recovered. The kernel's own
//! "swap space low" verdict, the one a panic report prints, has no reader.
//!
//! A platform that reads no level and no compressor figure (Linux today)
//! has no gate: an unknown pressure is never a refusal, since it would
//! refuse every start there. The arithmetic still decides.

use crate::config::PressureGate;
use darkmux_platform::{MemoryPressure, PressureLevel};

/// The compressor's fullness, in percent of the kernel's limit for it, from
/// which darkmux reads memory pressure as critical.
///
/// The kernel itself calls the compressor out of space at 98% of its limit
/// (XNU `vm_compressor.c`: `c_segments_nearing_limit` and
/// `c_segment_pages_compressed_nearing_limit` are both 98% of their
/// limits), and the machine this gate was written for panicked at 97% of
/// its segments limit. darkmux has to act before either: stopping a server
/// takes a poll of the reading, a signal and the server's exit, while the
/// start that panicked it committed about 10 GB a second (58 GiB resident
/// within its 5 to 7 s start). 90% leaves that stop a margin below both
/// marks. It is a chosen margin, not a measured point: the kernel's level
/// is the first signal, and this is the backstop for a level that lags.
pub const COMPRESSOR_NEAR_LIMIT_PCT: u8 = 90;

/// The pressure a reading comes to: the kernel's level, or critical when
/// the compressor is near its limit. `None` when the reading has neither
/// figure.
pub fn level(reading: &MemoryPressure) -> Option<PressureLevel> {
    let compressor = reading.compressor_pct.map(|pct| {
        if pct >= COMPRESSOR_NEAR_LIMIT_PCT {
            PressureLevel::Critical
        } else {
            PressureLevel::Normal
        }
    });
    reading.level.into_iter().chain(compressor).max()
}

/// The reading in a sentence's worth of words, every figure it has.
pub fn describe(reading: &MemoryPressure) -> String {
    let mut parts = Vec::new();
    if let Some(l) = reading.level {
        parts.push(format!("the kernel reports `{}` memory pressure", word(l)));
    }
    if let Some(pct) = reading.compressor_pct {
        parts.push(format!("the compressor is at {pct}% of its limit"));
    }
    if let Some(used) = reading.swap_used_bytes {
        parts.push(format!("{:.1} GiB of swap is in use", used as f64 / f64::from(1u32 << 30)));
    }
    if parts.is_empty() {
        return "no memory pressure figure could be read".to_string();
    }
    parts.join(", ")
}

fn word(level: PressureLevel) -> &'static str {
    match level {
        PressureLevel::Normal => "normal",
        PressureLevel::Warning => "warning",
        PressureLevel::Critical => "critical",
    }
}

/// Why a start or load is refused under `gate` at this reading, naming the
/// reading; `None` when it may go ahead (the gate is off, the pressure is
/// below the gate, or nothing could be read).
pub fn refuses_start(gate: PressureGate, reading: Option<&MemoryPressure>) -> Option<String> {
    let at = match gate {
        PressureGate::Warning => PressureLevel::Warning,
        PressureGate::Critical => PressureLevel::Critical,
        PressureGate::Off => return None,
    };
    let reading = reading?;
    let now = level(reading)?;
    (now >= at).then(|| {
        format!(
            "this machine's memory is already under pressure ({}), so darkmux starts and loads nothing until it \
             eases; runtime.memory.pressure_gate is `{}`",
            describe(reading),
            gate.as_str()
        )
    })
}

/// Why a start of darkmux's own that is under way is stopped at this
/// reading: the pressure turned critical. `None` otherwise, and always
/// under `off`.
pub fn stops_start(gate: PressureGate, reading: Option<&MemoryPressure>) -> Option<String> {
    if gate == PressureGate::Off {
        return None;
    }
    let reading = reading?;
    (level(reading)? >= PressureLevel::Critical).then(|| describe(reading))
}

/// This machine's gate: `runtime.memory.pressure_gate`. A value the setting
/// does not know never reaches a dispatch (every dispatching entry point
/// refuses it at its preflight, contract 9); a config edited to one mid-run
/// gates as the default does.
pub fn configured_gate() -> PressureGate {
    crate::config_access::memory_pressure_gate().unwrap_or(PressureGate::Warning)
}

/// This machine's reading now.
pub fn read() -> Option<MemoryPressure> {
    darkmux_platform::current().memory_pressure()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(level: Option<PressureLevel>, compressor_pct: Option<u8>) -> MemoryPressure {
        MemoryPressure { level, compressor_pct, swap_used_bytes: Some(24 << 30), swap_free_bytes: Some(1 << 30) }
    }

    /// The panic's own reading: the compressor at 97% of its limit is
    /// critical even where the kernel's level has not caught up.
    #[test]
    fn a_compressor_near_its_limit_is_critical_whatever_the_level_says() {
        assert_eq!(level(&at(Some(PressureLevel::Normal), Some(97))), Some(PressureLevel::Critical));
        assert_eq!(level(&at(None, Some(COMPRESSOR_NEAR_LIMIT_PCT))), Some(PressureLevel::Critical));
        assert_eq!(level(&at(Some(PressureLevel::Warning), Some(COMPRESSOR_NEAR_LIMIT_PCT - 1))), Some(PressureLevel::Warning));
        assert_eq!(level(&at(None, Some(10))), Some(PressureLevel::Normal));
        assert_eq!(level(&at(None, None)), None, "swap alone decides nothing");
    }

    #[test]
    fn each_gate_refuses_from_its_own_level_and_names_the_reading() {
        let warning = at(Some(PressureLevel::Warning), Some(40));
        let critical = at(Some(PressureLevel::Critical), Some(97));
        let normal = at(Some(PressureLevel::Normal), Some(5));
        let said = refuses_start(PressureGate::Warning, Some(&warning)).expect("warning refuses at warning");
        for part in ["`warning` memory pressure", "40% of its limit", "24.0 GiB of swap", "runtime.memory.pressure_gate is `warning`"] {
            assert!(said.contains(part), "{part}: {said}");
        }
        assert!(refuses_start(PressureGate::Warning, Some(&critical)).is_some());
        assert!(refuses_start(PressureGate::Warning, Some(&normal)).is_none());
        assert!(refuses_start(PressureGate::Critical, Some(&warning)).is_none(), "the critical gate lets warning through");
        assert!(refuses_start(PressureGate::Critical, Some(&critical)).is_some());
        assert!(refuses_start(PressureGate::Off, Some(&critical)).is_none());
    }

    /// Pressure nothing could read is not a refusal: it would refuse every
    /// start on a platform with no reader.
    #[test]
    fn an_unread_pressure_never_refuses_or_stops() {
        assert!(refuses_start(PressureGate::Warning, None).is_none());
        assert!(refuses_start(PressureGate::Warning, Some(&at(None, None))).is_none());
        assert!(stops_start(PressureGate::Warning, None).is_none());
    }

    #[test]
    fn a_start_is_stopped_only_at_critical_and_never_with_the_gate_off() {
        let critical = at(Some(PressureLevel::Critical), None);
        let warning = at(Some(PressureLevel::Warning), Some(50));
        assert!(stops_start(PressureGate::Warning, Some(&critical)).unwrap().contains("`critical`"));
        assert!(stops_start(PressureGate::Critical, Some(&critical)).is_some());
        assert!(stops_start(PressureGate::Warning, Some(&warning)).is_none());
        assert!(stops_start(PressureGate::Warning, Some(&at(Some(PressureLevel::Normal), Some(93)))).is_some());
        assert!(stops_start(PressureGate::Off, Some(&critical)).is_none());
    }
}
