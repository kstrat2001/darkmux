//! (#2300) Growth — a step's OUTPUT grows tasks into the graph.
//!
//! The seam this module owns is small and deliberately dumb: given one
//! [`TaskConfig`] that declares [`GrowSpec`] and the JSON artifact its
//! `from` task produced, render N concrete `TaskConfig`s — one per item —
//! with the template's `{{item.<field>}}` placeholders substituted. It
//! reads no files, emits no records, and knows nothing about phases; the
//! launcher owns all of that (`src/mission_launch.rs`).
//!
//! **Why this is not the retired `expand` primitive.** `expand`
//! (schema 1.1–1.4, removed in 2.0) fanned a template out over a
//! collection the LAUNCHER already held before the run started — which is
//! why both production launchers always passed an empty map and nothing
//! ever grew from it. `grow` fans out over an artifact the RUN produced:
//! the plan does not exist until a step writes it, so the fan-out is
//! structurally unknowable at launch time. Same shape on the page,
//! opposite lifetime.
//!
//! **Why growth happens at a PHASE BOUNDARY.** `scheduler::run_step_graph`
//! takes the task map by shared reference — the graph cannot grow while a
//! run is in flight. So the generic launcher runs the graph phase by phase
//! in config order and expands a phase's templates just before minting it,
//! when every earlier phase (and therefore every legal `from`) is done.

use super::{GrowSpec, TaskConfig};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Where one grown task/step came from. Stamped onto every grown step's
/// `config` under the `grown_from` key and mirrored into the run's
/// `graph-report.json`.
///
/// **Why `Step.config` rather than a typed field on `Task`/`Step`.**
/// `crew::types::Task`/`Step` have no `extras` overflow and no `Default`,
/// so a new field is an exhaustive-struct-literal change at ~104 call
/// sites across 17 files (48 `Task {`, 56 `Step {` — counted, not
/// estimated), almost all of them test fixtures. `Step.config` is already
/// the opaque per-step bag this codebase uses for kind-specific data, the
/// grown steps' configs are being CONSTRUCTED here anyway, and the
/// load-bearing track discriminator (`config.rule`, from the item) lands
/// there regardless. The task-side record is `graph-report.json`'s `grown`
/// section, which carries the same triple per copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrownFrom {
    /// The `grow.from` task id whose output produced this copy.
    pub task: String,
    /// The item's own `id` field, when it has one; otherwise the index.
    pub item: String,
    /// Position of the item in the `items` array.
    pub index: usize,
}

/// (#2300) One growth event, as provenance: appended to the run's
/// `graph-report.json` (`PruneReport::grown`) and mirrored into the
/// `mission.grow` flow record. `minted` carries the REAL task ids the
/// growth produced, so `minted.len()` is the count and the ids themselves
/// join a grown task back to the template it came from without reading
/// every task record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Grown {
    /// Real phase id the growth minted into.
    pub phase: String,
    /// Document id of the `grow` template task.
    pub task_template: String,
    /// The `grow.from` task id whose output was read.
    pub from: String,
    /// Path that task's last step output named.
    pub source_path: String,
    /// How many items the artifact's `items` array held.
    pub items: usize,
    /// The real task ids minted, in item order.
    #[serde(default)]
    pub minted: Vec<String>,
}

/// One template's whole expansion: the concrete task configs to mint and
/// the provenance triple for each, in item order.
#[derive(Debug, Clone, PartialEq)]
pub struct Growth {
    pub tasks: Vec<TaskConfig>,
    pub provenance: Vec<GrownFrom>,
}

/// Pull the `items` array out of an already-parsed artifact document.
///
/// The artifact is the JSON file a producing step's `output` PATH names.
/// A missing key, or a key holding something other than an array, is an
/// error naming both — a plan whose shape drifted must not quietly grow
/// zero tasks (that is the `expand` failure mode this whole feature is
/// built to not repeat). An array that is EMPTY is a legitimate outcome,
/// not an error: a rule that matched nothing planned nothing.
pub fn items_from_artifact<'a>(
    doc: &'a serde_json::Value,
    items_key: &str,
    source_path: &str,
) -> Result<&'a [serde_json::Value]> {
    // (#2301) A producer that wraps its output in
    // `darkmux_crew::step_output::Output` puts the real document under
    // `body`; one that predates the wrapper writes the body bare. Both
    // read — the transition needs no flag day — and a wrapped envelope is
    // recognized by carrying BOTH `kind` and `body`, never by `body`
    // alone (a body struct is free to have its own `body` field).
    let doc = match (doc.get("kind"), doc.get("body")) {
        (Some(_), Some(body)) => body,
        _ => doc,
    };
    match doc.get(items_key) {
        Some(serde_json::Value::Array(items)) => Ok(items.as_slice()),
        Some(other) => bail!(
            "grow: `{source_path}` has key `{items_key}`, but it holds {} rather than an array \
             of items",
            type_name(other)
        ),
        None => bail!(
            "grow: `{source_path}` has no top-level key `{items_key}` — a producing step's \
             output must be a path to a JSON document carrying the array to map over (top-level \
             keys present: {})",
            doc.as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>().join(", "))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "none — the document is not a JSON object".to_string())
        ),
    }
}

