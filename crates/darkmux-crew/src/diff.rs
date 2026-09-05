//! Unified-diff parsing (#2310 P4b move) — a straight port of the Python
//! reference's `parse_diff` (`bundler.py`, Phase A). Splits a multi-file
//! unified diff into per-file hunks, each carrying the added/removed/old/new
//! line sets a consumer needs.
//!
//! No `regex` crate (workspace dep discipline) — the two line shapes this
//! parses (`+++ [b/]<path>[\t<timestamp>]` and `@@ -a,b +c,d @@`) are
//! simple enough for hand-rolled prefix/token parsing. Every unified-diff
//! dialect a real tool emits is accepted (#2310 fix-loop B1) — see
//! [`header_path`] for the three verified header shapes and why the
//! optional `b/` prefix is accepted rather than required, and why a
//! header is recognized by POSITION (an adjacent `--- `/`+++ ` pair, or
//! a `+++ ` with no hunk body around it) rather than by prefix alone.
//!
//! **Canonical home, moved from `darkmux-lab`'s `bundle::diff` (#2310 P4b).**
//! `deliver_github_review`'s "is this line inside the diff" check needs the
//! SAME parser `darkmux-lab`'s bundler already had, and darkmux-lab depends
//! on darkmux-crew (never the reverse) — so the crate boundary is a reason
//! to MOVE the parser down to where both sides can reach it, not to
//! hand-roll a second copy in darkmux-crew (the original mistake this
//! move fixes; see this crate's own `deliver_github_review.rs` module doc
//! for the incident). `darkmux_lab::lab::bundle::diff` re-exports this
//! module's `Hunk`/`parse_diff` verbatim, so every existing lab call site
//! (`bundle::source`, `bundle::mod`) is untouched.

use std::collections::BTreeSet;


/// One `@@ ... @@` hunk within a file's diff.
#[derive(Debug, Clone, Default)]
pub struct Hunk {
    /// The hunk's starting line number in the NEW file (1-indexed), from
    /// the `@@ -a,b +c,d @@` header's `+c`.
    pub new_start: u32,
    /// The hunk's starting line number in the OLD file (1-indexed), from
    /// the same header's `-a` (#2310 P4b review, M-B). Not needed by the
    /// bundler (every existing caller anchors off the NEW side only) —
    /// added for `deliver_github_review`'s suggestion-block anchoring: a
    /// mod's kit is itself a unified diff whose OLD side names the lines
    /// it replaces in the file's CURRENT state, which is the SAME
    /// coordinate space the PR diff's NEW side already occupies (the file
    /// as the PR leaves it). Additive — every existing reader of `Hunk`
    /// that never looks at this field is unaffected.
    pub old_start: u32,
    /// Every line number (1-indexed, in the NEW file) touched by this
    /// hunk — added lines AND unchanged context lines (matches the
    /// reference: context lines advance `new_ln` and land in
    /// `new_lines` too, since a changed function can be located via a
    /// context line inside it just as well as an added one). Do NOT
    /// narrow this to "actually changed" lines — locating the enclosing
    /// function is exactly the case that needs context lines included.
    /// For "was this specific line actually added" (as opposed to merely
    /// present in the hunk's span), see `added_lines` below.
    pub new_lines: BTreeSet<u32>,
    /// Subset of `new_lines`: just the line numbers that were actually
    /// ADDED (`+` prefix) — no context lines (#1605 follow-up, QA
    /// finding). `new_lines` deliberately mixes added and context so a
    /// changed function can be located via either; a caller that needs to
    /// know whether a specific line was really touched by the diff (e.g.
    /// deciding whether an unenclosed line is real changed code worth
    /// bundling, vs. unchanged context that merely fell inside a hunk's
    /// span) wants this set instead.
    pub added_line_numbers: BTreeSet<u32>,
    /// Every line of the pre-image within this hunk's span: removed
    /// lines AND unchanged context lines, in order.
    pub old_block: Vec<String>,
    /// Every line of the post-image within this hunk's span: added
    /// lines AND unchanged context lines, in order.
    pub new_block: Vec<String>,
    /// Just the added lines (`+` prefix), in order.
    pub added: Vec<String>,
    /// Just the removed lines (`-` prefix), in order.
    pub removed: Vec<String>,
}

