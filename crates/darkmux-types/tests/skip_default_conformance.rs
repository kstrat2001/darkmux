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
//! carried over from the earlier draft.** Five of the seven gaps were
//! fixed and pinned by a permanent test each; the remaining two were
//! recorded as "honestly admitted as still-open, in the FALSE-FINDING
//! direction only."
//!
//! **A THIRD-round 2026-09-10 review disproved that "false-finding-only"
//! claim, and independently re-planted and confirmed all seven of the
//! round-2 fixes still hold (each reverted in turn, each paired test
//! confirmed red).** The claim was false in two ways at once, both fixed
//! below and re-verified by running the corrected scan over the REAL
//! swept tree (not assumed):
//!
//!   - **A real, previously-uncounted SILENT MISS existed in production
//!     code, not just adversarial test fixtures.** The same-line
//!     attribute-and-field form (`#[serde(...)] pub field: T,`) was never
//!     modeled at all — an instrumented sweep of the real tree found 432
//!     skip blocks, 76 of which use this form, ALL 76 in
//!     `darkmux-types/src/config.rs` (the config-schema file this whole
//!     scan exists to protect), and ALL 76 silently misattributed to
//!     whatever line happened to follow. Fixed below; see the bullet
//!     naming it.
//!   - **A third failure DIRECTION existed that was neither a false
//!     finding nor a silent miss: a wrong-cause panic that could ABORT
//!     THE ENTIRE TEST.** The round-2 loud-panic-on-unclosed-block guard
//!     was itself triggerable by two shapes of ordinary, legal code (a
//!     `//` inside a doc-string URL; an unbalanced `[` inside an ordinary
//!     attribute string) that the review proved panic in this tree TODAY,
//!     with a message that named one specific, wrong cause. Both are now
//!     fixed outright (no longer panic at all) rather than merely
//!     message-corrected, by making the SAME quote-tracking primitive
//!     (already needed for the keyword checks) do the same job for
//!     comment detection and bracket-depth counting. See the bullet
//!     naming it, and the "genuinely unclosed" bullet for what the panic
//!     guard now actually covers and how its message was corrected to
//!     stop asserting a cause it can't know.
//!
//! Every claim in this section is now stated with its VERIFIED failure
//! direction, not a blanket claim — an accounting that enumerates gaps by
//! name reads as exhaustive, and a wrong one is worse than an admittedly
//! vague one; this section does not repeat that mistake a third time
//! without saying so.
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
//! - **FIXED (was a silent miss, round 2; one of the two mechanisms this
//!   fix relies on was unpinned until round 3): a keyword substring
//!   inside a quoted VALUE no longer grants an exemption it shouldn't.**
//!   `skip_serializing_if = "is_default"` (a stock serde idiom — a helper
//!   function literally named `is_default`, not an adversarial rewrite)
//!   and `rename = "default"` (renaming the JSON key to the literal
//!   string `"default"`) both used to satisfy the old
//!   `combined.contains("default")` check by substring match against the
//!   quoted value, silently granting exemption route 1 to a field with no
//!   real `#[serde(default)]` keyword at all. The fix has TWO parts: the
//!   `default`/`deserialize_with` keyword checks strip quoted string
//!   CONTENTS first (`strip_quoted_content`), AND match the real keyword
//!   as a whole TOKEN, not a substring (`has_keyword_outside_strings`).
//!   Round 3 found only the stripping half was actually pinned —
//!   replacing the whole-token match with a plain substring check on the
//!   stripped text left every prior test in this file green, because no
//!   planted shape happened to need tokenization once stripping alone was
//!   applied. An unquoted, bare token merely CONTAINING "default" as a
//!   substring (`not_default`) now pins that half specifically. Pinned by
//!   `skip_serializing_if_is_default_value_does_not_grant_the_default_exemption`,
//!   `rename_to_default_does_not_grant_the_default_exemption`, and
//!   `a_real_default_keyword_next_to_a_default_shaped_value_is_still_recognized`
//!   (stripping, round 2), and
//!   `an_unquoted_token_merely_containing_the_default_keyword_as_a_substring_is_not_mistaken_for_the_real_keyword`
//!   plus its `strip_quoted_content`/`has_keyword_outside_strings`-level
//!   twins (tokenization, round 3).
//! - **FIXED (was a silent miss, round 3, admitted but never accounted
//!   for at round 2): `strip_quoted_content` had no escape-sequence
//!   handling, so an ESCAPED quote inside a value could flip string
//!   parity and expose a quoted keyword as if it were real, unquoted
//!   code.** `rename = "a \"default\" value"` — the escaped `"default"`
//!   sits INSIDE the value, but the old toggle-on-every-`"` tracking
//!   closed the string early at the first escaped quote and reopened on
//!   the second, misreading the text between them as ordinary code and
//!   exposing "default" as if it were the genuine keyword. Round 2's own
//!   doc admitted this ("no serde attribute value in this tree ... needs
//!   an escaped quote") but never counted it as a failure DIRECTION
//!   alongside the others — it is a silent miss (wrongly grants the
//!   exemption), not the "false-finding-only" direction round 2 claimed
//!   for the module as a whole. `strip_quoted_content` now tracks a
//!   trailing backslash and does not toggle on an escaped `"`, the same
//!   primitive `code_only` and `attr_block`'s depth counter now share.
//!   Pinned by
//!   `an_escaped_quote_inside_an_attribute_string_does_not_flip_quote_parity`
//!   and the unit-level `strip_quoted_content_does_not_toggle_on_an_escaped_quote`.
//! - **FIXED (was a silent miss, round 2; the fix itself then caused a
//!   wrong-cause panic on legal code, round 3): an unclosed attribute
//!   block panics instead of silently consuming the rest of the file —
//!   AND `code_only` / `attr_block` are now quote-aware, so the two real,
//!   idiomatic shapes that used to trigger that panic no longer do.**
//!   Round 2: `code_only` truncated a line at its first `//` with no
//!   string-literal awareness, so a `//` inside an attribute's own string
//!   VALUE was misread as a comment start, hiding the real closing `)]`
//!   from `attr_block`'s bracket-depth counter and silently consuming the
//!   rest of the file as one never-closing block. Round 2 converted that
//!   to a loud panic rather than teaching `code_only` to parse strings.
//!   Round 3 found that panic itself firing on ordinary, shipped code —
//!   `#[doc = "See https://example.com/spec"]` and
//!   `#[arg(long, help = "Base URL, e.g. http://localhost:1234")]` (the
//!   command-layer files alone carry 236 such attribute lines, and that
//!   exact URL is a documented darkmux default) — so round 3 did the
//!   string-literal-aware fix round 2 deferred: `code_only` now tracks
//!   double-quote (with backslash-escape) parity and only treats `//` as
//!   a comment start OUTSIDE a string, and `attr_block`'s bracket-depth
//!   counter does the same for `[`/`]` (also fixing an unbalanced bracket
//!   INSIDE a string, e.g. `#[doc = "index with arr[0"]`, nothing to do
//!   with comments — proven to also wrong-cause-panic). Both primitives'
//!   quote state is carried ACROSS physical lines within one block scan,
//!   not reset per line — required for a real shape the round-3 sweep
//!   found in THIS tree, `config_access.rs`'s `#[must_use = "..."]`
//!   attribute, whose string value spans several lines via Rust's
//!   `\`-newline continuation; a per-line reset misreads that string's
//!   final closing quote as opening a NEW one and reintroduces the exact
//!   panic being fixed, on real code — caught by running the fix against
//!   the real tree, not assumed. The panic guard itself remains (a
//!   genuinely unbalanced construct — malformed/truncated source, or the
//!   admitted raw-string/block-comment gap below — must still fail loudly,
//!   never silently), and its message no longer asserts one specific
//!   wrong cause; it names the guard's own honest limits and lists
//!   candidates instead. Pinned by
//!   `an_attribute_with_no_closing_bracket_at_all_panics_instead_of_silently_skipping_the_rest_of_the_file`
//!   and `a_trailing_attribute_with_no_closing_bracket_still_panics_with_nothing_downstream_to_miss`
//!   (the guard still fires on a genuinely unclosed block), plus
//!   `a_url_containing_a_double_slash_inside_a_doc_string_does_not_falsely_unbalance_the_attribute_scan`,
//!   `a_help_string_containing_a_url_does_not_falsely_unbalance_the_attribute_scan`,
//!   `an_unbalanced_bracket_inside_a_quoted_attribute_string_does_not_break_the_depth_counter`,
//!   and `a_string_literal_continued_across_multiple_lines_via_backslash_newline_does_not_break_the_depth_counter`
//!   (none of the four false-panic shapes trip it anymore).
//! - **FIXED (was a silent miss, round 2; two further silent-miss gaps in
//!   the SAME check found and fixed round 3): a `Serialize` derive plus a
//!   hand-written `impl Deserialize` is no longer silently exempted —
//!   including when that hand impl lives in a SIBLING file, or its header
//!   is wrapped across two physical lines.** Round 2: `container_derives`
//!   only ever read the derive ATTRIBUTE text — a type deriving
//!   `#[derive(Serialize)]` (no `Deserialize` named there) but ALSO
//!   carrying a hand-written `impl Deserialize for T` elsewhere in the
//!   SAME file is genuinely deserializable, and used to get exemption
//!   route 3 anyway; round 2 added `file_hand_impls_deserialize_for`
//!   (a token-based, single-line scan for `impl ... Deserialize ... for
//!   <the container's own type name>` in the current file) to deny it.
//!   Round 3 found this itself admitted, but never accounted for, two
//!   further silent misses in the SAME check: (1) it was FILE-scoped — a
//!   type's struct definition and its hand-written `impl Deserialize` are
//!   routinely split across `types.rs` and a sibling
//!   `deserialize_impl.rs`-shaped module, legal and orphan-rule-sound
//!   Rust the file-scoped check alone cannot see; and (2) it required a
//!   SINGLE-LINE impl header, missing one wrapped across two physical
//!   lines (idiomatic rustfmt output once the single-line form runs
//!   long). Both fixed: `every_skip_serializing_if_field_is_default_option_or_write_only`
//!   now builds a CRATE-wide (every file under the same sweep root) hand-
//!   impl type set once per root and `scan_lines` denies the exemption
//!   when EITHER that set or the current file's own local scan names the
//!   type, and `collect_hand_deserialize_impls` (which both the file-
//!   local and crate-wide forms are built on) tries a header on its own
//!   line and joined with the line after it, catching a two-line-wrapped
//!   header too (a header wrapped across three or more lines is still not
//!   recognized — narrower, explicitly admitted, no real site wraps that
//!   far today). Pinned by
//!   `a_serialize_derive_plus_hand_written_deserialize_impl_is_not_exempt`
//!   (and its twin, `..._stays_exempt`, with no hand-impl present, round
//!   2), `a_hand_written_deserialize_impl_in_a_sibling_file_of_the_same_crate_is_recognized`
//!   and `a_two_line_hand_written_deserialize_impl_header_is_recognized`
//!   (round 3). No real site in the swept crates hand-implements
//!   `Deserialize` today, in one file or split across two (verified by
//!   grep for `impl.*Deserialize.*for`).
//!
//!   **Cost-checked (round 3's own self-QA gate):** the crate-wide hand-
//!   impl set is real added work — reading every file's content once
//!   (not twice; an interim version that called the file-read pass a
//!   second time per root measured a real ~10x regression, 0.10s to
//!   1.07s, fixed by caching) and a light per-line substring prefilter
//!   before the allocate-heavy header tokenizer (an unconditional
//!   tokenize-every-line pass alone measured over a second; the prefilter
//!   brought it back down). Net measured cost of this round's ADDED
//!   crate-wide scan: ~0.08s on top of the round-2 baseline (0.10s →
//!   ~0.18s for `every_skip_serializing_if_field_is_default_option_or_write_only`
//!   alone) — real, proportional to genuinely new work, and reported
//!   rather than left silently in the walker's own runtime.
//! - **A manually-written `impl Deserialize` with NO `#[derive(...)]` at
//!   all** — as opposed to the fixed case above, which is `derive(Serialize)`
//!   PLUS a hand-impl — is still not specifically recognized as deserializable
//!   by name; but since exemption route 3 requires a derive attribute to be
//!   FOUND at all before granting anything, a type with no derive
//!   whatsoever is already conservatively treated as NOT proven safe and
//!   flagged if it also carries an unguarded `skip_serializing_if` — the
//!   safe direction either way (a possible false finding on a genuinely
//!   inert type, never a silent miss on a genuinely deserializable one).
//! - **FIXED (loud false finding, round 2; the fix itself then created a
//!   real silent miss, round 3): `default` written on its own line BEFORE
//!   `skip_serializing_if`, directly adjacent (no blank/code line
//!   between), is recognized — and a PRECEDING same-line field's own
//!   `default` can no longer leak into a different, later field's
//!   check.** Round 2: the field-merge in `scan_lines` previously only
//!   walked FORWARD from the `skip_serializing_if` block, so this
//!   ordering was invisible to the `default` check and got wrongly
//!   flagged despite being a real, safe split-attribute pair; round 2
//!   added a BACKWARD walk through directly-adjacent sibling blocks,
//!   chaining through any number of them, using the same "all-blank-
//!   between" adjacency rule as the forward walk. Round 3 found that rule
//!   itself insufficient once the same-line attribute-and-field form
//!   (below) exists: a PRECEDING field's own attribute-and-field line
//!   (`#[serde(default)] pub level: u8,`) has its block's `end` on the
//!   SAME line as its field, so the gap between that `end` and the NEXT
//!   field's block `start` can be vacuously empty with no separate line
//!   for the "all blank between" check to see — the backward walk would
//!   merge that unrelated field's `default` in anyway, silently clearing
//!   a genuine finding on a field that has no `default` of its own. Fixed
//!   by also checking whether the candidate block carries its OWN
//!   same-line field (via `attr_block`'s `trailing`) — if it does, it
//!   belongs to a DIFFERENT field entirely and the walk stops rather than
//!   merging it. Pinned by
//!   `default_written_before_skip_serializing_if_on_an_adjacent_line_is_recognized`
//!   and (proving the adjacency bound still holds)
//!   `a_default_attribute_separated_by_a_real_line_is_not_merged_backward`
//!   (round 2), and
//!   `a_precedings_same_line_fields_default_does_not_leak_into_the_next_fields_check`
//!   (round 3).
//! - **FIXED (was a silent miss, round 3): the SAME-LINE attribute-and-
//!   field form — `#[serde(...)] pub field: T,`, all on one physical
//!   line — is now attributed to its OWN field, not whatever line
//!   happened to follow.** Not adversarial: an instrumented sweep of the
//!   real tree found 432 skip blocks, 76 declaring their field this way,
//!   ALL 76 in `darkmux-types/src/config.rs` — the config-schema file
//!   this whole scan exists to protect, a 17.6% blind spot in precisely
//!   the wrong place. The old forward field-search always started at
//!   `end + 1`, one line PAST the block's own end, so it walked past the
//!   field sharing that line and searched (and usually found nothing
//!   real) further down, silently. `attr_block` now returns the same-line
//!   trailing code (if any) alongside the block, and `scan_lines`
//!   recognizes a trailing text containing `:` as the field declaration
//!   directly rather than walking past it — for BOTH the block a
//!   `skip_serializing_if` lives on directly, and a sibling block reached
//!   via the forward walk. Re-verified against the real tree with a
//!   corrected scan: zero live findings in `config.rs` today — nothing
//!   was hiding behind this blind spot, but nothing was PROTECTING it
//!   either, which is the point. Pinned by
//!   `a_same_line_attribute_and_field_is_attributed_to_its_own_field_not_the_next_line`
//!   (the misattribution itself),
//!   `a_same_line_attribute_and_field_with_default_present_is_not_flagged`
//!   and `a_same_line_attribute_and_field_bare_option_is_exempt` (the safe
//!   directions still work).
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
//! - **STILL OPEN (admitted, not fixed — and its failure direction was
//!   WRONG until round 3, so read this bullet's direction claim as
//!   corrected here, not as originally written): raw string literals and
//!   `/* */` block comments carrying attribute-shaped text are scanned as
//!   if they were real code.** `code_only` strips `//` line comments and
//!   (as of round 3) tracks ORDINARY double-quoted strings, but has no
//!   concept of a raw string (`r#"..."#`) or a `/* ... */` block comment,
//!   both of which can span multiple physical lines and legitimately
//!   CONTAIN text that looks exactly like a real
//!   `#[serde(skip_serializing_if = "...")]` field — the realistic case
//!   the round-2 reviewer named is a `#[cfg(test)] mod tests` block inside
//!   a `src/` file holding a Rust source snippet as a raw-string test
//!   fixture (this very test file does exactly that, for its own
//!   self-tests — though `tests/` isn't itself swept). A correct fix
//!   needs a real lexer pass (raw-string and nested-block-comment
//!   tracking at the WHOLE-FILE level, with care not to also misread `/*`
//!   or a bare `r"` appearing inside an ORDINARY quoted string) — a
//!   meaningfully larger and riskier change than anything else in this
//!   list, and one a hand-rolled partial version could get subtly wrong
//!   in ways worse than the status quo. Not attempted in this pass.
//!
//!   **The failure direction round 2 claimed for this gap — "a false
//!   finding, never a silent miss" — was itself wrong, per the round-3
//!   review (MUST FIX 4).** A raw string carrying attribute-shaped text
//!   can desync `code_only`/`attr_block`'s simple (non-raw-string-aware)
//!   quote tracking and trip the unclosed-block panic — a THIRD
//!   direction, neither a false finding nor a silent miss: it can ABORT
//!   THE ENTIRE CONFORMANCE TEST. That panic direction is correct and
//!   deliberately kept (see the "genuinely unclosed" bullet above) — a
//!   scan that can't parse a construct must say so loudly rather than
//!   guess — but its MESSAGE no longer asserts one specific wrong cause
//!   (the round-3-proven bug: it named "a `//` inside a URL" even when
//!   the actual cause was a raw string, or an unbalanced bracket with
//!   nothing to do with comments); it now names its own honest limits and
//!   lists candidates instead. Re-verify by hand (`grep -rln 'r#"'
//!   crates/*/src src | xargs grep -l 'skip_serializing_if\|#\[serde'`) if
//!   a swept `src/` file starts embedding serde-attribute-shaped text in a
//!   raw string or block comment; none does today.
//! - **`runtime/` is deliberately NOT swept.** It is not a Cargo workspace
//!   member (its own `Cargo.toml`, built into the `darkmux-runtime` Docker
//!   image, needs its own `cargo clippy --manifest-path
//!   runtime/Cargo.toml` per this repo's own convention) and this scan's
//!   crate discovery walks `crates/` only. Checked by hand instead
//!   (re-verified 2026-09-10, round 3 — the earlier hand-count here was
//!   itself off by one and is corrected below, an off-by-one caught by
//!   re-running the check rather than trusting the prior write-up):
//!   `grep -rn skip_serializing_if runtime/src` returns 26 LINES, but TWO
//!   of those are comments a plain grep can't distinguish from a real
//!   attribute — `compaction.rs:2953` (a test's own `//` comment
//!   explaining a field's serialization behavior) and
//!   `loop_runner.rs:12127` (a `//!` module-doc line naming the same
//!   attribute on a DIFFERENT type by prose, not declaring it) — so there
//!   are 24 REAL field lines. Of those, 23 are `Option::is_none` on
//!   `Option<T>` fields with no `deserialize_with` anywhere near them
//!   (exemption route 2, safe) and the remaining one — `ChatRequest::tools`
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

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The CODE half of a line — everything before its first `//` THAT SITS
/// OUTSIDE a double-quoted string literal. A line starting with `///` (a
/// doc comment) or a bare `//` comment therefore code-only's to an EMPTY
/// string, which is exactly what lets the backward derive-scan and the
/// forward field-scan treat doc comments the same as blank lines without
/// any special-casing.
///
/// **Quote-aware (2026-09-10 review fix — "the guard fires on legal,
/// idiomatic code").** A naive `line.find("//")` treats a `//` inside a
/// quoted string VALUE (a URL in `#[doc = "See https://example.com/spec"]`
/// or `#[arg(long, help = "Base URL, e.g. http://localhost:1234")]` — both
/// real, common, idiomatic shapes; the command-layer files alone carry 236
/// such attribute lines) as a comment start, truncating the line before
/// the attribute's real closing `)]` — which used to blind `attr_block`'s
/// bracket-depth counter and trip its unclosed-block panic on ordinary,
/// correct code. This tracks double-quote parity (with backslash-escape
/// awareness, matching `strip_quoted_content` below) as it scans and only
/// treats `//` as a comment start when NOT inside a string. Chosen over
/// the alternative the reviewer also offered — narrowing the unclosed-
/// block guard to only the attributes the scan cares about — because this
/// fix is smaller (one primitive, already needed by `strip_quoted_content`
/// for the same reason) and it also fixes `attr_block`'s bracket-depth
/// counter for a string VALUE that itself contains an unbalanced `[`/`]`
/// (e.g. `#[doc = "index with arr[0"]`, nothing to do with comments —
/// see `attr_block`'s own per-line depth scan, which reuses the same
/// quote-tracking). Does NOT attempt raw strings (`r#"..."#`) or `/* */`
/// block comments — those remain the admitted, still-open limit named in
/// the module doc; a raw string's embedded, unescaped quotes can still
/// desync this simple tracker, which is exactly why that gap is harder
/// and deliberately not attempted here.
fn code_only(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            return &line[..i];
        }
        i += 1;
    }
    line
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
/// Before this fix, `code_only` truncated a line at its first `//` with no
/// string-literal awareness, so a `//` inside an attribute's own string
/// VALUE silently left the block "open" and the scan would consume every
/// remaining line of the file looking for a `]` that would never balance —
/// every later attribute in the file silently unscanned, with the
/// assertion staying green. `code_only` is now quote-aware (see there) and
/// no longer trips on that specific shape. This guard stays regardless:
/// a raw string (`r#"..."#`) or a `/* */` block comment carrying
/// attribute-shaped text can still desync the simple quote-tracking both
/// `code_only` and this function's own depth counter use (the admitted,
/// still-open limit named in the module doc), and if that happens the
/// scan must fail LOUDLY rather than silently drop coverage. This is
/// EXACTLY the direction the rest of this module's known limits promise
/// never happens.
///
/// **Depth counting is also quote-aware (2026-09-10 review fix).** A `[`
/// or `]` appearing INSIDE a quoted string value (e.g. `#[doc = "index
/// with arr[0"]` — an unbalanced bracket inside an ordinary attribute
/// string, nothing to do with comments) used to be counted toward the
/// bracket depth like real code, throwing off the close detection. This
/// reuses the same double-quote (with backslash-escape) tracking as
/// `code_only` and `strip_quoted_content` — carried ACROSS physical lines
/// of the same block, not reset per line, so a bracket inside a string
/// never affects when the block is considered closed, even when the
/// string value itself spans multiple lines via Rust's `\`-newline string
/// continuation (a real shape in this tree —
/// `config_access.rs`'s `#[must_use = "... \` / continued text / `..."]`
/// attribute closes its string on its LAST physical line; a per-line reset
/// of the quote state misread that closing `"` as opening a NEW string,
/// swallowing the attribute's real closing `]` and reintroducing an
/// unclosed-block panic on ordinary, correct code — caught by running this
/// fix against the real swept tree, not assumed).
///
/// **Captures same-line trailing text (2026-09-10 review — MUST FIX 1/2).**
/// When the block's closing `]` is followed by more code on that SAME
/// physical line (the `#[serde(skip_serializing_if = "...")] pub tags:
/// Vec<String>,` form — real, common, and previously unrecognized: an
/// instrumented sweep of the real tree found 432 skip blocks, 76 of which
/// declare their field this way, ALL 76 in `darkmux-types/src/config.rs`
/// and ALL 76 misattributed to a different field by the old forward walk,
/// which started its search for "the field" one line past the block's
/// `end` — the returned text is trimmed to end at the closing `]`
/// (previously it included the whole rest of the line, conflating
/// attribute text with field-declaration text in every downstream
/// keyword/derive check), and everything after the bracket on that line is
/// returned separately as `trailing` so callers can recognize the
/// same-line field-declaration form directly instead of walking past it.
fn attr_block(lines: &[&str], start: usize, file_label: &str) -> (String, usize, Option<String>) {
    let mut depth: i32 = 0;
    let mut entered = false;
    let mut out = String::new();
    let mut end = start;
    let mut closed = false;
    let mut trailing: Option<String> = None;
    // `in_string`/`escaped` are declared OUTSIDE the per-line loop and
    // carried across physical lines deliberately — a real Rust string
    // literal can span multiple lines via `\`-newline continuation (see
    // the doc comment above), and resetting quote state at each new line
    // would misread that continuation's eventual closing `"` as opening a
    // fresh string.
    let mut in_string = false;
    let mut escaped = false;
    for (offset, line) in lines[start..].iter().enumerate() {
        let code = code_only(line);
        end = start + offset;
        let mut close_byte: Option<usize> = None;
        for (bi, ch) in code.char_indices() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '[' => {
                    depth += 1;
                    entered = true;
                }
                ']' => {
                    depth -= 1;
                    if entered && depth <= 0 && close_byte.is_none() {
                        close_byte = Some(bi + ch.len_utf8());
                    }
                }
                _ => {}
            }
        }
        match close_byte {
            Some(cb) => {
                out.push_str(&code[..cb]);
                out.push('\n');
                let rest = code[cb..].trim();
                if !rest.is_empty() {
                    trailing = Some(rest.to_string());
                }
                closed = true;
                break;
            }
            None => {
                out.push_str(code);
                out.push('\n');
            }
        }
    }
    assert!(
        closed,
        "{file_label}:{}: an attribute block starting here never closed before the end of the \
         scanned range. This scan reads source TEXT, not tokens, and its bracket-depth counter \
         is quote-aware only for ORDINARY double-quoted strings — a RAW string literal \
         (`r#\"...\"#`) or a `/* */` block comment carrying attribute-shaped text can still \
         desync it (the module doc's \"Known limits\" names this as a still-open gap), and so \
         can any other construct this simple text scan doesn't model. This message deliberately \
         does not assert a single specific cause — inspect the source starting at this line for \
         an unbalanced `[`/`]`, a raw string, or a block comment. Left unguarded, this would \
         silently consume every remaining line of the file as part of one never-closing block, \
         and every attribute after it would go unscanned with the assertion staying green — \
         exactly the silent-miss direction this scan promises never happens.",
        start + 1,
    );
    (out, end, trailing)
}

