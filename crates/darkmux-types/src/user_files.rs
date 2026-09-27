//! (4.0) THE unknown-key gate for user files.
//!
//! **The rule.** A user file is a JSON document the operator writes and
//! darkmux reads at run time: `config.json`, `profiles.json`, role / skill /
//! crew manifests, mission configs, rule files, workloads, lab fixture
//! manifests, and a crawl's workspace spec ([`UserFileKind`] is the full
//! set). A key its schema does not know, a typo or a key a newer or older
//! darkmux spelled differently, is bad config. Loading stays lenient, so one
//! bad key never crashes a load or discards the rest of the file, and
//! `darkmux doctor` still runs against it. What refuses is consumption: every
//! entry point that consumes the file refuses at preflight, before minting
//! anything, and doctor reports it as Fail. Both name the file, the key's
//! dotted path, and the closest valid key.
//!
//! **One mechanism, derived from the type.** Each file kind's valid keys are
//! its Rust type's derived JSON schema ([`schemars::JsonSchema`]), walked
//! against the raw document by [`unknown_keys`]. A new field is valid the
//! moment it exists; there is no key list to keep in step. The schema honors
//! serde's own attributes (`rename`, `rename_all`, `flatten`, `tag`,
//! `untagged`), so the keys it knows are the keys serde routes to a field.
//! A forward-compat `extras` overflow is marked `#[schemars(skip)]`: it
//! catches unknown keys so a load survives them, it does not make them
//! valid. A flattened map that IS the schema (a hook rule's `match`, whose
//! extra keys are payload fields) keeps its `additionalProperties`, and
//! `open_objects_are_declared` pins that set. [`COMMENT_KEY`] (`_comment`)
//! is valid in every object, as a note for the reader.
//!
//! **One suggester.** [`closest`] is the only "did you mean" in darkmux: this
//! gate, `darkmux config set`, and `mission launch`'s undeclared-param
//! warning all call it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde_json::Value;

use crate::config_enum::Scope;

/// Every kind of user file darkmux reads. The owner of each kind's Rust
/// type runs the check (`darkmux-types` for config, `darkmux-profiles` for
/// the registry, `darkmux-crew` for roles through rules, `darkmux-lab` for
/// workloads and fixtures); this enum names the set and which entry points
/// consume each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UserFileKind {
    Config,
    Profiles,
    Role,
    Skill,
    Crew,
    MissionConfig,
    Rule,
    Workload,
    LabFixture,
    /// A crawl's `--input` workspace spec: not at a fixed location, so it is
    /// checked where the launch loads it rather than by a directory scan.
    WorkspaceSpec,
}

impl UserFileKind {
    pub const ALL: [UserFileKind; 10] = [
        UserFileKind::Config,
        UserFileKind::Profiles,
        UserFileKind::Role,
        UserFileKind::Skill,
        UserFileKind::Crew,
        UserFileKind::MissionConfig,
        UserFileKind::Rule,
        UserFileKind::Workload,
        UserFileKind::LabFixture,
        UserFileKind::WorkspaceSpec,
    ];

