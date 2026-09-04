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
//! error ONLY in the whole-value shape: `"workspace": "{{workspace}}"`
//! (the value is exactly one placeholder and nothing else) has its KEY
//! OMITTED from the containing object entirely, so a consumer's
//! `step.config.get("no_fetch")` sees "not present" — identical to what it
//! saw before the document ever declared the placeholder. An EMBEDDED
//! placeholder (part of a larger string, e.g. `"/tmp/{{x}}/b"`) is
//! REFUSED instead when `x` is uncollected (#2310 P4c-2 review item 2) —
//! there is no key to omit, and silently rendering the empty string would
//! have produced `"/tmp//b"`, a silently shortened path a step kind reads
//! as a real value with no signal anything was missing. The same
//! reasoning applies inside an ARRAY: a whole-value placeholder element
//! that names an uncollected input is DROPPED from the array (never a
//! `null` element) — omission, the same rule the object case uses, just
//! applied to list membership instead of a map key.
//!
//! **Unknown placeholders are refused.** A placeholder naming anything
//! other than a declared input is a document bug: minting must not proceed
//! with a literal `{{...}}` a step kind would silently mis-parse (or, worse,
//! silently accept as a literal string). [`substitute_step_config`] errors
//! loudly, naming the step and the placeholder, before anything mints —
//! and (#2310 P4c-2 review MUST FIX) [`check_placeholders_declared`] runs
//! the SAME check over the whole static document (every step's `config`
//! AND every task's `grow.config`) BEFORE any of that minting starts, so a
//! typo is refused before the mission directory exists and before a
//! `grow`-templated phase's real dispatch work ever runs. Without it,
//! `interpret`'s own per-step check alone caught a static typo only AFTER
//! the mission was already partially minted on disk, and a `grow.config`
//! typo only after the ENTIRE producing phase had already run for real
//! (dispatches and all) — and `--dry-run` caught neither, since it never
//! calls `interpret` at all.

use super::MissionConfig;
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

/// `Ok(None)` means "omit the containing object key / drop this array
/// element" — only ever produced by a whole-value placeholder naming a
/// declared-but-uncollected input.
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
            // (#2310 P4c-2 review item 2) A whole-placeholder element
            // naming an uncollected input is DROPPED, never inserted as
            // `null` — the same "absent means not there" rule the object
            // branch below applies to a key, applied here to list
            // membership instead.
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                if let Some(substituted) = substitute_value(item, declared, collected, what)? {
                    out.push(substituted);
                }
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

/// Every `{{...}}` occurrence in `s`. Mirrors `grow.rs::render`'s scan
/// loop; the placeholder namespace here is bare declared-input names
/// rather than `item.*`/`from.output`. (#2310 P4c-2 review item 2) A
/// declared-but-uncollected input reached EMBEDDED (part of a larger
/// string) is a loud refusal, not a silent empty-string render — there is
/// no key to omit here the way the whole-value case has, and rendering
/// empty would silently shorten whatever string this placeholder sits
/// inside (a path, most likely).
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
        match collected.get(name) {
            Some(v) => out.push_str(&scalar_to_string(v)),
            None => bail!(
                "{what}: names embedded placeholder `{{{{{name}}}}}` inside `{s}`, but input \
                 `{name}` is not set for this launch — an EMBEDDED placeholder cannot omit part \
                 of a string (rendering empty would silently shorten it); only a WHOLE-value \
                 placeholder (the field's entire value is `{{{{{name}}}}}` and nothing else) can \
                 be omitted. Supply `{name}`, or move the placeholder to its own field"
            ),
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
             mission config{} — declared inputs: {}",
            grow_namespace_hint(name),
            if declared.is_empty() {
                "none".to_string()
            } else {
                declared.iter().cloned().collect::<Vec<_>>().join(", ")
            }
        );
    }
    Ok(())
}

