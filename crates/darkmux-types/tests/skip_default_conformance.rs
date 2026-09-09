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
//!      the same field, in EITHER order — the attribute carrying
//!      `default` may sit before or after the one carrying
//!      `skip_serializing_if`) also carries `default`;
//!   2. the field's type is `Option<...>` — including a path-qualified
//!      spelling (`std::option::Option<T>`) — and the attribute carries
//!      no `deserialize_with` (serde implicitly defaults a missing
//!      `Option` even with no `#[serde(default)]` at all —
//!      `deserialize_with` disables that implicit default and makes the
//!      field required again, so this exemption does NOT apply once one
//!      is present);
//!   3. the enclosing struct/enum is proven to NOT be deserializable — by
//!      reading its OWN nearest `#[derive(...)]` attribute (found on any
//!      directly-adjacent attribute block above it, INCLUDING one sharing
//!      its own declaration line, and INCLUDING one separated from it by
//!      a multi-line `#[cfg_attr(...)]`) and confirming that derive names
//!      no `Deserialize`, AND confirming no hand-written `impl ...
//!      Deserialize ... for <Type>` exists anywhere else in the file — a
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
//! **A 2026-09-10 frontier review planted 25 adversarial shapes against
//! an earlier draft of this scan and found seven unadmitted gaps, two of
//! which were SILENT MISSES — the one direction this module's doc
//! promises never happens. This section was rewritten from that review;
//! every claim below was re-verified against the CURRENT code, not
//! carried over from the earlier draft.** Five of the seven gaps are
//! fixed in the code below and pinned by a permanent test each (named in
//! the bullets); the remaining two are honestly admitted as still-open,
//! in the FALSE-FINDING direction only.
//!
//! - **Reflection-free.** There is no `#[serde(...)]` introspection in
//!   stable Rust. This scan approximates it with string matching on
//!   attribute text; a sufficiently adversarial rewrite (a macro that
//!   GENERATES a `#[derive(Deserialize)]` invisibly, or a proc macro that
//!   rewrites field attributes at compile time) defeats it. No such macro
//!   exists in this tree today (verified: no `derive_deserialize`-shaped
//!   proc macro is defined or used here). Renaming a value to literally
//!   contain the substring `"default"` is NO LONGER in this category —
//!   see the next bullet, fixed.
//! - **FIXED (was a silent miss): a keyword substring inside a quoted
//!   VALUE no longer grants an exemption it shouldn't.** `skip_serializing_if
//!   = "is_default"` (a stock serde idiom — a helper function literally
//!   named `is_default`, not an adversarial rewrite) and `rename =
//!   "default"` (renaming the JSON key to the literal string `"default"`)
//!   both used to satisfy the old `combined.contains("default")` check by
//!   substring match against the quoted value, silently granting
//!   exemption route 1 to a field with no real `#[serde(default)]`
//!   keyword at all. The `default`/`deserialize_with` keyword checks now
//!   strip quoted string CONTENTS first (`strip_quoted_content`) and
//!   match the real keyword as a whole token
//!   (`has_keyword_outside_strings`), so only the genuine, unquoted
//!   attribute keyword counts. Pinned by
//!   `skip_serializing_if_is_default_value_does_not_grant_the_default_exemption`,
//!   `rename_to_default_does_not_grant_the_default_exemption`, and
//!   `a_real_default_keyword_next_to_a_default_shaped_value_is_still_recognized`
//!   (the real keyword still works alongside those same value shapes).
//! - **FIXED (was a silent miss): an unclosed attribute block now panics
//!   instead of silently consuming the rest of the file.** `code_only`
//!   truncates a line at its first `//`, with no string-literal
//!   awareness — a `//` inside an attribute's own string VALUE (a URL in
//!   a `rename` or a doc-link attribute, e.g. `rename =
//!   "https://example.com/schema"`) is misread as a comment start, hiding
//!   the real closing `)]` from `attr_block`'s bracket-depth counter.
//!   Before this fix that left the block "open": the scan would consume
//!   every remaining line of the file as part of one never-closing
//!   attribute, and `all_attr_blocks` would treat every LATER attribute
//!   in the file as already consumed — silently unscanned, with the
//!   assertion staying green. `attr_block` now asserts the block actually
//!   closed before returning and panics, naming the file and the block's
//!   start line, when it doesn't — converting the failure from silent to
//!   loud rather than teaching `code_only` to parse string literals (a
//!   real fix, but a materially bigger one; not attempted here). Pinned
//!   by `an_unclosed_attribute_panics_instead_of_silently_skipping_the_rest_of_the_file`
//!   and `an_unclosed_trailing_attribute_still_panics_with_nothing_downstream_to_miss`.
//!   No real site in the swept crates carries a `//` inside an attribute
//!   string today (verified by grep) — this guard is for the day one
//!   does.
//! - **FIXED (was a silent miss): a `Serialize` derive plus a
//!   hand-written `impl Deserialize` is no longer silently exempted.**
//!   `container_derives` only ever read the derive ATTRIBUTE text — a
//!   type deriving `#[derive(Serialize)]` (no `Deserialize` named there)
//!   but ALSO carrying a hand-written `impl Deserialize for T` elsewhere
//!   in the file is genuinely deserializable, and used to get exemption
//!   route 3 anyway. `scan_lines` now also checks
//!   `file_hand_impls_deserialize_for` (a token-based scan for `impl ...
//!   Deserialize ... for <the container's own type name>` anywhere in the
//!   file) and denies the exemption when it matches. Pinned by
//!   `a_serialize_derive_plus_hand_written_deserialize_impl_is_not_exempt`
//!   (and its twin, `..._stays_exempt`, with no hand-impl present). No
//!   real site in the swept crates hand-implements `Deserialize` today
//!   (verified by grep for `impl.*Deserialize.*for`).
//! - **A manually-written `impl Deserialize` with NO `#[derive(...)]` at
//!   all** — as opposed to the fixed case above, which is `derive(Serialize)`
//!   PLUS a hand-impl — is still not specifically recognized as deserializable
//!   by name; but since exemption route 3 requires a derive attribute to be
//!   FOUND at all before granting anything, a type with no derive
//!   whatsoever is already conservatively treated as NOT proven safe and
//!   flagged if it also carries an unguarded `skip_serializing_if` — the
//!   safe direction either way (a possible false finding on a genuinely
//!   inert type, never a silent miss on a genuinely deserializable one).
//! - **FIXED (loud false finding): `default` written on its own line
//!   BEFORE `skip_serializing_if`, directly adjacent (no blank/code line
//!   between), is now recognized.** The field-merge in `scan_lines`
//!   previously only walked FORWARD from the `skip_serializing_if` block,
//!   so this ordering was invisible to the `default` check and got
//!   wrongly flagged despite being a real, safe split-attribute pair.
//!   `scan_lines` now also walks BACKWARD through directly-adjacent
//!   sibling blocks, chaining through any number of them, the same
//!   adjacency rule (blank lines tolerated, real code stops it) as the
//!   forward walk. Pinned by
//!   `default_written_before_skip_serializing_if_on_an_adjacent_line_is_recognized`
//!   and (proving the adjacency bound still holds)
//!   `a_default_attribute_separated_by_a_real_line_is_not_merged_backward`.
//! - **Container-level `#[serde(default)]`** (a single attribute on the
//!   struct itself, applying to every field with no per-field
//!   `#[serde(default)]` needed) is still not recognized as satisfying
//!   the per-field check — the field-merge walk (forward or backward)
//!   only ever considers attribute blocks directly adjacent to the FIELD,
//!   never a container-level attribute many lines away, before the
//!   struct's own declaration. Verified no site in the swept crates uses
//!   this form (`grep -rn '^#\[serde(default)\]$'` across every crate's
//!   `src/` returns nothing). Failure direction: a false finding, not a
//!   silent miss.
//! - **FIXED (loud false finding): a fully path-qualified
//!   `std::option::Option<T>` field is now recognized as a safe bare
//!   Option.** The old check was a literal `field_type.starts_with("Option<")`
//!   prefix match, which missed any qualified spelling and wrongly denied
//!   the route-2 exemption. `is_option_type` now checks only the LAST
//!   `::`-segment of the type's own LEADING path (the part before its
//!   first `<`), so a qualified `Option` is recognized while `Option`'s
//!   own generic ARGUMENT containing `::` (`Option<serde_json::Value>`,
//!   the real shape at `darkmux-flow/src/schema.rs`'s
//!   `payload: Option<serde_json::Value>`) is not mistaken for a
//!   qualification of `Option` itself (an unbounded first draft of this
//!   fix — splitting the WHOLE type string on `::` — caught exactly this
//!   the first time the real tree-wide sweep ran against it: it flipped
//!   `payload` to look like a non-Option and wrongly demand `default`).
//!   Pinned by `a_fully_qualified_option_type_is_exempt` and
//!   `an_option_whose_generic_argument_contains_a_path_is_still_exempt`.
//! - **FIXED (loud false finding): a derive on the SAME physical line as
//!   the struct declaration (`#[derive(...)] pub struct Foo {`) is now
//!   read.** The old backward derive-scan started one line ABOVE the
//!   declaration and never inspected the declaration's own line, so a
//!   same-line derive was invisible — read as "no derive found at all"
//!   and wrongly denied the write-only exemption to a genuinely
//!   Serialize-only type. `container_derives` now checks
//!   `all_attr_blocks` for a block whose START equals the declaration's
//!   own line first. Pinned by
//!   `a_derive_on_the_same_line_as_the_struct_declaration_is_recognized`
//!   and `a_serialize_only_derive_on_the_same_line_as_the_struct_declaration_is_exempt`.
//! - **FIXED (loud false finding): a MULTI-LINE `#[cfg_attr(...)]`
//!   sitting between the real derive and the struct declaration no longer
//!   breaks the backward derive lookup.** The old backward scan walked
//!   raw LINES one at a time and broke the instant it hit a continuation
//!   line (a bare `)]`, an indented `derive(...)`) that doesn't itself
//!   start with `#[` — so on a genuinely write-only (Serialize-only) type
//!   behind a multi-line `#[cfg_attr(...)]`, it never reached the real
//!   derive line, read `found` as false, and wrongly denied the
//!   write-only exemption (the reviewer's literal wording: "flagging a
//!   genuinely write-only type"). `container_derives` is now BLOCK-aware
//!   — it walks `all_attr_blocks`'s already-bounded blocks (each correct
//!   regardless of how many physical lines it spans) rather than raw
//!   lines, so a multi-line block in the chain can't break it mid-block.
//!   Pinned by
//!   `a_multiline_cfg_attr_between_the_derive_and_the_struct_does_not_break_the_write_only_exemption`
//!   (the polarity that actually distinguishes correct behavior from the
//!   old bug — a Deserialize-deriving type behind the same shape gets
//!   flagged either way, correctly or by accident, so that polarity alone
//!   can't prove this; kept anyway as
//!   `..._still_finds_a_real_deserialize_derive` since it's still a valid
//!   assertion of correct behavior). This shape (real derive line first,
//!   `#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]` trailing,
//!   both single-line) already worked before this fix and stays pinned by
//!   `the_ts_export_cfg_attr_wrapper_does_not_mask_the_real_derive` — the
//!   NEW gap this fix closes is specifically the WRAPPER ITSELF spanning
//!   multiple physical lines.
//! - **STILL OPEN (loud false finding, admitted, not fixed): raw string
//!   literals and `/* */` block comments carrying attribute-shaped text
//!   are scanned as if they were real code.** `code_only` strips `//`
//!   line comments but has no concept of a raw string (`r#"..."#`) or a
//!   `/* ... */` block comment, both of which can span multiple physical
//!   lines and legitimately CONTAIN text that looks exactly like a real
//!   `#[serde(skip_serializing_if = "...")]` field — the realistic case
//!   the reviewer named is a `#[cfg(test)] mod tests` block inside a
//!   `src/` file holding a Rust source snippet as a raw-string test
//!   fixture (this very test file does exactly that, for its own
//!   self-tests — though `tests/` isn't itself swept). A correct fix
//!   needs a real lexer pass (raw-string and nested-block-comment
//!   tracking at the WHOLE-FILE level, with care not to also misread `/*`
//!   or a bare `r"` appearing inside an ORDINARY quoted string) — a
//!   meaningfully larger and riskier change than anything else in this
//!   list, and one a hand-rolled partial version could get subtly wrong
//!   in ways worse than the status quo (e.g. mis-detecting a block-comment
//!   start inside a normal string and then blanking real code past it).
//!   Not attempted in this pass. Failure direction: a false finding (a
//!   fake attribute inside a raw string or block comment gets flagged as
//!   if real), never a silent miss (nothing REAL goes unscanned because
//!   of this). Re-verify by hand (`grep -rln 'r#"' crates/*/src src | xargs grep -l 'skip_serializing_if\|#\[serde'`)
//!   if a swept `src/` file starts embedding serde-attribute-shaped text
//!   in a raw string or block comment; none does today.
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
//! - **Two more real Cargo projects sit outside the swept roots**, beyond
//!   `runtime/`: `plugins/darkmux-bundler-rust` and
//!   `tools/darkmux-mock-model`, neither under `crates/` nor the
//!   top-level `src/`. Checked by hand (2026-09-10): the mock-model tool
//!   carries no `skip_serializing_if` fields at all (`grep -rn
//!   skip_serializing_if tools/darkmux-mock-model/src` returns nothing).
//!   The bundler plugin's `Bundle` (`plugins/darkmux-bundler-rust/src/contract.rs:35,39`)
//!   carries two — `manifest: Vec<String>` and `truncated: bool` — both
//!   already safe (route 1: both pair `skip_serializing_if` with an
//!   explicit `default` on the same attribute). `Bundle` is exactly the
//!   class this scan exists to protect: a cross-process wire contract
//!   (bundler output, consumed by the probe pass). Neither project is
//!   swept today; re-verify by hand if either grows a new unguarded
//!   `skip_serializing_if` field, the same discipline as `runtime/`
//!   above.
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
///
/// `file_label` names the source this scan is walking (a real file's
/// path for the tree-wide sweep, a stub like `<test>` for the in-memory
/// self-tests below) — used ONLY for the panic message below, never for
/// behavior.
///
/// **Loud on an unclosed block, never silent (2026-09-10 review fix).**
/// `code_only` truncates a line at its first `//`, with no string-literal
/// awareness — so a `//` inside an attribute's own string VALUE (a URL in
/// a `rename`, a doc link) is misread as a comment start, and the real
/// closing `)]` past it is hidden from the depth counter below. Before
/// this fix, that silently left the block "open": the scan would consume
/// every remaining line of the file looking for a `]` that will never
/// balance, and `all_attr_blocks` would then treat the ENTIRE rest of the
/// file as consumed by that one block — every later attribute in the file
/// silently unscanned, with the assertion staying green. This is
/// EXACTLY the direction the rest of this module's known limits promise
/// never happens. Rather than teach `code_only` to parse string literals
/// (a real fix, but a bigger one), this converts the failure to a loud
/// panic naming the file and the block's start line — a maintainer sees
/// a hard test failure instead of a quiet gap in coverage.
fn attr_block(lines: &[&str], start: usize, file_label: &str) -> (String, usize) {
    let mut depth: i32 = 0;
    let mut entered = false;
    let mut out = String::new();
    let mut end = start;
    let mut closed = false;
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
            closed = true;
            break;
        }
    }
    assert!(
        closed,
        "{file_label}:{}: an attribute block starting here never closed before the end of the \
         scanned range. The likely cause: a `//` inside the attribute's own string VALUE (a URL \
         in a `rename`/doc attribute, e.g. `rename = \"https://example.com/schema\"`) was \
         misread by `code_only` as a comment start, hiding the attribute's real closing `)]` \
         from the bracket-depth counter. Left unguarded, this would silently consume every \
         remaining line of the file as part of one never-closing block, and every attribute \
         after it would go unscanned with the assertion staying green — exactly the silent-miss \
         direction this scan promises never happens. Fix by removing the bare `//` from the \
         attribute's string value (e.g. via a URL shortener or a differently-worded doc \
         reference), or by teaching `code_only` to be string-literal aware.",
        start + 1,
    );
    (out, end)
}

