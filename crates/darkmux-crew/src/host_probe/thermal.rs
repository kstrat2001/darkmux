//! (#2108) Thermal pressure: the OS's own thermal state, and whether the
//! kernel is currently capping CPU speed.
//!
//! Both reads are in-process kernel/runtime lookups, not shell-outs.
//! `pmset -g therm` — the obvious source, and the one the contract names —
//! is a ~8 ms process spawn that prints a human sentence; it reads
//! `IOPMCopyCPUPowerStatus` underneath, so this module calls that directly
//! (~0.06 ms) and keeps the same semantics. `ProcessInfo.thermalState` has
//! no C entry point at all, so it is reached through `objc_msgSend` on
//! `NSProcessInfo` (~0.001 ms).
//!
//! Both degrade to `None` independently: no thermal source is worth a panic,
//! and a machine that reports neither still reports CPU, memory, GPU and
//! power.

/// `NSProcessInfoThermalState`'s four values, lowercased — the vocabulary the
/// viewer's `ThermalState` union expects verbatim.
pub const THERMAL_STATES: [&str; 4] = ["nominal", "fair", "serious", "critical"];

/// Map the raw `NSProcessInfoThermalState` integer to its name. An
/// out-of-range value (a state a future macOS adds) becomes
/// `"unknown-<n>"` rather than being clamped into a state it isn't — the
/// viewer's union has a `string` fallback precisely so an unrecognized state
/// renders as unrecognized instead of silently reading as "nominal". Pure.
pub fn thermal_state_name(raw: i64) -> String {
    THERMAL_STATES
        .get(raw as usize)
        .map(|s| (*s).to_string())
        .unwrap_or_else(|| format!("unknown-{raw}"))
}

