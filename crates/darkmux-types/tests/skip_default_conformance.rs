//! (#2578) Conformance: every non-`Option` `skip_serializing_if` field in
//! the swept crates carries `#[serde(default)]`.
//!
//! ## The class
//!
//! Any field pairing `skip_serializing_if` with `default` has this shape:
//! the serializer guarantees the key is USUALLY ABSENT from real output
//! (that is the whole point of `skip_serializing_if`), so the
//! deserializer's tolerance of that absence — which `default` alone
//! provides — is load-bearing and INVISIBLE to any round trip that starts
//! in memory with the field already populated. #2540 found the first
//! instance (`Plan::root_override`, a hand-built type-in-memory round trip
//! never exercised the omission a real on-disk `plan.json` always takes);
//! #2578 (this fix) found four more, unswept because #2540's own fix
//! covered only two of the five crates that carry the pattern.
//!
//! ## What this is: a lint, not an enumeration (#2572's distinction)
//!
//! This walks every `.rs` file under every workspace crate's `src/` tree
//! (dynamically discovered from `crates/`, plus the top-level CLI `src/`)
//! and finds every `#[serde(...)]` attribute containing
//! `skip_serializing_if`. For each, it asserts the field is SAFE by one of
//! the three routes the issue names:
//!
//!   1. the SAME attribute (or a directly adjacent sibling attribute on
//!      the same field) also carries `default`;
//!   2. the field's type is `Option<...>` and the attribute carries no
//!      `deserialize_with` (serde implicitly defaults a missing `Option`
//!      even with no `#[serde(default)]` at all — `deserialize_with`
//!      disables that implicit default and makes the field required
//!      again, so this exemption does NOT apply once one is present);
//!   3. the enclosing struct/enum is proven, by reading its OWN nearest
//!      `#[derive(...)]` attribute, to NOT derive `Deserialize` — a
//!      write-only type has no read path for this bug to live on.
//!
//! A field satisfying none of the three is a FINDING: it carries
//! `skip_serializing_if`, has no `default`, is not a safe bare `Option`,
//! and lives on a type that CAN be deserialized — the exact shape #2540
//! and #2578 both found by hand.
//!
//! **This is deliberately NOT the enumeration #2572 asks for** (that
//! requires reflection over `#[serde(...)]` attributes, which stable Rust
//! doesn't offer, or a newtype-style unrepresentable-by-construction
//! redesign of the derive macros themselves — out of scope here). It is a
//! SOURCE-TEXT SCAN: it reads lines, not tokens or types, and it is
//! best-effort in the same sense `pin_cwd_conformance.rs`
//! (`crates/darkmux-profiles/tests/pin_cwd_conformance.rs`, #2534) is —
//! that module's doc is the prior art this one follows for shape: name
//! the class, scan for it tree-wide, and state the scan's own honest
//! limits rather than imply more certainty than a text read can support.
//!
//! ## Known limits (stated so a green run isn't read as more than it is)
//!
//! - **Reflection-free.** There is no `#[serde(...)]` introspection in
//!   stable Rust. This scan approximates it with string matching on
//!   attribute text; a sufficiently adversarial rewrite (renaming an
//!   attribute value to literally contain the substring `"default"`, or a
//!   macro that GENERATES a `#[derive(Deserialize)]` invisibly) defeats
//!   it. No such macro exists in this tree today (verified: no
//!   `derive_deserialize`-shaped proc macro is defined or used here).
//! - **Split field attributes are not merged.** `#[serde(default)]` and
//!   `#[serde(skip_serializing_if = "...")]` as TWO separate attribute
//!   lines for the same field (rather than one combined attribute) are
//!   merged with any DIRECTLY ADJACENT sibling attribute block (no
//!   blank/code line between), which covers the pattern if it ever
//!   appears — but a `default` attribute separated from its field by even
//!   one intervening non-attribute line would not be found. No real site
//!   in the swept crates uses split attributes today (verified by grep);
//!   the direction of the miss is a FALSE FINDING (a real default read as
//!   missing), which fails loud rather than silently passing.
//! - **Container-level `#[serde(default)]`** (a single attribute on the
//!   struct itself, applying to every field with no per-field
//!   `#[serde(default)]` needed) is not recognized as satisfying the
//!   per-field check — verified no site in the swept crates uses this
//!   form (`grep -rn '^#\[serde(default)\]$'` across every crate's `src/`
//!   returns nothing). Same failure direction as above: a false finding,
//!   not a silent miss.
//! - **A manually-written `impl Deserialize` with no `#[derive(...)]` at
//!   all** is NOT recognized by exemption route 3 (which reads a
//!   `#[derive(...)]` attribute, not the trait impl itself) — such a type
//!   is treated as NOT proven safe and gets flagged for human review if it
//!   also carries an unguarded `skip_serializing_if`. Again the safe
//!   direction: a possible false finding, never a silent miss. No real
//!   site in the swept crates hand-implements `Deserialize` today
//!   (verified by grep for `impl.*Deserialize.*for`).
//! - **Multi-line `#[derive(...)]` attributes** (the derive's own
//!   argument list split across physical lines) are not specially
//!   handled by the backward container-derive scan — verified none exist
//!   in the swept crates (`grep -rn '#\[derive($'` returns nothing
//!   anywhere in `crates/*/src` or `src/`). A `#[cfg_attr(feature =
//!   "ts-export", derive(ts_rs::TS))]` wrapper trailing the real derive
//!   line (the actual, common shape in this tree — see
//!   `step_output.rs:64-65`) IS handled correctly: the backward scan
//!   walks past EVERY preceding attribute line rather than stopping at
//!   the first one containing the substring `"derive("`, so the
//!   `ts_rs::TS`-only wrapper doesn't mask the real serde derive above it.
//!   `the_ts_export_cfg_attr_wrapper_does_not_mask_the_real_derive` below
//!   pins this directly — it is the one shape that broke an earlier draft
//!   of this scan.
//! - **`runtime/` is deliberately NOT swept.** It is not a Cargo workspace
//!   member (its own `Cargo.toml`, built into the `darkmux-runtime` Docker
//!   image, needs its own `cargo clippy --manifest-path
//!   runtime/Cargo.toml` per this repo's own convention) and this scan's
//!   crate discovery walks `crates/` only. Checked by hand instead
//!   (2026-09-10, this packet): `grep -rn skip_serializing_if runtime/src`
//!   returns 26 lines. Of those, 25 are `Option::is_none` on `Option<T>`
//!   fields with no `deserialize_with` anywhere near them (exemption route
//!   2, safe) and the remaining one — `ChatRequest::tools`
//!   (`runtime/src/lmstudio.rs:208`, `Vec::is_empty`) — sits on a struct
//!   deriving `Serialize` only, no `Deserialize`
//!   (`runtime/src/lmstudio.rs:203`, exemption route 3). Zero risky
//!   pairings as of this commit; re-verify by hand if `runtime/`'s wire
//!   schemas grow a new non-`Option` `skip_serializing_if` field.
//!
//! ## What the four named sites turned out to be
//!
//! #2578 names four fields as unpinned. Three are real, currently-correct
//! sites this scan protects going forward:
//!
//!   - `Tier2Config::slot_caps` (`darkmux-types/src/lib.rs`) — nests under
//!     the OPERATOR-AUTHORED profiles registry; see the round-trip test in
//!     `lib.rs` itself for the direct backstop.
//!   - `BundleSelector::fact_families` (`darkmux-types/src/lib.rs`) —
//!     persisted on every review run record; same file, same backstop.
//!   - `IntegrityReport::legacy_format` (`darkmux-flow/src/integrity.rs`)
//!     — written by `flow integrity-check --json`; round-trip backstop in
//!     that file.
//!
//! The fourth, `GraphNode::steps` (`darkmux-serve/src/mission_graph.rs`),
//! is NOT one of these three — verified, not assumed (see "quoting vs
//! verifying a premise" doctrine): `GraphNode` derives `#[derive(Debug,
//! Clone, Serialize, PartialEq)]` — no `Deserialize` — and neither it nor
//! its sibling `StepRow`/`MissionGraph` (same file, same derive shape) is
//! ever deserialized anywhere in this tree (`grep -rn 'GraphNode\|StepRow'
//! crates/darkmux-serve/src` finds only construction sites and the
//! Serialize-only golden-fixture test in `wire_fixtures.rs`). This is the
//! SAME shape as the issue's own "checked and clean" pair
//! (`ChatRequest::tools`, `PhaseReviewOutput::findings`) — a
//! `skip_serializing_if` field on a type that derives `Serialize` only,
//! exemption route 3, not reachable as a read failure. The issue's own
//! table lists it as unpinned; this scan (and the manual check above)
//! finds it correctly EXEMPT under the issue's own stated fix direction.
//! No source or test change was made for this field — there is nothing to
//! protect that isn't already true by construction, and adding a
//! round-trip test for a type with no `Deserialize` impl would not
//! compile. Recorded here so the next sweep doesn't re-derive it, the same
//! way the issue itself recorded `ChatRequest::tools` /
//! `PhaseReviewOutput::findings`.