/// Every attribute block in `lines`, in file order, as `(start, end,
/// text)`. See `attr_block` for what `file_label` is for.
fn all_attr_blocks(lines: &[&str], file_label: &str) -> Vec<(usize, usize, String)> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if code_only(lines[i]).trim_start().starts_with("#[") {
            let (text, end) = attr_block(lines, i, file_label);
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
/// BLOCK directly above `decl_idx` (blank lines and doc comments — both
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
///
/// **BLOCK-granular, not LINE-granular (2026-09-10 review fix).** Takes
/// `blocks` — the file's already-computed `all_attr_blocks` list — and
/// walks backward through THOSE, rather than walking raw lines one at a
/// time. Two real shapes broke the old line-by-line walk:
///
///   1. A MULTI-LINE `#[cfg_attr(\n    feature = "...",\n    derive(...)
///      \n)]` between the real derive and the struct: the old walk broke
///      the instant it hit a continuation line that doesn't itself start
///      with `#[` (a line like `    derive(...)` or `)]` alone), so it
///      never reached the real derive line further back — a genuinely
///      Deserialize-deriving type read as Serialize-only and wrongly
///      exempted. Walking whole blocks (each already correctly bounded by
///      `attr_block`'s own bracket-depth scan, regardless of how many
///      physical lines it spans) can't break mid-block.
///   2. A derive on the SAME line as the declaration (`#[derive(Serialize,
///      Deserialize)] pub struct Foo {`): the old walk started at
///      `decl_idx - 1` and never looked at `decl_idx`'s own line, so a
///      derive sharing that line was never read at all. `all_attr_blocks`
///      already records this shape as a block whose `start == decl_idx`
///      (its bracket pair closes within the same line before the
///      declaration keyword); this walk checks for that block first.
fn container_derives(lines: &[&str], blocks: &[(usize, usize, String)], decl_idx: usize) -> (bool, bool) {
    let mut found = false;
    let mut has_deserialize = false;
    let consider = |text: &str, found: &mut bool, has_deserialize: &mut bool| {
        if text.contains("derive(") {
            *found = true;
            if text.contains("Deserialize") {
                *has_deserialize = true;
            }
        }
    };

    // `boundary`: the line ABOVE which the next candidate block's trailing
    // gap must be entirely blank to count as "directly adjacent".
    let mut boundary = decl_idx;

    // Same-line case: an attribute block that starts on `decl_idx` itself
    // (the derive and the declaration share one physical line).
    if let Some((b_start, _, b_text)) = blocks.iter().find(|(start, _, _)| *start == decl_idx) {
        consider(b_text, &mut found, &mut has_deserialize);
        boundary = *b_start;
    }

    let mut candidate = blocks.iter().rposition(|(_, end, _)| *end < boundary);
    while let Some(i) = candidate {
        let (b_start, b_end, b_text) = &blocks[i];
        let all_blank_between = (*b_end + 1..boundary).all(|li| code_only(lines[li]).trim().is_empty());
        if !all_blank_between {
            break;
        }
        consider(b_text, &mut found, &mut has_deserialize);
        boundary = *b_start;
        candidate = if i == 0 { None } else { Some(i - 1) };
    }

    (found, has_deserialize)
}

/// The name token immediately following `struct`/`enum` in `line`'s code
/// — tokenized the same way `is_type_decl_line` finds the keyword, so a
/// generic parameter list (`struct Scratch<T> {`) or a same-line derive
/// prefix doesn't defeat it. `None` should not happen for a line already
/// proven by `is_type_decl_line` to declare a type.
fn type_name_from_decl(line: &str) -> Option<String> {
    let tokens: Vec<&str> =
        code_only(line).split(|c: char| !c.is_alphanumeric() && c != '_').filter(|t| !t.is_empty()).collect();
    for (i, tok) in tokens.iter().enumerate() {
        if *tok == "struct" || *tok == "enum" {
            return tokens.get(i + 1).map(|s| s.to_string());
        }
    }
    None
}

/// True when `lines` contains a hand-written `impl ... Deserialize ...
/// for <type_name>` — i.e. `type_name` is deserializable via a manual
/// trait impl rather than (or in addition to) `#[derive(Deserialize)]`.
/// Token-based, not a real parser: splits each line's code on
/// non-identifier characters (so lifetimes/generics like `impl<'de>
/// Deserialize<'de> for Scratch` don't defeat it), and treats a line as a
/// matching impl header when its first token is `impl`, `Deserialize`
/// appears anywhere on it, and the token immediately after `for` equals
/// `type_name` — the shape of every real `impl Deserialize for T` header
/// this codebase or serde's own documentation writes on one line.
fn file_hand_impls_deserialize_for(lines: &[&str], type_name: &str) -> bool {
    for line in lines {
        let tokens: Vec<&str> =
            code_only(line).split(|c: char| !c.is_alphanumeric() && c != '_').filter(|t| !t.is_empty()).collect();
        if tokens.first() != Some(&"impl") {
            continue;
        }
        if !tokens.contains(&"Deserialize") {
            continue;
        }
        if let Some(for_idx) = tokens.iter().position(|t| *t == "for") {
            if tokens.get(for_idx + 1) == Some(&type_name) {
                return true;
            }
        }
    }
    false
}

/// Strip the CONTENTS of double-quoted string literals from `text` (the
/// quotes themselves stay, so bracket/paren scanning elsewhere in this
/// file is unaffected) — used only by the keyword checks below, so a
/// value string that happens to CONTAIN a keyword substring
/// (`skip_serializing_if = "is_default"`, `rename = "default"`) doesn't
/// get mistaken for the real, unquoted `#[serde(default)]` /
/// `#[serde(..., deserialize_with = "...")]` keywords. No escape-sequence
/// handling: no serde attribute value in this tree, or in serde's own
/// documented forms, needs an escaped quote.
fn strip_quoted_content(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    for ch in text.chars() {
        if ch == '"' {
            in_string = !in_string;
            out.push(ch);
            continue;
        }
        if in_string {
            continue;
        }
        out.push(ch);
    }
    out
}

/// True when `text`, with quoted string CONTENTS removed, contains
/// `keyword` as a whole token (split on non-identifier characters) —
/// the real, unquoted attribute keyword, not a substring match against a
/// quoted VALUE.
fn has_keyword_outside_strings(text: &str, keyword: &str) -> bool {
    strip_quoted_content(text)
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|tok| tok == keyword)
}