/// (#2310 P4c-2 review item 5) A placeholder that SURVIVED `grow.rs`'s own
/// pass verbatim (item 0's design: `grow::render` only recognizes
/// `item.*`/`from.output` and passes anything else through unresolved for
/// THIS module to resolve or refuse) but still starts with `item.` or
/// `from.` is very likely a mistyped grow-namespace reference
/// (`{{from.typo}}` meaning `{{from.output}}`), not a genuinely missing
/// launch input — "declared inputs: workspace, diff_file, ..." is a
/// misleading answer to that mistake. Named separately so the message
/// points at the actual namespace instead. Empty string for anything that
/// isn't grow-namespace-shaped.
pub(crate) fn grow_namespace_hint(name: &str) -> &'static str {
    if name == "from.output" {
        // Exact match already resolves inside grow.rs and never reaches
        // here — kept out of the two arms below so this function's
        // contract stays "only for a MISTYPED grow reference".
        ""
    } else if name.starts_with("from.") {
        " (this looks like a mistyped grow namespace: the only producer-output placeholder is `{{from.output}}`, valid only inside a `grow.config` template)"
    } else if name.starts_with("item.") {
        " (this looks like a mistyped grow namespace: `{{item.<field>}}` is valid only inside a `grow.config` template, never in a static step's config)"
    } else {
        ""
    }
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

/// (#2310 P4c-2 review MUST FIX) Every `{{name}}` occurrence in the
/// document's STATIC graph — every step's `config` AND every task's
/// `grow.config` (the two places [`substitute_step_config`] ever runs) —
/// that names neither grow's own namespace (`item.*`/`from.output`, valid
/// only inside a `grow.config` template and resolved by `grow.rs`, never
/// by this module) nor a name in `config.inputs`. One entry per bad
/// occurrence: `(location, placeholder_name)`. A `grow.id` template string
/// (e.g. `"{{item.id}}"`) is NOT scanned — it is exclusively `item.*`/
/// `from.output` by convention and by [`super::GrowSpec`]'s own contract,
/// so there is nothing this check would ever catch there; scoping to
/// `config` keeps this function's job identical to what actually mints.
pub fn undeclared_placeholders(config: &MissionConfig) -> Vec<(String, String)> {
    let declared: BTreeSet<String> = config.inputs.iter().map(|i| i.name.clone()).collect();
    let mut out = Vec::new();
    for phase in &config.phases {
        for task in &phase.tasks {
            for step in &task.steps {
                collect_undeclared(&step.config, &declared, &format!("step `{}`", step.id), &mut out);
            }
            if let Some(grow) = &task.grow {
                collect_undeclared(
                    &grow.config,
                    &declared,
                    &format!("task `{}`'s grow.config", task.id),
                    &mut out,
                );
            }
        }
    }
    out
}

fn collect_undeclared(value: &Value, declared: &BTreeSet<String>, where_: &str, out: &mut Vec<(String, String)>) {
    match value {
        Value::String(s) => {
            for name in placeholder_names(s) {
                if name.starts_with("item.") || name == "from.output" {
                    continue;
                }
                if !declared.contains(&name) {
                    out.push((where_.to_string(), name));
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_undeclared(v, declared, where_, out);
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                collect_undeclared(v, declared, where_, out);
            }
        }
        _ => {}
    }
}

/// Every well-formed `{{...}}` name in `s`, in order. An unclosed `{{` is
/// silently not reported here — [`substitute_step_config`]/`grow.rs`'s own
/// scan is what refuses a malformed placeholder at mint; this function's
/// job is only to name what a WELL-FORMED but undeclared one would say.
fn placeholder_names(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        match after.find("}}") {
            Some(close) => {
                out.push(after[..close].trim().to_string());
                rest = &after[close + 2..];
            }
            None => break,
        }
    }
    out
}

