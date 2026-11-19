//! The numeric `DARKMUX_*` env vars, and the refusal of one that is set but
//! does not read as its number.
//!
//! `config_access`'s numeric accessors (`pick_parsed`,
//! `pick_parsed_with_source`, and an `env_str(..)` parsed in place) read a
//! set but unreadable env var as unset, so
//! `DARKMUX_RUNTIME_MAX_TOKENS=50k` would mean no cap at all, while the same
//! value in `config.json` is refused as a wrong type. Every preflight refuses
//! it instead (`config_enum::preflight` adds [`bad_values`]), and
//! `darkmux doctor` fails it. The refusal names the variable, what kind of
//! value it holds and the accepted form, never the value itself: an env var
//! can hold a secret, and the message reaches stderr, doctor and the daemon's
//! log.
//!
//! [`NUMERIC_ENV_VARS`] is every variable those accessors read, with the
//! number type each parses (the type of the `config.json` field it
//! overrides). `every_numeric_accessor_reads_a_registered_var` fails on a
//! variable read there and missing here, or listed here and read nowhere.

use crate::config_enum::{InvalidSetting, SetIn};

/// The number type a numeric env var parses as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Number {
    U16,
    U32,
    U64,
    Usize,
    F64,
}

impl Number {
    /// Whether `raw` reads as this number, exactly as the accessor parses it.
    fn reads(self, raw: &str) -> bool {
        match self {
            Number::U16 => raw.parse::<u16>().is_ok(),
            Number::U32 => raw.parse::<u32>().is_ok(),
            Number::U64 => raw.parse::<u64>().is_ok(),
            Number::Usize => raw.parse::<usize>().is_ok(),
            Number::F64 => raw.parse::<f64>().is_ok(),
        }
    }

    /// The accepted form, in operator words.
    pub fn form(self) -> &'static str {
        match self {
            Number::U16 => "an integer from 0 to 65535",
            Number::U32 => "an integer from 0 to 4294967295",
            Number::U64 | Number::Usize => "a non-negative integer",
            Number::F64 => "a number",
        }
    }
}

/// Every numeric env var `config_access` reads, and its number type.
pub static NUMERIC_ENV_VARS: &[(&str, Number)] = &[
    ("DARKMUX_FLEET_LISTENER_PORT", Number::U16),
    ("DARKMUX_REDIS_MAXLEN", Number::Usize),
    ("DARKMUX_REDIS_TELEMETRY_MAXLEN", Number::Usize),
    ("DARKMUX_CLIENTS_PREVIEW_IDLE_MINUTES", Number::U64),
    ("DARKMUX_HOOKS_MAX_OUTBOX_MB", Number::U64),
    ("DARKMUX_HOOKS_JQ_TIMEOUT_MS", Number::U64),
    ("DARKMUX_HOOKS_JQ_MAX_OUTPUT_BYTES", Number::U64),
    ("DARKMUX_SERVE_PORT", Number::U16),
    ("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", Number::U64),
    ("DARKMUX_MODEL_LOAD_TIMEOUT_SECONDS", Number::U64),
    ("DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS", Number::U64),
    ("DARKMUX_MISSION_WALL_CLOCK_TIMEOUT_SECONDS", Number::U64),
    ("DARKMUX_DISPATCH_FREE_CONCURRENCY", Number::U32),
    ("DARKMUX_LOCAL_DISPATCH_CONCURRENCY", Number::U32),
    ("DARKMUX_RUNTIME_MAX_TURNS", Number::U32),
    ("DARKMUX_RUNTIME_MAX_TOKENS", Number::U32),
    ("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", Number::U32),
    ("DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL", Number::U32),
    (
        "DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL",
        Number::U32,
    ),
    ("DARKMUX_RUNTIME_MAX_STALL_RECOVERIES", Number::U32),
    ("DARKMUX_RADIO_HUMOR", Number::U64),
    ("DARKMUX_ACP_IDLE_EXIT_MINUTES", Number::U64),
    ("DARKMUX_INJECTED_CONTEXT_FRACTION", Number::F64),
    ("DARKMUX_TURN_DELAY_MS", Number::U64),
    ("DARKMUX_HOST_SAMPLER_INTERVAL_MS", Number::U64),
    ("DARKMUX_LIVE_SAMPLE_MS", Number::U64),
    ("DARKMUX_THERMAL_RESUME_HOLD_MS", Number::U64),
    ("DARKMUX_THERMAL_MAX_PAUSE_MS", Number::U64),
    ("DARKMUX_THERMAL_MIN_CPU_SPEED_LIMIT_PCT", Number::U64),
    ("DARKMUX_THERMAL_SPEED_LIMIT_HOLD_SAMPLES", Number::U32),
    ("DARKMUX_THERMAL_DUTY_DELAY_MS", Number::U64),
    ("DARKMUX_THERMAL_RATCHET_FACTOR", Number::U32),
    ("DARKMUX_THERMAL_EPISODE_THRESHOLD", Number::U32),
    ("DARKMUX_POWER_MIN_BATTERY_PCT", Number::U64),
    ("DARKMUX_MISSION_STALE_ACTIVE_DAYS", Number::U64),
    ("DARKMUX_LIVENESS_RETENTION_HOURS", Number::U64),
];