/// Split a unified multi-file diff into per-file hunks. Returns
/// `(path, hunks)` pairs in first-appearance order (mirrors Python's
/// `dict.setdefault` insertion-order semantics, which the reference
/// relies on for deterministic bundle ordering).
pub fn parse_diff(diff_text: &str) -> Vec<(String, Vec<Hunk>)> {
    let mut files: Vec<(String, Vec<Hunk>)> = Vec::new();
    let mut path: Option<String> = None;
    let mut cur: Option<Hunk> = None;
    let mut new_ln: u32 = 0;
    // The open hunk's own declared extents, used only to decide whether
    // it is FINISHED (#2310 fix-loop round 2, blocker) — see signal 3.
    let mut declared = HunkHeader { old_start: 0, old_count: 0, new_start: 0, new_count: 0 };

    fn flush(files: &mut Vec<(String, Vec<Hunk>)>, path: &Option<String>, cur: &mut Option<Hunk>) {
        if let (Some(h), Some(p)) = (cur.take(), path) {
            match files.iter_mut().find(|(fp, _)| fp == p) {
                Some(entry) => entry.1.push(h),
                None => files.push((p.clone(), vec![h])),
            }
        }
    }

    // (#2310 fix-loop R2) Indexed, not a plain `for` over `lines()` — a
    // file header is the ADJACENT `--- <old>` / `+++ <new>` PAIR, so
    // recognizing one needs a one-line lookahead. See the loop's own
    // header-detection comment for why position, not prefix alone,
    // decides.
    let lines: Vec<&str> = diff_text.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let ln = lines[i];
        // (#2310 fix-loop R2, R3 — review regressions) HEADER DETECTION IS
        // POSITIONAL. Widening the old literal `+++ b/` to a bare `+++ `
        // prefix made `+++ ` a header ANYWHERE — including inside a hunk
        // body, where a raw `+++ foo` line is an ADDED line whose content
        // is `++ foo`. That truncated the hunk, dropped every later line,
        // and rebound the path to a phantom file. A unified diff defines
        // the file header as the adjacent `--- <old>` / `+++ <new>` pair,
        // so that is what is matched here:
        //
        //   1. `--- <old>` immediately followed by `+++ <new>` — a file
        //      header pair, wherever it appears. The NEW side names the
        //      path (the OLD side is `/dev/null` for an added file and is
        //      never the path we bind).
        //   2. a lone `+++ <new>` with NO hunk open — a model-authored kit
        //      commonly omits the `--- ` half (see `mods::
        //      looks_like_unified_diff`), and at that point there is no
        //      hunk body for it to be content OF.
        //   3. a lone `+++ <new>` whose NEXT line is a hunk header AND
        //      whose currently-open hunk has already consumed the line
        //      count its own `@@` declared — the other half-headered
        //      shape, a concatenation of per-file `+++`/`@@` blocks with
        //      the `--- ` halves omitted.
        //
        //      The completeness test is the whole load-bearing part
        //      (#2310 fix-loop round 2, blocker — PROVEN). Without it,
        //      signal 3 fired on an ordinary two-hunk file whenever an
        //      added line whose content is `++ x` happened to sit right
        //      before the second `@@`: that line was swallowed AND the
        //      file's real second hunk bound to a phantom path `x`, so
        //      every finding from hunk 2 onward silently dropped out of
        //      the in-diff check. `@@` closing the open HUNK was never
        //      the risk; rebinding the PATH was.
        //
        //      The completeness test has its own cost, stated rather than
        //      hidden (#2310 fix-loop round 3, R4): it trusts the `@@`
        //      counts. A half-headered kit whose header OVER-declares
        //      (says `+1,9` and then supplies four lines, which a model
        //      writing a patch by hand does) leaves its hunk permanently
        //      unfinished, so the NEXT file's `+++ ` is read as content
        //      and that file MERGES into the current one — its hunks and
        //      path are lost. Nothing with `--- ` halves is affected
        //      (signal 1 has no completeness test), and a correctly
        //      counted concatenation is not either. The trade is
        //      deliberate: a miscounted kit loses a file that was already
        //      malformed, while the alternative lost hunk 2 onward of
        //      every WELL-FORMED two-hunk diff — the common case.
        //
        // Anything else starting `+++ `/`--- ` is hunk CONTENT, which is
        // also why the old `!ln.starts_with("+++")` / `!ln.starts_with(
        // "---")` content guards are gone (R3): they silently DROPPED an
        // added line whose content begins `++` and a removed line whose
        // content begins `--`, and a dropped added line fails to advance
        // `new_ln`, shifting every later line — the same class of bug B2
        // fixed for blank lines.
        //
        // Known, accepted corner, stated at full cost (#2310 fix-loop
        // round 2, item 3): a removed line whose content is `-- x` (raw
        // `--- x`) immediately followed by an added line whose content is
        // `++ y` (raw `+++ y`) reads as a header pair, and the damage is
        // not cosmetic — the open hunk is TRUNCATED there, every line
        // after the pair (trailing context included) is LOST from it, and
        // the path rebinds to `y`, a file that never materializes in the
        // tree. Findings in the lost tail then fail the in-diff check
        // silently. Accepted only because it needs two adjacent
        // adversarially-shaped content lines in that exact order, which
        // no diff-producing tool emits; signal 1 has no completeness
        // escape hatch the way signal 3 does, because a `--- `/`+++ `
        // pair mid-hunk is not a shape any real concatenation produces.
        if ln.starts_with("--- ") {
            if let Some(next) = lines.get(i + 1).and_then(|n| n.strip_prefix("+++ ")) {
                flush(&mut files, &path, &mut cur);
                path = header_path(next);
                cur = None;
                i += 2;
                continue;
            }
        }
        if let Some(rest) = ln.strip_prefix("+++ ") {
            let hunk_finished = match cur.as_ref() {
                None => true,
                Some(h) => {
                    // Lines consumed so far, read off the hunk itself:
                    // the new-side cursor has advanced once per added and
                    // per context line, and `old_block` holds exactly the
                    // removed + context lines.
                    let consumed_new = new_ln.saturating_sub(h.new_start);
                    let consumed_old = h.old_block.len() as u32;
                    // A pure-deletion hunk declares `+c,0`, so the
                    // new-side test passes from the first line — fall back
                    // to the old side for those rather than declaring the
                    // hunk finished before it starts.
                    consumed_new >= declared.new_count && (declared.new_count > 0 || consumed_old >= declared.old_count)
                }
            };
            if hunk_finished && lines.get(i + 1).is_some_and(|n| parse_hunk_header(n).is_some()) {
                flush(&mut files, &path, &mut cur);
                path = header_path(rest);
                i += 1;
                continue;
            }
        }
        i += 1;
        if let Some(h) = parse_hunk_header(ln) {
            flush(&mut files, &path, &mut cur);
            cur = Some(Hunk {
                new_start: h.new_start,
                old_start: h.old_start,
                ..Default::default()
            });
            new_ln = h.new_start;
            declared = h;
            continue;
        }
        if cur.is_none() || path.is_none() {
            continue;
        }
        if let Some(content) = ln.strip_prefix('+') {
            let h = cur.as_mut().unwrap();
            h.added.push(content.to_string());
            h.new_block.push(content.to_string());
            h.new_lines.insert(new_ln);
            h.added_line_numbers.insert(new_ln);
            new_ln += 1;
        } else if let Some(content) = ln.strip_prefix('-') {
            let h = cur.as_mut().unwrap();
            h.removed.push(content.to_string());
            h.old_block.push(content.to_string());
        } else if let Some(content) = ln.strip_prefix(' ') {
            let h = cur.as_mut().unwrap();
            h.old_block.push(content.to_string());
            h.new_block.push(content.to_string());
            h.new_lines.insert(new_ln);
            new_ln += 1;
        } else if ln.is_empty() || ln == "\r" {
            // (#2310 fix-loop B2, S3-3) A TRULY empty line inside a hunk
            // body is a context line whose content is empty. git emits it
            // as a lone space, but everything that touches a patch on the
            // way here — editors, copy/paste, trailing-whitespace
            // strippers, the GitHub API — routinely drops that space.
            // Skipping such a line left `new_ln` un-advanced, which
            // shifted the line number of EVERY later line in the hunk and
            // desynced `old_block`/`new_block` from the file, so a finding
            // on a real changed line read as outside the diff. Treated as
            // context with empty content, exactly like ` ` would be.
            let h = cur.as_mut().unwrap();
            h.old_block.push(String::new());
            h.new_block.push(String::new());
            h.new_lines.insert(new_ln);
            new_ln += 1;
        }
        // Other lines (e.g. `\ No newline at end of file`, a `diff --git`
        // header, `index`/mode/similarity lines, and a `--- ` line whose
        // `+++ ` partner is missing) carry no line-content signal —
        // ignored, matching the reference.
    }
    flush(&mut files, &path, &mut cur);
    files
}

