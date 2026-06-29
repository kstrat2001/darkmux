//! (#937) `darkmux config set/get/list` — a convenience reader/writer over
//! `~/.darkmux/config.json` so the network-health remediation blocks (#932/#934)
//! can promise copy-pasteable fixes (`darkmux config set redis.host <addr>`)
//! instead of "hand-edit the JSON." Operator-owned file; this just saves the
//! hand-edit step (sovereignty #44 — read + propose, the operator can still edit
//! by hand and `config list`/`doctor` explain what's set).
//!
//! `set` validates the dotted key against a registry (a typo is surfaced, never
//! silently written) and coerces the value to the field's type. **Secrets are
//! NOT config** (the Redis password + serve token live in the macOS Keychain) —
//! `set` refuses the known secret keys and points at the `security` form.

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use darkmux_types::config::{DarkmuxConfig, FleetMode};
use darkmux_types::paths::{ResolveScope, resolve};
use serde_json::Value;
use std::path::Path;

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Set a config key (dotted path) to a value, e.g.
    /// `darkmux config set redis.host 100.74.208.36` or
    /// `darkmux config set fleet.mode hub`.
    Set {
        /// Dotted config key (e.g. `redis.host`, `fleet.mode`,
        /// `runtime.strict_selection`).
        key: String,
        /// The value. Coerced to the key's type (bool/number/string).
        value: String,
    },
    /// Print a key's value as stored in config.json (the durable setting).
    /// `darkmux doctor` shows the fully RESOLVED value with provenance
    /// (env > config.json > built-in default).
    Get {
        /// Dotted config key.
        key: String,
    },
    /// Print the full config.json (the durable settings on this machine).
    List,
}

/// The scalar type of a settable value — drives parsing the operator's string
/// arg into the right JSON scalar and validating it.
#[derive(Clone, Copy)]
enum Ty {
    Str,
    Bool,
    Uint,
    Float,
    /// A string constrained to the `FleetMode` token set (#933).
    FleetMode,
}

/// Every settable dotted key + its type — THE contract for `config set`. A key
/// not listed here is rejected with a suggestion (typo-surfacing). Drift-guarded
/// by `every_with_defaults_key_is_settable` so a new visible config field can't
/// silently become un-settable. Secrets are deliberately absent (see
/// `SECRET_KEYS`); `schema_version` is managed, not operator-set.
const KEYS: &[(&str, Ty)] = &[
    ("machine_id", Ty::Str),
    ("orchestrator", Ty::Str),
    ("lms_bin", Ty::Str),
    ("lmstudio_url", Ty::Str),
    ("redis.enabled", Ty::Bool),
    ("redis.host", Ty::Str),
    ("redis.port", Ty::Uint),
    ("redis.db", Ty::Uint),
    ("redis.stream", Ty::Str),
    ("redis.maxlen", Ty::Uint),
    ("audit.enabled", Ty::Bool),
    ("audit.dir", Ty::Str),
    ("runtime.inactivity_timeout_seconds", Ty::Uint),
    ("runtime.max_turns", Ty::Uint),
    ("runtime.max_tokens", Ty::Uint),
    ("runtime.strict_selection", Ty::Bool),
    ("runtime.feedback_injection", Ty::Bool),
    ("runtime.default_role", Ty::Str),
    ("runtime.check_updates", Ty::Bool),
    ("runtime.daemon_cors_origins", Ty::Str),
    ("runtime.daemon_auth_enabled", Ty::Bool),
    ("runtime.injected_context_fraction", Ty::Float),
    ("fleet.mode", Ty::FleetMode),
    ("dirs.flows", Ty::Str),
    ("dirs.audit", Ty::Str),
    ("dirs.notebook", Ty::Str),
    ("dirs.skills", Ty::Str),
    ("dirs.crew", Ty::Str),
    ("dirs.templates", Ty::Str),
    ("dirs.runtime_agents", Ty::Str),
    ("dirs.ack", Ty::Str),
    ("dirs.identity", Ty::Str),
    ("dirs.fleet_file", Ty::Str),
    ("dirs.openclaw_config", Ty::Str),
];

/// Keys that are deliberately NOT config — a secret that lives in the macOS
/// Keychain. `set` refuses them with the `security` form instead of a generic
/// "unknown key", so the operator learns where the secret actually goes.
const SECRET_KEYS: &[(&str, &str)] = &[
    ("redis.password", "darkmux-redis"),
    ("redis.requirepass", "darkmux-redis"),
    ("serve.token", "darkmux-serve-token"),
    ("runtime.serve_token", "darkmux-serve-token"),
];

