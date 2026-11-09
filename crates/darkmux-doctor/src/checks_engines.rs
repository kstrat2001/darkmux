//! (#3448) The doctor rows for the engines darkmux starts servers for:
//! Tensorfold, and rec's Flint (#3453). It asks the installed binary only read-only questions
//! (`--version`, `capabilities --json`, `models --json`), and only when the
//! profile registry declares a Tensorfold endpoint.

use crate::{Check, Status};
use darkmux_profiles::engine::flint::{Rec, VERIFIED_RELEASE};
use darkmux_profiles::engine::tensorfold::{Facts, ServeFlags, Tensorfold, Version, VERIFIED_RELEASES};
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
        // The same facts a profile listing's availability reads (#3466).
        let Facts { version, flags, models } = Facts::probe(&Tensorfold::at(bin.clone()));
        TensorfoldFacts { bin, source, version, flags, models }
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
/// it does not accept: the per-model checks a listing's availability makes
/// too ([`Facts::has_model`], [`Facts::args_problem`]).
fn model_problems(models: &[TfModel], facts: &TensorfoldFacts) -> Vec<String> {
    let shared = Facts { version: facts.version.clone(), flags: facts.flags.clone(), models: facts.models.clone() };
    let mut problems = Vec::new();
    match &facts.models {
        Ok(_) => problems.extend(models.iter().filter(|(_, _, m)| shared.has_model(m) == Ok(false)).map(|(p, i, m)| {
            format!("profiles.{p}.models[{i}] names `{}`, which `tensorfold-native models` does not list", m.id)
        })),
        Err(e) => problems.push(format!("could not list its models: {e}")),
    }
    match &facts.flags {
        Ok(_) => problems.extend(models.iter().filter_map(|(p, i, m)| {
            shared.args_problem(m).ok().flatten().map(|why| format!("profiles.{p}.models[{i}].engine_args: {why}"))
        })),
        Err(e) => problems.push(format!("could not read its serve flags: {e}")),
    }
    problems
}

// ─── (#3453) rec's Flint ─────────────────────────────────────────────────────

const FLINT_NAME: &str = "flint engine";

/// What rec's install said, each answer kept apart.
pub(crate) struct FlintFacts {
    pub bin: String,
    pub source: Source,
    pub version: Result<String, String>,
    pub runtime: Result<(), String>,
    /// For each Flint profile model's `id`, whether rec's install holds it.
    pub models: Vec<Result<(), String>>,
    /// The address of rec's own server, when it is up.
    pub own_server: Result<Option<String>, String>,
}

impl FlintFacts {
    fn probe(ids: &[&str]) -> Self {
        let (bin, source) = darkmux_types::config_access::flint_bin_with_source();
        let rec = Rec::installed();
        let msg = |e: anyhow::Error| format!("{e:#}");
        FlintFacts {
            bin,
            source,
            version: rec.version().map_err(msg),
            runtime: rec.runtime().map(|_| ()).map_err(msg),
            models: ids.iter().map(|id| rec.model_dir(id).map(|_| ()).map_err(msg)).collect(),
            own_server: rec.own_server().map_err(msg),
        }
    }
}

pub(crate) fn check_flint_engine() -> Check {
    match darkmux_profiles::profiles::load_registry_quiet(None) {
        Ok(loaded) => flint_engine_status(&loaded.registry, FlintFacts::probe),
        Err(_) => Check { name: FLINT_NAME.into(), status: Status::Pass, message: "not checked: the profile registry does not load".into(), hint: None },
    }
}

fn is_flint(kind: Result<EndpointKind, darkmux_types::EndpointError>) -> bool {
    matches!(kind, Ok(EndpointKind::Managed(ManagedBackend::Flint)))
}

fn flint_row(status: Status, message: String, hint: Option<String>) -> Check {
    Check { name: FLINT_NAME.into(), status, message, hint }
}

/// The row, from the registry and (only when it declares a Flint endpoint)
/// what rec's install says. Asks only read-only questions: `rec --version`,
/// `rec status`, and which runtime and model directories the install holds.
pub(crate) fn flint_engine_status(registry: &ProfileRegistry, probe: impl FnOnce(&[&str]) -> FlintFacts) -> Check {
    if !registry.endpoints.values().any(|ep| is_flint(ep.kind())) {
        return flint_row(Status::Pass, "not checked: no endpoint declares `\"managed\": \"flint\"`".into(), None);
    }
    let models: Vec<(String, usize, &str)> = registry
        .profiles
        .iter()
        .flat_map(|(p, prof)| prof.models.iter().enumerate().map(move |(i, m)| (p.clone(), i, m)))
        .filter(|(_, _, m)| is_flint(m.endpoint_kind()))
        .map(|(p, i, m)| (p, i, m.id.as_str()))
        .collect();
    let ids: Vec<&str> = models.iter().map(|(_, _, id)| *id).collect();
    let facts = probe(&ids);
    let via = match facts.source {
        Source::Env => " (via DARKMUX_FLINT_BIN)",
        Source::Config => " (via config.engines.flint.bin)",
        Source::BuiltIn => "",
    };
    let version = match flint_verified(&facts, via) {
        Ok(v) => v,
        Err(row) => return row,
    };
    let lead = format!("rec {version} (`{}`{via})", facts.bin);
    let problems = flint_install_problems(&models, &facts);
    if !problems.is_empty() {
        return flint_row(
            Status::Fail,
            format!("{lead}: {}", problems.join("; ")),
            Some("`rec pull` installs the runtime and the model; a profile model's `id` names a revision rec holds \
                  (its manifest's name, such as `flint-int4`)"
                .into()),
        );
    }
    flint_server_row(&lead, &facts, models.len())
}