/// Every attribute block in `lines`, in file order, as `(start, end, text,
/// trailing)` — `trailing` is the same-line code (if any) that follows the
/// block's closing `]` on its own last physical line (see `attr_block`).
/// See `attr_block` for what `file_label` is for.
fn all_attr_blocks(lines: &[&str], file_label: &str) -> Vec<(usize, usize, String, Option<String>)> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if code_only(lines[i]).trim_start().starts_with("#[") {
            let (text, end, trailing) = attr_block(lines, i, file_label);
            blocks.push((i, end, text, trailing));
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
fn container_derives(
    lines: &[&str],
    blocks: &[(usize, usize, String, Option<String>)],
    decl_idx: usize,
) -> (bool, bool) {
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
    if let Some((b_start, _, b_text, _)) = blocks.iter().find(|(start, _, _, _)| *start == decl_idx) {
        consider(b_text, &mut found, &mut has_deserialize);
        boundary = *b_start;
    }

    let mut candidate = blocks.iter().rposition(|(_, end, _, _)| *end < boundary);
    while let Some(i) = candidate {
        let (b_start, b_end, b_text, _) = &blocks[i];
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

/// Token-based check (not a real parser) for whether `header_lines`,
/// joined, is a hand-written `impl ... Deserialize ... for <Type>` header:
/// splits the joined code on non-identifier characters (so lifetimes/
/// generics like `impl<'de> Deserialize<'de> for Scratch` don't defeat
/// it), and matches when the first token is `impl`, `Deserialize` appears
/// anywhere, and the token immediately after `for` names the type. Returns
/// that type name.
fn impl_deserialize_for_type(header_lines: &[&str]) -> Option<String> {
    let tokens: Vec<&str> = header_lines
        .iter()
        .flat_map(|l| code_only(l).split(|c: char| !c.is_alphanumeric() && c != '_'))
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.first() != Some(&"impl") {
        return None;
    }
    if !tokens.contains(&"Deserialize") {
        return None;
    }
    let for_idx = tokens.iter().position(|t| *t == "for")?;
    tokens.get(for_idx + 1).map(|s| s.to_string())
}

/// Every type named by a hand-written `impl ... Deserialize ... for
/// <Type>` header anywhere in `lines`, inserted into `out`.
///
/// **Header may now span two physical lines (2026-09-10 review — MUST FIX
/// 3, admitted gap #2).** `impl_deserialize_for_type` previously only ever
/// saw ONE line at a time, so a header wrapped across two lines (e.g.
/// `impl<'de> Deserialize<'de>\n    for Scratch {`, real, idiomatic
/// rustfmt output once the single-line form runs long) was invisible —
/// silently NOT recognized as a hand impl. This tries each line alone
/// first, then that line joined with the NEXT one, so a two-line-wrapped
/// header is now caught. A header wrapped across three or more lines is
/// still not recognized — a narrower, explicitly admitted remaining limit
/// (no real site in this tree wraps past two lines today; re-verify by
/// hand if one does).
///
/// **Cost-checked (2026-09-10 review's own self-QA gate).** This runs over
/// EVERY line of EVERY file in the crate (called once per sweep root, not
/// once per field), so a cheap substring reject BEFORE the allocate-heavy
/// tokenize-and-match matters: `impl_deserialize_for_type` splits and
/// collects a fresh `Vec<&str>` per call, and doing that unconditionally
/// for every line (the overwhelming majority of which are not, and never
/// start, an `impl` header) measured at over a full second added to the
/// tree-wide sweep — worse than the double-file-read this same fix pass
/// separately caught and fixed. A line that doesn't even contain the
/// substring `"impl"` cannot start (or, joined with its successor,
/// contain) a matching header, so it's skipped before any tokenizing;
/// measured back down to the ~0.10s baseline.
fn collect_hand_deserialize_impls(lines: &[&str], out: &mut HashSet<String>) {
    for i in 0..lines.len() {
        if !lines[i].contains("impl") {
            continue;
        }
        if let Some(name) = impl_deserialize_for_type(&lines[i..=i]) {
            out.insert(name);
            continue;
        }
        if i + 1 < lines.len() {
            if let Some(name) = impl_deserialize_for_type(&lines[i..=i + 1]) {
                out.insert(name);
            }
        }
    }
}

/// `file_hand_impls_deserialize_for(lines, type_name)`: true when `lines`
/// (a SINGLE file's lines) contains a hand-written `impl ... Deserialize
/// ... for <type_name>` header — i.e. `type_name` is deserializable via a
/// manual trait impl rather than (or in addition to)
/// `#[derive(Deserialize)]`. Thin wrapper over `collect_hand_deserialize_impls`
/// for the single-file, single-type-name callers.
///
/// The tree-wide sweep also needs the CRATE-wide, all-types form — MUST
/// FIX 3 admitted gap #1: a hand impl in a SIBLING file of the same crate
/// is a real, common, legal Rust shape this file-scoped check alone
/// cannot see, since a `#[serde(...)]`-derived type's own struct
/// definition and its hand-written `impl Deserialize` are routinely split
/// across `types.rs` and `deserialize_impl.rs`-shaped sibling modules
/// (orphan-rule sound: `impl ForeignTrait for LocalType` requires
/// `LocalType` to be local to the crate defining it, so a hand impl for a
/// type swept under one sweep root can only live in ANOTHER file under
/// that SAME root — never in a different crate). That form is built
/// directly in `every_skip_serializing_if_field_is_default_option_or_write_only`
/// from the same cached file reads the main scan already does — see that
/// test's own comment for why (an earlier version of this fix called
/// `collect_hand_deserialize_impls` from a SECOND full pass that
/// re-read every file under each root, taking the sweep from ~0.10s to
/// ~1.07s; measured, not assumed, and fixed by reading each file once).
fn file_hand_impls_deserialize_for(lines: &[&str], type_name: &str) -> bool {
    let mut found = HashSet::new();
    collect_hand_deserialize_impls(lines, &mut found);
    found.contains(type_name)
}

/// Strip the CONTENTS of double-quoted string literals from `text` (the
/// quotes themselves stay, so bracket/paren scanning elsewhere in this
/// file is unaffected) — used only by the keyword checks below, so a
/// value string that happens to CONTAIN a keyword substring
/// (`skip_serializing_if = "is_default"`, `rename = "default"`) doesn't
/// get mistaken for the real, unquoted `#[serde(default)]` /
/// `#[serde(..., deserialize_with = "...")]` keywords.
///
/// **Escape-aware (2026-09-10 review fix — MUST FIX 3, admitted gap #3).**
/// An earlier version toggled `in_string` on every literal `"`, with no
/// backslash-escape handling — so an escaped quote INSIDE a value
/// (`rename = "a \"quoted\" default"`) flipped string-parity early,
/// leaving the rest of the attribute misread as "outside a string" and
/// exposing whatever real-looking keyword text follows as if it were
/// genuine unquoted code. This now tracks a trailing backslash and does
/// not toggle `in_string` on an escaped `"`, matching the same tracking
/// `code_only` and `attr_block`'s depth counter use for the same reason.
fn strip_quoted_content(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                in_string = false;
                out.push(ch);
                continue;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            out.push(ch);
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
/// block panic (see there); the self-tests pass a stub. `crate_hand_impls`
/// names every type in the SAME crate (any file under the same sweep
/// root, not just this one — MUST FIX 3 admitted gap #1) with a
/// hand-written `impl ... Deserialize ... for <Type>`; self-tests that
/// don't need it pass an empty set (a hand impl WITHIN the synthetic
/// single-file test source is still caught, via the file-local check
/// below).
fn scan_lines(lines: &[&str], file_label: &str, crate_hand_impls: &HashSet<String>) -> Vec<(usize, String)> {
    let mut findings = Vec::new();
    let blocks = all_attr_blocks(lines, file_label);
    for (idx, (start, end, text, trailing)) in blocks.iter().enumerate() {
        let (start, end) = (*start, *end);
        if !text.contains("skip_serializing_if") {
            continue;
        }

        // The field this attribute (or block of attributes) belongs to.
        // Two shapes:
        //
        //   1. SAME-LINE form: `#[serde(skip_serializing_if = "...")] pub
        //      tags: Vec<String>,` — the field shares this block's own
        //      last physical line, captured as `trailing` by `attr_block`.
        //      This is the shape MUST FIX 1/2 (2026-09-10 review) closes:
        //      an instrumented sweep of the real tree found 432 skip
        //      blocks, 76 declaring their field this way (all 76 in
        //      `darkmux-types/src/config.rs`), and the OLD forward walk
        //      (which always started its search at `end + 1`, past this
        //      line) misattributed every one of them to whatever
        //      unrelated line happened to come next.
        //   2. The field is on a LATER line: the next non-blank,
        //      non-doc-comment, non-attribute line after this block —
        //      skipping any DIRECTLY ADJACENT sibling attribute blocks
        //      (e.g. a `#[cfg_attr(...)]` on the same field) along the
        //      way, and folding their text into the default/
        //      deserialize_with check too (see the module doc's "split
        //      field attributes" limit). A sibling block itself may ALSO
        //      use the same-line form (its own trailing text names the
        //      field), which stops the walk there.
        let mut combined = text.clone();
        let mut same_line_field: Option<String> = trailing.as_ref().filter(|t| t.contains(':')).cloned();
        let mut j = end;
        if same_line_field.is_none() {
            let mut jj = end + 1;
            loop {
                if jj >= lines.len() {
                    j = jj;
                    break;
                }
                let trimmed = code_only(lines[jj]).trim();
                if trimmed.is_empty() {
                    jj += 1;
                    continue;
                }
                if trimmed.starts_with("#[") {
                    let (sib_text, sib_end, sib_trailing) = attr_block(lines, jj, file_label);
                    combined.push_str(&sib_text);
                    if let Some(t) = sib_trailing.filter(|t| t.contains(':')) {
                        same_line_field = Some(t);
                        j = sib_end;
                        break;
                    }
                    jj = sib_end + 1;
                    continue;
                }
                j = jj;
                break;
            }
        }
        if same_line_field.is_none() && j >= lines.len() {
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
        //
        // **Guarded against a preceding block's OWN same-line field
        // (2026-09-10 review — the regression MUST FIX 1/2 also closes).**
        // A preceding block that itself carries a same-line field
        // (`#[serde(default)] pub level: u8,`) is NOT a sibling attribute
        // of the NEXT field — it's a complete, different field's own
        // attribute — even though the "all blank between" check below
        // would otherwise see a vacuously empty gap (the field text sits
        // on the attribute's OWN line, past its `end`, not on a separate
        // line the gap-check would ever inspect) and wrongly merge it.
        // Before this guard, exactly this shape let a preceding `default`
        // leak into an unrelated later field's check and silently clear a
        // genuine finding.
        let mut boundary = start;
        let mut prev_idx = idx;
        while prev_idx > 0 {
            let (prev_start, prev_end, prev_text, prev_trailing) = &blocks[prev_idx - 1];
            let prev_has_own_field = prev_trailing.as_ref().is_some_and(|t| t.contains(':'));
            if prev_has_own_field {
                break;
            }
            let all_blank_between = (*prev_end + 1..boundary).all(|li| code_only(lines[li]).trim().is_empty());
            if !all_blank_between {
                break;
            }
            combined.push_str(prev_text);
            boundary = *prev_start;
            prev_idx -= 1;
        }

        let field_line: String = match &same_line_field {
            Some(t) => t.clone(),
            None => code_only(lines[j]).to_string(),
        };
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
        // exemption). `has_keyword_outside_strings` matches the keyword as
        // a whole TOKEN, not a substring — pinned separately (a plain
        // substring check on the stripped text would also pass every
        // OTHER test in this file, which is exactly why that distinction
        // needs its own dedicated test rather than being taken on faith).
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
                    .map(|name| {
                        file_hand_impls_deserialize_for(lines, &name) || crate_hand_impls.contains(&name)
                    })
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
        // Read every file under this root ONCE and cache its lines — used
        // BOTH to build the crate-wide hand-impl set (MUST FIX 3 admitted
        // gap #1: a hand-written `impl Deserialize` for a type in a
        // SIBLING file of the same crate was previously invisible to the
        // file-scoped check alone) AND to run the per-file scan below.
        //
        // **Cost-checked (2026-09-10 round-3 review's own self-QA gate):**
        // an earlier version of this fix called `rust_files(&root)` +
        // `std::fs::read_to_string` a SECOND time per root (once to
        // collect hand impls, once to scan), which measured at 1.07s for
        // this one test — a real ~10x regression from the 0.10s baseline,
        // caused by re-reading every file in the sweep from disk twice.
        // Caching each file's content once (read from disk exactly once,
        // scanned twice — hand-impl collection and the field scan are
        // both cheap in-memory string work) measured back down to 0.10s.
        // Stored as one owned `String` per file (not a `Vec<String>` of
        // per-line clones) so `.lines()` can cheaply re-borrow `&str`
        // slices from it twice with no extra per-line allocation.
        let files: Vec<(PathBuf, String)> = rust_files(&root)
            .into_iter()
            .map(|file| {
                let src = std::fs::read_to_string(&file)
                    .unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
                (file, src)
            })
            .collect();

        let mut crate_hand_impls = HashSet::new();
        for (_, src) in &files {
            let lines: Vec<&str> = src.lines().collect();
            collect_hand_deserialize_impls(&lines, &mut crate_hand_impls);
        }

        for (file, src) in &files {
            let lines: Vec<&str> = src.lines().collect();
            let label = file.display().to_string();
            for (line, field_text) in scan_lines(&lines, &label, &crate_hand_impls) {
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
            total += all_attr_blocks(&lines, &label)
                .iter()
                .filter(|(_, _, t, _)| t.contains("skip_serializing_if"))
                .count();
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
    let findings = scan_lines(&lines, "<test>", &HashSet::new());
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
        scan_lines(&lines, "<test>", &HashSet::new()).is_empty(),
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
    assert!(scan_lines(&lines, "<test>", &HashSet::new()).is_empty(), "a bare Option field with no deserialize_with must be exempt (route 2)");
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
        scan_lines(&lines, "<test>", &HashSet::new()).len(),
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
        scan_lines(&lines, "<test>", &HashSet::new()).is_empty(),
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
        scan_lines(&lines2, "<test>", &HashSet::new()).is_empty(),
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
    let findings = scan_lines(&lines, "<test>", &HashSet::new());
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
    let findings = scan_lines(&lines, "<test>", &HashSet::new());
    assert_eq!(
        findings.len(),
        1,
        "the enum variant's field must resolve to `Scratch` (which derives Deserialize) as its \
         container, not to the preceding Serialize-only `Unrelated` struct — found {findings:?}"
    );
}

/// MUST FIX 1 (2026-09-10 review), REVISED after the 2026-09-10 round-3
/// review: `code_only` and `attr_block`'s bracket-depth counter are now
/// BOTH quote-aware (see their doc comments), so a `//` inside an
/// attribute's own string VALUE — the ORIGINAL planted shape here — no
/// longer blinds anything; it is now covered instead by
/// `a_url_containing_a_double_slash_inside_a_doc_string_does_not_falsely_unbalance_the_attribute_scan`
/// below, proving the false panic is GONE. This test now proves the loud-
/// panic-on-genuinely-unclosed guard still fires for what it's actually
/// for: a block that has no closing `]` in the source AT ALL (a malformed
/// or truncated file, not a string/comment misparse) must still panic
/// loudly rather than silently consume the rest of the file — including
/// the very next field's unguarded `skip_serializing_if`.
#[test]
fn an_attribute_with_no_closing_bracket_at_all_panics_instead_of_silently_skipping_the_rest_of_the_file() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty"
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan_lines(&lines, "<planted>", &HashSet::new())));
    assert!(
        result.is_err(),
        "an attribute block with no closing `]` anywhere in the scanned range must panic loudly \
         (naming the file and line) instead of silently skipping the unguarded `items` field \
         that follows it"
    );
    let msg = result.unwrap_err();
    let msg = msg.downcast_ref::<String>().map(String::as_str).unwrap_or("<non-string panic payload>");
    assert!(
        msg.contains("<planted>") && msg.contains("never closed"),
        "the panic must name the file label and explain the block never closed, got: {msg}"
    );
}

/// Twin of the above, in the OTHER order: the missing closing bracket sits
/// in an attribute AFTER all real fields in the file (so nothing
/// downstream would have been silently skipped) must still be recognized
/// as unclosed and panic — the guard isn't allowed to depend on there
/// being a later victim field to reveal it.
#[test]
fn a_trailing_attribute_with_no_closing_bracket_still_panics_with_nothing_downstream_to_miss() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub items: Vec<String>,
}

#[cfg_attr(test, doc = "see https://example.com/x"
fn helper() {}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan_lines(&lines, "<planted-tail>", &HashSet::new())));
    assert!(result.is_err(), "an unclosed trailing attribute must panic even with no field after it to miss");
}

/// MUST FIX 4 / "Also fix — the guard fires on legal, idiomatic code"
/// (2026-09-10 round-3 review): `#[doc = "See https://example.com/spec"]`
/// is real, common, idiomatic Rust (and the reviewer's exact proof case)
/// — the `//` inside the doc-string URL must NOT be misread as a comment
/// start by `code_only`, must NOT blind `attr_block`'s bracket-depth
/// counter, and must NOT panic. This is the loud false-panic MUST FIX 4
/// named as "proven"; it is fixed here (rather than merely message-
/// corrected) by making `code_only`'s comment detection quote-aware — the
/// reviewer's own suggested, smaller fix, chosen over narrowing the guard
/// to fewer attributes because it reuses a primitive
/// (`strip_quoted_content`'s escape-aware quote tracking) this file
/// already needed for another reason, rather than adding a second,
/// narrower special case.
#[test]
fn a_url_containing_a_double_slash_inside_a_doc_string_does_not_falsely_unbalance_the_attribute_scan() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
#[doc = "See https://example.com/spec"]
pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan_lines(&lines, "<planted>", &HashSet::new())));
    assert!(
        result.is_ok(),
        "a `//` inside an ordinary doc-string URL value must not blind the attribute scan or \
         panic — got {:?}",
        result.err()
    );
    assert_eq!(
        result.unwrap().len(),
        1,
        "the scan must still correctly find and flag `items`'s unguarded skip_serializing_if \
         after correctly parsing through the doc-string URL"
    );
}

