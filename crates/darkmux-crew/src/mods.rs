//! (#2265) The MOD record: how something could change.
//!
//! A mod is a **kit**: instructions plus data, in whatever form the proposer
//! chose — a diff, a sentence, pixel data, a config value — enough for an AI
//! to make the change correctly later, given the mod's own context. **darkmux
//! never types a kit and never opens it.**
//!
//! *Verbatim* means BYTE-EXACT, which is why the kit is always a string and is
//! never parsed on write. An earlier version parsed a JSON-looking kit and
//! re-serialized it; that silently collapsed duplicate keys and rounded large
//! integers through `f64`, so the stored kit was not the kit that was written.
//! A kit is not darkmux's data to normalize. `kit_looks_json` is a reader
//! HINT computed at write time — it says a parse succeeded once, and promises
//! nothing about what a reader will get back.
//!
//! **The key is MINTED per mod, never derived from a finding.** Two agents
//! review the same finding at different times; one proposes the code change,
//! the other recommends a comment. Both are valid, and they may overlap,
//! conflict or compose. A finding-derived key would have made the second
//! overwrite the first, so the record keeps both and judges neither.
//!
//! `for` is the only stored link between the two records: zero or more
//! finding keys, living on the thing created later. The view from a finding to
//! its mods is DERIVED by scanning mods — nothing is written back onto the
//! finding, which is an event and is never rewritten.
//!
//! `for` keys are CANONICALIZED on create (`<dispatch>/<seq>`, the seq
//! renumbered), so `sess-a/01` and `sess-a/1` are one address. One finding has
//! to have one address, or a mod is attached to a finding by one reader and
//! invisible to another. A key that can address no finding at all is refused
//! loudly rather than stored as a link nothing can follow.
//!
//! For each `for` key that exists in the finding store, the mod copies that
//! finding's `mission_id`, `context` and `emitted` into its own `context`, so
//! a reader of the mod never has to go find the finding. A `for` key with no
//! stored finding is allowed and recorded as `{key, missing: true}` — the mod
//! is still the change someone proposed.
//!
//! **That copy is a SNAPSHOT taken at create time.** It is what makes the mod
//! self-describing, and it is also the limit: a mod created before its finding
//! was synced records that finding as missing and carries no mission, so
//! `mod list --mission` will not see it. The snapshot is never refreshed —
//! rewriting it would make the mod a mutable view of a record that is itself
//! an event.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The record's own schema version. Bumped when the shape below GAINS or
/// changes a field; readers stay lenient in both directions (every added
/// field is `Option`/`default`, and anything a reader does not know rides
/// in `extras`), so a version here is a fact a reader may act on, never a
/// gate it must pass.
///
/// **`"1"` → `"2"` (#2310 swarm F / S2-2b).** The rule above said "bumped
/// when the shape changes" and the shape had changed four times without
/// one: `kit_kind` (#2310 P4b/P4c), `source` (#2361), `gate` and
/// `gate_skipped_reason` (#2310 P4c-2b). Each was additive and each doc
/// comment said "so the schema version does not move" — which is a
/// different rule than the one the constant declared, and two rules for
/// one number is how a version stops meaning anything. Resolved in the
/// direction that keeps the number honest: the shape moved, so the number
/// moves, and the rule is restated as "gains or changes a field".
///
/// The format has no minor slot on purpose — leniency, not the version, is
/// what makes an old reader work. A `"1"` record still reads: the four
/// fields above are `Option`/`default`, so a pre-#2310 mod comes back with
/// `kit_kind: None`, `source: None`, `gate: None`,
/// `gate_skipped_reason: None`, which is exactly what those absences mean
/// ("no hint", "already in repo coordinates", "not gated"). Pinned by
/// `a_schema_version_1_record_still_reads_with_every_added_field_absent`.
pub const MOD_SCHEMA_VERSION: &str = "2";

/// The runtime tool whose accepted calls become mods. Its finding sibling
/// needs a LIST (the pre-2026-09-03 `report_finding` is in the append-only
/// stream forever); this tool was born with its name, so there is one.
pub const MOD_TOOL_NAME: &str = "create_mod";

/// Whether a mod key is safe to use as a PATH SEGMENT under the store.
///
/// Minted keys always are, but a key also arrives from the operator (`mod
/// show <key>`), so it is untrusted input that ends up joined onto a
/// filesystem path. Anything that could resolve outside the store is refused
/// rather than sanitized — the same rule, for the same reason, as a finding's
/// dispatch segment.
pub fn is_safe_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('.')
        && !key.contains('/')
        && !key.contains('\\')
        && !key.contains('\0')
}

/// Whether an attachment file name is safe to write inside a mod's own
/// `attachments/`.
///
/// Deliberately WEAKER than [`is_safe_key`]: a key is a directory name darkmux
/// mints and controls, so it refuses a leading dot; a file name is the
/// proposer's, and `.env.example` is an ordinary attachment. Only what could
/// escape the directory is refused — a separator, `.`, `..`, an empty name.
pub fn is_safe_basename(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// Mint a key for one mod: `mod-<unix-secs>-<6 hex>`.
///
/// Same shape and same token scheme as `mission_launch::mint_run_id` — a
/// blake3 digest over (nanosecond time, pid, an in-process atomic counter), so
/// two mods minted within the same wall-clock second are still distinct. The
/// seconds prefix keeps the store browsable in rough chronological order;
/// ordering itself comes off the record's `ts`, never the key.
pub fn mint_key() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let digest = blake3::hash(format!("{nanos}-{pid}-{n}").as_bytes());
    let hex = digest.to_hex();
    format!("mod-{}-{}", nanos / 1_000_000_000, &hex.as_str()[..6])
}

/// One `for` finding, as copied onto the mod at create time. Either the
/// finding's own provenance (it was in the store) or a `missing` marker (it
/// was not) — never nothing, so a reader can always tell the two apart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ForFinding {
    /// The finding key exactly as the proposer named it.
    pub key: String,
    /// The finding's own `mission_id`, when it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    /// The finding's `context` verbatim (the launcher's blob).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
    /// The finding's `emitted` verbatim (the model's own arguments). Copied,
    /// never read: darkmux does not interpret an emission here either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitted: Option<serde_json::Value>,
    /// `true` when no finding with this key was in the store at create time.
    /// A mod for an unstored finding is allowed — the change was still
    /// proposed — so this is a recorded fact, not a refusal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub missing: bool,
}

/// What the mod carries about the findings it names, so a reader of the mod
/// never has to go find them.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModContext {
    #[serde(default)]
    pub findings: Vec<ForFinding>,
}

/// (#2310 P4c-2b) The result of running a review's confirmation gate — a
/// `test_command` — against one mod's targets. DESIGN.md "the changed
/// files name the test targets, which is what makes confirmation cheap
/// enough to do per finding". Written once, by [`record_gate`], never
/// re-run: a mod is a moment someone proposed a change, and confirming it
/// is a moment too.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GateOutcome {
    /// `true` — the mod's kit applied AND the command exited `0` against
    /// the PATCHED checkout. `false` — the kit failed to apply (see
    /// [`Self::applied`]/[`Self::reason`]), or the command ran against the
    /// patched checkout and exited nonzero. A run that could not even be
    /// ATTEMPTED (a missing workdir, an unspawnable command) is never
    /// represented by this type at all — that is `ModRecord::
    /// gate_skipped_reason`'s job (a genuine infrastructure failure is a
    /// SKIP, never a false "the mod's change is bad" answer).
    pub passed: bool,
    /// The command that ran, verbatim, so a gated mod is self-describing —
    /// a reader never has to go find `review-v2.json`'s own `test_command`
    /// input to know what confirmed (or failed to confirm) this mod.
    pub command: String,
    /// The process's own exit code, when the command actually ran to
    /// completion against a patched checkout. `None` when it never ran at
    /// all — the kit failed to apply, so there was nothing to test.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// (#2310 P4c-2b PR #2357 review MUST FIX A) Whether the mod's kit was
    /// actually applied to a scratch checkout before `command` ran —
    /// `false` means `command` never ran (the kit didn't apply); `true`
    /// means it did, and `passed` reflects the command's own exit. `None`
    /// only for pre-fix records (additive field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<bool>,
    /// A human-readable reason for `passed: false` when the failure isn't
    /// simply "the command exited nonzero" — e.g. `"kit did not apply"`.
    /// `None` for an ordinary command-exit outcome (pass OR fail).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One mod, as stored at `<mods dir>/<key>/mod.json`.