/// Render `template` into one concrete [`TaskConfig`] per item.
///
/// Each copy's task id is `<template id>-<rendered grow.id>`, and every
/// step id in the copy gets the same `-<rendered>` suffix, so the ids stay
/// readable and stay unique. `grow.config`'s keys are merged into EVERY
/// step's config in the copy (overwriting a same-named key the template
/// step declared — the grown value is the more specific one), and the
/// `grown_from` key is stamped alongside them. The copy carries no `grow`
/// of its own: growth is one level, never recursive.
/// (#2301) `from_output` is the PRODUCER's own output — the path its last
/// step named, i.e. the same artifact `items` were read out of. It renders
/// through the `{{from.output}}` placeholder, so a grown step can be handed
/// the plan it came from without the plan having to repeat its own path on
/// every item.
pub fn grow_task(
    template: &TaskConfig,
    spec: &GrowSpec,
    items: &[serde_json::Value],
    from_output: &str,
) -> Result<Growth> {
    let mut tasks = Vec::with_capacity(items.len());
    let mut provenance = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let suffix = render(&spec.id, item, from_output, &format!("{}.grow.id", template.id))?;
        if suffix.trim().is_empty() {
            bail!(
                "grow: task `{}` item {index} rendered an empty id suffix from `{}` — every copy \
                 needs a distinct id",
                template.id,
                spec.id
            );
        }
        let from = GrownFrom {
            task: spec.from.clone(),
            item: item
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| index.to_string()),
            index,
        };

        let mut copy = template.clone();
        copy.grow = None;
        copy.id = format!("{}-{suffix}", template.id);
        for step in &mut copy.steps {
            step.id = format!("{}-{suffix}", step.id);
            merge_grown_config(&mut step.config, &spec.config, item, from_output, template, &from)?;
        }
        tasks.push(copy);
        provenance.push(from);
    }
    Ok(Growth { tasks, provenance })
}

/// Merge the spec's rendered `config` templates plus `grown_from` into one
/// step's config. A step whose config is `null` (the `procedural.noop`
/// default) becomes an object here rather than losing the merge.
#[allow(clippy::too_many_arguments)]
fn merge_grown_config(
    step_config: &mut serde_json::Value,
    spec_config: &serde_json::Value,
    item: &serde_json::Value,
    from_output: &str,
    template: &TaskConfig,
    from: &GrownFrom,
) -> Result<()> {
    if step_config.is_null() {
        *step_config = serde_json::json!({});
    }
    let Some(obj) = step_config.as_object_mut() else {
        bail!(
            "grow: task `{}` has a step whose `config` is not an object, so the grown values \
             have nowhere to merge into",
            template.id
        );
    };
    if let Some(spec_obj) = spec_config.as_object() {
        for (key, value) in spec_obj {
            obj.insert(
                key.clone(),
                render_value(value, item, from_output, &format!("{}.grow.config.{key}", template.id))?,
            );
        }
    }
    obj.insert(
        "grown_from".to_string(),
        serde_json::to_value(from).expect("GrownFrom serializes"),
    );
    Ok(())
}