/// Twin of the above with the OTHER real, proven shape named by the
/// review: a command-line `#[arg(long, help = "...")]` attribute whose
/// help text contains a URL — the command-layer files carry 236 such
/// attribute lines, and this exact URL (`http://localhost:1234`) is a
/// documented darkmux default appearing in the repo's own instructions.
/// One help-string edit must not be able to hard-fail this conformance
/// test in an unrelated crate.
#[test]
fn a_help_string_containing_a_url_does_not_falsely_unbalance_the_attribute_scan() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[arg(long, help = "Base URL, e.g. http://localhost:1234")]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan_lines(&lines, "<planted>", &HashSet::new())));
    assert!(
        result.is_ok(),
        "a `//` inside a `help = \"...\"` URL value must not blind the attribute scan or panic \
         — got {:?}",
        result.err()
    );
    assert_eq!(result.unwrap().len(), 1, "the scan must still correctly flag `items`'s unguarded skip_serializing_if");
}

/// MUST FIX 4's second proven case: an unbalanced `[` inside an ordinary
/// (non-raw) attribute string VALUE, nothing to do with comments — proven
/// by the review to also panic, blaming the wrong cause. The bracket-depth
/// counter's quote-awareness (shared with `code_only`) fixes this the same
/// way: a `[`/`]` inside a tracked string is not counted toward depth.
#[test]
fn an_unbalanced_bracket_inside_a_quoted_attribute_string_does_not_break_the_depth_counter() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
#[doc = "index with arr[0"]
pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan_lines(&lines, "<planted>", &HashSet::new())));
    assert!(
        result.is_ok(),
        "an unbalanced `[` inside an ordinary quoted attribute string must not desync the \
         bracket-depth counter or panic — got {:?}",
        result.err()
    );
    assert_eq!(result.unwrap().len(), 1, "the scan must still correctly flag `items`'s unguarded skip_serializing_if");
}

