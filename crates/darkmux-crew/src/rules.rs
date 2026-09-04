//! Rule registry (#1959 — promoted from the crawl module to a general
//! template kind so any role's mission can bind to a named, searchable
//! property, not only the crawler).
//!
//! A rule is a named, searchable property bound to files by glob, with
//! match/no-match prose — nothing in this type or its loader is
//! crawl-specific. Rules are authored as JSON, embedded at compile time
//! from `templates/builtin/rules/`, mirroring `crew::loader`'s
//! `BUILTIN_ROLES` `include_str!` pattern (see `loader.rs`'s module doc:
//! "Search order: user dir → binary-embedded built-ins") so `cargo install
//! --path .` ships the three built-in rules with no source checkout
//! needed. A user tier at `<darkmux root>/rules/*.json` overrides an
//! embedded rule sharing its id — but unlike the role loader's whole-file
//! replace, a rule override MERGES: only the fields the override names
//! change, everything else survives from the rule underneath (#1959
//! finding 2; see `merge_json_object_shallow`). A malformed user rule file
//! is a WARNING naming the file, never a crash (config leniency —
//! validation is loud only at the point a rule id is actually resolved for
//! use).
//!
//! **Rules are the shared block between a crawl and a review** (#2310 P4c —
//! DESIGN.md "A rule is a procedure, because a small seat has no
//! intuition"): the main difference between the two missions is managing a
//! CORPUS versus a FOCUSED CHANGE SET, and a rule authored for one runs
//! unchanged in the other when its [`scope`](Rule::scope) allows. Two
//! fields carry that: `scope` (`["tree"]`, `["diff"]`, or both — a
//! crawl-only prefilter rule, a diff-only intent rule, or a rule that runs
//! either way; default both, so every pre-#2310-P4c rule keeps applying
//! everywhere it always did) and `confirm` (`"mod"` | `"search"` |
//! `"question"` — which of the three ways a review's finding gets
//! confirmed: a gated patch, an enumerated list of instances the model
//! searched for, or a question with candidates attached; default `"mod"`,
//! matching every rule this project shipped before this field existed).
//! `search`/`compare` are the optional recipe/question a `"search"`-
//! or `"question"`-confirmed rule declares — see [`SearchRecipe`] and
//! [`Rule::compare`].
//!
//! **Deliberately NOT named `applies_to`** even though DESIGN.md and the
//! P4 brief both use that word for this concept: `Rule::applies_to`
//! already exists, and already means something else entirely (the file
//! globs a `site`/`read` rule matches). Reusing the name for a second,
//! unrelated meaning on the same struct would be exactly the kind of
//! silent field collision this project's `#[serde(flatten)] extras`
//! leniency exists to catch loudly, not something to introduce on
//! purpose — so the tree/diff concept is named `scope` here instead. Any
//! reader who came from the brief expecting `applies_to: ["tree","diff"]`
//! should read `scope` in its place.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Rules compiled into the binary. `(id, json)` pairs, verbatim file
/// contents — see `crate::loader::BUILTIN_ROLES` for the precedent this
/// mirrors.
const EMBEDDED_RULES: &[(&str, &str)] = &[
    (
        "swallowed-error",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../templates/builtin/rules/swallowed-error.json"
        )),
    ),
    (
        "doc-contradicts-code",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../templates/builtin/rules/doc-contradicts-code.json"
        )),
    ),
    (
        "stale-consumer",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../templates/builtin/rules/stale-consumer.json"
        )),
    ),
    (
        // (#2206) The slop-chop program's first rule and its positive
        // control: compound conditions are COMMON, so a zero here means the
        // model is blind, not that the corpus is clean — which is exactly
        // what `swallowed-error`'s zero could not tell us.
        "unnamed-predicate",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../templates/builtin/rules/unnamed-predicate.json"
        )),
    ),
    // (#2310 P4c) The review rules catalog v1 — diff-scoped
    // (`scope: ["diff"]`), each with its own `confirm` form. See
    // DESIGN.md "A rule is a procedure" and the P4-brief-draft.md
    // 2026-09-04 sections for the design behind each.
    (
        "intent-vs-diff",
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/rules/intent-vs-diff.json")),
    ),
    (
        "existing-solution",
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/rules/existing-solution.json")),
    ),
    (
        "shared-symbol-callers",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../templates/builtin/rules/shared-symbol-callers.json"
        )),
    ),
    (
        "union-vs-enum",
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/rules/union-vs-enum.json")),
    ),
    (
        "test-gap",
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/builtin/rules/test-gap.json")),
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    Site,
    Read,
    Edge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeRuleConfig {
    pub ecosystem: String,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// (#2310 P4c) Which mission shape a rule is willing to run under. A
/// tree-only rule (e.g. a crawl-only prefilter that scans a whole
/// checkout) sets `["tree"]`; a diff-only rule (e.g. `intent-vs-diff`,
/// which has no meaning without a before/after) sets `["diff"]`; a rule
/// that works either way (most of the crawl's four built-ins, and most of
/// the review catalog's `swallowed-error`/`unnamed-predicate` reuse) sets
/// both or omits the field, since `Rule::scope_or_default` treats an
/// absent/empty `scope` as "everywhere" — the default every rule shipped
/// before this field existed already had.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleScope {
    Tree,
    Diff,
}

/// (#2310 P4c) Which of the three ways a review confirms a finding this
/// rule uses — DESIGN.md "Confirmation is a mod, a search, or a question":
/// `Mod` (the default, and every rule this project shipped before this
/// field existed) means the finding is confirmed by producing a patch that
/// passes its gate; `Search` means the rule declares a `search` recipe the
/// unit runs verbatim over the whole tree and delivers the instance list;
/// `Question` means the rule declares a `compare` question delivered with
/// candidates attached, honest about being unconfirmed. A small seat has
/// no intuition for which form applies — the rule file carries the
/// decision as data so the model never has to guess it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmForm {
    #[default]
    Mod,
    Search,
    Question,
}

