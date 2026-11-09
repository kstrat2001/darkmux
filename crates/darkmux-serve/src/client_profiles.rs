//! What a paired app may run on, as `darkmux client serve` decides it: which
//! profiles keep a role's work on this machine, and what each one is.
//!
//! Both answers come from the resolved dispatch target, never from a reading
//! of the profile's own fields: the role's selected model is resolved the way
//! `darkmux dispatch <role> --profile <p>` resolves it
//! (`darkmux_crew::target::resolve_in`), and whether its work leaves this
//! machine is the one data-boundary predicate dispatch routes on
//! (`darkmux_types::endpoint::brain_leaves_this_machine`): LM Studio stays,
//! an unmanaged endpoint stays only when it is this machine's
//! `default_engine` reached on loopback, and anything that cannot be read
//! fails closed. So a URL whose userinfo says `localhost`
//! (`https://localhost:x@api.example.com/v1`) is hosted, and an endpoint id
//! the registry does not define refuses.

use darkmux_types::config_enum::ConfigEnum;
use darkmux_types::endpoint::EndpointKind;
use serde::Serialize;
use std::collections::HashMap;

/// One profile an app may run on this machine, as `GET /client/profiles`
/// reports it. Read-only: what the registry declares and whether its server
/// answers right now.
#[derive(Debug, Clone, Serialize, PartialEq, schemars::JsonSchema)]
pub struct ClientProfileInfo {
    pub name: String,
    pub description: Option<String>,
    /// The model the role's work would run on (the resolved selection).
    pub model: Option<String>,
    /// `url` (an unmanaged server darkmux only sends to) or the managed
    /// backend's name (`lmstudio`).
    pub endpoint: String,
    /// Its server answered just now.
    pub reachable: bool,
    /// The model is loaded and serving (`None`: couldn't tell).
    pub loaded: Option<bool>,
    /// Why it couldn't be described (e.g. not in the registry).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ClientProfileInfo {
    /// The entry for a profile that could not be described.
    pub fn unknown(name: &str, error: String) -> Self {
        ClientProfileInfo {
            name: name.to_string(),
            description: None,
            model: None,
            endpoint: "unknown".into(),
            reachable: false,
            loaded: None,
            error: Some(error),
        }
    }
}

/// The role, by id, from this machine's role library.
fn role_named(role_id: &str) -> Result<darkmux_crew::types::Role, String> {
    darkmux_crew::loader::load_roles()
        .map_err(|e| format!("{e:#}"))?
        .into_iter()
        .find(|r| r.id == role_id)
        .ok_or_else(|| format!("no role {role_id} on this machine"))
}

/// The target `role`'s work on profile `name` resolves to, exactly as
/// dispatch resolves it. A name the registry does not define is refused here
/// (the resolver would fall to `default_profile` for it, #1054).
fn target_of(
    loaded: &darkmux_profiles::profiles::LoadedRegistry,
    role: &darkmux_crew::types::Role,
    name: &str,
) -> Result<darkmux_crew::target::Target, String> {
    if !loaded.registry.profiles.contains_key(name) {
        return Err(loaded
            .registry
            .quarantine_error_for(name)
            .unwrap_or_else(|| format!("profile {name} is not in this machine's profiles.json")));
    }
    darkmux_crew::target::resolve_in(&loaded.registry, role, Some(name), None, false)
        .and_then(|r| r.require(&role.id, Some(name), &loaded.path))
        .map_err(|e| format!("{e:#}"))
}

/// Whether `target`'s work stays on this machine: the data-boundary
/// predicate dispatch itself uses, against the live `default_engine`.
fn target_stays_here(target: &darkmux_crew::target::Target) -> bool {
    !darkmux_types::endpoint::brain_leaves_this_machine(
        target.kind,
        Some(&target.endpoint),
        &darkmux_types::config_access::default_engine(),
    )
}

/// The production `ClientServerState::profile_is_local`: whether `role`'s
/// work on profile `name` stays on this machine. `Err` (so the work is
/// refused) when the role, the registry, the profile or its endpoint cannot
/// be resolved.
pub fn work_stays_here(role_id: &str, name: &str) -> Result<bool, String> {
    let role = role_named(role_id)?;
    let loaded =
        darkmux_profiles::profiles::load_registry_quiet(None).map_err(|e| format!("{e:#}"))?;
    target_of(&loaded, &role, name).map(|t| target_stays_here(&t))
}