/// True when `field_type` is a bare `Option<...>` — including a
/// path-qualified spelling (`std::option::Option<T>`,
/// `core::option::Option<T>`) — by checking only the LAST `::`-separated
/// segment of the type's OWN leading path (the part before its first
/// `<`) rather than requiring the type string to literally start with
/// `"Option<"` or `"Option <"`. A qualified spelling is real, valid Rust
/// and a literal-prefix match on the unqualified form misses it, wrongly
/// denying the safe bare-`Option` exemption (route 2) to a field that
/// needs no `default` at all.
///
/// The prefix is bounded at the first `<` deliberately: `Option`'s own
/// generic ARGUMENT can itself contain `::` (`Option<serde_json::Value>`)
/// without qualifying `Option` at all — splitting the WHOLE type string
/// on `::` (rather than just the leading path) would take the argument's
/// last segment instead and wrongly conclude the field isn't an `Option`
/// — the opposite, unsafe-in-code-but-loud direction (a real bare
/// `Option` field flagged as needing `default`), caught by this scan's
/// own real-tree sweep the first time this helper was written without
/// the bound (`darkmux-flow/src/schema.rs`'s `payload:
/// Option<serde_json::Value>`).
fn is_option_type(field_type: &str) -> bool {
    let trimmed = field_type.trim_start();
    let prefix_end = trimmed.find('<').unwrap_or(trimmed.len());
    let prefix = trimmed[..prefix_end].trim_end();
    let last_segment = prefix.rsplit("::").next().unwrap_or(prefix);
    last_segment == "Option" && trimmed[prefix_end..].starts_with('<')
}

