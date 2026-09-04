//! (#2310 P4c-2 item 0) Generic `{{<input-id>}}` substitution for step
//! configs, applied at mint from a launch's own collected inputs.
//!
//! This is the ONE mechanism that replaces `mission_launch.rs`'s old
//! `crawl_plan_step_overrides`, which special-cased exactly one step kind
//! (`crawl.plan`) and therefore reached zero of `review-v2.json`'s
//! `plan.sites` steps — the P4c-1 BLOCKER (see `P4c-brief-draft.md`'s
//! "Status" section). A document declares an input in its own `inputs`
//! list (`MissionConfig::inputs`) and references it anywhere in a step's
//! `config` as `{{<name>}}`; [`substitute_step_config`] resolves every such
//! placeholder from the launch's collected values, the SAME namespace
//! `grow.rs`'s `{{item.<field>}}`/`{{from.output}}` occupies for grown
//! configs — this module never touches those (grow's own substitution
//! already resolved them before a grown config reaches here).
//!
//! **Absent optional inputs.** A placeholder naming a declared-but-not-
//! collected input (an optional input the operator left unset) is not an
//! error: a WHOLE-value placeholder (`"workspace": "{{workspace}}"`, the
//! value is exactly one placeholder and nothing else) has its KEY OMITTED
//! from the containing object entirely, so a consumer's `step.config.
//! get("no_fetch")` sees "not present" — identical to what it saw before
//! the document ever declared the placeholder. An EMBEDDED placeholder
//! (part of a larger string) substitutes the empty string instead, since
//! there is no key to omit.
//!
//! **Unknown placeholders are refused.** A placeholder naming anything
//! other than a declared input is a document bug: minting must not proceed
//! with a literal `{{...}}` a step kind would silently mis-parse (or, worse,
//! silently accept as a literal string). [`substitute_step_config`] errors
//! loudly, naming the step and the placeholder, before anything mints.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Substitute every declared-input placeholder in one step's `config`,
/// returning the rendered value. `declared` is the document's own
/// `MissionConfig::inputs` names (the legal placeholder vocabulary);
/// `collected` is what the launch actually gathered (`--input`/`--param`).
/// `what` names the step, for error messages.
pub fn substitute_step_config(
    config: &Value,
    declared: &BTreeSet<String>,
    collected: &BTreeMap<String, Value>,
    what: &str,
) -> Result<Value> {
    Ok(substitute_value(config, declared, collected, what)?.unwrap_or(Value::Null))
}

/// `Ok(None)` means "omit the containing object key" — only ever produced
/// by a whole-value placeholder naming a declared-but-uncollected input.
fn substitute_value(
    value: &Value,
    declared: &BTreeSet<String>,
    collected: &BTreeMap<String, Value>,
    what: &str,
) -> Result<Option<Value>> {
    match value {
        Value::String(s) => {
            if let Some(name) = whole_placeholder(s) {
                check_declared(name, declared, what)?;
                Ok(collected.get(name).cloned())
            } else if s.contains("{{") {
                Ok(Some(Value::String(render_embedded(s, declared, collected, what)?)))
            } else {
                Ok(Some(value.clone()))
            }
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(substitute_value(item, declared, collected, what)?.unwrap_or(Value::Null));
            }
            Ok(Some(Value::Array(out)))
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                if let Some(substituted) = substitute_value(v, declared, collected, what)? {
                    out.insert(k.clone(), substituted);
                }
                // else: an unset optional input's whole-placeholder value —
                // omit the key so a consumer sees "not present", matching
                // the document's pre-placeholder behavior.
            }
            Ok(Some(Value::Object(out)))
        }
        other => Ok(Some(other.clone())),
    }
}

