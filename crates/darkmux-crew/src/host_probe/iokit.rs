//! (#2108) The thin CoreFoundation + IOKit layer the other `host_probe`
//! modules read through.
//!
//! **Why this exists as its own module** (a small, named divergence from the
//! mod.rs/mach_cpu/ioreport/thermal layout): three of the four probe sources
//! need the same handful of CF accessors and the same IORegistry walk —
//! `ioreport.rs` reads the SoC's DVFS frequency tables out of the `pmgr`
//! node, the GPU utilization read walks `IOAccelerator`, and both need
//! type-checked CFNumber/CFData/CFString extraction. Duplicating that into
//! two modules is how the two copies drift.
//!
//! CoreFoundation and IOKit are PUBLIC system frameworks present on every
//! macOS install, so they are linked normally (cfg-gated to macOS/aarch64,
//! so no other target sees the link). Only the PRIVATE IOReport framework is
//! dlopen'd — see `ioreport.rs`.
//!
//! Every accessor here is type-checked before it dereferences: a property
//! that is the wrong CF type yields `None` rather than a reinterpreted
//! pointer. IOKit's schema is not a contract we control, so "this key exists
//! and is a CFData" is a runtime question on every macOS version.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use std::ffi::{c_char, c_void, CString};

pub type CFTypeRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFStringRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFDataGetTypeID() -> usize;
    fn CFDictionaryGetValue(d: CFDictionaryRef, key: *const c_void) -> *const c_void;
    fn CFDictionaryGetCount(d: CFDictionaryRef) -> isize;
    fn CFDictionaryCreateMutableCopy(
        alloc: *const c_void,
        capacity: isize,
        d: CFDictionaryRef,
    ) -> *mut c_void;
    fn CFDictionarySetValue(d: *mut c_void, key: *const c_void, value: *const c_void);
    fn CFDictionaryGetKeysAndValues(d: CFDictionaryRef, keys: *mut *const c_void, values: *mut *const c_void);
    fn CFArrayGetCount(a: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(a: CFArrayRef, idx: isize) -> *const c_void;
    fn CFArrayCreateMutable(alloc: *const c_void, capacity: isize, cb: *const c_void) -> *mut c_void;
    fn CFArrayAppendValue(a: *mut c_void, v: *const c_void);
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, enc: u32) -> CFStringRef;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, len: isize, enc: u32) -> bool;
    fn CFStringGetLength(s: CFStringRef) -> isize;
    fn CFNumberGetValue(n: *const c_void, the_type: i32, out: *mut c_void) -> bool;
    fn CFDataGetLength(d: *const c_void) -> isize;
    fn CFDataGetBytePtr(d: *const c_void) -> *const u8;

    /// The retain/release callback set every CF-object-holding array needs.
    /// **Load-bearing, not boilerplate**: an array created with NULL
    /// callbacks stores raw pointers WITHOUT retaining them, so the channel
    /// dictionaries in `ioreport.rs`'s filtered set die with the source
    /// dictionary and `IOReportCreateSubscription` then reads freed memory
    /// (SIGTRAP inside `CFGetTypeID`, observed 2026-08-29).
    static kCFTypeArrayCallBacks: c_void;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingServices(port: u32, matching: *const c_void, it: *mut u32) -> i32;
    fn IOIteratorNext(it: u32) -> u32;
    fn IORegistryEntryCreateCFProperties(
        entry: u32,
        props: *mut *mut c_void,
        allocator: *const c_void,
        options: u32,
    ) -> i32;
    fn IOObjectRelease(obj: u32) -> u32;
}

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// `kCFNumberSInt64Type` — CFNumber coerces to the requested width, so this
/// is safe for the SInt32-backed IOKit counters too.
const K_CF_NUMBER_SINT64: i32 = 4;

/// An owned CFString, released on drop. Every key lookup below needs one and
/// forgetting the release is a per-sample leak.
pub struct CfString(CFStringRef);

impl CfString {
    pub fn new(s: &str) -> Option<Self> {
        let c = CString::new(s).ok()?;
        // SAFETY: `c` is a NUL-terminated UTF-8 buffer alive for the call.
        let r = unsafe {
            CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8)
        };
        (!r.is_null()).then_some(Self(r))
    }
    pub fn as_ptr(&self) -> CFStringRef {
        self.0
    }
}