///
/// **The stored shape, at [`MOD_SCHEMA_VERSION`] `"2"`** (#2310 swarm F /
/// S2-2b — the doc used to stop at the `"1"` set, so three surfaces gained
/// fields nothing here named):
///
/// - `key`, `ts`, `by`, `for`, `kit`, `kit_looks_json`, `attachments`,
///   `context`, `warnings`, `schema_version` — the `"1"` set.
/// - `mission_id` / `phase_id` / `step_id` — the dispatch that proposed it.
/// - `kit_kind` — an optional hint at the kit's SHAPE (`"unified-diff"`,
///   or absent). Still opaque by contract; it exists so a consumer that
///   chooses to interpret a kit has a signal to key on rather than
///   sniffing bytes.
/// - `source` (#2361) — the workspace source id whose `/workspace/<source>/`
///   prefix was mapped off this kit's headers. Absent when nothing was
///   mapped.
/// - `gate` / `gate_skipped_reason` (#2310 P4c-2b) — the confirmation
///   gate's outcome, or why none ran. Mutually exclusive; `record_gate`
///   refuses to set both.
///
/// Every one of those is `Option`/`default` on read, which is what makes a
/// `"1"` record still parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModRecord {
    /// The minted key — the address every other surface uses.
    pub key: String,
    /// When the RECORD was written.
    pub ts: String,
    /// Who proposed it. A free actor string: a role handle plus model for a
    /// darkmux seat, or a plain name (`sonnet`, `kain`) for an external actor.
    /// Deliberately not a typed enum — the proposer may be anything, and a
    /// closed set would refuse the next one.
    pub by: String,
    /// The findings this mod addresses — zero or more keys. The only stored
    /// link between the two records, and a LIST because one change can
    /// address three observations.
    #[serde(rename = "for", default)]
    pub r#for: Vec<String>,
    /// The kit, BYTE-EXACT, always a string, never parsed. `None` when no
    /// kit text was given and the attachments are the whole kit — which is a
    /// different fact from a kit whose text is empty, so the two are not
    /// collapsed. A kit whose text is `null` is the four characters `null`.
    pub kit: Option<String>,
    /// A reader HINT: the kit text parsed as JSON at write time. It is not a
    /// promise and nothing in darkmux acts on it — the kit is handed on as
    /// bytes either way.
    #[serde(default)]
    pub kit_looks_json: bool,
    /// (#2310 P4b review, M-B; producer wiring in P4c) An OPTIONAL hint at
    /// the kit's shape — `"unified-diff"` today, nothing else recognized
    /// yet. Still opaque by contract (darkmux never opens a kit): this is
    /// not a parse, and it is never re-derived once written. Two producers,
    /// two ways it gets set: `mods::create` (the CLI path) writes it
    /// VERBATIM from whatever `--kit-kind` names, proposer-declared and
    /// unvalidated; `create_from_emission` (the runtime `create_mod`
    /// path) has no such argument to receive — the tool's wire schema is
    /// `for`/`kit`/`attach` only — so it sets it mechanically via
    /// `looks_like_unified_diff` instead. Either way it exists so a
    /// CONSUMER that DOES choose to interpret a kit
    /// (`deliver_github_review`'s suggestion-block rendering) has an
    /// explicit signal to key on, rather than sniffing the kit text itself
    /// — an opaque kit pasted verbatim into a GitHub suggestion block
    /// corrupts the PR the moment its shape isn't literally "the
    /// replacement text for one anchored line" (a unified diff's
    /// `+++`/`@@` lines are not that, and a multi-line kit silently
    /// duplicates the lines below the anchor). `None` (a kit that matches
    /// neither producer's detection) means "render as an opaque fenced
    /// patch, never a suggestion." Additive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kit_kind: Option<String>,
    /// Basenames of the files under this mod's `attachments/`.
    #[serde(default)]
    pub attachments: Vec<String>,
    /// Each `for` finding's own provenance, copied at create time.
    #[serde(default)]
    pub context: ModContext,
    /// What was WRONG with the parts of this mod that could not be kept — an
    /// attachment that did not decode, a `for` key that addressed no finding.
    ///
    /// The record is written ANYWAY when any part fails, because the kit is
    /// the product: a dispatch that spent its run producing a change must not
    /// lose it to a malformed sibling field. The problem rides here so the
    /// mod is honest about being partial instead of silently looking whole.
    /// Absent (not `[]`) on a mod with nothing wrong.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The mission / phase / step the DISPATCH that proposed this mod ran
    /// under — `null` for `mod create` (an external actor belongs to no
    /// dispatch) and for a plain `darkmux dispatch`. Top-level for the same
    /// reason a finding's are: `context` is the findings' copied provenance,
    /// and darkmux does not write its own ids into a blob it copied.
    ///
    /// Additive (`Option` reads a record written before them as `None`), so
    /// the schema version does not move.
    #[serde(default)]
    pub mission_id: Option<String>,
    #[serde(default)]
    pub phase_id: Option<String>,
    #[serde(default)]
    pub step_id: Option<String>,
    /// (#2361) The workspace source id the proposing dispatch's `context`
    /// named — the id whose prefix [`strip_kit_source_prefix`] mapped off
    /// this kit's headers. Present so nothing is lost by the mapping: the
    /// container path is `/workspace/<source>/<path>`, reconstructible
    /// from the two. `None` for `mod create` (an external actor's change,
    /// already in the repo's own coordinates) and for any dispatch with no
    /// source in its context, neither of which is mapped. Additive, so the
    /// schema version does not move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// (#2310 P4c-2b) The confirmation gate — DESIGN.md "the changed files
    /// name the test targets, which is what makes confirmation cheap
    /// enough to do per finding": a `test_command` run against this mod's
    /// finding's targets. `None` when nothing has gated this mod yet OR
    /// when create-mods deliberately skipped gating — those two cases are
    /// told apart by [`Self::gate_skipped_reason`], never conflated.
    /// Additive (`#[serde(default)]`), so a record written before this
    /// field reads back `None` — the same read-only-required-fields rule
    /// every other additive field on this record follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateOutcome>,
    /// Why no gate ran, set the moment create-mods DECIDES to skip (no
    /// `test_command` configured) — distinct from `gate: None` meaning
    /// "not yet examined". `Some` and [`Self::gate`]`: Some` are mutually
    /// exclusive; [`record_gate`] refuses to set both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_skipped_reason: Option<String>,
    pub schema_version: String,
    /// Lenient-on-read overflow, so a newer writer's fields survive a round
    /// trip through an older reader.
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// What [`materialize`] did. Write-once, like a finding: a mod records a
/// moment someone proposed a change, and that moment is not edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Materialized {
    Created,
    AlreadyPresent,
}

/// Where a mod is assembled before it becomes visible. A dot name so it is
/// never a valid mod key ([`is_safe_key`] refuses a leading dot), which is
/// what keeps a half-staged mod from ever being read as a record.
pub const STAGING_DIR: &str = ".staging";

/// The store root: `env(DARKMUX_MODS_DIR) > config.dirs.mods > <darkmux
/// root>/mods`.
pub fn mods_dir() -> PathBuf {
    darkmux_types::config_access::mods_dir()
}

/// The directory one mod lives in.
pub fn record_dir_at(root: &Path, key: &str) -> PathBuf {
    root.join(key)
}

/// The file one mod lives at.
pub fn record_path_at(root: &Path, key: &str) -> PathBuf {
    record_dir_at(root, key).join("mod.json")
}

/// Where one mod's attachments are copied.
pub fn attachments_dir_at(root: &Path, key: &str) -> PathBuf {
    record_dir_at(root, key).join("attachments")
}

/// (#2295) Where a briefed mod's `attachments/` is bind-mounted inside the
/// dispatch container, READ-ONLY. Named ONCE, here, and used by both sides
/// that must agree: `dispatch_internal`'s docker argv builds the `-v` from it,
/// and [`brief_block`] tells the model the files are at it. A test asserting
/// the argv is not a test that the mount WORKS (#975) — one constant is what
/// keeps the two from drifting apart while both stay green.
pub const CONTAINER_MODS_BASE: &str = "/darkmux-mods";

/// The container path one mod's `attachments/` is mounted at.
pub fn attachments_container_dir(key: &str) -> String {
    format!("{CONTAINER_MODS_BASE}/{key}/attachments")
}

/// (#2295 review, NIT b) ONE definition of the term `mod`, used by BOTH the
/// mod block below and the finding block in `findings::brief_block`. Two
/// wordings for one term in one brief is a model-facing defect: a clean-context
/// model grounds the term on whichever it reads last, and nothing keeps the
/// two in step.
pub const MOD_TERM: &str =
    "a change someone proposed: instructions and/or data, enough for whoever applies it later";

/// (#2295 review, NIT c) The `dispatch --mod` flag's help text, formatted from
/// [`CONTAINER_MODS_BASE`] so the CLI cannot come to advertise a path the
/// mounts do not use.
pub fn dispatch_mod_flag_help() -> String {
    format!(
        "Append a stored mod's record to the brief — repeatable. The mod is the HOW (a change \
         someone already proposed); this hands the role the kit BYTE-EXACT and unparsed, plus \
         its attached files, which are bind-mounted read-only at {CONTAINER_MODS_BASE}/<key>/\
         attachments/ and named by that path in the block. A key with no stored mod is refused \
         loudly rather than dispatched with a silently missing brief — `darkmux mod list` shows \
         what is stored. When both flags are given, the finding blocks come first, then the mod \
         blocks, each in the order given."
    )
}

/// (#2295 review, IMPORTANT 3) A boundary token the kit's own bytes cannot
/// contain, so a kit carrying `</kit>` or `</mod>` cannot close its own block
/// early and orphan whatever follows.
///
/// Derived from the kit text (blake3, re-salted until the token does not occur
/// in it) rather than from a random source, so one kit always renders to one
/// byte-identical block — a random nonce would make every render of the same
/// mod differ, which is the wrong trade for a brief that ought to be
/// reproducible. Termination is not an assumption: each salt yields a fresh
/// 6-hex token, and a text containing every one of them cannot be written.
pub fn kit_boundary(text: &str) -> String {
    for salt in 0u32.. {
        let digest = blake3::hash(format!("{salt}\u{0}{text}").as_bytes());
        let token = format!("k-{}", &digest.to_hex().as_str()[..6]);
        if !text.contains(&token) {
            return token;
        }
    }
    unreachable!("a text cannot contain every possible boundary token")
}

/// Render one stored mod for a dispatch brief (`dispatch --mod <key>`).
///
/// **The kit goes in byte-exact and unparsed.** darkmux does not know what is
/// inside it — a patch, prose, JSON, a shell script — and the model reading
/// this is the one that does. `attachments_mount` is the container directory
/// the files are readable at (see [`attachments_container_dir`]); it is a
/// parameter rather than derived here so the block and the `docker run` argv
/// can be asserted against the SAME value in one test.
///
/// **Nothing load-bearing follows the kit.** The instructions sit ABOVE the
/// block and every other field ABOVE the kit, because a kit is arbitrary bytes
/// that may contain `</kit>` or `</mod>`: anything placed after it can be
/// pushed outside the block by its own content. The kit itself is fenced with
/// a content-derived boundary token ([`kit_boundary`]) it cannot contain, and
/// the token is named in the line that opens the fence.
///
/// XML-tagged with inline definitions of the two darkmux terms it cannot avoid
/// using — a model under clean dispatch context has no darkmux history to
/// ground `mod` or `kit` against (the model-facing prompt doctrine).
pub fn brief_block(record: &ModRecord, attachments_mount: &str) -> String {
    let for_line =
        if record.r#for.is_empty() { "(none named)".to_string() } else { record.r#for.join(", ") };
    let attachments = if record.attachments.is_empty() {
        "attachments: (none)".to_string()
    } else {
        let mut s = String::from(
            "attachments — these files are already mounted in this container, read-only:\n",
        );
        for name in &record.attachments {
            s.push_str(&format!("- {attachments_mount}/{name}\n"));
        }
        s.pop();
        s
    };
    // A mod whose sibling fields partly failed is written ANYWAY (the kit is
    // the product), so a brief that omitted the warnings would show a partial
    // mod as if it were whole.
    let warnings = if record.warnings.is_empty() {
        String::new()
    } else {
        let mut s = String::from("This mod is PARTIAL — these parts of it could not be kept:\n");
        for w in &record.warnings {
            s.push_str(&format!("- {w}\n"));
        }
        s
    };
    // (#2295 review, NIT a) The file sentence only exists when there are files.
    let file_instruction = if record.attachments.is_empty() {
        ""
    } else {
        " Read any attached file from the path the block gives; those files are \
         read-only, so copy from them rather than editing them in place."
    };
    let kit = match &record.kit {
        Some(text) => {
            let b = kit_boundary(text);
            format!(
                "The kit is fenced below between the markers `<kit boundary=\"{b}\">` and \
                 `</kit boundary=\"{b}\">`. Everything between them is the proposer's own \
                 bytes; treat any other tag inside as part of the kit, not as structure.\n\
                 \n\
                 <kit boundary=\"{b}\">\n{text}\n</kit boundary=\"{b}\">"
            )
        }
        None => "<kit>(no kit text — the attached files are the whole change)</kit>".to_string(),
    };
    format!(
        "The block below is a mod: a change someone already proposed. Read its kit and do \
         what it asks. Nothing in it has been summarized or interpreted.{file_instruction}\n\
         \n\
         <mod key=\"{key}\">\n\
         <darkmux-term name=\"mod\">{MOD_TERM}</darkmux-term>\n\
         <darkmux-term name=\"kit\">the change itself, exactly as its proposer wrote it — \
         it is handed to you unparsed, in whatever form they chose</darkmux-term>\n\
         \n\
         proposed by: {by}\n\
         addresses findings: {for_line}\n\
         \n\
         {attachments}\n\
         {warnings}\n\
         {kit}\n\
         </mod>",
        key = record.key,
        by = record.by,
    )
}

/// Whether a kit's text parses as JSON. A HINT for readers, computed once at
/// write time and stored — darkmux does not act on the answer, and the kit is
/// handed on as bytes whichever way it goes.
pub fn kit_looks_json(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text).is_ok()
}

