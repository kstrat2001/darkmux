//! (#2614 review, Also-fix) Conformance: a production file that wires a
//! `resume_from`-shaped field to anything but the literal `None` also
//! contains a `StepKind` whose own `resume_precheck()` is overridden.
//!
//! `StepKind::resume_precheck` (see that method's own doc) defaults to
//! `Ok(())` — the correct behavior for every kind that never reads a
//! resume value, which today is every kind except `dispatch.internal`.
//! That default is what makes it possible to add a new kind tomorrow that
//! wires a resume-shaped value through its own config (a second
//! `DispatchOpts`-like struct, or `DispatchOpts` itself constructed with a
//! non-`None` `resume_from`) without EVER being told the scheduler-level
//! gate exists — the field would simply be threaded straight to a model
//! load, in whatever crate wrote it, with no error and no test failure,
//! because `resume_precheck`'s trait default reads as "no objection" to a
//! caller that never knew to ask.
//!
//! Today that risk is closed only by a grep: every `DispatchOpts`
//! construction outside `dispatch_opts_for` (the one `dispatch.internal`
//! uses, in `step_kinds::builtins`, whose owning kind DOES override
//! `resume_precheck`) hardcodes `resume_from: None`. Grepping is not a
//! guard — this module is one, following the identical shape this crate
//! already uses for the same class of hazard: `cwd_policy_conformance`'s
//! `every_production_step_kind_declares_its_own_cwd_policy` walks every
//! workspace crate that depends on `darkmux-crew` for a textual pattern
//! outside test modules and fails BY NAME on the ones missing an explicit
//! declaration. This module reuses that crate's `sweep_roots()` list (the
//! two crates a registry walk inside `darkmux-crew` structurally cannot
//! see — `darkmux-lab`'s crawl/review kinds and the top-level `src/`'s
//! coder-phase kinds — are exactly why a source sweep, not a registry
//! walk, is the right shape here too) and asks a narrower, CONDITIONAL
//! question: does this file set a `resume_from`-named struct field to
//! anything but `None`, and if so, does it (anywhere in the same file)
//! also declare `fn resume_precheck(`?
//!
//! **Why file-scoped, not impl-block-scoped (unlike `cwd_policy_
//! conformance`).** `dispatch.internal`'s own real `resume_from` wiring
//! does NOT live inside `impl StepKind for DispatchInternalStepKind`'s
//! braces — it lives in `dispatch_opts_for`, a free function `run()`
//! calls (extracted in #2480 specifically so the `Step`/`Task` ->
//! `DispatchOpts` reconstruction is unit-testable without Docker). A
//! block-scoped version of this scan — matching `cwd_policy_conformance`
//! line for line — would find ZERO matches in the one file that actually
//! needs to satisfy it, which is worse than not existing (a guard that
//! cannot see its own only current example teaches nothing about a new
//! one). Scoping to "the whole file, when the file also contains a
//! production `impl StepKind for` block" catches the real shape this
//! codebase actually uses (a StepKind's dispatch-options builder as a
//! sibling free function in the same file) at the cost of the coarser
//! blind spots named below.
//!
//! **What this is: a lint, not a parser (#2572's distinction).** Read
//! #2572 before trusting this section — it names the general failure
//! mode a source-text scan cannot close: an escape template someone
//! writes once and the scanner never learns to see. This scan matches a
//! struct-literal field OCCURRENCE ONLY when the character immediately
//! before the `resume_from` token (skipping whitespace) is `{` or `,` —
//! i.e. where a struct-literal field key belongs, right after the
//! opening brace or a previous field's trailing comma — and the
//! character immediately after is `:`, `,`, or `}`. That single rule is
//! what lets it tell a real field occurrence (`resume_from,` /
//! `resume_from: Some(x),`) apart from a bare variable use (`&resume_from`,
//! `resume_from.join(..)`, `Some(resume_from)` — preceded by `&`, `.`, or
//! `(`) and from a string literal naming the JSON config key
//! (`"resume_from"`, preceded by `"`) without parsing Rust at all.
//!
//! **Known blind spots, stated completely, not partially:**
//!   - **A function parameter can look identical to a struct field.** A
//!     parameter that is NOT the first in its list, on its own line
//!     immediately after a previous parameter's trailing comma (e.g. `fn
//!     f(a: T,\n resume_from: SomeType,\n ...)`), is textually
//!     indistinguishable from a struct-literal field at the same
//!     position and would be misflagged. This is a real false-positive
//!     risk this scan accepts, not a case it resolves — it does not
//!     recur today because every `resume_from` parameter in this
//!     workspace is either the FIRST parameter in its list (preceded by
//!     `(`, which this scan correctly excludes) or lives in a file with
//!     no `impl StepKind for` block at all (so a false flag there changes
//!     nothing — see the next bullet).
//!   - **Only files that already contain a production `impl StepKind
//!     for` block are checked at all.** A `resume_from` site flagged in a
//!     file with no `StepKind` impl (`dispatch.rs`'s `DispatchOpts`
//!     definition, `dispatch_internal.rs`'s free functions, the CLI's own
//!     `Cli`/`DispatchOpts` construction in `main.rs`) is silently
//!     skipped — there is no `StepKind` there for the requirement to
//!     attach to, and this scan is not the place to police those.
//!   - **File-scoped, not per-impl-block-scoped**, unlike its
//!     `cwd_policy` sibling. A file holding TWO `impl StepKind for`
//!     blocks where only one of them wires a real resume value, and only
//!     the OTHER one declares `resume_precheck`, satisfies this scan even
//!     though the wiring kind itself has no override. No file in any
//!     swept root does this today (every `StepKind`-implementing file
//!     that wires `resume_from` non-`None` has exactly one `impl StepKind
//!     for` block), so this is a named boundary condition, not a live
//!     gap.
//!   - **Presence, not correctness.** Like `cwd_policy_conformance`, this
//!     only checks that `fn resume_precheck(` appears as text somewhere
//!     in the file; it cannot tell a real, careful override from one that
//!     compiles, matches the trait signature, and unconditionally returns
//!     `Ok(())` — a no-op wearing the guard's clothes.
//!   - **A renamed trait import or a generic impl** (`impl<T> StepKind
//!     for Wrapper<T>`) evades the `impl StepKind for` detection entirely
//!     — the identical blind spot `cwd_policy_conformance` names for
//!     itself, inherited here because this module reuses that scan's
//!     `impl` detection.
//!   - **`code_only` blinds `//` line comments, not `/* */` block
//!     comments.** A `fn resume_precheck(` (or a `resume_from` field)
//!     sitting inside a `/* ... */` block still reads as real code to
//!     this scan — found empirically while red-proving this module: an
//!     early mutation attempt commented the whole override out with `/*
//!     */` and the suite stayed green, because the text `fn
//!     resume_precheck(` was still there for the scanner to see. The
//!     red-prove that ships with this module deletes the override
//!     outright rather than block-commenting it, for exactly this reason.
//!
//! Red-proved: commenting out `DispatchInternalStepKind`'s
//! `resume_precheck` override (with the free-function `resume_from,`
//! wiring left in place in `step_kinds/builtins.rs`) makes this fail,
//! naming that file and the flagged line; restoring the override makes
//! the suite green again.

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Identical list to `cwd_policy_conformance::sweep_roots` — every source
/// tree in this workspace whose crate depends on `darkmux-crew` and could
/// therefore `impl StepKind for` a type of its own. Duplicated rather than
/// shared: these are separate integration-test binaries, each compiled on
/// its own, and `cwd_policy_conformance`'s own module doc explains why
/// this list lives in exactly this shape (extend HERE the moment a
/// workspace member's `Cargo.toml` grows a `darkmux-crew` dependency it
/// didn't have before — see that sibling module for the same warning).
fn sweep_roots() -> Vec<PathBuf> {
    vec![
        manifest_dir().join("src"),
        manifest_dir().join("../darkmux-lab/src"),
        manifest_dir().join("../darkmux-fleet/src"),
        manifest_dir().join("../darkmux-serve/src"),
        manifest_dir().join("../darkmux-doctor/src"),
        manifest_dir().join("../../src"),
    ]
}