/// What kind of value `raw` is, never the value: a number (out of range or
/// fractional), a boolean, null, a quoted string, a list, an object, or
/// plain text.
fn kind_of(raw: &str) -> &'static str {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Number(_)) => "a number",
        Ok(v) => crate::user_files::json_type(&v),
        Err(_) => "text",
    }
}

/// The refusal of `var`, read as `number`, set to `raw`; `None` when it
/// reads, or is blank (unset).
pub fn refusal(var: &'static str, number: Number, raw: &str) -> Option<InvalidSetting> {
    let raw = raw.trim();
    if raw.is_empty() || number.reads(raw) {
        return None;
    }
    Some(InvalidSetting {
        set_in: SetIn::Env(var),
        problem: format!("set to {} that is not {}", kind_of(raw), number.form()),
        valid: format!("{}; or unset {var} to use config.json", number.form()),
    })
}

/// Every numeric env var `env` sets to a value that does not read.
pub fn bad_in(env: impl Fn(&str) -> Option<String>) -> Vec<InvalidSetting> {
    NUMERIC_ENV_VARS
        .iter()
        .filter_map(|(var, number)| refusal(var, *number, &env(var)?))
        .collect()
}

/// [`bad_in`] this process's environment: what every preflight refuses and
/// doctor fails.
pub fn bad_values() -> Vec<InvalidSetting> {
    bad_in(crate::config_access::env_str)
}

/// What a command that runs no preflight prints about each [`bad_values`]
/// entry: it reads the var as unset and runs, and a command that starts
/// work refuses. Never the value.
pub fn notices() -> Vec<String> {
    bad_values().iter().map(notice).collect()
}