    /// How a message names the kind.
    pub fn label(self) -> &'static str {
        match self {
            UserFileKind::Config => "config",
            UserFileKind::Profiles => "profile registry",
            UserFileKind::Role => "role manifest",
            UserFileKind::Skill => "skill manifest",
            UserFileKind::Crew => "crew manifest",
            UserFileKind::MissionConfig => "mission config",
            UserFileKind::Rule => "rule file",
            UserFileKind::Workload => "workload manifest",
            UserFileKind::LabFixture => "lab fixture manifest",
            UserFileKind::WorkspaceSpec => "workspace spec",
        }
    }

    /// The entry points whose preflight checks this kind. A dispatch reads
    /// the config, the registry and the role library (skills route roles);
    /// a mission launch adds mission configs and rules; a lab run adds
    /// workloads and fixtures. Fleet submission reads only `config.json`.
    /// Crews and the workspace spec have no preflight scope: nothing that
    /// starts work reads the crew library, and the workspace spec is refused
    /// where the launch loads it.
    pub fn scopes(self) -> &'static [Scope] {
        const EVERY: &[Scope] = &[Scope::Dispatch, Scope::MissionLaunch, Scope::LabRun, Scope::FleetSubmission];
        const DISPATCHING: &[Scope] = &[Scope::Dispatch, Scope::MissionLaunch, Scope::LabRun];
        match self {
            UserFileKind::Config => EVERY,
            UserFileKind::Profiles | UserFileKind::Role | UserFileKind::Skill => DISPATCHING,
            UserFileKind::MissionConfig | UserFileKind::Rule => &[Scope::MissionLaunch],
            UserFileKind::Workload | UserFileKind::LabFixture => &[Scope::LabRun],
            UserFileKind::Crew | UserFileKind::WorkspaceSpec => &[],
        }
    }

    /// The kinds `scope`'s preflight checks.
    pub fn consumed_by(scope: Scope) -> impl Iterator<Item = UserFileKind> {
        UserFileKind::ALL.into_iter().filter(move |k| k.scopes().contains(&scope))
    }
}

/// The closest of `candidates` to `key` by edit distance, the first of
/// several equally close. `None` only when there are no candidates. THE
/// suggester: every "did you mean" in darkmux is this call.
pub fn closest<'a>(key: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    candidates.into_iter().fold(None, |best: Option<(usize, &'a str)>, c| {
        let d = edit_distance(key, c);
        match best {
            Some((bd, _)) if bd <= d => best,
            _ => Some((d, c)),
        }
    })
    .map(|(_, c)| c)
}

/// Levenshtein distance (two-row DP). Inline rather than a crate: a ten-line
/// need.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = Vec::with_capacity(b.len() + 1);
        cur.push(i + 1);
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// One key a document carries that its schema does not know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKey {
    /// The key's dotted path from the document root (`redis.hots`,
    /// `hooks.rules[0].match.levle`).
    pub path: String,
    /// What to write instead.
    pub hint: KeyHint,
}

/// What an [`UnknownKey`]'s message tells the operator to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyHint {
    /// The closest valid key at that level (its full dotted path) and every
    /// valid key there. `closest` is `None` only for an object that has no
    /// named keys at all.
    Closest { closest: Option<String>, valid: Vec<String> },
    /// A key darkmux retired: the operator line naming what replaced it.
    Retired(String),
}

impl fmt::Display for UnknownKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.hint {
            KeyHint::Closest { closest, valid } => {
                write!(f, "unknown key `{}`", self.path)?;
                if let Some(c) = closest {
                    write!(f, ": did you mean `{c}`?")?;
                }
                write!(f, " (valid keys here: {})", valid.join(", "))
            }
            KeyHint::Retired(line) => write!(f, "unknown key `{}`: {line}", self.path),
        }
    }
}

/// Looks up a retired key by its path, with array indices dropped and a
/// map's own keys written `*` (`hooks.rules.match.x`,
/// `profiles.*.models.role`), and returns the line naming what replaced it.
pub type RetiredLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The [`RetiredLookup`] of a file kind that has retired no keys.
pub fn no_retired(_: &str) -> Option<String> {
    None
}

/// Every key in `doc` that `T`'s schema does not know, in key order. A
/// retired key names its replacement instead of the closest key.
pub fn unknown_keys<T: JsonSchema>(doc: &Value, retired: RetiredLookup<'_>) -> Vec<UnknownKey> {
    let schema = schemars::schema_for!(T);
    let root = schema.as_value();
    let mut walker = Walker { root, retired, out: Vec::new() };
    walker.walk(root, doc, "", "");
    walker.out
}

/// The one key valid in every object of every user file: a note for the
/// file's reader, which darkmux never reads. The shipped roles, rules and
/// workloads carry one, and an operator's copy of them must not be refused
/// for it.
pub const COMMENT_KEY: &str = "_comment";