/// Every `.rs` file under `root`, recursively. Panics on a missing root —
/// a typo'd sibling path must not silently sweep zero files and pass
/// vacuously.
fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    assert!(root.is_dir(), "conformance sweep root does not exist: {}", root.display());
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.unwrap_or_else(|e| panic!("reading an entry in {}: {e}", dir.display())).path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    out
}

/// The code half of a line — everything before its first `//`. A doc
/// comment mentioning `resume_from:` or `impl StepKind for` in prose
/// (this module's own doc above does both) must not count as a match.
/// Known limit, shared with `cwd_policy_conformance`: a `//` inside a
/// string literal earlier on the line blinds the rest, which can only
/// cause a MISSED match (a loud failure), never a false pass.
fn code_only(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Detects a struct-literal `resume_from` field being set to anything but
/// the literal `None`, on `line` (already comment-stripped). `prev_char`
/// is the last non-whitespace character seen so far in the FILE, carried
/// in from the previous line(s) — a struct literal's fields are almost
/// always one per line, so the "preceded by `{`/`,`" test in the doc above
/// has to look across the line boundary at the previous field's trailing
/// comma, not just within the current line. Returns `(match, new_prev_char)`
/// — the caller threads `new_prev_char` into the next call.
fn resume_non_none_site(line: &str, prev_char_in: Option<char>) -> (Option<String>, Option<char>) {
    let trimmed = line.trim();
    let last_char_of_line = trimmed.chars().last();
    let new_prev_char = last_char_of_line.or(prev_char_in);

    if trimmed.starts_with("let ") {
        // A local binding (`let resume_from = ...;`) names a variable,
        // not a struct field — the eventual struct-literal USE of that
        // variable (shorthand or otherwise) is what this scan looks for
        // instead.
        return (None, new_prev_char);
    }

    let bytes = line.as_bytes();
    let mut search_from = 0usize;
    let mut prev_char = prev_char_in;
    while let Some(rel) = line[search_from..].find("resume_from") {
        let start = search_from + rel;
        let end = start + "resume_from".len();

        // The char immediately before this occurrence: from within the
        // current line if there's anything non-whitespace before `start`
        // on this line, else carried in from the previous line(s).
        let within_line_prev = line[..start].trim_end().chars().last();
        let effective_prev_char = within_line_prev.or(prev_char);

        // Whole-word match only — `resume_from_x` must not count as
        // `resume_from`.
        let prev_is_word =
            start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
        let next_is_word =
            end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_');
        if prev_is_word || next_is_word {
            search_from = end;
            continue;
        }

        if effective_prev_char == Some('"') {
            // A string literal naming the JSON config key
            // (`step.config.get("resume_from")`), not a struct field.
            search_from = end;
            continue;
        }

        let is_field_position = matches!(effective_prev_char, Some('{') | Some(','));
        if is_field_position {
            let rest = &line[end..];
            let next_char = rest.trim_start().chars().next();
            match next_char {
                Some(':') => {
                    let after_colon = rest.trim_start()[1..].trim_start();
                    let value_end = after_colon.find([',', '}']).unwrap_or(after_colon.len());
                    let value = after_colon[..value_end].trim();
                    if value != "None" {
                        return (
                            Some(format!("`resume_from: {value}` (not `None`)")),
                            new_prev_char,
                        );
                    }
                }
                Some(',') | Some('}') => {
                    return (
                        Some(
                            "`resume_from` (struct-literal shorthand field, non-literal value)"
                                .to_string(),
                        ),
                        new_prev_char,
                    );
                }
                _ => {}
            }
        }
        // Advance the running prev_char to whatever's immediately before
        // the NEXT search position, so a second `resume_from` later on
        // the same line (unlikely, but not this scan's job to assume
        // away) still resolves correctly.
        prev_char = Some('_'); // the last char of "resume_from" itself
        search_from = end;
    }
    (None, new_prev_char)
}

/// One pass over `path`: whether it holds a production (non-test) `impl
/// StepKind for` block, whether that file also declares `fn
/// resume_precheck(` anywhere outside a test module, and every flagged
/// `resume_from`-non-`None` site found outside a test module. Reuses the
/// exact brace-depth / `#[cfg(test)]`-exclusion mechanism `cwd_policy_
/// conformance::scan_file` uses, applied file-wide rather than per-impl-
/// block (see this module's own doc for why).
fn scan_file(path: &Path) -> (bool, bool, Vec<(usize, String)>) {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let mut depth: i64 = 0;
    let mut test_mod_depths: Vec<i64> = Vec::new();
    let mut pending_cfg_test = false;
    let mut has_impl = false;
    let mut has_precheck = false;
    let mut flagged = Vec::new();
    let mut prev_char: Option<char> = None;

    for (idx, raw_line) in text.lines().enumerate() {
        let line = code_only(raw_line);
        let trimmed = line.trim();

        if trimmed == "#[cfg(test)]" {
            pending_cfg_test = true;
        }

        // Threaded across every line (test or not) so the "preceded by a
        // struct field's trailing comma" check stays correct regardless
        // of whether the CURRENT line's match is one this scan acts on —
        // only whether we RECORD a match is gated on `test_mod_depths`.
        let (site, new_prev_char) = resume_non_none_site(line, prev_char);
        prev_char = new_prev_char;

        if test_mod_depths.is_empty() {
            if line.contains("impl StepKind for ") {
                has_impl = true;
            }
            if line.contains("fn resume_precheck(") {
                has_precheck = true;
            }
            if let Some(site) = site {
                flagged.push((idx + 1, site));
            }
        }

        if pending_cfg_test && trimmed.starts_with("mod ") && line.contains('{') {
            test_mod_depths.push(depth);
            pending_cfg_test = false;
        } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
            pending_cfg_test = false;
        }

        for ch in line.chars() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if test_mod_depths.last() == Some(&depth) {
                        test_mod_depths.pop();
                    }
                }
                _ => {}
            }
        }
    }
    (has_impl, has_precheck, flagged)
}