/// Substitute every `{{item.<field>}}` / `{{from.output}}` occurrence in a
/// string.
///
/// A whole-string placeholder (`"{{item.est_tokens}}"` and nothing else)
/// is NOT special-cased here — this returns a `String` because a task id
/// suffix is one. [`render_value`] is what preserves an item's number or
/// bool as a JSON number or bool.
fn render(template: &str, item: &serde_json::Value, from_output: &str, what: &str) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let close = after.find("}}").with_context(|| {
            format!("grow: {what} has an unclosed `{{{{` placeholder: `{template}`")
        })?;
        let key = after[..close].trim();
        match key.strip_prefix("item.") {
            Some(field) => out.push_str(&scalar(item, field, what)?),
            // (#2301) The producer's own output — one name, not a
            // namespace: a producing step has exactly one output, and it
            // is the path the items were read from.
            None if key == "from.output" => out.push_str(from_output),
            None => bail!(
                "grow: {what} names placeholder `{{{{{key}}}}}` — the placeholder namespaces are \
                 `item.<field>` and `from.output`"
            ),
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Render one `grow.config` value. A string that is EXACTLY one
/// placeholder keeps the item field's own JSON type (a number stays a
/// number); any other string is rendered by [`render`]; arrays and objects
/// recurse; other scalars pass through.
fn render_value(
    value: &serde_json::Value,
    item: &serde_json::Value,
    from_output: &str,
    what: &str,
) -> Result<serde_json::Value> {
    match value {
        serde_json::Value::String(s) => {
            if let Some(field) = whole_placeholder(s) {
                // Type-preserving: `"{{item.est_tokens}}"` yields the
                // number, not `"1200"`. A step kind reading a typed config
                // key must not have its type silently changed by the fact
                // that the value arrived through a template.
                scalar_value(item, field, what)
            } else {
                Ok(serde_json::Value::String(render(s, item, from_output, what)?))
            }
        }
        serde_json::Value::Array(items) => items
            .iter()
            .map(|v| render_value(v, item, from_output, what))
            .collect::<Result<Vec<_>>>()
            .map(serde_json::Value::Array),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), render_value(v, item, from_output, what)?);
            }
            Ok(serde_json::Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

/// `"{{item.x}}"` (and nothing else) -> `Some("x")`. `{{from.output}}` is
/// deliberately NOT a whole-placeholder: it is always a path string, so
/// there is no item type to preserve, and [`render`] handles it.
fn whole_placeholder(s: &str) -> Option<&str> {
    let inner = s.strip_prefix("{{")?.strip_suffix("}}")?;
    if inner.contains("{{") || inner.contains("}}") {
        return None;
    }
    inner.trim().strip_prefix("item.")
}

fn scalar_value(item: &serde_json::Value, field: &str, what: &str) -> Result<serde_json::Value> {
    match item.get(field) {
        Some(v) if v.is_string() || v.is_number() || v.is_boolean() => Ok(v.clone()),
        Some(other) => bail!(
            "grow: {what} names `item.{field}`, which holds {} — only top-level SCALAR fields \
             (string, number, bool) substitute; a nested value would stringify into a blob \
             nothing downstream can read",
            type_name(other)
        ),
        None => bail!(
            "grow: {what} names `item.{field}`, which the item does not have (fields present: {})",
            item.as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>().join(", "))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "none — the item is not a JSON object".to_string())
        ),
    }
}

fn scalar(item: &serde_json::Value, field: &str, what: &str) -> Result<String> {
    Ok(match scalar_value(item, field, what)? {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    })
}

