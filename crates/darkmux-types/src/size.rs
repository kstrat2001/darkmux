//! Human-readable size-string parsing.
//!
//! Extracted from `darkmux-eureka` (its only original consumer) to
//! `darkmux-types` (a dependency leaf) so `darkmux-doctor` and
//! `darkmux-serve` — both of which need this to sum LMStudio's reported
//! model sizes for a RAM-headroom estimate — don't each need a live
//! dependency on the eureka rules-engine crate just for one parsing
//! helper unrelated to rule evaluation.

/// Parse an LMStudio-reported size string (`"18.45 GB"`, `"2.15 GB"`,
/// `"18.45 GiB"`, or `"18.45GB"` with no space) into GB as `f64`.
///
/// (#904) Tolerates the binary (`GiB`/`MiB`/`TiB`) suffixes and a missing
/// space — the old `split_once(' ')` + GB/MB/TB-only match dropped all of
/// those to `None`, which the caller then summed as 0, undercounting the
/// working set so the headroom warning under-fired. For a headroom
/// *estimate* the binary-vs-decimal gap is within the noise, so the
/// `i`-variants map to the same magnitude. Returns `None` on anything
/// genuinely unparseable (a localized comma, no unit) — callers should
/// surface that as a skip rather than silently treating it as 0.
pub fn parse_size_gb(s: &str) -> Option<f64> {
    let s = s.trim();
    // Split the leading number from the trailing unit, with or without a space.
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (num_str, unit_raw) = s.split_at(split);
    let num: f64 = num_str.trim().parse().ok()?;
    match unit_raw.trim().to_ascii_uppercase().as_str() {
        "GB" | "GIB" => Some(num),
        "MB" | "MIB" => Some(num / 1024.0),
        "TB" | "TIB" => Some(num * 1024.0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_gb_tolerates_gib_binary_and_missing_space() {
        // (#904) The old parser dropped all of these to None → summed as 0.
        assert_eq!(parse_size_gb("18.45 GB"), Some(18.45));
        assert_eq!(parse_size_gb("18.45GB"), Some(18.45)); // no space
        assert_eq!(parse_size_gb("18.45 GiB"), Some(18.45)); // binary suffix
        assert_eq!(parse_size_gb("512 MB"), Some(0.5));
        assert_eq!(parse_size_gb("512MiB"), Some(0.5));
        assert_eq!(parse_size_gb("2 TB"), Some(2048.0));
        assert_eq!(parse_size_gb("2TiB"), Some(2048.0));
        // Genuinely unparseable → None (caller surfaces it as Skipped, not 0).
        assert_eq!(parse_size_gb("18,45 GB"), None); // localized comma
        assert_eq!(parse_size_gb("nonsense"), None);
        assert_eq!(parse_size_gb("18.45"), None); // no unit
    }
}
