//! What a `--json` verb prints: one serialized, named type.
//!
//! From this release the shape a verb prints under `--json` is a semver
//! contract, the way the daemon's HTTP responses are. Three things hold it:
//!
//! - **The type.** A verb prints through [`emit`] (or [`render`]), which takes
//!   only a [`CliOutput`]. A hand-built `serde_json::Value` is not one, so it
//!   cannot reach stdout by that road; `no_verb_serializes_json_outside_emit`
//!   scans the sources for the others: production code may not call
//!   `serde_json::to_string*` (or `to_writer*`, `to_vec*`, however imported)
//!   outside a short, counted, reasoned allowlist of non-output uses.
//! - **The table.** [`cli_outputs!`] is the one place a type becomes a
//!   `CliOutput`, and it names the verbs that print it.
//! - **The golden.** `tests/cli-json.golden` lists every verb and, for every
//!   type reachable from one, its fields and their types, derived from the
//!   types themselves. A changed shape fails `cli_json_matches_the_golden`
//!   until the golden is regenerated on purpose
//!   (`DARKMUX_REGENERATE_FIXTURES=1 cargo nextest run -p darkmux cli_json`),
//!   which makes the change a visible line in the diff.
//!
//! **Versioning.** The contract's version is darkmux's own: a shape change is
//! a semver-visible change to the binary and a CHANGELOG line. An output does
//! NOT carry an in-band `schema_version`, with one kind of exception: a
//! document that is also written to disk as an artifact (`RunStats`) outlives
//! the binary that wrote it, so it names the schema it was written under.
//!
//! **Vocabulary.** No field spells the internal noun `session` (contract 8 in
//! CLAUDE.md): a field names the run, the dispatch or the role execution it
//! belongs to. `no_output_type_names_a_session` checks the golden.
//!
//! Fields a producer could not read are `null` (or absent, when marked
//! `skip_serializing_if`), never a zero or an empty string.

use anyhow::Result;
use darkmux_crew::corrections::Correction;
use darkmux_crew::lessons::Lesson;
use darkmux_types::{LoadedModel, ProfileRegistry};
use schemars::JsonSchema;
use serde::Serialize;

/// A type a verb may print under `--json`. Implemented only by
/// [`cli_outputs!`], so a type that is not in the golden's table cannot be
/// printed.
pub(crate) trait CliOutput: Serialize + JsonSchema {}

/// The document as printed: pretty JSON and a trailing newline.
pub(crate) fn render<T: CliOutput + ?Sized>(value: &T) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(value)?))
}

/// Print one output to stdout.
pub(crate) fn emit<T: CliOutput + ?Sized>(value: &T) -> Result<()> {
    print!("{}", render(value)?);
    Ok(())
}

/// `memory lesson list --json` and `memory lesson recall --json`: the lessons
/// of each store tier.
#[derive(Serialize, JsonSchema)]
pub(crate) struct LessonTiers {
    pub repo: Vec<Lesson>,
    pub global: Vec<Lesson>,
}

/// `memory correction list --json`.
#[derive(Serialize, JsonSchema)]
pub(crate) struct CorrectionList {
    pub corrections: Vec<Correction>,
}

/// `profile list --json`: the registry and the path it was read from.
#[derive(Serialize, JsonSchema)]
pub(crate) struct ProfileList<'a> {
    pub registry_path: String,
    pub registry: &'a ProfileRegistry,
}

/// `machine status --json`, for this machine or a roster peer.
///
/// When `lms_unreachable` is true the residents are UNKNOWN, not zero: both
/// lists are empty and the exit code is 2. `matching_profiles` and `registry`
/// are present only for a local read, which is the only one that reads this
/// host's registry.
#[derive(Serialize, JsonSchema)]
pub(crate) struct MachineStatusOutput<'a> {
    pub machine_id: Option<String>,
    pub lms_unreachable: bool,
    pub managed: Vec<&'a LoadedModel>,
    pub user_state: Vec<&'a LoadedModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matching_profiles: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<&'a str>,
}

impl MachineStatusOutput<'_> {
    /// The answer when LMStudio could not be queried.
    pub(crate) fn lms_unreachable(machine_id: Option<String>) -> Self {
        MachineStatusOutput {
            machine_id,
            lms_unreachable: true,
            managed: Vec::new(),
            user_state: Vec::new(),
            matching_profiles: None,
            registry: None,
        }
    }
}