fn notice(bad: &InvalidSetting) -> String {
    format!(
        "{bad}\n  read as unset here, so config.json or the default applies; a command that starts work \
         refuses until it is fixed (`darkmux doctor` names the fix)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Every `pick_parsed`/`pick_parsed_with_source` call in
    /// `config_access`'s production code that names its env var as a
    /// literal, and every `env_str("..")` parsed as a number in the same
    /// statement: the var, and the number type a turbofish names, if any.
    fn accessor_reads() -> BTreeMap<String, Option<String>> {
        let src = include_str!("config_access.rs");
        let src = src
            .find("#[cfg(test)]\nmod tests")
            .map_or(src, |i| &src[..i]);
        let mut out = BTreeMap::new();
        for (at, _) in src.match_indices("env_str(\"") {
            let rest = &src[at + "env_str(\"".len()..];
            let Some((var, tail)) = rest.split_once('"') else {
                continue;
            };
            let statement = tail.split([';', '{']).next().unwrap_or_default();
            if let Some((_, t)) = statement.split_once(".parse::<") {
                out.insert(var.to_string(), t.split('>').next().map(str::to_string));
            }
        }
        for (at, _) in src.match_indices("pick_parsed") {
            let rest = &src[at + "pick_parsed".len()..];
            let rest = rest.strip_prefix("_with_source").unwrap_or(rest);
            let (turbofish, rest) = match rest.strip_prefix("::<").and_then(|r| r.split_once('>')) {
                Some((t, r)) => (Some(t.to_string()), r),
                None => (None, rest),
            };
            let Some(arg) = rest
                .strip_prefix('(')
                .map(str::trim_start)
                .and_then(|r| r.strip_prefix('"'))
            else {
                continue;
            };
            let var = arg.split('"').next().unwrap_or_default();
            out.insert(var.to_string(), turbofish);
        }
        out
    }

    fn rust_name(n: Number) -> &'static str {
        match n {
            Number::U16 => "u16",
            Number::U32 => "u32",
            Number::U64 => "u64",
            Number::Usize => "usize",
            Number::F64 => "f64",
        }
    }

    /// The registry and the accessors agree: every var a numeric accessor
    /// reads is registered, every registered var is read, and a turbofish
    /// names the registered type.
    #[test]
    fn every_numeric_accessor_reads_a_registered_var() {
        let read = accessor_reads();
        assert!(read.len() >= 30, "the scan finds the accessors: {read:?}");
        let registered: BTreeMap<&str, Number> = NUMERIC_ENV_VARS.iter().copied().collect();
        assert_eq!(
            registered.len(),
            NUMERIC_ENV_VARS.len(),
            "a var registered twice"
        );
        for (var, turbofish) in &read {
            let number = registered.get(var.as_str()).unwrap_or_else(|| {
                panic!("{var} is read by a numeric accessor but not in NUMERIC_ENV_VARS")
            });
            if let Some(t) = turbofish {
                assert_eq!(t, rust_name(*number), "{var}");
            }
        }
        for var in registered.keys() {
            assert!(
                read.contains_key(*var),
                "{var} is registered but no numeric accessor reads it"
            );
        }
    }

    #[test]
    fn a_value_that_reads_or_is_blank_is_not_refused() {
        for (number, raw) in [
            (Number::U16, "65535"),
            (Number::U32, " 7 "),
            (Number::U64, "0"),
            (Number::Usize, "10000"),
            (Number::F64, "0.25"),
            (Number::U64, "  "),
        ] {
            assert_eq!(
                refusal("DARKMUX_X", number, raw),
                None,
                "{number:?} {raw:?}"
            );
        }
    }

    /// The refusal says what kind of value it is and what is accepted, and
    /// never quotes the value.
    #[test]
    fn a_refusal_names_the_kind_and_the_form_never_the_value() {
        for (number, raw, kind) in [
            (Number::U32, "50k", "text"),
            (Number::U16, "70000", "a number"),
            (Number::U64, "-5", "a number"),
            (Number::U32, "1.5", "a number"),
            (Number::U64, "true", "a boolean"),
            (Number::U64, "\"s3cret\"", "a string"),
            (Number::F64, "half", "text"),
        ] {
            let r =
                refusal("DARKMUX_X", number, raw).unwrap_or_else(|| panic!("{raw:?} is refused"));
            let line = r.to_string();
            assert_eq!(r.set_in, SetIn::Env("DARKMUX_X"));
            assert!(
                line.contains(&format!("set to {kind} that is not {}", number.form())),
                "{line}"
            );
            assert!(line.contains("unset DARKMUX_X"), "{line}");
            assert!(
                !line.contains(raw.trim_matches('"')),
                "never the value: {line}"
            );
        }
    }

    #[test]
    fn bad_in_reads_every_registered_var_through_env() {
        let env = |var: &str| match var {
            "DARKMUX_SERVE_PORT" => Some("http".to_string()),
            "DARKMUX_RUNTIME_MAX_TURNS" => Some("40".to_string()),
            _ => None,
        };
        let bad = bad_in(env);
        assert_eq!(bad.len(), 1, "{bad:?}");
        assert_eq!(bad[0].set_in, SetIn::Env("DARKMUX_SERVE_PORT"));
    }
}
