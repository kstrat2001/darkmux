//! (#316 stage 4, `docs/architecture/ENGINES.md` section 9.1) `default_engine`:
//! the engine this machine's `engine::local()` answers with.
//!
//! Its values are an open set: `lmstudio`, or the id of an OpenAI-compatible
//! endpoint in `profiles.json`. So it is not a [`crate::config_enum`]
//! setting (those are closed token lists) but the same rule applies (contract
//! 9): a value that names no usable engine is refused by every entry point
//! that would consume it, naming where it was set, the closest valid value
//! and the fix; never resolved to LM Studio. Whether a value is usable
//! depends on the profile registry, which `darkmux-types` cannot locate on its
//! own, so the registry is passed in, the way
//! [`crate::config_enum::invalid_endpoint_limits`] takes it.

use crate::config_access::Source;
use crate::config_enum::{InvalidSetting, SetIn};
use crate::endpoint::{Dialect, EndpointKind};
use crate::ProfileRegistry;

/// The value that names LM Studio, the existing spelling of
/// [`crate::ManagedBackend::Lmstudio`], and what an unset setting means.
pub const LMSTUDIO: &str = "lmstudio";
/// The `config.json` key.
pub const KEY: &str = "default_engine";
/// The env var that wins over it.
pub const ENV: &str = "DARKMUX_DEFAULT_ENGINE";

/// The engine a `default_engine` value names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineChoice {
    /// LM Studio, driven through `lms`.
    Lmstudio,
    /// The OpenAI-compatible server at `endpoints.<id>` in `profiles.json`.
    Endpoint(String),
}

/// What `raw` (set in the tier `source`) names, checked against `reg`.
/// `lmstudio` is matched trimmed and case-insensitively, as an enum token
/// is; an endpoint id exactly, after trimming, as profiles name it. An
/// endpoint must be one darkmux only sends requests to and that speaks an
/// OpenAI-compatible dialect: a managed endpoint is LM Studio itself, and a
/// Messages endpoint is a hosted API, not this machine's engine.
pub fn resolve(
    raw: &str,
    source: Source,
    reg: &ProfileRegistry,
) -> Result<EngineChoice, InvalidSetting> {
    let value = raw.trim();
    if value.eq_ignore_ascii_case(LMSTUDIO) {
        return Ok(EngineChoice::Lmstudio);
    }
    let refuse = |problem: String| InvalidSetting {
        set_in: set_in(source),
        problem,
        valid: valid_line(reg),
    };
    let Some(ep) = reg.endpoints.get(value) else {
        let candidates = std::iter::once(LMSTUDIO).chain(eligible(reg));
        let hint = crate::user_files::closest(value, candidates)
            .map(|c| format!("; the closest is `{c}`"))
            .unwrap_or_default();
        return Err(refuse(format!(
            "`{value}` is neither `{LMSTUDIO}` nor an endpoint id in profiles.json{hint}"
        )));
    };
    match (ep.kind(), ep.resolved_dialect()) {
        (Ok(EndpointKind::Managed(_)), _) => Err(refuse(format!(
            "endpoint `{value}` is managed by LM Studio; name that engine `{LMSTUDIO}`"
        ))),
        (Ok(EndpointKind::Unmanaged), Ok(Dialect::Messages)) => Err(refuse(format!(
            "endpoint `{value}` speaks Anthropic's Messages API, a hosted API rather than an \
             OpenAI-compatible server on this machine"
        ))),
        (Ok(EndpointKind::Unmanaged), Ok(_)) => Ok(EngineChoice::Endpoint(value.to_string())),
        (Err(e), _) | (_, Err(e)) => Err(refuse(format!("endpoint `{value}` cannot be used: {e}"))),
    }
}

/// [`resolve`] over the live tiers (`config_access::default_engine_with_source`).
pub fn current(reg: &ProfileRegistry) -> Result<EngineChoice, InvalidSetting> {
    let (raw, source) = crate::config_access::default_engine_with_source();
    resolve(&raw, source, reg)
}