pub fn run(cmd: ConfigCmd) -> Result<()> {
    let path = resolve(ResolveScope::Auto).config;
    match cmd {
        ConfigCmd::Set { key, value } => {
            let msg = set_at(&path, &key, &value)?;
            println!("{msg}");
        }
        ConfigCmd::Get { key } => {
            println!("{}", get_at(&path, &key)?);
        }
        ConfigCmd::List => {
            println!("{}", list_at(&path)?);
        }
    }
    Ok(())
}

/// Look up a key's type, or `None` if it isn't a known settable key.
fn key_type(key: &str) -> Option<Ty> {
    KEYS.iter().find(|(k, _)| *k == key).map(|(_, t)| *t)
}

/// Set `key` to `value` in the config.json at `path`, returning the operator
/// confirmation line. Pure-ish (file IO only) so tests drive it with a temp path.
fn set_at(path: &Path, key: &str, value: &str) -> Result<String> {
    if let Some((_, item)) = SECRET_KEYS.iter().find(|(k, _)| *k == key) {
        bail!(
            "`{key}` is a secret and never lives in config.json — store it in the macOS Keychain:\n  \
             security add-generic-password -U -a \"$USER\" -s {item} -w <value>"
        );
    }
    let Some(ty) = key_type(key) else {
        bail!("unknown config key `{key}`{}", suggestion(key));
    };
    // Flatten the parse detail into ONE message (anyhow's Display shows only the
    // top context), so the operator always sees the specific hint —
    // "invalid value for `fleet.mode`: invalid fleet.mode `hubb` — valid: …".
    let parsed = parse_value(ty, value).map_err(|e| anyhow!("invalid value for `{key}`: {e}"))?;

    let mut root = load_object(path)?;
    set_path(&mut root, key, parsed.clone());

    // Sanity: the result must still deserialize as a DarkmuxConfig (it will —
    // every key maps to a typed field of the right type), so a write can never
    // produce a config the loader would reject.
    serde_json::from_value::<DarkmuxConfig>(root.clone())
        .context("the resulting config.json would not parse — aborting the write")?;

    let pretty = serde_json::to_string_pretty(&root).context("serializing config.json")?;
    std::fs::write(path, pretty + "\n").with_context(|| format!("writing {}", path.display()))?;
    Ok(format!("set `{key}` = {parsed} in {}", path.display()))
}

/// Print a key's stored value, or note it's unset (falls through to env/default).
fn get_at(path: &Path, key: &str) -> Result<String> {
    if let Some((_, item)) = SECRET_KEYS.iter().find(|(k, _)| *k == key) {
        // Consistent with `set`: a secret is never in config.json — point at the
        // Keychain rather than reporting "(unset)".
        return Ok(format!(
            "`{key}` is a Keychain secret (item `{item}`), not a config value — darkmux never stores it in config.json"
        ));
    }
    if key_type(key).is_none() {
        bail!("unknown config key `{key}`{}", suggestion(key));
    }
    let root = load_object(path)?;
    Ok(match get_path(&root, key) {
        Some(v) => format!("{v}"),
        None => format!(
            "(unset — falls through to env/built-in default; `darkmux doctor` shows the resolved value for `{key}`)"
        ),
    })
}

/// Print the whole config.json (or a hint when there isn't one yet).
fn list_at(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(format!(
            "(no config.json at {} — run `darkmux init` to write the full default config)",
            path.display()
        ));
    }
    let root = load_object(path)?;
    serde_json::to_string_pretty(&root).context("serializing config.json")
}

