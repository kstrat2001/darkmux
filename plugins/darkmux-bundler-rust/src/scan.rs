//! Hand-rolled Rust-syntax function-boundary scanning — brace-matching,
//! not an AST. Mirrors the established pattern in
//! `darkmux-lab::lab::bundle::scan` (a hand-rolled byte/char scanner, no
//! `regex` dependency, measurement-acceptable rather than
//! parser-perfect): good enough to drive review quality, not a compiler
//! front end.
//!
//! Two lookup modes, because the caller may or may not have a worktree
//! (darkmux's own self-review workflow runs with NO checkout — see
//! `--github`/`--head-sha` in `.github/workflows/darkmux-review.yml` —
//! so this crate's diff-only path is the one that matters in production,
//! not an afterthought):
//! - [`find_enclosing_fn`] — full-file lookup (a real worktree is
//!   available): scan backward from a touched line for the nearest `fn`
//!   whose brace-matched extent contains it.
//! - [`find_all_fns_in_lines`] — bounded lookup (diff-only): every `fn`
//!   signature within a hunk's own line window (context lines included),
//!   each brace-matched forward within what's available. A hunk can
//!   legitimately contain several complete functions (a brand-new file
//!   has no pre-image, so unified diff has no per-function hunk
//!   boundaries — the whole file lands in one hunk). When a signature's
//!   closing brace never appears in the window, the caller marks that
//!   bundle `truncated: true` rather than guessing; when NO signature
//!   appears in the window at all, the caller falls back to an honest,
//!   unnamed hunk-level bundle.

