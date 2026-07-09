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
    /// `"<fn>@<path>"` — shared across a function's family-variant
    /// bundles so a probe-runner can group them.
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

/// Build the full `BundleSet` for `diff` read against `source`. Direct
/// port of `build_bundles(worktree, diff_text)`, generalized over
/// `FileSource` fidelity and extended with the manifest + truncation
/// bookkeeping (#1222 packet 3).
pub fn build_bundles(source: &FileSource, diff_text: &str) -> Result<BundleSet> {
    let files = diff::parse_diff(diff_text);
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
            continue;
        }
        let Some(content) = source.read_file(rel)? else {
            continue;
        };
        let lines: Vec<String> = content.lines().map(str::to_string).collect();

        let mut changed_new_lines: HashSet<u32> = HashSet::new();
        for h in hunks {
            changed_new_lines.extend(h.new_lines.iter().copied());
        }
        if changed_new_lines.is_empty() {
            continue;
        }

        let all_fns = scan::find_all_functions_in_text(&lines);
        let mut sorted_lines: Vec<u32> = changed_new_lines.into_iter().collect();
        sorted_lines.sort_unstable();
        let mut found_fns: Vec<(usize, usize, String, String)> = Vec::new();
        let mut found_keys: HashSet<(usize, usize)> = HashSet::new();
        for ln in sorted_lines {
            let Some(fndef) = scan::enclosing_fn_for_line(&all_fns, ln) else {
                continue;
            };
            let key = (fndef.start0, fndef.end0);
            if found_keys.insert(key) {
                found_fns.push((fndef.start0, fndef.end0, fndef.header.clone(), fndef.name.clone()));
            }
        }

        for (start0, end0, _header, name) in found_fns {
            let seen_key = (rel.clone(), start0, end0);
            if seen_fns.contains(&seen_key) {
                continue;
            }
            seen_fns.insert(seen_key);
            if end0 - start0 > 300 {
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

            let callees = facts::resolve_callees(&fn_lines, &repo_index, rel);
            // Deterministic emission order for reproducible golden output
            // (a `HashMap`'s iteration order isn't).
            let mut callee_names: Vec<&String> = callees.keys().collect();
            callee_names.sort();
            for cname in &callee_names {
                let cdef = callees[cname.as_str()];
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

            let pf_facts = facts::build_param_flow_facts(&fn_lines, &params, &default_params, &callees);

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
            let manifest = build_manifest(source, &code_refs, &repo_index, &own_file_content)?;
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
    }

    Ok(BundleSet { bundles })
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
}