struct Finding {
    file: PathBuf,
    line: usize, // 1-based, the FIELD's own line
    field_text: String,
}

/// Scan one file's lines for `skip_serializing_if` fields with no safe
/// exemption. `lines` rather than a path so the same function backs both
/// the real tree-wide sweep and the in-memory planted-violation self-test
/// below. `file_label` is threaded through to `attr_block`'s unclosed-
/// block panic (see there); the self-tests pass a stub.
fn scan_lines(lines: &[&str], file_label: &str) -> Vec<(usize, String)> {
    let mut findings = Vec::new();
    let blocks = all_attr_blocks(lines, file_label);
    for (idx, (start, end, text)) in blocks.iter().enumerate() {
        let (start, end) = (*start, *end);
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
                let (sib_text, sib_end) = attr_block(lines, j, file_label);
                combined.push_str(&sib_text);
                j = sib_end + 1;
                continue;
            }
            break;
        }
        if j >= lines.len() {
            continue; // malformed / attribute at EOF with no field after it — nothing to check
        }

        // Directly-adjacent PRECEDING sibling attribute blocks — e.g.
        // `#[serde(default)]` written on its own line immediately ABOVE
        // `#[serde(skip_serializing_if = "...")]`, no blank/code line
        // between — also contribute to the default/deserialize_with
        // check. The forward loop above only ever walked later blocks, so
        // this ordering (default-before-skip, a real and unremarkable
        // split-attribute shape) was previously invisible to `combined`
        // and got wrongly flagged (2026-09-10 review fix).
        let mut boundary = start;
        let mut prev_idx = idx;
        while prev_idx > 0 {
            let (prev_start, prev_end, prev_text) = &blocks[prev_idx - 1];
            let all_blank_between = (*prev_end + 1..boundary).all(|li| code_only(lines[li]).trim().is_empty());
            if !all_blank_between {
                break;
            }
            combined.push_str(prev_text);
            boundary = *prev_start;
            prev_idx -= 1;
        }

        let field_line = code_only(lines[j]);
        let Some(colon) = field_line.find(':') else {
            continue; // not actually a field declaration — this attribute wasn't on a field
        };
        let field_type = field_line[colon + 1..].trim().trim_end_matches(',').to_string();

        // Keyword checks run on `combined` with quoted string CONTENTS
        // stripped first, so a VALUE that happens to contain the keyword
        // as a substring (`skip_serializing_if = "is_default"` — a stock
        // serde idiom, not an adversarial rewrite; `rename = "default"`)
        // doesn't get mistaken for the real, unquoted attribute keyword
        // (2026-09-10 review fix — both were previously silent misses:
        // the old bare `combined.contains("default")` matched the
        // substring inside the quoted value and wrongly granted the
        // exemption).
        let has_default = has_keyword_outside_strings(&combined, "default");
        if has_default {
            continue; // exemption route 1
        }

        let has_deserialize_with = has_keyword_outside_strings(&combined, "deserialize_with");
        let is_option = is_option_type(&field_type);
        if is_option && !has_deserialize_with {
            continue; // exemption route 2
        }

        let exempt_serialize_only = match nearest_container_decl(lines, start) {
            Some(decl_idx) => {
                let (found, has_deserialize) = container_derives(lines, &blocks, decl_idx);
                let hand_impls_deserialize = type_name_from_decl(lines[decl_idx])
                    .map(|name| file_hand_impls_deserialize_for(lines, &name))
                    .unwrap_or(false);
                found && !has_deserialize && !hand_impls_deserialize
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
            let label = file.display().to_string();
            for (line, field_text) in scan_lines(&lines, &label) {
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
            let label = file.display().to_string();
            total +=
                all_attr_blocks(&lines, &label).iter().filter(|(_, _, t)| t.contains("skip_serializing_if")).count();
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
    let findings = scan_lines(&lines, "<test>");
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
        scan_lines(&lines, "<test>").is_empty(),
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
    assert!(scan_lines(&lines, "<test>").is_empty(), "a bare Option field with no deserialize_with must be exempt (route 2)");
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
        scan_lines(&lines, "<test>").len(),
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
        scan_lines(&lines, "<test>").is_empty(),
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
        scan_lines(&lines2, "<test>").is_empty(),
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
    let findings = scan_lines(&lines, "<test>");
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
    let findings = scan_lines(&lines, "<test>");
    assert_eq!(
        findings.len(),
        1,
        "the enum variant's field must resolve to `Scratch` (which derives Deserialize) as its \
         container, not to the preceding Serialize-only `Unrelated` struct — found {findings:?}"
    );
}

/// MUST FIX 1 (2026-09-10 review): a `//` inside an attribute's own
/// string VALUE (the URL in a `rename`) is misread by `code_only` as a
/// comment start, hiding the attribute's real closing `)]` from the
/// bracket-depth counter in `attr_block`. Left unguarded, the block never
/// "closes" and the scan would silently consume the rest of the file as
/// part of it — every later attribute unscanned, including the very next
/// field's unguarded `skip_serializing_if`. This must now panic loudly
/// instead.
#[test]
fn an_unclosed_attribute_panics_instead_of_silently_skipping_the_rest_of_the_file() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(rename = "https://example.com/schema", default)]
    pub safe: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan_lines(&lines, "<planted>")));
    assert!(
        result.is_err(),
        "a `//` inside the `rename` attribute's URL value must blind `code_only`'s comment \
         stripper and leave that attribute block unclosed for the rest of the file — this must \
         now panic loudly (naming the file and line) instead of silently skipping the \
         unguarded `items` field that follows it"
    );
    let msg = result.unwrap_err();
    let msg = msg.downcast_ref::<String>().map(String::as_str).unwrap_or("<non-string panic payload>");
    assert!(
        msg.contains("<planted>") && msg.contains("never closed"),
        "the panic must name the file label and explain the block never closed, got: {msg}"
    );
}

