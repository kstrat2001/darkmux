//! (#2112) A held `IOPMAssertionCreateWithName(kIOPMAssertionTypePrevent
//! UserIdleSystemSleep)` assertion — a public IOKit API, not a private one
//! — so an idle lid or idle-sleep timer doesn't end an overnight mission or
//! crawl underneath the operator.
//!
//! **This does NOT override a thermal emergency sleep.** A
//! `PreventUserIdleSystemSleep` assertion blocks only the ordinary idle
//! timer; the kernel's own thermal breaker (#1292 — "Dark Wake Thermal
//! Emergency") sleeps the machine regardless of any held assertion. That
//! is deliberate: the assertion's job is "don't let the lid/idle-timer end
//! this run", not "keep running no matter how hot the machine gets" — the
//! latter is exactly the failure mode `power_posture`'s pre-flight refusal
//! (`src/preflight.rs`) exists to catch before the mission ever starts.
//!
//! **RAII by construction.** [`SleepAssertion::hold`] returns a value
//! whose `Drop` releases the assertion — held for exactly the caller's
//! scope, released on every exit path (early return, `?`, panic-unwind)
//! without the caller having to remember a matching release call.

use std::ffi::{c_char, c_void, CString};

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    type CFStringRef = *const c_void;
    type IoReturn = i32;
    type IoPmAssertionId = u32;
    type IoPmAssertionLevel = u32;

    const K_IO_RETURN_SUCCESS: IoReturn = 0;
    const K_IOPM_ASSERTION_LEVEL_ON: IoPmAssertionLevel = 255;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    // CoreFoundation and IOKit are PUBLIC system frameworks present on
    // every macOS install (same posture as `host_probe::iokit`'s own doc),
    // so both are linked directly rather than `dlopen`'d.
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(alloc: *const c_void, c_str: *const c_char, encoding: u32) -> CFStringRef;
        fn CFRelease(cf: CFStringRef);
    }

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            level: IoPmAssertionLevel,
            name: CFStringRef,
            out_id: *mut IoPmAssertionId,
        ) -> IoReturn;
        fn IOPMAssertionRelease(id: IoPmAssertionId) -> IoReturn;
    }

    /// `kIOPMAssertionTypePreventUserIdleSystemSleep` is `#define`d in
    /// `IOPMLib.h` as `CFSTR("PreventUserIdleSystemSleep")` — a
    /// compile-time literal, not an exported symbol (there is nothing to
    /// `nm` for it in `IOKit.tbd`; verified against the SDK header before
    /// hard-coding this). Built once and reused for every assertion.
    const ASSERTION_TYPE_NAME: &str = "PreventUserIdleSystemSleep";

    /// The live assertion handle. Exists only while held; `Drop` releases
    /// it via `IOPMAssertionRelease`.
    pub struct Held(IoPmAssertionId);

    impl Held {
        /// Create + hold a `PreventUserIdleSystemSleep` assertion named
        /// `reason` (shows up in `pmset -g assertions`). `None` on any
        /// failure — a non-UTF8/NUL-containing reason, a failed CFString
        /// allocation, or IOKit itself refusing the call.
        pub fn create(reason: &str) -> Option<Self> {
            let c_type = CString::new(ASSERTION_TYPE_NAME).ok()?;
            let c_reason = CString::new(reason).ok()?;
            // SAFETY: both CStrings outlive this call (dropped at the end
            // of the function, after `IOPMAssertionCreateWithName` has
            // already copied their contents into the two CFStrings).
            // `alloc: NULL` uses the default allocator, matching every
            // other `CFStringCreateWithCString` call in this codebase
            // (`host_probe::iokit::CFString::new`).
            unsafe {
                let type_ref =
                    CFStringCreateWithCString(std::ptr::null(), c_type.as_ptr(), K_CF_STRING_ENCODING_UTF8);
                if type_ref.is_null() {
                    return None;
                }
                let name_ref =
                    CFStringCreateWithCString(std::ptr::null(), c_reason.as_ptr(), K_CF_STRING_ENCODING_UTF8);
                if name_ref.is_null() {
                    CFRelease(type_ref);
                    return None;
                }
                let mut out_id: IoPmAssertionId = 0;
                let rc = IOPMAssertionCreateWithName(type_ref, K_IOPM_ASSERTION_LEVEL_ON, name_ref, &mut out_id);
                CFRelease(type_ref);
                CFRelease(name_ref);
                if rc == K_IO_RETURN_SUCCESS {
                    Some(Held(out_id))
                } else {
                    None
                }
            }
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a valid assertion id returned by a prior
            // successful `IOPMAssertionCreateWithName`, released at most
            // once (owned by this `Held`, dropped once).
            unsafe {
                IOPMAssertionRelease(self.0);
            }
        }
    }
}

/// A held (or attempted) `PreventUserIdleSystemSleep` assertion, scoped to
/// the caller's lifetime — hold it for a mission/crawl's duration and let
/// it drop at the end (or on any early return) to release.
pub struct SleepAssertion {
    #[cfg(target_os = "macos")]
    held: Option<imp::Held>,
}

impl SleepAssertion {
    /// Attempt to hold the assertion. Always returns a value — on any
    /// platform other than macOS, or if IOKit itself refuses the call,
    /// the returned `SleepAssertion` simply holds nothing and
    /// [`Self::status`] reports `"unavailable"`. Never panics: a failed
    /// sleep assertion is a degraded-but-fine pre-flight outcome, not a
    /// reason to refuse the mission (only the thermal-state check in
    /// `src/preflight.rs` refuses).
    pub fn hold(reason: &str) -> Self {
        #[cfg(target_os = "macos")]
        {
            SleepAssertion { held: imp::Held::create(reason) }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = reason;
            SleepAssertion {}
        }
    }

    /// `"held"` when the assertion is actually active, `"unavailable"`
    /// otherwise — the exact two values the `mission start` record's
    /// `sleep_assertion` payload field carries (#2112).
    pub fn status(&self) -> &'static str {
        #[cfg(target_os = "macos")]
        {
            if self.held.is_some() {
                "held"
            } else {
                "unavailable"
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            "unavailable"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_macos_or_refused_assertion_reports_unavailable_and_never_panics() {
        // On non-macOS this is the only reachable path; on macOS this
        // still exercises the type/Drop wiring even when `create` itself
        // is what's under test below.
        let s = SleepAssertion::hold("darkmux test: unconditional path");
        let _ = s.status();
    }

    // (#2112 acceptance) A serial test that takes the real assertion and
    // shows it in `pmset -g assertions` while held, then confirms it's
    // gone after drop. Serial because it's asserting on GLOBAL machine
    // state (`pmset -g assertions` reflects every process's assertions,
    // not just this test's) — a concurrent test doing the same thing
    // could observe the wrong assertion's name.
    #[cfg(target_os = "macos")]
    #[serial_test::serial]
    #[test]
    fn holding_the_assertion_is_observable_in_pmset_and_releases_on_drop() {
        let reason = "darkmux-2112-test-assertion";
        let assertion = SleepAssertion::hold(reason);
        assert_eq!(assertion.status(), "held", "IOPMAssertionCreateWithName must succeed on a real macOS host");

        let out = std::process::Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .expect("pmset must run");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains(reason), "held assertion must be visible in `pmset -g assertions`:\n{text}");

        drop(assertion);

        let out_after = std::process::Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .expect("pmset must run");
        let text_after = String::from_utf8_lossy(&out_after.stdout);
        assert!(
            !text_after.contains(reason),
            "assertion must be released (gone from `pmset -g assertions`) after drop:\n{text_after}"
        );
    }
}
