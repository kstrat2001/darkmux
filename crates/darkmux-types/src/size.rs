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

/// (#3112) The form a sandbox memory limit (`sandbox.memory`) takes, as an
/// operator reads it: Docker's `--memory` syntax, the part both sandbox
/// backends accept.
pub const SANDBOX_MEMORY_FORM: &str =
    "a whole number followed by k, m or g (binary units, either case, an optional trailing b), at least 6m: `4g`, `16g`, `512m`";

/// Docker's smallest memory limit; anything below it (`0` included, which
/// Docker reads as no limit) is refused.
pub const SANDBOX_MEMORY_MIN_BYTES: u64 = 6 << 20;

/// A sandbox memory limit in bytes ([`SANDBOX_MEMORY_FORM`]), or why it is
/// not one. The one grammar `sandbox.memory`'s preflight, Docker's
/// `--memory` and Seatbelt's watch all read.
pub fn sandbox_memory_bytes(text: &str) -> Result<u64, String> {
    let bytes = size_bytes(text).ok_or_else(|| format!("`{text}` is not a size"))?;
    if bytes < SANDBOX_MEMORY_MIN_BYTES {
        return Err(format!("`{text}` is below the 6m every sandbox needs (Docker reads `0` as no limit at all)"));
    }
    Ok(bytes)
}

/// A whole number, then optionally `b`, `k`, `m` or `g` (binary units,
/// either case), then optionally `b`.
fn size_bytes(text: &str) -> Option<u64> {
    let lower = text.to_ascii_lowercase();
    let digits = lower.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let shift = match &lower[digits.len()..] {
        "" | "b" => 0,
        "k" | "kb" => 10,
        "m" | "mb" => 20,
        "g" | "gb" => 30,
        _ => return None,
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()?.checked_mul(1 << shift)
}

/// `bytes` for a person: `4 GiB`, `4.2 GiB`, `512 MiB`.
pub fn human_bytes(bytes: u64) -> String {
    let (value, unit) = if bytes >= 1 << 30 { (bytes as f64 / (1u64 << 30) as f64, "GiB") } else { (bytes as f64 / (1u64 << 20) as f64, "MiB") };
    let text = format!("{value:.1}");
    format!("{} {unit}", text.strip_suffix(".0").unwrap_or(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Docker's size syntax, at least 6m: `0`, a negative or a word is not
    /// a sandbox memory limit.
    #[test]
    fn sandbox_memory_reads_as_docker_reads_it() {
        for (text, bytes) in [("4g", 4u64 << 30), ("16G", 16 << 30), ("512m", 512 << 20), ("6m", 6 << 20), ("2gb", 2 << 30), ("8388608", 8 << 20), ("6144k", 6 << 20)] {
            assert_eq!(sandbox_memory_bytes(text), Ok(bytes), "{text}");
        }
        for bad in ["", "g", "lots", "0", "-1g", "1.5g", "4 g", "4x", "5m", "100b"] {
            assert!(sandbox_memory_bytes(bad).is_err(), "{bad}");
        }
        assert!(sandbox_memory_bytes("0").unwrap_err().contains("no limit"));
    }

    #[test]
    fn human_bytes_reads_for_a_person() {
        assert_eq!(human_bytes(4 << 30), "4 GiB");
        assert_eq!(human_bytes((4 << 30) + (1 << 28)), "4.2 GiB");
        assert_eq!(human_bytes(512 << 20), "512 MiB");
    }

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
