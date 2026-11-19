//! Unicode property tables for the strict terminal escape
//! (`user_files::escape_text_strict`), embedded as inclusive code point
//! ranges so no Unicode crate is needed.
//!
//! Both are Unicode 16.0.0, generated once:
//!
//! - [`DEFAULT_IGNORABLE`] is the Default_Ignorable_Code_Point property, read
//!   from regex-syntax 0.8.11's `unicode_tables/property_bool.rs` (its
//!   `ucd-generate property-bool ucd-16.0.0` output of DerivedCoreProperties).
//! - [`STRICT_RIGHT_TO_LEFT`] is every code point Python 3.14's `unicodedata`
//!   (unidata 16.0.0) gives bidi class R, AL or AN. Ranges are merged across
//!   gaps of at most 64 code points that are all unassigned (Cn), so an
//!   unassigned slot inside a right-to-left block, which the bidi algorithm
//!   gives a right-to-left default class, is in the table too.
//!
//! To regenerate for a new Unicode version, rerun the same two reads and
//! update the version above; `user_files_tests.rs` walks every code point
//! and pins the Default_Ignorable count.

/// Default_Ignorable_Code_Point, Unicode 16.0.0 (4174 code points).
pub(crate) const DEFAULT_IGNORABLE: &[(u32, u32)] = &[
    (0xAD, 0xAD), (0x34F, 0x34F), (0x61C, 0x61C), (0x115F, 0x1160), (0x17B4, 0x17B5),
    (0x180B, 0x180F), (0x200B, 0x200F), (0x202A, 0x202E), (0x2060, 0x206F), (0x3164, 0x3164),
    (0xFE00, 0xFE0F), (0xFEFF, 0xFEFF), (0xFFA0, 0xFFA0), (0xFFF0, 0xFFF8), (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A), (0xE0000, 0xE0FFF),
];

/// Bidi classes R, AL and AN, Unicode 16.0.0, with the all-unassigned gaps
/// between them filled (see the module doc).
pub(crate) const STRICT_RIGHT_TO_LEFT: &[(u32, u32)] = &[
    (0x5BE, 0x5BE), (0x5C0, 0x5C0), (0x5C3, 0x5C3), (0x5C6, 0x5C6), (0x5D0, 0x605), (0x608, 0x608),
    (0x60B, 0x60B), (0x60D, 0x60D), (0x61B, 0x64A), (0x660, 0x669), (0x66B, 0x66F), (0x671, 0x6D5),
    (0x6DD, 0x6DD), (0x6E5, 0x6E6), (0x6EE, 0x6EF), (0x6FA, 0x710), (0x712, 0x72F), (0x74D, 0x7A5),
    (0x7B1, 0x7EA), (0x7F4, 0x7F5), (0x7FA, 0x7FA), (0x7FE, 0x815), (0x81A, 0x81A), (0x824, 0x824),
    (0x828, 0x828), (0x830, 0x858), (0x85E, 0x891), (0x8A0, 0x8C9), (0x8E2, 0x8E2),
    (0x200F, 0x200F), (0xFB1D, 0xFB1D), (0xFB1F, 0xFB28), (0xFB2A, 0xFD3D), (0xFD50, 0xFDC7),
    (0xFDF0, 0xFDFC), (0xFE70, 0xFEFC), (0x10800, 0x1091B), (0x10920, 0x10A00), (0x10A10, 0x10A35),
    (0x10A40, 0x10AE4), (0x10AEB, 0x10B35), (0x10B40, 0x10BAF), (0x10C00, 0x10D23),
    (0x10D30, 0x10D65), (0x10D6F, 0x10D8F), (0x10E60, 0x10EA9), (0x10EAD, 0x10EC4),
    (0x10F00, 0x10F45), (0x10F51, 0x10F81), (0x10F86, 0x10FF6), (0x1E800, 0x1E8CF),
    (0x1E900, 0x1E943), (0x1E94B, 0x1E95F), (0x1EC71, 0x1ECB4), (0x1ED01, 0x1ED3D),
    (0x1EE00, 0x1EEBB),
];

/// Whether `c` falls in one of `table`'s sorted, disjoint ranges.
pub(crate) fn in_ranges(c: char, table: &[(u32, u32)]) -> bool {
    let cp = c as u32;
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}
