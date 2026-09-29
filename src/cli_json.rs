//! What a `--json` verb prints: one serialized, named type.
//!
//! From this release the shape a verb prints under `--json` is a semver
//! contract, the way the daemon's HTTP responses are. Three things hold it:
//!
//! - **The type.** A verb prints through [`emit`] (or [`render`]), which takes
//!   only a [`CliOutput`]. A hand-built `serde_json::Value` is not one, so it
//!   cannot reach stdout by that road; `no_verb_prints_a_hand_built_value`
//!   scans the sources for the other road (`println!` of a `to_string` call).
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
            "array" => match v.get("items") {
                Some(items) => format!("{}[]", wrap(&type_expr(items))),
                None => "any[]".to_string(),
            },
            "object" => object_expr(v),
            other => other.to_string(),
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

    /// `name: type` (or `name?: type` when not required), sorted by name.
    fn fields(node: &Value, props: &serde_json::Map<String, Value>) -> Vec<String> {
        let required: Vec<&str> = node
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut names: Vec<&String> = props.keys().collect();
        names.sort();
        names
            .into_iter()
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
        for name in names {
            out.push_str(&definition(name, &defs[name]));
        }
        out
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

    /// The statements in `text` that print a `serde_json::to_string*` call as
    /// the whole line: the road by which a hand-built value could reach stdout
    /// without [`emit`].
    fn json_prints(text: &str) -> Vec<String> {
        let mut found = Vec::new();
        for start in text.match_indices("print").map(|(i, _)| i) {
            let rest = &text[start..];
            if !(rest.starts_with("println!(") || rest.starts_with("print!(")) {
                continue;
            }
            let stmt = rest.split(";\n").next().unwrap_or(rest);
            // A bare `"{}"` format is a document; `"context   {}"` is a text
            // line that quotes a value.
            let args = stmt.split_once('(').map_or("", |(_, a)| a.trim_start());
            if args.starts_with("\"{}\"") && stmt.contains("serde_json::to_string") {
                found.push(stmt.lines().next().unwrap_or("").trim().to_string());
            }
        }
        found
    }

    /// A verb prints a JSON document only through [`emit`]: no `println!` of a
    /// `serde_json::to_string*` call exists in the binary's sources.
    #[test]
    fn no_verb_prints_a_hand_built_value() {
        let mut files = Vec::new();
        rust_files(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        let mut offenders = Vec::new();
        for file in files.iter().filter(|f| !f.ends_with("cli_json.rs")) {
            for line in json_prints(&std::fs::read_to_string(file).unwrap()) {
                offenders.push(format!("{}: {line}", file.display()));
            }
        }
        assert!(offenders.is_empty(), "a verb prints JSON without cli_json::emit: {offenders:#?}");
    }

    /// The scan itself can fail: it sees a print of a `to_string_pretty` call,
    /// on one line or across several, and lets an `emit` through.
    #[test]
    fn the_print_scan_sees_what_it_is_meant_to_see() {
        let bad = "fn f() { println!(\"{}\", serde_json::to_string_pretty(&x)?); }\n";
        assert_eq!(json_prints(bad).len(), 1);
        let multiline = "fn f() {\n    println!(\n        \"{}\",\n        serde_json::to_string(&x)?\n    );\n}\n";
        assert_eq!(json_prints(multiline).len(), 1);
        assert!(json_prints("fn f() { cli_json::emit(&x)?; println!(\"{}\", y); }\n").is_empty());
        let text_line = "fn f() { println!(\"context   {}\", serde_json::to_string(&x)?); }\n";
        assert!(json_prints(text_line).is_empty(), "a text line that quotes a value is not a document");
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