/// The path a `+++ <rest>` header names, or `None` when it names
/// `/dev/null` (a deleted file), decodes to nothing, or is a quoted value
/// that does not decode.
///
/// **Public because it is the ONE place a diff's path-shaped value is
/// normalized** (#2310 fix-loop round 3, R1). `darkmux-lab`'s
/// `diff_git_header_paths` reads the `diff --git <old> <new>` header for
/// its own accounting and needs byte-identical normalization: when it
/// hand-rolled its own, a git-quoted path landed there raw while
/// `parse_diff` bound the decoded form, the two never matched, and a file
/// that planned perfectly was ALSO reported as deleted. Two normalizers
/// is the same mistake this module's own doc records for two parsers.
/// Callers pass the value AFTER the `+++ `/`diff --git ` marker, with any
/// `b/` dialect prefix still attached — stripping it is this function's
/// job, not the caller's.
///
/// (#2310 fix-loop B1, S3-1 — PROVEN live) This used to be a literal
/// `strip_prefix("+++ b/")`, which recognized ONLY `git diff`'s default
/// dialect. Three shapes verified against real tool output on 2026-09-05:
///
/// ```text
/// +++ b/src/x.ts                          git diff (default)
/// +++ src/x.ts                            git diff --no-prefix
/// +++ src/x.ts\t2026-09-05 08:45:23       diff -u / diff -Naur
/// ```
///
/// A patch in either prefixless dialect parsed to ZERO files, so a
/// review run over it completed green with a noop payload — the
/// operator read "the PR is clean" off a diff that was never parsed. The
/// `-Naur` shape parsed the tab and timestamp INTO the path, which then
/// matched no file in the tree (`hunks_total:5, covered:0` → noop).
///
/// So: cut at the first tab (the timestamp field), drop a trailing `\r`
/// (a lone CR — `str::lines` already eats the one in a CRLF pair), then
/// strip an OPTIONAL `b/` prefix — `b/` only, never `a/`: no dialect
/// puts `a/` on the NEW side, and stripping it there mangled a
/// `--no-prefix` path under a top-level directory named `a`.
///
/// **The optional prefix is genuinely ambiguous and cannot be resolved
/// from one line.** A `--no-prefix` diff of a repo whose top-level
/// directory is literally named `b` yields `+++ b/foo.ts`, which is
/// indistinguishable from the default dialect's rendering of `foo.ts`.
/// Accepting the prefix is right for the overwhelmingly common case (git's
/// default) and for the operator's real one (`--no-prefix`); the `b/`-
/// directory repo is the sacrificed corner. Preferring the strict
/// dialect — the previous behavior — sacrifices EVERY prefixless patch
/// instead, silently, which is the bug being fixed.
pub fn header_path(rest: &str) -> Option<String> {
    let p = rest.split('\t').next().unwrap_or(rest);
    let p = p.strip_suffix('\r').unwrap_or(p);
    // (#2310 fix-loop R4) `core.quotepath` is git's DEFAULT, so a path
    // with a non-ASCII byte, a control character, a `"` or a `\` arrives
    // C-quoted with octal byte escapes: `+++ "b/src/caf\303\251.ts"`.
    // Binding THAT as the path meant no finding ever matched the file —
    // and because the file did "parse", the zero-files guard never fired
    // either, so the run read as a clean review. Unquoted BEFORE the
    // prefix strip: the quotes wrap the `b/` too.
    //
    // (#2310 fix-loop round 2, item 2) A value that LOOKS quoted but does
    // not decode is REFUSED, not passed through raw. Falling back to the
    // raw string bound a path containing quotes and backslash escapes —
    // one that can never match any file on disk — so the file was absent
    // from every in-diff check while the run still looked healthy. `None`
    // here means the hunk binds no file, exactly like `/dev/null`; the
    // caller's ledger (`plan.rs`) is where its absence is recorded.
    let owned;
    let p = if p.starts_with('"') {
        owned = unquote_c_path(p)?;
        owned.as_str()
    } else {
        p
    };
    // Only `b/` — the NEW side never carries `a/` in any dialect, and
    // stripping it there mangled a `--no-prefix` path under a top-level
    // directory literally named `a` (#2310 fix-loop, review).
    let p = p.strip_prefix("b/").unwrap_or(p);
    if p.is_empty() || p == "/dev/null" {
        None
    } else {
        Some(p.to_string())
    }
}

