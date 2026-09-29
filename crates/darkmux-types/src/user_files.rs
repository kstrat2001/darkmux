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
//! is valid in every struct-shaped object, as a note for the reader.
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
    /// A crawl's workspace spec: not at a fixed location, so no directory
    /// scan finds it; the mission-launch preflight checks the one a launch
    /// input names (`"workspace": "{{<input>}}"` in a step), and
    /// `WorkspaceSpec::load` checks it again where a plan step reads it.
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
    /// workloads and fixtures. Fleet submission and `serve` read only `config.json`.
    /// Crews and the workspace spec have no scope here: nothing that starts
    /// work reads the crew library, and the workspace spec is checked by the
    /// mission launch itself, which knows which input names it.
    pub fn scopes(self) -> &'static [Scope] {
        const EVERY: &[Scope] = &Scope::ALL;
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
    /// A key or shape darkmux removed, that a schema no longer describes: the
    /// operator line naming the rewrite (a profile model's inline `endpoint`
    /// object).
    Removed(String),
    /// A known key whose value the schema does not accept. One such value
    /// fails the whole typed load (`config.json` falls back to every
    /// default), so it is refused like an unknown key.
    WrongType { expected: String, got: String },
    /// A key the schema requires is absent, which fails the typed load the
    /// same way.
    Missing { expected: String },
    /// A value the schema accepts but its consumer refuses: the rule it
    /// breaks, named with the step and key.
    Rule(String),
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
            Issue::Removed(line) | Issue::Rule(line) => write!(f, "`{}`: {line}", self.path),
            Issue::WrongType { expected, got } => write!(f, "`{}` must be {expected}, got {got}", self.path),
            Issue::Missing { expected } if expected.is_empty() => write!(f, "missing required key `{}`", self.path),
            Issue::Missing { expected } => write!(f, "missing required key `{}` ({expected})", self.path),
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
pub fn key_issues<T: JsonSchema + 'static>(doc: &Value, retired: RetiredLookup<'_>) -> Vec<KeyIssue> {
    key_issues_at::<T>(doc, retired, "")
}

/// [`key_issues`] for a document that sits at `prefix` inside a larger one
/// (a mission step's `config`): every issue's path is written from the
/// larger document's root. `retired` looks paths up from `doc`'s own root.
pub fn key_issues_at<T: JsonSchema + 'static>(doc: &Value, retired: RetiredLookup<'_>, prefix: &str) -> Vec<KeyIssue> {
    let schema = schema_of::<T>();
    let root = schema.as_ref();
    let mut walker = Walker { root, retired, out: Vec::new() };
    walker.walk(&[root], doc, prefix, "");
    walker.out
}

/// The keys `T`'s object schema names at its top level, sorted.
pub fn top_level_keys<T: JsonSchema + 'static>() -> Vec<String> {
    let schema = schema_of::<T>();
    let mut keys: Vec<String> = own_shape(&schema).map(|s| s.named.keys().map(|k| k.to_string()).collect()).unwrap_or_default();
    for branch in branches(&schema) {
        keys.extend(own_shape(branch).into_iter().flat_map(|s| s.named.into_keys().map(str::to_string)));
    }
    keys.sort();
    keys.dedup();
    keys
}

/// Every token an enum's schema allows: its `enum` list, or the `const` of
/// each `oneOf`/`anyOf` branch (the form schemars uses when the variants are
/// documented).
pub fn enum_tokens(schema: &Value) -> Vec<String> {
    let listed = schema.get("enum").and_then(Value::as_array).into_iter().flatten();
    let consts = branches(schema).filter_map(|b| b.get("const").or_else(|| b.get("enum").and_then(|e| e.get(0))));
    listed.chain(consts).filter_map(Value::as_str).map(str::to_string).collect()
}

/// `T`'s JSON schema, generated once per type: every dispatch runs the
/// preflight, so the schema is not rebuilt on each one.
fn schema_of<T: JsonSchema + 'static>() -> std::sync::Arc<Value> {
    use std::any::TypeId;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<TypeId, Arc<Value>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut map = cache.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(TypeId::of::<T>()).or_insert_with(|| Arc::new(schemars::schema_for!(T).to_value())).clone()
}

