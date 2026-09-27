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
//! **A value of the wrong type is refused the same way** ([`Issue::WrongType`]):
//! one such value fails the whole typed load, which for `config.json` means
//! every setting falls back to its default, and for a user role, skill or
//! rule means the builtin of the same id silently stands in.
//!
//! **One mechanism, derived from the type.** Each file kind's valid keys and
//! value types are its Rust type's derived JSON schema
//! ([`schemars::JsonSchema`]), walked against the raw document by
//! [`key_issues`]. A new field is valid the
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

/// One key a document carries that its schema does not accept: a key the
/// schema does not know, or a value of the wrong type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyIssue {
    /// The key's dotted path from the document root (`redis.hots`,
    /// `hooks.rules[0].match.levle`).
    pub path: String,
    /// What is wrong, and what to write instead.
    pub issue: Issue,
}

/// What is wrong at one key, and what its message tells the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issue {
    /// A key the schema does not know: the closest valid key at that level
    /// (its full dotted path) and every valid key there. `closest` is `None`
    /// only for an object that has no named keys at all.
    Unknown { closest: Option<String>, valid: Vec<String> },
    /// A key darkmux retired: the operator line naming what replaced it.
    Retired(String),
    /// A known key whose value the schema does not accept. One such value
    /// fails the whole typed load (`config.json` falls back to every
    /// default), so it is refused like an unknown key.
    WrongType { expected: String, got: String },
}

impl fmt::Display for KeyIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.issue {
            Issue::Unknown { closest, valid } => {
                write!(f, "unknown key `{}`", self.path)?;
                if let Some(c) = closest {
                    write!(f, ": did you mean `{c}`?")?;
                }
                write!(f, " (valid keys here: {})", valid.join(", "))
            }
            Issue::Retired(line) => write!(f, "unknown key `{}`: {line}", self.path),
            Issue::WrongType { expected, got } => write!(f, "`{}` must be {expected}, got {got}", self.path),
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

/// Every key in `doc` that `T`'s schema does not accept, in key order: a key
/// it does not know (a retired one names its replacement instead of the
/// closest key), or a value of the wrong type.
pub fn key_issues<T: JsonSchema>(doc: &Value, retired: RetiredLookup<'_>) -> Vec<KeyIssue> {
    let schema = schemars::schema_for!(T);
    let root = schema.as_value();
    let mut walker = Walker { root, retired, out: Vec::new() };
    walker.walk(&[root], doc, "", "");
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

/// The keys an object schema names (each with every schema it may follow,
/// when several accepted shapes name it), and what it does with the rest.
struct ObjectShape<'a> {
    named: BTreeMap<&'a str, Vec<&'a Value>>,
    others: Others<'a>,
}

struct Walker<'a, 'r> {
    root: &'a Value,
    retired: RetiredLookup<'r>,
    out: Vec<KeyIssue>,
}

/// The `anyOf`/`oneOf`/`allOf` branches of a node.
fn branches(node: &Value) -> impl Iterator<Item = &Value> {
    ["anyOf", "oneOf", "allOf"].into_iter().flat_map(|c| node.get(c).and_then(Value::as_array).into_iter().flatten())
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
        for branch in branches(node) {
            if let Some(b) = self.object_shape(branch) {
                shape = Some(match shape {
                    None => b,
                    Some(s) => merge(s, b),
                });
            }
        }
        shape
    }

