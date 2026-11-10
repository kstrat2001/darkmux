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
//! and the swap in use. Three of them decide:
//!
//! - **The kernel's level** (normal, warning, critical). It is the
//!   kernel's own verdict, it is live, and it falls again when the pressure
//!   passes. It can lag the danger: XNU raises warning only once the memory
//!   not yet compressed falls below what the compressor itself holds, and
//!   critical later still (or with the compressor past 98% of a limit), and
//!   swap is not one of its inputs. The panic above may have read normal.
//! - **The compressor's segments filling**, at
//!   [`COMPRESSOR_SEGMENTS_FILLING_PCT`] of their limit or more, read as
//!   warning whatever the level says: the early trip for a level that has
//!   not caught up.
//! - **The compressor near its limit**, at [`COMPRESSOR_NEAR_LIMIT_PCT`]
//!   or more of either of its limits, read as critical whatever the level
//!   says.
//!
//! Swap is named in every refusal and decides nothing. On macOS swap grows
//! on demand, so the swap left in what is allocated is not headroom, and
//! swap in use outlives the pressure that caused it: a threshold on either
//! would refuse on a machine that has long recovered. The kernel's own
//! "swap space low" verdict, the one a panic report prints, has no reader,
//! and neither has its count of swap files (45 in the panic): no sysctl
//! gives it, and `vm.swapusage`'s total is the same figure in bytes, with
//! the same flaw. The compressor's segments, read live, are the early
//! signal instead.
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

/// The compressor's segments, in percent of the kernel's limit for them,
/// from which darkmux reads memory pressure as at least warning.
///
/// The kernel's level can still read normal when the compressor is nearly
/// out of segments (see the module doc): the panic's report printed "97% of
/// segments limit (OK)". A warning reading refuses a start or load under
/// the default gate, so this trips the gate well before
/// [`COMPRESSOR_NEAR_LIMIT_PCT`] would stop one. 70% is a chosen margin
/// below the 97% the panic showed, not a measured point: a machine that
/// far into its segments refuses the next start while the kernel may still
/// read normal, and a start that has begun is not stopped at it (only
/// critical stops one).
pub const COMPRESSOR_SEGMENTS_FILLING_PCT: u8 = 70;

/// The pressure a reading comes to: the worst of the kernel's level,
/// warning when the compressor's segments are filling, and critical when
/// the compressor is near its limit. `None` when the reading has none of
/// those figures.
pub fn level(reading: &MemoryPressure) -> Option<PressureLevel> {
    let from = |pct: Option<u8>, at: u8, then: PressureLevel| pct.map(|pct| if pct >= at { then } else { PressureLevel::Normal });
    let compressor = from(reading.compressor_pct, COMPRESSOR_NEAR_LIMIT_PCT, PressureLevel::Critical);
    let segments = from(reading.compressor_segments_pct, COMPRESSOR_SEGMENTS_FILLING_PCT, PressureLevel::Warning);
    [reading.level, compressor, segments].into_iter().flatten().max()
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
    if let Some(pct) = reading.compressor_segments_pct {
        parts.push(format!("its segments at {pct}% of their limit"));
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
        MemoryPressure { level, compressor_pct, compressor_segments_pct: compressor_pct, swap_used_bytes: Some(24 << 30), swap_free_bytes: Some(1 << 30) }
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

    /// The kernel's level can lag the danger: the panic's own report read
    /// the compressor's segments at 97% of their limit, where the level may
    /// still have been normal. Segments at 70% or more read as warning
    /// whatever the level says, so the default gate refuses there, and a
    /// start under way is not stopped for it.
    #[test]
    fn compressor_segments_filling_read_as_warning_whatever_the_level_says() {
        let segments = |pct: u8| MemoryPressure {
            level: Some(PressureLevel::Normal),
            compressor_segments_pct: Some(pct),
            compressor_pct: Some(pct),
            ..Default::default()
        };
        assert_eq!(level(&segments(75)), Some(PressureLevel::Warning));
        assert_eq!(level(&segments(COMPRESSOR_SEGMENTS_FILLING_PCT)), Some(PressureLevel::Warning));
        assert_eq!(level(&segments(COMPRESSOR_SEGMENTS_FILLING_PCT - 1)), Some(PressureLevel::Normal));
        assert_eq!(level(&segments(COMPRESSOR_NEAR_LIMIT_PCT)), Some(PressureLevel::Critical), "90% is still critical");
        let only_segments = MemoryPressure { compressor_segments_pct: Some(75), ..Default::default() };
        assert_eq!(level(&only_segments), Some(PressureLevel::Warning), "with no level read at all");
        let said = refuses_start(PressureGate::Warning, Some(&segments(75))).expect("the default gate refuses");
        assert!(said.contains("segments at 75% of their limit"), "the figure that decided is named: {said}");
        assert!(refuses_start(PressureGate::Critical, Some(&segments(75))).is_none());
        assert!(stops_start(PressureGate::Warning, Some(&segments(75))).is_none(), "warning never stops a start");
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