/// rec's version when it is the verified release, else the failing row.
fn flint_verified(facts: &FlintFacts, via: &str) -> Result<String, Check> {
    match &facts.version {
        Err(e) => Err(flint_row(
            Status::Fail,
            format!("`{}`{via} did not answer: {e}", facts.bin),
            Some("install rec, then `darkmux config set engines.flint.bin <path to rec>` if it is not on PATH; \
                  DARKMUX_FLINT_BIN overrides it for one shell"
                .into()),
        )),
        Ok(v) if v != VERIFIED_RELEASE => Err(flint_row(
            Status::Fail,
            format!("`{}`{via} is rec {v}; this darkmux supports rec {VERIFIED_RELEASE}", facts.bin),
            Some(format!("install rec {VERIFIED_RELEASE}, or a darkmux that supports rec {v}")),
        )),
        Ok(v) => Ok(v.clone()),
    }
}

/// A missing runtime, and every Flint model rec's install does not hold.
fn flint_install_problems(models: &[(String, usize, &str)], facts: &FlintFacts) -> Vec<String> {
    let mut problems: Vec<String> = facts.runtime.as_ref().err().cloned().into_iter().collect();
    for ((p, i, _), held) in models.iter().zip(&facts.models) {
        if let Err(e) = held {
            problems.push(format!("profiles.{p}.models[{i}]: {e}"));
        }
    }
    problems
}

/// The row once rec's install is good: a warning while rec's own server runs.
fn flint_server_row(lead: &str, facts: &FlintFacts, models: usize) -> Check {
    match &facts.own_server {
        Ok(Some(address)) => flint_row(
            Status::Warn,
            format!("{lead}: rec's own server is up or starting at {address}"),
            Some("every Flint dispatch is refused (`engine_in_use`) while it runs: darkmux never stops or takes over \
                  rec's server. Run `rec stop` to let darkmux start its own"
                .into()),
        ),
        Err(e) => flint_row(Status::Warn, format!("{lead}: `rec status` did not answer: {e}"), None),
        Ok(None) => flint_row(
            Status::Pass,
            format!(
                "{lead}: {models} model(s), each one rec's install holds; darkmux starts rec's server when a dispatch \
                 needs it"
            ),
            None,
        ),
    }
}

#[cfg(test)]
mod flint_tests {
    use super::*;

    fn registry(json: &str) -> ProfileRegistry {
        let mut r: ProfileRegistry = serde_json::from_str(json).unwrap();
        r.materialize_endpoints();
        r
    }

    const REG: &str = r#"{"profiles":{"f":{"models":[{"id":"flint-int4","n_ctx":8192,"endpoint":"fl"}]}},
        "endpoints":{"fl":{"managed":"flint","port":8092}}}"#;

    fn facts(version: &str) -> FlintFacts {
        FlintFacts {
            bin: "rec".into(),
            source: Source::BuiltIn,
            version: Ok(version.into()),
            runtime: Ok(()),
            models: vec![Ok(())],
            own_server: Ok(None),
        }
    }

    /// (#3453) No Flint endpoint: rec is never asked.
    #[test]
    fn with_no_flint_endpoint_rec_is_never_asked() {
        let r = registry(r#"{"profiles":{"p":{"models":[{"id":"m","n_ctx":8}]}}}"#);
        let c = flint_engine_status(&r, |_| panic!("no Flint endpoint, so no probe"));
        assert_eq!((c.status, c.message.contains("not checked")), (Status::Pass, true), "{}", c.message);
    }

    /// (#3453) A verified rec holding the runtime and every Flint model
    /// passes; rec's own server running warns, naming `rec stop`.
    #[test]
    fn a_verified_rec_that_holds_the_model_passes_and_its_own_server_warns() {
        let c = flint_engine_status(&registry(REG), |ids| {
            assert_eq!(ids, ["flint-int4"]);
            facts("0.1.0")
        });
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.message.contains("rec 0.1.0") && c.message.contains("1 model(s)"), "{}", c.message);
        let mut up = facts("0.1.0");
        up.own_server = Ok(Some("http://127.0.0.1:11500/v1".into()));
        let c = flint_engine_status(&registry(REG), |_| up);
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert!(c.hint.unwrap().contains("rec stop"));
    }

    /// (#3453) No rec, another release, no runtime or a revision rec does
    /// not hold fails, naming the model's path.
    #[test]
    fn a_missing_rec_an_unverified_release_or_a_missing_model_fails() {
        let mut gone = facts("0.1.0");
        gone.version = Err("`rec --version` failed: not found".into());
        let c = flint_engine_status(&registry(REG), |_| gone);
        assert_eq!(c.status, Status::Fail);
        assert!(c.hint.unwrap().contains("engines.flint.bin"));
        let c = flint_engine_status(&registry(REG), |_| facts("0.2.0"));
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("rec 0.2.0") && c.message.contains(VERIFIED_RELEASE), "{}", c.message);
        let mut missing = facts("0.1.0");
        missing.models = vec![Err("no model whose manifest is `flint-int4.json`".into())];
        let c = flint_engine_status(&registry(REG), |_| missing);
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("profiles.f.models[0]") && c.hint.unwrap().contains("rec pull"), "{}", c.message);
    }
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