/// (#2310 P4c) The `search` recipe a `confirm: "search"` rule declares —
/// DESIGN.md "a recipe the unit runs verbatim (grep and symbol search for
/// the same verbs and nouns, existing enums with overlapping members, the
/// package manifest for a known library)". Deliberately a thin, literal
/// shape (a list of patterns the unit's `search` tool runs one at a time,
/// each over the tree at `path`) rather than anything clever: the point is
/// that the unit executes a mechanical recipe, not that it reasons about
/// what to search for. Lenient on read — every field optional, unknown
/// keys ride in `extras` — same posture as [`Rule`] itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchRecipe {
    /// Literal substrings the unit's `search` tool runs, one call per
    /// pattern, verbatim — not compiled or interpreted here.
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Where to run the recipe, relative to the tree root. Absent means
    /// the whole tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A short note on what the results mean, injected into the unit's
    /// prompt alongside the pattern list — e.g. "each hit is a caller;
    /// list every one you find, do not stop at the first."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

/// One rule, matching the shape of the three built-in rule files
/// (`templates/builtin/rules/*.json`) — see those for worked examples of
/// every field. Lenient on read: only `id`/`kind` are required.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub kind: RuleKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub applies_to: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// `site`-kind only: mechanical regexes that narrow the tree before the
    /// model ever sees it.
    #[serde(default)]
    pub prefilter: Vec<String>,
    /// `site`/`edge`-kind: lines of context on each side of a hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<usize>,
    /// `read`-kind: max tokens per chunk (default 12000, applied by
    /// `crate::crawl::plan` when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_tokens: Option<usize>,
    /// `edge`-kind only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<EdgeRuleConfig>,
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_match: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why_hint: Option<String>,
    /// (#2310 P4c) Which mission shape(s) this rule runs under — see
    /// [`RuleScope`]. Empty (the wire default, and every pre-#2310-P4c
    /// rule) means "both" — read it through [`Rule::scope_or_default`],
    /// never this field directly, so an absent `scope` and an explicit
    /// `["tree","diff"]` are indistinguishable to every caller.
    #[serde(default)]
    pub scope: Vec<RuleScope>,
    /// (#2310 P4c) Which of the three confirmation forms this rule uses —
    /// see [`ConfirmForm`]. Defaults to `Mod`, matching every rule this
    /// project shipped before this field existed.
    #[serde(default)]
    pub confirm: ConfirmForm,
    /// (#2310 P4c) The recipe a `confirm: "search"` rule runs. `None` on
    /// every other rule; a `confirm: "search"` rule with no `search`
    /// block is a thin-rule warning (see `warn_on_thin_rules`) — it will
    /// never enumerate anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<SearchRecipe>,
    /// (#2310 P4c) The bounded, single-inference question a
    /// `confirm: "question"` rule asks — DESIGN.md "the only inference,
    /// bounded". `None` on every other rule; a `confirm: "question"` rule
    /// with no `compare` question is a thin-rule warning, same reasoning
    /// as `search` above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare: Option<String>,
    #[serde(flatten)]
    pub extras: BTreeMap<String, serde_json::Value>,
}

pub const DEFAULT_READ_CHUNK_TOKENS: usize = 12_000;
pub const DEFAULT_WINDOW: usize = 30;