/// How an object schema treats a key it does not name.
enum Others<'a> {
    /// Unknown: reported.
    Refused,
    /// Any key is fine (a free-form map, or a flattened payload).
    Free,
    /// A map whose values follow this schema.
    Map(&'a Value),
}

/// The keys an object schema names, and what it does with the rest.
struct ObjectShape<'a> {
    named: BTreeMap<&'a str, &'a Value>,
    others: Others<'a>,
}

struct Walker<'a, 'r> {
    root: &'a Value,
    retired: RetiredLookup<'r>,
    out: Vec<UnknownKey>,
}

impl<'a> Walker<'a, '_> {
    /// `node` with every `$ref` followed, `None` for the `true` schema
    /// (anything goes).
    fn resolve(&self, mut node: &'a Value) -> Option<&'a Value> {
        for _ in 0..64 {
            match node {
                Value::Bool(_) => return None,
                Value::Object(m) => match m.get("$ref").and_then(Value::as_str) {
                    Some(r) => node = self.pointer(r)?,
                    None => return Some(node),
                },
                _ => return None,
            }
        }
        None
    }

    fn pointer(&self, r: &str) -> Option<&'a Value> {
        self.root.pointer(r.strip_prefix('#')?)
    }

    /// The object shape `node` accepts, merging `anyOf`/`oneOf`/`allOf`
    /// branches (an enum's variants, an `Option`'s null arm, a lenient
    /// value's catch-all). A bare `true` branch (a lenient value's "whatever
    /// was written") does not make the keys free: the keys a known shape
    /// names are still the only valid ones. `None` when no branch is an
    /// object.
    fn object_shape(&self, node: &'a Value) -> Option<ObjectShape<'a>> {
        let node = self.resolve(node)?;
        let mut shape: Option<ObjectShape<'a>> = own_shape(node);
        for combo in ["anyOf", "oneOf", "allOf"] {
            for branch in node.get(combo).and_then(Value::as_array).into_iter().flatten() {
                if let Some(b) = self.object_shape(branch) {
                    shape = Some(match shape {
                        None => b,
                        Some(s) => merge(s, b),
                    });
                }
            }
        }
        shape
    }

    /// The item schema of an array `node` accepts, across branches.
    fn items(&self, node: &'a Value) -> Option<&'a Value> {
        let node = self.resolve(node)?;
        if let Some(items) = node.get("items") {
            return Some(items);
        }
        ["anyOf", "oneOf", "allOf"]
            .iter()
            .flat_map(|c| node.get(*c).and_then(Value::as_array).into_iter().flatten())
            .find_map(|b| self.items(b))
    }

    /// Walk `doc` against `node`. `path` is the display path, `bare` the
    /// retired-key lookup's form of it ([`RetiredLookup`]).
    fn walk(&mut self, node: &'a Value, doc: &Value, path: &str, bare: &str) {
        match doc {
            Value::Object(map) => {
                let Some(shape) = self.object_shape(node) else { return };
                for (key, value) in map.iter().filter(|(k, _)| k.as_str() != COMMENT_KEY) {
                    let p = join(path, key);
                    match (shape.named.get(key.as_str()), &shape.others) {
                        (Some(sub), _) => self.walk(sub, value, &p, &join(bare, key)),
                        (None, Others::Map(sub)) => self.walk(sub, value, &p, &join(bare, "*")),
                        (None, Others::Free) => {}
                        (None, Others::Refused) => self.refuse(&shape, path, p, &join(bare, key), key),
                    }
                }
            }
            Value::Array(items) => {
                let Some(item) = self.items(node) else { return };
                for (i, value) in items.iter().enumerate() {
                    self.walk(item, value, &format!("{path}[{i}]"), bare);
                }
            }
            _ => {}
        }
    }

    fn refuse(&mut self, shape: &ObjectShape<'a>, parent: &str, path: String, bare: &str, key: &str) {
        let hint = match (self.retired)(bare) {
            Some(line) => KeyHint::Retired(line),
            None => KeyHint::Closest {
                closest: closest(key, shape.named.keys().copied()).map(|c| join(parent, c)),
                valid: shape.named.keys().map(|k| k.to_string()).collect(),
            },
        };
        self.out.push(UnknownKey { path, hint });
    }
}

/// The object shape a node declares directly (its `properties` and
/// `additionalProperties`), before any `anyOf`/`oneOf`/`allOf` merge.
fn own_shape(node: &Value) -> Option<ObjectShape<'_>> {
    let props = node.get("properties").and_then(Value::as_object);
    let additional = node.get("additionalProperties");
    if props.is_none() && additional.is_none() && !declares_object(node) {
        return None;
    }
    let others = match additional {
        // A struct's schema omits `additionalProperties`: its named keys are
        // the only valid ones.
        None | Some(Value::Bool(false)) => {
            if props.is_none() && !has_combinators(node) { Others::Free } else { Others::Refused }
        }
        Some(Value::Bool(true)) => Others::Free,
        Some(schema) => Others::Map(schema),
    };
    Some(ObjectShape {
        named: props.into_iter().flatten().map(|(k, v)| (k.as_str(), v)).collect(),
        others,
    })
}