use std::path::{Path, PathBuf};

/// The CODE half of a line — everything before its first `//`. A line
/// starting with `///` (a doc comment) or a bare `//` comment therefore
/// code-only's to an EMPTY string, which is exactly what lets the
/// backward derive-scan and the forward field-scan treat doc comments the
/// same as blank lines without any special-casing.
fn code_only(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every crate's `src/` under `crates/`, discovered by READING the
/// directory rather than a hand-maintained list — a new crate is swept
/// the moment it exists, no test edit needed. Plus the top-level CLI
/// `src/` tree (a sibling of `crates/`, not inside it). Deliberately does
/// NOT include `runtime/` — see the module doc's "Known limits" section
/// for why, and the manual verification that stands in for it.
fn sweep_roots() -> Vec<PathBuf> {
    let crates_dir = manifest_dir()
        .parent()
        .expect("darkmux-types manifest dir has a parent (crates/)")
        .to_path_buf();
    assert_eq!(
        crates_dir.file_name().and_then(|n| n.to_str()),
        Some("crates"),
        "expected the parent of darkmux-types's manifest dir to be named `crates`, got {}",
        crates_dir.display()
    );

    let mut roots = Vec::new();
    let entries = std::fs::read_dir(&crates_dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", crates_dir.display()));
    let mut crate_count = 0usize;
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("reading an entry in {}: {e}", crates_dir.display()))
            .path();
        if path.is_dir() {
            let src = path.join("src");
            if src.is_dir() {
                roots.push(src);
                crate_count += 1;
            }
        }
    }
    assert!(
        crate_count >= 10,
        "crate discovery under {} found only {crate_count} crates with a src/ tree — expected \
         at least 10 (one per current workspace member). Either the workspace shrank a lot, or \
         this discovery is broken and every assertion below is sweeping far less than it claims.",
        crates_dir.display()
    );

    let top_level_src = crates_dir
        .parent()
        .expect("crates/ has a parent (the repo root)")
        .join("src");
    assert!(
        top_level_src.is_dir(),
        "expected {} to exist (the top-level CLI src/ tree)",
        top_level_src.display()
    );
    roots.push(top_level_src);

    roots
}