/// Decode one C-quoted path (`"a\303\251b"`), or `None` when `s` is not a
/// complete `"`-wrapped value or its escapes do not assemble into valid
/// UTF-8. Handles the escapes git emits: three-digit octal bytes
/// (assembled and validated as UTF-8), `\\`, `\"`, and the
/// `\a \b \f \n \r \t \v` set.
///
/// `None` is a REFUSAL, and [`header_path`] treats it as one for any
/// value that starts with `"` (#2310 fix-loop round 2, item 2) — the
/// earlier behavior of falling back to the raw quoted string bound an
/// unmatchable path instead of declining to bind one.
fn unquote_c_path(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let b = inner.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 >= b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let c = b[i + 1];
        if c.is_ascii_digit() && c < b'8' {
            let mut v: u32 = 0;
            let mut n = 0usize;
            while n < 3 {
                match b.get(i + 1 + n) {
                    Some(d) if d.is_ascii_digit() && *d < b'8' => {
                        v = v * 8 + u32::from(d - b'0');
                        n += 1;
                    }
                    _ => break,
                }
            }
            // A three-digit octal escape can name 0o400..=0o777, which is
            // not a byte — git never emits one, so refuse the whole path
            // rather than truncate it into a different file's name.
            out.push(u8::try_from(v).ok()?);
            i += 1 + n;
            continue;
        }
        out.push(match c {
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            other => other,
        });
        i += 2;
    }
    String::from_utf8(out).ok()
}

/// The four numbers a `@@ -a[,b] +c[,d] @@` header declares. The COUNTS
/// are load-bearing, not decoration (#2310 fix-loop round 2): they are
/// how `parse_diff` knows whether an open hunk has finished, which is
/// what stops a `+++ ` CONTENT line sitting right before the next `@@`
/// from being mistaken for a file header. An omitted count means 1.
#[derive(Debug, Clone, Copy)]
struct HunkHeader {
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
}