/// `type: "object"` (or a list containing it) with nothing else said: a
/// free-form object, a `serde_json::Value` map field.
fn declares_object(node: &Value) -> bool {
    match node.get("type") {
        Some(Value::String(t)) => t == "object",
        Some(Value::Array(ts)) => ts.iter().any(|t| t == "object"),
        _ => false,
    }
}

fn has_combinators(node: &Value) -> bool {
    ["anyOf", "oneOf", "allOf"].iter().any(|c| node.get(*c).is_some())
}

/// Two shapes accepted at one place (two enum variants, say): a key either
/// names is valid; the more permissive treatment of other keys wins.
fn merge<'a>(mut a: ObjectShape<'a>, b: ObjectShape<'a>) -> ObjectShape<'a> {
    for (k, v) in b.named {
        a.named.entry(k).or_insert(v);
    }
    a.others = match (a.others, b.others) {
        (Others::Free, _) | (_, Others::Free) => Others::Free,
        (Others::Map(s), _) | (_, Others::Map(s)) => Others::Map(s),
        (Others::Refused, Others::Refused) => Others::Refused,
    };
    a
}

fn join(parent: &str, key: &str) -> String {
    if parent.is_empty() { key.to_string() } else { format!("{parent}.{key}") }
}

/// What is wrong with one user file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The file exists but could not be read.
    Unreadable(String),
    /// The file is not valid JSON (the parser's message).
    NotJson(String),
    /// Keys the file's schema does not know.
    UnknownKeys(Vec<UnknownKey>),
}

/// One user file that the gate refuses: where it is, what kind it is, and
/// what is wrong. Its `Display` is the whole operator message, the same text
/// at preflight and in `darkmux doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProblem {
    pub kind: UserFileKind,
    pub path: PathBuf,
    pub problem: Problem,
}

impl fmt::Display for FileProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: ", self.kind.label(), self.path.display())?;
        match &self.problem {
            Problem::Unreadable(e) => write!(f, "could not be read ({e})"),
            Problem::NotJson(e) => write!(f, "not valid JSON ({e})"),
            Problem::UnknownKeys(keys) => {
                let lines: Vec<String> = keys.iter().map(ToString::to_string).collect();
                write!(f, "{}", lines.join("; "))
            }
        }
    }
}

/// Check one document's text against `T`. `None` when it is clean.
pub fn check_text<T: JsonSchema>(
    kind: UserFileKind,
    path: &Path,
    text: &str,
    retired: RetiredLookup<'_>,
) -> Option<FileProblem> {
    let problem = match serde_json::from_str::<Value>(text) {
        Err(e) => Problem::NotJson(e.to_string()),
        Ok(doc) => {
            let keys = unknown_keys::<T>(&doc, retired);
            if keys.is_empty() {
                return None;
            }
            Problem::UnknownKeys(keys)
        }
    };
    Some(FileProblem { kind, path: path.to_path_buf(), problem })
}

