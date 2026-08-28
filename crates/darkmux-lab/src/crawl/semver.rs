//! Minimal inline npm semver-range check for the `stale-consumer` edge rule
//! (#1959 packet 1). Supports exact versions, `^`, `~`, `>=`, `x`/`*`
//! wildcards, and `||` of any of those — the "minimal inline check" the
//! packet-1 brief scopes. Anything else (hyphen ranges, `>`/`<`/`<=`
//! comparators, dist-tags like `latest`, `workspace:*`, git/URL
//! dependencies) is deliberately UNSUPPORTED: [`range_admits`] returns
//! `None` rather than guessing, so the planner records `range_admits: null`
//! plus a note naming the range instead of a wrong answer.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SemVer(u64, u64, u64);

fn parse_semver(s: &str) -> Option<SemVer> {
    // Strip prerelease/build metadata (`-rc.1`, `+build5`) — packet-1 scope
    // treats a prerelease as its release-line core version for range checks.
    let core = s
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or(s)
        .trim();
    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts[0].is_empty() {
        return None;
    }
    // A MISSING component (partial version like "1.2") defaults to 0. A
    // PRESENT-but-unparseable component rejects the whole parse — the two
    // are not the same thing. Collapsing them (an earlier draft did, via
    // `.and_then(...).unwrap_or(0)`) silently mis-parsed a hyphen range
    // like "1.2.3 - 2.3.4" as the plain version "1.2.0" (patch "3 " failed
    // to parse and fell back to 0 instead of failing).
    let component = |i: usize| -> Option<u64> {
        match parts.get(i) {
            None => Some(0),
            Some(p) => p.parse::<u64>().ok(),
        }
    };
    let major = component(0)?;
    let minor = component(1)?;
    let patch = component(2)?;
    Some(SemVer(major, minor, patch))
}

enum RangeToken {
    Exact(SemVer),
    Caret(SemVer),
    Tilde(SemVer),
    Gte(SemVer),
    /// `major` fixed / `minor` fixed, from `1.x`, `1.2.x`, `*`.
    Wildcard(Option<u64>, Option<u64>),
}

fn parse_range_token(tok: &str) -> Option<RangeToken> {
    let tok = tok.trim();
    // Any of our supported forms (exact, ^, ~, >=, x/*) is a single
    // whitespace-free word. Internal whitespace means a hyphen range
    // ("1.2.3 - 2.3.4") or a multi-comparator range (">1.0.0 <2.0.0") —
    // both unsupported. Without this guard, `parse_semver`'s prerelease
    // stripping (splitting on '-') would silently truncate "1.2.3 - 2.3.4"
    // down to a clean-looking "1.2.3" and mis-parse the whole hyphen range
    // as if it were the exact version "1.2.3".
    if tok.is_empty() || tok.chars().any(char::is_whitespace) {
        return None;
    }
    if tok == "*" || tok.eq_ignore_ascii_case("x") {
        return Some(RangeToken::Wildcard(None, None));
    }
    if let Some(rest) = tok.strip_prefix('^') {
        return parse_semver(rest).map(RangeToken::Caret);
    }
    if let Some(rest) = tok.strip_prefix('~') {
        return parse_semver(rest).map(RangeToken::Tilde);
    }
    if let Some(rest) = tok.strip_prefix(">=") {
        return parse_semver(rest).map(RangeToken::Gte);
    }
    if tok.contains('x') || tok.contains('X') {
        let parts: Vec<&str> = tok.split('.').collect();
        // Every component must be a plain number or an x/* wildcard — a
        // stray non-numeric, non-wildcard component (e.g. a build tag)
        // means this isn't really a wildcard range, so bail to unsupported.
        if !parts
            .iter()
            .all(|p| p.parse::<u64>().is_ok() || p.eq_ignore_ascii_case("x") || *p == "*")
        {
            return None;
        }
        let major = parts.first().and_then(|p| p.parse::<u64>().ok());
        let minor = parts.get(1).and_then(|p| p.parse::<u64>().ok());
        return Some(RangeToken::Wildcard(major, minor));
    }
    parse_semver(tok).map(RangeToken::Exact)
}

fn token_admits(token: &RangeToken, v: SemVer) -> bool {
    match token {
        RangeToken::Exact(e) => *e == v,
        RangeToken::Gte(g) => v >= *g,
        RangeToken::Wildcard(maj, min) => match (maj, min) {
            (None, _) => true,
            (Some(ma), None) => v.0 == *ma,
            (Some(ma), Some(mi)) => v.0 == *ma && v.1 == *mi,
        },
        RangeToken::Caret(c) => {
            if c.0 > 0 {
                v.0 == c.0 && v >= *c
            } else if c.1 > 0 {
                v.0 == 0 && v.1 == c.1 && v >= *c
            } else {
                v.0 == 0 && v.1 == 0 && v.2 == c.2
            }
        }
        RangeToken::Tilde(t) => v.0 == t.0 && v.1 == t.1 && v >= *t,
    }
}