/// (#2310 P4c-2 review MUST FIX) Refuses (loud `Err`, naming every
/// offending step/template and placeholder) if [`undeclared_placeholders`]
/// finds anything in `config`'s static graph. The caller
/// (`mission_launch.rs::launch`) runs this BEFORE the `--dry-run`
/// short-circuit and before anything mints, closing the two gaps this
/// review found: a static-step typo used to mint the mission directory
/// before failing inside `interpret`; a `grow.config` typo used to run an
/// entire real phase (dispatch and all) before dying at the NEXT phase's
/// boundary inside `interpret_grown`; and `--dry-run` caught neither,
/// since the dry-run path never calls `interpret` at all.
pub fn check_placeholders_declared(config: &MissionConfig) -> Result<()> {
    let bad = undeclared_placeholders(config);
    if bad.is_empty() {
        return Ok(());
    }
    let declared: Vec<&str> = config.inputs.iter().map(|i| i.name.as_str()).collect();
    let lines: Vec<String> = bad
        .iter()
        .map(|(where_, name)| {
            format!("  {where_} names placeholder `{{{{{name}}}}}`{}", grow_namespace_hint(name))
        })
        .collect();
    bail!(
        "mission config \"{}\" has {} undeclared placeholder occurrence(s), refused before minting:\n{}\ndeclared inputs: {}",
        config.id,
        bad.len(),
        lines.join("\n"),
        if declared.is_empty() { "none".to_string() } else { declared.join(", ") }
    );
}

/// (#2310 P4c-2 review round 2, item a) Every EMBEDDED (not whole-value)
/// placeholder occurrence in the document's static graph — `(location,
/// placeholder_name)` pairs, skipping `item.*`/`from.output`, UNFILTERED
/// by declared-ness (the caller decides what to do with a name). Shared by
/// [`check_embedded_inputs_collected`] (filters to declared-but-
/// uncollected, at launch time, against one launch's `collected`) and
/// `MissionConfig::validate`'s own Warning (filters to declared-and-
/// optional, independent of any particular launch).
pub(crate) fn embedded_placeholders(config: &MissionConfig) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for phase in &config.phases {
        for task in &phase.tasks {
            for step in &task.steps {
                collect_embedded(&step.config, &format!("step `{}`", step.id), &mut out);
            }
            if let Some(grow) = &task.grow {
                collect_embedded(&grow.config, &format!("task `{}`'s grow.config", task.id), &mut out);
            }
        }
    }
    out
}

fn collect_embedded(value: &Value, where_: &str, out: &mut Vec<(String, String)>) {
    match value {
        Value::String(s) => {
            // A WHOLE-value placeholder for an uncollected input is the
            // documented "omit the key" case, never this function's
            // concern — only an EMBEDDED occurrence (part of a larger
            // string) is at risk of silently shortening that string.
            if whole_placeholder(s).is_some() {
                return;
            }
            for name in placeholder_names(s) {
                if name.starts_with("item.") || name == "from.output" {
                    continue;
                }
                out.push((where_.to_string(), name));
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_embedded(v, where_, out);
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                collect_embedded(v, where_, out);
            }
        }
        _ => {}
    }
}