/// Load config.json as a JSON object; a missing file is an empty object (so
/// `set` can create it), a malformed file is a hard error here (unlike the
/// lenient load path — `config set` must not silently clobber a file it can't
/// understand).
fn load_object(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(raw) if raw.trim().is_empty() => Ok(Value::Object(Default::default())),
        Ok(raw) => {
            let v: Value = serde_json::from_str(&raw)
                .with_context(|| format!("{} is not valid JSON — fix it by hand first", path.display()))?;
            match v {
                Value::Object(_) => Ok(v),
                _ => bail!("{} is not a JSON object", path.display()),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Default::default())),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Parse the operator's string arg into the JSON scalar the field expects.
fn parse_value(ty: Ty, raw: &str) -> Result<Value> {
    Ok(match ty {
        Ty::Str => Value::String(raw.to_string()),
        Ty::Bool => match raw.trim().to_ascii_lowercase().as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => bail!("expected `true` or `false`, got `{raw}`"),
        },
        Ty::Uint => {
            let n: u64 = raw
                .trim()
                .parse()
                .map_err(|_| anyhow!("expected a non-negative integer, got `{raw}`"))?;
            Value::Number(n.into())
        }
        Ty::Float => {
            let f: f64 = raw
                .trim()
                .parse()
                .map_err(|_| anyhow!("expected a number, got `{raw}`"))?;
            serde_json::Number::from_f64(f)
                .map(Value::Number)
                .ok_or_else(|| anyhow!("`{raw}` is not a finite number"))?
        }
        Ty::FleetMode => {
            let mode = FleetMode::parse(raw)
                .ok_or_else(|| anyhow!("invalid fleet.mode `{raw}` — valid: standalone, hub, peer"))?;
            // Store the canonical lowercase token regardless of the input casing.
            Value::String(mode.as_str().to_string())
        }
    })
}

/// Set a dotted path in a JSON object, creating intermediate objects. Replaces
/// a non-object intermediate (e.g. a stray scalar where a block belongs).
fn set_path(root: &mut Value, key: &str, val: Value) {
    let parts: Vec<&str> = key.split('.').collect();
    let mut cur = root;
    for seg in &parts[..parts.len() - 1] {
        let is_obj = cur.get(*seg).map(Value::is_object).unwrap_or(false);
        if !is_obj {
            cur[*seg] = Value::Object(Default::default());
        }
        cur = cur.get_mut(*seg).expect("just ensured the object exists");
    }
    cur[parts[parts.len() - 1]] = val;
}

/// Read a dotted path from a JSON object.
fn get_path<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
    let mut cur = root;
    for seg in key.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// "Did you mean" suffix for an unknown key — up to 3 closest known keys by
/// Levenshtein distance (≤ 3), else a pointer to `config list`.
fn suggestion(key: &str) -> String {
    let mut scored: Vec<(usize, &str)> = KEYS
        .iter()
        .map(|(k, _)| (levenshtein(key, k), *k))
        .filter(|(d, _)| *d <= 3)
        .collect();
    scored.sort_by_key(|(d, _)| *d);
    let near: Vec<&str> = scored.into_iter().take(3).map(|(_, k)| k).collect();
    if near.is_empty() {
        " — run `darkmux config list` to see the settable keys".to_string()
    } else {
        format!(" — did you mean: {}?", near.join(", "))
    }
}

