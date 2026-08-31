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
    /// `darkmux config set redis.host 100.64.0.2`,
    /// `darkmux config set fleet.mode hub`, or
    /// `darkmux config set role_profiles.review-judge qwen35b` (bind a role to
    /// a profile — #1475). The role id must be a real one (`darkmux role
    /// list`) — a typo'd or invented role id is settable but resolves nothing
    /// (#1547; `darkmux doctor` flags it).
    Set {
        /// Dotted config key (e.g. `redis.host`, `fleet.mode`,
        /// `runtime.strict_selection`, `role_profiles.<role-id>`).
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
    /// (#2110/#2109 review finding 6) A string constrained to
    /// `darkmux_crew::host_probe::thermal::THERMAL_STATES`
    /// (`nominal`/`fair`/`serious`/`critical`) — `runtime.thermal.pause_at`
    /// / `.resume_at`. Without this, a typo (`"seroius"`) silently parsed
    /// as `Ty::Str`, and `severity()`'s `unwrap_or(THERMAL_STATES.len())`
    /// ranks an unrecognized name WORSE than `critical` — inverting either
    /// knob's intent with no error anywhere in the path: a typo'd
    /// `pause_at` makes `sev >= severity(pause_at)` (4) all but
    /// unreachable for any real OS reading, silently disabling the
    /// governor's soft pause (the breaker's own hardcoded `"critical"`
    /// check is unaffected); a typo'd `resume_at` makes
    /// `sev <= severity(resume_at)` (4) true for every reading, so the
    /// hysteresis hold fills up regardless of actual temperature and the
    /// pause clears almost immediately even on a machine still hot.
    ThermalState,
    /// (#1685) Comma-separated list of non-empty, trimmed strings, coerced
    /// to a JSON array — `darkmux config set cmd.allowed pr-list,pr-merge`.
    /// REPLACES the whole array (there is no incremental add); an empty
    /// string clears it to `[]`.
    StrList,
    /// (#2093) The value is parsed as raw JSON and stored verbatim — for a
    /// field too structured for any scalar `Ty` (`hooks.rules`: an array of
    /// `{match: {...}, http: "..."}` objects). Any syntactically valid JSON
    /// is accepted here; SEMANTIC validation (a non-loopback URL, an empty
    /// match) is `HookSink`/`darkmux doctor`'s job, per the config-leniency
    /// contract (registry entry 7 — validation at resolution/consumption,
    /// never the hot load path).
    Json,
}

