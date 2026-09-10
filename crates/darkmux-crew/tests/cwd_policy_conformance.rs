//! (#2577 review) Conformance: every PRODUCTION `impl StepKind for` block
//! in the workspace declares its OWN `cwd_policy()`.
//!
//! `step_kinds::registry`'s `with_builtins_has_exactly_one_kind_with_
//! ambient_cwd_fallback` test pins `CwdPolicy` for the five Tier 1
//! builtins by walking the REAL `StepKindRegistry` — but it can only see
//! what that registry holds. Ten Tier 2/3 kinds (`mods.gate`, the two
//! crawl planners, the two crawl unit kinds, the three `mission.*` kinds,
//! `deliver.github_review`, `records.gather`) register themselves through
//! their own missions in other crates, with no shared registry a walk can
//! reach. Before this module, those ten were covered only by a HAND-
//! MAINTAINED roster living in a doc comment — and an audit on review
//! (#2577) found that roster was already wrong: three of the ten kinds it
//! claimed carried explicit declarations carried none at all (silently
//! inheriting the trait default), and two more production kinds were
//! missing from the roster entirely (both benign, but unmentioned).
//!
//! **This repository already has the right shape for this class of
//! hazard**, and it is deliberately NOT a hand-maintained roster:
//! `darkmux-profiles::tests::pin_cwd_conformance` walks every `.rs` file
//! under three crates' `src/` trees for a literal spawn shape and asserts
//! the enclosing scope satisfies the pin — "a fifth spawn simply wouldn't
//! be added to" a roster, so the guard reads source instead. This module
//! follows the identical shape for the identical hazard class: it does
//! NOT enumerate step-kind ids anywhere. It walks every source tree in
//! this workspace whose crate depends on `darkmux-crew` (`StepKind`'s
//! defining, publicly-re-exported crate — see `sweep_roots()` for the
//! current list) for every `impl StepKind for <Type>` block OUTSIDE a
//! `#[cfg(test)]` module, and fails BY NAME on any such block with no
//! `fn cwd_policy(` inside it — so an eleventh Tier 2/3 kind arriving
//! tomorrow, in any swept tree, is caught here without anyone
//! remembering to add a row.
//!
//! **What this is: a lint, not a type check.** Like its sibling, it reads
//! text. It does not know whether a matched `fn cwd_policy(` line is a
//! real trait-method override or a red herring (a differently-scoped
//! function that merely shares the name inside the same braces) — no
//! production file does that today, and a reviewer reading a failure here
//! would notice the mismatch immediately, so that risk is asymmetric:
//! this can produce a false PASS in an adversarial rewrite of an
//! ALREADY-KNOWN kind, never a false silence about one. It is NOT
//! asymmetric the same way for a genuinely NEW kind, though: a renamed
//! trait import (`use darkmux_crew::step_kinds::StepKind as SK; impl SK
//! for Foo`) or a generic impl (`impl<T> StepKind for Wrapper<T>`) never
//! contains the literal substring `"impl StepKind for "` this scan
//! matches on, so either shape evades it entirely — a genuine false
//! silence about an unexamined kind, not merely an adversarial rewrite of
//! a known one. (#2612 review Also-fix 1 — corrects the claim two
//! sentences up, which is only true against a kind this scan has already
//! seen once.)
//!
//! **Scope, stated honestly.** This sweep does NOT see:
//!   - A `StepKind` impl added in a workspace crate that does not
//!     currently depend on `darkmux-crew` in its `Cargo.toml` — such a
//!     crate could not `impl` a trait it cannot name, so this is not a
//!     live gap today, only the boundary condition that would reopen one:
//!     if a currently-independent crate later adds that dependency,
//!     `sweep_roots()` below needs the same addition BEFORE that crate
//!     starts implementing `StepKind`, not after. (#2612 review MUST-FIX
//!     2 — this bullet previously called the omitted trees "a new crate
//!     this repo doesn't have yet", which was false: `darkmux-fleet`,
//!     `darkmux-serve`, and `darkmux-doctor` all depend on `darkmux-crew`
//!     today and were simply missing from the swept list, so a planted
//!     implementation in any of them was invisible to this sweep despite
//!     the crate already existing.)
//!   - The five Tier 1 builtins in `step_kinds/builtins.rs` (excluded by
//!     file path, see `is_tier1_builtins_file`) — covered by the
//!     registry-walk test instead, a STRONGER check (pins each by VALUE
//!     over the real registry, not merely "an override exists") where a
//!     real registry already exists. Two of those five correctly rely on
//!     the trait default; this scan would flag both as "missing" with no
//!     way to know that's fine, which is exactly why the stronger check
//!     owns that file instead.
//!   - A `#[cfg(test)]`-gated `impl StepKind for` (test-only stub kinds in
//!     `scheduler.rs`'s and `registry.rs`'s own test modules) — deliberate:
//!     those are synthetic kinds that never register into a mission and
//!     never spawn a real subprocess, so requiring them to carry a
//!     declaration would be pure ceremony. The exclusion is detected
//!     structurally (a `#[cfg(test)]` line immediately above a `mod ... {`
//!     line, tracked by brace depth), not by a file-name allowlist, so it
//!     also excludes a future `#[cfg(test)] mod` anywhere else in these
//!     trees.
//!
//! Red-proved: deleting `RecordsGatherStepKind`'s `cwd_policy()` override
//! (added by this same review) makes this fail naming that exact kind and
//! file; restoring it makes the suite green again.

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every source tree in this workspace whose crate depends on
/// `darkmux-crew` — `StepKind`'s defining, publicly re-exported crate
/// (`step_kinds::StepKind`, re-exported from `darkmux_crew::step_kinds`)
/// — and could therefore `impl StepKind for` a type of its own.
///
/// (#2612 review MUST-FIX 2) This used to name only THREE of these six
/// trees (`darkmux-crew/src`, `darkmux-lab/src`, the top-level `src/`)
/// and its own doc claimed a `StepKind` impl outside them lived in "a new
/// crate this repo doesn't have yet" — false: `darkmux-fleet`,
/// `darkmux-serve`, and `darkmux-doctor` all depend on `darkmux-crew`
/// TODAY (see each one's own `Cargo.toml`), so a planted implementation
/// in any of their `src/` trees was invisible to this sweep despite the
/// crate already existing and the trait already being reachable from it.
/// Extend here — nowhere else — the moment a workspace member's
/// `Cargo.toml` grows a `darkmux-crew` dependency it didn't have before;
/// see this test module's own "Scope, stated honestly" section for the
/// boundary this still doesn't cover (a FUTURE such crate, before this
/// list is updated for it).
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
/// vacuously (same discipline as `pin_cwd_conformance::rust_files`).
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
/// comment mentioning `impl StepKind for` or `fn cwd_policy(` in prose
/// (this module's own doc above does both) must not count as either; a
/// `//` line comment is blinded the same way `pin_cwd_conformance` blinds
/// one. Known limit, same as that sibling: a `//` inside a string literal
/// earlier on the line blinds the rest, which can only cause a MISSED
/// match (a loud failure), never a false pass.
fn code_only(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

/// One `impl StepKind for <Type>` block found outside a `#[cfg(test)]`
/// module, with whether `fn cwd_policy(` appears anywhere inside it.
struct ImplBlock {
    file: PathBuf,
    type_name: String,
    has_cwd_policy: bool,
}

/// Scans one file for every production `impl StepKind for` block,
/// tracking brace depth to (a) know where each impl block ends and (b)
/// skip anything inside a `#[cfg(test)]`-gated `mod`. Line-oriented and
/// deliberately simple: every file in these trees writes `impl StepKind
/// for <Type> {` and `#[cfg(test)]` / `mod tests {` as their own
/// consecutive lines (verified true of every file this sweep reaches as
/// of #2577) — a more general parser is `pin_cwd_conformance`'s job to
/// pioneer if a future shape needs it, not this module's to duplicate
/// speculatively.
fn scan_file(path: &Path) -> Vec<ImplBlock> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let mut out = Vec::new();
    let mut depth: i64 = 0;
    let mut test_mod_depths: Vec<i64> = Vec::new();
    let mut pending_cfg_test = false;
    // (start_depth, type_name, has_cwd_policy) for the impl block we're
    // currently inside, if any.
    let mut current_impl: Option<(i64, String, bool)> = None;

    for raw_line in text.lines() {
        let line = code_only(raw_line);
        let trimmed = line.trim();

        if trimmed == "#[cfg(test)]" {
            pending_cfg_test = true;
        }

        if test_mod_depths.is_empty() && current_impl.is_none() {
            if let Some(idx) = line.find("impl StepKind for ") {
                let rest = &line[idx + "impl StepKind for ".len()..];
                let type_name =
                    rest.split([' ', '{', '<']).next().unwrap_or("").trim().to_string();
                if !type_name.is_empty() {
                    current_impl = Some((depth, type_name, false));
                }
            }
        }

        if pending_cfg_test && trimmed.starts_with("mod ") && line.contains('{') {
            test_mod_depths.push(depth);
            pending_cfg_test = false;
        } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
            // A real line of code between `#[cfg(test)]` and the `mod`
            // it gates would be unusual, but don't let a stale flag leak
            // forward past one — only a chain of attribute lines (`#`)
            // and blanks is allowed to sit between them.
            pending_cfg_test = false;
        }

        if let Some((_, _, has)) = current_impl.as_mut() {
            if line.contains("fn cwd_policy(") {
                *has = true;
            }
        }

        for ch in line.chars() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if test_mod_depths.last() == Some(&depth) {
                        test_mod_depths.pop();
                    }
                    if let Some((start, _, _)) = current_impl {
                        if depth == start {
                            let (_, type_name, has_cwd_policy) = current_impl.take().unwrap();
                            out.push(ImplBlock { file: path.to_path_buf(), type_name, has_cwd_policy });
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// `step_kinds::builtins` holds the five Tier 1 builtins, already pinned
/// BY VALUE (not merely "has an override") by `step_kinds::registry`'s
/// `with_builtins_has_exactly_one_kind_with_ambient_cwd_fallback` — a
/// strictly stronger check than this module's "declares one at all", over
/// the real registry rather than a text scan. Two of those five
/// (`dispatch.internal`'s and `dispatch.map`'s kinds, for instance) rely
/// on the trait default deliberately and correctly; requiring THIS scan
/// to also see an override on every one of them would just be a second,
/// weaker copy of a check the registry walk already owns. Excluded here
/// by file path, not by type name, so a genuinely NEW Tier 1 kind added
/// to this same file is still caught — by the registry test, which is the
/// stronger one.
fn is_tier1_builtins_file(path: &Path) -> bool {
    path.ends_with("step_kinds/builtins.rs")
}

#[test]
fn every_production_step_kind_declares_its_own_cwd_policy() {
    let mut missing = Vec::new();
    let mut seen_any = false;
    for root in sweep_roots() {
        for file in rust_files(&root) {
            if is_tier1_builtins_file(&file) {
                continue;
            }
            for block in scan_file(&file) {
                seen_any = true;
                if !block.has_cwd_policy {
                    missing.push(format!("{} ({})", block.type_name, block.file.display()));
                }
            }
        }
    }
    assert!(seen_any, "swept zero `impl StepKind for` blocks — the sweep roots or the match pattern regressed");
    assert!(
        missing.is_empty(),
        "the following `impl StepKind for` block(s) declare no `cwd_policy()` override, so they \
         silently report the trait default `CwdPolicy::NoAmbientDependency` with no record that \
         anyone checked whether that's true — add an explicit `fn cwd_policy(&self) -> CwdPolicy \
         {{ ... }}` naming the audit (see any existing kind's own doc for the shape): {missing:?}",
    );
}