impl Rule {
    pub fn chunk_tokens_or_default(&self) -> usize {
        self.chunk_tokens.unwrap_or(DEFAULT_READ_CHUNK_TOKENS)
    }
    pub fn window_or_default(&self) -> usize {
        self.window.unwrap_or(DEFAULT_WINDOW)
    }
    /// (#2310 P4c) An empty `scope` (the wire default) reads as "both" —
    /// the only place this rule's callers should ever ask "does this rule
    /// apply here", so an absent `scope` and an explicit
    /// `["tree","diff"]` behave identically everywhere, not just at parse
    /// time.
    pub fn scope_or_default(&self) -> &[RuleScope] {
        const BOTH: [RuleScope; 2] = [RuleScope::Tree, RuleScope::Diff];
        if self.scope.is_empty() {
            &BOTH
        } else {
            &self.scope
        }
    }
    /// Whether this rule is willing to run under `scope` (a single mission
    /// shape — `RuleScope::Tree` for a crawl, `RuleScope::Diff` for a
    /// review). The planner side of #2310 P4c filters a launch's resolved
    /// rule set through this before ever building a plan step for a rule
    /// scope doesn't admit.
    pub fn applies_to_scope(&self, scope: RuleScope) -> bool {
        self.scope_or_default().contains(&scope)
    }
}

/// Load every known rule (embedded, then user-tier overrides by id) plus
/// any non-fatal warnings collected along the way (a malformed embedded
/// rule — a darkmux bug — or a malformed user rule file).
///
/// `user_dir` is the `<darkmux root>/rules` directory to scan for
/// overrides; `None` skips the user tier entirely. Taking it as a plain
/// param (rather than reading `paths::resolve` internally) mirrors
/// `crate::loader`'s testable shape — this stays unit-testable without
/// env-var mutation.
/// (#2298) A rule id is also a file name (`<mission>/plan/<rule>.json`):
/// one path segment, no separators, no leading dot.
pub fn is_safe_rule_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// (#2298 / #2297) The `prefilter` field has ONE implemented shape today: a
/// list of regex strings. A second shape is reserved — `{"command": "..."}`,
/// a tool that emits SARIF/JSON sites (semgrep, ast-grep, a linter) — and is
/// NOT built yet. A rule declaring it must be refused by name at load, never
/// parsed into an empty list and silently crawled as if it had no prefilter.
/// Returns the refusal message, or `None` when the shape is the regex list.
fn unsupported_prefilter_shape(raw: &serde_json::Value, id: &str) -> Option<String> {
    let pf = raw.get("prefilter")?;
    let reserved = |v: &serde_json::Value| v.is_object() && v.get("command").is_some();
    if reserved(pf) || pf.as_array().is_some_and(|a| a.iter().any(reserved)) {
        return Some(format!(
            "rule '{id}' declares a `prefilter` of the `{{\"command\": ...}}` shape — a tool-backed \
             site producer (SARIF/JSON) is reserved by #2297 and not implemented yet; only a list \
             of regex strings is supported — skipped"
        ));
    }
    None
}

pub fn load_all(user_dir: Option<&Path>) -> (BTreeMap<String, Rule>, Vec<String>) {
    let mut map = BTreeMap::new();
    let mut warnings = Vec::new();

    for (id, json) in EMBEDDED_RULES {
        if let Some(msg) = serde_json::from_str::<serde_json::Value>(json)
            .ok()
            .and_then(|raw| unsupported_prefilter_shape(&raw, id))
        {
            warnings.push(msg);
            continue;
        }
        match serde_json::from_str::<Rule>(json) {
            Ok(r) => {
                map.insert((*id).to_string(), r);
            }
            Err(e) => warnings.push(format!(
                "embedded rule '{id}' failed to parse ({e}) — this is a darkmux bug"
            )),
        }
    }

    if let Some(dir) = user_dir {
        if let Ok(entries) = fs::read_dir(dir) {
            let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
            paths.sort();
            for path in paths {
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                match fs::read_to_string(&path) {
                    Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(override_value) => {
                            let Some(id) = override_value.get("id").and_then(|v| v.as_str()) else {
                                warnings.push(format!(
                                    "user rule {} has no `id` field — skipped",
                                    path.display()
                                ));
                                continue;
                            };
                            let id = id.to_string();
                            // A user file sharing an existing id MERGES over
                            // it — only the fields the override names
                            // change; everything else (e.g. an embedded
                            // rule's `applies_to`/`prefilter`) survives from
                            // the rule already in the map (#1959 finding 2).
                            // An id the map doesn't know yet gets an empty
                            // base, i.e. the override defines the whole rule.
                            let base = map
                                .get(&id)
                                .and_then(|r| serde_json::to_value(r).ok())
                                .unwrap_or_else(|| serde_json::json!({}));
                            let merged = merge_json_object_shallow(base, override_value);
                            if let Some(msg) = unsupported_prefilter_shape(&merged, &id) {
                                warnings.push(format!("user rule {}: {msg}", path.display()));
                                continue;
                            }
                            match serde_json::from_value::<Rule>(merged) {
                                Ok(r) => {
                                    map.insert(id, r);
                                }
                                Err(e) => warnings.push(format!(
                                    "user rule {} failed to parse ({e}) — skipped",
                                    path.display()
                                )),
                            }
                        }
                        Err(e) => warnings.push(format!(
                            "user rule {} failed to parse ({e}) — skipped",
                            path.display()
                        )),
                    },
                    Err(e) => warnings.push(format!(
                        "user rule {} could not be read ({e}) — skipped",
                        path.display()
                    )),
                }
            }
        }
    }

    (map, warnings)
}

