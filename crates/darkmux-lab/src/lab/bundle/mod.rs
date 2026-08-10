//! Built-in review bundler — diff -> per-changed-function code bundles +
//! mechanical facts + manifest (#1222 Phase B packet 3).
//!
//! A Rust port of the reference `bundler.py` (Phase A, procedural/no-AI
//! extraction): split a unified diff into per-changed-function bundles,
//! each carrying the function's own code region, resolved callee/sibling
//! bodies, and mechanically-extracted facts across three families
//! (param-flow, differential, siblings). Fidelity to the reference beats
//! elegance wherever the two conflict — the fact-emission heuristics here
//! are measurement-validated against a real defect corpus, not derived
//! from first principles.
//!
//! Two additions beyond the Python reference (packet 3 mandate, no
//! precedent in `bundler.py`):
//!
//! - **Default-parameter facts** (`scan::extract_param_defaults` +
//!   `facts::build_param_flow_facts`) — closes a measured false-positive
//!   class where an arity claim ("expects 3 args") ignores a default
//!   filling the gap.
//! - **External-symbol manifest** (`build_manifest` below) — every
//!   identifier referenced in a bundle's assembled code but not defined
//!   within its included regions and not resolvable via the
//!   [`FileSource`] lands as a `"referenced but not defined in bundle: X
//!   <- <module-or-unknown>"` manifest line, rather than being silently
//!   treated as ordinary project code.
//!
//! The escape hatch for callers who want a DIFFERENT bundler (a
//! TypeScript-native AST-based one, say) is [`external_bundles`]: run any
//! `<cmd> --worktree <dir> --diff <file>` that emits this same frozen
//! JSON contract on stdout.

pub mod diff;
pub mod external;
pub mod facts;
pub mod scan;
pub mod source;

pub use external::external_bundles;
pub use source::FileSource;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A single (path, line-span) pointer into a source file. 1-indexed,
/// inclusive on both ends — matches the reference's `{"path", "start",
/// "end"}` shape exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleRef {
    pub path: String,
    pub start: u32,
    pub end: u32,
}

/// One bundle: a changed function's code + one fact family's mechanical
/// findings about it. **This JSON shape is FROZEN** — external `--bundler`
/// commands emit it (see [`external_bundles`]); `manifest`/`truncated`
/// are additive fields (packet 3), safe for an older consumer to ignore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    /// `"<fn>@<path>"` for a function bundle — shared across a function's
    /// family-variant bundles so a probe-runner can group them. A
    /// top-level (unenclosed-line) bundle instead uses
    /// `"toplevel:<start>-<end>@<path>"` (#1605 follow-up) — there is no
    /// function name to key on, and the `start-end` span keeps the id
    /// unique across multiple top-level runs in the same file; a consumer
    /// grouping by id should not assume the `"<name>@<path>"` shape alone.
    pub id: String,
    pub code: Vec<BundleRef>,
    pub facts: Vec<String>,
    pub fact_family: String,
    /// External symbols referenced in `code` but not defined within it
    /// and not resolvable via the `FileSource` (#1222 packet 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manifest: Vec<String>,
    /// True when any region in `code` shows less than a callee/sibling's
    /// full extent (a header-only stub of a longer body) — see
    /// [`slice_code`]'s explicit truncation marker (#1222 packet 3).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BundleSet {
    pub bundles: Vec<Bundle>,
    /// Per-file decline accounting (#1605) — WHY a file present in the diff
    /// contributed zero bundles, so a `bundles: 0` result is self-diagnosing
    /// instead of a bare count. `#[serde(default)]` so an `external_bundles`
    /// plugin's JSON (which has no notion of this bookkeeping) deserializes
    /// to the honest empty report rather than failing — a zero-bundle result
    /// from a THIRD-PARTY bundler stays correctly unexplained (never
    /// misclassified as a benign/non-code diff it can't actually attest to).
    #[serde(default)]
    pub skip: BundleSkipReport,
}

/// (#1605) WHY a file the diff touched ended up contributing zero bundles.
/// Enumerated from `build_bundles`'s own decline points below — every early
/// `continue` (or equivalent "produced nothing" outcome) in that function has
/// a matching variant here, so this can never silently omit a real decline
/// path. Kept flat (no catch-all `Other`) on purpose: a NEW decline path
/// added to `build_bundles` without a matching variant here is a compile
/// error at the call site that constructs it, not a silent gap in the
/// breakdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// `scan::ts_file(rel)` is false — the file isn't one of the extensions
    /// the (TypeScript-only) built-in bundler understands at all. The most
    /// common real-world cause (darkmux#1605): a diff dominated by JSON
    /// config, lockfiles, or fixture data never had a chance to bundle.
    NonCodeExtension,
    /// A source file the bundler deliberately EXCLUDES rather than fails to
    /// understand: `scan::ts_file` rejects `tests/` and any basename
    /// containing "test". Split from [`SkipReason::NonCodeExtension`]
    /// (#1605 QA finding) because collapsing them made the operator-facing
    /// no-op comment describe a pure-test-file PR as "non-code content such
    /// as fixtures, lockfiles, or generated config" — which is false about
    /// real code, in a comment whose entire job is to be honest about why
    /// nothing was reviewed. Still BENIGN (the exclusion is deliberate, not
    /// a failure), just accurately named.
    TestFileExcluded,
    /// `source.read_file(rel)` returned `None` — the worktree/GitHub source
    /// doesn't have this file's content (a checkout desync, a file deleted
    /// on the reviewed ref, or an API fetch miss). Distinct from
    /// `NonCodeExtension`: this file WOULD have been considered, but its
    /// content wasn't available to read.
    UnreadableInWorktree,
    /// The hunk carried only removed lines — no surviving added/context line
    /// in the new-side file at all (e.g. a function deleted in its
    /// entirety). There is no post-image content left to bundle.
    NoSurvivingLines,
    /// Every ADDED new-side line in this file fell outside any function
    /// `scan::find_all_functions_in_text` could locate, AND no context
    /// line inside a hunk's span found one either. As of the top-level-
    /// statement fix (#1605 follow-up), an unenclosed line that was
    /// actually added now becomes its own `"toplevel"`-family bundle (see
    /// [`SkipReason::TopLevelOverSizeCap`] for the one way that can still
    /// decline) instead of landing here — so this reason is now specific
    /// to files with NO added lines at all (a pure deletion, or a
    /// context-only hunk) whose surviving context also sits outside every
    /// function. There is genuinely no changed-and-function-shaped (or
    /// changed-at-all) code for the bundler to anchor on in that case.
    NoEnclosingFunction,
    /// A changed function's body exceeded the bundler's per-function size
    /// cap (`end0 - start0 > 300` lines) — a real, internal-limit decline,
    /// NOT a benign "nothing here" (the issue's "diff exceeded some
    /// internal bound" case, distinct from both of the above). (#1751)
    /// Recorded PER FUNCTION, the instant it's skipped — independent of
    /// whether a sibling function (or a top-level run) in the same file
    /// bundled successfully. Before #1751 this only fired when EVERY
    /// function in the file hit the cap (inferred after the fact from
    /// "the file produced zero new bundles"), which silently dropped a
    /// mixed file's over-cap function — see [`SkippedFile::function`] for
    /// how the entry names which function was dropped.
    OverSizeCap,
    /// (#1605 follow-up) A contiguous run of changed lines with no enclosing
    /// function — see [`SkipReason::NoEnclosingFunction`] — exceeded the
    /// same size cap functions are held to (`run_end - run_start > 300`
    /// lines). Unlike the other reasons here, this one can coexist with a
    /// file that ALSO contributed bundles (from its functions, or from a
    /// smaller top-level run elsewhere in the same file) — see
    /// [`BundleSkipReport`]'s doc for what that means for the "not skipped
    /// implies fully covered" reading.
    TopLevelOverSizeCap,
}

/// One file the diff touched that ended up contributing zero bundles, with
/// the mechanical reason why (#1605).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedFile {
    pub path: String,
    pub reason: SkipReason,
    /// (#1751) The specific function name and 1-indexed line span this
    /// decline is scoped to, when `reason` names a decline that can fire
    /// per-function within an otherwise-successful file (currently
    /// `SkipReason::OverSizeCap`, recorded the moment a function is
    /// skipped rather than only when EVERY function in the file hit the
    /// cap) — e.g. `"huge (lines 5-330)"`. `None` for a file-scoped
    /// reason (`NonCodeExtension`, `UnreadableInWorktree`, etc.), where
    /// there is no single function to name. Additive field — a consumer
    /// built against the pre-#1751 two-field shape still deserializes
    /// fine (`#[serde(default)]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
}

/// The bundler's full per-file decline accounting for one `build_bundles`
/// run (#1605). `files_considered` is every file `diff::parse_diff` found in
/// the diff (before any filtering); `files_skipped` names each declined
/// portion, with why. A file NOT in `files_skipped` at all contributed at
/// least one bundle. (#1751) A file WITH some functions bundling and OTHERS
/// over the size cap now shows up in `files_skipped` too — one
/// `SkippedFile { reason: OverSizeCap, function: Some(...) }` entry per
/// dropped function, alongside the bundles its surviving functions
/// produced — so "in `files_skipped`" no longer means "this file
/// contributed nothing"; it means "at least this much was declined," which
/// can coexist with real bundles from the same file. (Same shape
/// [`SkipReason::TopLevelOverSizeCap`] (#1605 follow-up) already had for
/// top-level runs — #1751 brought function declines up to the same
/// per-decline-point recording instead of an after-the-fact,
/// whole-file-only inference.) So today: "in `files_skipped`" means "at
/// least this much was declined"; "NOT in `files_skipped`" means "at least
/// one bundle came out of this file, and nothing from it was dropped,"
/// neither more nor less. `bundles: 0` with an EMPTY `files_skipped` (only
/// possible when `files_considered == 0`, i.e. the diff itself parsed to no
/// files) is the one case this report can't explain further — see
/// [`BundleSet::skip`]'s own doc for why an external bundler's zero-bundle
/// result stays in that same unexplained state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BundleSkipReport {
    pub files_considered: usize,
    pub files_skipped: Vec<SkippedFile>,
}

/// Returns `Some(full_end1)` when `r` shows LESS than the full extent of
/// the function whose declaration it starts at — i.e. this ref is a
/// header-only stub (a callee body over [`facts::MAX_CALLEE_BODY_LINES`],
/// or a sibling pointer) of a function that actually runs to
/// `full_end1`. Purely mechanical: re-derived from the `FileSource`'s
/// own content, no extra bookkeeping needed on `BundleRef` itself.
fn truncated_extent(source: &FileSource, r: &BundleRef) -> Result<Option<u32>> {
    let Some(content) = source.read_file(&r.path)? else {
        return Ok(None);
    };
    let lines: Vec<String> = content.lines().map(str::to_string).collect();
    for f in scan::find_all_functions_in_text(&lines) {
        let full_start = f.start0 as u32 + 1;
        let full_end = f.end0 as u32 + 1;
        if full_start == r.start && full_end > r.end {
            return Ok(Some(full_end));
        }
    }
    Ok(None)
}