/// (#2310 P4c-2 review round 2, item a) Refuses (loud `Err`) if any
/// EMBEDDED placeholder in the document's static graph names a DECLARED
/// input this launch's `collected` does not carry. Companion to
/// [`check_placeholders_declared`] — same call site in
/// `mission_launch.rs::launch` (before `--dry-run`, before any mint),
/// closing the SAME "caught too late" shape for an input that IS declared
/// but happens to be optional-and-unset: before this, only `interpret`'s
/// own `substitute_step_config` refused it (round-1 item 2), which runs
/// AFTER `--dry-run`'s short-circuit and AFTER minting — a real launch
/// minted a mission directory, then failed and abandoned it; `--dry-run`
/// exited 0, silent.
pub fn check_embedded_inputs_collected(
    config: &MissionConfig,
    collected: &BTreeMap<String, Value>,
) -> Result<()> {
    let declared: BTreeSet<String> = config.inputs.iter().map(|i| i.name.clone()).collect();
    let bad: Vec<(String, String)> = embedded_placeholders(config)
        .into_iter()
        .filter(|(_, name)| declared.contains(name) && !collected.contains_key(name))
        .collect();
    if bad.is_empty() {
        return Ok(());
    }
    let lines: Vec<String> = bad
        .iter()
        .map(|(where_, name)| {
            format!(
                "  {where_} names embedded placeholder `{{{{{name}}}}}`, but input `{name}` is not \
                 set for this launch (declared optional, never supplied) — an embedded placeholder \
                 cannot omit part of a string the way a whole-value placeholder can"
            )
        })
        .collect();
    bail!(
        "mission config \"{}\" has {} embedded placeholder(s) naming an unset input, refused before minting:\n{}",
        config.id,
        bad.len(),
        lines.join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission_config::{GrowSpec, PhaseConfig, StepConfig, TaskConfig};
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

    /// (#2310 P4c-2 review item 2 — proven) Was
    /// `an_embedded_placeholder_renders_empty_string_when_uncollected`: an
    /// uncollected input reached embedded now REFUSES instead of silently
    /// shortening the string.
    #[test]
    fn an_embedded_placeholder_is_refused_when_uncollected() {
        let collected = BTreeMap::new();
        let cfg = json!({"note": "plan={{workspace}} done"});
        let err = substitute_step_config(&cfg, &declared(&["workspace"]), &collected, "step `s`")
            .unwrap_err()
            .to_string();
        assert!(err.contains("workspace"), "{err}");
        assert!(err.contains("embedded"), "{err}");
    }

    #[test]
    fn an_embedded_placeholder_still_substitutes_when_collected() {
        let mut collected = BTreeMap::new();
        collected.insert("workspace".to_string(), json!("ws"));
        let cfg = json!({"note": "plan={{workspace}} done"});
        let out =
            substitute_step_config(&cfg, &declared(&["workspace"]), &collected, "step `s`").unwrap();
        assert_eq!(out, json!({"note": "plan=ws done"}));
    }

    /// (#2310 P4c-2 review item 2 — proven) An uncollected whole-placeholder
    /// ARRAY element is DROPPED, never a `null` entry.
    #[test]
    fn an_uncollected_whole_placeholder_array_element_is_dropped_not_null() {
        let mut collected = BTreeMap::new();
        collected.insert("a".to_string(), json!("A"));
        let cfg = json!({"list": ["{{a}}", "{{b}}", "x"]});
        let out = substitute_step_config(&cfg, &declared(&["a", "b"]), &collected, "step `s`").unwrap();
        assert_eq!(out, json!({"list": ["A", "x"]}), "no null in the middle");
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

    /// (#2310 P4c-2 review item 5 — proven) A `{{from.typo}}` that
    /// survives `grow.rs`'s own pass verbatim must NOT be told "declared
    /// inputs: workspace, diff_file" (misleading) — it must be told it
    /// looks like a mistyped grow namespace.
    #[test]
    fn a_from_dot_typo_names_the_grow_namespace_not_the_declared_inputs() {
        let collected = BTreeMap::new();
        let cfg = json!({"plan": "{{from.typo}}"});
        let err = substitute_step_config(&cfg, &declared(&["workspace"]), &collected, "step `unit-step`")
            .unwrap_err()
            .to_string();
        assert!(err.contains("from.typo"), "{err}");
        assert!(err.contains("from.output"), "{err}");
        assert!(err.contains("grow"), "{err}");
    }

    #[test]
    fn an_item_dot_typo_in_a_static_step_names_the_grow_namespace() {
        let collected = BTreeMap::new();
        let cfg = json!({"plan": "{{item.rule}}"});
        let err = substitute_step_config(&cfg, &declared(&["workspace"]), &collected, "step `plan-step`")
            .unwrap_err()
            .to_string();
        assert!(err.contains("item.rule"), "{err}");
        assert!(err.contains("grow.config"), "{err}");
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

    // ─── check_placeholders_declared (MUST FIX) ──────────────────────

    fn minimal_config(phases: Vec<PhaseConfig>) -> MissionConfig {
        MissionConfig {
            id: "test-config".to_string(),
            name: "Test".to_string(),
            description: None,
            schema_version: None,
            inputs: vec![crate::mission_config::MissionInput {
                name: "workspace".to_string(),
                description: None,
                required: Some(true),
                ignored: None,
                ignored_reason: None,
                extras: BTreeMap::new(),
            }],
            phases,
            outcome_from: None,
            panel: None,
            cmd: None,
            extras: BTreeMap::new(),
        }
    }

    fn step(id: &str, kind: &str, config: Value) -> StepConfig {
        StepConfig { id: id.to_string(), kind: kind.to_string(), enabled: None, config, gate: None, extras: BTreeMap::new() }
    }

    fn task(id: &str, steps: Vec<StepConfig>) -> TaskConfig {
        TaskConfig {
            id: id.to_string(),
            enabled: None,
            description: None,
            display_name: None,
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            run_on: None,
            steps,
            grow: None,
            extras: BTreeMap::new(),
        }
    }

    #[test]
    fn a_clean_document_passes_the_placeholder_check() {
        let cfg = minimal_config(vec![PhaseConfig {
            id: "p".to_string(),
            display_name: None,
            description: None,
            enabled: None,
            tasks: vec![task("t", vec![step("s", "k", json!({"workspace": "{{workspace}}"}))])],
            extras: BTreeMap::new(),
        }]);
        assert!(check_placeholders_declared(&cfg).is_ok());
    }

    /// (#2310 P4c-2 review MUST FIX — proven) A STATIC step's typo is
    /// refused by `check_placeholders_declared` alone, with no `interpret`
    /// call and no mission ever minted.
    #[test]
    fn a_static_step_typo_is_refused_naming_the_step_and_placeholder() {
        let cfg = minimal_config(vec![PhaseConfig {
            id: "p".to_string(),
            display_name: None,
            description: None,
            enabled: None,
            tasks: vec![task("t", vec![step("s", "k", json!({"intent_file": "{{intent_fle}}"}))])],
            extras: BTreeMap::new(),
        }]);
        let err = check_placeholders_declared(&cfg).unwrap_err().to_string();
        assert!(err.contains("step `s`"), "{err}");
        assert!(err.contains("intent_fle"), "{err}");
    }

    /// (#2310 P4c-2 review MUST FIX — proven) A `grow.config` typo is
    /// refused the SAME way, without needing the producing phase to run
    /// first.
    #[test]
    fn a_grow_config_typo_is_refused_before_any_phase_runs() {
        let mut t = task("t", vec![step("s", "k", json!({}))]);
        t.grow = Some(GrowSpec {
            from: "other".to_string(),
            items: "units".to_string(),
            id: "{{item.id}}".to_string(),
            config: json!({"intent_file": "{{intent_fle}}"}),
            extras: BTreeMap::new(),
        });
        let cfg = minimal_config(vec![PhaseConfig {
            id: "p".to_string(),
            display_name: None,
            description: None,
            enabled: None,
            tasks: vec![t],
            extras: BTreeMap::new(),
        }]);
        let err = check_placeholders_declared(&cfg).unwrap_err().to_string();
        assert!(err.contains("grow.config"), "{err}");
        assert!(err.contains("intent_fle"), "{err}");
    }

    /// A well-formed `item.*`/`from.output` reference inside `grow.config`
    /// is grow's own namespace, never refused here.
    #[test]
    fn grow_namespace_placeholders_in_grow_config_are_never_flagged() {
        let mut t = task("t", vec![step("s", "k", json!({}))]);
        t.grow = Some(GrowSpec {
            from: "other".to_string(),
            items: "units".to_string(),
            id: "{{item.id}}".to_string(),
            config: json!({"plan": "{{from.output}}", "unit": "{{item.id}}"}),
            extras: BTreeMap::new(),
        });
        let cfg = minimal_config(vec![PhaseConfig {
            id: "p".to_string(),
            display_name: None,
            description: None,
            enabled: None,
            tasks: vec![t],
            extras: BTreeMap::new(),
        }]);
        assert!(check_placeholders_declared(&cfg).is_ok());
    }

    // ─── check_embedded_inputs_collected (review round 2, item a) ─────

    fn config_with_optional_tag(phases: Vec<PhaseConfig>) -> MissionConfig {
        let mut cfg = minimal_config(phases);
        cfg.inputs.push(crate::mission_config::MissionInput {
            name: "tag".to_string(),
            description: None,
            required: Some(false),
            ignored: None,
            ignored_reason: None,
            extras: BTreeMap::new(),
        });
        cfg
    }

    #[test]
    fn an_embedded_placeholder_naming_an_uncollected_optional_input_is_refused() {
        let cfg = config_with_optional_tag(vec![PhaseConfig {
            id: "p".to_string(),
            display_name: None,
            description: None,
            enabled: None,
            tasks: vec![task("t", vec![step("t-step", "k", json!({"label": "run-{{tag}}"}))])],
            extras: BTreeMap::new(),
        }]);
        let collected: BTreeMap<String, Value> =
            [("workspace".to_string(), json!("/tmp/ws.json"))].into_iter().collect();
        let err = check_embedded_inputs_collected(&cfg, &collected).unwrap_err().to_string();
        assert!(err.contains("t-step"), "{err}");
        assert!(err.contains("tag"), "{err}");
    }

    #[test]
    fn an_embedded_placeholder_naming_a_collected_optional_input_is_fine() {
        let cfg = config_with_optional_tag(vec![PhaseConfig {
            id: "p".to_string(),
            display_name: None,
            description: None,
            enabled: None,
            tasks: vec![task("t", vec![step("t-step", "k", json!({"label": "run-{{tag}}"}))])],
            extras: BTreeMap::new(),
        }]);
        let collected: BTreeMap<String, Value> = [
            ("workspace".to_string(), json!("/tmp/ws.json")),
            ("tag".to_string(), json!("nightly")),
        ]
        .into_iter()
        .collect();
        assert!(check_embedded_inputs_collected(&cfg, &collected).is_ok());
    }

    #[test]
    fn a_whole_value_placeholder_for_an_uncollected_optional_input_is_never_flagged_here() {
        // `check_embedded_inputs_collected` is specifically about EMBEDDED
        // occurrences — the whole-value case is the documented "omit the
        // key" outcome `substitute_step_config` itself already handles.
        let cfg = config_with_optional_tag(vec![PhaseConfig {
            id: "p".to_string(),
            display_name: None,
            description: None,
            enabled: None,
            tasks: vec![task("t", vec![step("t-step", "k", json!({"tag": "{{tag}}"}))])],
            extras: BTreeMap::new(),
        }]);
        let collected: BTreeMap<String, Value> =
            [("workspace".to_string(), json!("/tmp/ws.json"))].into_iter().collect();
        assert!(check_embedded_inputs_collected(&cfg, &collected).is_ok());
    }

    #[test]
    fn an_embedded_placeholder_naming_an_uncollected_optional_input_in_grow_config_is_refused() {
        let mut t = task("t", vec![step("t-step", "k", json!({}))]);
        t.grow = Some(GrowSpec {
            from: "other".to_string(),
            items: "units".to_string(),
            id: "{{item.id}}".to_string(),
            config: json!({"label": "run-{{tag}}"}),
            extras: BTreeMap::new(),
        });
        let cfg = config_with_optional_tag(vec![PhaseConfig {
            id: "p".to_string(),
            display_name: None,
            description: None,
            enabled: None,
            tasks: vec![t],
            extras: BTreeMap::new(),
        }]);
        let collected: BTreeMap<String, Value> =
            [("workspace".to_string(), json!("/tmp/ws.json"))].into_iter().collect();
        let err = check_embedded_inputs_collected(&cfg, &collected).unwrap_err().to_string();
        assert!(err.contains("grow.config"), "{err}");
        assert!(err.contains("tag"), "{err}");
    }
}