/// Every `{{...}}` occurrence in `s`, substituted from `collected` (empty
/// string for a declared-but-uncollected input — there is no key here to
/// omit). Mirrors `grow.rs::render`'s scan loop; the placeholder namespace
/// here is bare declared-input names rather than `item.*`/`from.output`.
fn render_embedded(
    s: &str,
    declared: &BTreeSet<String>,
    collected: &BTreeMap<String, Value>,
    what: &str,
) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let close = after
            .find("}}")
            .with_context(|| format!("{what}: has an unclosed `{{{{` placeholder: `{s}`"))?;
        let name = after[..close].trim();
        check_declared(name, declared, what)?;
        if let Some(v) = collected.get(name) {
            out.push_str(&scalar_to_string(v));
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `"{{name}}"` (and nothing else) -> `Some("name")`.
fn whole_placeholder(s: &str) -> Option<&str> {
    let inner = s.strip_prefix("{{")?.strip_suffix("}}")?;
    if inner.contains("{{") || inner.contains("}}") {
        return None;
    }
    Some(inner.trim())
}

fn check_declared(name: &str, declared: &BTreeSet<String>, what: &str) -> Result<()> {
    if !declared.contains(name) {
        bail!(
            "{what}: names placeholder `{{{{{name}}}}}`, which is not a declared input of this \
             mission config — declared inputs: {}",
            if declared.is_empty() {
                "none".to_string()
            } else {
                declared.iter().cloned().collect::<Vec<_>>().join(", ")
            }
        );
    }
    Ok(())
}

/// Every literal `{{` still present in `value` — a real launch must leave
/// none anywhere in a minted step's config (P4c-2 item 0's own test
/// obligation). Recurses through arrays/objects; used only by tests, here
/// and in `mission_launch.rs`'s own no-literal-braces proof.
pub fn find_unsubstituted_braces(value: &Value, path: &str, out: &mut Vec<String>) {
    match value {
        Value::String(s) if s.contains("{{") => out.push(format!("{path}: `{s}`")),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                find_unsubstituted_braces(item, &format!("{path}[{i}]"), out);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                find_unsubstituted_braces(v, &format!("{path}.{k}"), out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn declared(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn whole_placeholder_substitutes_the_collected_value_verbatim() {
        let mut collected = BTreeMap::new();
        collected.insert("workspace".to_string(), json!("/tmp/ws.json"));
        let cfg = json!({"rule": "x", "workspace": "{{workspace}}"});
        let out = substitute_step_config(&cfg, &declared(&["workspace"]), &collected, "step `s`").unwrap();
        assert_eq!(out, json!({"rule": "x", "workspace": "/tmp/ws.json"}));
    }

    #[test]
    fn a_declared_but_uncollected_whole_placeholder_omits_the_key() {
        let collected = BTreeMap::new();
        let cfg = json!({"rule": "x", "head_sha": "{{head_sha}}"});
        let out =
            substitute_step_config(&cfg, &declared(&["head_sha"]), &collected, "step `s`").unwrap();
        assert_eq!(out, json!({"rule": "x"}), "the key is gone, not an empty string");
    }

    #[test]
    fn an_embedded_placeholder_renders_empty_string_when_uncollected() {
        let collected = BTreeMap::new();
        let cfg = json!({"note": "plan={{workspace}} done"});
        let out =
            substitute_step_config(&cfg, &declared(&["workspace"]), &collected, "step `s`").unwrap();
        assert_eq!(out, json!({"note": "plan= done"}));
    }

    #[test]
    fn an_undeclared_placeholder_is_refused_naming_the_step_and_placeholder() {
        let collected = BTreeMap::new();
        let cfg = json!({"rule": "{{bogus}}"});
        let err = substitute_step_config(&cfg, &declared(&["workspace"]), &collected, "step `plan-step`")
            .unwrap_err()
            .to_string();
        assert!(err.contains("plan-step"), "{err}");
        assert!(err.contains("bogus"), "{err}");
        assert!(err.contains("workspace"), "{err}");
    }

    #[test]
    fn nested_objects_and_arrays_substitute_recursively() {
        let mut collected = BTreeMap::new();
        collected.insert("a".to_string(), json!("A"));
        let cfg = json!({"outer": {"inner": ["{{a}}", "x"]}});
        let out = substitute_step_config(&cfg, &declared(&["a"]), &collected, "step `s`").unwrap();
        assert_eq!(out, json!({"outer": {"inner": ["A", "x"]}}));
    }

    #[test]
    fn find_unsubstituted_braces_reports_the_path() {
        let cfg = json!({"a": {"b": "{{oops}}"}});
        let mut found = Vec::new();
        find_unsubstituted_braces(&cfg, "config", &mut found);
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("config.a.b"), "{}", found[0]);
        assert!(found[0].contains("oops"), "{}", found[0]);
    }
}