/// Render `refs` as source text: `// <path> (lines <a>-<b>)` header
/// lines, the actual source lines, and — where a region shows less than
/// its enclosing function's full extent — an explicit truncation marker
/// (#1222 packet 3; the reference never rendered code to text at all,
/// only emitted line-span pointers for a downstream consumer to resolve).
pub fn slice_code(source: &FileSource, refs: &[BundleRef]) -> Result<String> {
    let mut out = String::new();
    for r in refs {
        out.push_str(&format!("// {} (lines {}-{})\n", r.path, r.start, r.end));
        match source.read_file(&r.path)? {
            Some(content) => {
                let lines: Vec<&str> = content.lines().collect();
                let start_idx = r.start.saturating_sub(1) as usize;
                let end_idx = (r.end as usize).min(lines.len());
                if start_idx < end_idx {
                    for l in &lines[start_idx..end_idx] {
                        out.push_str(l);
                        out.push('\n');
                    }
                }
                if let Some(full_end) = truncated_extent(source, r)? {
                    out.push_str(&format!(
                        "// … excerpt truncated — full function continues to line {full_end} …\n"
                    ));
                }
            }
            None => {
                out.push_str(&format!("// (unreadable: {})\n", r.path));
            }
        }
        out.push('\n');
    }
    Ok(out)
}

/// Render `refs` in the PROBE seat's Phase A format — a byte-for-byte
/// Rust port of `probe-runner.py`'s `read_code_excerpt` + `dedupe_refs` +
/// the `"\n\n".join(blocks)` in its `build_prompt` (#1256 parity): refs
/// deduped by `(path, start, end)` in first-seen order, then per ref one
/// block
///
/// ````text
/// ### `<path>` (lines <s>-<e>)
/// ```typescript
/// <raw source lines s..=e>
/// ```
/// ````
///
/// with `s = max(1, start)` and `e = min(file_lines, end)` — the header
/// echoes the CLAMPED span (unlike [`slice_code`], whose header echoes
/// the ref verbatim). A ref whose file is unreadable, or whose clamped
/// start exceeds the file's line count, is SKIPPED entirely (Python
/// returns `None` and `build_prompt` drops it) — no `(unreadable: ...)`
/// placeholder, no truncation marker, and no trailing newline after the
/// last block. This is deliberately a SEPARATE renderer from
/// [`slice_code`] (the judge seat's format, matching `judge-runner.py`'s
/// own `slice_code`): Phase A formatted the two seats' code differently,
/// and per-seat parity means porting both formats, not unifying them.
pub fn slice_code_probe(source: &FileSource, refs: &[BundleRef]) -> Result<String> {
    let mut seen: HashSet<(String, u32, u32)> = HashSet::new();
    let mut blocks: Vec<String> = Vec::new();
    for r in refs {
        if !seen.insert((r.path.clone(), r.start, r.end)) {
            continue;
        }
        let Some(content) = source.read_file(&r.path)? else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        let s = r.start.max(1) as usize;
        let e = (r.end as usize).min(lines.len());
        if s > lines.len() {
            continue;
        }
        // Python's `lines[s-1:e]` yields [] when e < s (a degenerate ref);
        // the block is still emitted, with an empty snippet — mirror that
        // rather than panicking on a backwards range.
        let snippet = if e >= s { lines[s - 1..e].join("\n") } else { String::new() };
        blocks.push(format!(
            "### `{}` (lines {}-{})\n```typescript\n{}\n```",
            r.path, s, e, snippet
        ));
    }
    Ok(blocks.join("\n\n"))
}

/// Just the raw source lines `refs` point at, concatenated — no `//
/// <path> (lines a-b)` header, no truncation marker. Used where callers
/// need to scan CODE (identifier extraction), as opposed to
/// [`slice_code`]'s human-readable rendering.
fn raw_code_text(source: &FileSource, refs: &[BundleRef]) -> Result<String> {
    let mut out = String::new();
    for r in refs {
        if let Some(content) = source.read_file(&r.path)? {
            let lines: Vec<&str> = content.lines().collect();
            let start_idx = r.start.saturating_sub(1) as usize;
            let end_idx = (r.end as usize).min(lines.len());
            if start_idx < end_idx {
                for l in &lines[start_idx..end_idx] {
                    out.push_str(l);
                    out.push('\n');
                }
            }
        }
    }
    Ok(out)
}

