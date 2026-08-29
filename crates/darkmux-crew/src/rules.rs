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
pub fn load_all(user_dir: Option<&Path>) -> (BTreeMap<String, Rule>, Vec<String>) {
    let mut map = BTreeMap::new();
    let mut warnings = Vec::new();

    for (id, json) in EMBEDDED_RULES {
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
/// case, silently before this fix — #1959 finding 2). Runs only over the
/// rules a manifest actually RESOLVED for use — not every rule sitting in
/// the registry — so an unrelated user-authored rule id never generates
/// noise for a manifest that doesn't reference it.
fn warn_on_thin_rules(resolved: &[Rule], warnings: &mut Vec<String>) {
    for rule in resolved {
        if matches!(rule.kind, RuleKind::Site | RuleKind::Read) && rule.applies_to.is_empty() {
            warnings.push(format!(
                "rule '{}' has an empty `applies_to` — it will never match any file",
                rule.id
            ));
        }
        if rule.kind == RuleKind::Site && rule.prefilter.is_empty() {
            warnings.push(format!(
                "rule '{}' is a `site` rule with an empty `prefilter` — it will never produce a site",
                rule.id
            ));
        }
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
                bail!(
                    "rule '{id}' not found — known rules: {}",
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
        assert_eq!(map.len(), 3, "{:?}", map.keys().collect::<Vec<_>>());
        assert_eq!(map["swallowed-error"].kind, RuleKind::Site);
        assert_eq!(map["doc-contradicts-code"].kind, RuleKind::Read);
        assert_eq!(map["stale-consumer"].kind, RuleKind::Edge);
        assert_eq!(
            map["stale-consumer"].edge.as_ref().unwrap().ecosystem,
            "npm"
        );
        assert!(!map["swallowed-error"].prefilter.is_empty());
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
    }

    /// #1959 finding 2: a user-tier override file only names the fields it
    /// wants to change. Before this fix, `load_all` REPLACED the embedded
    /// rule wholesale on a matching id (`map.insert(r.id.clone(), r)` with
    /// `r` being the freshly-parsed override alone) — every field the
    /// override didn't name (here, `applies_to`/`prefilter`) silently
    /// dropped to its serde default (an empty vec) instead of surviving
    /// from the embedded rule underneath.
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
        // Embedded rules are still all present.
        assert_eq!(map.len(), 3);
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
}