/// Twin of the above, in the OTHER order: `//` inside a URL value that
/// appears in an attribute AFTER all real fields in the file (so nothing
/// downstream would have been silently skipped) must still be recognized
/// as unclosed and panic — the guard isn't allowed to depend on there
/// being a later victim field to reveal it.
#[test]
fn an_unclosed_trailing_attribute_still_panics_with_nothing_downstream_to_miss() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub items: Vec<String>,
}

#[cfg_attr(test, doc = "see https://example.com/x")]
fn helper() {}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan_lines(&lines, "<planted-tail>")));
    assert!(result.is_err(), "an unclosed trailing attribute must panic even with no field after it to miss");
}

/// MUST FIX 2 (2026-09-10 review): a type deriving `Serialize` only, with
/// no `Deserialize` in its derive list, but carrying a HAND-WRITTEN `impl
/// Deserialize for Scratch` elsewhere in the file, is genuinely
/// deserializable — exemption route 3 must not grant it the write-only
/// exemption just because the derive attribute alone doesn't mention
/// `Deserialize`.
#[test]
fn a_serialize_derive_plus_hand_written_deserialize_impl_is_not_exempt() {
    let src = r#"
#[derive(Debug, Clone, Serialize)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

impl<'de> Deserialize<'de> for Scratch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        unimplemented!()
    }
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let findings = scan_lines(&lines, "<test>");
    assert_eq!(
        findings.len(),
        1,
        "a type deriving Serialize only but with a hand-written `impl Deserialize for Scratch` \
         is deserializable and must NOT get the write-only exemption — found {findings:?} instead"
    );
}