/// The verbs whose `--json` output is a stream of records rather than one
/// document, and what pins each record. The golden lists them beside the
/// documents so every `--json` verb is accounted for.
#[cfg(test)]
pub(crate) const STREAMS: &[(&str, &str)] = &[(
    "flow tail",
    "one flow record per line, verbatim; the record is pinned by FLOW_SCHEMA_VERSION, not by this file",
)];

/// The verbs that print JSON with no `--json` flag: the document is their only
/// output. The clap walk cannot see them, so this list is the second half of
/// the agreement: each is a row of [`cli_outputs!`] and a real verb without a
/// `--json` flag, and every row of the table names a verb that has the flag, is
/// listed here, or is a stream.
#[cfg(test)]
pub(crate) const ALWAYS_JSON: &[&str] = &["profile draft", "memory lesson export"];

/// Declare the output types and the verbs that print each. One row per type;
/// a verb that can print several types appears in each of their rows.
macro_rules! cli_outputs {
    ($($ty:ty => [$($verb:literal),+ $(,)?]),+ $(,)?) => {
        $(impl CliOutput for $ty {})+

        /// Every verb and the root type it prints, in the order declared.
        #[cfg(test)]
        pub(crate) fn table(generator: &mut schemars::SchemaGenerator) -> Vec<(&'static str, String)> {
            let mut rows = Vec::new();
            $(
                generator.subschema_for::<$ty>();
                let name = <$ty as JsonSchema>::schema_name().into_owned();
                $(rows.push(($verb, name.clone()));)+
            )+
            rows
        }
    };
}