fn line_has_call(line: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let b = line.as_bytes();
    for (idx, _) in line.match_indices(name) {
        let before_ok = idx == 0 || !is_word_byte(b[idx - 1]);
        if !before_ok {
            continue;
        }
        let mut p = idx + name.len();
        while p < b.len() && matches!(b[p], b' ' | b'\t') {
            p += 1;
        }
        if p < b.len() && b[p] == b'(' {
            return true;
        }
    }
    false
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Port of the reference's inline caller-grep loop: for every file in
/// `candidate_files`, the first call-site line to `name` (outside the
/// function's own definition span) becomes a `+-3`-line excerpt, capped
/// at `facts::MAX_CALLERS`.
fn find_caller_refs(
    source: &FileSource,
    candidate_files: &[String],
    name: &str,
    own_path: &str,
    own_span: (usize, usize),
) -> Result<Vec<BundleRef>> {
    let mut out = Vec::new();
    let mut found = 0usize;
    for crel in candidate_files {
        if found >= facts::MAX_CALLERS {
            break;
        }
        let Some(content) = source.read_file(crel)? else {
            continue;
        };
        let clines: Vec<&str> = content.lines().collect();
        for (i, cl) in clines.iter().enumerate() {
            if crel == own_path && own_span.0 <= i && i <= own_span.1 {
                continue;
            }
            if line_has_call(cl, name) {
                let cs = i.saturating_sub(3);
                let ce = (i + 3).min(clines.len().saturating_sub(1));
                out.push(BundleRef {
                    path: crel.clone(),
                    start: cs as u32 + 1,
                    end: ce as u32 + 1,
                });
                found += 1;
                break;
            }
        }
    }
    Ok(out)
}

/// External-symbol manifest (#1222 packet 3 — new, no Python
/// precedent). Mechanical only: every distinct call-site identifier
/// found across `code_refs`' assembled text that (a) isn't the name of a
/// function DECLARED at one of those refs, and (b) doesn't resolve
/// anywhere in `repo_index` (the source's own known-function surface),
/// is reported — with the best-effort import module (from the primary
/// function's own file) as provenance, or `unknown` when no matching
/// import binding is found.
fn build_manifest(
    source: &FileSource,
    code_refs: &[BundleRef],
    repo_index: &facts::RepoIndex,
    own_file_content: &str,
) -> Result<Vec<String>> {
    let mut defined: HashSet<String> = HashSet::new();
    for r in code_refs {
        if let Some(content) = source.read_file(&r.path)? {
            let lines: Vec<String> = content.lines().map(str::to_string).collect();
            for f in scan::find_all_functions_in_text(&lines) {
                if f.start0 as u32 + 1 == r.start {
                    defined.insert(f.name.clone());
                }
            }
        }
    }
    // Raw source text only — NOT `slice_code`'s decorated output. The
    // `// <path> (lines a-b)` header/truncation-marker lines that
    // function adds are prose, not code, but `order.ts (lines 3-25)`
    // would itself tokenize as a spurious call site (`ts(...)`,
    // `lines(...)`) if fed through `extract_calls` — scan only the
    // actual source lines each ref points at.
    let assembled = raw_code_text(source, code_refs)?;
    let mut seen = HashSet::new();
    let mut referenced: Vec<String> = Vec::new();
    for (bare, _display, _argc) in scan::extract_calls(&assembled) {
        if seen.insert(bare.clone()) {
            referenced.push(bare);
        }
    }
    let bindings = source::parse_import_bindings(own_file_content);
    let mut manifest = Vec::new();
    for name in referenced {
        if defined.contains(&name) {
            continue;
        }
        if repo_index.get(&name).is_some() {
            continue;
        }
        let module = bindings.get(&name).cloned().unwrap_or_else(|| "unknown".to_string());
        manifest.push(format!("referenced but not defined in bundle: {name} <- {module}"));
    }
    manifest.sort();
    Ok(manifest)
}

/// Group ascending, already-deduped 1-indexed line numbers into maximal
/// runs of consecutive integers: `[1,2,3,7,8,12]` -> `[(1,3),(7,8),(12,12)]`.
/// (#1605 follow-up) Used to bundle changed lines with no enclosing function
/// as locally-scoped units — a top-of-file import block and a tail
/// invocation chain shouldn't collapse into one span stretching across
/// everything an enclosing function already covers in between.
fn contiguous_runs(sorted: &[u32]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut iter = sorted.iter().copied();
    let Some(first) = iter.next() else { return out };
    let mut start = first;
    let mut prev = first;
    for ln in iter {
        if ln == prev + 1 {
            prev = ln;
        } else {
            out.push((start, prev));
            start = ln;
            prev = ln;
        }
    }
    out.push((start, prev));
    out
}

/// Build the full `BundleSet` for `diff` read against `source`. Direct
/// port of `build_bundles(worktree, diff_text)`, generalized over
/// `FileSource` fidelity and extended with the manifest + truncation
/// bookkeeping (#1222 packet 3).
pub fn build_bundles(source: &FileSource, diff_text: &str) -> Result<BundleSet> {
    let files = diff::parse_diff(diff_text);
    // (#1605) Per-file decline accounting — see `BundleSkipReport`'s doc.
    // `files_considered` is the diff's own file count, fixed up front; every
    // early `continue` below that leaves a file with zero bundles records WHY
    // into `skip.files_skipped` before moving on.
    let mut skip = BundleSkipReport { files_considered: files.len(), files_skipped: Vec::new() };
    let candidate_files = source
        .candidate_files(diff_text)
        .context("resolving candidate files for the repo-wide function index")?;
    let repo_index = facts::build_repo_index(source, &candidate_files)?;

    let mut global_added_calls: HashSet<String> = HashSet::new();
    for (_path, hunks) in &files {
        for h in hunks {
            let added_text = h.added.join("\n");
            for (name, _display, _argc) in scan::extract_calls(&added_text) {
                global_added_calls.insert(name);
            }
        }
    }

    let mut bundles: Vec<Bundle> = Vec::new();
    let mut seen_fns: HashSet<(String, usize, usize)> = HashSet::new();

    for (rel, hunks) in &files {
        if !scan::ts_file(rel) {
            // (#1605 QA finding) Distinguish "not code I understand" from
            // "code I deliberately exclude" — a `.ts`/`.tsx` file rejected
            // here was rejected for being a TEST, not for being data.
            let reason = if rel.ends_with(".ts") || rel.ends_with(".tsx") {
                SkipReason::TestFileExcluded
            } else {
                SkipReason::NonCodeExtension
            };
            skip.files_skipped.push(SkippedFile { path: rel.clone(), reason, function: None });
            continue;
        }
        let Some(content) = source.read_file(rel)? else {
            skip.files_skipped.push(SkippedFile {
                path: rel.clone(),
                reason: SkipReason::UnreadableInWorktree,
                function: None,
            });
            continue;
        };
        let lines: Vec<String> = content.lines().map(str::to_string).collect();

        let mut changed_new_lines: HashSet<u32> = HashSet::new();
        // (#1605 follow-up, QA finding) `new_lines` deliberately mixes
        // added lines AND unchanged context lines (so a changed function
        // can be located via either) — that's still exactly what we want
        // for `sorted_lines` below, which feeds `enclosing_fn_for_line`.
        // But a line with no enclosing function only belongs in a
        // top-level bundle if it was ACTUALLY ADDED — an unchanged import
        // line that merely fell inside a hunk's context window is not
        // "changed code" and must not be handed to a seat as if it were.
        // `added_new_lines` is that narrower set, checked below.
        let mut added_new_lines: HashSet<u32> = HashSet::new();
        for h in hunks {
            changed_new_lines.extend(h.new_lines.iter().copied());
            added_new_lines.extend(h.added_line_numbers.iter().copied());
        }
        if changed_new_lines.is_empty() {
            skip.files_skipped.push(SkippedFile {
                path: rel.clone(),
                reason: SkipReason::NoSurvivingLines,
                function: None,
            });
            continue;
        }

        let all_fns = scan::find_all_functions_in_text(&lines);
        let mut sorted_lines: Vec<u32> = changed_new_lines.into_iter().collect();
        sorted_lines.sort_unstable();
        let mut found_fns: Vec<(usize, usize, String, String)> = Vec::new();
        let mut found_keys: HashSet<(usize, usize)> = HashSet::new();
        // (#1605 follow-up) A changed line with no enclosing function used to
        // hit this `continue` and vanish: it reached no seat, and nothing
        // recorded the loss (the file still yielded a bundle for whatever
        // functions it DID have, so `bundles > 0` and `skip` stayed clean).
        // Collected here instead — bundled as its own unit below (a
        // top-level statement is real changed code; it just has no function
        // to anchor on) rather than dropped. Gated on `added_new_lines`
        // (QA finding): `sorted_lines` walks `new_lines`, which — on
        // purpose, see its doc — mixes added lines with unchanged CONTEXT
        // lines so a changed function can be located via either. An
        // unenclosed CONTEXT line is not itself changed code (it merely
        // fell inside a hunk's span); only an unenclosed line that was
        // actually ADDED belongs in a top-level bundle.
        let mut unenclosed_lines: Vec<u32> = Vec::new();
        for ln in sorted_lines {
            let Some(fndef) = scan::enclosing_fn_for_line(&all_fns, ln) else {
                if added_new_lines.contains(&ln) {
                    unenclosed_lines.push(ln);
                }
                continue;
            };
            let key = (fndef.start0, fndef.end0);
            if found_keys.insert(key) {
                found_fns.push((fndef.start0, fndef.end0, fndef.header.clone(), fndef.name.clone()));
            }
        }
        if found_fns.is_empty() && unenclosed_lines.is_empty() {
            // Reachable in one narrow case: a hunk with NO added lines at
            // all (a pure deletion, or a context-only hunk) whose context
            // lines all fall outside every function — there is no
            // actually-added code to bundle, so `unenclosed_lines` stays
            // empty even though `sorted_lines` (context included) was not.
            // That's correct, not a gap: an unenclosed CONTEXT line was
            // never changed code, so silently excluding it from
            // `unenclosed_lines` above loses nothing worth recording.
            // Otherwise defensive: `sorted_lines` is non-empty here (the
            // `NoSurvivingLines` check above already handled the empty
            // case), and every line in it either lands in `found_fns`, or
            // is unenclosed-and-added (landing in `unenclosed_lines`), or
            // is unenclosed-and-context (correctly dropped, per above) —
            // kept as a defensive decline for this last combination, see
            // `SkipReason::NoEnclosingFunction`'s doc, rather than a silent
            // fallthrough if that invariant ever changes.
            skip.files_skipped.push(SkippedFile {
                path: rel.clone(),
                reason: SkipReason::NoEnclosingFunction,
                function: None,
            });
            continue;
        }

        // (#1751) Each over-cap function records its OWN decline the
        // instant it's skipped, below — no file-level "did anything come
        // out of this file's functions at all" inference needed anymore
        // (the top-level-run loop just below does its own, separate,
        // per-run accounting the same way).
        for (start0, end0, _header, name) in found_fns {
            let seen_key = (rel.clone(), start0, end0);
            if seen_fns.contains(&seen_key) {
                continue;
            }
            seen_fns.insert(seen_key);
            if end0 - start0 > 300 {
                // (#1751) Recorded PER FUNCTION, immediately — independent
                // of whether a sibling function in this same file (or a
                // top-level run) already bundled successfully. Previously
                // this loss was only recorded when EVERY function in the
                // file hit the cap (inferred after the loop from "zero new
                // bundles came out of this file"), so a file with one
                // small function and one huge one silently dropped the
                // huge one's changed lines with no skip entry at all —
                // the file still "worked" because its small function
                // bundled.
                skip.files_skipped.push(SkippedFile {
                    path: rel.clone(),
                    reason: SkipReason::OverSizeCap,
                    function: Some(format!("{name} (lines {}-{})", start0 as u32 + 1, end0 as u32 + 1)),
                });
                continue;
            }

            let fn_lines: Vec<String> = lines[start0..=end0].to_vec();
            let params = scan::extract_params(&lines, start0);
            let default_params = scan::extract_param_defaults(&lines, start0);
            let bundle_id = format!("{name}@{rel}");

            let mut code_refs = vec![BundleRef {
                path: rel.clone(),
                start: start0 as u32 + 1,
                end: end0 as u32 + 1,
            }];

            // Callee code refs emit in FIRST-CALL-APPEARANCE order —
            // `resolve_callees` returns the same insertion order the
            // Python reference's dict iterates in, so ref ordering
            // matches the reference (and is deterministic).
            let callees = facts::resolve_callees(&fn_lines, &repo_index, rel);
            for (_cname, cdef) in &callees {
                let clen = cdef.end0 - cdef.start0 + 1;
                if clen <= facts::MAX_CALLEE_BODY_LINES {
                    code_refs.push(BundleRef {
                        path: cdef.path.clone(),
                        start: cdef.start0 as u32 + 1,
                        end: cdef.end0 as u32 + 1,
                    });
                } else {
                    code_refs.push(BundleRef {
                        path: cdef.path.clone(),
                        start: cdef.start0 as u32 + 1,
                        end: cdef.start0 as u32 + 1,
                    });
                }
            }

            let caller_refs = find_caller_refs(source, &candidate_files, &name, rel, (start0, end0))?;
            code_refs.extend(caller_refs);

            let siblings = facts::find_siblings(&name, rel, &repo_index);
            for s in &siblings {
                code_refs.push(BundleRef {
                    path: s.path.clone(),
                    start: s.start0 as u32 + 1,
                    end: s.start0 as u32 + 1,
                });
            }

            // Name-keyed lookup view over the ordered callee pairs — the
            // fact builder only ever looks up by name; ordering lives in
            // the Vec above.
            let callee_index: std::collections::HashMap<String, &facts::FnRecord> =
                callees.iter().map(|(n, d)| (n.clone(), *d)).collect();
            let pf_facts =
                facts::build_param_flow_facts(&fn_lines, &params, &default_params, &callee_index);

            let mut fn_old_block: Option<&Vec<String>> = None;
            for h in hunks {
                if h.new_start <= (end0 as u32 + 1) && (h.new_start + h.new_block.len() as u32) >= (start0 as u32 + 1)
                {
                    fn_old_block = Some(&h.old_block);
                    break;
                }
            }
            let diff_facts = match fn_old_block {
                Some(block) => facts::build_differential_facts(block, &global_added_calls),
                None => Vec::new(),
            };

            let sib_facts = facts::build_siblings_facts(&siblings);

            let own_file_content = lines.join("\n");
            let mut manifest = build_manifest(source, &code_refs, &repo_index, &own_file_content)?;
            // A GithubApi source that hit the MAX_API_FILES hard cap has
            // an INCOMPLETE repo index — every bundle built against it
            // carries the truncation on the artifact itself, not just in
            // the stderr log (an unresolvable symbol might have resolved
            // in one of the unscanned files).
            let unscanned = source.unscanned_file_count();
            if unscanned > 0 {
                manifest.push(format!("file budget exceeded: {unscanned} files not scanned"));
            }
            let mut truncated = false;
            for r in &code_refs {
                if truncated_extent(source, r)?.is_some() {
                    truncated = true;
                    break;
                }
            }

            let (has_pf, has_diff, has_sib) =
                (!pf_facts.is_empty(), !diff_facts.is_empty(), !sib_facts.is_empty());
            if has_pf {
                bundles.push(Bundle {
                    id: bundle_id.clone(),
                    code: code_refs.clone(),
                    facts: pf_facts,
                    fact_family: "param-flow".to_string(),
                    manifest: manifest.clone(),
                    truncated,
                });
            }
            if has_diff {
                bundles.push(Bundle {
                    id: bundle_id.clone(),
                    code: code_refs.clone(),
                    facts: diff_facts,
                    fact_family: "differential".to_string(),
                    manifest: manifest.clone(),
                    truncated,
                });
            }
            if has_sib {
                bundles.push(Bundle {
                    id: bundle_id.clone(),
                    code: code_refs.clone(),
                    facts: sib_facts,
                    fact_family: "siblings".to_string(),
                    manifest: manifest.clone(),
                    truncated,
                });
            }
            if !(has_pf || has_diff || has_sib) {
                bundles.push(Bundle {
                    id: bundle_id,
                    code: code_refs,
                    facts: Vec::new(),
                    fact_family: "hunk".to_string(),
                    manifest,
                    truncated,
                });
            }
        }

        // (#1605 follow-up) `unenclosed_lines` is every changed line that
        // fell outside a function above — grouped into maximal runs of
        // consecutive line numbers so a top-of-file import block and a tail
        // invocation chain (two unrelated regions separated by the function
        // the earlier loop already bundled) become two separate, locally-
        // scoped bundles rather than one span swallowing everything between
        // them. Same size cap as a function (`end0 - start0 > 300`); a run
        // over the cap is declined per-run, immediately, regardless of
        // whether this file's functions (or another run) already
        // contributed bundles — see `SkipReason::TopLevelOverSizeCap`'s doc
        // (#1751 brought the function-cap check just above to this exact
        // same per-decline-point recording, so both loops now behave the
        // same way here).
        for (run_start, run_end) in contiguous_runs(&unenclosed_lines) {
            if run_end - run_start > 300 {
                skip.files_skipped.push(SkippedFile {
                    path: rel.clone(),
                    reason: SkipReason::TopLevelOverSizeCap,
                    function: Some(format!("toplevel (lines {run_start}-{run_end})")),
                });
                continue;
            }
            let code_refs = vec![BundleRef { path: rel.clone(), start: run_start, end: run_end }];
            let own_file_content = lines.join("\n");
            let mut manifest = build_manifest(source, &code_refs, &repo_index, &own_file_content)?;
            let unscanned = source.unscanned_file_count();
            if unscanned > 0 {
                manifest.push(format!("file budget exceeded: {unscanned} files not scanned"));
            }
            bundles.push(Bundle {
                // Deliberately not `"<fn>@<path>"` — there is no function
                // name here. The `start-end` suffix keeps ids unique across
                // multiple runs in the same file and can't collide with a
                // real function id (a TS/JS identifier can't contain `:`).
                id: format!("toplevel:{run_start}-{run_end}@{rel}"),
                code: code_refs,
                facts: Vec::new(),
                fact_family: "toplevel".to_string(),
                manifest,
                // Not `truncated_extent`-checked: that helper answers "does
                // this ref show less than an enclosing function/callee's
                // full extent" — a category error for a ref whose span IS
                // by definition its full declared extent (the changed-line
                // run itself, not a stub of something longer).
                truncated: false,
            });
        }
    }

    Ok(BundleSet { bundles, skip })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &std::path::Path, rel: &str, content: &str) {
        let full = dir.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, content).unwrap();
    }

    // ── slice_code_probe: Phase A probe-format parity (#1256) ─────────
    //
    // Golden provenance: every expected string below was captured by
    // RUNNING probe-runner.py's real `read_code_excerpt` (and, for the
    // multi-ref join/dedupe cases, its `build_prompt`'s ref handling) on
    // this exact synthetic two-function fixture during the #1256
    // correction round — not hand-transcribed from a reading of the
    // Python.

    /// The synthetic fixture file — identical to the worktree file the
    /// python reference was run against.
    const PROBE_FIXTURE_TS: &str = "export function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n\nexport function shouldRetry(attempt: number, maxAttempts: number): boolean {\n  return attempt < maxAttempts;\n}\n";

    fn probe_fixture_source() -> (TempDir, FileSource) {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "src/example.ts", PROBE_FIXTURE_TS);
        let source = FileSource::worktree(dir.path());
        (dir, source)
    }

    #[test]
    fn slice_code_probe_single_ref_matches_read_code_excerpt_golden() {
        let (_dir, source) = probe_fixture_source();
        let refs = [BundleRef { path: "src/example.ts".to_string(), start: 1, end: 4 }];
        // read_code_excerpt golden, verbatim.
        let golden = "### `src/example.ts` (lines 1-4)\n```typescript\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n```";
        assert_eq!(slice_code_probe(&source, &refs).unwrap(), golden);
    }

    #[test]
    fn slice_code_probe_dedupes_refs_and_joins_blocks_with_a_blank_line() {
        let (_dir, source) = probe_fixture_source();
        let refs = [
            BundleRef { path: "src/example.ts".to_string(), start: 1, end: 4 },
            // Exact duplicate — dedupe_refs drops it (first-seen wins).
            BundleRef { path: "src/example.ts".to_string(), start: 1, end: 4 },
            BundleRef { path: "src/example.ts".to_string(), start: 6, end: 8 },
        ];
        // build_prompt's code_section golden for the same three refs.
        let golden = "### `src/example.ts` (lines 1-4)\n```typescript\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n```\n\n### `src/example.ts` (lines 6-8)\n```typescript\nexport function shouldRetry(attempt: number, maxAttempts: number): boolean {\n  return attempt < maxAttempts;\n}\n```";
        assert_eq!(slice_code_probe(&source, &refs).unwrap(), golden);
    }

    #[test]
    fn slice_code_probe_clamps_the_header_to_the_files_real_extent() {
        // Unlike slice_code (judge format), whose header echoes the ref's
        // own out-of-range end verbatim, read_code_excerpt clamps BOTH the
        // slice and the header: end=100 on an 8-line file renders as
        // `(lines 1-8)`. Golden from running read_code_excerpt on this ref.
        let (_dir, source) = probe_fixture_source();
        let refs = [BundleRef { path: "src/example.ts".to_string(), start: 1, end: 100 }];
        let golden = "### `src/example.ts` (lines 1-8)\n```typescript\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n\nexport function shouldRetry(attempt: number, maxAttempts: number): boolean {\n  return attempt < maxAttempts;\n}\n```";
        assert_eq!(slice_code_probe(&source, &refs).unwrap(), golden);
    }

    #[test]
    fn slice_code_probe_skips_unreadable_and_past_eof_refs_entirely() {
        // Python returns None for an unreadable file OR a start past EOF,
        // and build_prompt drops the block — no placeholder, no header.
        let (_dir, source) = probe_fixture_source();
        let refs = [
            BundleRef { path: "src/missing.ts".to_string(), start: 1, end: 3 },
            BundleRef { path: "src/example.ts".to_string(), start: 99, end: 120 },
            BundleRef { path: "src/example.ts".to_string(), start: 6, end: 8 },
        ];
        let out = slice_code_probe(&source, &refs).unwrap();
        assert!(
            out.starts_with("### `src/example.ts` (lines 6-8)"),
            "skipped refs leave no trace — the surviving block is the whole output: {out}"
        );
        assert!(!out.contains("missing.ts"));
        assert!(!out.contains("unreadable"));
    }

    #[test]
    fn manifest_flags_unresolvable_symbol() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "src/order.ts",
            "import { computeTotal } from './pricing';\n\
             import { mysteryHelper } from 'some-external-pkg';\n\
             \n\
             function placeOrder(items) {\n\
             \u{20}\u{20}const total = computeTotal(items);\n\
             \u{20}\u{20}mysteryHelper(total);\n\
             \u{20}\u{20}return total;\n\
             }\n",
        );
        write(
            dir.path(),
            "src/pricing.ts",
            "export function computeTotal(items) {\n  return items.length;\n}\n",
        );
        let diff = "+++ b/src/order.ts\n\
@@ -1,3 +1,8 @@\n\
+import { computeTotal } from './pricing';\n\
+import { mysteryHelper } from 'some-external-pkg';\n\
+\n\
+function placeOrder(items) {\n\
+  const total = computeTotal(items);\n\
+  mysteryHelper(total);\n\
+  return total;\n\
+}\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).unwrap();
        let manifest_lines: Vec<&String> = set.bundles.iter().flat_map(|b| b.manifest.iter()).collect();
        assert!(
            manifest_lines
                .iter()
                .any(|l| l.contains("mysteryHelper") && l.contains("some-external-pkg")),
            "expected a manifest line for mysteryHelper, got: {manifest_lines:?}"
        );
        assert!(
            !manifest_lines.iter().any(|l| l.contains("computeTotal")),
            "computeTotal is resolvable in-repo and must not be manifested: {manifest_lines:?}"
        );
    }

    #[test]
    fn truncated_callee_marks_bundle_and_slice_marker() {
        let dir = TempDir::new().unwrap();
        let mut long_body = String::from("export function longHelper(x) {\n");
        for i in 0..50 {
            long_body.push_str(&format!("  console.log({i});\n"));
        }
        long_body.push_str("  return x;\n}\n");
        write(dir.path(), "src/helpers.ts", &long_body);
        write(
            dir.path(),
            "src/caller.ts",
            "import { longHelper } from './helpers';\n\
             function useIt(x) {\n\
             \u{20}\u{20}return longHelper(x);\n\
             }\n",
        );
        let diff = "+++ b/src/caller.ts\n\
@@ -1,2 +1,4 @@\n\
+import { longHelper } from './helpers';\n\
+function useIt(x) {\n\
+  return longHelper(x);\n\
+}\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).unwrap();
        assert!(!set.bundles.is_empty());
        let b = &set.bundles[0];
        assert!(b.truncated, "expected truncated=true, bundle: {b:?}");
        let code_text = slice_code(&source, &b.code).unwrap();
        assert!(
            code_text.contains("excerpt truncated"),
            "expected a truncation marker in slice_code output:\n{code_text}"
        );
    }

    #[test]
    fn build_bundles_on_empty_diff_returns_empty_bundle_set() {
        let dir = TempDir::new().unwrap();
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, "").unwrap();
        assert!(set.bundles.is_empty(), "an empty diff must yield an empty (not error, not null) bundle set");
    }

    #[test]
    fn build_bundles_ignores_non_ts_file_changes() {
        // A diff touching only a non-`.ts`/`.tsx` file (`scan::ts_file`
        // filters it) must produce zero bundles — the file is skipped
        // entirely, not treated as an unreadable/errored input.
        let dir = TempDir::new().unwrap();
        write(dir.path(), "src/config.json", "{\"a\": 1}\n");
        let diff = "+++ b/src/config.json\n\
@@ -1,1 +1,1 @@\n\
+{\"a\": 2}\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).unwrap();
        assert!(set.bundles.is_empty(), "a non-TS-file-only diff must yield zero bundles, got: {:?}", set.bundles);
    }

    // ── (#1605) bundler decline-path accounting ────────────────────────
    //
    // darkmux#1605: `bundles: 0` plus a fixed string couldn't distinguish
    // "diff was entirely non-code" from "bundler bug" from "diff exceeded
    // an internal bound". These tests pin the structured breakdown
    // (`BundleSet::skip`) that now makes each decline path self-diagnosing
    // — asserting the REASON tallies, not just the zero-bundles total.

    #[test]
    fn build_bundles_on_diff_of_only_non_code_files_reports_skip_breakdown_by_reason() {
        // Two non-code files (a lockfile-shaped JSON, a config-shaped JSON)
        // — the darkmux#1605 report's own example ("diffs dominated by
        // JSON/fixture content"). Every file must land in `files_skipped`
        // tagged `NonCodeExtension`, and `files_considered` must match the
        // diff's real file count.
        let dir = TempDir::new().unwrap();
        write(dir.path(), "package-lock.json", "{\"lockfileVersion\": 2}\n");
        write(dir.path(), "fixtures/sample.json", "{\"a\": 1}\n");
        let diff = "+++ b/package-lock.json\n\
@@ -1,1 +1,1 @@\n\
+{\"lockfileVersion\": 3}\n\
+++ b/fixtures/sample.json\n\
@@ -1,1 +1,1 @@\n\
+{\"a\": 2}\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).unwrap();
        assert!(set.bundles.is_empty(), "a non-code-only diff must still yield zero bundles");
        assert_eq!(
            set.skip.files_considered, 2,
            "both diffed files must be counted as considered, got: {:?}",
            set.skip
        );
        assert_eq!(
            set.skip.files_skipped.len(),
            2,
            "both files must be recorded as skipped, got: {:?}",
            set.skip
        );
        let non_code_extension_count = set
            .skip
            .files_skipped
            .iter()
            .filter(|f| f.reason == SkipReason::NonCodeExtension)
            .count();
        assert_eq!(
            non_code_extension_count, 2,
            "both skips must carry the NonCodeExtension reason specifically \
             (never just a bare total), got: {:?}",
            set.skip
        );
        let paths: Vec<&str> = set.skip.files_skipped.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"package-lock.json"));
        assert!(paths.contains(&"fixtures/sample.json"));
    }

    #[test]
    fn build_bundles_separates_excluded_test_files_from_non_code_data() {
        // (#1605 QA finding) `scan::ts_file` rejects BOTH data files and
        // TypeScript TEST files, and the first cut tagged them identically —
        // so a pure-test-file PR got a no-op comment calling its `.test.ts`
        // sources "fixtures, lockfiles, or generated config". Benign either
        // way; the LABEL was false, in the one comment whose entire purpose
        // is explaining honestly why nothing was reviewed.
        //
        // This drives the real `build_bundles` ASSIGNMENT. A sibling test in
        // review_tests.rs pins the CLASSIFIER for the same case — that one
        // constructs the reason directly, so on its own it would pass against
        // an implementation that never produces `TestFileExcluded` at all.
        // Both halves are needed; neither alone is evidence.
        let dir = TempDir::new().unwrap();
        write(dir.path(), "src/foo.test.ts", "export const a = 1;\n");
        write(dir.path(), "package-lock.json", "{}\n");
        let diff = "+++ b/src/foo.test.ts\n\
@@ -1,1 +1,1 @@\n\
+export const a = 2;\n\
+++ b/package-lock.json\n\
@@ -1,1 +1,1 @@\n\
+{\"v\": 2}\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).unwrap();

        let by_path: std::collections::BTreeMap<&str, SkipReason> =
            set.skip.files_skipped.iter().map(|f| (f.path.as_str(), f.reason)).collect();
        assert_eq!(
            by_path.get("src/foo.test.ts"),
            Some(&SkipReason::TestFileExcluded),
            "a .test.ts file is EXCLUDED code, not non-code data: {:?}",
            set.skip
        );
        assert_eq!(
            by_path.get("package-lock.json"),
            Some(&SkipReason::NonCodeExtension),
            "a lockfile is genuinely non-code and must keep that reason: {:?}",
            set.skip
        );
    }

    #[test]
    fn build_bundles_reports_unreadable_file_skip_reason() {
        // The diff names a file that was never written to the worktree —
        // `source.read_file` returns `None` — distinct from a non-code
        // extension: this file WOULD have been considered.
        let dir = TempDir::new().unwrap();
        let diff = "+++ b/src/missing.ts\n\
@@ -1,1 +1,1 @@\n\
+export const x = 2;\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).unwrap();
        assert!(set.bundles.is_empty());
        assert_eq!(set.skip.files_skipped.len(), 1);
        assert_eq!(set.skip.files_skipped[0].reason, SkipReason::UnreadableInWorktree);
    }

    #[test]
    fn build_bundles_bundles_a_top_level_only_change_instead_of_dropping_it() {
        // Every changed line sits at top level (no function wraps it) — the
        // scanner finds functions in the file, but none of them enclose the
        // changed line. This used to hit `SkipReason::NoEnclosingFunction`
        // with zero bundles (the exact defect the `toplevel::tests` fixture
        // pins at a larger scale, #1605 follow-up); the fix bundles the
        // unenclosed line as its own `"toplevel"`-family unit instead, so
        // this is the smallest possible reproduction of the same fix.
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "src/consts.ts",
            "export const A = 1;\nexport function untouched() {\n  return 0;\n}\n",
        );
        let diff = "+++ b/src/consts.ts\n\
@@ -1,1 +1,1 @@\n\
+export const A = 2;\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).unwrap();
        assert_eq!(set.bundles.len(), 1, "the top-level line must reach a seat, got: {:?}", set.bundles);
        assert_eq!(set.bundles[0].fact_family, "toplevel");
        assert_eq!(
            set.bundles[0].code,
            vec![BundleRef { path: "src/consts.ts".to_string(), start: 1, end: 1 }]
        );
        assert!(
            set.skip.files_skipped.is_empty(),
            "a successfully-bundled top-level line must not ALSO be recorded as declined: {:?}",
            set.skip
        );
    }

    #[test]
    fn build_bundles_reports_over_size_cap_skip_reason() {
        // A single changed function whose body exceeds the 300-line cap —
        // real code, mechanically declined for an internal size limit, not
        // a benign "nothing here".
        let dir = TempDir::new().unwrap();
        let mut body = String::from("export function hugeFn(x) {\n");
        for i in 0..320 {
            body.push_str(&format!("  console.log({i});\n"));
        }
        body.push_str("  return x;\n}\n");
        write(dir.path(), "src/huge.ts", &body);
        let diff = "+++ b/src/huge.ts\n\
@@ -1,1 +1,1 @@\n\
+  console.log(0);\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).unwrap();
        assert!(set.bundles.is_empty(), "an over-cap function must yield zero bundles, got: {:?}", set.bundles);
        assert_eq!(set.skip.files_skipped.len(), 1);
        assert_eq!(set.skip.files_skipped[0].reason, SkipReason::OverSizeCap);
    }

    #[test]
    fn build_bundles_skips_wholly_removed_function_no_context() {
        // A hunk with ONLY removed lines and no surviving context/added
        // line carries no `new_lines` at all —
        // `changed_new_lines.is_empty()` short-circuits the whole file —
        // so a function deleted in its entirety (pre-image only, no
        // post-image counterpart anywhere in the hunk) never produces a
        // bundle for itself OR for anything else in the file.
        let dir = TempDir::new().unwrap();
        write(dir.path(), "src/legacy.ts", "function foo(a) {\n  return a;\n}\n");
        let diff = "+++ b/src/legacy.ts\n\
@@ -4,4 +3,0 @@\n\
-\n\
-function bar(b) {\n\
-  return b;\n\
-}\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).unwrap();
        assert!(
            set.bundles.is_empty(),
            "a hunk with only removed lines (no context/added line) must yield zero bundles, got: {:?}",
            set.bundles
        );
        // (#1605) The decline reason is NoSurvivingLines specifically, not a
        // bare zero — this is a real, honest decline, never mistaken for
        // NonCodeExtension or a bundler bug.
        assert_eq!(set.skip.files_skipped.len(), 1);
        assert_eq!(set.skip.files_skipped[0].reason, SkipReason::NoSurvivingLines);
    }

    #[test]
    fn differential_family_absent_for_brand_new_file() {
        // A whole new file (`@@ -0,0 +1,N @@`, every line added) has no
        // pre-image at all — `Hunk::old_block` stays empty for it, and
        // `build_differential_facts` short-circuits on an empty
        // `fn_old_block` — so a brand-new file must never emit a
        // "differential" family bundle, even though it can (and here
        // does) emit a "param-flow" bundle for the same function.
        let dir = TempDir::new().unwrap();
        write(dir.path(), "src/newfile.ts", "function greet(name) {\n  return name;\n}\n");
        let diff = "+++ b/src/newfile.ts\n\
@@ -0,0 +1,3 @@\n\
+function greet(name) {\n\
+  return name;\n\
+}\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).unwrap();
        assert!(!set.bundles.is_empty(), "expected at least one bundle for the new function");
        assert!(
            !set.bundles.iter().any(|b| b.fact_family == "differential"),
            "a brand-new file (no pre-image) must never produce a differential-family bundle, got: {:?}",
            set.bundles
        );
    }

    #[test]
    fn manifest_empty_when_every_symbol_resolves() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "src/order.ts",
            "import { computeTotal } from './pricing';\n\
             \n\
             function placeOrder(items) {\n\
             \u{20}\u{20}const total = computeTotal(items);\n\
             \u{20}\u{20}return total;\n\
             }\n",
        );
        write(
            dir.path(),
            "src/pricing.ts",
            "export function computeTotal(items) {\n  return items.length;\n}\n",
        );
        let diff = "+++ b/src/order.ts\n\
@@ -0,0 +1,6 @@\n\
+import { computeTotal } from './pricing';\n\
+\n\
+function placeOrder(items) {\n\
+  const total = computeTotal(items);\n\
+  return total;\n\
+}\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).unwrap();
        assert!(!set.bundles.is_empty(), "expected at least one bundle for placeOrder");
        for b in &set.bundles {
            assert!(
                b.manifest.is_empty(),
                "expected an empty manifest when every referenced symbol resolves, got: {:?}",
                b.manifest
            );
        }
        // `#[serde(default, skip_serializing_if = "Vec::is_empty")]` on
        // `Bundle::manifest` — an empty manifest must not even appear as
        // a literal `"manifest": []` key in the serialized JSON.
        let json = serde_json::to_string(&set).unwrap();
        assert!(
            !json.contains("\"manifest\""),
            "an empty manifest field must be omitted from serialized JSON entirely, got: {json}"
        );
    }

    #[test]
    fn slice_code_full_file_and_out_of_bounds_end_are_characterized() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "src/tiny.ts", "function foo(a) {\n  return a;\n}\n");
        let source = FileSource::worktree(dir.path());

        // Ref A: start=1, end=3 — the FULL file, exactly matching the
        // function's real extent. No truncation marker expected.
        let full_ref = BundleRef { path: "src/tiny.ts".to_string(), start: 1, end: 3 };
        let full_text = slice_code(&source, std::slice::from_ref(&full_ref)).unwrap();
        assert!(full_text.contains("function foo(a) {"));
        assert!(full_text.contains("(lines 1-3)"));
        assert!(!full_text.contains("excerpt truncated"));

        // Ref B: end=100, far beyond the file's 3 lines. `slice_code`
        // must clamp the SLICE it actually renders (`end_idx =
        // r.end.min(lines.len())`, no panic on an out-of-bounds index),
        // but the `// <path> (lines a-b)` HEADER line it prints is not
        // clamped — it echoes the ref's own out-of-range numbers
        // verbatim. Characterized as current behavior (documentation of
        // the requested span vs. the actually-available content), not
        // asserted as a defect.
        let oob_ref = BundleRef { path: "src/tiny.ts".to_string(), start: 1, end: 100 };
        let oob_text = slice_code(&source, std::slice::from_ref(&oob_ref)).unwrap();
        assert!(
            oob_text.contains("(lines 1-100)"),
            "header echoes the ref's own out-of-range end verbatim, got: {oob_text}"
        );
        assert!(!oob_text.contains("excerpt truncated"));
        let rendered_source_lines =
            oob_text.lines().filter(|l| !l.starts_with("//") && !l.trim().is_empty()).count();
        assert_eq!(
            rendered_source_lines, 3,
            "only the file's 3 real lines may render despite end=100 (no fabricated/panicking OOB read), got:\n{oob_text}"
        );
    }

    // ── Top-level statements are dropped from every bundle ────────────────
    //
    // `build_bundles` maps each changed line to its ENCLOSING FUNCTION and
    // makes that function's extent the excerpt handed to the seats. A changed
    // line with no enclosing function hits the `continue` above and reaches
    // no seat at all — and because the file still yields a bundle for its
    // functions, nothing in `BundleSkipReport` records the loss either.
    //
    // The shape below is the one that made this visible in production: a
    // script whose only function is `main`, invoked by a top-level
    // `main().then(...).catch(...)` chain. A reviewer seat asked about that
    // chain answers from a window that does not contain it, and answers
    // confidently, because nothing marks the excerpt as partial.
    //
    // Written as a synthetic reproduction on purpose — the production case
    // was proprietary, and the defect needs none of it.

    const TOPLEVEL_TAIL_TS: &str = r#"import { AppDataSource } from "./data-source.js"