/// Every settable dotted key + its type — THE contract for `config set`. A key
/// not listed here is rejected with a suggestion (typo-surfacing). Drift-guarded
/// by `every_with_defaults_key_is_settable` so a new visible config field can't
/// silently become un-settable. Secrets are deliberately absent (see
/// `SECRET_KEYS`); `schema_version` is managed, not operator-set.
///
/// (#1475 packet 1) The `role_profiles` map is NOT listed here — its keys are
/// dynamic (`role_profiles.<role-id>`, one per role the operator binds), so
/// `key_type` recognizes that prefix directly. `with_defaults` writes it as an
/// empty `{}` (no leaves), so the settable-keys drift guard is unaffected.
const KEYS: &[(&str, Ty)] = &[
    ("machine_id", Ty::Str),
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
    ("runtime.model_load_timeout_seconds", Ty::Uint),
    ("runtime.max_turns", Ty::Uint),
    ("runtime.max_tokens", Ty::Uint),
    ("runtime.max_tokens_per_call", Ty::Uint),
    ("runtime.reasoning_checkpoint_interval_tokens", Ty::Uint),
    // (#2171) The GENERATION check-in — bounds every call that does NOT
    // carry the reasoning bound above, not just reasoning ones.
    ("runtime.generation_checkpoint_interval_tokens", Ty::Uint),
    // (#2190) Per-dispatch budget for intra-turn stall recoveries — see
    // #2195's caution: this registry has broken main TWICE by a new
    // runtime knob being forgotten here.
    ("runtime.max_stall_recoveries", Ty::Uint),
    ("runtime.strict_selection", Ty::Bool),
    ("runtime.log_level", Ty::Str),
    ("runtime.feedback_injection", Ty::Bool),
    ("runtime.default_role", Ty::Str),
    ("runtime.check_updates", Ty::Bool),
    ("runtime.daemon_cors_origins", Ty::Str),
    ("runtime.daemon_auth_enabled", Ty::Bool),
    ("runtime.injected_context_fraction", Ty::Float),
    // (#1698 Packet B2) The `darkmux acp` process's idle self-exit budget.
    ("runtime.acp_idle_exit_minutes", Ty::Uint),
    // (#2094) The global inter-turn rest, in milliseconds, the internal
    // runtime sleeps between inference turns on every LOCAL dispatch.
    ("runtime.turn_delay_ms", Ty::Uint),
    // (#2107, #1833) Cadence of `darkmux serve`'s daemon-side continuous
    // host sampler feeding the machine stats drawer. `0` disables it.
    ("runtime.host_sampler_interval_ms", Ty::Uint),
    // (#2111) Every Nth dispatch-sampler tick emits a `machine.telemetry` record; 0 = off.
    ("runtime.telemetry_record_every_samples", Ty::Uint),
    // (#2110/#2109) The thermal governor + breaker's tuning block —
    // see `ThermalConfig`'s own doc.
    ("runtime.thermal.enabled", Ty::Bool),
    ("runtime.thermal.pause_at", Ty::ThermalState),
    ("runtime.thermal.resume_at", Ty::ThermalState),
    ("runtime.thermal.resume_hold_ms", Ty::Uint),
    ("runtime.thermal.max_pause_ms", Ty::Uint),
    ("runtime.thermal.min_cpu_speed_limit_pct", Ty::Uint),
    ("runtime.thermal.speed_limit_hold_samples", Ty::Uint),
    ("fleet.mode", Ty::FleetMode),
    // (#1260) The per-execution remote token allowance for endpoint-staffed
    // crew seats (one pipeline stage = one execution). Tokens, never currency.
    ("remote.max_tokens_per_execution", Ty::Uint),
    // (#1230 Packet 1) Max concurrent remote dispatches
    // `darkmux_crew::concurrent_dispatch::run_bounded` runs at once.
    ("remote.concurrent_cap", Ty::Uint),
    // (#1230 Packet 5) `mission status`'s stale-active drift threshold.
    ("mission.stale_active_days", Ty::Uint),
    // (#1349) The PR-review pipeline's judge-step bounded-concurrency cap.
    ("review.judge_concurrency", Ty::Uint),
    // (#1876/#1877) The judge stage's remote-budget exhaustion policy —
    // `false` (default): a skip is a coverage fact, findings still render
    // plus a banner; `true`: restores the pre-#1876 "any skip is fatal"
    // behavior.
    ("review.judge_fail_on_any_skip", Ty::Bool),
    // (#1698 Packet B2) The radio interpreter's staffing + persona knobs.
    ("radio.router_profile", Ty::Str),
    ("radio.answerer_profile", Ty::Str),
    ("radio.humor", Ty::Uint),
    ("dirs.flows", Ty::Str),
    ("dirs.audit", Ty::Str),
    ("dirs.notebook", Ty::Str),
    ("dirs.skills", Ty::Str),
    ("dirs.crew", Ty::Str),
    ("dirs.templates", Ty::Str),
    ("dirs.ack", Ty::Str),
    ("dirs.identity", Ty::Str),
    ("dirs.fleet_file", Ty::Str),
    // (#1585) The drift guard `every_with_defaults_key_is_settable` cannot
    // catch an omission here: it walks `with_defaults()`, where `dirs` is
    // `None`, so no `dirs.*` key is ever checked. Adding a `DirsConfig` field
    // means adding it HERE by hand, or the tier exists in the resolver and is
    // unreachable from `config set`/`get`.
    ("dirs.lab", Ty::Str),
    // (#1685) The `gh`-verb allowlist gate — see `CmdConfig`'s own doc.
    ("cmd.enabled", Ty::Bool),
    ("cmd.allowed", Ty::StrList),
    // (#2093) The flow-record hook sink — see `HooksConfig`'s own doc.
    // `hooks.rules` is deliberately NOT a per-field-settable structure (a
    // rule is an object, not a scalar) — it's settable only as a whole,
    // via raw JSON (`Ty::Json`). An operator wiring up their first rule by
    // hand-editing `~/.darkmux/config.json` is the expected path; `config
    // set hooks.rules '[...]'` covers the scriptable case.
    ("hooks.enabled", Ty::Bool),
    ("hooks.outbox_dir", Ty::Str),
    ("hooks.rules", Ty::Json),
    // (#2183) jq transform bounds — a runaway or oversized adapter output is a
    // TERMINAL per-line failure, so both caps are operator-tunable like every other
    // hooks knob rather than living only in code.
    ("hooks.jq_timeout_ms", Ty::Uint),
    ("hooks.jq_max_output_bytes", Ty::Uint),
    // (#2093 merge-gate finding 5) The hard cap on undelivered bytes per
    // rule, in MiB — see `HooksConfig::max_outbox_mb`'s own doc.
    ("hooks.max_outbox_mb", Ty::Uint),
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
    // (#1323) ForceUser, not Auto — `darkmux config get/set/list` operates on
    // the user-scope config.json, matching `DarkmuxConfig::load_resolved`. Under
    // Auto a stray project-local `.darkmux/` (missions/phases/lessons) would
    // silently redirect reads/writes to the wrong file. Config is user/machine-
    // level; there is no legitimate per-project config.
    let path = resolve(ResolveScope::ForceUser).config;
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

/// Look up a key's type, or `None` if it isn't a known settable key. Static
/// keys come from `KEYS`; the `role_profiles.<role-id>` map (#1475 packet 1) is
/// DYNAMIC — any single `role_profiles.<role-id>` segment is a settable string
/// (the bound profile name). Only the SHAPE is validated here (one level:
/// `role_profiles.<role> -> <profile>`); that the named profile EXISTS is a
/// resolution-time / `darkmux doctor` concern (config-leniency contract 7).
fn key_type(key: &str) -> Option<Ty> {
    if let Some(role) = key.strip_prefix("role_profiles.") {
        // Exactly one more segment — reject a blank role (`role_profiles.`) or a
        // deeper path (`role_profiles.a.b`); the map is role -> profile name.
        return (!role.is_empty() && !role.contains('.')).then_some(Ty::Str);
    }
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
        // This used to return here WITHOUT reading the file, asserting
        // "darkmux never stores it in config.json". Two problems: the claim
        // was made without looking, and it is false in a reachable state —
        // `#[serde(flatten)] extras` plus `set_at`'s whole-root writeback
        // preserves a hand-added secret across every subsequent `config set`.
        // So the one command an operator runs to check whether a secret leaked
        // into their config reassured them it hadn't. Describe the mechanism
        // (`config set` refuses to write it) and then actually look.
        let present = load_object(path).ok().is_some_and(|root| get_path(&root, key).is_some());
        return Ok(if present {
            format!(
                "`{key}` is read from the macOS Keychain (item `{item}`), and `darkmux config set` refuses to write it here — \
                 but it IS PRESENT in {}. Something added it by hand; remove it, and treat the value as exposed.",
                path.display()
            )
        } else {
            format!(
                "`{key}` is read from the macOS Keychain (item `{item}`); `darkmux config set` refuses to write it to config.json. \
                 Not present in {}.",
                path.display()
            )
        });
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
/// `pub(crate)` (#1698 Packet B2) — the radio answering seat's grounding
/// assembler (`src/radio_answer.rs`) reuses this AS the "live config
/// surface" grounding source rather than re-deriving its own read of
/// `config.json` (single derivation of "what does `config list` show").
pub(crate) fn list_at(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(format!(
            "(no config.json at {} — run `darkmux init` to write the full default config)",
            path.display()
        ));
    }
    let mut root = load_object(path)?;
    redact_secret_keys(&mut root);
    serde_json::to_string_pretty(&root).context("serializing config.json")
}

/// Replace the value at every [`SECRET_KEYS`] path with a marker, in place.
///
/// `config set` refuses to write these, but nothing stops a hand edit, and
/// `#[serde(flatten)] extras` preserves whatever it finds. Printing the file
/// verbatim then defeats the entire Keychain carve-out, whose stated purpose
/// is that these values never reach a terminal — and on this project, never
/// reach an agent-session transcript.
///
/// Applied in [`list_at`] rather than at the call sites, because it has two
/// consumers: the `darkmux config list` verb and
/// `radio_answer::config_block`, which uses the same function as the live
/// config-surface grounding for the answering seat. One layer covers both.
/// (That grounding is already gated to machine-local seats, so this is not a
/// cloud-egress fix — it is a terminal, transcript, and local-model-context
/// one.)
///
/// The KEY is deliberately still shown. Its presence is exactly what the
/// operator needs to know; only the value is withheld.
fn redact_secret_keys(root: &mut Value) {
    for (key, _) in SECRET_KEYS {
        let mut cur: &mut Value = root;
        let mut parts = key.split('.').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                if let Some(obj) = cur.as_object_mut() {
                    if let Some(slot) = obj.get_mut(part) {
                        *slot = Value::String("(redacted — secrets belong in the Keychain, not here)".into());
                    }
                }
                break;
            }
            match cur.get_mut(part) {
                Some(next) => cur = next,
                None => break,
            }
        }
    }
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
        Ty::ThermalState => {
            let lower = raw.trim().to_ascii_lowercase();
            let states = darkmux_crew::host_probe::thermal::THERMAL_STATES;
            if !states.contains(&lower.as_str()) {
                bail!(
                    "invalid thermal state `{raw}` — valid: {}",
                    states.join(", ")
                );
            }
            Value::String(lower)
        }
        Ty::StrList => Value::Array(
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_string()))
                .collect(),
        ),
        Ty::Json => serde_json::from_str(raw)
            .map_err(|e| anyhow!("not valid JSON: {e}"))?,
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
        set_at(p, "redis.host", "100.64.0.2").unwrap();
        set_at(p, "redis.port", "6380").unwrap();
        set_at(p, "redis.enabled", "true").unwrap();
        set_at(p, "runtime.injected_context_fraction", "0.2").unwrap();
        set_at(p, "fleet.mode", "HUB").unwrap();
        set_at(p, "runtime.turn_delay_ms", "3000").unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert_eq!(v["redis"]["host"], Value::String("100.64.0.2".into()));
        assert_eq!(v["redis"]["port"], serde_json::json!(6380), "coerced to number");
        assert_eq!(v["redis"]["enabled"], Value::Bool(true), "coerced to bool");
        assert_eq!(v["runtime"]["injected_context_fraction"], serde_json::json!(0.2));
        assert_eq!(v["fleet"]["mode"], Value::String("hub".into()), "fleet.mode normalized to canonical token");
        assert_eq!(v["runtime"]["turn_delay_ms"], serde_json::json!(3000));
        // The written file still parses as a DarkmuxConfig.
        let cfg: DarkmuxConfig = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert_eq!(cfg.redis.unwrap().port, Some(6380));
        assert_eq!(cfg.runtime.unwrap().turn_delay_ms, Some(3000));
    }

    /// (#2094) `runtime.turn_delay_ms` rejects a non-integer value the same
    /// way every other `Ty::Uint` key does, and `config get` reports the
    /// stored value verbatim.
    #[test]
    fn turn_delay_ms_rejects_bad_value_and_get_round_trips() {
        let f = tmp();
        let p = f.path();
        let err = set_at(p, "runtime.turn_delay_ms", "abc").unwrap_err();
        assert!(
            format!("{err}").contains("runtime.turn_delay_ms"),
            "error names the key: {err}"
        );
        set_at(p, "runtime.turn_delay_ms", "500").unwrap();
        assert_eq!(get_at(p, "runtime.turn_delay_ms").unwrap(), "500");
    }

    /// (#2190) `runtime.max_stall_recoveries` is settable via `darkmux config
    /// set` and round-trips through get — the exact knob #2190's live
    /// evidence needed (a hard-coded budget of 2 with no operator override).
    /// Same `Ty::Uint` shape as `turn_delay_ms` above.
    #[test]
    fn max_stall_recoveries_settable_and_get_round_trips() {
        let f = tmp();
        let p = f.path();
        let err = set_at(p, "runtime.max_stall_recoveries", "abc").unwrap_err();
        assert!(
            format!("{err}").contains("runtime.max_stall_recoveries"),
            "error names the key: {err}"
        );
        set_at(p, "runtime.max_stall_recoveries", "4").unwrap();
        assert_eq!(get_at(p, "runtime.max_stall_recoveries").unwrap(), "4");
        let cfg: DarkmuxConfig = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert_eq!(cfg.runtime.unwrap().max_stall_recoveries, Some(4));
    }

    /// (#1475 packet 1) `role_profiles.<role>` is a dynamic settable key — it
    /// round-trips through set → get → list and the written file parses back as
    /// a `DarkmuxConfig` with the role bound to the profile name. Many roles may
    /// name one profile.
    ///
    /// (#1547) Uses REAL role ids (`review-judge`, `review-verify`,
    /// `review-probe-high`, `analyst`) — the pre-#1547 version of this test used
    /// bare `judge`/`verify`/`probe-high`, none of which are real role ids, which
    /// was itself an instance of the trap #1547 fixes: a doc/test example that
    /// reads as live config but no-ops on every dispatch path. `set_at` itself
    /// only validates the KEY SHAPE (one role segment) — that the role id is
    /// real is a resolution-time / `darkmux doctor` concern (config-leniency
    /// contract 7), so this test still isn't the place that would catch a typo'd
    /// role id; that's `role_profiles_status` in `darkmux-doctor`.
    #[test]
    fn role_profiles_dynamic_key_round_trips() {
        let f = tmp();
        let p = f.path();
        set_at(p, "role_profiles.review-judge", "qwen35b").unwrap();
        set_at(p, "role_profiles.review-verify", "qwen35b").unwrap();
        set_at(p, "role_profiles.review-probe-high", "qwen27b").unwrap();

        // get reads the stored value back.
        assert!(get_at(p, "role_profiles.review-judge").unwrap().contains("qwen35b"));
        assert!(get_at(p, "role_profiles.review-probe-high").unwrap().contains("qwen27b"));
        // An unbound role reports unset (falls through to default_profile).
        assert!(get_at(p, "role_profiles.analyst").unwrap().contains("unset"));

        // list dumps the whole map.
        let listed = list_at(p).unwrap();
        assert!(listed.contains("role_profiles"));

        // The written file parses back as a DarkmuxConfig with the bindings.
        let cfg: DarkmuxConfig =
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        let map = cfg.role_profiles.unwrap();
        assert_eq!(map.get("review-judge").map(String::as_str), Some("qwen35b"));
        assert_eq!(map.get("review-verify").map(String::as_str), Some("qwen35b"), "many roles -> one profile");
    }

    /// The `role_profiles` map is one level deep: a blank role or a deeper path
    /// isn't settable, and the bare `role_profiles` key sets a specific role, not
    /// the whole map.
    #[test]
    fn role_profiles_rejects_malformed_keys() {
        let f = tmp();
        assert!(set_at(f.path(), "role_profiles", "x").is_err(), "bare map key not settable");
        assert!(set_at(f.path(), "role_profiles.", "x").is_err(), "blank role rejected");
        assert!(set_at(f.path(), "role_profiles.a.b", "x").is_err(), "deeper path rejected");
    }

    /// (#1685) `cmd.allowed` round-trips as a JSON array, trims whitespace,
    /// drops empty entries, and REPLACES (rather than appends to) whatever
    /// was there before.
    #[test]
    fn cmd_allowed_str_list_round_trips_and_replaces() {
        let f = tmp();
        let p = f.path();
        set_at(p, "cmd.enabled", "true").unwrap();
        set_at(p, "cmd.allowed", "pr-list, pr-info ,,pr-merge").unwrap();
        let cfg: DarkmuxConfig = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        let cmd = cfg.cmd.unwrap();
        assert_eq!(cmd.enabled, Some(true));
        assert_eq!(
            cmd.allowed,
            Some(vec!["pr-list".to_string(), "pr-info".to_string(), "pr-merge".to_string()])
        );
        // A second set REPLACES the list, not appends.
        set_at(p, "cmd.allowed", "pr-approve").unwrap();
        let cfg: DarkmuxConfig = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert_eq!(cfg.cmd.unwrap().allowed, Some(vec!["pr-approve".to_string()]));
        // An empty value clears the list.
        set_at(p, "cmd.allowed", "").unwrap();
        let cfg: DarkmuxConfig = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        assert_eq!(cfg.cmd.unwrap().allowed, Some(Vec::<String>::new()));
    }

    /// (#2093) `hooks.enabled` and `hooks.outbox_dir` are ordinary scalar
    /// keys through the dotted-key registry.
    #[test]
    fn hooks_enabled_and_outbox_dir_set() {
        let f = tmp();
        let p = f.path();
        set_at(p, "hooks.enabled", "true").unwrap();
        set_at(p, "hooks.outbox_dir", "~/.darkmux/hooks").unwrap();
        let cfg: DarkmuxConfig = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        let hooks = cfg.hooks.unwrap();
        assert_eq!(hooks.enabled, Some(true));
        assert_eq!(hooks.outbox_dir.as_deref(), Some("~/.darkmux/hooks"));
    }

    /// (#2093) `hooks.rules` is a structured array — settable as raw JSON
    /// (`Ty::Json`), the one exception to the scalar-only registry. Valid
    /// JSON that doesn't shape-match `Vec<HookRule>` is still accepted at
    /// THIS layer (HookRule's own fields are all-`Option` + `extras`
    /// overflow, so it never fails to parse) — semantic validation (a
    /// non-loopback URL, an empty match) is `HookSink`/doctor's job, not
    /// `config set`'s, per the config-leniency contract.
    #[test]
    fn hooks_rules_settable_as_raw_json() {
        let f = tmp();
        let p = f.path();
        set_at(
            p,
            "hooks.rules",
            r#"[{"match":{"action":"crawl.*"},"http":"http://127.0.0.1:8790/events"}]"#,
        )
        .unwrap();
        let cfg: DarkmuxConfig = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        let rules = cfg.hooks.unwrap().rules.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].http.as_deref(), Some("http://127.0.0.1:8790/events"));
        assert_eq!(rules[0].r#match.as_ref().unwrap().action.as_deref(), Some("crawl.*"));

        // Invalid JSON is a clean rejection, not a panic.
        assert!(set_at(p, "hooks.rules", "not json").is_err());
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
    fn thermal_state_typo_is_rejected_not_silently_stored() {
        // (#2110/#2109 review finding 6) A typo used to parse cleanly as
        // Ty::Str and silently invert the governor's intent (see
        // Ty::ThermalState's own doc). This proves it's now rejected.
        let f = tmp();
        let err = set_at(f.path(), "runtime.thermal.pause_at", "seroius").unwrap_err().to_string();
        assert!(err.contains("invalid thermal state"), "{err}");
        assert!(err.contains("serious"), "error should list valid values: {err}");
    }

    #[test]
    fn thermal_state_accepts_valid_tokens_case_insensitively() {
        let f = tmp();
        set_at(f.path(), "runtime.thermal.pause_at", "SERIOUS").unwrap();
        assert!(get_at(f.path(), "runtime.thermal.pause_at").unwrap().contains("serious"));
        set_at(f.path(), "runtime.thermal.resume_at", "fair").unwrap();
        assert!(get_at(f.path(), "runtime.thermal.resume_at").unwrap().contains("fair"));
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
                // a valid THERMAL_STATES token, same reasoning as FleetMode above
                Ty::ThermalState => Value::String("nominal".into()),
                Ty::StrList => serde_json::json!(["sentinel"]),
                // An empty array is valid JSON that parses cleanly to an
                // empty `Vec<HookRule>` — sufficient to prove the KEY
                // resolves to the typed `hooks.rules` field rather than
                // overflowing into `extras`.
                Ty::Json => serde_json::json!([]),
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
            + c.cmd.as_ref().map_or(0, |x| x.extras.len())
            + c.hooks.as_ref().map_or(0, |x| x.extras.len())
    }

    #[test]
    fn get_on_a_secret_key_points_at_the_keychain() {
        let f = tmp();
        let out = get_at(f.path(), "redis.password").unwrap();
        assert!(out.contains("Keychain"), "{out}");
        assert!(out.contains("darkmux-redis"), "names the item: {out}");
    }

    /// Canary for the secret-in-file tests. Not a credential — it exists so an
    /// assertion can prove the real value never reaches an output surface.
    const CANARY: &str = "NOT-A-REAL-SECRET-canary";

    fn write_cfg_with_secret(p: &Path) {
        std::fs::write(
            p,
            format!(
                r#"{{"schema_version":"1.2","machine_id":"testbox","redis":{{"enabled":true,"host":"127.0.0.1","password":"{CANARY}"}}}}"#
            ),
        )
        .unwrap();
    }

    /// `config get <secret>` answered "darkmux never stores it in config.json"
    /// WITHOUT opening the file — it short-circuited on `SECRET_KEYS` before
    /// `load_object`. The claim is also false in a reachable state:
    /// `#[serde(flatten)] extras` plus a whole-root writeback preserves a
    /// hand-added key across every `config set`. So the one command an
    /// operator runs to check whether a secret leaked into their config
    /// actively reassured them it hadn't.
    #[test]
    fn get_on_a_secret_key_reports_it_when_the_file_actually_has_one() {
        let f = tmp();
        write_cfg_with_secret(f.path());
        let out = get_at(f.path(), "redis.password").unwrap();
        assert!(
            !out.contains("never stores it in config.json"),
            "must not claim absence without having looked: {out}"
        );
        assert!(out.to_lowercase().contains("present"), "must say it IS in the file: {out}");
        assert!(!out.contains(CANARY), "must not echo the value while reporting it: {out}");
    }

    /// The other half: with no such key in the file, say so without
    /// over-claiming about what darkmux "never" does.
    #[test]
    fn get_on_a_secret_key_absent_from_the_file_says_so_without_overclaiming() {
        let f = tmp();
        std::fs::write(f.path(), r#"{"schema_version":"1.2"}"#).unwrap();
        let out = get_at(f.path(), "redis.password").unwrap();
        assert!(out.contains("darkmux-redis"), "still names the Keychain item: {out}");
        assert!(!out.contains("never stores"), "no universal claim: {out}");
    }

    /// `config list` printed the file verbatim, so a hand-added secret reached
    /// the terminal — and any agent-session transcript. It also feeds
    /// `radio_answer::config_block` as the live config-surface grounding, so
    /// redacting here covers both consumers at one layer.
    #[test]
    fn list_redacts_a_secret_that_was_hand_added_to_the_file() {
        let f = tmp();
        write_cfg_with_secret(f.path());
        let out = list_at(f.path()).unwrap();
        assert!(!out.contains(CANARY), "the value must never reach stdout: {out}");
        assert!(out.contains("redacted"), "the key is still shown, marked: {out}");
        assert!(out.contains("machine_id"), "the rest of the config still renders: {out}");
    }

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("host", "hsot"), 2);
        assert_eq!(levenshtein("fleet.mode", "fleet.mode"), 0);
    }
}