/// Every `.rs` file under `root`, recursively. Panics if `root` doesn't
/// exist — a typo'd or moved sweep root must not silently sweep zero
/// files and pass vacuously.
fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    assert!(root.is_dir(), "conformance sweep root does not exist: {}", root.display());
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
        for entry in entries {
            let path = entry
                .unwrap_or_else(|e| panic!("reading an entry in {}: {e}", dir.display()))
                .path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    out
}

/// Given `lines[start]` beginning an attribute (trimmed code starts with
/// `#[`), returns the joined, comment-stripped text of the WHOLE
/// attribute — spanning multiple physical lines when the attribute
/// itself does (e.g. `#[serde(\n    default,\n    skip_serializing_if =
/// "...",\n)]`, the real shape in `darkmux-crew/src/types.rs:303` and
/// `darkmux-lab/src/workloads/types.rs`) — plus the 0-based index of its
/// LAST line. Bounded on `[`/`]` depth, not on "the next blank line",
/// same reasoning as `pin_cwd_conformance.rs`'s `same_function_window`:
/// depth needs no format-specific knowledge to know when an attribute
/// closes.
fn attr_block(lines: &[&str], start: usize) -> (String, usize) {
    let mut depth: i32 = 0;
    let mut entered = false;
    let mut out = String::new();
    let mut end = start;
    for (offset, line) in lines[start..].iter().enumerate() {
        let code = code_only(line);
        for ch in code.chars() {
            match ch {
                '[' => {
                    depth += 1;
                    entered = true;
                }
                ']' => depth -= 1,
                _ => {}
            }
        }
        out.push_str(code);
        out.push('\n');
        end = start + offset;
        if entered && depth <= 0 {
            break;
        }
    }
    (out, end)
}