/// A real shape found in THIS tree by the round-3 review while confirming
/// the depth-counter fix against the actual swept crates:
/// `config_access.rs`'s `#[must_use = "... \` continued across several
/// physical lines via Rust's `\`-newline string continuation, closing its
/// quote only on the LAST line. A per-LINE reset of the quote-tracking
/// state (an earlier draft of this fix) misread that final closing `"` as
/// OPENING a new string, swallowed the attribute's real closing `]`, and
/// reintroduced an unclosed-block panic on this ordinary, correct,
/// already-shipped code. The fix carries `in_string`/`escaped` state
/// across physical lines within one block scan instead of resetting them
/// each line.
#[test]
fn a_string_literal_continued_across_multiple_lines_via_backslash_newline_does_not_break_the_depth_counter() {
    let src = "\n\
#[derive(Debug, Clone, Serialize, Deserialize)]\n\
#[must_use = \"line one \\\n\
              line two\"]\n\
pub struct Scratch {\n\
    #[serde(skip_serializing_if = \"Vec::is_empty\")]\n\
    pub items: Vec<String>,\n\
}\n";
    let lines: Vec<&str> = src.lines().collect();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan_lines(&lines, "<planted>", &HashSet::new())));
    assert!(
        result.is_ok(),
        "a string literal continued across physical lines via `\\`-newline must not desync the \
         quote-aware depth counter or panic — got {:?}",
        result.err()
    );
    assert_eq!(result.unwrap().len(), 1, "the scan must still correctly flag `items`'s unguarded skip_serializing_if");
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
    let findings = scan_lines(&lines, "<test>", &HashSet::new());
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
        scan_lines(&lines, "<test>", &HashSet::new()).is_empty(),
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
    let findings = scan_lines(&lines, "<test>", &HashSet::new());
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
    let findings = scan_lines(&lines, "<test>", &HashSet::new());
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
        scan_lines(&lines, "<test>", &HashSet::new()).is_empty(),
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
        scan_lines(&lines, "<test>", &HashSet::new()).is_empty(),
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
    let findings = scan_lines(&lines, "<test>", &HashSet::new());
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
        scan_lines(&lines, "<test>", &HashSet::new()).is_empty(),
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
        scan_lines(&lines, "<test>", &HashSet::new()).is_empty(),
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
    let findings = scan_lines(&lines, "<test>", &HashSet::new());
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
        scan_lines(&lines, "<test>", &HashSet::new()).is_empty(),
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
        scan_lines(&lines, "<test>", &HashSet::new()).is_empty(),
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
    let findings = scan_lines(&lines, "<test>", &HashSet::new());
    assert_eq!(
        findings.len(),
        1,
        "a multi-line `#[cfg_attr(...)]` between the real derive and the struct must not break \
         the backward derive lookup — `Scratch` derives Deserialize on the line above it and its \
         unguarded field must be flagged — found {findings:?}"
    );
}