/// Check the file at `path` against `T`. `None` when it is clean or absent.
pub fn check_path<T: JsonSchema>(kind: UserFileKind, path: &Path, retired: RetiredLookup<'_>) -> Option<FileProblem> {
    if is_operator_state(path) {
        return None;
    }
    match std::fs::read_to_string(path) {
        Ok(text) => check_text::<T>(kind, path, &text, retired),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(FileProblem { kind, path: path.to_path_buf(), problem: Problem::Unreadable(e.to_string()) }),
    }
}

/// A test build never reads the operator's own darkmux state (the same
/// isolation `config_access` gives `config.json`, #811): a test that does not
/// pin its root would otherwise pass or fail on whatever the developer's
/// `~/.darkmux` holds. Every read this gate makes goes through it, including
/// an index such as the lab's fixture registry. Always `false` in a release
/// build.
pub fn is_operator_state(path: &Path) -> bool {
    #[cfg(any(test, feature = "test-support"))]
    {
        let Some(home) = dirs::home_dir() else { return false };
        [home.join(".darkmux"), home.join(".config").join("darkmux")].iter().any(|root| path.starts_with(root))
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        let _ = path;
        false
    }
}

/// Check every `*.json` file directly in `dir` against `T`, in name order.
/// An absent directory has nothing to check.
pub fn check_dir<T: JsonSchema>(kind: UserFileKind, dir: &Path, retired: RetiredLookup<'_>) -> Vec<FileProblem> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    paths.iter().filter_map(|p| check_path::<T>(kind, p, retired)).collect()
}

/// Every named object in `T`'s schema that accepts keys it does not name.
/// Each file kind's tests pin this list, so a struct that forgets
/// `#[schemars(skip)]` on its `extras` overflow, and so silently accepts
/// every typo, fails a test instead.
pub fn open_objects<T: JsonSchema>() -> Vec<String> {
    let schema = schemars::schema_for!(T);
    let defs = schema.as_value().get("$defs").and_then(Value::as_object);
    defs.into_iter()
        .flatten()
        .filter(|(_, d)| d.get("properties").is_some() && d.get("additionalProperties") == Some(&Value::Bool(true)))
        .map(|(n, _)| n.clone())
        .collect()
}

/// `config.json`'s retired keys: a renamed setting names its new key, a
/// removed one says so and what to do.
fn config_retired(path: &str) -> Option<String> {
    crate::config::RENAMED_SETTINGS
        .iter()
        .find(|r| r.old_key == path)
        .map(|r| format!("renamed to `{}` in 4.0 (#2902); {}", r.new_key, r.advice))
        .or_else(|| {
            crate::config::REMOVED_SETTINGS
                .iter()
                .find(|r| r.key == path)
                .map(|r| format!("removed in {}; {}", r.removed_in, r.advice))
        })
}

/// The config document at `path` checked against [`crate::config::DarkmuxConfig`].
pub fn config_json_problem_at(path: &Path) -> Option<FileProblem> {
    check_path::<crate::config::DarkmuxConfig>(UserFileKind::Config, path, &config_retired)
}

/// The resolved `config.json` checked against its schema. A test build reads
/// the in-process test config instead of the operator's file, the same
/// isolation `config_access` gives every setting (#811).
pub fn config_json_problems() -> Vec<FileProblem> {
    #[cfg(any(test, feature = "test-support"))]
    {
        let path = PathBuf::from("config.json");
        let doc = serde_json::to_string(crate::config_access::config()).unwrap_or_default();
        check_text::<crate::config::DarkmuxConfig>(UserFileKind::Config, &path, &doc, &config_retired)
            .into_iter()
            .collect()
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        let path = crate::paths::resolve(crate::paths::ResolveScope::ForceUser).config;
        config_json_problem_at(&path).into_iter().collect()
    }
}

#[cfg(test)]
#[path = "user_files_tests.rs"]
mod tests;
