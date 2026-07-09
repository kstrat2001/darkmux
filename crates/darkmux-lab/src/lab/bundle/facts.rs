//! The three mechanical fact families (param-flow, differential,
//! siblings) plus the round-robin per-branch call-fact budget and the
//! new default-parameter facts (#1222 packet 3 mandate — no Python
//! precedent). Also owns the repo-wide function index + callee/sibling
//! resolution that feed those families, since both are meaningless
//! without a `FileSource` to resolve against.
//!
//! The fact emitter NEVER ranks or interprets a finding — every string
//! this module produces is a mechanically-derived count, a call-site
//! signature, or a presence/absence check. Judgment stays with whatever
//! reviews the bundle downstream.

use super::scan::{self, FnDef};
use super::source::FileSource;
use anyhow::Result;
use std::collections::{HashMap, HashSet};

/// Scoping-law target for a bundle's fact count is ~8-20; a couple of
/// lines of headroom lets the per-branch call+ambient-enrichment pair
/// survive on functions with many switch branches (else the enrichment
/// line — the most diagnostic fact this family computes — gets cut).
pub const MAX_FACT_LINES: usize = 24;
pub const MAX_CALLERS: usize = 4;
pub const MAX_SIBLINGS: usize = 4;
pub const MAX_CALLEE_BODY_LINES: usize = 40;

// ---------------------------------------------------------------------
// Repo-wide function index
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FnRecord {
    pub path: String,
    pub start0: usize,
    pub end0: usize,
    pub header: String,
    pub params: Vec<String>,
    pub body_text: String,
}

/// `name -> [defs]` across every candidate file, in FIRST-SEEN
/// (insertion) order — deliberately NOT a `HashMap` so `find_siblings`'s
/// stem-overlap scoring iterates in a deterministic, reproducible order
/// (mirrors Python's insertion-ordered `dict` semantics, which the
/// reference's stable sort relies on for tie-breaking).
#[derive(Debug, Default)]
pub struct RepoIndex {
    entries: Vec<(String, Vec<FnRecord>)>,
}

impl RepoIndex {
    fn push(&mut self, name: String, rec: FnRecord) {
        if let Some((_, v)) = self.entries.iter_mut().find(|(n, _)| n == &name) {
            v.push(rec);
        } else {
            self.entries.push((name, vec![rec]));
        }
    }