/// MUST FIX 1/2 (2026-09-10 round-3 review): the SAME-LINE attribute-and-
/// field form — `#[serde(...)] pub field: T,` all on one physical line.
/// An instrumented sweep of the real tree found 432 skip blocks, 76
/// declaring their field this way (all 76 in
/// `darkmux-types/src/config.rs`), and the OLD forward walk — which
/// always started its search for "the field" one line PAST the block's
/// own end — misattributed every one of them to whatever line happened to
/// come next, silently. This plants the shape directly: `tags`'s
/// `skip_serializing_if` shares its own physical line with the field
/// declaration, and a DIFFERENT field (`level`) sits on the line above.
/// The old code would attribute `tags`'s block to whatever came after
/// `tags`'s own line (nothing, here — EOF) and silently find nothing;
/// the fix must find exactly one violation, correctly pointing at `tags`.
#[test]
fn a_same_line_attribute_and_field_is_attributed_to_its_own_field_not_the_next_line() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(default)] pub level: u8,
    #[serde(skip_serializing_if = "Vec::is_empty")] pub tags: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let findings = scan_lines(&lines, "<test>", &HashSet::new());
    assert_eq!(
        findings.len(),
        1,
        "the same-line attribute-and-field form must be caught exactly once — found {findings:?}"
    );
    assert!(
        findings[0].1.contains("tags"),
        "the finding must point at `tags`'s OWN line (the same-line attribute-and-field form), \
         not some unrelated line the old forward walk stumbled onto — got {findings:?}"
    );
}