cli_outputs! {
    darkmux_crew::dispatch_envelope::DispatchEnvelope => ["dispatch <role> (a container execution)"],
    darkmux_crew::dispatch_envelope::DirectDispatchEnvelope => ["dispatch <role> (a hosted-endpoint execution)"],
    darkmux_heuristics::ProfileDraft => ["profile draft"],
    LessonTiers => ["memory lesson list", "memory lesson recall"],
    darkmux_crew::lessons::LessonsExport => ["memory lesson export"],
    CorrectionList => ["memory correction list"],
    ProfileList<'_> => ["profile list"],
    MachineStatusOutput<'_> => ["machine status"],
    darkmux_profiles::model_ledger::ModelLedger => ["machine resources"],
    darkmux_serve::wire::MachineResourcesResponse => ["machine resources <peer>"],
    crate::fleet_cli::MachineListOutput => ["machine list"],
    crate::flow_cli::DrainOutput => ["flow drain"],
    crate::flow_cli::StrayDrainOutput => ["flow drain --file"],
    crate::flow_cli::IntegrityCheckOutput<'_> => ["flow integrity-check"],
    darkmux_flow::FlowStatus => ["flow status"],
    darkmux_lab::lab::loop_report::LoopReport => ["lab loop"],
    crate::lab_cli::lab_loop::LoopAbReport<'_> => ["lab loop --ab"],
    crate::finding_cli::FindingList<'_> => ["finding list"],
    darkmux_crew::findings::FindingRecord => ["finding show"],
    darkmux_crew::findings::SyncReport => ["finding sync"],
    crate::mod_cli::ModCreated<'_> => ["mod create"],
    crate::mod_cli::ModList<'_> => ["mod list"],
    darkmux_crew::mods::ModRecord => ["mod show"],
    crate::coder_phase::DebriefReport => ["mission debrief"],
    darkmux_lab::lab::stats::RunStats => ["run stats <one run>"],
    darkmux_lab::lab::stats_render::RunSetStats<'_> => ["run stats <several runs, or --baseline>"],
    crate::run_list::RunListOutput<'_> => ["run list"],
    crate::mission_show::MissionShow => ["mission show"],
    crate::mission_status::MissionBoard<'_> => ["mission status"],
    crate::mission_config_cli::ConfigList => ["mission config list"],
    crate::mission_config_cli::ConfigShow => ["mission config show"],
    crate::role_cli::RoleList => ["role list"],
    crate::role_cli::RoleShow => ["role show"],
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::generate::SchemaSettings;
    use serde_json::Value;
    use std::path::PathBuf;

    fn golden_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cli-json.golden")
    }

    /// A JSON-schema node as one TypeScript-like type expression.
    fn type_expr(v: &Value) -> String {
        if let Some(r) = v.get("$ref").and_then(Value::as_str) {
            return r.rsplit('/').next().unwrap_or(r).to_string();
        }
        for key in ["oneOf", "anyOf"] {
            if let Some(arms) = v.get(key).and_then(Value::as_array) {
                return arms.iter().map(type_expr).collect::<Vec<_>>().join(" | ");
            }
        }
        if let Some(arms) = v.get("allOf").and_then(Value::as_array) {
            return arms.iter().map(type_expr).collect::<Vec<_>>().join(" & ");
        }
        if let Some(c) = v.get("const") {
            return c.to_string();
        }
        if let Some(vals) = v.get("enum").and_then(Value::as_array) {
            return vals.iter().map(Value::to_string).collect::<Vec<_>>().join(" | ");
        }
        match v.get("type") {
            Some(Value::Array(types)) => types
                .iter()
                .filter_map(Value::as_str)
                .map(|t| typed(t, v))
                .collect::<Vec<_>>()
                .join(" | "),
            Some(Value::String(t)) => typed(t, v),
            _ => "any".to_string(),
        }
    }

    /// One `type` keyword, with the shape its siblings give it.
    fn typed(t: &str, v: &Value) -> String {
        match t {
            "array" => array_expr(v),
            "object" => object_expr(v),
            "integer" | "number" => numeric_expr(t, v),
            other => other.to_string(),
        }
    }

    /// A number by its width and sign: schemars' `format` (`uint64`, `int32`,
    /// `double`), and a `minimum` when the format does not already say the
    /// value is unsigned. A `u64` turning into an `i64` changes the golden.
    fn numeric_expr(t: &str, v: &Value) -> String {
        let base = v.get("format").and_then(Value::as_str).unwrap_or(t);
        match v.get("minimum") {
            Some(min) if !base.starts_with("uint") => format!("{base}>={min}"),
            _ => base.to_string(),
        }
    }

    /// An array: a tuple as its element types in order, a list as `T[]`.
    fn array_expr(v: &Value) -> String {
        if let Some(elems) = v.get("prefixItems").and_then(Value::as_array) {
            let types = elems.iter().map(type_expr).collect::<Vec<_>>();
            return format!("[{}]", types.join(", "));
        }
        match v.get("items") {
            Some(items) => format!("{}[]", wrap(&type_expr(items))),
            None => "any[]".to_string(),
        }
    }

    fn wrap(expr: &str) -> String {
        if expr.contains(' ') {
            format!("({expr})")
        } else {
            expr.to_string()
        }
    }

    /// An object node: its properties inline, a map, or a bare `object`.
    fn object_expr(v: &Value) -> String {
        if let Some(props) = v.get("properties").and_then(Value::as_object) {
            return format!("{{ {} }}", fields(v, props).join("; "));
        }
        match v.get("additionalProperties") {
            Some(Value::Bool(false)) | None => "object".to_string(),
            Some(Value::Bool(true)) => "{ [key: string]: any }".to_string(),
            Some(other) => format!("{{ [key: string]: {} }}", type_expr(other)),
        }
    }

    /// `name: type` (or `name?: type` when not required), in declaration order
    /// (schemars keeps it), so reordering a struct's fields changes the golden:
    /// a script that reads the output positionally would notice.
    fn fields(node: &Value, props: &serde_json::Map<String, Value>) -> Vec<String> {
        let required: Vec<&str> = node
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        props
            .keys()
            .map(|n| {
                let opt = if required.contains(&n.as_str()) { "" } else { "?" };
                format!("{n}{opt}: {}", type_expr(&props[n]))
            })
            .collect()
    }

    /// One definition: an object as a block of fields, anything else on a line.
    fn definition(name: &str, node: &Value) -> String {
        match node.get("properties").and_then(Value::as_object) {
            Some(props) if node.get("type").and_then(Value::as_str) == Some("object") => {
                let mut out = format!("{name}\n");
                for f in fields(node, props) {
                    out.push_str(&format!("  {f}\n"));
                }
                out
            }
            _ => format!("{name} = {}\n", type_expr(node)),
        }
    }

    /// The golden: the verb table, then every reachable type, sorted.
    pub(super) fn render_golden() -> String {
        let mut generator = SchemaSettings::draft2020_12().for_serialize().into_generator();
        let mut rows = table(&mut generator);
        rows.sort();
        let mut out = String::from("# verbs: what each prints under --json\n");
        for (verb, ty) in &rows {
            out.push_str(&format!("{verb}  {ty}\n"));
        }
        out.push_str("\n# streams: one record per line\n");
        for (verb, pinned_by) in STREAMS {
            out.push_str(&format!("{verb}  {pinned_by}\n"));
        }
        out.push_str("\n# types\n");
        let defs = Value::Object(generator.take_definitions(true));
        let mut names: Vec<&String> = defs.as_object().map(|m| m.keys().collect()).unwrap_or_default();
        names.sort();
        for name in &names {
            out.push_str(&definition(name, &defs[*name]));
        }
        out.push_str("\n# untyped: fields (or types) that hold free-form JSON, each explained in DESIGN.md\n");
        for line in untyped(&names, &defs) {
            out.push_str(&format!("{line}\n"));
        }
        out
    }

    /// Whether a type expression mentions `any`: a value with no declared shape.
    fn mentions_any(expr: &str) -> bool {
        expr.split(|c: char| !c.is_alphanumeric()).any(|word| word == "any")
    }

    /// `Type.field` for every field whose type mentions `any`, and `Type` for a
    /// definition that is not an object but does. The golden lists them so a
    /// new free-form field is a visible line, not a silent hole in the pin.
    fn untyped(names: &[&String], defs: &Value) -> Vec<String> {
        let mut found = Vec::new();
        for name in names {
            let node = &defs[name.as_str()];
            match node.get("properties").and_then(Value::as_object) {
                Some(props) if node.get("type").and_then(Value::as_str) == Some("object") => {
                    found.extend(
                        props.iter().filter(|(_, ty)| mentions_any(&type_expr(ty))).map(|(field, _)| format!("{name}.{field}")),
                    );
                }
                _ if mentions_any(&type_expr(node)) => found.push(name.to_string()),
                _ => {}
            }
        }
        found
    }

    /// A changed output shape fails here until the golden is regenerated on
    /// purpose: a visible line in the diff, and a CHANGELOG line.
    #[test]
    fn cli_json_matches_the_golden() {
        let rendered = render_golden();
        if std::env::var_os("DARKMUX_REGENERATE_FIXTURES").is_some() {
            std::fs::write(golden_path(), &rendered).expect("writing the cli-json golden");
            return;
        }
        let golden = std::fs::read_to_string(golden_path())
            .expect("tests/cli-json.golden is missing: regenerate with DARKMUX_REGENERATE_FIXTURES=1");
        assert_eq!(
            rendered, golden,
            "a `--json` output shape changed. Output shapes are semver contracts: if this is \
             intended, regenerate with `DARKMUX_REGENERATE_FIXTURES=1 cargo nextest run -p darkmux \
             cli_json` and add a CHANGELOG migration line."
        );
    }

    /// What the golden can tell apart: an integer's width and sign, a tuple's
    /// elements, and the order a struct declares its fields in. Each is a shape a
    /// script would notice, so each must change the rendered text.
    #[test]
    fn the_golden_tells_apart_widths_signs_tuples_and_order() {
        let ty = |json: &str| type_expr(&serde_json::from_str::<Value>(json).unwrap());
        assert_ne!(
            ty(r#"{"type":"integer","format":"uint64","minimum":0}"#),
            ty(r#"{"type":"integer","format":"int64"}"#),
            "u64 and i64 render alike"
        );
        assert_ne!(ty(r#"{"type":"integer","format":"uint32","minimum":0}"#), ty(r#"{"type":"integer","format":"uint64","minimum":0}"#));
        assert_ne!(ty(r#"{"type":"number","format":"float"}"#), ty(r#"{"type":"number","format":"double"}"#));
        assert_eq!(ty(r#"{"type":"integer"}"#), "integer");
        assert_eq!(ty(r#"{"type":"integer","minimum":1}"#), "integer>=1", "a sign with no format still shows");
        let tuple = r#"{"type":"array","prefixItems":[{"type":"string"},{"type":"integer","format":"uint64","minimum":0}],"minItems":2,"maxItems":2}"#;
        assert_eq!(ty(tuple), "[string, uint64]");
        assert_ne!(ty(tuple), ty(r#"{"type":"array","prefixItems":[{"type":"integer","format":"uint64","minimum":0},{"type":"string"}]}"#));
        let order = |json: &str| definition("T", &serde_json::from_str::<Value>(json).unwrap());
        assert_ne!(
            order(r#"{"type":"object","properties":{"a":{"type":"string"},"b":{"type":"string"}},"required":["a","b"]}"#),
            order(r#"{"type":"object","properties":{"b":{"type":"string"},"a":{"type":"string"}},"required":["a","b"]}"#),
            "a reordered struct renders alike"
        );
    }

    /// Every field the golden lists as untyped is explained in DESIGN.md's
    /// "CLI `--json` is a contract" section, by its `Type.field` name in code
    /// font, so a new free-form field cannot land without a stated reason.
    #[test]
    fn every_untyped_field_is_explained_in_design_md() {
        let design = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("DESIGN.md")).unwrap();
        let section = design.split("## CLI `--json` is a contract").nth(1).and_then(|rest| rest.split("\n## ").next()).unwrap();
        let golden = render_golden();
        let listed = golden.split("# untyped:").nth(1).unwrap().lines().skip(1).filter(|l| !l.is_empty());
        let missing: Vec<&str> = listed.filter(|name| !section.contains(&format!("`{name}`"))).collect();
        assert!(missing.is_empty(), "untyped fields with no reason in DESIGN.md: {missing:?}");
    }

    /// The names the golden holds (fields and type names) never spell the internal
    /// noun `session` (CLAUDE.md contract 8): a field names the run, the dispatch
    /// or the role execution it belongs to.
    #[test]
    fn no_output_type_names_a_session() {
        let offenders: Vec<String> = render_golden()
            .lines()
            .filter(|l| l.to_lowercase().contains("session"))
            .map(str::to_string)
            .collect();
        assert!(offenders.is_empty(), "an output names a session: {offenders:#?}");
    }

    /// Every `.rs` file under `dir`, recursively.
    fn rust_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// `text` with comments and string literals blanked (newlines kept), so a
    /// scan sees only code: a message that quotes `serde_json::to_string` is
    /// not a call.
    fn code_only(text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < chars.len() {
            let end = literal_end(&chars, i);
            if end == i {
                out.push(chars[i]);
                i += 1;
            } else {
                out.extend(chars[i..end].iter().map(|c| if *c == '\n' { '\n' } else { ' ' }));
                i = end;
            }
        }
        out
    }

    /// Where the comment or string literal starting at `i` ends; `i` itself
    /// when none starts there.
    fn literal_end(chars: &[char], i: usize) -> usize {
        let at = |k: usize| chars.get(k).copied();
        match (chars[i], at(i + 1)) {
            ('/', Some('/')) => (i..chars.len()).find(|&k| chars[k] == '\n').unwrap_or(chars.len()),
            ('/', Some('*')) => (i + 2..chars.len()).find(|&k| chars[k] == '*' && at(k + 1) == Some('/')).map_or(chars.len(), |k| k + 2),
            ('"', _) => string_end(chars, i + 1, 0),
            ('r', Some('"' | '#')) => {
                let hashes = chars[i + 1..].iter().take_while(|c| **c == '#').count();
                if at(i + 1 + hashes) == Some('"') { string_end(chars, i + 2 + hashes, hashes + 1) } else { i }
            }
            _ => i,
        }
    }

    /// The end of a string body starting at `from`: a plain string closes at an
    /// unescaped `"`, a raw string with `hashes` marks at `"` and that many `#`.
    fn string_end(chars: &[char], from: usize, raw_marks: usize) -> usize {
        let hashes = raw_marks.saturating_sub(1);
        let mut k = from;
        while k < chars.len() {
            if raw_marks == 0 && chars[k] == '\\' {
                k += 2;
                continue;
            }
            let closes = chars[k] == '"' && chars[k + 1..].iter().take(hashes).filter(|c| **c == '#').count() == hashes;
            if closes {
                return k + 1 + hashes;
            }
            k += 1;
        }
        chars.len()
    }

    /// The serializers that turn a value into JSON text or bytes.
    fn is_json_writer(name: &str) -> bool {
        ["to_string", "to_writer", "to_vec"].iter().any(|p| name.starts_with(p))
    }

    /// The identifier at the start of `code`.
    fn leading_ident(code: &str) -> &str {
        let len = code.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(code.len());
        &code[..len]
    }

    /// The text of each `use` statement in `code` (up to its `;`).
    fn use_statements(code: &str) -> Vec<&str> {
        code.split(';')
            .filter_map(|stmt| {
                stmt.match_indices("use ")
                    .find(|(at, _)| !stmt[..*at].ends_with(|c: char| c.is_alphanumeric() || c == '_'))
                    .map(|(at, _)| &stmt[at + "use ".len()..])
            })
            .collect()
    }

    /// Every way `code` reaches a `serde_json` serializer: a path
    /// `serde_json::to_string*` (whitespace and line breaks allowed), a `use`
    /// that imports one by name, by glob, or under an alias (so an unqualified
    /// or renamed call cannot slip past the path check).
    fn json_writer_calls(code: &str) -> Vec<String> {
        let mut found = Vec::new();
        for (at, _) in code.match_indices("serde_json") {
            let after = code[at + "serde_json".len()..].trim_start();
            let Some(rest) = after.strip_prefix("::") else { continue };
            let rest = rest.trim_start();
            let name = leading_ident(rest);
            if is_json_writer(name) {
                found.push(format!("serde_json::{name}"));
            }
        }
        for import in use_statements(code).into_iter().filter(|i| i.contains("serde_json")) {
            let words: Vec<&str> = import.split(|c: char| !(c.is_alphanumeric() || c == '_')).collect();
            if let Some(name) = words.iter().find(|w| is_json_writer(w)) {
                found.push(format!("use of {name}"));
            } else if import.contains("serde_json::*") || import.contains("serde_json as ") || import.contains("self as ") {
                found.push("a glob or aliased import of serde_json".to_string());
            }
        }
        found
    }

    /// `code` (already blanked by [`code_only`]) without its `#[cfg(test)]`
    /// items: each is blanked through its closing brace, or through its `;`
    /// when that comes first (`mod x;`, a `use`). Code before or after a test
    /// module stays, so a serializer added below one is still seen.
    fn without_test_items(code: &str) -> String {
        const MARK: &str = "#[cfg(test)]";
        let mut out = String::new();
        let mut rest = code;
        while let Some(at) = rest.find(MARK) {
            out.push_str(&rest[..at]);
            let item = &rest[at + MARK.len()..];
            let end = item_end(item);
            out.extend(rest[at..at + MARK.len() + end].chars().map(|c| if c == '\n' { '\n' } else { ' ' }));
            rest = &item[end..];
        }
        out.push_str(rest);
        out
    }

    /// The byte length of the item at the start of `item`: through the matching
    /// `}` of its first `{`, or through its `;` if that comes first.
    fn item_end(item: &str) -> usize {
        let Some(open) = item.find(['{', ';']) else { return item.len() };
        if item.as_bytes()[open] == b';' {
            return open + 1;
        }
        let mut depth = 0usize;
        for (i, c) in item[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return open + i + 1;
                    }
                }
                _ => {}
            }
        }
        item.len()
    }

    /// Production code that may serialize JSON outside [`emit`], with the count
    /// of call sites and why. None of these prints a document to stdout: each
    /// writes a file, feeds a fingerprint or a prompt, or quotes a free-form
    /// field inside a text line.
    const JSON_WRITER_ALLOWLIST: &[(&str, usize, &str)] = &[
        ("src/config_cmd.rs", 2, "writes config.json"),
        ("src/init.rs", 2, "writes config.json"),
        ("src/fleet_cli.rs", 2, "writes the roster's config file"),
        ("src/acp_panel.rs", 1, "writes the launch spec file"),
        ("src/mission_launch.rs", 2, "a spec fingerprint, and JSON quoted inside a prompt"),
        ("src/finding_cli.rs", 2, "`finding show` text mode: two free-form fields as lines of a text view"),
    ];

    /// The production call sites in every source file, as (relative path, calls).
    fn production_writer_calls() -> Vec<(String, Vec<String>)> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        rust_files(&root.join("src"), &mut files);
        files
            .iter()
            .filter(|f| !f.ends_with("cli_json.rs") && !f.file_name().is_some_and(|n| n.to_string_lossy().contains("tests")))
            .map(|f| {
                let text = std::fs::read_to_string(f).unwrap();
                let calls = json_writer_calls(&without_test_items(&code_only(&text)));
                (f.strip_prefix(&root).unwrap().display().to_string(), calls)
            })
            .filter(|(_, calls)| !calls.is_empty())
            .collect()
    }

    /// A verb prints a JSON document only through [`emit`]: production code
    /// under `src/` never calls `serde_json::to_string*` (or `to_writer*`,
    /// `to_vec*`, however imported), except the allowlisted non-output uses,
    /// counted per file so a new call in an allowed file fails too.
    #[test]
    fn no_verb_serializes_json_outside_emit() {
        let mut offenders = Vec::new();
        for (path, calls) in production_writer_calls() {
            let allowed = JSON_WRITER_ALLOWLIST.iter().find(|(p, _, _)| *p == path).map_or(0, |(_, n, _)| *n);
            if calls.len() != allowed {
                offenders.push(format!("{path}: {} call(s), {allowed} allowed: {calls:?}", calls.len()));
            }
        }
        for (path, _, _) in JSON_WRITER_ALLOWLIST {
            assert!(
                production_writer_calls().iter().any(|(p, _)| p == path),
                "allowlist row for {path} names a file with no serializer call: delete the row"
            );
        }
        assert!(offenders.is_empty(), "JSON serialized outside cli_json::emit: {offenders:#?}");
    }

    /// The scan can fail: each way a print could have bypassed the old textual
    /// pattern is seen, and the ways that are not calls are not.
    #[test]
    fn the_writer_scan_sees_every_bypass() {
        let bypasses = [
            ("named argument", "fn f() { println!(\"{s}\", s = serde_json::to_string(&x)?); }"),
            ("unqualified after use", "use serde_json::to_string_pretty;\nfn f() { println!(\"{}\", to_string_pretty(&x)?); }"),
            ("writeln to stdout", "fn f() { writeln!(out, \"{}\", serde_json::to_string(&x)?)?; }"),
            ("let then print", "use serde_json::to_string;\nfn f() { let s = to_string(&x)?; println!(\"{s}\"); }"),
            ("grouped import", "use serde_json::{Value, to_string_pretty};"),
            ("path over a line break", "fn f() { serde_json\n    ::to_string(&x); }"),
            ("to_writer", "fn f() { serde_json::to_writer(stdout(), &x)?; }"),
            ("to_vec_pretty", "fn f() { serde_json::to_vec_pretty(&x)?; }"),
            ("glob import", "use serde_json::*;"),
            ("aliased import", "use serde_json as sj;"),
        ];
        for (why, src) in bypasses {
            assert!(!json_writer_calls(&code_only(src)).is_empty(), "the scan misses: {why}");
        }
        let innocent = [
            ("a comment", "// serde_json::to_string(&x)\nfn f() {}"),
            ("a string", "fn f() { let m = \"call serde_json::to_string\"; }"),
            ("a raw string", "fn f() { let m = r#\"serde_json::to_string\"#; }"),
            ("a value import", "use serde_json::{json, Value};"),
            ("a Display to_string", "fn f() { let s = n.to_string(); }"),
        ];
        for (why, src) in innocent {
            assert!(json_writer_calls(&code_only(src)).is_empty(), "the scan flags {why}");
        }
        let tests = "fn f() {}\n#[cfg(test)]\npub(crate) mod tests {\n    fn t() { if a { serde_json::to_string(&x); } }\n}\n";
        let prod = |src: &str| json_writer_calls(&without_test_items(&code_only(src)));
        assert!(prod(tests).is_empty(), "test code is not production");
        let after = format!("{tests}fn g() {{ serde_json::to_string(&x); }}\n");
        assert_eq!(prod(&after).len(), 1, "a serializer added below the test module is production");
        assert!(prod("#[cfg(test)]\nmod tests_file;\nfn g() { serde_json::to_string(&x); }").len() == 1);
    }

    /// The table's verbs, without their qualifier: the leading words that name a
    /// subcommand path.
    fn table_verb_paths() -> Vec<String> {
        let mut generator = SchemaSettings::draft2020_12().into_generator();
        let root = <crate::cli::Cli as clap::CommandFactory>::command();
        table(&mut generator)
            .into_iter()
            .map(|(verb, _)| verb)
            .chain(STREAMS.iter().map(|(verb, _)| *verb))
            .map(|verb| {
                let mut cmd = &root;
                let mut path = Vec::new();
                for word in verb.split(' ') {
                    match cmd.find_subcommand(word) {
                        Some(next) => {
                            cmd = next;
                            path.push(word);
                        }
                        None => break,
                    }
                }
                path.join(" ")
            })
            .collect()
    }

    /// Every subcommand path that takes `--json`.
    fn json_flag_paths(cmd: &clap::Command, prefix: &str, out: &mut Vec<String>) {
        if cmd.get_arguments().any(|a| a.get_long() == Some("json")) {
            out.push(prefix.to_string());
        }
        for sub in cmd.get_subcommands() {
            let path = if prefix.is_empty() { sub.get_name().to_string() } else { format!("{prefix} {}", sub.get_name()) };
            json_flag_paths(sub, &path, out);
        }
    }

    /// Every verb with a `--json` flag is in the table (so it is in the golden),
    /// and every row of the table names a verb that exists.
    #[test]
    fn every_json_verb_is_in_the_table_and_every_row_is_a_verb() {
        let root = <crate::cli::Cli as clap::CommandFactory>::command();
        let mut flagged = Vec::new();
        json_flag_paths(&root, "", &mut flagged);
        let in_table = table_verb_paths();
        let missing: Vec<&String> = flagged.iter().filter(|p| !in_table.contains(p)).collect();
        assert!(missing.is_empty(), "verbs with --json and no output type in cli_outputs!: {missing:?}");
        let mut generator = SchemaSettings::draft2020_12().into_generator();
        let verbs = table(&mut generator).into_iter().map(|(verb, _)| verb).chain(STREAMS.iter().map(|(v, _)| *v));
        for (verb, path) in verbs.zip(&in_table) {
            assert!(!path.is_empty() && verb.starts_with(path.as_str()), "table row `{verb}` names no verb");
        }
    }

    /// The clap tree and the table agree in both directions for the verbs with
    /// no `--json` flag: each always-JSON verb is a real verb, has no flag (else
    /// it is not "always"), and is in the table; and every verb the table names
    /// either has the flag, is a stream, or is listed as always-JSON. A new
    /// always-JSON verb is therefore a table row that fails here until it is
    /// declared.
    #[test]
    fn always_json_verbs_and_the_clap_tree_agree() {
        let root = <crate::cli::Cli as clap::CommandFactory>::command();
        let mut flagged = Vec::new();
        json_flag_paths(&root, "", &mut flagged);
        let mut generator = SchemaSettings::draft2020_12().into_generator();
        let rows: Vec<&str> = table(&mut generator).into_iter().map(|(verb, _)| verb).collect();
        for verb in ALWAYS_JSON {
            assert!(
                rows.iter().any(|r| r == verb),
                "always-JSON verb `{verb}` is not a row of cli_outputs!"
            );
            assert!(!flagged.iter().any(|p| p == verb), "`{verb}` takes --json: it is not always-JSON");
            let mut cmd = &root;
            for word in verb.split(' ') {
                cmd = cmd.find_subcommand(word).unwrap_or_else(|| panic!("always-JSON verb `{verb}` is not a verb"));
            }
        }
        let streams: Vec<&str> = STREAMS.iter().map(|(v, _)| *v).collect();
        for (row, path) in rows.iter().zip(table_verb_paths()) {
            let covered = flagged.contains(&path) || ALWAYS_JSON.contains(&path.as_str()) || streams.contains(&path.as_str());
            assert!(covered, "table row `{row}` (verb `{path}`) has no --json flag and is not listed in ALWAYS_JSON");
        }
    }

    /// `machine status` answers in one shape: the unreachable answer and the
    /// ordinary one carry the same keys, so a reader checks `lms_unreachable`
    /// instead of guessing which document it got.
    #[test]
    fn machine_status_answers_with_the_same_keys_whether_or_not_lms_answered() {
        let keys = |v: &serde_json::Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };
        let unreachable = serde_json::to_value(MachineStatusOutput::lms_unreachable(Some("studio".into()))).unwrap();
        let answered = serde_json::to_value(MachineStatusOutput {
            machine_id: Some("studio".into()),
            lms_unreachable: false,
            managed: Vec::new(),
            user_state: Vec::new(),
            matching_profiles: None,
            registry: None,
        })
        .unwrap();
        assert_eq!(keys(&unreachable), keys(&answered));
        assert_eq!(unreachable["lms_unreachable"], true);
        assert_eq!(answered["lms_unreachable"], false);
    }
}