    pub fn get(&self, name: &str) -> Option<&Vec<FnRecord>> {
        self.entries.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    pub fn iter(&self) -> impl Iterator<Item = &(String, Vec<FnRecord>)> {
        self.entries.iter()
    }
}

/// Port of `build_repo_function_index`, generalized over `FileSource`:
/// scans every file in `candidate_files` (the source's own fidelity
/// boundary — full tree for `Worktree`, bounded diff+import-hop set for
/// `GithubApi`), skipping files over 3000 lines (pathological generated
/// files, matching the reference).
pub fn build_repo_index(source: &FileSource, candidate_files: &[String]) -> Result<RepoIndex> {
    let mut index = RepoIndex::default();
    for rel in candidate_files {
        let Some(content) = source.read_file(rel)? else {
            continue;
        };
        let lines: Vec<String> = content.lines().map(str::to_string).collect();
        if lines.len() > 3000 {
            continue;
        }
        for f in scan::find_all_functions_in_text(&lines) {
            let params = scan::extract_params(&lines, f.start0);
            let body_text = lines[f.start0..=f.end0].join("\n");
            index.push(
                f.name.clone(),
                FnRecord {
                    path: rel.clone(),
                    start0: f.start0,
                    end0: f.end0,
                    header: f.header,
                    params,
                    body_text,
                },
            );
        }
    }
    Ok(index)
}

/// Port of `resolve_callees`: for each distinct call name in
/// `fn_body_lines`, the best-match repo def (prefer one NOT in
/// `own_path`, else the same-path one). Returned in FIRST-CALL-APPEARANCE
/// order — the Python reference builds its `out` dict in `names` order
/// (first appearance in the body text) and iterates it in insertion
/// order, so downstream callee code-ref emission matches the reference
/// exactly. Callers needing name lookup build a `HashMap` view over the
/// pairs (the `callee_index`); this Vec is the ordering-bearing surface.
pub fn resolve_callees<'a>(
    fn_body_lines: &[String],
    repo_index: &'a RepoIndex,
    own_path: &str,
) -> Vec<(String, &'a FnRecord)> {
    let body_text = fn_body_lines.join("\n");
    let calls = scan::extract_calls(&body_text);
    let mut seen: HashSet<String> = HashSet::new();
    let mut names: Vec<String> = Vec::new();
    for (name, _display, _argc) in calls {
        if seen.insert(name.clone()) {
            names.push(name);
        }
    }
    let mut out = Vec::new();
    for name in names {
        let Some(defs) = repo_index.get(&name) else {
            continue;
        };
        let chosen = defs.iter().find(|d| d.path != own_path).or_else(|| defs.first());
        if let Some(c) = chosen {
            out.push((name, c));
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct Sibling {
    pub same_name: bool,
    pub name: String,
    pub path: String,
    pub header: String,
    pub params: Vec<String>,
    pub start0: usize,
}

/// Port of `find_siblings`: same-name-other-path matches first, then
/// stem-overlap matches, capped at `MAX_SIBLINGS` total.
pub fn find_siblings(fn_name: &str, own_path: &str, repo_index: &RepoIndex) -> Vec<Sibling> {
    let mut out: Vec<Sibling> = Vec::new();
    if let Some(defs) = repo_index.get(fn_name) {
        for d in defs.iter().filter(|d| d.path != own_path).take(MAX_SIBLINGS) {
            out.push(Sibling {
                same_name: true,
                name: fn_name.to_string(),
                path: d.path.clone(),
                header: d.header.clone(),
                params: d.params.clone(),
                start0: d.start0,
            });
        }
    }
    if out.len() >= MAX_SIBLINGS {
        return out;
    }
    let my_tokens = scan::stem_tokens(fn_name);
    let mut scored: Vec<(usize, &str, &FnRecord)> = Vec::new();
    if !my_tokens.is_empty() {
        for (other_name, defs) in repo_index.iter() {
            if other_name == fn_name {
                continue;
            }
            let overlap = scan::stem_tokens(other_name).intersection(&my_tokens).count();
            if overlap >= 1 {
                if let Some(d) = defs.first() {
                    scored.push((overlap, other_name.as_str(), d));
                }
            }
        }
        scored.sort_by_key(|t| std::cmp::Reverse(t.0)); // stable descending sort
    }
    let remaining = MAX_SIBLINGS - out.len();
    for (_, other_name, d) in scored.into_iter().take(remaining) {
        out.push(Sibling {
            same_name: false,
            name: other_name.to_string(),
            path: d.path.clone(),
            header: d.header.clone(),
            params: d.params.clone(),
            start0: d.start0,
        });
    }
    out
}

// ---------------------------------------------------------------------
// param-flow fact family
// ---------------------------------------------------------------------

/// `^\s*(case\s+|default\s*:)` against an already-stripped line — a
/// PREFIX match (no `$`), so any line merely STARTING with a case/default
/// label counts, regardless of trailing content on the same line.
fn is_case_or_default_label_line(stripped: &str) -> bool {
    if let Some(rest) = stripped.strip_prefix("case") {
        let ws_len = rest.len() - rest.trim_start_matches(char::is_whitespace).len();
        return ws_len > 0;
    }
    if let Some(rest) = stripped.strip_prefix("default") {
        return rest.trim_start_matches(char::is_whitespace).starts_with(':');
    }
    false
}

/// Port of `call_facts_for`: ranked, deduped, budget-capped call facts
/// for `text`. `single=true` (per-branch usage) stops after the top-
/// ranked call so the round-robin across branches stays fair.
fn call_facts_for(
    text: &str,
    callee_index: &HashMap<String, &FnRecord>,
    budget: usize,
    single: bool,
) -> Vec<String> {
    let calls = scan::extract_calls(text);
    if calls.is_empty() || budget == 0 {
        return Vec::new();
    }
    let rank = |bare: &str, display: &str| -> u8 {
        if let Some(callee) = callee_index.get(bare) {
            if scan::ambient_label(&callee.body_text).is_some() {
                return 0;
            }
            return 1;
        }
        if scan::ambient_label(&format!("{display}(")).is_some() {
            return 2;
        }
        3
    };
    let mut seen: HashSet<(String, usize)> = HashSet::new();
    let mut ranked: Vec<(String, String, usize)> = Vec::new();
    for (bare, display, argc) in calls {
        if seen.insert((bare.clone(), argc)) {
            ranked.push((bare, display, argc));
        }
    }
    ranked.sort_by_key(|(bare, display, _)| rank(bare, display));

    let mut facts = Vec::new();
    for (bare, display, argc) in &ranked {
        if facts.len() >= budget {
            break;
        }
        facts.push(format!("call `{display}(...)`: {argc} arg(s)"));
        if let Some(callee) = callee_index.get(bare) {
            if let Some(label) = scan::ambient_label(&callee.body_text) {
                if facts.len() < budget {
                    facts.push(format!(
                        "callee `{bare}` reads ambient `{label}` internally (ignores caller-supplied args)"
                    ));
                }
            } else if facts.len() < budget {
                let params_str = if callee.params.is_empty() {
                    "(none)".to_string()
                } else {
                    callee.params.join(", ")
                };
                facts.push(format!(
                    "callee `{bare}` signature expects {} param(s): {params_str}",
                    callee.params.len()
                ));
            }
        } else if let Some(label) = scan::ambient_label(&format!("{display}(")) {
            if facts.len() < budget {
                facts.push(format!("direct ambient read: `{label}` in this scope"));
            }
        }
        if single {
            break;
        }
    }
    facts.truncate(budget);
    facts
}

/// Port of `build_param_flow_facts`, extended with the default-parameter
/// facts mandate (#1222 packet 3 — new, no Python precedent): when
/// `default_params` is non-empty, a single mechanical fact line names
/// every declared default up front (never crowded out by the branch
/// budget below it) — the measured FP class this closes is an arity
/// claim ("callee expects 3 args, caller passed 1") that ignores a
/// default filling the gap.
pub fn build_param_flow_facts(
    fn_lines: &[String],
    params: &[String],
    default_params: &[(String, String)],
    callee_index: &HashMap<String, &FnRecord>,
) -> Vec<String> {
    let mut facts: Vec<String> = Vec::new();
    if !default_params.is_empty() {
        let rendered: Vec<String> = default_params.iter().map(|(n, v)| format!("{n} = {v}")).collect();
        facts.push(format!("default parameter(s): {}", rendered.join(", ")));
    }

    let body: Vec<String> = if fn_lines.len() > 2 {
        fn_lines[1..fn_lines.len() - 1].to_vec()
    } else {
        fn_lines.to_vec()
    };
    let body_text = body.join("\n");
    let branches = scan::split_switch_branches(&body);
    let mut branch_texts: Vec<(String, String)> = Vec::new();
    for (names, blines) in &branches {
        let btext = blines.join("\n");
        let label = names.join("/");
        let has_own_body = blines
            .iter()
            .any(|l| !l.trim().is_empty() && !is_case_or_default_label_line(l.trim()));
        if !has_own_body {
            facts.push(format!("{label} branch: fallthrough (no own block)"));
            continue;
        }
        for p in params {
            facts.push(format!("{label} branch: {} references to `{p}`", scan::count_refs(&btext, p)));
        }
        branch_texts.push((label, btext));
    }
    for p in params {
        facts.push(format!("whole function: {} references to `{p}`", scan::count_refs(&body_text, p)));
    }

    if facts.len() >= MAX_FACT_LINES {
        facts.truncate(MAX_FACT_LINES);
        return facts;
    }
    let mut remaining = MAX_FACT_LINES - facts.len();

    if !branch_texts.is_empty() {
        let per_branch_budget = (remaining / branch_texts.len()).max(2);
        for (label, btext) in &branch_texts {
            if remaining == 0 {
                break;
            }
            let cf = call_facts_for(btext, callee_index, per_branch_budget.min(remaining), true);
            for f in &cf {
                facts.push(format!("[{label}] {f}"));
            }
            remaining = remaining.saturating_sub(cf.len());
        }
    } else {
        let cf = call_facts_for(&body_text, callee_index, remaining, false);
        facts.extend(cf);
    }
    facts.truncate(MAX_FACT_LINES);
    facts
}

// ---------------------------------------------------------------------
// differential fact family
// ---------------------------------------------------------------------

/// Port of `build_differential_facts`: calls present in the function's
/// pre-image that were never re-added anywhere in the diff.
pub fn build_differential_facts(fn_old_block: &[String], global_added_calls: &HashSet<String>) -> Vec<String> {
    if fn_old_block.is_empty() {
        return Vec::new();
    }
    let old_text = fn_old_block.join("\n");
    let old_calls: HashSet<String> = scan::extract_calls(&old_text).into_iter().map(|(name, _, _)| name).collect();
    let mut dropped: Vec<String> = old_calls
        .into_iter()
        .filter(|n| !global_added_calls.contains(n))
        .collect();
    dropped.sort();
    dropped
        .into_iter()
        .take(MAX_FACT_LINES)
        .map(|name| format!("call `{name}(...)` present in the pre-image of this function, absent from all additions in this diff"))
        .collect()
}

// ---------------------------------------------------------------------
// siblings fact family
// ---------------------------------------------------------------------

/// Port of `build_siblings_facts`.
pub fn build_siblings_facts(siblings: &[Sibling]) -> Vec<String> {
    let mut facts: Vec<String> = siblings
        .iter()
        .map(|s| {
            let tag = if s.same_name {
                "same name, different path"
            } else {
                "shared identifier stem"
            };
            let params = if s.params.is_empty() {
                "(none)".to_string()
            } else {
                s.params.join(", ")
            };
            format!("sibling ({tag}): `{}` in `{}` — params: {params}", s.name, s.path)
        })
        .collect();
    facts.truncate(MAX_FACT_LINES);
    facts
}

/// Just enough of `FnDef` re-exported for `mod.rs`'s manifest/truncation
/// helpers, which also need to re-scan a file's functions.
pub type ScannedFn = FnDef;

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(path: &str, start0: usize, end0: usize, params: &[&str], body: &str) -> FnRecord {
        FnRecord {
            path: path.to_string(),
            start0,
            end0,
            header: String::new(),
            params: params.iter().map(|s| s.to_string()).collect(),
            body_text: body.to_string(),
        }
    }

    #[test]
    fn branch_param_ref_counts_are_exact_including_zero() {
        // `id` and `total` are BOTH parameters; the `'a'` branch touches
        // neither, so per-branch counting must emit an EXACT `0` for
        // both, not silently drop the fact for an unreferenced param.
        let fn_lines: Vec<String> = vec![
            "function f(kind, id, total) {".to_string(),
            "  switch (kind) {".to_string(),
            "    case 'a': {".to_string(),
            "      doThing();".to_string(),
            "      break;".to_string(),
            "    }".to_string(),
            "    case 'b': {".to_string(),
            "      save(id, total);".to_string(),
            "      break;".to_string(),
            "    }".to_string(),
            "  }".to_string(),
            "}".to_string(),
        ];
        let params = vec!["kind".to_string(), "id".to_string(), "total".to_string()];
        let callee_index: HashMap<String, &FnRecord> = HashMap::new();
        let facts = build_param_flow_facts(&fn_lines, &params, &[], &callee_index);
        assert!(
            facts.contains(&"a branch: 0 references to `id`".to_string()),
            "expected an exact 0-reference fact for `id` in branch a, got: {facts:?}"
        );
        assert!(
            facts.contains(&"a branch: 0 references to `total`".to_string()),
            "expected an exact 0-reference fact for `total` in branch a, got: {facts:?}"
        );
        assert!(
            facts.contains(&"b branch: 1 references to `id`".to_string()),
            "expected an exact 1-reference fact for `id` in branch b, got: {facts:?}"
        );
    }

    #[test]
    fn short_callee_is_inlined_full_body_in_call_facts_index() {
        // A callee at/under MAX_CALLEE_BODY_LINES resolves as a full-body
        // FnRecord (the code_ref inlining decision itself lives in
        // `mod.rs::build_bundles`; here we confirm the callee_index this
        // family reads from carries the FULL body, not a header stub —
        // the positive control paired with
        // `mod::tests::truncated_callee_marks_bundle_and_slice_marker`.
        let short_body = "function helper(x) {\n  return x + 1;\n}";
        let helper = rec("src/helpers.ts", 0, 2, &["x"], short_body);
        assert!(helper.end0 - helper.start0 < MAX_CALLEE_BODY_LINES);
        let mut callee_index: HashMap<String, &FnRecord> = HashMap::new();
        callee_index.insert("helper".to_string(), &helper);
        let fn_lines: Vec<String> = vec!["function f(x) {".to_string(), "  return helper(x);".to_string(), "}".to_string()];
        let facts = build_param_flow_facts(&fn_lines, &["x".to_string()], &[], &callee_index);
        assert!(
            facts
                .iter()
                .any(|f| f.contains("callee `helper` signature expects 1 param(s): x")),
            "expected the fully-resolved callee signature fact, got: {facts:?}"
        );
    }

    #[test]
    fn orm_noise_callees_excluded_from_calls() {
        let calls = scan::extract_calls("db.query('x').where('a', 1).orderBy('b').first();");
        let names: Vec<&str> = calls.iter().map(|c| c.0.as_str()).collect();
        assert!(!names.contains(&"query"));
        assert!(!names.contains(&"where"));
        assert!(!names.contains(&"orderBy"));
        assert!(!names.contains(&"first"));
    }

    #[test]
    fn default_param_fact_is_present_and_first() {
        let fn_lines: Vec<String> = vec![
            "function f(a, retries = 3) {".to_string(),
            "  return a + retries;".to_string(),
            "}".to_string(),
        ];
        let params = vec!["a".to_string(), "retries".to_string()];
        let defaults = vec![("retries".to_string(), "3".to_string())];
        let callee_index: HashMap<String, &FnRecord> = HashMap::new();
        let facts = build_param_flow_facts(&fn_lines, &params, &defaults, &callee_index);
        assert_eq!(facts[0], "default parameter(s): retries = 3");
    }

    #[test]
    fn round_robin_call_budget_no_branch_starvation() {
        // Three branches, each referencing several distinct calls — with
        // a tight budget every branch must still get at least one call
        // fact (no single branch can consume the whole budget).
        let body: Vec<String> = vec![
            "switch (kind) {".to_string(),
            "  case 'a': {".to_string(),
            "    alphaOne(x); alphaTwo(x); alphaThree(x);".to_string(),
            "    break;".to_string(),
            "  }".to_string(),
            "  case 'b': {".to_string(),
            "    betaOne(x); betaTwo(x); betaThree(x);".to_string(),
            "    break;".to_string(),
            "  }".to_string(),
            "  case 'c': {".to_string(),
            "    gammaOne(x); gammaTwo(x); gammaThree(x);".to_string(),
            "    break;".to_string(),
            "  }".to_string(),
            "}".to_string(),
        ];
        let mut fn_lines = vec!["function f(x) {".to_string()];
        fn_lines.extend(body);
        fn_lines.push("}".to_string());
        let params = vec!["x".to_string()];
        let callee_index: HashMap<String, &FnRecord> = HashMap::new();
        let facts = build_param_flow_facts(&fn_lines, &params, &[], &callee_index);
        let joined = facts.join("\n");
        assert!(joined.contains("[a] call `alphaOne"), "missing branch a call fact:\n{joined}");
        assert!(joined.contains("[b] call `betaOne"), "missing branch b call fact:\n{joined}");
        assert!(joined.contains("[c] call `gammaOne"), "missing branch c call fact:\n{joined}");
    }

    #[test]
    fn siblings_facts_render_same_name_and_stem_overlap() {
        let siblings = vec![
            Sibling {
                same_name: true,
                name: "createOrder".to_string(),
                path: "src/legacy/order.ts".to_string(),
                header: String::new(),
                params: vec!["a".to_string()],
                start0: 0,
            },
            Sibling {
                same_name: false,
                name: "createOrderDraft".to_string(),
                path: "src/drafts.ts".to_string(),
                header: String::new(),
                params: vec![],
                start0: 0,
            },
        ];
        let facts = build_siblings_facts(&siblings);
        assert!(facts[0].contains("same name, different path"));
        assert!(facts[1].contains("shared identifier stem"));
        assert!(facts[1].contains("(none)"));
    }

    #[test]
    fn differential_facts_only_for_dropped_calls() {
        let old_block: Vec<String> = vec!["  validate(x);".to_string(), "  save(x);".to_string()];
        let mut added: HashSet<String> = HashSet::new();
        added.insert("save".to_string());
        let facts = build_differential_facts(&old_block, &added);
        assert_eq!(facts.len(), 1);
        assert!(facts[0].contains("validate"));
    }

    #[test]
    fn resolve_callees_prefers_other_path() {
        let mut idx = RepoIndex::default();
        idx.push("helper".to_string(), rec("src/a.ts", 0, 2, &[], "function helper() {}"));
        idx.push("helper".to_string(), rec("src/b.ts", 0, 2, &[], "function helper() {}"));
        let body = vec!["  helper();".to_string()];
        let callees = resolve_callees(&body, &idx, "src/a.ts");
        let helper = callees.iter().find(|(n, _)| n == "helper").unwrap();
        assert_eq!(helper.1.path, "src/b.ts");
    }

    #[test]
    fn zero_params_zero_calls_produces_no_facts() {
        // A changed function with an empty param list and a body that
        // makes no calls at all (no branches, no callees, no defaults)
        // must produce an EMPTY fact list — not a spurious "0 references"
        // line (there are no params to count references for) and not a
        // panic on the empty ranked-calls path in `call_facts_for`.
        let fn_lines: Vec<String> =
            vec!["function noop() {".to_string(), "  // does nothing".to_string(), "}".to_string()];
        let callee_index: HashMap<String, &FnRecord> = HashMap::new();
        let facts = build_param_flow_facts(&fn_lines, &[], &[], &callee_index);
        assert!(facts.is_empty(), "expected no facts for a zero-param, zero-call function, got: {facts:?}");
    }

    #[test]
    fn resolve_callees_preserves_first_call_appearance_order() {
        // The reference's dict insertion order = first appearance in the
        // body text; the ported Vec must match (zebra called first even
        // though alpha sorts first).
        let mut idx = RepoIndex::default();
        idx.push("zebra".to_string(), rec("src/z.ts", 0, 2, &[], "function zebra() {}"));
        idx.push("alpha".to_string(), rec("src/a.ts", 0, 2, &[], "function alpha() {}"));
        let body = vec!["  zebra(); alpha();".to_string()];
        let callees = resolve_callees(&body, &idx, "src/own.ts");
        let names: Vec<&str> = callees.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["zebra", "alpha"]);
    }
}