/// Shallow top-level JSON-object merge: every key `patch` names overwrites
/// `base`'s value at that key WHOLESALE (an array field, e.g. `applies_to`,
/// REPLACES rather than concatenates — there is no recursive merge into
/// nested structures) — any key `patch` doesn't name survives from `base`
/// untouched (#1959 finding 2). `patch` wins entirely when either side
/// isn't a JSON object (a malformed base/override falls through to
/// `Rule`'s own deserialize error rather than merging nonsense).
fn merge_json_object_shallow(base: serde_json::Value, patch: serde_json::Value) -> serde_json::Value {
    match (base, patch) {
        (serde_json::Value::Object(mut base_map), serde_json::Value::Object(patch_map)) => {
            for (k, v) in patch_map {
                base_map.insert(k, v);
            }
            serde_json::Value::Object(base_map)
        }
        (_, patch) => patch,
    }
}

/// Warn (never fail — this is advisory, not validation) when a resolved
/// `site`/`read` rule has an empty `applies_to` (it will never match any
/// file) or a `site` rule has an empty `prefilter` (the crawl planner's
/// `collect_site_units` early-returns with zero units for exactly this
/// case, silently before this fix — #1959 finding 2), or (#2310 P4c) a
/// `confirm: "search"` rule declares no `search` recipe (it will never
/// enumerate anything) or a `confirm: "question"` rule declares no
/// `compare` question (it has nothing to ask). Runs only over the rules a
/// manifest actually RESOLVED for use — not every rule sitting in the
/// registry — so an unrelated user-authored rule id never generates noise
/// for a manifest that doesn't reference it. This is `doctor`'s surface
/// for an invalid rule too: `darkmux doctor` calls `resolve` (or
/// `load_all` — see `darkmux-doctor`'s own rules check) over every rule id
/// a mission config's `rules` input can name and folds these warnings into
/// its report, so a thin or malformed rule is visible before a launch ever
/// tries to plan against it.
/// (#2310 P4c review round 2, MUST FIX 2) The ONE place every "this rule
/// is thin/inert" check lives — `warn_on_thin_rules` (below, over a
/// manifest's resolved subset) and `darkmux-doctor`'s `build_rules_check`
/// (over the WHOLE registry) both call this per-rule function rather than
/// each carrying its own copy of the same four checks, which is exactly
/// how the two drifted before this extraction: P4c's own first pass added
/// the `confirm`/`search`/`compare` checks to `warn_on_thin_rules` and had
/// to remember to ALSO add them to doctor's separate copy — a repeat of
/// #1959 finding 2's own lesson, one packet later, in this same file.
///
/// The `applies_to`/`prefilter` checks below assume a TREE walk, where an
/// empty `applies_to`/`prefilter` really does mean "matches nothing" —
/// `SourceFiles::matching`/`collect_site_units`'s own early return. A rule
/// scoped to `["diff"]` only reads the OPPOSITE way under
/// `plan_diff_rule`/`FilteredDiffSource`: empty `applies_to` means "every
/// file the diff touches", and empty `prefilter` means "every hunk line
/// is a candidate" (DESIGN.md "Hunks are natural windows. No prefilter is
/// needed"). Firing these checks on a diff-only rule would call CORRECT,
/// deliberate config "thin" — gated on `applies_to_scope(RuleScope::Tree)`
/// below. That gate is airtight, not just conventional, now that
/// `plan_step::plan_one_rule`/`plan::plan_diff_rule` (#2310 P4c review
/// round 2, MUST FIX 2) both REFUSE to plan a rule outside its declared
/// scope — a diff-only rule genuinely cannot reach the tree planner any
/// more, so "this warning assumes tree scope" is a fact this function can
/// rely on, not a convention a caller could silently violate.
pub fn thin_rule_warnings(rule: &Rule) -> Vec<String> {
    let mut warnings = Vec::new();
    let tree_scoped = rule.applies_to_scope(RuleScope::Tree);
    if tree_scoped && matches!(rule.kind, RuleKind::Site | RuleKind::Read) && rule.applies_to.is_empty() {
        warnings.push(format!(
            "rule '{}' has an empty `applies_to` — it will never match any file",
            rule.id
        ));
    }
    if tree_scoped && rule.kind == RuleKind::Site && rule.prefilter.is_empty() {
        warnings.push(format!(
            "rule '{}' is a `site` rule with an empty `prefilter` — it will never produce a site",
            rule.id
        ));
    }
    if rule.confirm == ConfirmForm::Search && rule.search.is_none() {
        warnings.push(format!(
            "rule '{}' declares `confirm: \"search\"` but has no `search` recipe — it will never enumerate anything",
            rule.id
        ));
    }
    if rule.confirm == ConfirmForm::Question && rule.compare.is_none() {
        warnings.push(format!(
            "rule '{}' declares `confirm: \"question\"` but has no `compare` question — it has nothing to ask",
            rule.id
        ));
    }
    warnings
}