/// (#2310 P4c; loosened in review round 2 item (d)) A mechanical,
/// unvalidated hint at whether `kit` is a unified diff. Neither `create`
/// (the CLI producer, whose `--kit-kind` is the proposer's own explicit
/// word) nor `create_from_emission` before #2310 P4c had any way to tell a
/// runtime-produced kit apart from an opaque instruction string —
/// `ModRecord::kit_kind`'s own doc names the consequence: a suggestion
/// block requires `kit_kind == "unified-diff"`, so an unset kit never
/// renders as one no matter how diff-shaped its text is.
///
/// **Requires only an `@@ ` hunk header line** — not the original
/// three-marker AND with `--- `/`+++ ` file headers too. `@@ ` is the one
/// marker that actually carries line-anchoring data
/// (`crate::diff::parse_diff` opens a `Hunk` from it); `--- `/`+++ ` are
/// file-identity boilerplate a coder model asked for "the exact diff" for
/// ONE file commonly omits, and their absence was a real false negative
/// this function's own doc already claimed to avoid but didn't — a kit
/// that is just a hunk plus its +/- lines is still, honestly, a unified
/// diff. `parse_diff` itself still needs `+++ b/<path>` to bind a hunk to
/// a file path, so a header-less kit still falls back to the opaque
/// fenced-patch bullet in practice (`deliver_github_review::
/// render_gated_mod`'s `hunks.is_empty()` branch) — this function only
/// changes SELF-DESCRIPTION accuracy, not rendering behavior, for that
/// case. (#2310 fix-loop B1/R2: `+++ <path>` binds with the `b/` prefix
/// OPTIONAL, so a `--no-prefix` or `diff -u` kit header binds too — but
/// the header still has to be PRESENT, and it is recognized by POSITION
/// (paired with `--- `, or with no open hunk around it) so that a `+++`
/// line INSIDE a hunk stays content.) Deliberately loose either way: a false positive just means a
/// malformed diff renders as a fenced patch once its parse comes back
/// empty; a false negative is the failure mode worth avoiding.
pub fn looks_like_unified_diff(kit: &str) -> bool {
    kit.lines().any(|l| l.starts_with("@@ "))
}

/// One finding, one address. `sess-a/01` and `sess-a/1` name the same finding,
/// so the seq is renumbered and the pair rejoined — the form every reader
/// compares against. A key that resolves to no `<dispatch>/<seq>` at all can
/// address no finding and is refused by the caller.
pub fn canonical_finding_key(key: &str) -> Option<String> {
    let (dispatch, seq) = crate::findings::parse_key(key)?;
    Some(format!("{dispatch}/{seq}"))
}

/// Canonicalize every `for` key, refusing one that can address no finding.
/// Loud, because such a key would otherwise be stored as a link nothing could
/// ever follow.
pub fn canonical_for_keys(for_keys: &[String]) -> Result<Vec<String>> {
    for_keys
        .iter()
        .map(|k| {
            canonical_finding_key(k).with_context(|| {
                format!("not a finding key: {k:?} (expected <dispatch>/<seq>, e.g. sess-abc/1)")
            })
        })
        .collect()
}

/// Copy each named finding's provenance onto the mod. Keys must already be
/// canonical (see [`canonical_for_keys`]).
pub fn finding_context(findings_root: &Path, for_keys: &[String]) -> Result<ModContext> {
    let mut findings = Vec::new();
    for key in for_keys {
        let stored = match crate::findings::parse_key(key) {
            None => None,
            Some((dispatch, seq)) => crate::findings::load_at(findings_root, &dispatch, seq)?,
        };
        findings.push(match stored {
            Some(f) => ForFinding {
                key: key.clone(),
                mission_id: f.mission_id.clone(),
                context: Some(f.context.clone()),
                emitted: Some(f.emitted.clone()),
                missing: false,
            },
            None => ForFinding {
                key: key.clone(),
                mission_id: None,
                context: None,
                emitted: None,
                missing: true,
            },
        });
    }
    Ok(ModContext { findings })
}

/// Write a mod **once**. A minted key never collides, so an existing file
/// means something else is already at that address — it is left exactly as it
/// is, the same contract a finding gets.
pub fn materialize(root: &Path, record: &ModRecord) -> Result<Materialized> {
    anyhow::ensure!(
        is_safe_key(&record.key),
        "refusing to write a mod under an unsafe key {:?}",
        record.key
    );
    let path = record_path_at(root, &record.key);
    if path.exists() {
        return Ok(Materialized::AlreadyPresent);
    }
    let dir = path.parent().expect("record path always has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("creating mod dir {}", dir.display()))?;
    let body = serde_json::to_string_pretty(record)? + "\n";
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(body.as_bytes())
                .with_context(|| format!("writing mod {}", path.display()))?;
            Ok(Materialized::Created)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(Materialized::AlreadyPresent),
        Err(e) => Err(e).with_context(|| format!("creating mod {}", path.display())),
    }
}

/// Record ONE mod's confirmation gate — the one deliberate exception to
/// [`materialize`]'s write-once discipline. A gate result is darkmux's OWN
/// annotation about an already-stored mod (never a competing proposal, and
/// never a rewrite of the kit itself), so this patches `gate`/
/// `gate_skipped_reason` onto the existing record in place. The exception
/// stays narrow, not a general "mods are mutable" door: exactly one of
/// `outcome`/`skipped_reason` is expected non-`None` (a caller passing
/// both, or neither, is a caller bug — `mods.gate`'s own step kind is the
/// only production caller and never does either), and a mod that already
/// carries a gate or a skip reason is left untouched, reported the same
/// [`Materialized::AlreadyPresent`] shape `materialize` uses for "this
/// call changed nothing" — a mod is gated at most once.
pub fn record_gate(
    root: &Path,
    key: &str,
    outcome: Option<GateOutcome>,
    skipped_reason: Option<&str>,
) -> Result<Materialized> {
    record_gate_with_source(root, key, outcome, skipped_reason, None)
}

/// [`record_gate`], also recording the source id the gate resolved when the
/// mod arrived without one. A create-mod dispatch carries no `context.source`,
/// so its kit is stored in container coordinates and nothing downstream could
/// map it: the gate is the first place the source is known (it is the
/// directory the kit was applied in), and the deliverer maps the kit by this
/// field before parsing. A `source` already on the record is never replaced.
pub fn record_gate_with_source(
    root: &Path,
    key: &str,
    outcome: Option<GateOutcome>,
    skipped_reason: Option<&str>,
    resolved_source: Option<&str>,
) -> Result<Materialized> {
    anyhow::ensure!(is_safe_key(key), "refusing to gate a mod under an unsafe key {key:?}");
    let path = record_path_at(root, key);
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading mod {}", path.display()))?;
    let mut record: ModRecord =
        serde_json::from_str(&raw).with_context(|| format!("parsing mod {}", path.display()))?;
    if record.gate.is_some() || record.gate_skipped_reason.is_some() {
        return Ok(Materialized::AlreadyPresent);
    }
    record.gate = outcome;
    record.gate_skipped_reason = skipped_reason.map(str::to_string);
    if record.source.is_none() {
        record.source = resolved_source.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    }
    let body = serde_json::to_string_pretty(&record)? + "\n";
    std::fs::write(&path, body).with_context(|| format!("writing gated mod {}", path.display()))?;
    Ok(Materialized::Created)
}

/// Mint a key, copy the attachments, and write one mod.
///
/// **Idempotence is not a goal.** Every call mints a new key: two agents
/// proposing for one finding at different times are two mods, and that is the
/// point.
#[allow(clippy::too_many_arguments)]
pub fn create(
    root: &Path,
    findings_root: &Path,
    by: &str,
    for_keys: &[String],
    kit: Option<&str>,
    attachments: &[PathBuf],
    // (#2310 P4b review, M-B) Proposer-declared, opaque hint at the kit's
    // shape — threaded straight to `ModRecord::kit_kind`, unvalidated
    // (see that field's own doc). `None` from every caller that predates
    // this parameter.
    kit_kind: Option<&str>,
) -> Result<ModRecord> {
    anyhow::ensure!(!by.trim().is_empty(), "a mod needs a proposer: pass --by <actor>");
    // A mod with neither instructions nor data is not a kit. Refused here
    // rather than in the CLI so both producers get the same floor.
    anyhow::ensure!(
        kit.is_some() || !attachments.is_empty(),
        "a mod needs a kit: pass --kit <file>|- and/or --attach <path>"
    );

    // Everything that can be checked without touching the store is checked
    // first; everything that CANNOT is staged (below), so no failure can leave
    // a half-written mod.
    let for_keys = canonical_for_keys(for_keys)?;
    let mut names: Vec<String> = Vec::new();
    for path in attachments {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| is_safe_basename(n))
            .with_context(|| {
                format!(
                    "attachment file name is not usable as a name inside the mod \
(a separator, `.`, `..` or empty): {}",
                    path.display()
                )
            })?
            .to_string();
        anyhow::ensure!(
            !names.contains(&name),
            "two attachments share the basename {name:?} — one would overwrite the other"
        );
        anyhow::ensure!(path.is_file(), "attachment is not a readable file: {}", path.display());
        names.push(name);
    }

    let key = mint_key();
    let record = ModRecord {
        key: key.clone(),
        ts: darkmux_flow::ts_utc_now(),
        by: by.to_string(),
        r#for: for_keys.clone(),
        // The bytes that came in, unchanged. No parse, no re-serialize.
        kit: kit.map(str::to_string),
        kit_looks_json: kit.is_some_and(kit_looks_json),
        kit_kind: kit_kind.map(str::to_string),
        attachments: names.clone(),
        context: finding_context(findings_root, &for_keys)?,
        warnings: Vec::new(),
        // `mod create` is the EXTERNAL producer: the change was made outside
        // darkmux, so there is no dispatch and no mission to name.
        mission_id: None,
        phase_id: None,
        step_id: None,
        // (#2361) An external actor's change is already written in the
        // repo's own coordinates — there is no container to map out of.
        source: None,
        // (#2310 P4c-2b) `mod create` is a one-shot CLI write, not part of
        // any create-mods gate loop — never gated at create time.
        gate: None,
        gate_skipped_reason: None,
        schema_version: MOD_SCHEMA_VERSION.to_string(),
        extras: serde_json::Map::new(),
    };

    stage_and_commit(root, &record, &|dest| {
        for (path, name) in attachments.iter().zip(&names) {
            std::fs::copy(path, dest.join(name))
                .with_context(|| format!("copying attachment {}", path.display()))?;
        }
        Ok(())
    })?;
    Ok(record)
}

/// Assemble one mod under `.staging`, then rename it into place.
///
/// Staged, then renamed, so the record and its attachments become visible
/// TOGETHER or not at all: a copy that fails halfway would otherwise persist a
/// write-once record listing a file that is not on disk — impossible to
/// complete, and a retry would mint a second key and leave the broken one
/// behind forever.
///
/// `write_attachments` is handed the staging `attachments/` directory and is
/// called only when the record names any. It is the ONLY difference between
/// the two producers: `create` copies from host paths, `create_from_emission`
/// writes bytes that rode out of a container, where no host path exists.
fn stage_and_commit(
    root: &Path,
    record: &ModRecord,
    write_attachments: &dyn Fn(&Path) -> Result<()>,
) -> Result<()> {
    let key = &record.key;
    let staging_root = root.join(STAGING_DIR);
    let staging = staging_root.join(key);
    let staged = (|| -> Result<()> {
        // Attachments FIRST, then the record that names them, so the last
        // thing written inside the staging dir is the thing that makes it a
        // record at all.
        if !record.attachments.is_empty() {
            let dest = staging.join("attachments");
            std::fs::create_dir_all(&dest)
                .with_context(|| format!("creating attachments dir {}", dest.display()))?;
            write_attachments(&dest)?;
        }
        // A minted key never collides, so anything but `Created` means
        // something else already owns that address — an error, not a shrug,
        // because the alternative is attaching these files to another mod.
        anyhow::ensure!(
            materialize(&staging_root, record)? == Materialized::Created,
            "a mod already exists at the minted key {key} — refusing to write over it"
        );
        let final_dir = record_dir_at(root, key);
        anyhow::ensure!(
            !final_dir.exists(),
            "a mod already exists at {} — refusing to write over it",
            final_dir.display()
        );
        // Atomic within one filesystem, and staging lives under the store, so
        // it always is one.
        std::fs::rename(&staging, &final_dir).with_context(|| {
            format!("moving the staged mod into {}", final_dir.display())
        })?;
        Ok(())
    })();
    if staged.is_err() {
        // Nothing partial survives a failure — not the record, not the files.
        let _ = std::fs::remove_dir_all(&staging);
    }
    // Best-effort tidy; only succeeds when no other create is staging.
    let _ = std::fs::remove_dir(&staging_root);
    staged
}