/// Every attribute block in `lines`, in file order, as `(start, end,
/// text)`.
fn all_attr_blocks(lines: &[&str]) -> Vec<(usize, usize, String)> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if code_only(lines[i]).trim_start().starts_with("#[") {
            let (text, end) = attr_block(lines, i);
            blocks.push((i, end, text));
            i = end + 1;
        } else {
            i += 1;
        }
    }
    blocks
}

/// True when `line`'s code (comments stripped) contains `struct` or
/// `enum` as a whole word — tokenized by splitting on non-identifier
/// characters, so `restructure` or a string literal containing `struct`
/// mid-word never matches, without needing a regex dependency (this repo
/// keeps its dep set deliberately small).
fn is_type_decl_line(line: &str) -> bool {
    code_only(line)
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|tok| tok == "struct" || tok == "enum")
}

/// Walking backward from `field_idx`, the index of the nearest preceding
/// line declaring a `struct` or `enum` — the field's container. In real
/// compiling Rust a struct/enum BODY cannot itself contain another
/// struct/enum DEFINITION (only field lists, which may reference nested
/// types by name but never define them inline), so the nearest preceding
/// declaration line, found by simple backward text search with no brace-
/// depth bookkeeping at all, is always the correct container — including
/// for a field inside an enum's struct-like variant (`Variant { field:
/// T }`), since an anonymous variant body never itself contains a
/// `struct`/`enum` keyword to be mistaken for a nested definition. `None`
/// only when no such line exists above `field_idx` in the file at all
/// (should not happen for a real field-position attribute).
fn nearest_container_decl(lines: &[&str], field_idx: usize) -> Option<usize> {
    let mut i = field_idx;
    while i > 0 {
        i -= 1;
        if is_type_decl_line(lines[i]) {
            return Some(i);
        }
    }
    None
}

/// `(found_a_derive_attribute, that_attribute_names_deserialize)` for the
/// struct/enum declared at `decl_idx`. Walks EVERY contiguous attribute
/// line directly above `decl_idx` (blank lines and doc comments — both
/// blank after `code_only` — don't break the walk; any other real code
/// does), rather than stopping at the FIRST line containing the substring
/// `"derive("`. That distinction is load-bearing: this tree's real
/// pattern for a `ts-export`-gated type is the REAL derive line first,
/// then `#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]` trailing
/// it, closer to the struct (`darkmux-crew/src/step_output.rs:64-65`). A
/// scan that stops at the first `"derive("` match hits the `ts_rs::TS`-
/// only wrapper, reads no `Deserialize` in it, and wrongly grants the
/// Serialize-only exemption to a type that plainly does derive
/// `Deserialize` two lines further back — the false-negative direction,
/// which is the one direction this scan cannot allow itself (see the
/// module doc's known limits: every OTHER approximation in this file
/// fails toward a false FINDING, never a silent miss; this is the one
/// place that took a second pass to get onto the same side).
fn container_derives(lines: &[&str], decl_idx: usize) -> (bool, bool) {
    let mut i = decl_idx;
    let mut found = false;
    let mut has_deserialize = false;
    while i > 0 {
        i -= 1;
        let trimmed = code_only(lines[i]).trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("#[") {
            if trimmed.contains("derive(") {
                found = true;
                if trimmed.contains("Deserialize") {
                    has_deserialize = true;
                }
            }
            continue;
        }
        break;
    }
    (found, has_deserialize)
}

struct Finding {
    file: PathBuf,
    line: usize, // 1-based, the FIELD's own line
    field_text: String,
}