fn type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a bool",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission_config::StepConfig;
    use serde_json::json;

    /// The producing step's output — the path `{{from.output}}` renders.
    const PLAN_PATH: &str = "/runs/crawl-1/plan/r-a.json";

    fn template() -> TaskConfig {
        TaskConfig {
            id: "unit".into(),
            enabled: None,
            description: None,
            display_name: None,
            depends_on: vec!["plan".into()],
            reads: Vec::new(),
            role_id: Some("crawler".into()),
            steps: vec![StepConfig {
                id: "unit-step".into(),
                kind: "procedural.noop".into(),
                enabled: None,
                config: json!({ "keep": "me" }),
                gate: None,
                extras: Default::default(),
            }],
            grow: None,
            extras: Default::default(),
        }
    }

    fn spec() -> GrowSpec {
        GrowSpec {
            from: "plan".into(),
            items: "units".into(),
            id: "{{item.id}}".into(),
            config: json!({ "unit": "{{item.id}}", "rule": "{{item.rule}}" }),
            extras: Default::default(),
        }
    }

    #[test]
    fn grows_one_copy_per_item_with_substituted_config() {
        let items = vec![
            json!({"id": "u-0001", "rule": "r-a"}),
            json!({"id": "u-0002", "rule": "r-b"}),
        ];
        let grown = grow_task(&template(), &spec(), &items, PLAN_PATH).unwrap();
        assert_eq!(grown.tasks.len(), 2);
        assert_eq!(grown.tasks[0].id, "unit-u-0001");
        assert_eq!(grown.tasks[1].id, "unit-u-0002");
        assert_eq!(grown.tasks[0].steps[0].id, "unit-step-u-0001");
        let cfg = &grown.tasks[0].steps[0].config;
        assert_eq!(cfg["unit"], json!("u-0001"));
        assert_eq!(cfg["rule"], json!("r-a"));
        assert_eq!(cfg["keep"], json!("me"), "the template's own config keys survive the merge");
        assert_eq!(cfg["grown_from"]["task"], json!("plan"));
        assert_eq!(cfg["grown_from"]["item"], json!("u-0001"));
        assert_eq!(cfg["grown_from"]["index"], json!(0));
        // Edges are inherited; copies never depend on each other.
        assert_eq!(grown.tasks[1].depends_on, vec!["plan".to_string()]);
        assert!(grown.tasks[1].grow.is_none(), "growth is one level, never recursive");
        assert_eq!(grown.provenance[1].index, 1);
    }

    #[test]
    fn a_number_field_keeps_its_json_type_in_config_and_stringifies_in_an_id() {
        let mut s = spec();
        s.id = "{{item.id}}-{{item.est_tokens}}".into();
        s.config = json!({ "est": "{{item.est_tokens}}", "mixed": "n={{item.est_tokens}}" });
        let items = vec![json!({"id": "u-1", "est_tokens": 1200})];
        let grown = grow_task(&template(), &s, &items, PLAN_PATH).unwrap();
        assert_eq!(grown.tasks[0].id, "unit-u-1-1200");
        let cfg = &grown.tasks[0].steps[0].config;
        assert_eq!(cfg["est"], json!(1200), "a whole-string placeholder keeps the number");
        assert_eq!(cfg["mixed"], json!("n=1200"), "an embedded placeholder renders into the string");
    }

    #[test]
    fn a_null_step_config_still_receives_the_grown_values() {
        let mut t = template();
        t.steps[0].config = json!(null);
        let items = vec![json!({"id": "u-1", "rule": "r"})];
        let grown = grow_task(&t, &spec(), &items, PLAN_PATH).unwrap();
        assert_eq!(grown.tasks[0].steps[0].config["unit"], json!("u-1"));
    }

    #[test]
    fn zero_items_grows_zero_tasks() {
        let grown = grow_task(&template(), &spec(), &[], PLAN_PATH).unwrap();
        assert!(grown.tasks.is_empty() && grown.provenance.is_empty());
    }

    #[test]
    fn a_missing_or_nested_item_field_is_an_error_naming_it() {
        let items = vec![json!({"id": "u-1"})];
        let err = grow_task(&template(), &spec(), &items, PLAN_PATH).unwrap_err().to_string();
        assert!(err.contains("item.rule"), "{err}");

        let items = vec![json!({"id": "u-1", "rule": {"nested": true}})];
        let err = grow_task(&template(), &spec(), &items, PLAN_PATH).unwrap_err().to_string();
        assert!(err.contains("an object"), "{err}");
    }

    #[test]
    fn items_from_artifact_names_the_file_and_the_key_when_the_shape_is_wrong() {
        let doc = json!({ "units": [1, 2], "totals": {} });
        assert_eq!(items_from_artifact(&doc, "units", "/p.json").unwrap().len(), 2);
        let err = items_from_artifact(&doc, "rules", "/p.json").unwrap_err().to_string();
        assert!(err.contains("/p.json") && err.contains("rules"), "{err}");
        let err = items_from_artifact(&doc, "totals", "/p.json").unwrap_err().to_string();
        assert!(err.contains("an object"), "{err}");
    }

    #[test]
    fn from_output_renders_the_producers_own_output_in_config_and_in_an_id() {
        // (#2301) The plan a unit came from, without every item having to
        // repeat the path.
        let mut spec = spec();
        spec.config = json!({ "plan": "{{from.output}}", "note": "plan={{from.output}} unit={{item.id}}" });
        let items = vec![json!({"id": "u-1", "rule": "r-a"})];
        let grown = grow_task(&template(), &spec, &items, PLAN_PATH).unwrap();
        let cfg = &grown.tasks[0].steps[0].config;
        assert_eq!(cfg["plan"], json!(PLAN_PATH));
        assert_eq!(cfg["note"], json!(format!("plan={PLAN_PATH} unit=u-1")));
    }

    #[test]
    fn an_unknown_placeholder_namespace_names_both_legal_ones() {
        let mut spec = spec();
        spec.config = json!({ "plan": "{{from.path}}" });
        let items = vec![json!({"id": "u-1", "rule": "r-a"})];
        let err = grow_task(&template(), &spec, &items, PLAN_PATH).unwrap_err().to_string();
        assert!(err.contains("from.path"), "{err}");
        assert!(err.contains("item.<field>") && err.contains("from.output"), "{err}");
    }
}