/// The note key: valid in every object whose keys the schema NAMES (a
/// struct), for the file's reader; darkmux never reads it. The shipped
/// roles, rules and workloads carry one, and an operator's copy of them must
/// not be refused for it. Inside a map (`fleet.accept_work`, a hook's
/// `headers`) it is an entry like any other and is checked as one: serde
/// reads it as an entry, so exempting it there would pass a file the load
/// then rejects.
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
/// when several accepted shapes name it), the keys it requires, and what it
/// does with the rest.
struct ObjectShape<'a> {
    named: BTreeMap<&'a str, Vec<&'a Value>>,
    required: Vec<&'a str>,
    others: Others<'a>,
    /// The schema a map's own keys must fit (`propertyNames`: an enum-keyed
    /// map such as a capability vector), when it has one.
    key_names: Option<&'a Value>,
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

/// How strictly [`Walker::accepts`] judges a value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Depth {
    /// The value's own type only (the node's `type`, `const`, `enum`).
    Shallow,
    /// Every nested value's type too, but not required keys.
    Typed,
    /// Every nested value's type and every required key: what serde needs.
    Full,
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

    /// Of `nodes`, the ones that best fit `value`: those that accept it
    /// fully, else those whose nested types fit (a required key missing),
    /// else those whose own type fits (something nested is wrong). This is
    /// how a tagged enum's variant, or an untagged value's shape, is chosen,
    /// so each message is about the variant the file meant.
    fn select(&self, nodes: &[&'a Value], value: &Value) -> Vec<&'a Value> {
        for depth in [Depth::Full, Depth::Typed, Depth::Shallow] {
            let fit: Vec<&'a Value> = nodes.iter().copied().filter(|n| self.accepts(n, value, depth)).collect();
            if !fit.is_empty() {
                return fit;
            }
        }
        Vec::new()
    }

    /// Whether `node` accepts `value` at `depth`.
    fn accepts(&self, node: &'a Value, value: &Value, depth: Depth) -> bool {
        let Some(node) = self.resolve(node) else { return true };
        if !own_accepts(node, value) {
            return false;
        }
        let one_of = ["anyOf", "oneOf"].iter().flat_map(|c| node.get(*c).and_then(Value::as_array)).flatten();
        let mut alternatives = one_of.peekable();
        if alternatives.peek().is_some() && !alternatives.any(|b| self.accepts(b, value, depth)) {
            return false;
        }
        let all_of = node.get("allOf").and_then(Value::as_array).into_iter().flatten();
        if !all_of.clone().all(|b| self.accepts(b, value, depth)) {
            return false;
        }
        depth == Depth::Shallow || self.children_accepted(node, value, depth)
    }

    /// Whether the values inside `value` (an object's fields, a map's
    /// values, a list's items) fit `node`, and, at [`Depth::Full`], its
    /// required keys are present.
    fn children_accepted(&self, node: &'a Value, value: &Value, depth: Depth) -> bool {
        match value {
            Value::Object(map) => {
                let props = node.get("properties").and_then(Value::as_object);
                let others = node.get("additionalProperties").filter(|a| a.is_object());
                let required = node.get("required").and_then(Value::as_array).into_iter().flatten();
                let present = required.filter_map(Value::as_str).all(|r| map.contains_key(r));
                let names = node.get("propertyNames");
                let keys_fit = names.is_none_or(|n| map.keys().all(|k| self.accepts(n, &Value::String(k.clone()), depth)));
                (depth != Depth::Full || present)
                    && keys_fit
                    && map.iter().all(|(k, v)| match (props.and_then(|p| p.get(k)), others) {
                        (Some(sub), _) | (None, Some(sub)) => self.accepts(sub, v, depth),
                        (None, None) => true,
                    })
            }
            Value::Array(items) => {
                items.iter().enumerate().all(|(i, v)| self.item_schema(node, i).is_none_or(|n| self.accepts(n, v, depth)))
            }
            _ => true,
        }
    }

    /// The object shape the best-fitting of `node` and its branches give
    /// `value` (see [`Self::select`]). A bare `true` branch (a lenient
    /// value's "whatever was written") does not make the keys free: the
    /// keys a known shape names are still the only valid ones.
    fn object_shape(&self, node: &'a Value, value: &Value) -> Option<ObjectShape<'a>> {
        let node = self.resolve(node)?;
        let chosen = self.select(&branches(node).collect::<Vec<_>>(), value);
        chosen.iter().filter_map(|b| self.object_shape(b, value)).fold(own_shape(node), |acc, b| match acc {
            None => Some(b),
            Some(a) => Some(merge(a, b)),
        })
    }

    /// The schema of item `i` of an array `node` accepts, across branches:
    /// a tuple's `prefixItems[i]`, else the list's `items`.
    fn items(&self, node: &'a Value, i: usize) -> Option<&'a Value> {
        let node = self.resolve(node)?;
        self.item_schema(node, i).or_else(|| branches(node).find_map(|b| self.items(b, i)))
    }

    /// Item `i`'s schema on `node` itself (no branches).
    fn item_schema(&self, node: &'a Value, i: usize) -> Option<&'a Value> {
        let tuple = node.get("prefixItems").and_then(Value::as_array);
        tuple.and_then(|t| t.get(i)).or_else(|| node.get("items"))
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
        let chosen = self.select(nodes, doc);
        if chosen.is_empty() {
            let mut expected: Vec<String> = nodes.iter().flat_map(|n| self.describe(n)).collect();
            expected.dedup();
            let issue = Issue::WrongType { expected: expected.join(" or "), got: shorten(&escape_text(&doc.to_string())) };
            self.out.push(KeyIssue { path: path.to_string(), issue });
            return;
        }
        match doc {
            Value::Object(map) => self.walk_object(&chosen, map, path, bare),
            Value::Array(items) => {
                for (i, value) in items.iter().enumerate() {
                    let item_nodes: Vec<&'a Value> = chosen.iter().filter_map(|n| self.items(n, i)).collect();
                    if !item_nodes.is_empty() {
                        self.walk(&item_nodes, value, &format!("{path}[{i}]"), bare);
                    }
                }
            }
            _ => {}
        }
    }

    fn walk_object(&mut self, nodes: &[&'a Value], map: &serde_json::Map<String, Value>, path: &str, bare: &str) {
        let doc = Value::Object(map.clone());
        let Some(shape) = nodes.iter().filter_map(|n| self.object_shape(n, &doc)).reduce(merge) else { return };
        for r in shape.required.iter().filter(|r| !map.contains_key(**r)) {
            let expected = shape.named.get(r).map(|subs| subs.iter().flat_map(|n| self.describe(n)).collect::<Vec<_>>());
            let issue = Issue::Missing { expected: expected.unwrap_or_default().join(" or ") };
            self.out.push(KeyIssue { path: join_display(path, r), issue });
        }
        for (key, value) in map {
            let p = join_display(path, key);
            match (shape.named.get(key.as_str()), &shape.others) {
                (Some(subs), _) => self.walk(subs, value, &p, &join(bare, key)),
                (None, Others::Map(_)) if !self.key_fits(&shape, key) => self.refuse_map_key(&shape, path, p, key),
                (None, Others::Map(sub)) => self.walk(&[*sub], value, &p, &join(bare, "*")),
                (None, Others::Free) => {}
                // A note, only where the schema names its keys: in a map it
                // is an entry, walked above like any other.
                (None, Others::Refused) if key == COMMENT_KEY => {}
                (None, Others::Refused) => self.refuse(&shape, path, p, &join(bare, key), key),
            }
        }
    }

    /// Whether a map key fits the map's `propertyNames` (always, without).
    fn key_fits(&self, shape: &ObjectShape<'a>, key: &str) -> bool {
        shape.key_names.is_none_or(|names| self.accepts(names, &Value::String(key.to_string()), Depth::Full))
    }

    /// A map key its `propertyNames` does not allow: named with the closest
    /// allowed key.
    fn refuse_map_key(&mut self, shape: &ObjectShape<'a>, parent: &str, path: String, key: &str) {
        let tokens: Vec<&str> = shape
            .key_names
            .and_then(|n| self.resolve(n))
            .and_then(|n| n.get("enum"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let issue = Issue::Unknown {
            closest: closest(key, tokens.iter().copied()).map(|c| join_display(parent, c)),
            valid: tokens.iter().map(|t| t.to_string()).collect(),
        };
        self.out.push(KeyIssue { path, issue });
    }

    fn refuse(&mut self, shape: &ObjectShape<'a>, parent: &str, path: String, bare: &str, key: &str) {
        let issue = match (self.retired)(bare) {
            Some(line) => Issue::Retired(line),
            None => Issue::Unknown {
                closest: closest(key, shape.named.keys().copied()).map(|c| join_display(parent, c)),
                valid: shape.named.keys().map(|k| k.to_string()).collect(),
            },
        };
        self.out.push(KeyIssue { path, issue });
    }
}

/// Whether a node's own `const`, `enum` and `type` constraints accept
/// `value` (a node with none of them accepts anything).
fn own_accepts(node: &Value, value: &Value) -> bool {
    if let Some(len) = value.as_array().map(Vec::len) {
        let min = node.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
        let max = node.get("maxItems").and_then(Value::as_u64).map_or(usize::MAX, |m| m as usize);
        if len < min || len > max {
            return false;
        }
    }
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
        "string" => value.as_str().is_some_and(|text| string_accepts(node, text)),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        "number" => value.is_number() && in_range(node, value),
        "integer" => (value.is_i64() || value.is_u64()) && in_range(node, value),
        _ => true,
    }
}

/// Whether a string fits a `string` node: any string, unless the node names
/// one of the step-config text forms ([`crate::param_scalar`]).
fn string_accepts(node: &Value, text: &str) -> bool {
    let format = node.get("format").and_then(Value::as_str);
    format.and_then(|f| crate::param_scalar::text_form_ok(f, text)).unwrap_or(true)
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
        ("string", _, _) => match node.get("format").and_then(Value::as_str) {
            Some(crate::param_scalar::COUNT_FORMAT) => "the text of a non-negative integer".to_string(),
            Some(crate::param_scalar::BLANKABLE_COUNT_FORMAT) => "the text of a non-negative integer, or blank".to_string(),
            Some(crate::param_scalar::FLAG_FORMAT) => "the text `true` or `false`".to_string(),
            Some(crate::param_scalar::SESSION_ID_FORMAT) => "a session id in its wire form".to_string(),
            _ => "a string".to_string(),
        },
        ("boolean", _, _) => "true or false".to_string(),
        ("array", _, _) => match (node.get("minItems").and_then(Value::as_u64), node.get("maxItems").and_then(Value::as_u64)) {
            (Some(lo), Some(hi)) if lo == hi => format!("a list of {lo}"),
            _ => "a list".to_string(),
        },
        ("object", _, _) => "an object".to_string(),
        (other, _, _) => other.to_string(),
    }
}

/// A value as JSON, cut to 60 characters so a pasted blob never floods the
/// message.
fn shorten(text: &str) -> String {
    shorten_to(text, 60)
}

fn shorten_to(text: &str, cap: usize) -> String {
    match text.char_indices().nth(cap) {
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
        // A struct that denies unknown fields names its keys as the only
        // valid ones, even when it names none.
        Some(Value::Bool(false)) => Others::Refused,
        // A struct's schema omits `additionalProperties`: its named keys are
        // the only valid ones.
        None => {
            if props.is_none() && !has_combinators(node) { Others::Free } else { Others::Refused }
        }
        Some(Value::Bool(true)) => Others::Free,
        Some(schema) => Others::Map(schema),
    };
    Some(ObjectShape {
        named: props.into_iter().flatten().map(|(k, v)| (k.as_str(), vec![v])).collect(),
        required: node.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect(),
        others,
        key_names: node.get("propertyNames"),
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
/// names is valid, following either's schema; a key is required only if
/// both require it; the more permissive treatment of other keys wins.
fn merge<'a>(mut a: ObjectShape<'a>, b: ObjectShape<'a>) -> ObjectShape<'a> {
    a.required.retain(|r| b.required.contains(r));
    for (k, vs) in b.named {
        let slot = a.named.entry(k).or_default();
        for v in vs {
            if !slot.iter().any(|x| std::ptr::eq(*x, v)) {
                slot.push(v);
            }
        }
    }
    a.key_names = a.key_names.or(b.key_names);
    a.others = match (a.others, b.others) {
        (Others::Free, _) | (_, Others::Free) => Others::Free,
        (Others::Map(s), _) | (_, Others::Map(s)) => Others::Map(s),
        (Others::Refused, Others::Refused) => Others::Refused,
    };
    a
}

/// `parent.key` for a message: the key rendered by [`segment`].
fn join_display(parent: &str, key: &str) -> String {
    join(parent, &segment(key))
}

/// One key as a message shows it. A plain key (letters, digits, `_`, `-`)
/// prints as is; any other is JSON-quoted with every control and invisible
/// format character escaped, and capped at 48 characters, so a key can
/// never break a line, indent a fake one, reorder text, or push the message
/// off the screen.
fn segment(key: &str) -> String {
    const CAP: usize = 48;
    if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') && key.len() <= CAP {
        return key.to_string();
    }
    let escaped = escape_text(&key.replace('\\', "\\\\").replace('"', "\\\""));
    format!("\"{}\"", shorten_to(&escaped, CAP))
}

/// `text` with every control character and every invisible format
/// character (bidi overrides and isolates, zero-width marks, line and
/// paragraph separators) written as a `\u{..}` escape, so a terminal shows
/// it instead of acting on it.
pub fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || is_invisible_format(c) => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Unicode format characters a terminal acts on without showing them.
fn is_invisible_format(c: char) -> bool {
    matches!(c as u32, 0xAD | 0x61C | 0x180E | 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB)
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
    /// Keys the file's schema does not accept.
    Keys(Vec<KeyIssue>),
}

/// Which files of a kind a check reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Every file on disk, for `darkmux doctor`: a file nothing loads is
    /// still reported, with a [`FileProblem::note`] saying why it refuses
    /// nothing.
    Every,
    /// Only the files an operation would load, for a preflight: the
    /// effective copy of each id, never one another tier shadows.
    Effective,
}

/// One user file that the gate refuses: where it is, what kind it is, and
/// what is wrong. Its `Display` is the whole operator message, the same text
/// at preflight and in `darkmux doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProblem {
    pub kind: UserFileKind,
    pub path: PathBuf,
    pub problem: Problem,
    /// Why this file refuses nothing, when it refuses nothing (it is
    /// shadowed, or only a run that binds it reads it). Doctor only.
    pub note: Option<String>,
}

impl fmt::Display for FileProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: ", self.kind.label(), escape_text(&self.path.display().to_string()))?;
        match &self.problem {
            Problem::Unreadable(e) => write!(f, "could not be read ({e})"),
            Problem::NotJson(e) => write!(f, "not valid JSON ({e})"),
            Problem::Keys(keys) => {
                let lines: Vec<String> = keys.iter().map(ToString::to_string).collect();
                write!(f, "{}", lines.join("; "))
            }
        }?;
        match &self.note {
            Some(note) => write!(f, " ({note})"),
            None => Ok(()),
        }
    }
}

/// Check tiered documents: `docs` is every `(id, path)` in precedence order
/// (the first copy of an id is the one that loads). [`Reach::Effective`]
/// checks only those; [`Reach::Every`] checks all, noting on a shadowed copy
/// which file shadows it.
pub fn check_tiered<T: JsonSchema + 'static>(
    kind: UserFileKind,
    docs: &[(String, PathBuf)],
    retired: RetiredLookup<'_>,
    extra: ExtraIssues<'_>,
    reach: Reach,
) -> Vec<FileProblem> {
    let mut first: BTreeMap<&str, &Path> = BTreeMap::new();
    let mut out = Vec::new();
    for (id, path) in docs {
        let shadowed_by = match first.get(id.as_str()) {
            Some(winner) => Some(*winner),
            None => {
                first.insert(id, path);
                None
            }
        };
        if shadowed_by.is_some() && reach == Reach::Effective {
            continue;
        }
        if let Some(mut found) = check_path_and::<T>(kind, path, retired, extra) {
            found.note = shadowed_by.map(|w| {
                format!("shadowed by {}: never loaded, so nothing refuses to start over it", escape_text(&w.display().to_string()))
            });
            out.push(found);
        }
    }
    out
}

/// Check one document's text against `T`. `None` when it is clean.
pub fn check_text<T: JsonSchema + 'static>(
    kind: UserFileKind,
    path: &Path,
    text: &str,
    retired: RetiredLookup<'_>,
) -> Option<FileProblem> {
    check_text_and::<T>(kind, path, text, retired, &|_| Vec::new())
}

/// Issues a document carries that its schema cannot express, found by reading
/// the document itself ([`check_text_and`]).
pub type ExtraIssues<'a> = &'a dyn Fn(&Value) -> Vec<KeyIssue>;

/// [`check_text`] plus `extra`: issues about a shape the schema cannot say.
pub fn check_text_and<T: JsonSchema + 'static>(
    kind: UserFileKind,
    path: &Path,
    text: &str,
    retired: RetiredLookup<'_>,
    extra: ExtraIssues<'_>,
) -> Option<FileProblem> {
    let problem = match serde_json::from_str::<Value>(text) {
        Err(e) => Problem::NotJson(e.to_string()),
        Ok(doc) => {
            let mut keys = key_issues::<T>(&doc, retired);
            keys.extend(extra(&doc));
            if keys.is_empty() {
                return None;
            }
            Problem::Keys(keys)
        }
    };
    Some(FileProblem { kind, path: path.to_path_buf(), problem, note: None })
}

