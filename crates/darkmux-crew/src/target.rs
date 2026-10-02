//! (#2902 step 3) The one endpoint/model resolver.
//!
//! Every path that turns (role, profile) into "this model, at this endpoint,
//! with this window" goes through [`resolve_in`]: the dispatch's remote
//! branch decision, the container path's model selection and residency, the
//! compaction window, the scheduler's seat placement, radio's boundary and
//! busy checks. Before it there were three resolvers with three copies of
//! the precedence rules, and the compaction window came from the profile's
//! DEFAULT model even when selection picked another one (#2902). A
//! [`Target`] carries the SELECTED model with its own endpoint and its own
//! `n_ctx`, so the two can no longer come apart.
//!
//! The endpoint rules themselves (classification, chat URL, dialect, host,
//! credential source) live on `darkmux_types::ModelEndpoint`; this module
//! applies them to a selection. `step_kinds::endpoint_conformance` fails if
//! a new call site builds a chat URL or decides "remote" on its own.

use anyhow::{bail, Result};
use darkmux_types::{Dialect, EndpointKind, ModelEndpoint, Profile, ProfileModel, ProfileRegistry};

/// A resolved dispatch target: the selected model together with its own
/// endpoint, and the facts every caller needs about where the request goes.
#[derive(Debug, Clone)]
pub struct Target {
    /// The profile the model came from (after `--profile` > `role_profiles`
    /// > `default_profile`).
    pub profile_name: String,
    pub profile: Profile,
    /// The SELECTED model (`select_model`), never simply the profile default.
    pub model: ProfileModel,
    /// The model's endpoint, or the managed default when it declares none.
    pub endpoint: ModelEndpoint,
    pub kind: EndpointKind,
    pub dialect: Dialect,
    /// The chat-completions URL (`ModelEndpoint::chat_url`), resolved once.
    pub chat_url: String,
}

impl Target {
    /// Whether darkmux manages this model's residency.
    pub fn is_managed(&self) -> bool {
        self.kind.is_managed()
    }

    /// The SELECTED model's own declared window: the compaction trigger's
    /// window, and the load size on a managed endpoint.
    pub fn n_ctx(&self) -> Option<u32> {
        self.model.n_ctx
    }

    /// The model id on the wire: darkmux's own namespaced instance on a
    /// managed endpoint (#2240: `darkmux:<key>`, or the profile's explicit
    /// `identifier` opt-out), the model id as written on an unmanaged one.
    pub fn wire_model(&self) -> String {
        match self.kind {
            EndpointKind::Managed(_) => crate::dispatch_internal::managed_wire_model(&self.model),
            EndpointKind::Unmanaged => self.model.id.clone(),
        }
    }

    /// The route label for records (`<protocol>:<host>/<model>`), for an
    /// unmanaged endpoint. `None` on a managed one: its records carry the LM
    /// Studio base instead (`usage::lmstudio_endpoint`).
    pub fn route_label(&self) -> Option<String> {
        (!self.is_managed()).then(|| endpoint_route_label(&self.endpoint, &self.model.id))
    }
}

/// The route label for an unmanaged endpoint and the model requested there:
/// the host (never the path, never credentials) and the model id, through
/// `darkmux_flow::unmanaged_route_label`.
pub fn endpoint_route_label(ep: &ModelEndpoint, model_id: &str) -> String {
    darkmux_flow::unmanaged_route_label(ep.host().as_deref().unwrap_or("remote"), model_id)
}

/// What [`resolve_in`] found.
#[derive(Debug, Clone)]
pub enum Resolution {
    Target(Box<Target>),
    /// No `--profile` match, no `role_profiles` binding and no usable
    /// `default_profile`. The container path refuses to dispatch.
    NoProfile,
    /// A profile resolved but `select_model` returned no model.
    NoModel { profile_name: String, profile: Box<Profile>, error: String },
}