impl Drop for CfString {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a Create function, so we own one reference.
        unsafe { CFRelease(self.0) }
    }
}

/// Read a borrowed CFString as a Rust `String`. Returns `None` for a null or
/// non-CFString pointer, so a schema change surfaces as an absent field.
///
/// # Safety
/// `s` must be null or a valid CF object pointer.
pub unsafe fn cfstring_to_string(s: CFStringRef) -> Option<String> {
    if s.is_null() || CFGetTypeID(s) != CFStringGetTypeID() {
        return None;
    }
    // 4 bytes/char is the UTF-8 worst case, +1 for the NUL.
    let cap = (CFStringGetLength(s) * 4 + 1).max(2) as usize;
    let mut buf = vec![0i8; cap];
    if !CFStringGetCString(s, buf.as_mut_ptr(), cap as isize, K_CF_STRING_ENCODING_UTF8) {
        return None;
    }
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as u8)
        .collect();
    String::from_utf8(bytes).ok()
}

/// Borrowed value for `key`, or `None` when the key is absent.
///
/// # Safety
/// `d` must be null or a valid CFDictionary.
pub unsafe fn dict_get(d: CFDictionaryRef, key: &str) -> Option<*const c_void> {
    if d.is_null() || CFGetTypeID(d) != CFDictionaryGetTypeID() {
        return None;
    }
    let k = CfString::new(key)?;
    let v = CFDictionaryGetValue(d, k.as_ptr());
    (!v.is_null()).then_some(v)
}

/// `key` as an i64, when it is present AND actually a CFNumber.
///
/// # Safety
/// `d` must be null or a valid CFDictionary.
pub unsafe fn dict_i64(d: CFDictionaryRef, key: &str) -> Option<i64> {
    let v = dict_get(d, key)?;
    if CFGetTypeID(v) != CFNumberGetTypeID() {
        return None;
    }
    let mut out: i64 = 0;
    CFNumberGetValue(v, K_CF_NUMBER_SINT64, &mut out as *mut i64 as *mut c_void)
        .then_some(out)
}

/// `key` as a nested CFDictionary (borrowed).
///
/// # Safety
/// `d` must be null or a valid CFDictionary.
pub unsafe fn dict_dict(d: CFDictionaryRef, key: &str) -> Option<CFDictionaryRef> {
    let v = dict_get(d, key)?;
    (CFGetTypeID(v) == CFDictionaryGetTypeID()).then_some(v)
}

/// `key`'s CFData bytes, copied out.
///
/// # Safety
/// `d` must be null or a valid CFDictionary.
pub unsafe fn dict_bytes(d: CFDictionaryRef, key: &str) -> Option<Vec<u8>> {
    let v = dict_get(d, key)?;
    if CFGetTypeID(v) != CFDataGetTypeID() {
        return None;
    }
    let len = CFDataGetLength(v);
    let ptr = CFDataGetBytePtr(v);
    if len <= 0 || ptr.is_null() {
        return None;
    }
    Some(std::slice::from_raw_parts(ptr, len as usize).to_vec())
}

/// Every `(key, value)` pair of a CFDictionary whose keys are CFStrings.
/// Values stay borrowed pointers into `d`.
///
/// # Safety
/// `d` must be null or a valid CFDictionary, and must outlive the returned
/// borrowed value pointers.
pub unsafe fn dict_pairs(d: CFDictionaryRef) -> Vec<(String, *const c_void)> {
    if d.is_null() || CFGetTypeID(d) != CFDictionaryGetTypeID() {
        return Vec::new();
    }
    let n = CFDictionaryGetCount(d);
    if n <= 0 {
        return Vec::new();
    }
    let mut keys = vec![std::ptr::null(); n as usize];
    let mut vals = vec![std::ptr::null(); n as usize];
    CFDictionaryGetKeysAndValues(d, keys.as_mut_ptr(), vals.as_mut_ptr());
    keys.into_iter()
        .zip(vals)
        .filter_map(|(k, v)| Some((cfstring_to_string(k)?, v)))
        .collect()
}