/// The preflight pass: the live value's refusal, if it has one.
pub fn invalid(reg: &ProfileRegistry) -> Vec<InvalidSetting> {
    current(reg).err().into_iter().collect()
}

/// [`resolve`] when the profile registry could not be loaded (`load_error`
/// says why): `lmstudio` needs no registry; any other value cannot be checked
/// and is refused naming the load error, never blamed for it.
pub fn resolve_without_registry(
    raw: &str,
    source: Source,
    load_error: &str,
) -> Result<EngineChoice, InvalidSetting> {
    if raw.trim().eq_ignore_ascii_case(LMSTUDIO) {
        return Ok(EngineChoice::Lmstudio);
    }
    Err(InvalidSetting {
        set_in: set_in(source),
        problem: format!(
            "`{}` names an endpoint, but the profile registry that defines it could not be loaded: {load_error}",
            raw.trim()
        ),
        valid: valid_line(&ProfileRegistry::default()),
    })
}

/// The preflight pass when the registry could not be loaded.
pub fn invalid_without_registry(load_error: &str) -> Vec<InvalidSetting> {
    let (raw, source) = crate::config_access::default_engine_with_source();
    resolve_without_registry(&raw, source, load_error)
        .err()
        .into_iter()
        .collect()
}

/// The command that changes the value where it was set.
pub fn fix(set_in: &SetIn) -> String {
    match set_in {
        SetIn::Env(var) => format!("export {var}=<value>, or unset {var} to use config.json"),
        _ => format!("darkmux config set {KEY} <value>"),
    }
}

fn set_in(source: Source) -> SetIn {
    match source {
        Source::Env => SetIn::Env(ENV),
        Source::Config | Source::BuiltIn => SetIn::Config(KEY.to_string()),
    }
}

/// The endpoint ids [`resolve`] accepts, in registry order.
fn eligible(reg: &ProfileRegistry) -> impl Iterator<Item = &str> {
    reg.endpoints.iter().filter_map(|(id, ep)| {
        let usable = matches!(ep.kind(), Ok(EndpointKind::Unmanaged))
            && matches!(ep.resolved_dialect(), Ok(d) if d != Dialect::Messages);
        usable.then_some(id.as_str())
    })
}