impl Resolution {
    /// The target, or THE error for a resolution that selects nothing:
    /// [`darkmux_profiles::profiles::no_profile_message`] for no profile, the
    /// profile and `select_model`'s reason for no model. (4.0) Both are
    /// fatal for a dispatch; there is no fallback model.
    pub fn require(
        self,
        role_id: &str,
        requested: Option<&str>,
        registry_path: &std::path::Path,
    ) -> Result<Target> {
        match self {
            Resolution::Target(t) => Ok(*t),
            Resolution::NoProfile => bail!(darkmux_profiles::profiles::no_profile_message(
                Some(role_id),
                requested,
                Some(registry_path),
            )),
            Resolution::NoModel { profile_name, error, .. } => bail!(
                "profile `{profile_name}` selects no model for role `{role_id}` ({error}). Add a model \
                 for it to profile `{profile_name}` in {}.",
                registry_path.display()
            ),
        }
    }

    pub fn target(self) -> Option<Target> {
        match self {
            Resolution::Target(t) => Some(*t),
            _ => None,
        }
    }
}

/// (#2905) The `role_profiles.<role>` binding a resolution honors: read live
/// from `config_access`, and only when no explicit `--profile` override was
/// given (the override always wins, so the map is not even consulted).
pub(crate) fn role_profile_binding(role_id: Option<&str>, profile_override: Option<&str>) -> Option<String> {
    match (role_id, profile_override) {
        (Some(role_id), None) => darkmux_types::config_access::role_profile(role_id),
        _ => None,
    }
}

/// (#1547) The profile precedence, in one place: an explicit `--profile`
/// (a name undefined here falls to `default_profile`, the #1054
/// machine-agnostic-caller contract), else the `role_profiles.<role>` binding
/// (`mapped`; a binding naming an undefined profile is a LOUD error,
/// contract 7), else `default_profile`. `mapped` is supplied rather than read
/// so every arm is unit-testable (`config_access` is hard-empty under test
/// builds, #811); production passes [`role_profile_binding`].
pub(crate) fn resolve_role_aware_profile_with<'a>(
    role_id: &str,
    profile_override: Option<&str>,
    mapped: Option<String>,
    registry: &'a ProfileRegistry,
) -> Result<Option<(String, &'a Profile)>> {
    if profile_override.is_none() {
        if let Some(mapped) = mapped {
            let binding = darkmux_profiles::profiles::RoleBinding::Mapped(mapped);
            let resolved = darkmux_profiles::profiles::resolve_role_profile_with(role_id, &binding, registry)?;
            return Ok(Some((resolved.profile_name, resolved.profile)));
        }
    }
    Ok(registry.resolve_active(profile_override).map(|(name, profile)| (name.to_string(), profile)))
}

/// THE resolver: role + profile precedence + `select_model` + the selected
/// model's endpoint, against an already-loaded registry.
///
/// - A quarantined requested (or default) profile is an error (#1282): never
///   a silent fall to a different profile.
/// - The machine utility model is set aside unless `allow_utility_model`
///   (the lab's benchmark opt-in, #2914).
/// - A selected model whose endpoint cannot be classified (an id no
///   `endpoints` entry defines) is an error naming the id: a request is never
///   sent to the managed default on a guess.
pub fn resolve_in(
    registry: &ProfileRegistry,
    role: &crate::types::Role,
    profile_override: Option<&str>,
    mapped: Option<String>,
    allow_utility_model: bool,
) -> Result<Resolution> {
    if let Some(req) = profile_override {
        // (#2916 stage 2) A `profile@machine` address reaching the local
        // resolver is never read as an undefined local name: that would fall
        // to `default_profile` (#1054) and run something else, here.
        if let Some(msg) = darkmux_types::profile_address::local_only_refusal(req, "this dispatch path") {
            bail!(msg);
        }
        if let Some(msg) = registry.quarantine_error_for(req) {
            bail!(msg);
        }
    }
    let Some((profile_name, profile)) =
        resolve_role_aware_profile_with(&role.id, profile_override, mapped, registry)?
    else {
        if let Some(default_name) = registry.default_profile.as_deref() {
            if let Some(msg) = registry.quarantine_error_for(default_name) {
                bail!(msg);
            }
        }
        return Ok(Resolution::NoProfile);
    };
    select_in_profile(registry, role, profile_name, profile, allow_utility_model)
}