/// The regression MUST FIX 1/2 also closes: fixing the same-line form via
/// a NAIVE backward-merge guard could let a PRECEDING same-line field's
/// own `default` attribute leak into a later, unrelated field's check,
/// because the gap between that preceding block's `end` (which sits on
/// the SAME line as the field it belongs to) and the next block's `start`
/// can be vacuously empty — there is no separate blank/code line between
/// them to notice, since the field IS the "gap". `names` here is a
/// genuine violation (no `default` of its own) and must stay flagged;
/// `level`'s `default`, on the line above, must not be able to clear it.
#[test]
fn a_precedings_same_line_fields_default_does_not_leak_into_the_next_fields_check() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cfg {
    #[serde(default)] pub level: u8,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let findings = scan_lines(&lines, "<test>", &HashSet::new());
    assert_eq!(
        findings.len(),
        1,
        "`names` carries no `default` of its own and must stay flagged — `level`'s same-line \
         `default` (a different field entirely) must not leak into `names`'s check just because \
         the gap between them is vacuously empty — found {findings:?}"
    );
    assert!(findings[0].1.contains("names"), "the finding must point at `names`, got {findings:?}");
}

/// Twin, safe direction: the SAME same-line field form, but genuinely
/// safe — `default` and `skip_serializing_if` both on the ONE shared
/// attribute-and-field line. Proves the same-line fix isn't just
/// always-flagging.
#[test]
fn a_same_line_attribute_and_field_with_default_present_is_not_flagged() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub tags: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    assert!(
        scan_lines(&lines, "<test>", &HashSet::new()).is_empty(),
        "a same-line attribute-and-field pair that already carries `default` must not be flagged"
    );
}

