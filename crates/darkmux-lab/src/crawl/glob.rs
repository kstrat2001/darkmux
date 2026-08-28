//! Minimal inline glob matcher for `applies_to`/`exclude` patterns
//! (`**`, `*`, `?`) against `/`-separated relative paths.
//!
//! No `glob`/`globset` crate exists anywhere in this workspace (checked:
//! neither is in `Cargo.lock`, even transitively) — this ~40-line matcher is
//! the "small one-off need beats a crate" convention's call, not a
//! `regex`-style promotion (see `crate::crawl::plan`'s module docs for why
//! `regex` WAS promoted instead: the built-in rules' `prefilter` patterns use
//! real regex syntax this matcher cannot express).

/// True if `path` (a `/`-separated relative path, no leading `/`) matches
/// `pattern`. `**` matches zero or more whole path segments; `*` matches
/// zero or more characters within one segment; `?` matches exactly one
/// character within one segment.
pub fn matches(pattern: &str, path: &str) -> bool {
    // #1959 finding 7: a trailing-slash pattern (`node_modules/`) reads as
    // "this directory and everything under it" to a human author, but
    // segment-matching it literally requires an exact empty final segment,
    // which never matches a nested path. Normalize `<stem>/` to
    // `<stem>/**` before matching so the natural spelling works.
    let normalized;
    let pattern = match pattern.strip_suffix('/') {
        Some(stem) => {
            normalized = format!("{stem}/**");
            normalized.as_str()
        }
        None => pattern,
    };
    let pat: Vec<&str> = pattern.split('/').collect();
    let pth: Vec<&str> = path.split('/').collect();
    match_segments(&pat, &pth)
}

/// True if `path` matches ANY of `applies_to` and NONE of `exclude`.
pub fn applies(applies_to: &[String], exclude: &[String], path: &str) -> bool {
    applies_to.iter().any(|p| matches(p, path)) && !exclude.iter().any(|p| matches(p, path))
}

fn match_segments(pat: &[&str], pth: &[&str]) -> bool {
    match pat.first() {
        None => pth.is_empty(),
        Some(&"**") => {
            match_segments(&pat[1..], pth) || (!pth.is_empty() && match_segments(pat, &pth[1..]))
        }
        Some(seg) => {
            !pth.is_empty() && match_segment(seg, pth[0]) && match_segments(&pat[1..], &pth[1..])
        }
    }
}

fn match_segment(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = s.chars().collect();
    match_chars(&p, &s)
}

fn match_chars(p: &[char], s: &[char]) -> bool {
    match (p.first(), s.first()) {
        (None, None) => true,
        (Some('*'), _) => match_chars(&p[1..], s) || (!s.is_empty() && match_chars(p, &s[1..])),
        (Some('?'), Some(_)) => match_chars(&p[1..], &s[1..]),
        (Some(pc), Some(sc)) if pc == sc => match_chars(&p[1..], &s[1..]),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_star_matches_across_directories() {
        assert!(matches("**/*.ts", "src/a.ts"));
        assert!(matches("**/*.ts", "a.ts"));
        assert!(matches("**/*.ts", "deep/nested/dir/a.ts"));
        assert!(!matches("**/*.ts", "a.js"));
    }

    #[test]
    fn single_star_does_not_cross_a_slash() {
        assert!(!matches("*.ts", "src/a.ts"));
        assert!(matches("*.ts", "a.ts"));
    }

    #[test]
    fn question_mark_matches_exactly_one_char() {
        assert!(matches("a?.ts", "ab.ts"));
        assert!(!matches("a?.ts", "abc.ts"));
        assert!(!matches("a?.ts", "a.ts"));
    }

    #[test]
    fn node_modules_exclude_pattern() {
        assert!(matches("**/node_modules/**", "node_modules/x/y.js"));
        assert!(matches("**/node_modules/**", "a/b/node_modules/c.js"));
        // Segment matching is exact, not substring — "node_modulesish"
        // must not match the "node_modules" segment.
        assert!(!matches("**/node_modules/**", "src/node_modulesish/c.js"));
    }

    #[test]
    fn applies_honors_exclude_over_applies_to() {
        let applies_to = vec!["**/*.ts".to_string()];
        let exclude = vec!["**/node_modules/**".to_string()];
        assert!(applies(&applies_to, &exclude, "src/a.ts"));
        assert!(!applies(&applies_to, &exclude, "node_modules/pkg/a.ts"));
        assert!(!applies(&applies_to, &exclude, "src/a.js"));
    }

    /// #1959 finding 7: a trailing-slash pattern like `node_modules/` reads
    /// as "this whole directory" to a human author, but before this fix
    /// `match_segments` required an EXACT empty final segment to match —
    /// `node_modules/` (segments `["node_modules", ""]`) never matched a
    /// path under it (`node_modules/pkg/a.ts`), so an exclude written the
    /// natural way silently excluded nothing.
    #[test]
    fn trailing_slash_pattern_excludes_the_whole_directory() {
        assert!(matches("node_modules/", "node_modules/pkg/a.ts"));
        assert!(matches("node_modules/", "node_modules/a.ts"));
        assert!(!matches("node_modules/", "not_node_modules/a.ts"));

        let applies_to = vec!["**/*.ts".to_string()];
        let exclude = vec!["node_modules/".to_string()];
        assert!(!applies(&applies_to, &exclude, "node_modules/pkg/a.ts"));
        assert!(applies(&applies_to, &exclude, "src/a.ts"));
    }
}