fn valid_line(reg: &ProfileRegistry) -> String {
    let ids: Vec<String> = eligible(reg).map(|id| format!("`{id}`")).collect();
    let ids = if ids.is_empty() {
        "none defined".to_string()
    } else {
        ids.join(", ")
    };
    format!("`{LMSTUDIO}`, or the id of an OpenAI-compatible endpoint in profiles.json ({ids})")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> ProfileRegistry {
        let mut reg: ProfileRegistry = serde_json::from_value(serde_json::json!({
            "profiles": {"p": {"models": [{"id": "m", "n_ctx": 1}]}},
            "endpoints": {
                "mlx": {"url": "http://127.0.0.1:8080/v1"},
                "lms": {"managed": "lmstudio"},
                "claude": {"url": "https://api.anthropic.com/v1"},
                "odd": {"url": "http://127.0.0.1:9/v1", "dialect": "smoke-signals"},
            },
        }))
        .unwrap();
        reg.materialize_endpoints();
        reg
    }

    #[test]
    fn lmstudio_in_any_case_or_padding_is_lm_studio() {
        for raw in ["lmstudio", " LMStudio ", "LMSTUDIO"] {
            assert_eq!(
                resolve(raw, Source::Config, &reg()),
                Ok(EngineChoice::Lmstudio),
                "{raw}"
            );
        }
    }

    #[test]
    fn an_openai_compatible_endpoint_id_is_that_endpoint() {
        assert_eq!(
            resolve(" mlx ", Source::Env, &reg()),
            Ok(EngineChoice::Endpoint("mlx".into()))
        );
    }

    /// Never defaulted to LM Studio: an unknown value is refused where it was
    /// set, with the closest valid value and the valid list.
    #[test]
    fn an_unknown_value_is_refused_where_it_was_set_with_the_closest_value() {
        let err = resolve("mxl", Source::Config, &reg()).unwrap_err();
        assert_eq!(err.set_in, SetIn::Config(KEY.into()));
        assert_eq!(
            err.problem,
            "`mxl` is neither `lmstudio` nor an endpoint id in profiles.json; the closest is `mlx`"
        );
        assert_eq!(
            err.valid,
            "`lmstudio`, or the id of an OpenAI-compatible endpoint in profiles.json (`mlx`)"
        );
        let err = resolve("lmstuido", Source::Env, &reg()).unwrap_err();
        assert_eq!(err.set_in, SetIn::Env(ENV));
        assert!(
            err.problem.ends_with("the closest is `lmstudio`"),
            "{}",
            err.problem
        );
    }

    /// Ids are names: matched exactly, never folded the way `lmstudio` is.
    #[test]
    fn an_endpoint_id_is_matched_exactly() {
        assert!(resolve("MLX", Source::Config, &reg()).is_err());
    }

    #[test]
    fn a_managed_a_messages_or_an_unusable_endpoint_is_refused_naming_why() {
        let problem = |raw| resolve(raw, Source::Config, &reg()).unwrap_err().problem;
        assert_eq!(
            problem("lms"),
            "endpoint `lms` is managed by LM Studio; name that engine `lmstudio`"
        );
        assert!(
            problem("claude").contains("Messages API"),
            "{}",
            problem("claude")
        );
        assert!(
            problem("odd").starts_with("endpoint `odd` cannot be used: "),
            "{}",
            problem("odd")
        );
    }

    #[test]
    fn with_no_usable_endpoint_the_valid_line_says_so() {
        let empty = ProfileRegistry::default();
        assert_eq!(
            resolve("mlx", Source::Config, &empty).unwrap_err().valid,
            "`lmstudio`, or the id of an OpenAI-compatible endpoint in profiles.json (none defined)"
        );
    }

    /// A registry that cannot be loaded is named as the cause, never the
    /// value: a typo in profiles.json must not read as a bad engine id.
    #[test]
    fn without_a_registry_only_lmstudio_passes_and_the_load_error_is_named() {
        assert_eq!(
            resolve_without_registry(" LMStudio ", Source::Config, "e"),
            Ok(EngineChoice::Lmstudio)
        );
        let err =
            resolve_without_registry("mlx", Source::Env, "expected `,` at line 3").unwrap_err();
        assert_eq!(err.set_in, SetIn::Env(ENV));
        assert_eq!(
            err.problem,
            "`mlx` names an endpoint, but the profile registry that defines it could not be loaded: \
             expected `,` at line 3"
        );
    }

    #[test]
    fn the_fix_names_the_tier_that_set_it() {
        assert_eq!(
            fix(&SetIn::Config(KEY.into())),
            "darkmux config set default_engine <value>"
        );
        assert_eq!(
            fix(&SetIn::Env(ENV)),
            "export DARKMUX_DEFAULT_ENGINE=<value>, or unset DARKMUX_DEFAULT_ENGINE to use config.json"
        );
    }

    /// The live tiers: env over config over `lmstudio`, and the preflight pass
    /// reports a bad value from either.
    #[test]
    #[serial_test::serial]
    fn the_live_value_resolves_env_over_config_over_lmstudio() {
        unsafe { std::env::remove_var(ENV) };
        assert_eq!(current(&reg()), Ok(EngineChoice::Lmstudio));
        let cfg = crate::config::DarkmuxConfig {
            default_engine: Some("nope".into()),
            ..Default::default()
        };
        let _guard = crate::config_access::set_config_for_test(cfg);
        assert_eq!(invalid(&reg())[0].set_in, SetIn::Config(KEY.into()));
        unsafe { std::env::set_var(ENV, "mlx") };
        let env_wins = current(&reg());
        unsafe { std::env::remove_var(ENV) };
        assert_eq!(env_wins, Ok(EngineChoice::Endpoint("mlx".into())));
    }
}