/// Twin of the above: no hand-written impl present at all — the
/// Serialize-only exemption must still apply normally, proving the new
/// hand-impl check isn't just always-unsafe.
#[test]
fn a_serialize_derive_with_no_hand_written_deserialize_impl_stays_exempt() {
    let src = r#"
#[derive(Debug, Clone, Serialize)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    assert!(
        scan_lines(&lines, "<test>").is_empty(),
        "with no hand-written Deserialize impl anywhere in the file, the Serialize-only \
         exemption must still apply"
    );
}

/// Silent miss #1 (2026-09-10 review): `skip_serializing_if =
/// "is_default"` is a stock serde idiom (a helper function literally
/// named `is_default`), not an adversarial rewrite. The old
/// `combined.contains("default")` check matched the substring inside
/// that quoted VALUE and wrongly granted the `default` exemption even
/// though no real `#[serde(default)]` keyword is present.
#[test]
fn skip_serializing_if_is_default_value_does_not_grant_the_default_exemption() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(skip_serializing_if = "is_default")]
    pub count: u32,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let findings = scan_lines(&lines, "<test>");
    assert_eq!(
        findings.len(),
        1,
        "`skip_serializing_if = \"is_default\"` must NOT be mistaken for a real `#[serde(default)]` \
         keyword just because the quoted VALUE contains the substring \"default\" — found \
         {findings:?} instead"
    );
}