const LIMIT = 10

async function main(): Promise<number> {
  await AppDataSource.initialize()
  if (LIMIT > 0) {
    return 0
  }
  return 1
}

main()
  .then((code) => {
    // exitCode, never exit() — stdout is non-blocking on a pipe and
    // exit() truncates it.
    process.exitCode = code
  })
  .catch((err) => {
    process.stderr.write(`could not run: ${String(err)}\n`)
    process.exitCode = 2
  })
"#;

    /// Build a `new file` diff for `content` — every line added, one hunk,
    /// exactly what a PR that introduces a script produces.
    fn new_file_diff(rel: &str, content: &str) -> String {
        let n = content.lines().count();
        let mut d = format!(
            "diff --git a/{rel} b/{rel}\nnew file mode 100644\n--- /dev/null\n+++ b/{rel}\n@@ -0,0 +1,{n} @@\n"
        );
        for l in content.lines() {
            d.push('+');
            d.push_str(l);
            d.push('\n');
        }
        d
    }

    /// 1-indexed line numbers covered by any ref in any bundle.
    fn covered_lines(set: &BundleSet, rel: &str) -> std::collections::HashSet<u32> {
        let mut out = std::collections::HashSet::new();
        for b in &set.bundles {
            for r in &b.code {
                if r.path == rel {
                    for ln in r.start..=r.end {
                        out.insert(ln);
                    }
                }
            }
        }
        out
    }

    fn toplevel_fixture() -> (TempDir, FileSource, String, BundleSet) {
        let dir = TempDir::new().unwrap();
        let rel = "src/scripts/audit.ts";
        write(dir.path(), rel, TOPLEVEL_TAIL_TS);
        let source = FileSource::worktree(dir.path());
        let diff = new_file_diff(rel, TOPLEVEL_TAIL_TS);
        let set = build_bundles(&source, &diff).unwrap();
        (dir, source, rel.to_string(), set)
    }

    /// (QA gate finding) A hunk's `new_lines` includes unchanged CONTEXT
    /// lines, not only added ones — `diff.rs`'s own doc and its
    /// `parses_single_file_single_hunk` test both say so. Pre-fix that was
    /// harmless (an unenclosed line was dropped). Post-fix every unenclosed
    /// line becomes a bundle, so ordinary `-U3` context bleeding past a
    /// function boundary bundles UNCHANGED code and hands it to every seat
    /// as code under review.
    #[test]
    fn context_lines_do_not_become_toplevel_bundles() {
        let dir = TempDir::new().unwrap();
        let rel = "src/a.ts";
        write(dir.path(), rel,
            "import { x } from \"./x.js\"\n\nexport function foo() {\n  return x + 1\n}\n");
        // Edit one line INSIDE foo. Context (lines 1-2) is top-level and
        // unchanged; only line 4 is actually added.
        let diff = format!(
            "diff --git a/{rel} b/{rel}\n--- a/{rel}\n+++ b/{rel}\n@@ -1,5 +1,5 @@\n import {{ x }} from \"./x.js\"\n \n export function foo() {{\n+  return x + 2\n }}\n"
        );
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, &diff).unwrap();
        let toplevel: Vec<&str> =
            set.bundles.iter().filter(|b| b.fact_family == "toplevel").map(|b| b.id.as_str()).collect();
        assert!(
            toplevel.is_empty(),
            "unchanged context lines were bundled as changed code under review: {toplevel:?}"
        );
    }

    /// The INVERTED CASE, asserted first: the fixture really does bundle.
    /// Without this, the next test could pass trivially on a fixture that
    /// produced no bundles at all — a green that proves nothing.
    #[test]
    fn toplevel_control_the_function_body_is_bundled() {
        let (_dir, _source, rel, set) = toplevel_fixture();
        assert!(!set.bundles.is_empty(), "fixture produced no bundles at all — the test below would be vacuous");
        let covered = covered_lines(&set, &rel);
        let body = TOPLEVEL_TAIL_TS
            .lines()
            .position(|l| l.contains("await AppDataSource.initialize()"))
            .map(|i| i as u32 + 1)
            .expect("fixture line missing");
        assert!(covered.contains(&body), "a line inside `main` must be covered; covered = {covered:?}");
    }

    /// The defect. Every line of a new file is a changed line, so the
    /// top-level chain is in the diff — it is the BUNDLER that drops it.
    #[test]
    fn toplevel_statements_after_a_function_reach_a_seat() {
        let (_dir, _source, rel, set) = toplevel_fixture();
        let covered = covered_lines(&set, &rel);

        for needle in ["process.exitCode = code", ".catch((err) => {"] {
            let ln = TOPLEVEL_TAIL_TS
                .lines()
                .position(|l| l.contains(needle))
                .map(|i| i as u32 + 1)
                .expect("fixture line missing");
            assert!(
                covered.contains(&ln),
                "line {ln} ({needle:?}) reaches no seat — it is in the diff but no bundle ref covers it. \
                 Covered lines: {covered:?}"
            );
        }
    }

    /// The general form: a changed line must either be shown to a seat or be
    /// accounted for as declined. Silent loss is the property that lets a
    /// seat rule confidently on code it was never given.
    #[test]
    fn every_changed_line_is_either_covered_or_accounted_for() {
        let (_dir, _source, rel, set) = toplevel_fixture();
        let covered = covered_lines(&set, &rel);
        let total = TOPLEVEL_TAIL_TS.lines().count() as u32;
        let declined = set.skip.files_skipped.iter().any(|s| s.path == rel);

        let missing: Vec<u32> = (1..=total).filter(|ln| !covered.contains(ln)).collect();
        assert!(
            missing.is_empty() || declined,
            "{} of {total} changed lines are in no bundle and the file records no decline — \
             the loss is invisible to every consumer. Missing: {missing:?}",
            missing.len()
        );
    }

    /// Companion to `build_bundles_reports_over_size_cap_skip_reason`
    /// (functions) — the same cap, applied to a top-level run: a changed
    /// line with no enclosing function still doesn't reach a seat when its
    /// contiguous run is over 300 lines, but unlike the pre-fix behavior
    /// that decline is now RECORDED (#1605 follow-up) rather than silent.
    #[test]
    fn build_bundles_reports_top_level_over_size_cap_skip_reason() {
        let dir = TempDir::new().unwrap();
        // 320 top-level `const` lines — no function anywhere in the file, so
        // every one of them is an unenclosed changed line in a single
        // contiguous run over the 300-line cap.
        let mut content = String::new();
        for i in 0..320 {
            content.push_str(&format!("const c{i} = {i};\n"));
        }
        write(dir.path(), "src/huge_toplevel.ts", &content);
        let diff = new_file_diff("src/huge_toplevel.ts", &content);
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, &diff).unwrap();
        assert!(
            set.bundles.is_empty(),
            "an over-cap top-level run must yield zero bundles, got: {:?}",
            set.bundles
        );
        assert_eq!(set.skip.files_skipped.len(), 1, "got: {:?}", set.skip);
        assert_eq!(set.skip.files_skipped[0].reason, SkipReason::TopLevelOverSizeCap);
    }

    // ═══════════════════════════════════════════════════════════════════
    // (branch: bundler/coverage-gap-audit) COVERAGE-GAP AUDIT — WRITTEN
    // REPORT
    // ═══════════════════════════════════════════════════════════════════
    //
    // Audited against the invariant `every_changed_line_is_either_covered_
    // or_accounted_for` (above): for any diff, every changed line either
    // appears within some bundle's code refs, or is named in
    // `BundleSkipReport`. This report also covers two adjacent classes the
    // brief asked for: (a) a seat shown the WRONG code with no signal
    // (mis-resolved same-named callees), and (b) a mechanically-computed
    // SIGNAL (manifest/truncation) that never reaches the seat meant to
    // use it, even though the code itself does.
    //
    // Companion tests for (b) live in `crates/darkmux-lab/src/lab/
    // review_tests.rs` (`probe_seat_never_sees_the_truncation_marker_the_
    // judge_seat_gets_inline`, `manifest_never_reaches_the_probe_user_
    // message`) because they exercise `review.rs`'s prompt builders, which
    // this module doesn't depend on.
    //
    // ── RANKED FINDINGS (worst first) ───────────────────────────────────
    //
    // 1. CONFIRMED, WORST — Mixed-file over-cap silent loss
    //    (`build_bundles_silently_drops_an_over_cap_function_when_a_
    //    sibling_function_in_the_same_file_bundles`). A file with TWO
    //    changed functions, one under the 300-line cap and one over it:
    //    the small one bundles normally; the huge one's changed lines are
    //    dropped by the `if end0 - start0 > 300 { continue; }` in
    //    `build_bundles`'s function loop — and because the FILE still
    //    produced a bundle (from the small function), the `had_functions
    //    && bundles.len() == bundles_before_file` check that would record
    //    `SkipReason::OverSizeCap` is false, so NOTHING records the loss.
    //    This is exactly the gap `BundleSkipReport`'s own doc comment
    //    (lines ~174-191 above) already names as suspected-but-unverified
    //    ("That gap is pre-existing and NOT closed by
    //    `SkipReason::TopLevelOverSizeCap`... a mixed file isn't
    //    distinguished from a fully-covered one today") — this test is
    //    the first execution that confirms it. A reviewer investigating
    //    the huge function's change sees NO excerpt, NO skip reason, and
    //    a `bundles: N > 0` result that reads as "this file was
    //    reviewed." Fix direction: record a `SkipReason` variant (or an
    //    addition to the existing `OverSizeCap`/manifest) per DROPPED
    //    function, independent of whether the file's other functions
    //    succeeded.
    //    Python reference: bundler.py has the identical per-function
    //    300-line cap and (per its own description as a line-span-pointer
    //    emitter with no reporting layer at all) almost certainly shares
    //    this exact gap — the Rust `BundleSkipReport` structure is a
    //    packet-3/1605 addition with NO Python precedent, so there is
    //    nothing in the reference to have caught this either way. Not a
    //    port regression; a gap in a Rust-only feature that was already
    //    self-diagnosed as incomplete.
    //
    // 2. CONFIRMED — darkmux's own repository is unreviewable by this
    //    bundler (`build_bundles_treats_darkmuxs_own_rust_source_as_non_
    //    code`, `build_bundles_treats_common_non_ts_languages_as_non_
    //    code`). `scan::ts_file` accepts only `.ts`/`.tsx`; every other
    //    language — including the `.rs` files this very crate is written
    //    in, plus `.py`/`.go`/`.sh`/`.sql`/`.yml` — is classified
    //    `SkipReason::NonCodeExtension`. That variant's own doc comment
    //    frames the common case as "a diff dominated by JSON config,
    //    lockfiles, or fixture data" — true for those, but FALSE for a
    //    500-line Rust/Python/Go change, which is real code that
    //    genuinely needed review and got none. A PR to darkmux's own
    //    `crates/`/`src/`/`runtime/` trees gets a `bundles: 0` result
    //    that reads as benign no-op, identical in shape to a pure
    //    lockfile bump. Fix direction: at minimum, split
    //    `NonCodeExtension` (as `TestFileExcluded` was already split out,
    //    #1605 QA finding) into "genuinely non-code" vs. "code in a
    //    language this bundler doesn't parse" — the comment for the
    //    latter should say so honestly, the way `TestFileExcluded`'s does.
    //    Python reference: `bundler.py`'s `ts_file`/`iter_ts_files` gate
    //    on the identical `.ts`/`.tsx` extension pair (module doc: "A
    //    Rust port of the reference bundler.py... TypeScript-native"), so
    //    the reference shares the exact same blind spot — this is a
    //    property of the tool's scope (TS-only), not a port defect. What
    //    IS Rust-only is the misleading label on `NonCodeExtension`
    //    itself (a #1605 addition), which is where the fix belongs.
    //
    // 3. CONFIRMED — Ambiguous same-named callee resolution ignores the
    //    caller's own import and can silently attach the WRONG function
    //    body (`resolve_callees_ignores_the_explicit_import_and_can_
    //    attach_the_wrong_function_body`). `facts::resolve_callees`
    //    matches a call site to the repo-wide function index purely by
    //    NAME — it never consults `source::parse_import_bindings` (which
    //    the bundler already computes, just for the SEPARATE manifest
    //    feature) to check which file the caller actually imports the
    //    symbol from. When two files export a same-named function, the
    //    repo index's insertion order (for `Worktree`, `iter_ts_files`'
    //    alphabetical directory walk) silently picks one — and the test
    //    below confirms it is NOT necessarily the one the call site
    //    imports. The reviewer is shown a plausible, syntactically valid
    //    function body under the right name that the code never actually
    //    calls, while the REAL callee never reaches any seat at all. This
    //    is the "wrong content, not missing content" failure mode the
    //    brief calls out ("does anything verify those refs point at
    //    real, current content?") — the answer is no. Fix direction: when
    //    `parse_import_bindings` names a specific module for the call
    //    name, prefer the repo-index def whose path resolves from that
    //    binding before falling back to first-non-own-path.
    //    Python reference: near-certainly shares this — `bundler.py`'s
    //    `resolve_callees` is described as the direct ancestor of this
    //    function (facts.rs module doc: "Port of `resolve_callees`") and
    //    there is no mention anywhere in this codebase of the reference
    //    ever consulting import bindings for callee resolution (the
    //    binding-aware `parse_import_bindings` is itself a Rust-only,
    //    packet-3 addition built for the unrelated manifest feature).
    //    Likely a reference-inherited limitation, not a port regression —
    //    but a real one, and now on a fresh Rust-only piece of context
    //    (`parse_import_bindings`) that makes the fix cheap.
    //
    // 4. CONFIRMED — The PROBE seat (the seat that actually ORIGINATES a
    //    finding) never sees the truncation marker the JUDGE seat gets
    //    inline (`probe_seat_never_sees_the_truncation_marker_the_judge_
    //    seat_gets_inline`, in review_tests.rs). `bundle_inputs_from_set`
    //    renders the SAME code refs through two formatters: `slice_code`
    //    (-> `BundleInput.code`, the judge's rendering) embeds an
    //    explicit "excerpt truncated" marker as inline text when a callee
    //    ref is a header-only stub; `slice_code_probe` (->
    //    `BundleInput.probe_code`, the probe's rendering) never does —
    //    confirmed both by this new test and by the EXISTING golden
    //    `slice_code_probe_skips_unreadable_and_past_eof_refs_entirely`
    //    above ("no placeholder, no header"). So the seat most likely to
    //    raise a "this looks incomplete" finding has zero textual signal
    //    that its excerpt is a stub — the exact structural shape of the
    //    #1605 top-level-drop defect this audit was commissioned over,
    //    just for a DIFFERENT input (a truncated callee body instead of
    //    an unenclosed top-level statement), and pre-existing rather than
    //    newly introduced.
    //    Python reference: SHARED, and documented as such — `slice_code_
    //    probe`'s own doc comment says it is a byte-for-byte port of
    //    `probe-runner.py`'s `read_code_excerpt`, "Golden provenance:
    //    every expected string below was captured by RUNNING probe-
    //    runner.py's real `read_code_excerpt`." The Python probe seat has
    //    the identical blind spot. Not a port regression — but also not
    //    something any existing test frames as a REVIEW-QUALITY finding;
    //    the golden tests pin it as a formatting fact, not as "the
    //    finding-originating seat is blind to truncation."
    //
    // 5. CONFIRMED — The PROBE seat also never sees the external-symbol
    //    manifest (`manifest_never_reaches_the_probe_user_message`, in
    //    review_tests.rs). The JUDGE side of this exact exclusion is
    //    ALREADY known, documented, and pinned by an existing test
    //    (`manifest_never_reaches_the_dispatched_judge_prompt`, #1256,
    //    "match Phase A exactly" operator decision) — so that half is not
    //    a new finding. The PROBE side has the identical observable
    //    behavior (`probe_user_message` never reads `bundle.manifest`)
    //    but, unlike the judge's, it has NO doc paragraph and NO test
    //    anywhere pinning it as deliberate. Ranked below #4 because the
    //    judge precedent makes "deliberate, Phase-A-parity" the likely
    //    explanation here too — but nothing currently guards it, so a
    //    future edit could change it in either direction with zero
    //    signal either way.
    //    Python reference: `probe-runner.py`'s `build_prompt` has no
    //    manifest input to drop in the first place (`bundler.py`'s
    //    bundles carry no such field) — same story as the judge side.
    //
    // 6. CONFIRMED, UNPREDICTED — Class constructors are never recognized
    //    as functions at all (`class_member_shapes_are_recognized_by_
    //    the_scanner`). Reading `scan.rs` predicted `constructor(...) {`
    //    would match `match_name_method` (bare `ident(`, no modifier
    //    needed) — running the test instead showed `"constructor"` is
    //    filtered OUT by `KEYWORD_NAMES` (`scan.rs`'s own list, which
    //    includes `"constructor"` alongside `if`/`for`/`switch`/etc.) at
    //    the exact point `find_all_functions_in_text` checks
    //    `is_keyword_name`. So every class's constructor is invisible to
    //    the function scanner — not a syntax miss like the anonymous-
    //    export shapes above, but a NAME the scanner explicitly rejects.
    //    Consequence, also executed: the toplevel fallback still catches
    //    the change (this audit's core invariant holds — nothing is
    //    silently lost), but it merges the constructor's body into ONE
    //    coarse blob with the class's opening line and EVERY OTHER
    //    unenclosed line preceding the next recognized method (in the
    //    fixture: the class header, a private-field declaration, AND the
    //    full constructor body all collapse into a single `toplevel:1-7`
    //    ref) — losing per-function callee/sibling/param-flow enrichment
    //    specifically for constructors, project-wide, every time. This is
    //    exactly the kind of misprediction-from-reading-alone the audit
    //    brief warned about ("reading the scanner mispredicts which stage
    //    fires") — caught only by running the positive-control test.
    //    Python reference: near-certain to share this — `KEYWORD_NAMES`
    //    is described as ported ("Names `NAME_METHOD_RE` candidates are
    //    filtered against"), and `"constructor"` sits in the list
    //    unremarked among genuine language keywords, reading like an
    //    intentional (if undocumented) reference decision rather than a
    //    port slip. Fix direction (if desired): special-case `constructor`
    //    out of `KEYWORD_NAMES` (it is a valid method name in a class
    //    body, unlike every other entry in that list, which are all
    //    actual reserved words).
    //
    // ── PROBED AND CLEAN (no silent-loss gap found) ─────────────────────
    //
    // - `renamed_file_with_edits_still_bundles_under_the_new_path` — a
    //   git rename-with-content-change hunk (`+++ b/<new path>`) bundles
    //   correctly under the NEW path. `diff::parse_diff` only ever reads
    //   the `+++ b/` line, so this was never actually at risk, but it was
    //   explicitly in scope ("deletions and moves... a rename") and is
    //   now executed, not just reasoned about.
    // - `scanner_missed_function_shapes_still_reach_a_seat_via_toplevel_
    //   fallback` — `scan::find_all_functions_in_text`'s three matchers
    //   all require a syntactic NAME, so a fully anonymous `export
    //   default function () {}` / `export default () => {}`, and an
    //   object-literal arrow-valued property (`{ onClick: () => {} }`),
    //   are ALL scanner misses (zero `FnDef`s found). But every line in
    //   each fixture is still an ADDED line with no enclosing function,
    //   so the #1605-follow-up top-level fallback catches all three as a
    //   `"toplevel"` bundle. The code reaches a seat; it just loses
    //   function-shaped enrichment (no callee/sibling facts) — a real
    //   quality gap, but NOT a silent-loss coverage gap under this
    //   audit's invariant.
    // - `class_member_shapes_are_recognized_by_the_scanner` — getters and
    //   static methods (`get value()`, `static create()`) ARE matched by
    //   `scan::match_name_method`'s modifier loop (`get`/`set`/`static`
    //   are explicit `METHOD_MODIFIERS` entries) and found as real,
    //   individually-named functions. Constructors are NOT — see finding
    //   #6 above; this test's coverage assertion still passes (toplevel
    //   fallback), only its original "found as a named function" premise
    //   for constructors was wrong, corrected after running it.
    // - `github_api_source_still_bundles_every_changed_file_beyond_the_
    //   repo_index_cap` — `FileSource::MAX_API_FILES` (30) bounds
    //   `candidate_files()` (the repo-wide index used for callee/sibling
    //   resolution), NOT `build_bundles`'s own per-file loop, which walks
    //   `diff::parse_diff`'s full file list unconditionally. A 35-file
    //   GithubApi-sourced diff still bundles all 35 changed files. The
    //   incomplete-index condition IS self-reported into every affected
    //   bundle's manifest (`"file budget exceeded: N files not
    //   scanned"`) — but per finding #5, that self-report never reaches
    //   any seat either way, so an operator reading the JSON artifact
    //   directly is the only consumer who currently sees it.
    //
    // ── SUSPECTED, NOT EXECUTED (could not construct a case, or judged
    //    out of this audit's scope) ────────────────────────────────────
    //
    // - Pure rename with NO content change (`rename from`/`rename to`,
    //   100% similarity, no `+++ b/` line at all) never enters
    //   `files_considered` — `diff::parse_diff` only recognizes a file via
    //   its `+++ b/` line. Not executed: there is no content to lose in
    //   this case (nothing changed), so it is very unlikely to be a
    //   review-quality gap, just a `files_considered` undercount. Would
    //   need a live `git diff -M` capture to confirm the exact line shape
    //   git emits for this case rather than a hand-built one.
    // - Decorated class methods (`@Injectable() foo() {}`), overloaded
    //   TS signatures, and async generators were reasoned through against
    //   `scan.rs`'s matchers (all plausibly caught by `match_name_method`
    //   or the top-level fallback, same as the confirmed-clean shapes
    //   above) but NOT executed — cut for time after the ranked findings
    //   above; the fallback's existence makes a full silent-loss unlikely
    //   for any of them, so this is a low-priority follow-up, not a
    //   suspected defect.

    #[test]
    fn build_bundles_silently_drops_an_over_cap_function_when_a_sibling_function_in_the_same_file_bundles(
    ) {
        let dir = TempDir::new().unwrap();
        let mut content = String::from("export function small(x) {\n  return x + 1;\n}\n\nexport function huge(x) {\n");
        for i in 0..320 {
            content.push_str(&format!("  console.log({i});\n"));
        }
        content.push_str("  return x;\n}\n");
        write(dir.path(), "src/mixed.ts", &content);
        let diff = new_file_diff("src/mixed.ts", &content);
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, &diff).unwrap();

        // The small function must have bundled — otherwise this fixture
        // doesn't actually exercise the "file already produced a bundle"
        // condition the gap depends on.
        assert!(!set.bundles.is_empty(), "the small function must bundle, or this test proves nothing");
        let covered = covered_lines(&set, "src/mixed.ts");
        let total = content.lines().count() as u32;
        let huge_start = content
            .lines()
            .position(|l| l.contains("export function huge"))
            .map(|i| i as u32 + 1)
            .expect("fixture line missing");

        // `huge`'s changed lines are NOT covered by any bundle...
        let huge_missing: Vec<u32> = (huge_start..=total).filter(|ln| !covered.contains(ln)).collect();
        assert!(
            !huge_missing.is_empty(),
            "expected `huge`'s lines to be dropped by the size cap (fixture assumption failed): {:?}",
            set.bundles
        );

        // ...AND (#1751 fix) the file MUST now be recorded as declined —
        // the huge function's drop is no longer invisible. Checked for ANY
        // entry naming this file, matching whatever reason the fix uses
        // (today: `SkipReason::OverSizeCap`, recorded per function at the
        // point of skip rather than inferred from a file-level bundle
        // count — see the removed `had_functions` check in git history).
        let declined = set.skip.files_skipped.iter().any(|s| s.path == "src/mixed.ts");
        assert!(
            declined,
            "the over-cap `huge` function's decline must be recorded even though the sibling `small` \
             function bundled successfully — a mixed file must not silently swallow one function's loss: \
             {:?}",
            set.skip
        );
        // The recorded entry must name WHICH function was dropped, not just
        // the file — the whole point of the fix is that an operator reading
        // `files_skipped` can tell `huge` was the casualty, not `small`.
        let huge_entry = set
            .skip
            .files_skipped
            .iter()
            .find(|s| s.path == "src/mixed.ts")
            .expect("checked above");
        assert_eq!(huge_entry.reason, SkipReason::OverSizeCap);
        assert!(
            huge_entry.function.as_deref().is_some_and(|f| f.contains("huge")),
            "the skip entry must name the dropped function (`huge`), got: {huge_entry:?}"
        );
    }

    #[test]
    fn build_bundles_treats_darkmuxs_own_rust_source_as_non_code() {
        // darkmux's OWN repository is Rust. A `.rs` PR through this
        // bundler gets `SkipReason::NonCodeExtension` — whose doc comment
        // frames the common case as "content such as fixtures,
        // lockfiles, or generated config," which is not true of this
        // fixture (a real, small, syntactically valid Rust function).
        let dir = TempDir::new().unwrap();
        write(dir.path(), "src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
        let diff = "+++ b/src/lib.rs\n\
@@ -0,0 +1,3 @@\n\
+pub fn add(a: i32, b: i32) -> i32 {\n\
+    a + b\n\
+}\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).unwrap();
        assert!(
            set.bundles.is_empty(),
            "a real Rust source change must currently yield zero bundles \
             (documenting the gap, not asserting it as desired): {:?}",
            set.bundles
        );
        assert_eq!(set.skip.files_skipped.len(), 1, "got: {:?}", set.skip);
        assert_eq!(set.skip.files_skipped[0].reason, SkipReason::NonCodeExtension);
    }

    #[test]
    fn build_bundles_treats_common_non_ts_languages_as_non_code() {
        // Same class as the Rust case above, across several other
        // common languages a real-world PR might touch. Each is real,
        // syntactically valid source code — none is a lockfile or
        // generated config, the case `NonCodeExtension`'s doc comment
        // actually describes.
        let cases: &[(&str, &str, &str)] = &[
            ("src/helper.py", "def add(a, b):\n    return a + b\n", "+def add(a, b):\n+    return a + b\n"),
            (
                "cmd/main.go",
                "func add(a, b int) int {\n\treturn a + b\n}\n",
                "+func add(a, b int) int {\n+\treturn a + b\n+}\n",
            ),
            (
                "scripts/deploy.sh",
                "#!/bin/sh\necho \"deploying\"\n",
                "+#!/bin/sh\n+echo \"deploying\"\n",
            ),
            (
                "migrations/001_up.sql",
                "CREATE TABLE widgets (id INT PRIMARY KEY);\n",
                "+CREATE TABLE widgets (id INT PRIMARY KEY);\n",
            ),
            (
                "src/styles.css",
                ".widget { color: red; }\n",
                "+.widget { color: red; }\n",
            ),
        ];
        for (path, content, added) in cases {
            let dir = TempDir::new().unwrap();
            write(dir.path(), path, content);
            let n = content.lines().count();
            let diff = format!("+++ b/{path}\n@@ -0,0 +1,{n} @@\n{added}");
            let source = FileSource::worktree(dir.path());
            let set = build_bundles(&source, &diff).unwrap();
            assert!(set.bundles.is_empty(), "{path}: expected zero bundles, got: {:?}", set.bundles);
            assert_eq!(
                set.skip.files_skipped.first().map(|s| s.reason),
                Some(SkipReason::NonCodeExtension),
                "{path}: got: {:?}",
                set.skip
            );
        }
    }

    #[test]
    fn resolve_callees_ignores_the_explicit_import_and_can_attach_the_wrong_function_body() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "src/a.ts",
            "export function validate(x) {\n  return x > 0; // WRONG callee — caller.ts never imports this one\n}\n",
        );
        write(
            dir.path(),
            "src/b.ts",
            "export function validate(x) {\n  return x < 0; // the REAL callee — explicitly imported below\n}\n",
        );
        write(
            dir.path(),
            "src/caller.ts",
            "import { validate } from './b';\nfunction useIt(y) {\n  return validate(y);\n}\n",
        );
        let diff = "+++ b/src/caller.ts\n\
@@ -0,0 +1,4 @@\n\
+import { validate } from './b';\n\
+function useIt(y) {\n\
+  return validate(y);\n\
+}\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).unwrap();
        assert!(!set.bundles.is_empty(), "expected at least one bundle for useIt");
        let callee_paths: Vec<&str> = set.bundles[0]
            .code
            .iter()
            .map(|r| r.path.as_str())
            .filter(|p| *p != "src/caller.ts")
            .collect();
        assert!(
            !callee_paths.is_empty(),
            "expected a resolved callee ref for `validate` (fixture assumption failed): {:?}",
            set.bundles[0].code
        );
        assert!(
            !callee_paths.contains(&"src/b.ts"),
            "documents CURRENT behavior: the caller's explicit `import ... from './b'` is ignored by \
             name-only resolution, so the REAL callee (b.ts) never reaches a seat. If this now fails, \
             import-aware resolution has been added and this test should be rewritten to assert b.ts \
             IS chosen: {:?}",
            set.bundles[0].code
        );
        assert!(
            callee_paths.contains(&"src/a.ts"),
            "documents CURRENT behavior: the WRONG same-named function (a.ts) is silently attached as \
             the callee instead: {:?}",
            set.bundles[0].code
        );
    }

    #[test]
    fn renamed_file_with_edits_still_bundles_under_the_new_path() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "src/new_name.ts", "export function greet(name) {\n  return `hi ${name}`;\n}\n");
        let diff = "diff --git a/src/old_name.ts b/src/new_name.ts\n\
similarity index 88%\n\
rename from src/old_name.ts\n\
rename to src/new_name.ts\n\
--- a/src/old_name.ts\n\
+++ b/src/new_name.ts\n\
@@ -1,3 +1,3 @@\n\
 export function greet(name) {\n\
-  return name;\n\
+  return `hi ${name}`;\n\
 }\n";
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, diff).unwrap();
        assert!(!set.bundles.is_empty(), "a renamed-with-edits file must still bundle under its NEW path: {:?}", set.skip);
        assert!(
            set.bundles.iter().any(|b| b.id.contains("src/new_name.ts")),
            "got: {:?}",
            set.bundles
        );
    }

    #[test]
    fn scanner_missed_function_shapes_still_reach_a_seat_via_toplevel_fallback() {
        for (label, content) in [
            (
                "anonymous default export function",
                "export default function (x: number) {\n  return x * 2;\n}\n",
            ),
            (
                "anonymous default export arrow",
                "export default (x: number) => {\n  return x * 2;\n};\n",
            ),
            (
                "object-literal arrow-valued property",
                "export const handlers = {\n  onClick: () => {\n    return 1;\n  },\n};\n",
            ),
        ] {
            let dir = TempDir::new().unwrap();
            write(dir.path(), "src/anon.ts", content);
            let diff = new_file_diff("src/anon.ts", content);
            let source = FileSource::worktree(dir.path());
            let set = build_bundles(&source, &diff).unwrap();
            let covered = covered_lines(&set, "src/anon.ts");
            let total = content.lines().count() as u32;
            let missing: Vec<u32> = (1..=total).filter(|ln| !covered.contains(ln)).collect();
            assert!(
                missing.is_empty(),
                "{label}: lines missing from every bundle: {missing:?}, bundles: {:?}",
                set.bundles
            );
        }
    }

    #[test]
    fn class_member_shapes_are_recognized_by_the_scanner() {
        let dir = TempDir::new().unwrap();
        let content = "export class Widget {\n  private count: number;\n\n  constructor(start: number) {\n    this.count = start;\n  }\n\n  get value(): number {\n    return this.count;\n  }\n\n  static create(): Widget {\n    return new Widget(0);\n  }\n}\n";
        write(dir.path(), "src/widget.ts", content);
        let diff = new_file_diff("src/widget.ts", content);
        let source = FileSource::worktree(dir.path());
        let set = build_bundles(&source, &diff).unwrap();

        // No silent loss overall — every line still reaches a seat, one
        // way or another (this audit's core invariant).
        let covered = covered_lines(&set, "src/widget.ts");
        let total = content.lines().count() as u32;
        let missing: Vec<u32> = (1..=total).filter(|ln| !covered.contains(ln)).collect();
        assert!(missing.is_empty(), "lines missing from every bundle: {missing:?}, bundles: {:?}", set.bundles);

        let found_names: std::collections::HashSet<String> =
            set.bundles.iter().map(|b| b.id.split('@').next().unwrap().to_string()).collect();
        // Getters and static methods ARE recognized as their own named
        // function (`METHOD_MODIFIERS` covers `get`/`static` explicitly).
        for want in ["value", "create"] {
            assert!(
                found_names.contains(want),
                "expected {want} to be scanned as its own function, found: {found_names:?}"
            );
        }
        // Finding #6 (unpredicted, confirmed only by running this test):
        // `constructor` is filtered OUT by `scan::KEYWORD_NAMES` — it is
        // NEVER recognized as its own function, class-wide, project-wide.
        // It still reaches a seat (via the toplevel fallback swallowing
        // it into a coarser blob alongside the class's other unenclosed
        // lines) but never as an individually-enriched function bundle.
        assert!(
            !found_names.contains("constructor"),
            "if this now fails, `constructor` has been removed from `KEYWORD_NAMES` and finding #6 is \
             fixed — update this assertion (and the report above) to expect it found: {found_names:?}"
        );
    }

    #[test]
    fn github_api_source_still_bundles_every_changed_file_beyond_the_repo_index_cap() {
        let mut diff = String::new();
        for i in 0..35 {
            diff.push_str(&format!(
                "+++ b/src/f{i}.ts\n@@ -0,0 +1,3 @@\n+export function fn{i}(x) {{\n+  return x + {i};\n+}}\n"
            ));
        }
        let source = FileSource::github_api("owner/repo", "deadbeef");
        if let FileSource::GithubApi { cache, .. } = &source {
            let mut c = cache.borrow_mut();
            for i in 0..35 {
                c.insert(
                    format!("src/f{i}.ts"),
                    Some(format!("export function fn{i}(x) {{\n  return x + {i};\n}}\n")),
                );
            }
        } else {
            panic!("expected a GithubApi source");
        }
        let set = build_bundles(&source, &diff).unwrap();
        let bundled_files: HashSet<String> =
            set.bundles.iter().flat_map(|b| b.code.iter().map(|r| r.path.clone())).collect();
        for i in 0..35 {
            let f = format!("src/f{i}.ts");
            assert!(
                bundled_files.contains(&f),
                "{f} must reach a seat despite the 30-file repo-index cap; bundled: {bundled_files:?}"
            );
        }
        assert!(
            set.bundles.iter().all(|b| b.manifest.iter().any(|m| m.contains("file budget exceeded"))),
            "every bundle must self-report the incomplete repo index: {:?}",
            set.bundles.iter().map(|b| &b.manifest).collect::<Vec<_>>()
        );
    }
}