    /// The item schema of an array `node` accepts, across branches.
    fn items(&self, node: &'a Value) -> Option<&'a Value> {
        let node = self.resolve(node)?;
        node.get("items").or_else(|| branches(node).find_map(|b| self.items(b)))
    }

    /// Whether `node` accepts `value`'s type (and, for a number, its range;
    /// for an enum or a tag, its token). Keys inside an object are the
    /// walk's business, not this.
    fn accepts(&self, node: &'a Value, value: &Value) -> bool {
        let Some(node) = self.resolve(node) else { return true };
        let own = own_accepts(node, value);
        let has_any = node.get("anyOf").is_some() || node.get("oneOf").is_some();
        let any = !has_any
            || ["anyOf", "oneOf"]
                .iter()
                .flat_map(|c| node.get(*c).and_then(Value::as_array).into_iter().flatten())
                .any(|b| self.accepts(b, value));
        let all = node.get("allOf").and_then(Value::as_array).into_iter().flatten().all(|b| self.accepts(b, value));
        own && any && all
    }

    /// What `node` accepts, in operator words.
    fn describe(&self, node: &'a Value) -> Vec<String> {
        let Some(node) = self.resolve(node) else { return vec!["anything".to_string()] };
        let own = own_description(node);
        if !own.is_empty() {
            return own;
        }
        branches(node).flat_map(|b| self.describe(b)).collect()
    }

    /// Walk `doc` against the schemas `nodes` (several when merged shapes
    /// name one key). `path` is the display path, `bare` the retired-key
    /// lookup's form of it ([`RetiredLookup`]).
    fn walk(&mut self, nodes: &[&'a Value], doc: &Value, path: &str, bare: &str) {
        let accepting: Vec<&'a Value> = nodes.iter().copied().filter(|n| self.accepts(n, doc)).collect();
        if accepting.is_empty() {
            let mut expected: Vec<String> = nodes.iter().flat_map(|n| self.describe(n)).collect();
            expected.dedup();
            let issue = Issue::WrongType { expected: expected.join(" or "), got: shorten(&doc.to_string()) };
            self.out.push(KeyIssue { path: path.to_string(), issue });
            return;
        }
        match doc {
            Value::Object(map) => self.walk_object(&accepting, map, path, bare),
            Value::Array(items) => {
                let item_nodes: Vec<&'a Value> = accepting.iter().filter_map(|n| self.items(n)).collect();
                if item_nodes.is_empty() {
                    return;
                }
                for (i, value) in items.iter().enumerate() {
                    self.walk(&item_nodes, value, &format!("{path}[{i}]"), bare);
                }
            }
            _ => {}
        }
    }

    fn walk_object(&mut self, nodes: &[&'a Value], map: &serde_json::Map<String, Value>, path: &str, bare: &str) {
        let Some(shape) = nodes.iter().filter_map(|n| self.object_shape(n)).reduce(merge) else { return };
        for (key, value) in map.iter().filter(|(k, _)| k.as_str() != COMMENT_KEY) {
            let p = join(path, key);
            match (shape.named.get(key.as_str()), &shape.others) {
                (Some(subs), _) => self.walk(subs, value, &p, &join(bare, key)),
                (None, Others::Map(sub)) => self.walk(&[*sub], value, &p, &join(bare, "*")),
                (None, Others::Free) => {}
                (None, Others::Refused) => self.refuse(&shape, path, p, &join(bare, key), key),
            }
        }
    }

    fn refuse(&mut self, shape: &ObjectShape<'a>, parent: &str, path: String, bare: &str, key: &str) {
        let issue = match (self.retired)(bare) {
            Some(line) => Issue::Retired(line),
            None => Issue::Unknown {
                closest: closest(key, shape.named.keys().copied()).map(|c| join(parent, c)),
                valid: shape.named.keys().map(|k| k.to_string()).collect(),
            },
        };
        self.out.push(KeyIssue { path, issue });
    }
}

/// Whether a node's own `const`, `enum` and `type` constraints accept
/// `value` (a node with none of them accepts anything).
fn own_accepts(node: &Value, value: &Value) -> bool {
    if let Some(c) = node.get("const") {
        return c == value;
    }
    if let Some(tokens) = node.get("enum").and_then(Value::as_array) {
        return tokens.contains(value);
    }
    let Some(types) = node.get("type") else { return true };
    let one = |t: &Value| t.as_str().is_some_and(|t| type_accepts(t, node, value));
    match types {
        Value::Array(ts) => ts.iter().any(one),
        t => one(t),
    }
}

/// Whether one JSON-schema `type` name accepts `value`, including an
/// integer's `minimum`/`maximum`.
fn type_accepts(t: &str, node: &Value, value: &Value) -> bool {
    match t {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        "number" => value.is_number() && in_range(node, value),
        "integer" => (value.is_i64() || value.is_u64()) && in_range(node, value),
        _ => true,
    }
}

fn in_range(node: &Value, value: &Value) -> bool {
    let v = value.as_f64().unwrap_or(f64::NAN);
    let min = node.get("minimum").and_then(Value::as_f64).unwrap_or(f64::NEG_INFINITY);
    let max = node.get("maximum").and_then(Value::as_f64).unwrap_or(f64::INFINITY);
    v >= min && v <= max
}

/// A node's own constraints in operator words; empty when it has none (its
/// branches say it instead). `null` is left out beside another type: it
/// means "unset", which every optional key allows.
fn own_description(node: &Value) -> Vec<String> {
    if let Some(c) = node.get("const") {
        return vec![token(c)];
    }
    if let Some(tokens) = node.get("enum").and_then(Value::as_array) {
        return vec![format!("one of {}", tokens.iter().map(token).collect::<Vec<_>>().join(", "))];
    }
    let types: Vec<&str> = match node.get("type") {
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        Some(Value::String(t)) => vec![t.as_str()],
        _ => return Vec::new(),
    };
    let named: Vec<&str> = if types.len() > 1 { types.into_iter().filter(|t| *t != "null").collect() } else { types };
    named.into_iter().map(|t| type_words(t, node)).collect()
}

fn token(v: &Value) -> String {
    match v.as_str() {
        Some(s) => format!("`{s}`"),
        None => format!("`{v}`"),
    }
}

fn type_words(t: &str, node: &Value) -> String {
    let (min, max) = (node.get("minimum").and_then(Value::as_i64), node.get("maximum").and_then(Value::as_u64));
    match (t, min, max) {
        ("integer", Some(lo), Some(hi)) => format!("an integer from {lo} to {hi}"),
        ("integer", Some(0), None) => "a non-negative integer".to_string(),
        ("integer", _, _) => "an integer".to_string(),
        ("number", _, _) => "a number".to_string(),
        ("string", _, _) => "a string".to_string(),
        ("boolean", _, _) => "true or false".to_string(),
        ("array", _, _) => "a list".to_string(),
        ("object", _, _) => "an object".to_string(),
        (other, _, _) => other.to_string(),
    }
}

/// A value as JSON, cut to 60 characters so a pasted blob never floods the
/// message.
fn shorten(text: &str) -> String {
    match text.char_indices().nth(60) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
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
        named: props.into_iter().flatten().map(|(k, v)| (k.as_str(), vec![v])).collect(),
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
    branches(node).next().is_some()
}

/// Two shapes accepted at one place (two enum variants, say): a key either
/// names is valid, following either's schema; the more permissive treatment
/// of other keys wins.
fn merge<'a>(mut a: ObjectShape<'a>, b: ObjectShape<'a>) -> ObjectShape<'a> {
    for (k, vs) in b.named {
        let slot = a.named.entry(k).or_default();
        for v in vs {
            if !slot.iter().any(|x| std::ptr::eq(*x, v)) {
                slot.push(v);
            }
        }
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
    Keys(Vec<KeyIssue>),
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
            Problem::Keys(keys) => {
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
            let keys = key_issues::<T>(&doc, retired);
            if keys.is_empty() {
                return None;
            }
            Problem::Keys(keys)
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