/// Scan one file's lines for `skip_serializing_if` fields with no safe
/// exemption. `lines` rather than a path so the same function backs both
/// the real tree-wide sweep and the in-memory planted-violation self-test
/// below.
fn scan_lines(lines: &[&str]) -> Vec<(usize, String)> {
    let mut findings = Vec::new();
    for (start, end, text) in all_attr_blocks(lines) {
        if !text.contains("skip_serializing_if") {
            continue;
        }

        // The field this attribute (or block of attributes) belongs to:
        // the next non-blank, non-doc-comment, non-attribute line after
        // this block — skipping any DIRECTLY ADJACENT sibling attribute
        // blocks (e.g. a `#[cfg_attr(...)]` on the same field) along the
        // way, and folding their text into the default/deserialize_with
        // check too (see the module doc's "split field attributes" limit).
        let mut combined = text.clone();
        let mut j = end + 1;
        loop {
            if j >= lines.len() {
                break;
            }
            let trimmed = code_only(lines[j]).trim();
            if trimmed.is_empty() {
                j += 1;
                continue;
            }
            if trimmed.starts_with("#[") {
                let (sib_text, sib_end) = attr_block(lines, j);
                combined.push_str(&sib_text);
                j = sib_end + 1;
                continue;
            }
            break;
        }
        if j >= lines.len() {
            continue; // malformed / attribute at EOF with no field after it — nothing to check
        }

        let field_line = code_only(lines[j]);
        let Some(colon) = field_line.find(':') else {
            continue; // not actually a field declaration — this attribute wasn't on a field
        };
        let field_type = field_line[colon + 1..].trim().trim_end_matches(',').to_string();

        let has_default = combined.contains("default");
        if has_default {
            continue; // exemption route 1
        }

        let has_deserialize_with = combined.contains("deserialize_with");
        let is_option = field_type.starts_with("Option<") || field_type.starts_with("Option <");
        if is_option && !has_deserialize_with {
            continue; // exemption route 2
        }

        let exempt_serialize_only = match nearest_container_decl(lines, start) {
            Some(decl_idx) => {
                let (found, has_deserialize) = container_derives(lines, decl_idx);
                found && !has_deserialize
            }
            None => false, // no container found — can't prove the exemption, so don't grant it
        };
        if exempt_serialize_only {
            continue; // exemption route 3
        }

        findings.push((j + 1, lines[j].trim().to_string()));
    }
    findings
}

/// The main assertion: nothing in the swept crates carries an unguarded
/// `skip_serializing_if`.
#[test]
fn every_skip_serializing_if_field_is_default_option_or_write_only() {
    let mut findings: Vec<Finding> = Vec::new();
    for root in sweep_roots() {
        for file in rust_files(&root) {
            let src = std::fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
            let lines: Vec<&str> = src.lines().collect();
            for (line, field_text) in scan_lines(&lines) {
                findings.push(Finding { file: file.clone(), line, field_text });
            }
        }
    }

    assert!(
        findings.is_empty(),
        "found {} `skip_serializing_if` field(s) with no `default`, not a bare `Option`, and \
         on a type that DOES derive `Deserialize` (#2578 — the exact shape that stopped a real \
         on-disk document from deserializing while every in-memory-only test stayed green):\n{}\n\n\
         Fix: add `default` to the field's `#[serde(...)]` attribute (alongside \
         `skip_serializing_if`) — or, if the omission is genuinely intentional (a newly-required \
         field), confirm every writer of this type's JSON always includes it and document why.",
        findings.len(),
        findings
            .iter()
            .map(|f| format!("  {}:{}: {}", f.file.display(), f.line, f.field_text))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

/// Negative control: a scanner whose sweep roots resolved wrong, or whose
/// pattern match silently stopped matching this codebase's attribute
/// style, would report the assertion above as vacuously green — worse
/// than no test at all. Assert the scan actually SEES a realistic number
/// of `skip_serializing_if` occurrences (the raw grep behind this fix
/// found 300+ across the swept crates' `src/` trees; this thresholds well
/// under that so a reasonable amount of future code churn doesn't make
/// this test itself the thing that needs constant tending).
#[test]
fn the_scan_actually_finds_a_realistic_number_of_skip_fields() {
    let mut total = 0usize;
    for root in sweep_roots() {
        for file in rust_files(&root) {
            let src = std::fs::read_to_string(&file).unwrap();
            let lines: Vec<&str> = src.lines().collect();
            total += all_attr_blocks(&lines).iter().filter(|(_, _, t)| t.contains("skip_serializing_if")).count();
        }
    }
    assert!(
        total >= 200,
        "the conformance scan found only {total} `skip_serializing_if` occurrence(s) across the \
         swept crates — expected at least 200. Either the sweep roots are wrong, the attribute- \
         block scan no longer matches this codebase's style, or a huge amount of code was \
         deleted — in any case, `every_skip_serializing_if_field_is_default_option_or_write_only` \
         above is not actually testing what it claims to."
    );
}

/// Self-test: plant a violating field in a synthetic in-memory source and
/// confirm `scan_lines` catches it. This is the walker's own red-prove,
/// permanent rather than a one-off manual mutation — a future edit to the
/// scan logic that breaks detection fails HERE, not silently on the next
/// real regression.
#[test]
fn a_planted_violation_is_caught() {
    let src = r#"
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let findings = scan_lines(&lines);
    assert_eq!(
        findings.len(),
        1,
        "planting `Scratch::items` — skip_serializing_if, no default, non-Option, on a type \
         that derives Deserialize — must be caught exactly once; the scan found {findings:?} \
         instead"
    );
    assert!(findings[0].1.contains("items"), "the finding should point at the `items` field, got: {findings:?}");
}

/// Twin of the above: the SAME planted field, with `default` restored,
/// must NOT be flagged — proves the assertion isn't just always red.
#[test]
fn the_same_field_with_default_restored_is_not_flagged() {
    let src = r#"
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    assert!(
        scan_lines(&lines).is_empty(),
        "restoring `default` on the planted field must clear the finding — the scan is not \
         actually keying off `default`'s presence"
    );
}

/// Exemption route 2: a bare `Option` field needs no `default` at all.
#[test]
fn a_bare_option_field_is_not_flagged() {
    let src = r#"
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maybe: Option<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    assert!(scan_lines(&lines).is_empty(), "a bare Option field with no deserialize_with must be exempt (route 2)");
}

/// Route 2 does NOT apply once `deserialize_with` is present — it
/// disables serde's implicit Option default and makes the field required
/// again on read, so the field needs an explicit `default` just like any
/// non-Option field.
#[test]
fn an_option_field_with_deserialize_with_is_flagged() {
    let src = r#"
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "parse_it")]
    pub maybe: Option<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    assert_eq!(
        scan_lines(&lines).len(),
        1,
        "an Option field with deserialize_with must NOT get the bare-Option exemption — \
         deserialize_with disables serde's implicit default, so omitting the field on read \
         becomes a real failure again"
    );
}

