//! (#3448) The doctor row for the engines darkmux starts servers for:
//! Tensorfold first. It asks the installed binary only read-only questions
//! (`--version`, `capabilities --json`, `models --json`), and only when the
//! profile registry declares a Tensorfold endpoint.

use crate::{Check, Status};
use darkmux_profiles::engine::tensorfold::{ServeFlags, Tensorfold, Version, VERIFIED_RELEASES};
use darkmux_types::config_access::Source;
use darkmux_types::{EndpointKind, ManagedBackend, ProfileRegistry};

const NAME: &str = "tensorfold engine";

/// What the installed binary said, each answer kept apart so one failed probe
/// does not hide the others.
pub(crate) struct TensorfoldFacts {
    pub bin: String,
    pub source: Source,
    pub version: Result<Version, String>,
    pub flags: Result<ServeFlags, String>,
    pub models: Result<Vec<String>, String>,
}

impl TensorfoldFacts {
    fn probe() -> Self {
        let (bin, source) = darkmux_types::config_access::tensorfold_bin_with_source();
        let tf = Tensorfold::at(bin.clone());
        let msg = |e: anyhow::Error| format!("{e:#}");
        TensorfoldFacts {
            bin,
            source,
            version: tf.version().map_err(msg),
            flags: tf.serve_flags().map_err(msg),
            models: tf.models().map(|ms| ms.into_iter().map(|m| m.repo).collect()).map_err(msg),
        }
    }
}

pub(crate) fn check_tensorfold_engine() -> Check {
    match darkmux_profiles::profiles::load_registry_quiet(None) {
        Ok(loaded) => tensorfold_engine_status(&loaded.registry, TensorfoldFacts::probe),
        // The `profile registry` row reports why it does not load.
        Err(_) => Check { name: NAME.into(), status: Status::Pass, message: "not checked: the profile registry does not load".into(), hint: None },
    }
}

/// A profile model on a Tensorfold endpoint, with where it is declared.
type TfModel<'a> = (String, usize, &'a darkmux_types::ProfileModel);

fn is_tf(kind: Result<EndpointKind, darkmux_types::EndpointError>) -> bool {
    matches!(kind, Ok(EndpointKind::Managed(ManagedBackend::Tensorfold)))
}

fn pass(message: String) -> Check {
    Check { name: NAME.into(), status: Status::Pass, message, hint: None }
}

fn fail(message: String, hint: String) -> Check {
    Check { name: NAME.into(), status: Status::Fail, message, hint: Some(hint) }
}

/// The row, from the registry and (only when it declares a Tensorfold
/// endpoint) what the binary says.
pub(crate) fn tensorfold_engine_status(registry: &ProfileRegistry, probe: impl FnOnce() -> TensorfoldFacts) -> Check {
    let endpoints = registry.endpoints.values().filter(|ep| is_tf(ep.kind())).count();
    if endpoints == 0 {
        return pass("not checked: no endpoint declares `\"managed\": \"tensorfold\"`".into());
    }
    let models: Vec<TfModel> = registry
        .profiles
        .iter()
        .flat_map(|(p, prof)| prof.models.iter().enumerate().map(move |(i, m)| (p.clone(), i, m)))
        .filter(|(_, _, m)| is_tf(m.endpoint_kind()))
        .collect();
    let facts = probe();
    let via = match facts.source {
        Source::Env => " (via DARKMUX_TENSORFOLD_BIN)",
        Source::Config => " (via config.engines.tensorfold.bin)",
        Source::BuiltIn => "",
    };
    let version = match verified(&facts, via) {
        Ok(v) => v,
        Err(row) => return row,
    };
    let problems = model_problems(&models, &facts);
    if !problems.is_empty() {
        return fail(
            format!("Tensorfold {version} (`{}`{via}): {}", facts.bin, problems.join("; ")),
            "download a missing model with `tensorfold-native pull <repo>` or fix the profile's `id`; drop or \
             correct an `engine_args` flag the engine does not accept (`tensorfold-native capabilities --json` lists them)"
                .into(),
        );
    }
    pass(format!(
        "Tensorfold {version} (`{}`{via}): {} model(s) on {endpoints} endpoint(s), each one the engine has, every \
         `engine_args` flag accepted; darkmux starts each server when a dispatch needs it",
        facts.bin,
        models.len()
    ))
}