/// A borrowed value known to be CFData, copied out.
///
/// # Safety
/// `v` must be null or a valid CF object pointer.
pub unsafe fn value_bytes(v: *const c_void) -> Option<Vec<u8>> {
    if v.is_null() || CFGetTypeID(v) != CFDataGetTypeID() {
        return None;
    }
    let len = CFDataGetLength(v);
    let ptr = CFDataGetBytePtr(v);
    if len <= 0 || ptr.is_null() {
        return None;
    }
    Some(std::slice::from_raw_parts(ptr, len as usize).to_vec())
}

/// Walk every IOService matching `class_name`, handing each entry's property
/// dictionary to `f`. The dictionary is released after `f` returns, so `f`
/// must copy anything it keeps.
///
/// Measured on this machine: `IOAccelerator` ≈ 1.0 ms, `AppleARMIODevice`
/// ≈ 7 ms. The `ioreg -r -d 1 -c IOAccelerator` shell-out this replaces cost
/// ≈ 21 ms.
pub fn for_each_service<F: FnMut(CFDictionaryRef)>(class_name: &str, mut f: F) {
    let Ok(cname) = CString::new(class_name) else {
        return;
    };
    // SAFETY: `IOServiceMatching` takes a NUL-terminated class name and returns
    // a matching dict whose reference `IOServiceGetMatchingServices` consumes.
    unsafe {
        let matching = IOServiceMatching(cname.as_ptr());
        if matching.is_null() {
            return;
        }
        let mut it: u32 = 0;
        if IOServiceGetMatchingServices(0, matching, &mut it) != 0 {
            return;
        }
        loop {
            let entry = IOIteratorNext(it);
            if entry == 0 {
                break;
            }
            let mut props: *mut c_void = std::ptr::null_mut();
            if IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null(), 0) == 0
                && !props.is_null()
            {
                f(props as CFDictionaryRef);
                CFRelease(props as CFTypeRef);
            }
            IOObjectRelease(entry);
        }
        IOObjectRelease(it);
    }
}

// ── CF plumbing the IOReport subscription builder needs ────────────────────
// Re-exported rather than duplicated so `ioreport.rs` never declares its own
// CoreFoundation externs (two declarations of one ABI is one too many).

/// # Safety
/// `d` must be a valid CFDictionary; the caller owns the returned copy.
pub unsafe fn dict_mutable_copy(d: CFDictionaryRef) -> *mut c_void {
    CFDictionaryCreateMutableCopy(std::ptr::null(), CFDictionaryGetCount(d), d)
}
/// # Safety
/// `d` must be a valid CFMutableDictionary and `key`/`value` valid CF objects.
pub unsafe fn dict_set(d: *mut c_void, key: CFStringRef, value: *const c_void) {
    CFDictionarySetValue(d, key, value)
}
/// A mutable CFArray that RETAINS what is appended to it
/// (`kCFTypeArrayCallBacks`) — see that symbol's own doc for why the
/// alternative is a use-after-free rather than a leak.
///
/// # Safety
/// The caller owns the returned array.
pub unsafe fn array_mutable(capacity: isize) -> *mut c_void {
    CFArrayCreateMutable(
        std::ptr::null(),
        capacity,
        &kCFTypeArrayCallBacks as *const c_void,
    )
}
/// # Safety
/// `a` must be a valid CFMutableArray created by [`array_mutable`].
pub unsafe fn array_append(a: *mut c_void, v: *const c_void) {
    CFArrayAppendValue(a, v)
}
/// # Safety
/// `a` must be null or a valid CFArray.
pub unsafe fn array_count(a: CFArrayRef) -> isize {
    if a.is_null() || CFGetTypeID(a) != CFArrayGetTypeID() {
        return 0;
    }
    CFArrayGetCount(a)
}
/// # Safety
/// `a` must be a valid CFArray and `i` within its bounds.
pub unsafe fn array_at(a: CFArrayRef, i: isize) -> *const c_void {
    CFArrayGetValueAtIndex(a, i)
}
/// # Safety
/// `v` must be a CF object the caller owns a reference to.
pub unsafe fn release(v: CFTypeRef) {
    if !v.is_null() {
        CFRelease(v)
    }
}