/// Twin, route-2 direction: the same-line form on a bare `Option` field —
/// no `default` needed at all.
#[test]
fn a_same_line_attribute_and_field_bare_option_is_exempt() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Option::is_none")] pub maybe: Option<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    assert!(
        scan_lines(&lines, "<test>", &HashSet::new()).is_empty(),
        "a same-line attribute-and-field pair on a bare Option field must stay exempt (route 2)"
    );
}

/// MUST FIX 3, admitted gap #1 (2026-09-10 round-3 review): a
/// hand-written `impl Deserialize for Scratch` living in a SIBLING file of
/// the SAME crate — legal, common Rust (and orphan-rule-sound: a foreign
/// trait impl for a local type can only live in the crate that defines the
/// type). The file-scoped `file_hand_impls_deserialize_for` alone cannot
/// see it; `scan_lines`'s `crate_hand_impls` parameter must.
#[test]
fn a_hand_written_deserialize_impl_in_a_sibling_file_of_the_same_crate_is_recognized() {
    let src = r#"
#[derive(Debug, Clone, Serialize)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();

    // Without the cross-file set: still exempt (nothing wrong with the
    // CURRENT file alone — this is the honest baseline, not the bug).
    assert!(
        scan_lines(&lines, "<test-a.rs>", &HashSet::new()).is_empty(),
        "with no known cross-file hand impl, a Serialize-only type must still get the \
         write-only exemption"
    );

    // A sibling file (`sibling.rs`, elsewhere in the SAME crate) hand-
    // implements Deserialize for Scratch.
    let sibling_src = r#"
impl<'de> Deserialize<'de> for Scratch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        unimplemented!()
    }
}
"#;
    let sibling_lines: Vec<&str> = sibling_src.lines().collect();
    let mut crate_hand_impls = HashSet::new();
    collect_hand_deserialize_impls(&sibling_lines, &mut crate_hand_impls);

    let findings = scan_lines(&lines, "<test-a.rs>", &crate_hand_impls);
    assert_eq!(
        findings.len(),
        1,
        "once the crate-wide hand-impl set names `Scratch` (found in the SIBLING file), the \
         write-only exemption must be denied — `Scratch` is genuinely deserializable via that \
         sibling impl — found {findings:?} instead"
    );
}