/// The lower half of [`resolve_in`], for a caller that resolved the PROFILE
/// by its own precedence (the mission launcher's strict `--param` bindings,
/// which refuse an undefined name rather than falling to the default):
/// `select_model` with the utility set-aside, then the selected model's
/// endpoint.
pub fn select_in_profile(
    registry: &ProfileRegistry,
    role: &crate::types::Role,
    profile_name: String,
    profile: &Profile,
    allow_utility_model: bool,
) -> Result<Resolution> {
    let skill_index: std::collections::HashMap<String, crate::types::Skill> = crate::loader::load_skills()
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.id.clone(), s))
        .collect();
    let set_aside = if allow_utility_model { None } else { registry.utility_model_id() };
    let id = match crate::select::select_model(role, profile, |id| skill_index.get(id), set_aside) {
        Ok(id) => id,
        Err(e) => {
            return Ok(Resolution::NoModel {
                profile_name,
                profile: Box::new(profile.clone()),
                error: e.to_string(),
            })
        }
    };
    let Some(model) = profile.models.iter().find(|m| m.id == id).cloned() else {
        return Ok(Resolution::NoModel {
            profile_name,
            profile: Box::new(profile.clone()),
            error: format!("selected model `{id}` is not in the profile's models[]"),
        });
    };
    Ok(Resolution::Target(Box::new(target_for(profile_name, profile.clone(), model)?)))
}

/// The [`Target`] for an already-selected model.
pub fn target_for(profile_name: String, profile: Profile, model: ProfileModel) -> Result<Target> {
    let endpoint = model.endpoint.clone().unwrap_or_else(ModelEndpoint::managed_lmstudio);
    let kind = endpoint.kind()?;
    let dialect = endpoint.resolved_dialect()?;
    let chat_url = endpoint.chat_url()?;
    Ok(Target { profile_name, profile, model, endpoint, kind, dialect, chat_url })
}

/// A step's `config.endpoint`, resolved, with its kind (#2902 step 3, #3035).
/// An inline definition is used as written; an `endpoints` id is looked up in
/// the registry at `config_path` (one registry read, only for the id form).
/// An undefined id or an endpoint of no declared kind is an error.
fn step_endpoint_of_any_kind(
    endpoint: Option<&crate::step_config::EndpointRef>,
    config_path: Option<&str>,
) -> Result<Option<(ModelEndpoint, EndpointKind)>> {
    use crate::step_config::EndpointRef;
    let Some(endpoint) = endpoint else { return Ok(None) };
    let ep: ModelEndpoint = match endpoint {
        // The registry's own id lookup, the one `materialize_endpoints` uses.
        EndpointRef::Id(id) => darkmux_profiles::profiles::load_registry(config_path)?.registry.endpoint_named(id),
        EndpointRef::Inline(inline) => (**inline).clone(),
    };
    let kind = ep.kind()?;
    Ok(Some((ep, kind)))
}

/// A step's `config.endpoint` when it names an UNMANAGED endpoint (the step
/// kinds' "is this hosted?" test): `Ok(Some(ep))` for the step's hosted arm,
/// `Ok(None)` when absent or managed (the local arm).
pub fn step_unmanaged_endpoint(
    endpoint: Option<&crate::step_config::EndpointRef>,
    config_path: Option<&str>,
) -> Result<Option<ModelEndpoint>> {
    Ok(match step_endpoint_of_any_kind(endpoint, config_path)? {
        Some((ep, EndpointKind::Unmanaged)) => Some(ep),
        Some((_, EndpointKind::Managed(_))) | None => None,
    })
}

/// (#3035) A step's `config.endpoint` when it names a MANAGED endpoint: its
/// local arm still carries that endpoint's limits (the window gate, the
/// per-dispatch cap, the id its usage records are summed by). The inverse of
/// [`step_unmanaged_endpoint`].
pub fn step_managed_endpoint(
    endpoint: Option<&crate::step_config::EndpointRef>,
    config_path: Option<&str>,
) -> Result<Option<ModelEndpoint>> {
    Ok(match step_endpoint_of_any_kind(endpoint, config_path)? {
        Some((ep, EndpointKind::Managed(_))) => Some(ep),
        Some((_, EndpointKind::Unmanaged)) | None => None,
    })
}

#[cfg(test)]
mod step_endpoint_tests {
    use crate::step_config::EndpointRef;
    use serde_json::json;

    fn step_unmanaged_endpoint(
        config: &serde_json::Value,
        path: Option<&str>,
    ) -> anyhow::Result<Option<darkmux_types::ModelEndpoint>> {
        let endpoint: Option<EndpointRef> = config.get("endpoint").map(|v| serde_json::from_value(v.clone()).unwrap());
        super::step_unmanaged_endpoint(endpoint.as_ref(), path)
    }