/// One attachment as it arrived inside a runtime emission: the name it gets
/// inside the mod, and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineAttachment {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Decode standard base64. Inline rather than a dependency — the dep set is
/// deliberately small and this is the whole decoder. The encoder lives in the
/// runtime (`runtime/src/tools/mod.rs`), which cannot depend on this crate.
pub fn decode_b64(s: &str) -> Result<Vec<u8>> {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for c in s.bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = A
            .iter()
            .position(|a| *a == c)
            .with_context(|| format!("not base64: byte {c:?}"))? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

/// Map a kit's file headers out of the container's coordinates and into
/// the repo's, using the HOST-known workspace source id (#2361).
///
/// A dispatch sees its workspace at `/workspace/<source-id>/…`, and a model
/// asked for a unified diff writes the paths it can see: `--- a/<source-id>/
/// <path>`. `mods.gate` applies the kit inside a copy of the SOURCE
/// CHECKOUT, and `deliver.github_review` anchors a suggestion by the diff's
/// own repo-relative path — so a kit in container coordinates applies to
/// nothing and anchors to nothing. Proven live: every gated mod on mission
/// `review-v2-1788566897-9c149e` recorded "kit did not apply".
///
/// Only header forms are touched — `diff --git`, `---`, `+++`, and (#2310
/// fix-loop E2) the bare-path family `rename from`/`rename to`/`copy from`/
/// `copy to` — and only the `<source-id>` prefix (with or without git's
/// `a/`/`b/` marker, and with or without the `/workspace/` root).
/// `/dev/null` is left exactly as it is.
///
/// **How hunk bodies stay out of it, stated honestly.** EVERY line of the
/// kit is examined against those prefixes. There is no hunk parser here,
/// no state machine tracking whether we are inside a `@@` range, and
/// nothing that identifies a header STRUCTURALLY — so a body line is not
/// safe because it was recognized as a body line. It is safe because the
/// unified-diff format prefixes it: every body line carries a leading
/// `' '` (context), `'+'` (added), `'-'` (removed) or `'\'` (the
/// no-newline marker), which shifts its own content one column right so
/// it cannot begin `diff --git `, `rename from `, `copy to `, and so on.
///
/// That guard is not total, and the gap is worth naming rather than
/// papering over: a REMOVED line whose own content begins `-- ` renders
/// as `--- ` and is byte-identical to a `---` header (likewise content
/// beginning `++ ` under `+`). Nothing short of walking `@@` ranges can
/// tell those apart. What keeps it harmless is that the mapping below is
/// a no-op unless the path begins with THIS run's `<source-id>`, so a
/// collision must also start with that id before one byte changes. If
/// that ever stops being narrow enough, the fix is a real range walk, not
/// a longer comment.
///
/// **Idempotent for the MARKED forms, not for the bare ones.** An
/// `a/`/`b/`-prefixed header (what `git diff` emits, and what every kit
/// observed so far uses) round-trips: `a/src/x.ts` has no `app/` prefix
/// left to strip, so a second pass is a no-op. A BARE path cannot be told
/// apart from an already-repo-relative one when the repo has a top-level
/// directory named like the source id — `app/x.ts` is either "container
/// coordinates for source `app`" or "the repo's own `app/` directory", and
/// nothing in the header text distinguishes them. This function strips, so
/// the second reading is over-stripped, and repeating the pass strips
/// again. Deliberately NOT guessed around: the host knows the source id,
/// not the repo's top-level layout, and every available heuristic would
/// trade a rare wrong strip for a rare wrong keep. Pinned by
/// `the_bare_path_branch_cannot_tell_container_coords_from_a_top_level_dir_of_the_same_name`.
///
/// **This is the ONE place a stored kit is not byte-identical to the
/// emission** — a host translation between two coordinate systems the host
/// alone knows the mapping for, not an interpretation of the kit. The
/// source id rides on [`ModRecord::source`] so the original is
/// reconstructible.
/// (#2310 fix-loop E2) The git header lines that carry ONE bare path each
/// — no `a/`/`b/` marker, no `/dev/null` case, no trailing timestamp. Every
/// one of them needs the same mapping the `---`/`+++`/`diff --git` headers
/// get, or a renamed/copied file's kit describes two different trees.
const RENAME_COPY_HEADERS: [&str; 4] = ["rename from ", "rename to ", "copy from ", "copy to "];

pub fn strip_kit_source_prefix(source_id: &str, kit: &str) -> String {
    if source_id.is_empty() {
        return kit.to_string();
    }
    let map_path = |p: &str| -> String {
        for marker in ["a/", "b/"] {
            if let Some(rest) = p.strip_prefix(marker) {
                let mapped = crate::findings::strip_source_prefix(source_id, rest);
                return format!("{marker}{mapped}");
            }
        }
        crate::findings::strip_source_prefix(source_id, p)
    };
    let mut out = String::with_capacity(kit.len());
    for line in kit.split_inclusive('\n') {
        let (body, eol) = match line.strip_suffix('\n') {
            Some(b) => match b.strip_suffix('\r') {
                Some(b2) => (b2, "\r\n"),
                None => (b, "\n"),
            },
            None => (line, ""),
        };
        let mapped = if let Some(rest) = body.strip_prefix("diff --git ") {
            // Two paths, space-separated. A path with a space in it is not
            // representable in this header form anyway (git quotes it), and
            // a quoted one simply falls through unmapped rather than being
            // corrupted.
            let parts: Vec<&str> = rest.split(' ').collect();
            if parts.len() == 2 {
                format!("diff --git {} {}", map_path(parts[0]), map_path(parts[1]))
            } else {
                body.to_string()
            }
        } else if let Some((marker, path)) = RENAME_COPY_HEADERS.iter().find_map(|m| body.strip_prefix(*m).map(|r| (*m, r)))
        {
            // (#2310 fix-loop E2, from the loop-D review) A rename/copy
            // carries its two paths on their OWN header lines, unmarked by
            // `a/`/`b/`. Mapping the `diff --git` line while leaving these
            // behind produced a kit whose halves disagreed about where the
            // file lives, which `git apply` refuses outright — the mod then
            // recorded "kit did not apply" and a real change posted as a
            // double-check thread instead.
            format!("{marker}{}", map_path(path))
        } else if let Some(rest) = body.strip_prefix("--- ").or_else(|| body.strip_prefix("+++ ")) {
            let marker = &body[..4];
            // A `---`/`+++` header may carry a tab plus timestamp
            // (`diff -u` output); only the path half is mapped.
            let (path, tail) = match rest.find('\t') {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, ""),
            };
            if path == "/dev/null" {
                body.to_string()
            } else {
                format!("{marker}{}{tail}", map_path(path))
            }
        } else {
            body.to_string()
        };
        out.push_str(&mapped);
        out.push_str(eol);
    }
    out
}

/// The mod a DISPATCH proposed, from the runtime's emission.
///
/// The second producer of the same record: `create` is the external actor's
/// (`mod create`, a change made outside darkmux), this one is the runtime
/// tool's. Both mint a key, both stage before they commit, and both copy the
/// named findings' provenance — the difference is only where the attachments
/// come from (a path on the host vs bytes that rode the emission out of the
/// container, which no host path can reach).
// One record's worth of parts, each a distinct thing the record must carry;
// a struct here would only move the same list one line up (`create` above
// carries the same shape).
#[allow(clippy::too_many_arguments)]
pub fn create_from_emission(
    root: &Path,
    findings_root: &Path,
    by: &str,
    for_keys: &[String],
    kit: &str,
    attachments: &[InlineAttachment],
    scope: crate::findings::Scope,
    // (#2361) The workspace source id the proposing dispatch's
    // `record_context` named, when it named one — the prefix mapped off
    // this kit's headers. `None` leaves the kit byte-identical.
    source: Option<&str>,
    warnings: Vec<String>,
) -> Result<ModRecord> {
    anyhow::ensure!(!by.trim().is_empty(), "a mod needs a proposer");
    // The same floor `create` enforces, here too so neither producer can write
    // a mod that is not a kit. The runtime already refused an empty kit at the
    // tool boundary, where the model could read the refusal; this is the
    // backstop, not the message.
    anyhow::ensure!(!kit.trim().is_empty(), "a mod needs a kit: `kit` was empty");
    // (#2361) The HOST boundary: container coordinates in, repo
    // coordinates stored. A dispatch with no source in its context is not
    // mapped at all.
    let kit = match source {
        Some(id) => strip_kit_source_prefix(id, kit),
        None => kit.to_string(),
    };
    let for_keys = canonical_for_keys(for_keys)?;

    let mut names: Vec<String> = Vec::new();
    for a in attachments {
        anyhow::ensure!(
            is_safe_basename(&a.name),
            "attachment name is not usable inside the mod (a separator, `.`, `..` or empty): {:?}",
            a.name
        );
        anyhow::ensure!(
            !names.contains(&a.name),
            "two attachments share the name {:?} — one would overwrite the other",
            a.name
        );
        names.push(a.name.clone());
    }

    let key = mint_key();
    let record = ModRecord {
        key: key.clone(),
        ts: darkmux_flow::ts_utc_now(),
        by: by.to_string(),
        r#for: for_keys.clone(),
        // The bytes that came in — with the ONE host translation
        // `strip_kit_source_prefix` documents (#2361): the container's
        // `/workspace/<source>/…` headers become the repo-relative ones
        // every consumer of a kit speaks. No parse, no re-serialize.
        kit: Some(kit.to_string()),
        kit_looks_json: kit_looks_json(&kit),
        // (#2310 P4c) The runtime producer has no `kit_kind` argument the
        // model can set (the `create_mod` tool takes `for`/`kit`/`attach`
        // only — see `runtime/src/tools/mod.rs`), so this is detected
        // mechanically from the kit's own text rather than left `None`
        // forever, per `mods.rs`'s own #2310 P4b review note this replaces:
        // a review finding's mod never becomes a GitHub suggestion block
        // without this, no matter how diff-shaped the kit actually is.
        kit_kind: looks_like_unified_diff(&kit).then(|| "unified-diff".to_string()),
        attachments: names.clone(),
        context: finding_context(findings_root, &for_keys)?,
        warnings,
        mission_id: scope.mission_id,
        phase_id: scope.phase_id,
        step_id: scope.step_id,
        source: source.map(str::to_string),
        // (#2310 P4c-2b) Gated after the fact, by `mods.gate` — never at
        // creation time (the coder proposing the mod hasn't run any test
        // yet).
        gate: None,
        gate_skipped_reason: None,
        schema_version: MOD_SCHEMA_VERSION.to_string(),
        extras: serde_json::Map::new(),
    };

    stage_and_commit(root, &record, &|dest| {
        for a in attachments {
            std::fs::write(dest.join(&a.name), &a.bytes)
                .with_context(|| format!("writing attachment {:?}", a.name))?;
        }
        Ok(())
    })?;
    Ok(record)
}