/// MUST FIX 3, admitted gap #2 (2026-09-10 round-3 review): a
/// hand-written `impl Deserialize for T` header wrapped across TWO
/// physical lines (idiomatic rustfmt output once the single-line form
/// runs long) must still be recognized, not just the single-line form.
#[test]
fn a_two_line_hand_written_deserialize_impl_header_is_recognized() {
    let src = r#"
#[derive(Debug, Clone, Serialize)]
pub struct Scratch {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

impl<'de> Deserialize<'de>
    for Scratch
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        unimplemented!()
    }
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let findings = scan_lines(&lines, "<test>", &HashSet::new());
    assert_eq!(
        findings.len(),
        1,
        "a hand-written `impl Deserialize for Scratch` header wrapped across two physical lines \
         must still deny the write-only exemption — found {findings:?} instead"
    );
}

/// MUST FIX 3, admitted gap #3 (2026-09-10 round-3 review): an ESCAPED
/// quote inside an attribute string value must not flip the quote-parity
/// tracking `strip_quoted_content` (and the `default`/`deserialize_with`
/// keyword checks built on it) rely on. `rename = "a \"default\" value"`
/// contains an escaped `"default"` INSIDE the value — the real,
/// unescaped `default` keyword is genuinely absent, so this field must
/// still be flagged.
#[test]
fn an_escaped_quote_inside_an_attribute_string_does_not_flip_quote_parity() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(rename = "a \"default\" value", skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let findings = scan_lines(&lines, "<test>", &HashSet::new());
    assert_eq!(
        findings.len(),
        1,
        "an escaped quote inside `rename`'s value must not desync quote-parity tracking and \
         expose the escaped \"default\" text as if it were the real, unquoted keyword — found \
         {findings:?} instead"
    );
}

/// Twin of the above at the `strip_quoted_content` unit level — direct,
/// narrower proof that escaped quotes don't flip parity, independent of
/// the field-scan plumbing above.
#[test]
fn strip_quoted_content_does_not_toggle_on_an_escaped_quote() {
    let text = r#"rename = "a \"default\" value", skip_serializing_if = "Vec::is_empty""#;
    let stripped = strip_quoted_content(text);
    assert!(
        !has_keyword_outside_strings(&stripped, "default"),
        "an escaped quote inside the value must not flip string parity and expose the escaped \
         \"default\" text as an unquoted keyword — stripped form was: {stripped:?}"
    );
}

/// "Also fix — half of one fix is unpinned" (2026-09-10 round-3 review):
/// `has_keyword_outside_strings` matches the real keyword as a WHOLE
/// TOKEN, not a substring — but no existing test actually distinguished
/// that from a plain substring check on the stripped text (the review's
/// own proof: swapping in a substring check left all 24 prior tests
/// green). An unquoted, bare identifier merely CONTAINING "default" as a
/// substring (`not_default` — not a real serde option, but this is a
/// source-text scan, not a compiler, and doesn't need one) must not be
/// mistaken for the real `default` keyword.
#[test]
fn an_unquoted_token_merely_containing_the_default_keyword_as_a_substring_is_not_mistaken_for_the_real_keyword() {
    let src = r#"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratch {
    #[serde(not_default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
"#;
    let lines: Vec<&str> = src.lines().collect();
    let findings = scan_lines(&lines, "<test>", &HashSet::new());
    assert_eq!(
        findings.len(),
        1,
        "`not_default` is a bare unquoted TOKEN, not the real `default` keyword — a substring \
         check would wrongly grant the exemption (all 24 prior tests stayed green under a \
         substring check, which is exactly why this needed its own dedicated pin) — found \
         {findings:?} instead"
    );
}

/// Direct unit-level twin of the above, isolating `has_keyword_outside_strings`
/// itself from the field-scan plumbing.
#[test]
fn has_keyword_outside_strings_requires_a_whole_token_not_a_substring() {
    assert!(
        !has_keyword_outside_strings("not_default, skip_serializing_if", "default"),
        "`not_default` must not be mistaken for the whole-token keyword `default`"
    );
    assert!(
        has_keyword_outside_strings("not_default, default, skip_serializing_if", "default"),
        "a REAL, separately-present whole-token `default` alongside `not_default` must still \
         be recognized"
    );
}