/// The largest user file the gate reads (and the crew loader parses): 1 MiB,
/// far past any real manifest or config. A larger one is reported, never
/// buffered.
pub const MAX_USER_FILE_BYTES: u64 = 1024 * 1024;

/// `path`'s text, bounded by [`MAX_USER_FILE_BYTES`].
fn read_bounded(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut text = String::new();
    file.take(MAX_USER_FILE_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_USER_FILE_BYTES {
        return Err(std::io::Error::other(format!("larger than the {MAX_USER_FILE_BYTES}-byte cap")));
    }
    Ok(text)
}

/// Check the file at `path` against `T`. `None` when it is clean or absent.
pub fn check_path<T: JsonSchema + 'static>(kind: UserFileKind, path: &Path, retired: RetiredLookup<'_>) -> Option<FileProblem> {
    check_path_and::<T>(kind, path, retired, &|_| Vec::new())
}

/// [`check_path`] plus `extra` ([`check_text_and`]).
pub fn check_path_and<T: JsonSchema + 'static>(
    kind: UserFileKind,
    path: &Path,
    retired: RetiredLookup<'_>,
    extra: ExtraIssues<'_>,
) -> Option<FileProblem> {
    if is_operator_state(path) {
        return None;
    }
    match read_bounded(path) {
        Ok(text) => check_text_and::<T>(kind, path, &text, retired, extra),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(FileProblem { kind, path: path.to_path_buf(), problem: Problem::Unreadable(e.to_string()), note: None }),
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
pub fn check_dir<T: JsonSchema + 'static>(kind: UserFileKind, dir: &Path, retired: RetiredLookup<'_>) -> Vec<FileProblem> {
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
pub fn config_retired(path: &str) -> Option<String> {
    crate::config::RENAMED_SETTINGS
        .iter()
        .find(|r| r.old_key == path)
        .map(|r| format!("renamed to `{}` in 4.0 (#2902); {}", r.new_key, r.advice))
        .or_else(|| {
            crate::config::RETIRED_SETTINGS.iter().find(|r| r.key == path).map(|r| r.line.to_string())
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