/// Silent miss #2 (2026-09-10 review): `rename = "default"` renames the
/// field's JSON key to the literal string `"default"` — it says nothing
/// about a `#[serde(default)]` keyword. Same substring hole as above, on
/// a different attribute.
#[test]
fn rename_to_default_does_not_grant_the_default_exemption() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(rename = "default", skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let findings = scan_lines(&lines, "<test>");
    assert_eq!(
        findings.len(),
        1,
        "`rename = \"default\"` must NOT be mistaken for a real `#[serde(default)]` keyword just \
         because the renamed KEY happens to spell \"default\" — found {findings:?} instead"
    );
}

/// Twin of the two above: a REAL `#[serde(default)]` keyword sitting
/// alongside a value string that also happens to contain "default" must
/// still be recognized — the quote-stripping fix isn't allowed to start
/// missing the real keyword too.
#[test]
fn a_real_default_keyword_next_to_a_default_shaped_value_is_still_recognized() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(default, rename = "default", skip_serializing_if = "is_default")]
    pub count: u32,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    assert!(
        scan_lines(&lines, "<test>").is_empty(),
        "a real, unquoted `default` keyword must still be recognized even when it sits beside \
         `rename = \"default\"` and `skip_serializing_if = \"is_default\"` on the same attribute"
    );
}

/// Loud false finding #1 (2026-09-10 review): `#[serde(default)]` placed
/// BEFORE `skip_serializing_if` as a separate, directly-adjacent sibling
/// attribute line. The forward merge in `scan_lines` only ever walked
/// LATER blocks, so this ordering was invisible to the `default` check
/// and got wrongly flagged despite being a real, safe split-attribute
/// pair.
#[test]
fn default_written_before_skip_serializing_if_on_an_adjacent_line_is_recognized() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    assert!(
        scan_lines(&lines, "<test>").is_empty(),
        "`#[serde(default)]` written on its own line directly ABOVE `#[serde(skip_serializing_if \
         = ...)]` (no blank/code line between) is the same split-attribute shape as the \
         already-supported after-ordering — it must be recognized too"
    );
}

/// Twin of the above: a genuine GAP (a real field/code line) between the
/// `default` attribute and the `skip_serializing_if` attribute must NOT
/// be merged — proves the backward merge is adjacency-bounded, not just
/// always-safe.
#[test]
fn a_default_attribute_separated_by_a_real_line_is_not_merged_backward() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(default)]
    pub other: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let findings = scan_lines(&lines, "<test>");
    assert_eq!(
        findings.len(),
        1,
        "a `default` attribute belonging to a DIFFERENT, preceding field must not be merged into \
         `items`'s check just because it's a `default` attribute somewhere above — found \
         {findings:?} instead"
    );
}