/// Exemption route 3: a `Serialize`-only type (no `Deserialize` derived)
/// has no read path for this bug to live on — the exact shape of the
/// real `GraphNode::steps` finding this module's doc discusses at length.
#[test]
fn a_serialize_only_type_is_not_flagged() {
    let src = r#"
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Scratch {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    assert!(
        scan_lines(&lines).is_empty(),
        "a Serialize-only struct (no Deserialize derived) must be exempt (route 3) regardless \
         of whether `default` is present — there's no reader for the omission to break"
    );

    // And WITHOUT `default` present at all — still exempt, same reason.
    let src_no_default = r#"
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines2: Vec<&str> = src_no_default.lines().collect();
    assert!(
        scan_lines(&lines2).is_empty(),
        "a Serialize-only struct's skip_serializing_if field must be exempt even with no \
         default at all — the type can never be read back, so there's nothing to protect"
    );
}

/// The bug this scan's own development hit: a `#[cfg_attr(feature =
/// "ts-export", derive(ts_rs::TS))]` line trailing the REAL derive line
/// must not be mistaken for the type's only derive attribute. An earlier
/// draft of `container_derives` stopped at the FIRST attribute line
/// containing `"derive("`, which is exactly this `ts_rs::TS` wrapper —
/// reading no `Deserialize` in it and wrongly granting the Serialize-only
/// exemption to a type that plainly derives `Deserialize` on the line
/// above. This is the real shape from `darkmux-crew/src/step_output.rs`
/// (derive line first, `cfg_attr(ts-export, derive(...))` trailing,
/// closer to the struct) — not a contrived ordering.
#[test]
fn the_ts_export_cfg_attr_wrapper_does_not_mask_the_real_derive() {
    let src = r#"
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let findings = scan_lines(&lines);
    assert_eq!(
        findings.len(),
        1,
        "a type deriving real Deserialize behind a trailing ts-export cfg_attr wrapper must \
         still be recognized as deriving Deserialize, and its unguarded skip_serializing_if \
         field must be flagged — found {findings:?} instead"
    );
}

/// The container-lookup itself: a field inside an ENUM's struct-like
/// variant resolves to the ENUM as its container, not to some unrelated
/// preceding struct.
#[test]
fn an_enum_variant_field_resolves_to_the_enum_as_its_container() {
    let src = r#"
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Unrelated {
    pub x: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Scratch {
    Variant {
        #[serde(skip_serializing_if = "Vec::is_empty")]
        items: Vec<String>,
    },
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let findings = scan_lines(&lines);
    assert_eq!(
        findings.len(),
        1,
        "the enum variant's field must resolve to `Scratch` (which derives Deserialize) as its \
         container, not to the preceding Serialize-only `Unrelated` struct — found {findings:?}"
    );
}