    /// A step's `config.endpoint`: only an UNMANAGED endpoint takes the
    /// hosted arm; absent and managed ones run locally; an undefined id is
    /// an error (never the managed default).
    #[test]
    fn a_step_endpoint_is_hosted_only_when_unmanaged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let pf = tmp.path().join("profiles.json");
        std::fs::write(
            &pf,
            r#"{"profiles":{"p":{"models":[{"id":"m","n_ctx":1}]}},
                "endpoints":{"hosted":{"url":"https://h.example/v1"},"lms":{"managed":"lmstudio"}}}"#,
        )
        .unwrap();
        let path = pf.to_str();
        assert!(step_unmanaged_endpoint(&json!({}), path).unwrap().is_none());
        let err = step_unmanaged_endpoint(&json!({"endpoint": {}}), path).unwrap_err();
        assert!(format!("{err:#}").contains("neither"), "an endpoint of no declared kind is refused: {err:#}");
        assert!(step_unmanaged_endpoint(&json!({"endpoint": {"managed": "lmstudio"}}), path).unwrap().is_none());
        let inline = step_unmanaged_endpoint(&json!({"endpoint": {"url": "https://i.example/v1"}}), path).unwrap();
        assert_eq!(inline.unwrap().url.as_deref(), Some("https://i.example/v1"));
        let named = step_unmanaged_endpoint(&json!({"endpoint": "hosted"}), path).unwrap().unwrap();
        assert_eq!(named.named_id(), Some("hosted"));
        assert_eq!(named.url.as_deref(), Some("https://h.example/v1"));
        assert!(step_unmanaged_endpoint(&json!({"endpoint": "lms"}), path).unwrap().is_none());
        // (#3035) The managed twin: only a managed endpoint is returned.
        let managed = |config: &serde_json::Value| {
            let endpoint: Option<EndpointRef> = config.get("endpoint").map(|v| serde_json::from_value(v.clone()).unwrap());
            super::step_managed_endpoint(endpoint.as_ref(), path).unwrap()
        };
        assert_eq!(managed(&json!({"endpoint": "lms"})).unwrap().named_id(), Some("lms"));
        assert!(managed(&json!({"endpoint": "hosted"})).is_none() && managed(&json!({})).is_none());
        let err = step_unmanaged_endpoint(&json!({"endpoint": "nope"}), path).unwrap_err();
        assert!(format!("{err:#}").contains("nope"), "{err:#}");
    }
}

#[cfg(test)]
mod resolve_tests {
    fn role() -> crate::types::Role {
        serde_json::from_str(
            r#"{"id":"r","description":"d","tool_palette":{"allow":[],"deny":[]},"escalation_contract":"bail-with-explanation"}"#,
        )
        .unwrap()
    }

    /// A selected model whose endpoint id is undefined is an ERROR naming
    /// the id, never a target (which would send to the managed default).
    #[test]
    fn resolve_in_refuses_a_model_on_an_undefined_endpoint() {
        let mut reg: darkmux_types::ProfileRegistry = serde_json::from_str(
            r#"{"profiles":{"p":{"models":[{"id":"gpt","endpoint":"nope"}]}},"default_profile":"p"}"#,
        )
        .unwrap();
        reg.materialize_endpoints();
        let err = super::resolve_in(&reg, &role(), None, None, false).unwrap_err();
        assert!(format!("{err:#}").contains("nope"), "{err:#}");
    }

    /// (#2916 stage 2) A `profile@machine` address that reaches the LOCAL
    /// resolver (the lab, a mission step, anything that runs only here) is
    /// refused naming it, never read as an undefined name that falls to
    /// `default_profile` and runs the default model here.
    #[test]
    fn resolve_in_refuses_a_profile_address() {
        let reg: darkmux_types::ProfileRegistry = serde_json::from_str(
            r#"{"profiles":{"host":{"models":[{"id":"big","n_ctx":32000}]}},"default_profile":"host"}"#,
        )
        .unwrap();
        let err = super::resolve_in(&reg, &role(), Some("host@studio"), None, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("host@studio") && msg.contains("only on this machine"), "{msg}");
        assert!(super::resolve_in(&reg, &role(), Some("host"), None, false).is_ok(), "a plain name still resolves");
    }
}