/// A listed profile before its server is looked at.
struct Selected {
    name: String,
    description: Option<String>,
    model: String,
    /// The managed backend's name, or `None` for an unmanaged endpoint.
    managed: Option<&'static str>,
    /// The unmanaged endpoint's URL.
    url: Option<String>,
}

fn selected(target: darkmux_crew::target::Target) -> Selected {
    let managed = match target.kind {
        EndpointKind::Managed(backend) => Some(backend.token()),
        EndpointKind::Unmanaged => None,
    };
    Selected {
        name: target.profile_name,
        description: target.profile.description,
        model: target.model.id,
        url: managed.map_or(target.endpoint.url, |_| None),
        managed,
    }
}

/// Each distinct server probed once, all at the same time.
fn probe_servers(entries: &[Result<Selected, String>]) -> HashMap<String, bool> {
    let mut urls: Vec<&String> = entries
        .iter()
        .filter_map(|e| e.as_ref().ok().and_then(|s| s.url.as_ref()))
        .collect();
    urls.sort();
    urls.dedup();
    std::thread::scope(|s| {
        let probes: Vec<_> = urls
            .into_iter()
            .map(|url| (url.clone(), s.spawn(move || server_answers(url))))
            .collect();
        probes
            .into_iter()
            .map(|(url, h)| (url, h.join().unwrap_or(false)))
            .collect()
    })
}

/// What one selected profile reports, given the probes.
fn describe(
    s: Selected,
    up: &HashMap<String, bool>,
    managed_loaded: Option<&Vec<darkmux_types::LoadedModel>>,
) -> ClientProfileInfo {
    let (endpoint, reachable, loaded) = match (s.managed, &s.url) {
        (Some(backend), _) => match managed_loaded {
            Some(models) => {
                let here = models
                    .iter()
                    .any(|m| m.model == s.model || m.identifier.ends_with(&s.model));
                (backend, true, Some(here))
            }
            None => (backend, false, None),
        },
        (None, url) => {
            let answered = url
                .as_ref()
                .and_then(|u| up.get(u))
                .copied()
                .unwrap_or(false);
            ("url", answered, Some(answered))
        }
    };
    ClientProfileInfo {
        name: s.name,
        description: s.description,
        model: Some(s.model),
        endpoint: endpoint.to_string(),
        reachable,
        loaded,
        error: None,
    }
}

/// The production `ClientServerState::profile_info`: the registry read once,
/// each profile resolved for `role` as dispatch would (the model it reports
/// is the one the work would run on), each distinct server probed once and
/// all at the same time (about 1 s at most), and the managed backend's
/// listing asked for once. Looked at per profile, 30-odd local profiles took
/// longer than an app waits.
pub fn describe_profiles(
    role_id: &str,
    names: &[String],
) -> Vec<Result<ClientProfileInfo, String>> {
    let loaded = role_named(role_id).and_then(|role| {
        darkmux_profiles::profiles::load_registry_quiet(None)
            .map(|l| (role, l))
            .map_err(|e| format!("{e:#}"))
    });
    let (role, loaded) = match loaded {
        Ok(rl) => rl,
        Err(e) => return names.iter().map(|_| Err(e.clone())).collect(),
    };
    let entries: Vec<Result<Selected, String>> = names
        .iter()
        .map(|name| target_of(&loaded, &role, name).map(selected))
        .collect();
    let up = probe_servers(&entries);
    let needs_managed = entries
        .iter()
        .any(|e| e.as_ref().is_ok_and(|s| s.managed.is_some()));
    let managed_loaded = needs_managed
        .then(|| darkmux_profiles::engine::lmstudio().loaded().ok())
        .flatten();
    entries
        .into_iter()
        .map(|e| e.map(|s| describe(s, &up, managed_loaded.as_ref())))
        .collect()
}

/// One profile's entry ([`describe_profiles`] for a single name).
#[cfg(test)]
pub(crate) fn describe_profile(role_id: &str, name: &str) -> Result<ClientProfileInfo, String> {
    describe_profiles(role_id, &[name.to_string()]).remove(0)
}

/// Whether a model server answers within about a second. Any HTTP answer
/// (401 included: it wants its key) means it's up.
fn server_answers(url: &str) -> bool {
    let probe = format!("{}/models", url.trim_end_matches('/'));
    matches!(
        ureq::get(&probe)
            .timeout(std::time::Duration::from_secs(1))
            .call(),
        Ok(_) | Err(ureq::Error::Status(..))
    )
}