/// Read one mod by its key.
pub fn load_at(root: &Path, key: &str) -> Result<Option<ModRecord>> {
    // Validated HERE rather than at the join: `mod show '../x'` must not read
    // outside the store.
    if !is_safe_key(key) {
        return Ok(None);
    }
    let path = record_path_at(root, key);
    if !path.exists() {
        return Ok(None);
    }
    let body =
        std::fs::read_to_string(&path).with_context(|| format!("reading mod {}", path.display()))?;
    let rec: ModRecord =
        serde_json::from_str(&body).with_context(|| format!("parsing mod {}", path.display()))?;
    // A record's key IS its address. If they disagree, the record cannot be
    // reached by what it claims to be, so it is not served under a name that
    // does not resolve.
    if !is_addressable(&rec, key) {
        return Ok(None);
    }
    Ok(Some(rec))
}

/// Whether a record read out of `<root>/<dir_name>` may be served: its `key`
/// is that directory, and every attachment name is a plain basename. Both are
/// checked BEFORE anything stats or opens an attachment path.
fn is_addressable(rec: &ModRecord, dir_name: &str) -> bool {
    rec.key == dir_name
        && is_safe_key(&rec.key)
        && rec.attachments.iter().all(|n| is_safe_basename(n))
}

/// Every mod in the store, ts-ascending. Unreadable or unparseable files are
/// skipped rather than failing the read — the same casual-reader contract the
/// finding store gets.
pub fn load_all_at(root: &Path) -> Result<Vec<ModRecord>> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Skips the staging dir for free, along with anything else that could
        // not be a mod's address.
        if !is_safe_key(&name) {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(entry.path().join("mod.json")) else {
            continue;
        };
        if let Ok(rec) = serde_json::from_str::<ModRecord>(&body) {
            if is_addressable(&rec, &name) {
                out.push(rec);
            }
        }
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.key.cmp(&b.key)));
    Ok(out)
}

/// The mods that name one finding — DERIVED by scanning mods, because nothing
/// about a mod is ever written back onto the finding it addresses.
///
/// The QUERY is canonicalized here, not only at the caller, so every reader
/// gets the same answer for `sess-a/01` and `sess-a/1`. Stored `for` keys are
/// canonical by construction, so canonicalizing only on write would leave one
/// finding with two addresses from the reader's side. A query that can address
/// no finding matches nothing — there is no stored key it could equal.
pub fn mods_for<'a>(all: &'a [ModRecord], finding_key: &str) -> Vec<&'a ModRecord> {
    let Some(key) = canonical_finding_key(finding_key) else {
        return Vec::new();
    };
    all.iter().filter(|m| m.r#for.contains(&key)).collect()
}