/// Parse `@@ -a[,b] +c[,d] @@...` into a [`HunkHeader`], or `None` if
/// `ln` isn't a hunk header.
/// Hand-rolled equivalent of `re.match(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@", ln)`.
///
/// (#2310 P4b review, M-B) Returns BOTH sides now (used to return only
/// `c`) — `Hunk::old_start` needs `a` too. Every existing call site reads
/// `.1` for what used to be the whole return value, so nothing downstream
/// of the new-side number changed.
fn parse_hunk_header(ln: &str) -> Option<HunkHeader> {
    let rest = ln.strip_prefix("@@ -")?;
    let space = rest.find(' ')?;
    let mut minus = rest[..space].split(',');
    let old_start: u32 = minus.next()?.parse().ok()?;
    // An omitted count means 1 (`@@ -a +c @@`), the unified-diff default.
    let old_count: u32 = match minus.next() {
        Some(d) => d.parse().ok()?,
        None => 1,
    };
    let after_minus = &rest[space + 1..];
    let plus_digits = after_minus.strip_prefix('+')?;
    let end = plus_digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(plus_digits.len());
    if end == 0 {
        return None;
    }
    // The char right after the digit run must be `,` (a `+c,d` count) or
    // ` ` (bare `+c`) followed eventually by ` @@` — anything else means
    // this wasn't really a hunk header (defensive; real diffs won't hit
    // this branch).
    let tail = &plus_digits[end..];
    if !(tail.starts_with(',') || tail.starts_with(' ')) {
        return None;
    }
    let new_start: u32 = plus_digits[..end].parse().ok()?;
    let new_count: u32 = match tail.strip_prefix(',') {
        Some(after_comma) => {
            let n = after_comma.find(|c: char| !c.is_ascii_digit()).unwrap_or(after_comma.len());
            after_comma[..n].parse().ok()?
        }
        None => 1,
    };
    Some(HunkHeader { old_start, old_count, new_start, new_count })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_file_single_hunk() {
        // Built via an explicit line array (not a backslash-continued
        // string literal) — a `\` line continuation strips ALL leading
        // whitespace off the next line, which would silently eat the
        // significant leading space on unified-diff context lines below.
        let diff = [
            "diff --git a/x.ts b/x.ts",
            "--- a/x.ts",
            "+++ b/x.ts",
            "@@ -1,3 +1,4 @@",
            " line one",
            "-old line",
            "+new line",
            "+added line",
            " line four",
            "",
        ]
        .join("\n");
        let files = parse_diff(&diff);
        assert_eq!(files.len(), 1);
        let (path, hunks) = &files[0];
        assert_eq!(path, "x.ts");
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.new_start, 1);
        assert_eq!(h.old_start, 1, "(#2310 P4b) the OLD-side start line, from the header's `-a`");
        assert_eq!(h.added, vec!["new line", "added line"]);
        assert_eq!(h.removed, vec!["old line"]);
        // new_lines: line one(1), new line(2), added line(3), line four(4)
        assert_eq!(
            h.new_lines,
            [1, 2, 3, 4].into_iter().collect::<BTreeSet<u32>>()
        );
        // (#1605 follow-up) `added_line_numbers` is the STRICT subset of
        // `new_lines` that was actually added — lines 1 and 4 are context
        // (unchanged), only 2 and 3 ("new line"/"added line") carry a `+`.
        assert_eq!(
            h.added_line_numbers,
            [2, 3].into_iter().collect::<BTreeSet<u32>>()
        );
    }

    #[test]
    fn dev_null_target_drops_file() {
        let diff = "+++ /dev/null\n@@ -1,2 +0,0 @@\n-gone\n-gone2\n";
        let files = parse_diff(diff);
        assert!(files.is_empty());
    }

    /// (#2310 fix-loop R2) The fixture carries its `--- ` half now. It
    /// used to be `+++ b/a.ts` twice with no `--- ` line — a shape no
    /// tool emits, and one that only passed by accident once header
    /// detection became positional: the second `+++` landed INSIDE the
    /// open hunk as content, and the following `@@` opened hunk two
    /// anyway, so the count assertion held while testing nothing. Pinned
    /// per-hunk content now so it cannot pass that way again.
    #[test]
    fn multiple_hunks_same_file_collect_under_one_entry() {
        let diff = ["--- a/a.ts", "+++ b/a.ts", "@@ -1,1 +1,1 @@", "+x", "--- a/a.ts", "+++ b/a.ts", "@@ -10,1 +10,1 @@", "+y", ""].join("\n");
        let files = parse_diff(&diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1.len(), 2);
        assert_eq!(files[0].1[0].added, vec!["x"]);
        assert_eq!(files[0].1[1].added, vec!["y"]);
    }

    /// (#2310 fix-loop B1, S3-1 — PROVEN live) The parser used to bind a
    /// file ONLY via a literal `+++ b/` prefix, so a `git diff
    /// --no-prefix` or a plain `diff -u`/`diff -Naur` patch parsed to ZERO
    /// files — a review run over such a diff completed green with a
    /// noop payload and the operator read that as "the PR is clean". Every
    /// dialect below is the SAME logical change and must parse to the same
    /// path, hunk count and line sets.
    ///
    /// The four header shapes are transcribed from real tool output
    /// (`git diff --cached -M`, the same with `--no-prefix`, and BSD
    /// `diff -Naur`, run 2026-09-05) — not guessed.
    #[test]
    fn every_unified_diff_dialect_parses_to_the_same_file_and_lines() {
        let body = ["@@ -1,3 +1,4 @@", " a", "-b", "+B", "+B2", " c"];
        let dialects: Vec<(&str, Vec<String>)> = vec![
            (
                "git default (a/ b/ prefixes)",
                vec!["diff --git a/src/x.ts b/src/x.ts".into(), "--- a/src/x.ts".into(), "+++ b/src/x.ts".into()],
            ),
            (
                "git --no-prefix",
                vec!["diff --git src/x.ts src/x.ts".into(), "--- src/x.ts".into(), "+++ src/x.ts".into()],
            ),
            (
                "diff -Naur (no prefix, tab + timestamp)",
                vec![
                    "diff -Naur old/src/x.ts new/src/x.ts".into(),
                    "--- old/src/x.ts\t2026-09-05 08:45:23".into(),
                    "+++ src/x.ts\t2026-09-05 08:45:23".into(),
                ],
            ),
            (
                "git default + tab-and-timestamp suffix",
                vec!["--- a/src/x.ts\t2026-09-05 08:45:23".into(), "+++ b/src/x.ts\t2026-09-05 08:45:23".into()],
            ),
        ];
        for (label, header) in dialects {
            let mut lines = header;
            lines.extend(body.iter().map(|s| s.to_string()));
            for (sep, sep_label) in [("\n", "LF"), ("\r\n", "CRLF")] {
                let text = lines.join(sep) + sep;
                let files = parse_diff(&text);
                assert_eq!(files.len(), 1, "{label} / {sep_label}: parsed {files:?}");
                assert_eq!(files[0].0, "src/x.ts", "{label} / {sep_label}");
                assert_eq!(files[0].1.len(), 1, "{label} / {sep_label}");
                let h = &files[0].1[0];
                assert_eq!(h.added, vec!["B", "B2"], "{label} / {sep_label}");
                assert_eq!(h.removed, vec!["b"], "{label} / {sep_label}");
                assert_eq!(h.new_start, 1, "{label} / {sep_label}");
                assert_eq!(h.old_start, 1, "{label} / {sep_label}");
                assert_eq!(h.new_lines, [1, 2, 3, 4].into_iter().collect::<BTreeSet<u32>>(), "{label} / {sep_label}");
                assert_eq!(h.added_line_numbers, [2, 3].into_iter().collect::<BTreeSet<u32>>(), "{label} / {sep_label}");
            }
        }
    }

    /// (#2310 fix-loop B1) `/dev/null` must still drop the file in the
    /// prefixless dialects too — the guard is on the PATH, not on the
    /// `b/` prefix that used to gate the whole branch.
    #[test]
    fn dev_null_target_drops_file_in_every_dialect() {
        for header in ["+++ /dev/null", "+++ b//dev/null", "+++ /dev/null\t2026-09-05 08:45:23"] {
            let diff = format!("{header}\n@@ -1,2 +0,0 @@\n-gone\n-gone2\n");
            assert!(parse_diff(&diff).is_empty(), "{header}");
        }
    }

    /// (#2310 fix-loop R2 — review regression) Widening `+++ b/` to
    /// `+++ ` made `+++ ` a header ANYWHERE, including inside a hunk
    /// body. A raw `+++ foo` line there is an ADDED line whose content is
    /// `++ foo` — treating it as a header truncated the hunk at that
    /// point, dropped every later line, and rebound the path to a phantom
    /// file. For a model-authored kit that means the suggestion anchors
    /// and replaces a SHORTER span than the operator's code.
    #[test]
    fn a_plus_plus_plus_line_inside_a_hunk_is_content_not_a_header() {
        let diff = ["+++ b/x.ts", "@@ -1,3 +1,4 @@", " a", "+++ foo", " b", "+z", ""].join("\n");
        let files = parse_diff(&diff);
        assert_eq!(files.len(), 1, "no phantom second file: {files:?}");
        assert_eq!(files[0].0, "x.ts");
        assert_eq!(files[0].1.len(), 1, "the hunk is not truncated: {:?}", files[0].1);
        let h = &files[0].1[0];
        assert_eq!(h.new_lines, [1, 2, 3, 4].into_iter().collect::<BTreeSet<u32>>());
        assert_eq!(h.added_line_numbers, [2, 4].into_iter().collect::<BTreeSet<u32>>());
        assert_eq!(h.added, vec!["++ foo", "z"]);
    }

    /// (#2310 fix-loop R2) The half-headered concatenation shape — one
    /// `+++ <path>` / `@@` block per file, `--- ` halves omitted — is what
    /// this repo's own bundler fixtures use and what a tool emitting only
    /// the new side produces. Every file must bind, not just the first:
    /// under a `--- `-pair-only rule the second `+++` landed inside the
    /// first file's open hunk as content and the diff collapsed to ONE
    /// file. Recognized by the third positional signal (the next line is
    /// a hunk header), which is already a hunk boundary either way.
    #[test]
    fn a_half_headered_concatenation_binds_every_file_not_just_the_first() {
        let diff = ["+++ b/a.ts", "@@ -1,1 +1,1 @@", "+x", "+++ b/b.ts", "@@ -1,1 +1,1 @@", "+y", ""].join("\n");
        let files = parse_diff(&diff);
        assert_eq!(files.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), vec!["a.ts", "b.ts"]);
        assert_eq!(files[0].1[0].added, vec!["x"]);
        assert_eq!(files[1].1[0].added, vec!["y"]);
    }

    /// (#2310 fix-loop R3 — review regression) The in-hunk guards
    /// `!ln.starts_with("+++")` / `!ln.starts_with("---")` DROPPED an
    /// added line whose content begins with `++` and a removed line whose
    /// content begins with `--`. A dropped added line fails to advance
    /// `new_ln`, which shifts every later line in the hunk — the exact
    /// class B2 fixed for blank lines. Header detection is positional
    /// now (a `--- `/`+++ ` pair, or `+++ ` with no hunk open), so the
    /// content guards are gone.
    #[test]
    fn added_and_removed_lines_whose_content_starts_with_a_marker_are_kept() {
        let diff = ["--- a/x.ts", "+++ b/x.ts", "@@ -1,3 +1,3 @@", " a", "++++ b/evil.ts", "---foo", " b", "+z", ""].join("\n");
        let files = parse_diff(&diff);
        assert_eq!(files.len(), 1, "{files:?}");
        let h = &files[0].1[0];
        assert_eq!(h.added, vec!["+++ b/evil.ts", "z"], "the `++++` line is an ADDED line with content `+++ b/evil.ts`");
        assert_eq!(h.removed, vec!["--foo"], "the `---foo` line is a REMOVED line with content `--foo`");
        // a(1), +++ b/evil.ts(2), b(3), z(4) — the removed line consumes
        // no new-side number.
        assert_eq!(h.new_lines, [1, 2, 3, 4].into_iter().collect::<BTreeSet<u32>>());
        assert_eq!(h.added_line_numbers, [2, 4].into_iter().collect::<BTreeSet<u32>>());
    }

    /// (#2310 fix-loop R4 — review regression) With `core.quotepath` (git's
    /// DEFAULT), a path with a non-ASCII byte, a control character, a `"`
    /// or a `\` is emitted C-quoted with octal byte escapes. Binding the
    /// path WITH its quotes and escapes means no finding ever matches it,
    /// and — since the file DID parse — the zero-files guard never fires
    /// either: a silently empty review.
    #[test]
    fn a_c_quoted_path_is_unquoted_and_octal_decoded() {
        let diff = ["--- \"a/src/caf\\303\\251.ts\"", "+++ \"b/src/caf\\303\\251.ts\"", "@@ -1,1 +1,1 @@", "-x", "+y", ""].join("\n");
        let files = parse_diff(&diff);
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].0, "src/caf\u{e9}.ts", "quotes stripped, \\303\\251 decoded to one UTF-8 char");
    }

    /// (#2310 fix-loop R4) The other C escapes git emits, and the `"`
    /// wrapper that is NOT a quoted path (a real file whose name starts
    /// with a quote is itself quoted, so a bare unterminated `"` is left
    /// alone rather than half-decoded).
    #[test]
    fn c_quoting_decodes_backslash_quote_and_tab_and_leaves_unquoted_paths_alone() {
        let cases = [
            ("+++ \"b/a\\\\b.ts\"", "a\\b.ts"),
            ("+++ \"b/a\\\"b.ts\"", "a\"b.ts"),
            ("+++ \"b/a\\tb.ts\"", "a\tb.ts"),
            ("+++ b/plain.ts", "plain.ts"),
        ];
        for (header, want) in cases {
            let diff = format!("{header}\n@@ -1,1 +1,1 @@\n+y\n");
            let files = parse_diff(&diff);
            assert_eq!(files.len(), 1, "{header}: {files:?}");
            assert_eq!(files[0].0, want, "{header}");
        }
    }

    /// (#2310 fix-loop round 2, item 2) A `"`-wrapped value that does NOT
    /// decode is REFUSED, not bound raw. Binding the undecoded string
    /// gives a path that can never match any file — the file is silently
    /// absent from every in-diff check while the run still looks healthy.
    /// `\377` is a lone 0xFF byte, which is not valid UTF-8; an
    /// unterminated quote is malformed the same way.
    #[test]
    fn a_quoted_path_that_does_not_decode_binds_nothing() {
        for header in ["+++ \"b/bad\\377name.ts\"", "+++ \"b/unterminated.ts"] {
            let diff = format!("{header}\n@@ -1,1 +1,1 @@\n+y\n");
            let files = parse_diff(&diff);
            assert!(files.is_empty(), "{header} must bind no file, got {files:?}");
        }
    }

    /// (#2310 fix-loop round 2, BLOCKER — proven) Header signal 3 (a lone
    /// `+++ ` whose next line is a hunk header) fired regardless of
    /// whether the OPEN hunk had finished. In a normal two-hunk file, an
    /// added line whose content is `++ x` sitting right before the second
    /// `@@` was read as a file header: the added line was swallowed AND
    /// the file's real second hunk bound to a phantom path `x`, so every
    /// finding in hunk 2 onward dropped out of the in-diff check
    /// silently. Signal 3 now fires only when the open hunk has consumed
    /// the line count its own `@@` header declared.
    #[test]
    fn signal_three_never_fires_while_the_open_hunk_is_still_short_of_its_declared_lines() {
        let diff = [
            "--- a/real.ts",
            "+++ b/real.ts",
            "@@ -1,3 +1,4 @@",
            " a",
            " b",
            "+++ x",
            "@@ -10,3 +10,3 @@",
            " p",
            "+q",
            " r",
            "",
        ]
        .join("\n");
        let files = parse_diff(&diff);
        assert_eq!(files.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), vec!["real.ts"], "no phantom file");
        assert_eq!(files[0].1.len(), 2, "both hunks belong to real.ts: {:?}", files[0].1);
        assert_eq!(files[0].1[0].added, vec!["++ x"], "the added line is content, not a header");
        assert_eq!(files[0].1[0].new_lines, [1, 2, 3].into_iter().collect::<BTreeSet<u32>>());
        assert_eq!(files[0].1[1].new_start, 10, "the second hunk keeps its own start");
        assert_eq!(files[0].1[1].added, vec!["q"]);
        assert_eq!(files[0].1[1].new_lines, [10, 11, 12].into_iter().collect::<BTreeSet<u32>>());
    }

    /// (#2310 fix-loop, review) No dialect emits `a/` on the `+++` side —
    /// stripping it there mangled a `--no-prefix` path under a top-level
    /// directory literally named `a`.
    #[test]
    fn an_a_slash_prefix_on_the_plus_side_is_a_real_directory_not_a_dialect() {
        let diff = ["--- a/mod.ts", "+++ a/mod.ts", "@@ -1,1 +1,1 @@", "+y", ""].join("\n");
        assert_eq!(parse_diff(&diff)[0].0, "a/mod.ts", "`a/` on the NEW side is a directory named `a`");
    }

    /// (#2310 fix-loop R1 — review regression) A delete-only patch binds
    /// `/dev/null` on the NEW side and therefore yields no files. That is
    /// CORRECT and must stay cheap to distinguish from an unreadable
    /// diff: the caller decides what to do about it (see `plan.rs`'s
    /// header-presence guard), the parser just reports the truth.
    #[test]
    fn a_delete_only_patch_yields_no_files_but_keeps_its_diff_git_header() {
        let diff = ["diff --git a/gone.ts b/gone.ts", "deleted file mode 100644", "index 1234567..0000000", "--- a/gone.ts", "+++ /dev/null", "@@ -1,2 +0,0 @@", "-one", "-two", ""].join("\n");
        assert!(parse_diff(&diff).is_empty(), "the NEW side is /dev/null");
    }

    /// (#2310 fix-loop B2, S3-3) A truly empty line inside a hunk body is
    /// a CONTEXT line whose content happens to be empty (tools and copy
    /// paths routinely strip the significant trailing space git emits).
    /// Dropping it shifted `new_ln` for every later line, which moved the
    /// old/new blocks and mis-classified findings as outside the diff.
    #[test]
    fn a_bare_blank_line_inside_a_hunk_is_context_like_a_space_prefixed_one() {
        let with_space = ["+++ b/x.ts", "@@ -1,4 +1,5 @@", " a", " ", " c", "+d", " e", ""].join("\n");
        let bare = ["+++ b/x.ts", "@@ -1,4 +1,5 @@", " a", "", " c", "+d", " e", ""].join("\n");
        let a = parse_diff(&with_space);
        let b = parse_diff(&bare);
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        let (ha, hb) = (&a[0].1[0], &b[0].1[0]);
        assert_eq!(hb.new_lines, ha.new_lines, "same line set as the space-prefixed form");
        assert_eq!(hb.new_lines, [1, 2, 3, 4, 5].into_iter().collect::<BTreeSet<u32>>());
        assert_eq!(hb.added_line_numbers, [4].into_iter().collect::<BTreeSet<u32>>(), "`+d` is line 4, not line 3");
        assert_eq!(hb.new_block, ha.new_block);
        assert_eq!(hb.old_block, ha.old_block);
        assert_eq!(hb.new_block, vec!["a", "", "c", "d", "e"]);
    }
}