#[cfg(test)]
mod equivalence_tests {

    /// One profile model shape, and what a dispatch against it puts on the
    /// wire. `auth` is `(header name, credential source)` where the source
    /// is `env:<VAR>` or `keychain:<item>`, never a value.
    struct Row {
        name: &'static str,
        model: &'static str,
        chat_url: &'static str,
        wire_model: &'static str,
        managed: bool,
        cap_field: &'static str,
        auth: Option<(&'static str, &'static str)>,
        n_ctx: Option<u32>,
    }

    const LMS: &str = "http://127.0.0.1:4321";
    const KEY_ENV: &str = "DMX_2902_EQUIV_KEY";

    fn rows() -> Vec<Row> {
        vec![
            Row {
                name: "managed, bare (profiles.example.json fast/balanced/deep)",
                model: r#"{"id":"qwen3.6-35b-a3b","n_ctx":100000}"#,
                chat_url: "http://127.0.0.1:4321/v1/chat/completions",
                wire_model: "darkmux:qwen3.6-35b-a3b",
                managed: true,
                cap_field: "max_tokens",
                auth: None,
                n_ctx: Some(100000),
            },
            Row {
                name: "managed, identifier opt-out",
                model: r#"{"id":"worker-35b","n_ctx":65536,"identifier":"my-alias"}"#,
                chat_url: "http://127.0.0.1:4321/v1/chat/completions",
                wire_model: "my-alias",
                managed: true,
                cap_field: "max_tokens",
                auth: None,
                n_ctx: Some(65536),
            },
            Row {
                name: "managed, namespaced id in the registry",
                model: r#"{"id":"darkmux:worker-35b","n_ctx":65536}"#,
                chat_url: "http://127.0.0.1:4321/v1/chat/completions",
                wire_model: "darkmux:worker-35b",
                managed: true,
                cap_field: "max_tokens",
                auth: None,
                n_ctx: Some(65536),
            },
            Row {
                name: "hosted, api_version (profiles.example.json hosted-frontier)",
                model: r#"{"id":"gpt-5.1","endpoint":{"url":"https://r.cognitiveservices.azure.com/openai/deployments/d","api_version":"2025-01-01-preview"}}"#,
                chat_url: "https://r.cognitiveservices.azure.com/openai/deployments/d/chat/completions?api-version=2025-01-01-preview",
                wire_model: "gpt-5.1",
                managed: false,
                cap_field: "max_completion_tokens",
                auth: None,
                n_ctx: None,
            },
            Row {
                name: "hosted, bearer keychain, declared window (guide mod seats)",
                model: r#"{"id":"grok-4","n_ctx":128000,"endpoint":{"url":"https://api.x.ai/v1","auth":{"type":"bearer","keychain":"darkmux-grok"}}}"#,
                chat_url: "https://api.x.ai/v1/chat/completions",
                wire_model: "grok-4",
                managed: false,
                cap_field: "max_completion_tokens",
                auth: Some(("Authorization", "keychain:darkmux-grok")),
                n_ctx: Some(128000),
            },
            Row {
                name: "hosted, api-key from key_env, trailing slash",
                model: r#"{"id":"gpt-4o","endpoint":{"url":"https://x.openai.azure.com/openai/deployments/gpt-4o/","api_version":"v1","auth":{"type":"api-key","key_env":"DMX_2902_EQUIV_KEY","keychain":"unused-item"}}}"#,
                chat_url: "https://x.openai.azure.com/openai/deployments/gpt-4o/chat/completions?api-version=v1",
                wire_model: "gpt-4o",
                managed: false,
                cap_field: "max_completion_tokens",
                auth: Some(("api-key", "env:DMX_2902_EQUIV_KEY")),
                n_ctx: None,
            },
            Row {
                name: "hosted, auth block with no type sends no header",
                model: r#"{"id":"proxy-model","endpoint":{"url":"http://localhost:8080/v1","auth":{"keychain":"x"}}}"#,
                chat_url: "http://localhost:8080/v1/chat/completions",
                wire_model: "proxy-model",
                managed: false,
                cap_field: "max_completion_tokens",
                auth: None,
                n_ctx: None,
            },
        ]
    }