fn warn_on_thin_rules(resolved: &[Rule], warnings: &mut Vec<String>) {
    for rule in resolved {
        warnings.extend(thin_rule_warnings(rule));
    }
}

/// Resolve a manifest's `rules: [ids...]` against the registry. Errors
/// loudly on an unknown id, listing every known id — never silently drops
/// an unresolvable rule.
pub fn resolve(ids: &[String], user_dir: Option<&Path>) -> Result<(Vec<Rule>, Vec<String>)> {
    let (map, mut warnings) = load_all(user_dir);
    let mut out = Vec::new();
    for id in ids {
        match map.get(id) {
            Some(r) => out.push(r.clone()),
            None => {
                let mut known: Vec<&str> = map.keys().map(|s| s.as_str()).collect();
                known.sort();
                // (#2298 review) A rule that failed to LOAD is "not found" here
                // too — a reserved `{"command": ...}` prefilter, a malformed
                // user file. Its load warning is the reason, so it rides the
                // error instead of being dropped with the warnings vector.
                let why: String = warnings
                    .iter()
                    .filter(|w| w.contains(&format!("'{id}'")) || w.contains(&format!("{id}.json")))
                    .map(|w| format!("\n  because: {w}"))
                    .collect();
                bail!(
                    "rule '{id}' not found — known rules: {}{why}",
                    if known.is_empty() {
                        "(none)".to_string()
                    } else {
                        known.join(", ")
                    }
                );
            }
        }
    }
    warn_on_thin_rules(&out, &mut warnings);
    Ok((out, warnings))
}