/// Tiny inline Levenshtein (two-row DP) — a 10-line need beats a crate, per the
/// dep-discipline convention.
fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn tmp() -> NamedTempFile {
        NamedTempFile::new().unwrap()
    }

    #[test]
    fn set_creates_and_coerces_types() {
        let f = tmp();
        let p = f.path();
        // missing/empty file → starts from {}; string, bool, number, float, fleet.
        set_at(p, "redis.host", "100.74.208.36").unwrap();
        set_at(p, "redis.port", "6380").unwrap();
        set_at(p, "redis.enabled", "true").unwrap();
        set_at(p, "runtime.injected_context_fraction", "0.2").unwrap();
        set_at(p, "fleet.mode", "HUB").unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert_eq!(v["redis"]["host"], Value::String("100.74.208.36".into()));
        assert_eq!(v["redis"]["port"], serde_json::json!(6380), "coerced to number");
        assert_eq!(v["redis"]["enabled"], Value::Bool(true), "coerced to bool");
        assert_eq!(v["runtime"]["injected_context_fraction"], serde_json::json!(0.2));
        assert_eq!(v["fleet"]["mode"], Value::String("hub".into()), "fleet.mode normalized to canonical token");
        // The written file still parses as a DarkmuxConfig.
        let cfg: DarkmuxConfig = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert_eq!(cfg.redis.unwrap().port, Some(6380));
    }

    #[test]
    fn unknown_key_is_rejected_with_suggestion() {
        let f = tmp();
        let err = set_at(f.path(), "redis.hsot", "x").unwrap_err().to_string();
        assert!(err.contains("unknown config key"), "{err}");
        assert!(err.contains("redis.host"), "suggests the near key: {err}");
    }

    #[test]
    fn secret_key_is_refused_with_keychain_pointer() {
        let f = tmp();
        let err = set_at(f.path(), "redis.password", "leaked").unwrap_err().to_string();
        assert!(err.contains("secret"), "{err}");
        assert!(err.contains("security add-generic-password"), "points at the keychain form: {err}");
        assert!(err.contains("darkmux-redis"), "names the item: {err}");
        // And nothing was written.
        assert!(std::fs::read_to_string(f.path()).unwrap_or_default().trim().is_empty());
    }

    #[test]
    fn bad_value_type_is_rejected() {
        let f = tmp();
        assert!(set_at(f.path(), "redis.port", "not-a-number").is_err());
        assert!(set_at(f.path(), "redis.enabled", "yes").is_err(), "bool is strict true/false");
        assert!(set_at(f.path(), "fleet.mode", "hubb").unwrap_err().to_string().contains("invalid fleet.mode"));
    }

    #[test]
    fn get_reports_value_or_unset() {
        let f = tmp();
        set_at(f.path(), "machine_id", "studio").unwrap();
        assert!(get_at(f.path(), "machine_id").unwrap().contains("studio"));
        assert!(get_at(f.path(), "redis.host").unwrap().contains("unset"));
    }

    #[test]
    fn set_preserves_other_keys() {
        let f = tmp();
        set_at(f.path(), "machine_id", "studio").unwrap();
        set_at(f.path(), "redis.host", "h").unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(f.path()).unwrap()).unwrap();
        assert_eq!(v["machine_id"], Value::String("studio".into()), "first key survives the second set");
        assert_eq!(v["redis"]["host"], Value::String("h".into()));
    }

    /// Drift guard: every visible key `with_defaults()` writes must be settable
    /// via `config set` — so adding a visible config field without a registry
    /// entry fails here, not silently at an operator's `config set`.
    #[test]
    fn every_with_defaults_key_is_settable() {
        let v = serde_json::to_value(DarkmuxConfig::with_defaults()).unwrap();
        let mut leaves = Vec::new();
        collect_leaves(&v, String::new(), &mut leaves);
        for leaf in leaves {
            // `schema_version` is managed, not operator-set.
            if leaf == "schema_version" {
                continue;
            }
            assert!(
                key_type(&leaf).is_some(),
                "with_defaults writes `{leaf}` but it isn't in the config-set KEYS registry — add it"
            );
        }
    }

    fn collect_leaves(v: &Value, prefix: String, out: &mut Vec<String>) {
        match v {
            Value::Object(map) => {
                for (k, val) in map {
                    let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    collect_leaves(val, path, out);
                }
            }
            _ => out.push(prefix),
        }
    }

    /// Reverse drift guard: a typo IN the registry (a key that isn't a real
    /// typed field) would land in `extras` on parse and pass silently — the
    /// re-parse gate is lenient about unknown keys. Assert each KEYS key, when
    /// set, leaves EVERY extras map empty, i.e. it resolves to a typed field.
    #[test]
    fn every_keys_entry_resolves_to_a_typed_field() {
        for (key, ty) in KEYS {
            let sentinel = match ty {
                Ty::Bool => Value::Bool(true),
                Ty::Uint => serde_json::json!(1),
                Ty::Float => serde_json::json!(0.5),
                // a valid FleetMode token doubles as the generic string sentinel
                Ty::Str | Ty::FleetMode => Value::String("standalone".into()),
            };
            let mut root = Value::Object(Default::default());
            set_path(&mut root, key, sentinel);
            let cfg: DarkmuxConfig = serde_json::from_value(root).unwrap();
            assert_eq!(
                total_extras(&cfg),
                0,
                "KEYS key `{key}` is not a real typed field — it overflowed into `extras` (registry typo)"
            );
        }
    }

    /// Sum of every `extras` overflow map in a parsed config — non-zero means an
    /// unknown key landed in forward-compat overflow rather than a typed field.
    fn total_extras(c: &DarkmuxConfig) -> usize {
        c.extras.len()
            + c.dirs.as_ref().map_or(0, |x| x.extras.len())
            + c.redis.as_ref().map_or(0, |x| x.extras.len())
            + c.audit.as_ref().map_or(0, |x| x.extras.len())
            + c.runtime.as_ref().map_or(0, |x| x.extras.len())
            + c.fleet.as_ref().map_or(0, |x| x.extras.len())
    }

    #[test]
    fn get_on_a_secret_key_points_at_the_keychain() {
        let f = tmp();
        let out = get_at(f.path(), "redis.password").unwrap();
        assert!(out.contains("Keychain secret"), "{out}");
        assert!(out.contains("darkmux-redis"), "names the item: {out}");
    }

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("host", "hsot"), 2);
        assert_eq!(levenshtein("fleet.mode", "fleet.mode"), 0);
    }
}