    /// What the code resolves `model` (one profile's only model) to.
    struct Observed {
        chat_url: String,
        wire_model: String,
        managed: bool,
        cap_field: &'static str,
        auth: Option<(String, String)>,
        n_ctx: Option<u32>,
    }

    /// Through the one resolver (#2902 step 3): each row, observed on the
    /// consolidated code, resolves to the wire facts it lists.
    fn observe(pf: &std::path::Path) -> Observed {
        let role: crate::types::Role = serde_json::from_str(
            r#"{"id":"r","description":"d","tool_palette":{"allow":[],"deny":[]},"escalation_contract":"bail-with-explanation"}"#,
        )
        .unwrap();
        let loaded = darkmux_profiles::profiles::load_registry(pf.to_str()).unwrap();
        let t = super::resolve_in(&loaded.registry, &role, Some("p"), None, false).unwrap().target().unwrap();
        let wire_model = t.wire_model();
        let body = if t.is_managed() {
            crate::single_shot::local_chat_body(&wire_model, "s", "u", 0.7, 10)
        } else {
            crate::single_shot::chat_body(&crate::single_shot::ChatBody {
                dialect: t.dialect,
                model: &wire_model,
                messages: crate::single_shot::chat_messages("s", "u"),
                max_tokens: 10,
                temperature: None,
                reasoning_effort: None,
            })
        };
        let cap_field = if body.get("max_tokens").is_some() { "max_tokens" } else { "max_completion_tokens" };
        assert_eq!(cap_field, t.dialect.cap_field(), "the body and the resolved dialect agree");
        let auth = t.endpoint.auth.as_ref().and_then(|a| {
            let header = a.auth_type?.header_name().to_string();
            let source = match a.credential_source() {
                darkmux_types::CredentialSource::Env(v) => format!("env:{v}"),
                darkmux_types::CredentialSource::Keychain(k) => format!("keychain:{k}"),
                other => format!("{other:?}"),
            };
            Some((header, source))
        });
        Observed {
            chat_url: t.chat_url.clone(),
            wire_model,
            managed: t.is_managed(),
            cap_field,
            auth,
            n_ctx: t.n_ctx(),
        }
    }

    #[test]
    #[serial_test::serial] // mutates DARKMUX_LMSTUDIO_URL and a key env var
    fn every_shipped_profile_shape_resolves_to_the_same_wire_facts() {
        let prev_url = std::env::var("DARKMUX_LMSTUDIO_URL").ok();
        unsafe {
            std::env::set_var("DARKMUX_LMSTUDIO_URL", LMS);
            std::env::set_var(KEY_ENV, "fake-test-value");
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let mut failures = Vec::new();
        for row in rows() {
            // The row's model, its endpoint (if any) declared once under
            // `endpoints` and named by id.
            let mut model: serde_json::Value = serde_json::from_str(row.model).unwrap();
            let mut registry = serde_json::json!({"default_profile": "p"});
            if let Some(ep) = model.as_object_mut().unwrap().remove("endpoint") {
                model["endpoint"] = serde_json::json!("e");
                registry["endpoints"] = serde_json::json!({ "e": ep });
            }
            registry["profiles"] = serde_json::json!({ "p": { "models": [model] } });
            let pf = tmp.path().join("profiles.json");
            std::fs::write(&pf, registry.to_string()).unwrap();
            let got = observe(&pf);
            let want_auth = row.auth.map(|(h, s)| (h.to_string(), s.to_string()));
            if got.chat_url != row.chat_url
                || got.wire_model != row.wire_model
                || got.managed != row.managed
                || got.cap_field != row.cap_field
                || got.auth != want_auth
                || got.n_ctx != row.n_ctx
            {
                failures.push(format!(
                    "{}: url={} model={} managed={} cap={} auth={:?} n_ctx={:?}",
                    row.name, got.chat_url, got.wire_model, got.managed, got.cap_field, got.auth, got.n_ctx
                ));
            }
        }
        unsafe {
            std::env::remove_var(KEY_ENV);
            match prev_url {
                Some(v) => std::env::set_var("DARKMUX_LMSTUDIO_URL", v),
                None => std::env::remove_var("DARKMUX_LMSTUDIO_URL"),
            }
        }
        assert!(failures.is_empty(), "resolution moved:\n{}", failures.join("\n"));
    }
}