/// Loud false finding #2 (2026-09-10 review): a fully path-qualified
/// `std::option::Option<T>` field is a real, valid, safe bare Option —
/// the old literal `starts_with("Option<")` prefix check missed the
/// qualified spelling and wrongly flagged it.
#[test]
fn a_fully_qualified_option_type_is_exempt() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maybe: std::option::Option<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    assert!(
        scan_lines(&lines, "<test>").is_empty(),
        "`std::option::Option<T>` is a real, safe bare Option and must get the same route-2 \
         exemption as the unqualified spelling"
    );
}

/// Twin of the above: `Option`'s own generic ARGUMENT containing `::`
/// (`Option<serde_json::Value>`) must not be mistaken for a
/// path-qualified `Option` itself — the qualification check is bounded
/// at the type's OWN leading path, not the whole type string. This is
/// the exact real shape (`darkmux-flow/src/schema.rs`'s `payload:
/// Option<serde_json::Value>`) that caught an unbounded first draft of
/// this fix via the real tree-wide sweep.
#[test]
fn an_option_whose_generic_argument_contains_a_path_is_still_exempt() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    assert!(
        scan_lines(&lines, "<test>").is_empty(),
        "`Option<serde_json::Value>` is a bare Option whose generic argument merely HAS a path — \
         the `::` inside the angle brackets must not defeat the Option check"
    );
}

/// Loud false finding #3 (2026-09-10 review): a derive on the SAME
/// physical line as the struct declaration (`#[derive(...)] pub struct
/// Foo {`). The old backward walk started at `decl_idx - 1` and never
/// inspected the declaration's own line, so a same-line derive was never
/// read at all — a genuinely Deserialize-deriving type read as having no
/// derive attribute whatsoever and wrongly flagged.
#[test]
fn a_derive_on_the_same_line_as_the_struct_declaration_is_recognized() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)] pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let findings = scan_lines(&lines, "<test>");
    assert_eq!(
        findings.len(),
        1,
        "a derive sharing the struct's own declaration line must still be read — `Scratch` \
         derives Deserialize here and its unguarded field must be flagged — found {findings:?}"
    );
}

/// Twin of the above: the same same-line shape, but Serialize-only — must
/// still get the write-only exemption, proving the same-line lookup
/// reads the derive correctly in both directions.
#[test]
fn a_serialize_only_derive_on_the_same_line_as_the_struct_declaration_is_exempt() {
    let src = r#"
#[derive(Debug, Clone, Serialize)] pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    assert!(
        scan_lines(&lines, "<test>").is_empty(),
        "a Serialize-only derive sharing the struct's own declaration line must still grant the \
         write-only exemption"
    );
}

/// Loud false finding #4 (2026-09-10 review): a MULTI-LINE
/// `#[cfg_attr(...)]` sitting between the real derive and the struct
/// declaration, on a type that derives Serialize ONLY (write-only — the
/// reviewer's exact wording: "flagging a genuinely write-only type"). The
/// old backward walk was line-by-line and broke the instant it hit a
/// continuation line (like a bare `)]` or an indented `derive(...)`) that
/// doesn't itself start with `#[` — so it never reached the real derive
/// line further back, read `found` as false, and wrongly denied the
/// write-only exemption. Deliberately Serialize-ONLY here (not
/// Deserialize) — a Deserialize-deriving type would get flagged either
/// way (found=false and found=true-with-Deserialize both deny the
/// exemption), so THAT polarity can't tell a correct derive-lookup apart
/// from one that silently gave up; only the Serialize-only polarity below
/// actually distinguishes them.
#[test]
fn a_multiline_cfg_attr_between_the_derive_and_the_struct_does_not_break_the_write_only_exemption() {
    let src = r#"
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(
    feature = "ts-export",
    derive(ts_rs::TS)
)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    assert!(
        scan_lines(&lines, "<test>").is_empty(),
        "a multi-line `#[cfg_attr(...)]` between the real Serialize-only derive and the struct \
         must not break the backward derive lookup — `Scratch` derives Serialize only and its \
         skip_serializing_if field must stay exempt (route 3)"
    );
}

/// Twin of the above, in the OTHER polarity: the type behind the
/// multi-line `#[cfg_attr(...)]` genuinely DOES derive Deserialize —
/// confirms the lookup still finds it (not just that it stops denying the
/// exemption).
#[test]
fn a_multiline_cfg_attr_between_the_derive_and_the_struct_still_finds_a_real_deserialize_derive() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "ts-export",
    derive(ts_rs::TS)
)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let findings = scan_lines(&lines, "<test>");
    assert_eq!(
        findings.len(),
        1,
        "a multi-line `#[cfg_attr(...)]` between the real derive and the struct must not break \
         the backward derive lookup — `Scratch` derives Deserialize on the line above it and its \
         unguarded field must be flagged — found {findings:?}"
    );
}