/// One thermal reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThermalSample {
    pub state: String,
    /// The kernel's current CPU speed cap as a percent. **100 means "no cap
    /// recorded"**, which is what an idle, cool machine reports —
    /// `IOPMCopyCPUPowerStatus` returns `kIOReturnNotFound` until something
    /// has actually throttled, exactly as `pmset -g therm` prints "No CPU
    /// power status has been recorded".
    pub cpu_speed_limit_pct: u64,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod imp {
    use super::ThermalSample;
    use crate::host_probe::iokit;
    use std::ffi::{c_char, c_void, CString};
    use std::sync::OnceLock;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPMCopyCPUPowerStatus(status: *mut *const c_void) -> i32;
    }

    type FnGetClass = unsafe extern "C" fn(*const c_char) -> *const c_void;
    type FnSelReg = unsafe extern "C" fn(*const c_char) -> *const c_void;
    type FnMsgSendPtr = unsafe extern "C" fn(*const c_void, *const c_void) -> *const c_void;
    type FnMsgSendI64 = unsafe extern "C" fn(*const c_void, *const c_void) -> i64;

    struct Objc {
        process_info: *const c_void,
        sel_thermal_state: *const c_void,
        msg_send_i64: FnMsgSendI64,
    }

    // SAFETY: `process_info` is the `+[NSProcessInfo processInfo]` singleton
    // and `sel_thermal_state` an interned selector — both are immortal,
    // immutable process-wide values, and `-thermalState` is documented as
    // safe to read from any thread.
    unsafe impl Send for Objc {}
    unsafe impl Sync for Objc {}

    /// Resolve `NSProcessInfo`'s thermal-state accessor ONCE. `None` when the
    /// Objective-C runtime or the class/selector is unavailable, which is
    /// also the "log once, then stay quiet" signal the caller reports.
    fn objc() -> Option<&'static Objc> {
        static CELL: OnceLock<Option<Objc>> = OnceLock::new();
        CELL.get_or_init(|| {
            // SAFETY: every pointer is null-checked before the next use, and
            // each transmute is from a `dlsym` result to that symbol's real
            // C signature.
            unsafe {
                let path = CString::new("/usr/lib/libobjc.A.dylib").ok()?;
                let h = libc::dlopen(path.as_ptr(), libc::RTLD_LAZY);
                if h.is_null() {
                    return None;
                }
                let get_class: FnGetClass = {
                    let n = CString::new("objc_getClass").ok()?;
                    let p = libc::dlsym(h, n.as_ptr());
                    if p.is_null() {
                        return None;
                    }
                    std::mem::transmute::<*mut c_void, FnGetClass>(p)
                };
                let sel_reg: FnSelReg = {
                    let n = CString::new("sel_registerName").ok()?;
                    let p = libc::dlsym(h, n.as_ptr());
                    if p.is_null() {
                        return None;
                    }
                    std::mem::transmute::<*mut c_void, FnSelReg>(p)
                };
                let msg_send = {
                    let n = CString::new("objc_msgSend").ok()?;
                    let p = libc::dlsym(h, n.as_ptr());
                    if p.is_null() {
                        return None;
                    }
                    p
                };
                let msg_send_ptr = std::mem::transmute::<*mut c_void, FnMsgSendPtr>(msg_send);
                let msg_send_i64 = std::mem::transmute::<*mut c_void, FnMsgSendI64>(msg_send);

                let cls_name = CString::new("NSProcessInfo").ok()?;
                let cls = get_class(cls_name.as_ptr());
                if cls.is_null() {
                    return None;
                }
                let sel_pi = sel_reg(CString::new("processInfo").ok()?.as_ptr());
                let sel_ts = sel_reg(CString::new("thermalState").ok()?.as_ptr());
                if sel_pi.is_null() || sel_ts.is_null() {
                    return None;
                }
                let pi = msg_send_ptr(cls, sel_pi);
                if pi.is_null() {
                    return None;
                }
                Some(Objc {
                    process_info: pi,
                    sel_thermal_state: sel_ts,
                    msg_send_i64,
                })
            }
        })
        .as_ref()
    }

    /// `ProcessInfo.processInfo.thermalState`, as its lowercase name.
    pub fn thermal_state() -> Option<String> {
        let o = objc()?;
        // SAFETY: `-thermalState` returns an `NSProcessInfoThermalState`
        // (an `NSInteger`), which is exactly the i64 return we cast to.
        let raw = unsafe { (o.msg_send_i64)(o.process_info, o.sel_thermal_state) };
        Some(super::thermal_state_name(raw))
    }

    /// The kernel's current CPU speed cap, as a percent.
    ///
    /// `IOPMCopyCPUPowerStatus` returns `kIOReturnNotFound` when nothing has
    /// throttled since boot — the overwhelmingly common case on a healthy
    /// machine, and the one `pmset -g therm` renders as "No CPU power status
    /// has been recorded". That is reported as **100** (no cap), not as
    /// `None`: "the kernel is not capping us" is a measurement, not a
    /// missing source.
    pub fn cpu_speed_limit_pct() -> u64 {
        let mut dict: *const c_void = std::ptr::null();
        // SAFETY: `dict` is a valid out-param; on success we own the returned
        // dictionary and release it below.
        let rc = unsafe { IOPMCopyCPUPowerStatus(&mut dict) };
        if rc != 0 || dict.is_null() {
            return 100;
        }
        // SAFETY: `dict` is a live CFDictionary until the release below;
        // `dict_i64` type-checks the value before reading it.
        let v = unsafe {
            let v = iokit::dict_i64(dict, "CPU_Speed_Limit");
            iokit::release(dict);
            v
        };
        v.map(|n| n.clamp(0, 100) as u64).unwrap_or(100)
    }

    /// One thermal reading, or `None` when the OS thermal state is
    /// unreadable (in which case a bare speed-limit number would have no
    /// context to sit in).
    pub fn sample() -> Option<ThermalSample> {
        Some(ThermalSample {
            state: thermal_state()?,
            cpu_speed_limit_pct: cpu_speed_limit_pct(),
        })
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use imp::sample;

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn sample() -> Option<ThermalSample> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thermal_state_names_match_the_viewer_vocabulary() {
        assert_eq!(thermal_state_name(0), "nominal");
        assert_eq!(thermal_state_name(1), "fair");
        assert_eq!(thermal_state_name(2), "serious");
        assert_eq!(thermal_state_name(3), "critical");
    }

    #[test]
    fn an_unknown_thermal_state_is_named_not_clamped() {
        assert_eq!(
            thermal_state_name(7),
            "unknown-7",
            "a future macOS state must not silently read as nominal"
        );
        assert_eq!(thermal_state_name(-1), "unknown--1");
    }
}