/// The installed version when darkmux was verified against it, else the
/// failing row.
fn verified(facts: &TensorfoldFacts, via: &str) -> Result<Version, Check> {
    match &facts.version {
        Err(e) => Err(fail(
            format!("`{}`{via} did not answer: {e}", facts.bin),
            "install Tensorfold, then `darkmux config set engines.tensorfold.bin <path to tensorfold-native>` \
             if it is not on PATH; DARKMUX_TENSORFOLD_BIN overrides it for one shell"
                .into(),
        )),
        Ok(v) if !v.is_verified() => Err(fail(
            format!("`{}`{via} is Tensorfold {v}; this darkmux supports {VERIFIED_RELEASES}", facts.bin),
            format!("install a Tensorfold {VERIFIED_RELEASES} release, or a darkmux that supports {v}"),
        )),
        Ok(v) => Ok(*v),
    }
}

/// Every Tensorfold model the engine does not have, and every `engine_args`
/// it does not accept.
fn model_problems(models: &[TfModel], facts: &TensorfoldFacts) -> Vec<String> {
    let mut problems = Vec::new();
    match &facts.models {
        Ok(have) => problems.extend(models.iter().filter(|(_, _, m)| !have.iter().any(|r| r == &m.id)).map(|(p, i, m)| {
            format!("profiles.{p}.models[{i}] names `{}`, which `tensorfold-native models` does not list", m.id)
        })),
        Err(e) => problems.push(format!("could not list its models: {e}")),
    }
    match &facts.flags {
        Ok(flags) => problems.extend(models.iter().filter(|(_, _, m)| !m.engine_args.is_empty()).filter_map(|(p, i, m)| {
            flags.check(&m.engine_args).err().map(|why| format!("profiles.{p}.models[{i}].engine_args: {why}"))
        })),
        Err(e) => problems.push(format!("could not read its serve flags: {e}")),
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_profiles::engine::tensorfold::{parse_capabilities, parse_version};

    fn registry(json: &str) -> ProfileRegistry {
        let mut r: ProfileRegistry = serde_json::from_str(json).unwrap();
        r.materialize_endpoints();
        r
    }

    const REG: &str = r#"{"profiles":{"q":{"models":[
            {"id":"TensorFold/Qwen3.8-27B-MLX-4bit","n_ctx":8192,"endpoint":"tf","engine_args":["--drafter","z/d"]}]}},
        "endpoints":{"tf":{"managed":"tensorfold","port":8091}}}"#;

    fn facts(version: &str, models: &[&str]) -> TensorfoldFacts {
        TensorfoldFacts {
            bin: "tensorfold-native".into(),
            source: Source::BuiltIn,
            version: parse_version(version).map_err(|e| e.to_string()),
            flags: Ok(parse_capabilities(r#"{"schema":1,"serve":{"flags":{"--drafter":{}}}}"#).unwrap()),
            models: Ok(models.iter().map(|s| s.to_string()).collect()),
        }
    }

    #[test]
    fn with_no_tensorfold_endpoint_the_binary_is_never_asked() {
        let r = registry(r#"{"profiles":{"p":{"models":[{"id":"m","n_ctx":8}]}}}"#);
        let c = tensorfold_engine_status(&r, || panic!("no Tensorfold endpoint, so no probe"));
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("not checked"), "{}", c.message);
    }

    #[test]
    fn a_verified_engine_that_has_the_model_and_accepts_the_args_passes() {
        let c = tensorfold_engine_status(&registry(REG), || facts("tensorfold-native 1.2.4", &["TensorFold/Qwen3.8-27B-MLX-4bit"]));
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.message.contains("1.2.4") && c.message.contains("starts each server"), "{}", c.message);
    }

    #[test]
    fn a_missing_binary_an_unverified_release_a_missing_model_or_a_bad_flag_fails() {
        let mut gone = facts("tensorfold-native 1.2.4", &[]);
        gone.version = Err("`tensorfold-native --version` failed: not found".into());
        let c = tensorfold_engine_status(&registry(REG), || gone);
        assert_eq!(c.status, Status::Fail);
        assert!(c.hint.unwrap().contains("engines.tensorfold.bin"));

        let c = tensorfold_engine_status(&registry(REG), || facts("tensorfold-native 1.3.0", &["TensorFold/Qwen3.8-27B-MLX-4bit"]));
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("1.3.0") && c.message.contains(VERIFIED_RELEASES), "{}", c.message);

        let c = tensorfold_engine_status(&registry(REG), || facts("tensorfold-native 1.2.4", &["other/model"]));
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("profiles.q.models[0]") && c.message.contains("does not list"), "{}", c.message);

        let bad_args = REG.replace(r#"["--drafter","z/d"]"#, r#"["--turbo"]"#);
        let c = tensorfold_engine_status(&registry(&bad_args), || facts("tensorfold-native 1.2.4", &["TensorFold/Qwen3.8-27B-MLX-4bit"]));
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("engine_args") && c.message.contains("--turbo"), "{}", c.message);
    }
}
