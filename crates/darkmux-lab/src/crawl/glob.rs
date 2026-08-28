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
}