/// The prerelease tag (everything between the version core and any build
/// metadata `+...`) of a semver string, if it carries one — `"8.1.1-rc.1"`
/// -> `Some("rc.1")`, `"8.1.1"` -> `None`. Used by
/// `crate::crawl::plan::plan` (#1959 finding 11) to note in the edge ledger
/// that a prerelease's range check treats it as its release-line core
/// version (the same normalization `parse_semver` already applies), rather
/// than silently doing that with no record of it.
pub fn prerelease_tag(s: &str) -> Option<String> {
    let core = s.trim().trim_start_matches('v');
    // Strip build metadata (`+build5`) before hunting for the prerelease
    // separator — `-` inside build metadata isn't a prerelease marker.
    let without_build = core.split('+').next().unwrap_or(core);
    let mut parts = without_build.splitn(2, '-');
    let _version_core = parts.next();
    parts.next().filter(|tag| !tag.is_empty()).map(str::to_string)
}

/// Does `range` (an npm semver range string) admit `version`? `None` means
/// "unsupported syntax — don't know", never a silent `false`.
pub fn range_admits(range: &str, version: &str) -> Option<bool> {
    let v = parse_semver(version)?;
    let range = range.trim();
    if range.is_empty() {
        return None;
    }
    let mut any_unsupported = false;
    for part in range.split("||") {
        match parse_range_token(part) {
            Some(tok) => {
                if token_admits(&tok, v) {
                    return Some(true);
                }
            }
            None => any_unsupported = true,
        }
    }
    if any_unsupported {
        None
    } else {
        Some(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_version() {
        assert_eq!(range_admits("5.5.0", "5.5.0"), Some(true));
        assert_eq!(range_admits("5.5.0", "5.5.1"), Some(false));
    }

    #[test]
    fn caret_same_major_or_higher_within_major() {
        assert_eq!(range_admits("^5.5.0", "5.5.0"), Some(true));
        assert_eq!(range_admits("^5.5.0", "5.9.0"), Some(true));
        assert_eq!(range_admits("^5.5.0", "8.1.1"), Some(false));
        assert_eq!(range_admits("^5.5.0", "5.4.9"), Some(false));
    }

    #[test]
    fn caret_zero_major_is_minor_locked() {
        assert_eq!(range_admits("^0.2.3", "0.2.9"), Some(true));
        assert_eq!(range_admits("^0.2.3", "0.3.0"), Some(false));
    }

    #[test]
    fn caret_zero_major_zero_minor_is_patch_exact() {
        assert_eq!(range_admits("^0.0.3", "0.0.3"), Some(true));
        assert_eq!(range_admits("^0.0.3", "0.0.4"), Some(false));
    }

    #[test]
    fn tilde_locks_minor() {
        assert_eq!(range_admits("~1.2.3", "1.2.9"), Some(true));
        assert_eq!(range_admits("~1.2.3", "1.3.0"), Some(false));
    }

    #[test]
    fn gte_has_no_upper_bound() {
        assert_eq!(range_admits(">=1.2.3", "99.0.0"), Some(true));
        assert_eq!(range_admits(">=1.2.3", "1.2.2"), Some(false));
    }

    #[test]
    fn wildcards() {
        assert_eq!(range_admits("*", "1.2.3"), Some(true));
        assert_eq!(range_admits("1.x", "1.9.9"), Some(true));
        assert_eq!(range_admits("1.x", "2.0.0"), Some(false));
        assert_eq!(range_admits("1.2.x", "1.2.9"), Some(true));
        assert_eq!(range_admits("1.2.x", "1.3.0"), Some(false));
    }

    #[test]
    fn or_of_ranges() {
        assert_eq!(range_admits("^1.0.0 || ^2.0.0", "2.5.0"), Some(true));
        assert_eq!(range_admits("^1.0.0 || ^2.0.0", "3.0.0"), Some(false));
    }

    #[test]
    fn unsupported_syntax_is_none_not_false() {
        assert_eq!(range_admits("1.2.3 - 2.3.4", "2.0.0"), None);
        assert_eq!(range_admits(">1.0.0 <2.0.0", "1.5.0"), None);
        assert_eq!(range_admits("latest", "1.0.0"), None);
        assert_eq!(range_admits("workspace:*", "1.0.0"), None);
    }

    #[test]
    fn malformed_version_is_none() {
        assert_eq!(range_admits("^1.0.0", "not-a-version"), None);
    }

    #[test]
    fn prerelease_tag_extracted_ignoring_build_metadata() {
        assert_eq!(prerelease_tag("8.1.1-rc.1"), Some("rc.1".to_string()));
        assert_eq!(prerelease_tag("8.1.1-rc.1+build5"), Some("rc.1".to_string()));
        assert_eq!(prerelease_tag("v8.1.1-beta"), Some("beta".to_string()));
        assert_eq!(prerelease_tag("8.1.1"), None);
        assert_eq!(prerelease_tag("8.1.1+build5"), None);
    }
}