/// Strip string/char literal CONTENTS (replaced with a single space each
/// so identifier boundaries on either side stay intact), block-comment
/// contents, and everything from a `//` line comment onward. Shared by
/// [`brace_delta`] (braces inside a format string or a commented-out
/// code fragment must not count) and `facts::extract_calls` (call-shaped
/// text inside a string literal must not register as a real call site)
/// — one sanitizer, so the string/lifetime/comment disambiguation logic
/// lives in exactly one place.
///
/// `in_block_comment` is CALLER-OWNED and threaded across every line of
/// a scan — a `/* ... */` block comment can span multiple lines, and a
/// brace inside one (e.g. commented-out code like `/* if x { */`) must
/// not corrupt the count on ANY of the lines it spans, not just the one
/// where it opens. Pass `&mut false` for a scan that's known to start
/// outside any comment (every current caller does).
///
/// Distinguishes a char literal (`'x'`, `'\n'`) from a lifetime (`'a`,
/// `'static`) by lookahead — lifetimes are far more common in real Rust
/// source, so an ambiguous `'` defaults to "not a literal" (left alone)
/// rather than the reverse, which would silently eat the rest of the
/// line on every generic-lifetime signature.
pub fn sanitize_line(line: &str, in_block_comment: &mut bool) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0usize;
    let mut in_string = false;
    while i < chars.len() {
        if *in_block_comment {
            if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                *in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        let c = chars[i];
        if in_string {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            *in_block_comment = true;
            i += 2;
            continue;
        }
        match c {
            '/' if chars.get(i + 1) == Some(&'/') => break,
            '"' => {
                in_string = true;
                out.push(' ');
                i += 1;
            }
            '\'' => {
                if chars.get(i + 1) == Some(&'\\') {
                    if let Some(close) = (i + 2..chars.len()).find(|&j| chars[j] == '\'') {
                        i = close + 1;
                    } else {
                        i += 1;
                    }
                    out.push(' ');
                } else if chars.get(i + 2) == Some(&'\'') {
                    i += 3;
                    out.push(' ');
                } else {
                    out.push(c); // lifetime — no skip
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// One brace-matched line delta over the sanitized text — a `{`/`}`
/// inside a format string or a (possibly multi-line) comment never
/// corrupts the count. `in_block_comment` is threaded exactly like
/// [`sanitize_line`]'s.
pub fn brace_delta(line: &str, in_block_comment: &mut bool) -> i32 {
    let mut delta = 0i32;
    for c in sanitize_line(line, in_block_comment).chars() {
        match c {
            '{' => delta += 1,
            '}' => delta -= 1,
            _ => {}
        }
    }
    delta
}

/// Strip leading `fn`-declaration modifiers (`pub`, `pub(crate)`,
/// `pub(in <path>)`, `const`, `async`, `unsafe`, `extern "C"`, in any
/// combination/order a real signature can carry) and return what's left.
/// Loops until no modifier matches, so `pub(crate) async unsafe fn`
/// strips cleanly.
fn strip_fn_modifiers(line: &str) -> &str {
    let mut rest = line.trim_start();
    loop {
        let mut advanced = false;
        // `pub(in <path>)` is checked BEFORE the bare "pub" fallback below
        // — a dynamic-length path, not a fixed keyword, so it needs its
        // own closing-paren search rather than a fixed-prefix match. If
        // this weren't tried first, the bare "pub" branch would still
        // strip (its guard accepts a following `(`), leaving `(in
        // crate::foo)` unconsumed — which nothing else in this loop
        // recognizes, so the signature would fail to resolve at all
        // (previously a silent function drop, not just a mislocation).
        if let Some(r) = rest.strip_prefix("pub(in ") {
            if let Some(close_idx) = r.find(')') {
                rest = r[close_idx + 1..].trim_start();
                advanced = true;
            }
        }
        if advanced {
            continue;
        }
        for prefix in ["pub(crate)", "pub(super)", "pub(self)", "pub"] {
            if let Some(r) = rest.strip_prefix(prefix) {
                if r.is_empty() || r.starts_with(char::is_whitespace) || r.starts_with('(') {
                    rest = r.trim_start();
                    advanced = true;
                    break;
                }
            }
        }
        if advanced {
            continue;
        }
        for kw in ["const", "async", "unsafe", "default"] {
            if let Some(r) = rest.strip_prefix(kw) {
                if r.starts_with(char::is_whitespace) {
                    rest = r.trim_start();
                    advanced = true;
                    break;
                }
            }
        }
        if advanced {
            continue;
        }
        if let Some(r) = rest.strip_prefix("extern") {
            let r = r.trim_start();
            let r = if let Some(after_quote) = r.strip_prefix('"') {
                match after_quote.find('"') {
                    Some(idx) => after_quote[idx + 1..].trim_start(),
                    None => r,
                }
            } else {
                r
            };
            rest = r;
            advanced = true;
        }
        if !advanced {
            return rest;
        }
    }
}

/// `true` iff `line` (after stripping known modifiers) is a Rust function
/// declaration — `fn <name>(` or `fn <name><generics>(`.
pub fn is_fn_signature(line: &str) -> bool {
    extract_fn_name(line).is_some()
}

/// The function name from a signature line, or `None` if `line` isn't
/// one. `fn foo(...)`, `fn foo<T>(...)`, `pub(crate) async fn bar(...)`
/// all resolve; a bare `fn(...)` closure-type position does not (no name
/// to key a bundle id on).
pub fn extract_fn_name(line: &str) -> Option<String> {
    let rest = strip_fn_modifiers(line);
    let rest = rest.strip_prefix("fn")?;
    let rest = rest.strip_prefix(char::is_whitespace)?;
    let rest = rest.trim_start();
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(rest[..end].to_string())
}

/// Attribute (`#[...]`) or doc-comment (`///`, `//!`) line immediately
/// preceding a signature — walked backward so the bundle's code span
/// includes `#[test]`/`#[tokio::test]`/doc context, not just the bare
/// `fn` line.
fn is_attached_prefix_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("#[") || t.starts_with("///") || t.starts_with("//!")
}

/// Walk backward from `sig_idx` over any attribute/doc-comment lines,
/// returning the first line index that belongs to this function's
/// bundle (inclusive).
fn extent_start(lines: &[String], sig_idx: usize) -> usize {
    let mut start = sig_idx;
    while start > 0 && is_attached_prefix_line(&lines[start - 1]) {
        start -= 1;
    }
    start
}

/// Brace-match forward from `sig_idx` for this function's closing line.
/// `None` when the braces never balance within `lines` — the function
/// extends beyond what's available (the diff-only truncation case; the
/// caller marks `truncated: true`).
fn find_fn_extent_end(lines: &[String], sig_idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut seen_open = false;
    let mut in_block_comment = false;
    for (i, line) in lines.iter().enumerate().skip(sig_idx) {
        let d = brace_delta(line, &mut in_block_comment);
        if d != 0 {
            seen_open = true;
        }
        depth += d;
        if seen_open && depth <= 0 {
            return Some(i);
        }
    }
    None
}

/// One resolved function span within a line array: `[start, end]`
/// inclusive 0-indexed positions in `lines`, the function name, and
/// whether the closing brace was actually found (`truncated` is the
/// caller's inverse of this).
pub struct FnSpan {
    pub name: String,
    pub start0: usize,
    pub end0: usize,
    pub closed: bool,
}

/// Resolve one `FnSpan` from a KNOWN signature line index — the shared
/// core [`find_all_fns_in_lines`] (every signature in the window) and
/// [`find_enclosing_fn`] (full-file, nearest-above-a-point lookup) build
/// on.
fn resolve_span_from_signature(lines: &[String], sig_idx: usize) -> Option<FnSpan> {
    let name = extract_fn_name(&lines[sig_idx])?;
    let start0 = extent_start(lines, sig_idx);
    match find_fn_extent_end(lines, sig_idx) {
        Some(end0) => Some(FnSpan { name, start0, end0, closed: true }),
        None => Some(FnSpan { name, start0, end0: lines.len().saturating_sub(1), closed: false }),
    }
}

/// Every function signature within `lines`, each resolved to its own
/// brace-matched extent — the multi-function-per-hunk case (a large
/// additive hunk, most commonly a brand-new file, where unified diff
/// format has no old-side content to anchor per-function hunk
/// boundaries against, so N functions land in ONE hunk). Signatures that
/// fall INSIDE an already-resolved earlier function's extent are
/// skipped (a nested `fn` — a closure-adjacent inner function, or a
/// function defined inside another for scoping — is part of its
/// parent's bundle, not a sibling top-level entry).
pub fn find_all_fns_in_lines(lines: &[String]) -> Vec<FnSpan> {
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        if is_fn_signature(&lines[i]) {
            if let Some(span) = resolve_span_from_signature(lines, i) {
                // Blast-radius containment for a brace miscount (defense
                // in depth alongside `sanitize_line`'s block-comment
                // handling above): an unclosed span resumes at `i + 1`,
                // NOT `end0 + 1`. A genuinely truncated function is
                // always the last one in a diff-only window — nothing
                // valid follows it, so resuming at `i + 1` costs
                // nothing there. But if `closed: false` came from a
                // brace-counting error rather than a real truncation,
                // `end0` can run all the way to the end of `lines`,
                // and jumping there would silently skip every real
                // sibling that follows. A mislocated span is an
                // acceptable heuristic error; a silently dropped
                // function is not.
                let next = if span.closed { span.end0 + 1 } else { i + 1 };
                spans.push(span);
                i = next.max(i + 1);
                continue;
            }
        }
        i += 1;
    }
    spans
}

/// Full-file lookup (a worktree is available): scan backward from
/// `touched_idx` for the nearest enclosing `fn`, verifying its
/// brace-matched extent actually CONTAINS `touched_idx` (the naive
/// "nearest signature above" guess can be wrong when the touched line
/// sits between two sibling functions, e.g. in a trailing blank-line
/// gap) — falls through to the next candidate signature above when it
/// doesn't.
pub fn find_enclosing_fn(lines: &[String], touched_idx: usize) -> Option<FnSpan> {
    let mut search_from = touched_idx.min(lines.len().saturating_sub(1));
    loop {
        let sig_idx = (0..=search_from).rev().find(|&i| is_fn_signature(&lines[i]))?;
        let name = extract_fn_name(&lines[sig_idx])?;
        let start0 = extent_start(lines, sig_idx);
        if let Some(end0) = find_fn_extent_end(lines, sig_idx) {
            if end0 >= touched_idx {
                return Some(FnSpan { name, start0, end0, closed: true });
            }
            // This candidate closed BEFORE the touched line — it's a
            // sibling above, not the enclosing function. Keep searching
            // above it.
            if sig_idx == 0 {
                return None;
            }
            search_from = sig_idx - 1;
            continue;
        }
        // Never closes within the file at all (shouldn't happen on a
        // syntactically valid file, but stay honest rather than loop
        // forever): report it truncated rather than fail.
        return Some(FnSpan { name, start0, end0: lines.len().saturating_sub(1), closed: false });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    #[test]
    fn brace_delta_ignores_string_contents() {
        assert_eq!(brace_delta(r#"    let s = "{ not a brace }";"#, &mut false), 0);
    }

    #[test]
    fn brace_delta_ignores_line_comments() {
        assert_eq!(brace_delta("    // this { comment } has braces", &mut false), 0);
    }

    #[test]
    fn brace_delta_distinguishes_lifetime_from_char_literal() {
        assert_eq!(
            brace_delta("fn foo<'a>(x: &'a str) {", &mut false),
            1,
            "lifetime must not eat the real brace"
        );
        assert_eq!(brace_delta("let c = '{';", &mut false), 0, "a char literal brace must not count");
    }

    #[test]
    fn brace_delta_ignores_single_line_block_comment() {
        assert_eq!(brace_delta("    /* if x { */ real_code();", &mut false), 0);
    }

    #[test]
    fn brace_delta_threads_block_comment_state_across_lines() {
        // A multi-line block comment containing an unbalanced brace must
        // not corrupt the count on ANY line it spans, not just the one
        // where it opens.
        let mut state = false;
        assert_eq!(brace_delta("/* commented out:", &mut state), 0);
        assert!(state, "still inside the block comment after line 1");
        assert_eq!(brace_delta("   while cond {", &mut state), 0, "the brace is still inside the comment");
        assert!(state, "still inside the block comment after line 2");
        assert_eq!(brace_delta("*/ fn real() {", &mut state), 1, "the comment closes, then a real brace counts");
        assert!(!state, "the block comment has closed");
    }

    #[test]
    fn extract_fn_name_handles_modifier_combinations() {
        assert_eq!(extract_fn_name("fn plain(x: u32) {"), Some("plain".to_string()));
        assert_eq!(extract_fn_name("pub fn public_one() {"), Some("public_one".to_string()));
        assert_eq!(extract_fn_name("pub(crate) async fn combo() {"), Some("combo".to_string()));
        assert_eq!(extract_fn_name("pub async unsafe fn many_mods() {"), Some("many_mods".to_string()));
        assert_eq!(extract_fn_name("extern \"C\" fn ffi_fn() {"), Some("ffi_fn".to_string()));
        assert_eq!(extract_fn_name("fn generic<T: Clone>(x: T) {"), Some("generic".to_string()));
        assert_eq!(extract_fn_name("    let x = 1;"), None);
        assert_eq!(extract_fn_name("struct Foo { field: u32 }"), None);
        // QA-caught regression: `pub(in <path>)` used to leave `(in
        // crate::foo)` unconsumed after the bare "pub" branch matched
        // it too eagerly — the signature never resolved at all (a
        // silent drop, not just a mislocation).
        assert_eq!(
            extract_fn_name("pub(in crate::foo) fn restricted() {"),
            Some("restricted".to_string())
        );
    }

    #[test]
    fn find_all_fns_in_lines_resolves_simple_function() {
        let l = lines("fn foo(x: u32) -> u32 {\n    let y = x + 1;\n    y\n}\n");
        let spans = find_all_fns_in_lines(&l);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "foo");
        assert_eq!(spans[0].start0, 0);
        assert_eq!(spans[0].end0, 3);
        assert!(spans[0].closed);
    }

    #[test]
    fn find_all_fns_in_lines_includes_attributes_and_doc_comments() {
        let l = lines("/// Does a thing.\n#[test]\nfn my_test() {\n    assert!(true);\n}\n");
        let spans = find_all_fns_in_lines(&l);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].start0, 0, "the doc comment + attribute must be included");
        assert_eq!(spans[0].end0, 4);
    }

    #[test]
    fn find_all_fns_in_lines_marks_truncated_when_never_closes() {
        let l = lines("fn long_fn() {\n    let a = 1;\n    let b = 2;\n");
        let spans = find_all_fns_in_lines(&l);
        assert_eq!(spans.len(), 1);
        assert!(!spans[0].closed, "the window never showed a closing brace");
        assert_eq!(spans[0].end0, l.len() - 1);
    }

    /// QA-caught regression (PR #1333 review): a commented-out brace used
    /// to corrupt `brace_delta`'s count, mark the enclosing function
    /// `closed: false`, and then `find_all_fns_in_lines` jumped past the
    /// WHOLE window (`end0 + 1`, which for an unclosed span is
    /// `lines.len()`) — silently dropping every real sibling that
    /// followed. Two independent fixes now cover this: the root cause
    /// (`sanitize_line` understands block comments, so the brace never
    /// miscounts in the first place) and a containment backstop (an
    /// unclosed span resumes at `i + 1`, not `end0 + 1`, so even a
    /// hypothetical FUTURE miscount couldn't silently swallow siblings
    /// again). This test proves the root-cause fix specifically: `first`
    /// must resolve CLOSED (not truncated) despite its unbalanced-looking
    /// commented-out brace, and both siblings must still be found.
    #[test]
    fn find_all_fns_in_lines_survives_a_commented_out_brace() {
        let l = lines(
            "fn first() {\n    /* if x { */\n    1\n}\n\nfn second() {\n    2\n}\n\nfn third() {\n    3\n}\n",
        );
        let spans = find_all_fns_in_lines(&l);
        assert_eq!(spans.len(), 3, "all three siblings must be found, not just the first");
        assert_eq!(spans[0].name, "first");
        assert!(spans[0].closed, "the commented-out brace must not corrupt the count");
        assert_eq!(spans[1].name, "second");
        assert_eq!(spans[2].name, "third");
    }

    #[test]
    fn find_all_fns_in_lines_survives_a_multiline_block_comment() {
        let l = lines(
            "fn alpha() {\n    /* a comment\n       spanning multiple lines\n       with a { brace */\n    1\n}\n\nfn beta() {\n    2\n}\n",
        );
        let spans = find_all_fns_in_lines(&l);
        assert_eq!(spans.len(), 2, "both functions must be found across the multi-line comment");
        assert_eq!(spans[0].name, "alpha");
        assert!(spans[0].closed);
        assert_eq!(spans[1].name, "beta");
    }

    #[test]
    fn find_enclosing_fn_skips_a_closed_sibling_above() {
        let l = lines(
            "fn sibling_above() {\n    1\n}\n\nfn target() {\n    let x = 2;\n    x\n}\n",
        );
        let span = find_enclosing_fn(&l, 5).expect("should find target, not sibling_above");
        assert_eq!(span.name, "target");
        assert_eq!(span.start0, 4);
        assert_eq!(span.end0, 7);
    }

    #[test]
    fn find_all_fns_in_lines_resolves_every_sibling_in_one_window() {
        // The brand-new-file case: unified diff has no old-side content
        // to anchor per-function hunk boundaries against, so N functions
        // land in ONE hunk. A single-anchor lookup would only ever find
        // the last one; this must find all three.
        let l = lines(
            "fn first() {\n    1\n}\n\nfn second() {\n    2\n}\n\nfn third() {\n    3\n}\n",
        );
        let spans = find_all_fns_in_lines(&l);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].name, "first");
        assert_eq!(spans[1].name, "second");
        assert_eq!(spans[2].name, "third");
        assert!(spans.iter().all(|s| s.closed));
    }

    #[test]
    fn find_all_fns_in_lines_skips_nested_signatures() {
        let l = lines(
            "fn outer() {\n    fn inner() {\n        1\n    }\n    inner()\n}\n",
        );
        let spans = find_all_fns_in_lines(&l);
        assert_eq!(spans.len(), 1, "the nested fn is part of outer's own bundle, not a sibling");
        assert_eq!(spans[0].name, "outer");
    }

    #[test]
    fn find_all_fns_in_lines_empty_when_no_signature_present() {
        let l = lines("    let mid = compute();\n    return mid;\n");
        assert!(find_all_fns_in_lines(&l).is_empty());
    }
}