/// `resolve` against the real `<darkmux root>/rules` user tier.
pub fn resolve_default(ids: &[String]) -> Result<(Vec<Rule>, Vec<String>)> {
    let user_dir = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto)
        .root
        .join("rules");
    resolve(ids, Some(&user_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn embedded_rules_all_parse() {
        let (map, warnings) = load_all(None);
        assert!(warnings.is_empty(), "{warnings:?}");
        // (#2310 P4c) 4 crawl rules + 5 review-catalog rules.
        assert_eq!(map.len(), 9, "{:?}", map.keys().collect::<Vec<_>>());
        assert_eq!(map["swallowed-error"].kind, RuleKind::Site);
        assert_eq!(map["doc-contradicts-code"].kind, RuleKind::Read);
        assert_eq!(map["stale-consumer"].kind, RuleKind::Edge);
        assert_eq!(
            map["stale-consumer"].edge.as_ref().unwrap().ecosystem,
            "npm"
        );
        assert!(!map["swallowed-error"].prefilter.is_empty());
        // (#2206) unnamed-predicate is site-shaped and judgment-shaped: a
        // prefilter to narrow, prose `match`/`no_match` for the model, and a
        // `why_hint` that demands a SIGNATURE, not just a name.
        let up = &map["unnamed-predicate"];
        assert_eq!(up.kind, RuleKind::Site);
        assert_eq!(up.prefilter.len(), 3, "{:?}", up.prefilter);
        assert_eq!(up.window, Some(40));
        assert!(up.matches.as_deref().unwrap_or("").contains("THREE OR MORE operands"));
        assert!(up.no_match.as_deref().unwrap_or("").contains("null guard"));
        assert!(up.why_hint.as_deref().unwrap_or("").contains("SIGNATURE"));
        assert!(up.exclude.iter().any(|g| g.contains("*.test.")), "tests are excluded: {:?}", up.exclude);
    }

    #[test]
    fn resolve_known_ids_in_order() {
        let (rules, _) = resolve(
            &[
                "stale-consumer".to_string(),
                "swallowed-error".to_string(),
            ],
            None,
        )
        .unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].id, "stale-consumer");
        assert_eq!(rules[1].id, "swallowed-error");
    }

    #[test]
    fn resolve_unknown_id_errors_loudly_listing_known() {
        let err = resolve(&["not-a-real-rule".to_string()], None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not-a-real-rule"), "{msg}");
        assert!(msg.contains("swallowed-error"), "{msg}");
        assert!(msg.contains("doc-contradicts-code"), "{msg}");
        assert!(msg.contains("stale-consumer"), "{msg}");
        assert!(msg.contains("unnamed-predicate"), "{msg}");
    }

    /// #1959 finding 2: a user-tier override file only names the fields it
    /// wants to change. Before this fix, `load_all` REPLACED the embedded
    /// rule wholesale on a matching id (`map.insert(r.id.clone(), r)` with
    /// `r` being the freshly-parsed override alone) — every field the
    /// override didn't name (here, `applies_to`/`prefilter`) silently
    /// dropped to its serde default (an empty vec) instead of surviving
    /// from the embedded rule underneath.
    #[test]
    fn a_command_shaped_prefilter_is_refused_by_name_not_parsed_as_empty() {
        let dir = TempDir::new().unwrap();
        for (file, prefilter) in [
            ("obj.json", serde_json::json!({"command": "semgrep --config p/ts --sarif"})),
            ("mixed.json", serde_json::json!(["\\bif\\b", {"command": "ast-grep --json"}])),
        ] {
            fs::write(
                dir.path().join(file),
                serde_json::json!({
                    "id": format!("tool-{file}"), "kind": "site", "applies_to": ["**/*.ts"], "prefilter": prefilter
                })
                .to_string(),
            )
            .unwrap();
        }
        let (map, warnings) = load_all(Some(dir.path()));
        assert!(!map.contains_key("tool-obj.json") && !map.contains_key("tool-mixed.json"), "{map:?}");
        let named: Vec<&String> = warnings.iter().filter(|w| w.contains("#2297") && w.contains("command")).collect();
        assert_eq!(named.len(), 2, "each refusal names the reserved shape and the issue: {warnings:?}");
    }

    #[test]
    fn a_rule_refused_at_load_names_the_reason_when_resolved() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("tool-rule.json"),
            serde_json::json!({
                "id": "tool-rule", "kind": "site", "applies_to": ["**/*.ts"],
                "prefilter": {"command": "semgrep --sarif"}
            })
            .to_string(),
        )
        .unwrap();
        let err = resolve(&["tool-rule".to_string()], Some(dir.path())).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not found"), "{msg}");
        assert!(msg.contains("#2297") && msg.contains("command"), "the load refusal is the reason: {msg}");
    }

    #[test]
    fn rule_ids_usable_as_file_names() {
        for ok in ["unnamed-predicate", "swallowed_error", "r.1"] {
            assert!(is_safe_rule_id(ok), "{ok}");
        }
        for bad in ["", ".hidden", "a/b", "a b", "..", "a\\b"] {
            assert!(!is_safe_rule_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn user_tier_override_merges_named_fields_over_the_embedded_rule() {
        let dir = TempDir::new().unwrap();
        let override_json = serde_json::json!({
            "id": "swallowed-error",
            "kind": "site",
            "title": "operator override",
            "window": 5
        });
        fs::write(
            dir.path().join("swallowed-error.json"),
            override_json.to_string(),
        )
        .unwrap();

        let (map, warnings) = load_all(Some(dir.path()));
        assert!(warnings.is_empty(), "{warnings:?}");
        let merged = &map["swallowed-error"];
        // The override's own fields won.
        assert_eq!(merged.title.as_deref(), Some("operator override"));
        assert_eq!(merged.window, Some(5));
        // Everything the override didn't name survived from the embedded
        // rule underneath — this is the actual bug this test guards.
        let (embedded, _) = load_all(None);
        assert_eq!(merged.applies_to, embedded["swallowed-error"].applies_to);
        assert!(!merged.prefilter.is_empty(), "{merged:?}");
        assert_eq!(merged.prefilter, embedded["swallowed-error"].prefilter);
    }

    #[test]
    fn user_tier_override_array_field_replaces_rather_than_concatenates() {
        let dir = TempDir::new().unwrap();
        let override_json = serde_json::json!({
            "id": "swallowed-error",
            "kind": "site",
            "applies_to": ["**/*.custom"]
        });
        fs::write(
            dir.path().join("swallowed-error.json"),
            override_json.to_string(),
        )
        .unwrap();

        let (map, _) = load_all(Some(dir.path()));
        // Replaced, not appended to the embedded rule's own applies_to.
        assert_eq!(
            map["swallowed-error"].applies_to,
            vec!["**/*.custom".to_string()]
        );
    }

    #[test]
    fn malformed_user_rule_file_is_a_warning_not_a_crash() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("broken.json"), "{ not json").unwrap();

        let (map, warnings) = load_all(Some(dir.path()));
        // Embedded rules are still all present (#2310 P4c: 9, was 4).
        assert_eq!(map.len(), 9);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("broken.json"), "{warnings:?}");
    }

    #[test]
    fn resolve_warns_on_empty_applies_to_and_empty_site_prefilter() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("thin-site.json"),
            serde_json::json!({"id": "thin-site", "kind": "site"}).to_string(),
        )
        .unwrap();
        fs::write(
            dir.path().join("thin-read.json"),
            serde_json::json!({"id": "thin-read", "kind": "read", "applies_to": ["**/*.ts"]})
                .to_string(),
        )
        .unwrap();

        let (rules, warnings) = resolve(
            &["thin-site".to_string(), "thin-read".to_string()],
            Some(dir.path()),
        )
        .unwrap();
        assert_eq!(rules.len(), 2);

        // thin-site: empty applies_to AND empty prefilter -> two warnings.
        assert!(
            warnings.iter().any(|w| w.contains("thin-site") && w.contains("applies_to")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("thin-site") && w.contains("prefilter")),
            "{warnings:?}"
        );
        // thin-read: applies_to is populated, so no applies_to warning for it.
        assert!(
            !warnings.iter().any(|w| w.contains("thin-read")),
            "{warnings:?}"
        );
    }

    /// (#2310 P4c review round 2, SHOULD FIX (a)) A `scope: ["diff"]`
    /// `site` rule with an empty `applies_to`/`prefilter` is DELIBERATE
    /// config (DESIGN.md "Hunks are natural windows") and must produce NO
    /// thin-rule warning at all through `resolve()` — the manifest-scoped
    /// path `warn_on_thin_rules` (via `thin_rule_warnings`) serves,
    /// exactly mirroring `resolve_warns_on_empty_applies_to_and_empty_site_
    /// prefilter` above but for the diff-only case that check's own
    /// `tree_scoped` gate exists to exempt.
    #[test]
    fn resolve_does_not_warn_on_an_empty_applies_to_or_prefilter_for_a_diff_only_rule() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("diff-only-thin.json"),
            serde_json::json!({"id": "diff-only-thin", "kind": "site", "scope": ["diff"]}).to_string(),
        )
        .unwrap();

        let (rules, warnings) = resolve(&["diff-only-thin".to_string()], Some(dir.path())).unwrap();
        assert_eq!(rules.len(), 1);
        assert!(
            warnings.is_empty(),
            "a diff-only rule's empty applies_to/prefilter is deliberate, not thin: {warnings:?}"
        );
    }

    #[test]
    fn user_tier_adds_a_new_rule_id() {
        let dir = TempDir::new().unwrap();
        let new_rule = serde_json::json!({
            "id": "custom-rule",
            "kind": "read",
            "applies_to": ["**/*.py"]
        });
        fs::write(dir.path().join("custom-rule.json"), new_rule.to_string()).unwrap();

        let (rules, _) = resolve(&["custom-rule".to_string()], Some(dir.path())).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].kind, RuleKind::Read);
    }

    // --- #2310 P4c: scope / confirm / search / compare ---

    #[test]
    fn an_absent_scope_reads_as_both_tree_and_diff() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("no-scope.json"),
            serde_json::json!({"id": "no-scope", "kind": "site"}).to_string(),
        )
        .unwrap();
        let (rules, _) = resolve(&["no-scope".to_string()], Some(dir.path())).unwrap();
        assert_eq!(rules[0].scope, Vec::<RuleScope>::new(), "the wire field itself stays empty");
        assert_eq!(rules[0].scope_or_default(), &[RuleScope::Tree, RuleScope::Diff]);
        assert!(rules[0].applies_to_scope(RuleScope::Tree));
        assert!(rules[0].applies_to_scope(RuleScope::Diff));
    }

    #[test]
    fn an_explicit_single_element_scope_excludes_the_other() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("diff-only.json"),
            serde_json::json!({"id": "diff-only", "kind": "site", "scope": ["diff"]}).to_string(),
        )
        .unwrap();
        let (rules, _) = resolve(&["diff-only".to_string()], Some(dir.path())).unwrap();
        assert!(!rules[0].applies_to_scope(RuleScope::Tree));
        assert!(rules[0].applies_to_scope(RuleScope::Diff));
    }

    #[test]
    fn an_absent_confirm_defaults_to_mod() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("no-confirm.json"),
            serde_json::json!({
                "id": "no-confirm", "kind": "site", "applies_to": ["**/*.rs"], "prefilter": ["x"]
            })
            .to_string(),
        )
        .unwrap();
        let (rules, warnings) = resolve(&["no-confirm".to_string()], Some(dir.path())).unwrap();
        assert_eq!(rules[0].confirm, ConfirmForm::Mod);
        assert!(
            !warnings.iter().any(|w| w.contains("no-confirm")),
            "a mod-confirm rule with no search/compare gets no thin-rule warning \
             (`applies_to`/`prefilter` are populated so those unrelated thin-rule checks stay \
             quiet too): {warnings:?}"
        );
    }

    #[test]
    fn an_unrecognized_confirm_value_fails_to_parse_loudly() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("bad-confirm.json"),
            serde_json::json!({"id": "bad-confirm", "kind": "site", "confirm": "vibes"}).to_string(),
        )
        .unwrap();
        let (map, warnings) = load_all(Some(dir.path()));
        assert!(!map.contains_key("bad-confirm"), "{map:?}");
        assert!(
            warnings.iter().any(|w| w.contains("bad-confirm") && w.contains("failed to parse")),
            "an invalid confirm value is a loud, named warning, not a silent default: {warnings:?}"
        );
    }

    #[test]
    fn search_confirm_with_no_recipe_warns_and_question_confirm_with_no_compare_warns() {
        // (#2310 P4c review round 2, SHOULD FIX (b) — proven half-vacuous)
        // The rule id `thin-search` itself contains the substring
        // "search", so `w.contains("thin-search") && w.contains("search")`
        // was satisfied by ANY warning naming this rule — including the
        // unrelated "empty `applies_to`" warning this rule (declaring no
        // `applies_to`/`prefilter`) also triggers. The original assertion
        // could pass even if the search-recipe-specific check never fired
        // at all. Fixed two ways: `applies_to`/`prefilter` are populated
        // so the unrelated thin-rule checks stay quiet (same fix already
        // applied to `an_absent_confirm_defaults_to_mod` and
        // `a_search_recipe_and_a_compare_question_round_trip` above), and
        // the assertion checks for "recipe" — a word that appears ONLY in
        // the search-confirm-specific warning text, never in the rule id
        // or any other warning here.
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("thin-search.json"),
            serde_json::json!({
                "id": "thin-search", "kind": "site", "confirm": "search",
                "applies_to": ["**/*.rs"], "prefilter": ["x"]
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            dir.path().join("thin-question.json"),
            serde_json::json!({
                "id": "thin-question", "kind": "site", "confirm": "question",
                "applies_to": ["**/*.rs"], "prefilter": ["x"]
            })
            .to_string(),
        )
        .unwrap();
        let (_, warnings) = resolve(
            &["thin-search".to_string(), "thin-question".to_string()],
            Some(dir.path()),
        )
        .unwrap();
        assert_eq!(
            warnings.len(),
            2,
            "with applies_to/prefilter populated, only the confirm-specific checks should fire: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("thin-search") && w.contains("recipe")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("thin-question") && w.contains("compare")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_search_recipe_and_a_compare_question_round_trip() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("full-search.json"),
            serde_json::json!({
                "id": "full-search",
                "kind": "site",
                "confirm": "search",
                "search": {"patterns": ["fn helper_name"], "note": "list every caller"},
                "applies_to": ["**/*.rs"],
                "prefilter": ["helper_name"]
            })
            .to_string(),
        )
        .unwrap();
        let (rules, warnings) = resolve(&["full-search".to_string()], Some(dir.path())).unwrap();
        assert!(
            !warnings.iter().any(|w| w.contains("full-search")),
            "a search rule with a recipe gets no thin-rule warning: {warnings:?}"
        );
        let recipe = rules[0].search.as_ref().expect("search recipe present");
        assert_eq!(recipe.patterns, vec!["fn helper_name".to_string()]);
        assert_eq!(recipe.note.as_deref(), Some("list every caller"));
    }

    /// (#2310 P4c) The four crawl rules gained `scope`/`confirm` with no
    /// behavior change: every embedded rule still parses, and the two
    /// site-shaped rules the review catalog reuses (`swallowed-error`,
    /// `unnamed-predicate`) declare both scopes so a diff-scoped review can
    /// select them; the read/edge-shaped rules stay tree-only, since
    /// neither a whole-file read pass nor an npm-range edge check has a
    /// diff-scoped meaning.
    #[test]
    fn the_four_crawl_rules_gained_scope_and_confirm_with_no_behavior_change() {
        let (map, warnings) = load_all(None);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(map["swallowed-error"].applies_to_scope(RuleScope::Diff));
        assert!(map["unnamed-predicate"].applies_to_scope(RuleScope::Diff));
        assert!(!map["doc-contradicts-code"].applies_to_scope(RuleScope::Diff));
        assert!(!map["stale-consumer"].applies_to_scope(RuleScope::Diff));
        for id in ["swallowed-error", "unnamed-predicate", "doc-contradicts-code", "stale-consumer"] {
            assert_eq!(map[id].confirm, ConfirmForm::Mod, "{id}");
        }
    }
}
