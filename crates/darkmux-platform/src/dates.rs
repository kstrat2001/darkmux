//! An RFC 3339 UTC time (`2025-10-28T16:10:30Z`) as unix milliseconds,
//! without a date crate: the one shape the OS answers here use.

pub(crate) fn rfc3339_utc_ms(s: &str) -> Option<u64> {
    let [y, m, d, hh, mm, ss] = rfc3339_fields(s.trim())?;
    let secs = days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss;
    u64::try_from(secs).ok().map(|s| s * 1000)
}

/// The separators of `YYYY-MM-DDTHH:MM:SSZ`, by byte offset.
const SEPARATORS: [(usize, u8); 6] = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':'), (19, b'Z')];

/// Each field's bytes and its allowed range, in order: year, month, day,
/// hour, minute, second (60 for a leap second).
const FIELDS: [(std::ops::Range<usize>, std::ops::RangeInclusive<i64>); 6] =
    [(0..4, 0..=9999), (5..7, 1..=12), (8..10, 1..=31), (11..13, 0..=23), (14..16, 0..=59), (17..19, 0..=60)];

/// The six fields of an RFC 3339 UTC time, each in range; None for any other shape.
fn rfc3339_fields(s: &str) -> Option<[i64; 6]> {
    let b = s.as_bytes();
    if b.len() != 20 || !SEPARATORS.iter().all(|&(i, c)| b[i] == c) {
        return None;
    }
    let mut out = [0i64; 6];
    for (slot, (bytes, range)) in out.iter_mut().zip(FIELDS) {
        let n = s.get(bytes)?.parse::<i64>().ok()?;
        *slot = range.contains(&n).then_some(n)?;
    }
    Some(out)
}

/// Days since 1970-01-01: Howard Hinnant's days-from-civil.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