/// Whether a mod belongs to one mission. Answered from the mod ALONE — the
/// finding store need not still hold anything it names.
///
/// (#2310 fix-loop E2, S5-8/S3-4) Two sources, in this order:
///
/// 1. **The mod's own `mission_id`** — HOST-stamped by
///    [`create_from_emission`] from the proposing dispatch's scope, never
///    model-supplied. This is the authoritative answer to "which run made
///    this", and it is right even for a mod that cites no finding at all.
/// 2. **The copied `for`-finding provenance** — the fallback for a mod with
///    no run of its own (`mod create`, an external actor), which can only be
///    placed through the findings it names.
///
/// Consulting only (2) made a run-produced mod with an EMPTY `for` list
/// belong to no mission: invisible to `mod list --mission`, and — the real
/// cost — filtered out of `records.gather`'s own mission scan, so a change
/// the run had actually produced never reached the review it was made for.
pub fn names_mission(record: &ModRecord, mission: &str) -> bool {
    if record.mission_id.as_deref() == Some(mission) {
        return true;
    }
    record.context.findings.iter().any(|f| f.mission_id.as_deref() == Some(mission))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings;
    use tempfile::TempDir;

    /// (#2265) The runtime producer writes the SAME record the CLI producer
    /// does — a minted key, canonical `for` keys, the kit byte-exact, the
    /// findings' provenance copied — plus the dispatch's own scope, and its
    /// attachments come from BYTES rather than from a host path.
    #[test]
    fn a_mod_from_an_emission_carries_the_dispatch_scope_and_byte_identical_attachments() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("mods");
        let findings_root = tmp.path().join("findings");
        store_finding(&findings_root, "sess-a", 1, Some("m-9"));
        let bytes: Vec<u8> = (0u8..=255).collect();

        let rec = create_from_emission(
            &root,
            &findings_root,
            "coder (darkmux:qwen3.6)",
            &["sess-a/01".to_string()],
            "{\"n\": 12345678901234567890}",
            &[InlineAttachment { name: "blob.bin".into(), bytes: bytes.clone() }],
            findings::Scope {
                mission_id: Some("m-9".into()),
                phase_id: Some("p-1".into()),
                step_id: Some("step-3".into()),
            },
            None,
            vec!["a part the host could not keep".to_string()],
        )
        .unwrap();

        assert_eq!(rec.r#for, vec!["sess-a/1".to_string()], "the `for` key is canonicalized");
        assert_eq!(rec.by, "coder (darkmux:qwen3.6)");
        assert_eq!(rec.mission_id.as_deref(), Some("m-9"));
        assert_eq!(rec.phase_id.as_deref(), Some("p-1"));
        assert_eq!(rec.step_id.as_deref(), Some("step-3"));
        assert_eq!(rec.context.findings[0].mission_id.as_deref(), Some("m-9"));
        assert!(!rec.context.findings[0].missing);
        assert_eq!(
            rec.warnings,
            vec!["a part the host could not keep".to_string()],
            "what the host could not keep rides ON the record, not only on stderr"
        );

        let stored = load_at(&root, &rec.key).unwrap().expect("the mod is readable");
        assert_eq!(
            stored.kit.as_deref(),
            Some("{\"n\": 12345678901234567890}"),
            "the kit is byte-exact: never parsed, never re-serialized"
        );
        assert_eq!(stored.attachments, vec!["blob.bin".to_string()]);
        assert_eq!(
            std::fs::read(attachments_dir_at(&root, &rec.key).join("blob.bin")).unwrap(),
            bytes,
            "the attachment's bytes survive the round trip"
        );
        assert!(!root.join(STAGING_DIR).exists(), "nothing is left staged");
    }

    /// (#2310 P4c review round 2, item (d) — proven) The original
    /// three-marker AND required `--- `/`+++ `/`@@ ` all present, which was
    /// itself the false-negative failure mode this function's own doc says
    /// to avoid: a kit that is a single file's diff with no `diff --git`/
    /// `--- `/`+++ ` header boilerplate — just the `@@ ` hunk plus its
    /// +/- lines, a shape a coder model asked for "the exact diff" commonly
    /// emits — used to read as NOT a unified diff. `@@ ` is the one marker
    /// that actually carries line-anchoring data (`crate::diff::parse_diff`
    /// needs it to open a `Hunk` at all); `--- `/`+++ ` are file-identity
    /// boilerplate that parser also wants for PATH binding, but their
    /// absence does not make the hunk itself any less a unified diff.
    /// `looks_like_unified_diff` is now `@@ ` alone.
    #[test]
    fn looks_like_unified_diff_counts_a_hunk_with_no_file_headers() {
        // Edge 1 (must be true): only `@@ ` hunks, no `--- `/`+++ ` at all
        // — the shape this fix exists for.
        let hunk_only = "@@ -1,2 +1,2 @@\n-old\n+new\n";
        assert!(looks_like_unified_diff(hunk_only), "{hunk_only:?}");

        // Edge 2 (must stay false): file headers with NO `@@ ` hunk at
        // all — there is no line-anchoring data whatsoever, so this is
        // not a usable diff shape no matter how diff-flavored its
        // headers look.
        let headers_only = "--- a/f.rs\n+++ b/f.rs\n";
        assert!(!looks_like_unified_diff(headers_only), "{headers_only:?}");
    }

    /// The genuinely negative shapes: plain prose and JSON (`kit_looks_json`'s
    /// own case) — neither carries an `@@ ` hunk header under any reading.
    #[test]
    fn looks_like_unified_diff_rejects_plain_prose_and_json() {
        for not_a_diff in [
            "just replace this one line with that one line",
            "{\"n\": 1}",
        ] {
            assert!(!looks_like_unified_diff(not_a_diff), "{not_a_diff:?}");
        }
    }

    /// The full-header shape still counts too — this fix widens what
    /// counts, it does not narrow it.
    #[test]
    fn looks_like_unified_diff_still_counts_a_kit_with_full_headers() {
        let real = "--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n";
        assert!(looks_like_unified_diff(real), "{real:?}");
    }

    /// (#2310 P4c) `create_from_emission` — the runtime `create_mod` path,
    /// which has no `kit_kind` argument the model can set — now detects the
    /// unified-diff shape mechanically rather than writing `kit_kind: None`
    /// forever (the #2310 P4b review note this test guards against
    /// regressing): a review finding's mod would otherwise never become a
    /// GitHub suggestion block no matter how diff-shaped its kit was.
    #[test]
    fn an_emission_diff_shaped_kit_is_detected_as_unified_diff() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("mods");
        let findings_root = tmp.path().join("findings");
        store_finding(&findings_root, "sess-b", 1, None);

        let diff_kit = "--- a/f.rs\n+++ b/f.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let rec = create_from_emission(
            &root,
            &findings_root,
            "reviewer (darkmux:qwen3.6)",
            &["sess-b/1".to_string()],
            diff_kit,
            &[],
            findings::Scope::default(),
            None,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(rec.kit_kind.as_deref(), Some("unified-diff"), "{rec:?}");

        let plain = create_from_emission(
            &root,
            &findings_root,
            "reviewer (darkmux:qwen3.6)",
            &["sess-b/1".to_string()],
            "did you consider using the existing helper at src/util.rs instead?",
            &[],
            findings::Scope::default(),
            None,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(plain.kit_kind, None, "{plain:?}");
    }

    /// A kit that is empty, or two attachments that would collide, are refused
    /// before anything is written — the same floor `create` enforces.
    #[test]
    fn an_emission_mod_refuses_an_empty_kit_a_colliding_or_unsafe_attachment_name() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("mods");
        let findings_root = tmp.path().join("findings");
        let one = |name: &str| InlineAttachment { name: name.into(), bytes: vec![1] };
        let cases: Vec<(&str, Vec<InlineAttachment>)> = vec![
            ("", vec![]),
            ("k", vec![one("a.txt"), one("a.txt")]),
            ("k", vec![one("../escape")]),
            ("k", vec![one("")]),
        ];
        for (kit, attachments) in cases {
            let err = create_from_emission(
                &root,
                &findings_root,
                "coder (m)",
                &[],
                kit,
                &attachments,
                findings::Scope::default(),
                None,
                Vec::new(),
            )
            .expect_err("refused");
            let _ = err;
        }
        assert!(
            load_all_at(&root).unwrap().is_empty(),
            "a refused mod writes nothing at all"
        );
    }

    #[test]
    fn base64_round_trips_every_byte_and_every_length() {
        for n in 0..=32usize {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 7 + 3) as u8).collect();
            let encoded = {
                // the runtime's encoder, mirrored here so the two agree
                const A: &[u8; 64] =
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
                let mut out = String::new();
                for chunk in bytes.chunks(3) {
                    let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
                    let v = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
                    out.push(A[(v >> 18) as usize & 63] as char);
                    out.push(A[(v >> 12) as usize & 63] as char);
                    out.push(if chunk.len() > 1 { A[(v >> 6) as usize & 63] as char } else { '=' });
                    out.push(if chunk.len() > 2 { A[v as usize & 63] as char } else { '=' });
                }
                out
            };
            assert_eq!(decode_b64(&encoded).unwrap(), bytes, "length {n}");
        }
        assert!(decode_b64("not base64!").is_err());
    }

    /// A finding in the store, so a mod's `for` has something real to copy.
    fn store_finding(root: &Path, dispatch: &str, seq: u64, mission: Option<&str>) {
        let rec = findings::build_record(
            dispatch,
            seq,
            "2026-09-03T01:00:00Z".to_string(),
            "create_finding",
            findings::Proposer {
                handle: "crawler".into(),
                model: "darkmux:qwen3.6".into(),
                machine_id: Some("studio".into()),
            },
            findings::Scope {
                mission_id: mission.map(String::from),
                phase_id: None,
                step_id: None,
            },
            Some(serde_json::json!({"rule": "unnamed-predicate", "unit": "u1"})),
            serde_json::json!({"file": "a.ts", "line": 4}),
        );
        findings::materialize(root, &rec).unwrap();
    }

    /// (#2361, PROVEN live on mission `review-v2-1788566897-9c149e`) The
    /// mod's kit came back with the CONTAINER's headers — `--- a/app/src/
    /// auth.ts` / `+++ b/app/src/auth.ts`, where `app` is the workspace
    /// source id — while `mods.gate` applies the kit inside a copy of the
    /// SOURCE CHECKOUT, whose paths are repo-relative. `git apply` found no
    /// `app/src/auth.ts` there, so the gate recorded "kit did not apply"
    /// and a real, gate-able finding posted as "worth a double check".
    /// Mapped at the host boundary from the source id the launcher
    /// stamped — never guessed from the path.
    #[test]
    fn a_kit_written_in_container_paths_is_mapped_to_repo_relative_headers() {
        let kit = "diff --git a/app/src/auth.ts b/app/src/auth.ts\n\
                   --- a/app/src/auth.ts\n\
                   +++ b/app/src/auth.ts\n\
                   @@ -3,1 +3,1 @@\n\
                   -  if ((user.role === \"admin\"))\n\
                   +  if (isAdmin(user))\n";
        let mapped = strip_kit_source_prefix("app", kit);
        assert!(mapped.contains("diff --git a/src/auth.ts b/src/auth.ts"), "{mapped}");
        assert!(mapped.contains("--- a/src/auth.ts"), "{mapped}");
        assert!(mapped.contains("+++ b/src/auth.ts"), "{mapped}");
        // Only the headers move: the body is byte-for-byte what the model wrote.
        assert!(mapped.contains("-  if ((user.role === \"admin\"))"), "{mapped}");
        assert!(mapped.contains("+  if (isAdmin(user))"), "{mapped}");
        assert!(!mapped.contains("app/src/auth.ts"), "no container path survives: {mapped}");
    }

    /// The absolute container form maps too, `/dev/null` (a new file) is
    /// left alone, and a kit already written in repo-relative paths comes
    /// back unchanged — so mapping twice is the same as mapping once.
    #[test]
    fn kit_mapping_handles_the_absolute_form_dev_null_and_is_idempotent() {
        let abs = "--- /workspace/app/src/auth.ts\n+++ /workspace/app/src/auth.ts\n";
        assert_eq!(strip_kit_source_prefix("app", abs), "--- src/auth.ts\n+++ src/auth.ts\n");

        let new_file = "--- /dev/null\n+++ b/app/src/new.ts\n";
        assert_eq!(strip_kit_source_prefix("app", new_file), "--- /dev/null\n+++ b/src/new.ts\n");

        let already = "--- a/src/auth.ts\n+++ b/src/auth.ts\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(strip_kit_source_prefix("app", already), already);
        assert_eq!(
            strip_kit_source_prefix("app", &strip_kit_source_prefix("app", new_file)),
            strip_kit_source_prefix("app", new_file),
            "mapping is idempotent"
        );
        // No source id: nothing is touched.
        let container = "--- a/app/src/auth.ts\n";
        assert_eq!(strip_kit_source_prefix("", container), container);
    }

    /// (#2310 fix-loop E2, from the loop-D review) A RENAME kit maps too.
    ///
    /// `git diff` writes a rename as `rename from <path>` / `rename to
    /// <path>` — repo-relative paths on their own header lines, with no
    /// `a/`/`b/` marker. Mapping the `diff --git` line while leaving those
    /// two behind produced a kit whose halves disagreed about where the
    /// file lives, and `git apply` refuses that outright: the mod recorded
    /// "kit did not apply" and a real change posted as a double-check
    /// thread. `copy from`/`copy to` is the same header family (git emits
    /// it for a detected copy) and has the same failure.
    #[test]
    fn a_rename_kit_maps_its_rename_headers_not_only_its_diff_git_line() {
        let kit = "diff --git a/app/src/old.ts b/app/src/new.ts\n\
                   similarity index 96%\n\
                   rename from app/src/old.ts\n\
                   rename to app/src/new.ts\n\
                   --- a/app/src/old.ts\n\
                   +++ b/app/src/new.ts\n\
                   @@ -1 +1 @@\n\
                   -const a = 1;\n\
                   +const a = 2;\n";
        let mapped = strip_kit_source_prefix("app", kit);
        assert!(mapped.contains("rename from src/old.ts"), "{mapped}");
        assert!(mapped.contains("rename to src/new.ts"), "{mapped}");
        assert!(!mapped.contains("app/src/"), "no container path survives anywhere in the kit: {mapped}");
        // The non-path metadata line is untouched.
        assert!(mapped.contains("similarity index 96%"), "{mapped}");
    }

    /// The `copy from`/`copy to` half of the same header family.
    #[test]
    fn a_copy_kit_maps_its_copy_headers() {
        let kit = "copy from app/src/a.ts\ncopy to app/src/b.ts\n";
        assert_eq!(strip_kit_source_prefix("app", kit), "copy from src/a.ts\ncopy to src/b.ts\n");
    }

    /// (#2310 fix-loop E2, from the loop-D review) The idempotency claim,
    /// pinned WITH its limitation rather than restated.
    ///
    /// The bare (unmarked) branch cannot distinguish two identical strings:
    /// `app/x.ts` as CONTAINER coordinates for source `app` (map it to
    /// `x.ts`) and `app/x.ts` as an already-repo-relative path in a repo
    /// whose top-level directory happens to be named `app` (leave it). It
    /// strips, so the second case is over-stripped — and mapping twice
    /// strips twice.
    ///
    /// Not "fixed" here, because every available fix is a guess: the header
    /// text carries no marker for which coordinate system it is in, and the
    /// host knows only the source id, not the repo's own top-level layout.
    /// The `a/`/`b/`-marked forms — which is what `git diff` actually emits
    /// and what every real kit observed so far uses — are unaffected, since
    /// their marker is stripped and re-attached around the mapping. This
    /// test pins the CURRENT behavior so a future change to it is a
    /// deliberate one.
    #[test]
    fn the_bare_path_branch_cannot_tell_container_coords_from_a_top_level_dir_of_the_same_name() {
        // Case 1 — container coordinates. Mapping is correct.
        assert_eq!(strip_kit_source_prefix("app", "rename from app/x.ts\n"), "rename from x.ts\n");
        // Case 2 — the SAME string as a repo-relative path in a repo with a
        // top-level `app/`. Indistinguishable, so it is over-stripped.
        // KNOWN LIMITATION, not a desired behavior.
        assert_eq!(
            strip_kit_source_prefix("app", "--- app/x.ts\n"),
            "--- x.ts\n",
            "the bare branch over-strips a repo-relative path under a top-level dir named like the source id"
        );
        // And therefore the bare form is NOT idempotent under repetition
        // when the path keeps re-matching.
        let once = strip_kit_source_prefix("app", "--- app/app/x.ts\n");
        assert_eq!(once, "--- app/x.ts\n");
        assert_eq!(strip_kit_source_prefix("app", &once), "--- x.ts\n", "a second pass strips again");
    }

    /// The whole boundary, through the record: an emission whose kit is in
    /// container paths is STORED repo-relative, and the source id that made
    /// the mapping possible is on the record so nothing is lost.
    #[test]
    fn a_mod_from_an_emission_stores_the_mapped_kit_and_names_its_source() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("mods");
        let findings_root = tmp.path().join("findings");
        store_finding(&findings_root, "sess-c", 1, None);
        let kit = "--- a/app/src/auth.ts\n+++ b/app/src/auth.ts\n@@ -1 +1 @@\n-a\n+b\n";
        let rec = create_from_emission(
            &root,
            &findings_root,
            "coder (m)",
            &["sess-c/1".to_string()],
            kit,
            &[],
            findings::Scope::default(),
            Some("app"),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(rec.kit.as_deref(), Some("--- a/src/auth.ts\n+++ b/src/auth.ts\n@@ -1 +1 @@\n-a\n+b\n"));
        assert_eq!(rec.source.as_deref(), Some("app"));
        assert_eq!(rec.kit_kind.as_deref(), Some("unified-diff"), "still detected as a diff: {rec:?}");
    }

    #[test]
    fn minted_keys_are_unique_and_path_safe() {
        let a = mint_key();
        let b = mint_key();
        assert_ne!(a, b, "a key is minted per mod, never derived — two calls, two keys");
        for k in [&a, &b] {
            assert!(k.starts_with("mod-"), "got {k}");
            assert!(is_safe_key(k), "a minted key must be a safe path segment: {k}");
        }
    }

    #[test]
    fn is_safe_key_refuses_anything_that_could_escape_the_store() {
        assert!(is_safe_key("mod-1-abcdef"));
        for bad in ["", ".", "..", "../escape", "a/b", "a\\b", ".hidden"] {
            assert!(!is_safe_key(bad), "must be refused: {bad:?}");
        }
    }

    /// VERBATIM means byte-exact. The earlier version parsed a JSON kit and
    /// re-serialized it, which silently collapsed duplicate keys and rounded
    /// large integers through f64 — a kit is not darkmux's data to normalize.
    #[test]
    fn a_kit_is_stored_byte_exact_and_is_never_parsed() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");

        // Two shapes a JSON round trip destroys, plus the exact whitespace.
        let hostile = "{\n  \"a\": 1,\n  \"a\": 2,\n  \"big\": 12345678901234567890123\n}\n";
        let rec = create(&mods, &finds, "kain", &[], Some(hostile), &[], None).unwrap();
        assert_eq!(rec.kit.as_deref(), Some(hostile), "the kit is the bytes that came in");
        assert!(rec.kit_looks_json, "a reader HINT — not a parse, and not a promise");
        let back = load_at(&mods, &rec.key).unwrap().unwrap();
        assert_eq!(back.kit.as_deref(), Some(hostile), "byte-exact through disk too");

        // Prose stays prose, with its own whitespace.
        let prose = "rename the predicate, then add a test\n";
        let rec = create(&mods, &finds, "kain", &[], Some(prose), &[], None).unwrap();
        assert_eq!(rec.kit.as_deref(), Some(prose));
        assert!(!rec.kit_looks_json);

        // A kit that is literally the text `null` is that text — not a null.
        let rec = create(&mods, &finds, "kain", &[], Some("null"), &[], None).unwrap();
        assert_eq!(rec.kit.as_deref(), Some("null"));
        let raw = std::fs::read_to_string(record_path_at(&mods, &rec.key)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["kit"], "null", "stored as a STRING, so `null` is text: {raw}");
    }

    #[test]
    fn create_mints_a_new_key_every_call_so_two_mods_for_one_finding_both_survive() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        store_finding(&finds, "sess-a", 1, Some("crawl-1"));
        let for_keys = vec!["sess-a/1".to_string()];

        let one = create(&mods, &finds, "sonnet", &for_keys, Some("change the code"), &[], None).unwrap();
        let two = create(&mods, &finds, "kain", &for_keys, Some("add a comment"), &[], None).unwrap();

        assert_ne!(one.key, two.key, "the second must NOT overwrite the first");
        assert!(record_path_at(&mods, &one.key).exists());
        assert!(record_path_at(&mods, &two.key).exists());
        assert_eq!(load_all_at(&mods).unwrap().len(), 2, "both records survive");
    }

    #[test]
    fn create_copies_each_stored_findings_provenance_and_marks_a_missing_one() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        store_finding(&finds, "sess-a", 1, Some("crawl-1"));

        let rec = create(
            &mods,
            &finds,
            "sonnet",
            &["sess-a/1".to_string(), "sess-z/9".to_string()],
            Some("kit"),
            &[],
            None,
        )
        .unwrap();

        assert_eq!(rec.r#for, vec!["sess-a/1", "sess-z/9"]);
        let stored = &rec.context.findings[0];
        assert_eq!(stored.key, "sess-a/1");
        assert!(!stored.missing);
        assert_eq!(stored.mission_id.as_deref(), Some("crawl-1"));
        // A reader of the MOD never has to go find the finding.
        assert_eq!(stored.emitted, Some(serde_json::json!({"file": "a.ts", "line": 4})));
        assert_eq!(
            stored.context,
            Some(serde_json::json!({"rule": "unnamed-predicate", "unit": "u1"}))
        );

        // A `for` key with no stored finding is allowed — the change was still
        // proposed — and is recorded as missing rather than as absent context.
        let absent = &rec.context.findings[1];
        assert_eq!(absent.key, "sess-z/9");
        assert!(absent.missing, "an unstored finding is marked, not silently empty");
        assert!(absent.emitted.is_none());

        // It round-trips through disk with the same shape.
        let back = load_at(&mods, &rec.key).unwrap().expect("round trips");
        assert_eq!(back.context, rec.context);
        assert_eq!(back.by, "sonnet");
        assert_eq!(back.schema_version, MOD_SCHEMA_VERSION);
    }

    /// (#2310 swarm F / S2-2b) The leniency the `"1"` → `"2"` bump leans
    /// on, proven against a record written the way a pre-#2310 producer
    /// wrote one: `schema_version: "1"` and NONE of the four fields the
    /// shape has gained since. It must read back whole, with each added
    /// field absent — because absent is a real value for every one of
    /// them, not a hole.
    #[test]
    fn a_schema_version_1_record_still_reads_with_every_added_field_absent() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let key = "sess-old-01";
        std::fs::create_dir_all(mods.join(key)).unwrap();
        // Written by hand, not by `create` — a fixture built from today's
        // struct would carry today's fields and prove nothing.
        std::fs::write(
            mods.join(key).join("mod.json"),
            r#"{
              "key": "sess-old-01",
              "ts": "2026-08-01T00:00:00Z",
              "by": "sonnet",
              "for": ["sess-a/1"],
              "kit": "--- a/x\n+++ b/x\n",
              "kit_looks_json": false,
              "attachments": [],
              "context": { "findings": [] },
              "schema_version": "1"
            }"#,
        )
        .unwrap();

        let back = load_at(&mods, key).unwrap().expect("a schema `1` record still loads");
        assert_eq!(back.schema_version, "1", "the record keeps the version it was written at");
        assert_eq!(back.by, "sonnet");
        assert_eq!(back.r#for, vec!["sess-a/1"]);
        assert!(back.kit_kind.is_none(), "no hint");
        assert!(back.source.is_none(), "already in repo coordinates");
        assert!(back.gate.is_none(), "never gated");
        assert!(back.gate_skipped_reason.is_none(), "and no skip was recorded either");
        assert!(back.mission_id.is_none() && back.phase_id.is_none() && back.step_id.is_none());
        assert!(back.extras.is_empty(), "nothing in the `1` shape is unknown to this reader");
    }

    /// (#2310 swarm F / S2-2b) The forward direction: a record from a
    /// writer NEWER than this binary keeps its unknown fields in `extras`
    /// rather than failing the whole parse.
    #[test]
    fn a_future_schema_record_reads_and_keeps_its_unknown_fields() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let key = "sess-new-01";
        std::fs::create_dir_all(mods.join(key)).unwrap();
        std::fs::write(
            mods.join(key).join("mod.json"),
            r#"{
              "key": "sess-new-01",
              "ts": "2026-09-05T00:00:00Z",
              "by": "sonnet",
              "for": [],
              "kit": "x",
              "kit_looks_json": false,
              "attachments": [],
              "context": { "findings": [] },
              "a_field_from_schema_3": { "n": 1 },
              "schema_version": "3"
            }"#,
        )
        .unwrap();

        let back = load_at(&mods, key).unwrap().expect("a newer record still loads");
        assert_eq!(back.schema_version, "3");
        assert_eq!(back.extras.get("a_field_from_schema_3"), Some(&serde_json::json!({ "n": 1 })));
    }

    #[test]
    fn create_copies_attachments_byte_for_byte_and_refuses_a_colliding_basename() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("a")).unwrap();
        std::fs::create_dir_all(src.join("b")).unwrap();
        std::fs::write(src.join("a/patch.diff"), b"--- a\n+++ b\n").unwrap();
        std::fs::write(src.join("b/shot.png"), [0x89u8, 0x50, 0x4e, 0x47]).unwrap();

        let rec = create(
            &mods,
            &finds,
            "kain",
            &[],
            None,
            &[src.join("a/patch.diff"), src.join("b/shot.png")],
            None,
        )
        .unwrap();
        assert_eq!(rec.attachments, vec!["patch.diff", "shot.png"]);
        assert!(rec.kit.is_none(), "attachments alone are a kit; no kit text was given");
        let dest = attachments_dir_at(&mods, &rec.key);
        assert_eq!(std::fs::read(dest.join("patch.diff")).unwrap(), b"--- a\n+++ b\n");
        assert_eq!(std::fs::read(dest.join("shot.png")).unwrap(), [0x89u8, 0x50, 0x4e, 0x47]);

        // Two attachments with one basename: one would overwrite the other, so
        // the whole create is refused rather than quietly losing a file.
        std::fs::write(src.join("b/patch.diff"), b"other").unwrap();
        let err = create(
            &mods,
            &finds,
            "kain",
            &[],
            None,
            &[src.join("a/patch.diff"), src.join("b/patch.diff")],
            None,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("patch.diff"), "the error names it: {err:#}");
    }

    /// A copy that fails HALFWAY must leave no record at all. The record is
    /// write-once, so a persisted mod listing an attachment that is not on
    /// disk could never be completed — and a retry would mint a second key,
    /// leaving the broken one behind forever.
    #[test]
    fn a_failed_attachment_copy_leaves_no_record_behind() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("ok.diff"), b"--- a\n").unwrap();
        let unreadable = src.join("unreadable.bin");
        std::fs::write(&unreadable, b"secret").unwrap();
        // Readable enough to pass the is-a-file check, unreadable at copy time
        // — the failure lands BETWEEN the two attachments.
        std::fs::set_permissions(&unreadable, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .unwrap();

        let err = create(
            &mods,
            &finds,
            "kain",
            &[],
            Some("kit"),
            &[src.join("ok.diff"), unreadable.clone()],
            None,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("unreadable.bin"), "the error names it: {err:#}");

        assert!(load_all_at(&mods).unwrap().is_empty(), "no half-written mod is visible");
        // Nothing at all is left in the store — not a record dir, not staging.
        let leftovers: Vec<String> = std::fs::read_dir(&mods)
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "the store is untouched, got: {leftovers:?}");

        std::fs::set_permissions(&unreadable, std::os::unix::fs::PermissionsExt::from_mode(0o644))
            .unwrap();
    }

    /// One finding must have ONE address. `sess-a/01` and `sess-a/1` name the
    /// same finding, and storing the raw string made a mod attached to the
    /// finding (context copied, `--mission` matching) yet invisible to
    /// `list --for sess-a/1` and to that finding's own derived section.
    #[test]
    fn create_canonicalizes_for_keys_and_refuses_one_that_cannot_address_a_finding() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        store_finding(&finds, "sess-a", 1, Some("crawl-1"));

        let rec = create(&mods, &finds, "kain", &["sess-a/01".into()], Some("k"), &[], None).unwrap();
        assert_eq!(rec.r#for, vec!["sess-a/1"], "stored in canonical form");
        assert_eq!(rec.context.findings[0].key, "sess-a/1");
        assert!(!rec.context.findings[0].missing, "it resolved to the real finding");

        // The one address is the one every reader uses.
        // One address, on the READ side too. Stored keys are canonical, so a
        // reader that compares the caller's raw string finds nothing — the
        // same mod is attached by one query and invisible to another.
        let all = load_all_at(&mods).unwrap();
        assert_eq!(mods_for(&all, "sess-a/1").len(), 1, "the derived view finds it");
        assert_eq!(
            mods_for(&all, "sess-a/01").len(),
            1,
            "a non-canonical QUERY finds the mod stored under the canonical key"
        );
        assert_eq!(mods_for(&all, "sess-a/2").len(), 0, "a different finding is still different");
        assert_eq!(
            mods_for(&all, "no-slash").len(),
            0,
            "a query that can address no finding matches nothing"
        );

        // A key that can address no finding is refused LOUDLY at create time,
        // rather than stored as a link that nothing can ever follow.
        for bad in ["no-slash", "sess-a/notanumber", "../x/1", "/1"] {
            let err = create(&mods, &finds, "kain", &[bad.to_string()], Some("k"), &[], None).unwrap_err();
            assert!(
                format!("{err:#}").contains("finding key"),
                "the error names the shape for {bad:?}: {err:#}"
            );
        }
    }

    /// A KEY may not start with a dot (it is a directory name darkmux mints).
    /// A FILE may — `.env.example` is an ordinary attachment. The two rules
    /// were the same function, so a legitimate dotfile was refused.
    #[test]
    fn an_attachment_may_be_a_dotfile_but_never_a_traversal_name() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(".env.example"), b"KEY=value\n").unwrap();

        let rec = create(&mods, &finds, "kain", &[], None, &[src.join(".env.example")], None).unwrap();
        assert_eq!(rec.attachments, vec![".env.example"]);
        assert_eq!(
            std::fs::read(attachments_dir_at(&mods, &rec.key).join(".env.example")).unwrap(),
            b"KEY=value\n"
        );

        assert!(is_safe_basename(".env.example"));
        for bad in [".", "..", "a/b", "a\\b", ""] {
            assert!(!is_safe_basename(bad), "must be refused: {bad:?}");
        }
    }

    /// A record's key is also its address on disk. If the two disagree, the
    /// record cannot be addressed by what it claims to be, so it is skipped
    /// rather than served under a name that does not resolve.
    #[test]
    fn a_record_whose_key_disagrees_with_its_directory_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        let good = create(&mods, &finds, "kain", &[], Some("k"), &[], None).unwrap();

        std::fs::create_dir_all(mods.join("mod-liar")).unwrap();
        std::fs::write(
            mods.join("mod-liar/mod.json"),
            serde_json::to_string(&serde_json::json!({
                "key": good.key, "ts": "2026-09-03T09:00:00Z", "by": "x", "for": [],
                "kit": "k", "kit_looks_json": false, "attachments": [],
                "context": {"findings": []}, "schema_version": "1",
            }))
            .unwrap(),
        )
        .unwrap();

        let keys: Vec<String> = load_all_at(&mods).unwrap().into_iter().map(|m| m.key).collect();
        assert_eq!(keys, vec![good.key.clone()], "the impostor is skipped, not served twice");
        assert!(
            load_at(&mods, "mod-liar").unwrap().is_none(),
            "a record that does not own its directory does not resolve there"
        );
    }

    #[test]
    fn create_refuses_a_mod_with_neither_instructions_nor_data() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        assert!(create(&mods, &finds, "kain", &[], None, &[], None).is_err());
        assert!(create(&mods, &finds, "  ", &[], Some("kit"), &[], None).is_err(), "a mod names its proposer");
        assert!(!mods.exists(), "a refusal writes nothing at all");
    }

    #[test]
    fn materialize_writes_once_and_refuses_a_key_that_could_escape_the_store() {
        let tmp = TempDir::new().unwrap();
        let mut rec = ModRecord {
            key: mint_key(),
            ts: "2026-09-03T01:00:00Z".into(),
            by: "kain".into(),
            r#for: vec![],
            kit: Some("k".into()),
            kit_looks_json: false,
            kit_kind: None,
            attachments: vec![],
            context: ModContext::default(),
            warnings: Vec::new(),
            mission_id: None,
            phase_id: None,
            step_id: None,
            source: None,
            gate: None,
            gate_skipped_reason: None,
            schema_version: MOD_SCHEMA_VERSION.into(),
            extras: serde_json::Map::new(),
        };
        assert_eq!(materialize(tmp.path(), &rec).unwrap(), Materialized::Created);
        let path = record_path_at(tmp.path(), &rec.key);
        std::fs::write(&path, "{\"key\":\"sentinel\"}").unwrap();
        assert_eq!(materialize(tmp.path(), &rec).unwrap(), Materialized::AlreadyPresent);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"key\":\"sentinel\"}");

        for bad in ["../escape", "a/b", ".hidden", ""] {
            rec.key = bad.to_string();
            assert!(materialize(tmp.path(), &rec).is_err(), "must be refused: {bad:?}");
        }
    }

    fn a_gateable_mod(key: &str) -> ModRecord {
        ModRecord {
            key: key.to_string(),
            ts: "2026-09-05T00:00:00Z".into(),
            by: "coder".into(),
            r#for: vec![],
            kit: Some("k".into()),
            kit_looks_json: false,
            kit_kind: None,
            attachments: vec![],
            context: ModContext::default(),
            warnings: Vec::new(),
            mission_id: None,
            phase_id: None,
            step_id: None,
            source: None,
            gate: None,
            gate_skipped_reason: None,
            schema_version: MOD_SCHEMA_VERSION.into(),
            extras: serde_json::Map::new(),
        }
    }

    /// (#2310 P4c-2b PR #2357 review item G) `record_gate`'s own direct
    /// test — every existing coverage of it was indirect, through
    /// `mods_gate`'s step-level tests.
    #[test]
    fn record_gate_writes_the_outcome_onto_an_existing_mod() {
        let tmp = TempDir::new().unwrap();
        materialize(tmp.path(), &a_gateable_mod("mod-g1")).unwrap();

        let res = record_gate(
            tmp.path(),
            "mod-g1",
            Some(GateOutcome { passed: true, command: "cargo test".into(), exit_code: Some(0), applied: Some(true), reason: None }),
            None,
        )
        .unwrap();
        assert_eq!(res, Materialized::Created);

        let rec = load_at(tmp.path(), "mod-g1").unwrap().unwrap();
        assert!(rec.gate.as_ref().unwrap().passed);
        assert_eq!(rec.gate.as_ref().unwrap().command, "cargo test");
        assert!(rec.gate_skipped_reason.is_none());
    }

    /// (#2310 P4c-2b PR #2357 review item G) A SECOND write to an
    /// already-gated mod changes nothing and reports `AlreadyPresent` —
    /// the record's own write-once-per-gate discipline, tested directly
    /// rather than only through `mods_gate`'s "already gated" step test
    /// (which proves the STEP skips the spawn, not that `record_gate`
    /// itself refuses to overwrite).
    #[test]
    fn record_gate_a_second_write_is_a_no_op_reporting_already_present() {
        let tmp = TempDir::new().unwrap();
        materialize(tmp.path(), &a_gateable_mod("mod-g2")).unwrap();

        record_gate(
            tmp.path(),
            "mod-g2",
            Some(GateOutcome { passed: true, command: "first".into(), exit_code: Some(0), applied: Some(true), reason: None }),
            None,
        )
        .unwrap();

        let second = record_gate(
            tmp.path(),
            "mod-g2",
            Some(GateOutcome { passed: false, command: "second".into(), exit_code: Some(1), applied: Some(true), reason: None }),
            None,
        )
        .unwrap();
        assert_eq!(second, Materialized::AlreadyPresent, "a gated mod must never be re-gated");

        let rec = load_at(tmp.path(), "mod-g2").unwrap().unwrap();
        assert_eq!(rec.gate.as_ref().unwrap().command, "first", "the FIRST gate result survives, never overwritten");
    }

    /// (#2310 P4c-2b PR #2357 review item G) The same no-op discipline
    /// when the first write was a SKIP (`gate_skipped_reason`, not a real
    /// `GateOutcome`) — a second call naming a real outcome must not
    /// promote a skip into a pass/fail behind the reader's back.
    #[test]
    fn record_gate_a_skip_is_also_write_once() {
        let tmp = TempDir::new().unwrap();
        materialize(tmp.path(), &a_gateable_mod("mod-g3")).unwrap();

        record_gate(tmp.path(), "mod-g3", None, Some("no test_command configured")).unwrap();
        let second = record_gate(
            tmp.path(),
            "mod-g3",
            Some(GateOutcome { passed: true, command: "late".into(), exit_code: Some(0), applied: Some(true), reason: None }),
            None,
        )
        .unwrap();
        assert_eq!(second, Materialized::AlreadyPresent);

        let rec = load_at(tmp.path(), "mod-g3").unwrap().unwrap();
        assert!(rec.gate.is_none(), "the original skip must survive: {rec:?}");
        assert_eq!(rec.gate_skipped_reason.as_deref(), Some("no test_command configured"));
    }

    #[test]
    fn load_all_at_sorts_by_ts_and_skips_what_it_cannot_read() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("mods");
        assert!(load_all_at(&root).unwrap().is_empty(), "an absent store is empty, not an error");

        let mk = |key: &str, ts: &str| ModRecord {
            key: key.into(),
            ts: ts.into(),
            by: "kain".into(),
            r#for: vec![],
            kit: Some("k".into()),
            kit_looks_json: false,
            kit_kind: None,
            attachments: vec![],
            context: ModContext::default(),
            warnings: Vec::new(),
            mission_id: None,
            phase_id: None,
            step_id: None,
            source: None,
            gate: None,
            gate_skipped_reason: None,
            schema_version: MOD_SCHEMA_VERSION.into(),
            extras: serde_json::Map::new(),
        };
        materialize(&root, &mk("mod-c", "2026-09-03T03:00:00Z")).unwrap();
        materialize(&root, &mk("mod-a", "2026-09-03T01:00:00Z")).unwrap();
        materialize(&root, &mk("mod-b", "2026-09-03T02:00:00Z")).unwrap();
        std::fs::create_dir_all(root.join("mod-x")).unwrap();
        std::fs::write(root.join("mod-x/mod.json"), "{ not json").unwrap();

        let keys: Vec<String> = load_all_at(&root).unwrap().into_iter().map(|m| m.key).collect();
        assert_eq!(keys, vec!["mod-a", "mod-b", "mod-c"], "ts-ascending");

        // A key that could escape the store never resolves to a read.
        assert!(load_at(&root, "../mod-a").unwrap().is_none());
        assert!(load_at(&root, "mod-nope").unwrap().is_none());
    }

    #[test]
    fn the_view_from_a_finding_to_its_mods_is_derived_by_scanning_mods() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        store_finding(&finds, "sess-a", 1, Some("crawl-1"));
        store_finding(&finds, "sess-b", 2, Some("crawl-2"));

        let a1 = create(&mods, &finds, "sonnet", &["sess-a/1".into()], Some("x"), &[], None).unwrap();
        let a2 = create(&mods, &finds, "kain", &["sess-a/1".into()], Some("y"), &[], None).unwrap();
        let b = create(&mods, &finds, "kain", &["sess-b/2".into()], Some("z"), &[], None).unwrap();
        let none = create(&mods, &finds, "kain", &[], Some("standalone"), &[], None).unwrap();

        let all = load_all_at(&mods).unwrap();
        let keys: Vec<&str> = mods_for(&all, "sess-a/1").iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys.len(), 2, "one observation can attract competing changes");
        assert!(keys.contains(&a1.key.as_str()) && keys.contains(&a2.key.as_str()));
        assert!(mods_for(&all, "sess-c/9").is_empty());

        // `--mission` matches through the `for` finding's OWN mission, copied
        // onto the mod at create time.
        let by_mission = |m: &str| -> Vec<String> {
            all.iter().filter(|r| names_mission(r, m)).map(|r| r.key.clone()).collect()
        };
        assert_eq!(by_mission("crawl-2"), vec![b.key.clone()]);
        assert_eq!(by_mission("crawl-1").len(), 2);
        assert!(by_mission("no-such-mission").is_empty());
        assert!(
            !by_mission("crawl-1").contains(&none.key),
            "a mod naming no finding belongs to no mission"
        );
    }

    /// (#2310 fix-loop E2, S5-8/S3-4) A mod the RUN itself produced belongs
    /// to that run, whether or not it names a finding.
    ///
    /// `create_from_emission` stamps the dispatch's own scope onto
    /// `record.mission_id` — host-stamped, never model-supplied — and that
    /// is the authoritative answer to "which run made this". Reading only
    /// the copied `for`-finding provenance made a mod with an empty `for`
    /// (a coder that proposed a change without citing a finding key, which
    /// the record shape explicitly permits) belong to NO mission: it was
    /// invisible to `mod list --mission M` and, worse, to
    /// `records.gather`'s own `mods::names_mission` filter, so the review
    /// it was made for never delivered it.
    #[test]
    fn a_mod_created_under_a_mission_names_it_even_with_an_empty_for_list() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("mods");
        let findings_root = tmp.path().join("findings");

        let rec = create_from_emission(
            &root,
            &findings_root,
            "coder (darkmux:qwen3.6)",
            &[],
            "the patch text",
            &[],
            findings::Scope {
                mission_id: Some("review-7".into()),
                phase_id: Some("p-1".into()),
                step_id: Some("step-3".into()),
            },
            None,
            Vec::new(),
        )
        .unwrap();

        assert!(rec.context.findings.is_empty(), "the fixture's premise: nothing to read provenance from");
        assert!(names_mission(&rec, "review-7"), "the run that made it is the run that gathers it");
        assert!(!names_mission(&rec, "review-8"), "and only that run");
    }

    /// The finding-provenance path is not replaced by the host stamp, it is
    /// the FALLBACK: a `mod create` from an external actor has no
    /// `mission_id` of its own and must still be found through the finding
    /// it names. Without this, deleting the second half of `names_mission`
    /// would leave the test above green.
    #[test]
    fn a_mod_with_no_mission_of_its_own_is_still_found_through_its_findings() {
        let tmp = TempDir::new().unwrap();
        let mods = tmp.path().join("mods");
        let finds = tmp.path().join("findings");
        store_finding(&finds, "sess-a", 1, Some("crawl-1"));

        let rec = create(&mods, &finds, "kain", &["sess-a/1".into()], Some("p"), &[], None).unwrap();
        assert!(rec.mission_id.is_none(), "the fixture's premise: an external actor stamps no mission");
        assert!(names_mission(&rec, "crawl-1"), "the copied finding provenance still answers");
    }
}