#[test]
fn every_resume_wiring_file_declares_resume_precheck() {
    let mut missing = Vec::new();
    let mut seen_any_wiring = false;
    for root in sweep_roots() {
        for file in rust_files(&root) {
            let (has_impl, has_precheck, flagged) = scan_file(&file);
            if flagged.is_empty() {
                continue;
            }
            seen_any_wiring = true;
            if !has_impl {
                // Out of scope by design — see this module's "Known blind
                // spots" section. There is no `StepKind` in this file for
                // the requirement to attach to.
                continue;
            }
            if !has_precheck {
                for (line_no, site) in &flagged {
                    missing.push(format!("{}:{line_no}: {site}", file.display()));
                }
            }
        }
    }
    assert!(
        seen_any_wiring,
        "swept zero `resume_from`-non-`None` sites — the sweep roots or the match pattern \
         regressed (dispatch.internal's own wiring in step_kinds/builtins.rs should always match)"
    );
    assert!(
        missing.is_empty(),
        "the following file(s) set a `resume_from`-shaped field to something other than `None` \
         but declare no `fn resume_precheck(` override anywhere in the same file, so a \
         `--resume-from`-shaped value wired through this kind's config would reach model \
         selection with no scheduler-level checkpoint gate ever asked (see `StepKind::\
         resume_precheck`'s own doc for the hazard this closes): {missing:?}",
    );
}
