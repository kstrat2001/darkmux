//! (#2902 steps 3 and 4) Endpoints: where a model's requests go, and what
//! darkmux does there.
//!
//! darkmux records what it INVOKED (the endpoint and the model) and never
//! classifies an endpoint by where it is or what it costs. The one
//! distinction it draws is what darkmux itself DOES at the endpoint,
//! [`EndpointKind`]: on a managed endpoint it loads and unloads models
//! (`lms`), on an unmanaged one it only sends requests.
//!
//! An endpoint is declared once in the registry's `endpoints` map and named
//! by id from each profile model (`"endpoint": "<id>"`), or, for configs
//! written before 4.0, inline as an object on the model (still read; doctor
//! names the move to an id). Both spellings become one [`ModelEndpoint`];
//! everything that decides a URL, a dialect, a host or a credential source
//! reads it through the methods here, so the rules live in one place.

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize, Serializer};

/// What darkmux does at an endpoint: its own action, never the endpoint's
/// location, owner or cost.
///
/// An enum so a later kind is additive: #2916 adds a fleet machine here, and
/// every `match` on this type then names what it does for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    /// darkmux manages what is loaded: it runs the backend's own tool to load
    /// and unload models, dispatches to its own `darkmux:`-namespaced
    /// instance (#2240), and knows the loaded model and window because it
    /// loaded them. The address is the machine's configured backend URL
    /// (`lmstudio_url` for LM Studio), never a per-endpoint `url`.
    Managed(ManagedBackend),
    /// darkmux only sends requests. What serves them can change without
    /// darkmux seeing it; darkmux knows the model it requested and whatever
    /// the response reports, nothing more.
    Unmanaged,
}

impl EndpointKind {
    pub fn is_managed(self) -> bool {
        matches!(self, EndpointKind::Managed(_))
    }

    /// The dialect this kind speaks when the endpoint declares none.
    pub fn default_dialect(self) -> Dialect {
        match self {
            EndpointKind::Managed(ManagedBackend::Lmstudio) => Dialect::ChatCompletionsMaxTokens,
            EndpointKind::Unmanaged => Dialect::ChatCompletions,
        }
    }
}

/// The backend darkmux manages at a managed endpoint. Named only as a VALUE
/// (`"managed": "lmstudio"`), never in a field or type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManagedBackend {
    /// LM Studio, loaded and unloaded through `lms`.
    Lmstudio,
}

/// The request shape an endpoint accepts. Both are OpenAI-compatible chat
/// completions; they differ in the fields around the messages, which are
/// assembled identically for both (contract 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Dialect {
    /// `max_completion_tokens`, an optional `reasoning_effort`, no
    /// `temperature`: what hosted reasoning models require (they reject
    /// `max_tokens` and a non-default temperature). The default for an
    /// unmanaged endpoint.
    ChatCompletions,
    /// `max_tokens` and `temperature` (and, for a single-shot call,
    /// `"stream": false`); `reasoning_effort` is never sent. What LM Studio
    /// takes; the default for a managed endpoint.
    ChatCompletionsMaxTokens,
}

impl Dialect {
    /// The request field that carries the completion cap.
    pub fn cap_field(self) -> &'static str {
        match self {
            Dialect::ChatCompletions => "max_completion_tokens",
            Dialect::ChatCompletionsMaxTokens => "max_tokens",
        }
    }

    /// The `serde` spelling, for messages and the runtime's `--dialect` flag.
    pub fn as_str(self) -> &'static str {
        match self {
            Dialect::ChatCompletions => "chat-completions",
            Dialect::ChatCompletionsMaxTokens => "chat-completions-max-tokens",
        }
    }
}

/// Standard usage limits for one endpoint (#2902 step 4: the SHAPE only).
///
/// **Not enforced yet.** These are parsed, validated and shown by
/// `darkmux doctor`; enforcement is #2902 step 5, which replaces today's
/// `remote.*` knobs with one per-endpoint regime. Until then the existing
/// `remote.max_tokens_per_execution` / `remote.concurrent_cap` still apply
/// and these values change nothing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageLimits {
    /// Tokens one dispatch (one execution) may spend at this endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_dispatch: Option<u64>,
    /// Calls in flight at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrent_calls: Option<u32>,
    /// A budget over a period of time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<UsageWindow>,
    /// Forward-compat overflow.
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// A usage budget over a period (`"period": "1d"`), in tokens, calls, or
/// both. Whether the window is calendar or rolling is decided with
/// enforcement (#2902 step 5); the shape carries the period as written.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// `<n>m`, `<n>h` or `<n>d`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls: Option<u64>,
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

impl UsageLimits {
    /// One line for `darkmux doctor`, e.g. `500000 tokens/dispatch · 2
    /// concurrent calls · 2000000 tokens, 400 calls per 1d`. Empty when no
    /// limit is set.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(n) = self.tokens_per_dispatch {
            parts.push(format!("{n} tokens/dispatch"));
        }
        if let Some(n) = self.concurrent_calls {
            parts.push(format!("{n} concurrent calls"));
        }
        if let Some(w) = &self.window {
            let mut budget: Vec<String> = Vec::new();
            if let Some(t) = w.tokens {
                budget.push(format!("{t} tokens"));
            }
            if let Some(c) = w.calls {
                budget.push(format!("{c} calls"));
            }
            if !budget.is_empty() {
                let period = w.period.as_deref().unwrap_or("(no period)");
                parts.push(format!("{} per {period}", budget.join(", ")));
            }
        }
        parts.join(" · ")
    }

    /// Shape checks only (enforcement is step 5).
    pub fn validate(&self) -> Result<(), String> {
        if let Some(w) = &self.window {
            if w.tokens.is_none() && w.calls.is_none() {
                return Err("limits.window sets neither `tokens` nor `calls`".to_string());
            }
            match w.period.as_deref() {
                Some(p) if is_period(p) => {}
                Some(p) => {
                    return Err(format!(
                        "limits.window.period must be `<n>m`, `<n>h` or `<n>d` (got {p:?})"
                    ))
                }
                None => return Err("limits.window needs a `period` (`<n>m`, `<n>h` or `<n>d`)".to_string()),
            }
        }
        Ok(())
    }
}

/// `<n>m`, `<n>h` or `<n>d` with `n >= 1`.
fn is_period(p: &str) -> bool {
    let Some(unit) = p.chars().last() else { return false };
    matches!(unit, 'm' | 'h' | 'd') && p[..p.len() - 1].parse::<u32>().is_ok_and(|n| n >= 1)
}

/// How a profile model's endpoint was written. Runtime-only: set by the
/// deserializer and the registry loader, never serialized itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum EndpointSource {
    /// An inline object on the model (the pre-4.0 spelling), or an entry of
    /// the `endpoints` map itself.
    #[default]
    Inline,
    /// `"endpoint": "<id>"`, matched to the `endpoints` map's definition,
    /// whose fields this value now carries.
    Named(String),
    /// `"endpoint": "<id>"` that no `endpoints` entry defines (or a registry
    /// that was never passed through the loader's materialization). Carries
    /// no fields; every consumer refuses it loudly rather than guessing.
    Unresolved(String),
}

/// An endpoint a model is served from. Absent on a [`crate::ProfileModel`] ⇒
/// the managed LM Studio default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelEndpoint {
    /// Base URL of an UNMANAGED OpenAI-compatible server, up to (not
    /// including) `/chat/completions`, e.g. `https://api.openai.com/v1`.
    /// A managed endpoint has none: its address is `lmstudio_url`.
    ///
    /// Legacy rule, kept so every pre-4.0 config reads unchanged: an inline
    /// endpoint with no `url` and no `managed` is managed LM Studio; one with
    /// a `url` and no `managed` is unmanaged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// API-version query parameter (Azure OpenAI requires one, e.g.
    /// `"2025-01-01-preview"`). Unmanaged endpoints only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    /// Where the credential lives (a Keychain item name or an env-var name,
    /// never the secret). Absent ⇒ no auth header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<EndpointAuth>,
    /// Reasoning effort requested (`"low"`/`"medium"`/`"high"`, sent verbatim
    /// as `reasoning_effort` in the `chat-completions` dialect). Absent ⇒ the
    /// parameter is omitted. Reasoning tokens bill inside
    /// `max_completion_tokens`, so setting this also raises the hosted
    /// single-shot cap default (4096 → 16384); an explicit cap still wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// What darkmux manages here: `"lmstudio"`, or absent for none (see
    /// [`EndpointKind`] and the legacy rule on `url`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<ManagedBackend>,
    /// The request shape, when it differs from the kind's default
    /// ([`EndpointKind::default_dialect`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<Dialect>,
    /// Standard usage limits (the shape only; not enforced yet, see
    /// [`UsageLimits`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<UsageLimits>,
    /// How this value was written. Runtime-only.
    #[serde(skip)]
    pub source: EndpointSource,
    /// Forward-compat overflow.
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// Why an endpoint cannot be used as written. Its `Display` is the whole
/// operator-facing message, fix included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointError(pub String);

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EndpointError {}

impl ModelEndpoint {
    /// A reference to `endpoints.<id>` that has not been matched to a
    /// definition yet (what `"endpoint": "<id>"` deserializes to).
    pub fn reference(id: impl Into<String>) -> Self {
        ModelEndpoint { source: EndpointSource::Unresolved(id.into()), ..Default::default() }
    }

    /// The `endpoints` id this value names, when it was written by id.
    pub fn named_id(&self) -> Option<&str> {
        match &self.source {
            EndpointSource::Named(id) | EndpointSource::Unresolved(id) => Some(id),
            EndpointSource::Inline => None,
        }
    }

    /// THE classification: what darkmux does at this endpoint. An unresolved
    /// reference is an error, never a guess (a dangling id treated as the
    /// managed default would send the request to LM Studio unannounced).
    pub fn kind(&self) -> Result<EndpointKind, EndpointError> {
        if let EndpointSource::Unresolved(id) = &self.source {
            return Err(EndpointError(format!(
                "darkmux: endpoint `{id}` is named by id but the profile registry's `endpoints` map \
                 does not define it. Add `\"endpoints\": {{ \"{id}\": {{ ... }} }}` to profiles.json \
                 (or fix the id); `darkmux doctor` lists every dangling reference. (#2902)"
            )));
        }
        Ok(match (self.managed, &self.url) {
            (Some(backend), _) => EndpointKind::Managed(backend),
            (None, None) => EndpointKind::Managed(ManagedBackend::Lmstudio),
            (None, Some(_)) => EndpointKind::Unmanaged,
        })
    }

    /// The request shape: declared, else the kind's default.
    pub fn resolved_dialect(&self) -> Result<Dialect, EndpointError> {
        let kind = self.kind()?;
        Ok(self.dialect.unwrap_or_else(|| kind.default_dialect()))
    }

    /// THE chat-completions URL builder. Managed: the configured LM Studio
    /// URL normalized to its `/v1` root (`config_access::lmstudio_url`).
    /// Unmanaged: `{url}/chat/completions`, plus `?api-version=` when set.
    pub fn chat_url(&self) -> Result<String, EndpointError> {
        match self.kind()? {
            EndpointKind::Managed(ManagedBackend::Lmstudio) => {
                Ok(lmstudio_chat_url(&crate::config_access::lmstudio_url()))
            }
            EndpointKind::Unmanaged => {
                // `kind()` is Unmanaged only when a url is set.
                let base = self.url.as_deref().unwrap_or_default().trim_end_matches('/');
                Ok(match self.api_version.as_deref() {
                    Some(v) => format!("{base}/chat/completions?api-version={v}"),
                    None => format!("{base}/chat/completions"),
                })
            }
        }
    }

    /// The host (authority, userinfo stripped) of an unmanaged endpoint's
    /// URL, for labels and debug lines. `None` for a managed endpoint. Never
    /// the path (an Azure deployment URL embeds the deployment name) and
    /// never credentials (a `https://tok@proxy/v1` URL keeps its token out).
    pub fn host(&self) -> Option<String> {
        match self.kind() {
            Ok(EndpointKind::Unmanaged) => self.url.as_deref().and_then(url_host),
            _ => None,
        }
    }

    /// Coherence of the endpoint AS WRITTEN (no I/O; the credential's
    /// presence is `darkmux doctor`'s live check). Returns the reason.
    pub fn validate(&self) -> Result<(), String> {
        let kind = self.kind().map_err(|e| e.0)?;
        if let Some(u) = &self.url {
            if !(u.starts_with("http://") || u.starts_with("https://")) {
                return Err(format!("endpoint.url must start with http:// or https:// (got {u:?})"));
            }
            if kind.is_managed() {
                return Err(format!(
                    "a managed endpoint has no `url` of its own: darkmux dispatches to the machine's \
                     LM Studio at `lmstudio_url` (`darkmux config set lmstudio_url {u}`). Drop `url`, \
                     or drop `managed` if darkmux should only send requests there"
                ));
            }
        }
        if self.api_version.is_some() && kind.is_managed() {
            return Err("`api_version` applies to an unmanaged endpoint's URL; a managed endpoint never sends it".to_string());
        }
        if self.reasoning_effort.is_some()
            && self.resolved_dialect().map_err(|e| e.0)? == Dialect::ChatCompletionsMaxTokens
            && self.dialect.is_some()
        {
            return Err(format!(
                "`reasoning_effort` is never sent in the `{}` dialect; drop one of the two",
                Dialect::ChatCompletionsMaxTokens.as_str()
            ));
        }
        if let Some(auth) = &self.auth {
            // (#1312) A credential SOURCE must be declared: the Keychain item
            // (`keychain`) or the env-var name (`key_env`). One is enough.
            if auth.auth_type.is_some()
                && auth.keychain.as_deref().unwrap_or("").is_empty()
                && auth.key_env.as_deref().unwrap_or("").is_empty()
            {
                return Err("endpoint.auth.type is set but no credential source is declared — set \
                     endpoint.auth.keychain (a macOS Keychain item name) or endpoint.auth.key_env \
                     (the NAME of an env var holding the key)"
                    .to_string());
            }
        }
        if let Some(limits) = &self.limits {
            limits.validate()?;
        }
        Ok(())
    }
}

/// The `/v1` root of an LM Studio base URL: a trailing `/` and a trailing
/// `/v1` are both tolerated, so `http://h:1234`, `http://h:1234/` and
/// `http://h:1234/v1` all yield `http://h:1234/v1`.
pub fn lmstudio_v1_base(base: &str) -> String {
    let base = base.trim_end_matches('/').trim_end_matches("/v1");
    format!("{base}/v1")
}

/// The chat-completions URL of an LM Studio server at `base` (any spelling
/// [`lmstudio_v1_base`] accepts). The managed arm of
/// [`ModelEndpoint::chat_url`], exposed for the callers that are handed an
/// explicit base (the mock-model harness).
pub fn lmstudio_chat_url(base: &str) -> String {
    format!("{}/chat/completions", lmstudio_v1_base(base))
}

/// The authority of `url` with any userinfo stripped: `https://tok@h:1/p`
/// → `h:1`. `None` when `url` has no `scheme://`.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split('/').next().unwrap_or(rest);
    Some(authority.rsplit('@').next().unwrap_or(authority).to_string())
}

/// Auth for an endpoint. The secret is **never** stored here — only where it
/// lives: a macOS Keychain item *name* and/or an environment variable's
/// *name*. The machine holding the item is the endpoint's keymaster.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EndpointAuth {
    /// Header mechanics: `api-key` or `bearer`. Absent ⇒ no auth header.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<EndpointAuthType>,
    /// macOS Keychain item name holding the secret. Read at runtime; never
    /// logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keychain: Option<String>,
    /// (#1312) NAME of an environment variable holding the key. When set AND
    /// present in the environment it is used verbatim and the Keychain is
    /// never read (the headless-runner escape hatch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    /// Forward-compat overflow.
    #[serde(flatten)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// The header mechanics for an endpoint's auth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndpointAuthType {
    /// `api-key: <secret>`.
    ApiKey,
    /// `Authorization: Bearer <secret>`.
    Bearer,
}

impl EndpointAuthType {
    /// The header name this mechanic sends.
    pub fn header_name(self) -> &'static str {
        match self {
            EndpointAuthType::ApiKey => "api-key",
            EndpointAuthType::Bearer => "Authorization",
        }
    }
}

/// Where an endpoint's credential comes from, in THE precedence order (the
/// one place it is written): a declared env var that is present > the
/// declared Keychain item. Names only; the value is read by the caller that
/// sends the request, never here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource<'a> {
    /// No auth header is sent (no `auth`, or no `auth.type`).
    NoHeader,
    /// The declared env var, present and non-empty in this environment.
    Env(&'a str),
    /// The declared Keychain item.
    Keychain(&'a str),
    /// A header is required but nothing resolves: `key_env` (if declared)
    /// is not set here, and no Keychain item is declared.
    Missing { key_env: Option<&'a str> },
}

impl EndpointAuth {
    /// THE credential-source precedence (see [`CredentialSource`]). Reads the
    /// environment for the declared variable's PRESENCE only.
    pub fn credential_source(&self) -> CredentialSource<'_> {
        if self.auth_type.is_none() {
            return CredentialSource::NoHeader;
        }
        let key_env = self.key_env.as_deref().filter(|v| !v.is_empty());
        if let Some(var) = key_env {
            if std::env::var(var).is_ok_and(|v| !v.is_empty()) {
                return CredentialSource::Env(var);
            }
        }
        match self.keychain.as_deref().filter(|k| !k.is_empty()) {
            Some(item) => CredentialSource::Keychain(item),
            None => CredentialSource::Missing { key_env },
        }
    }
}

/// `serde(with)` for `ProfileModel.endpoint`: a string names an `endpoints`
/// entry by id; an object is the inline (pre-4.0) spelling. A named endpoint
/// serializes back as its id, so a registry round-trips in the shape it was
/// written.
pub(crate) mod endpoint_field {
    use super::*;

    pub fn serialize<S: Serializer>(v: &Option<ModelEndpoint>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            None => s.serialize_none(),
            Some(ep) => match ep.named_id() {
                Some(id) => s.serialize_some(id),
                None => s.serialize_some(ep),
            },
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<ModelEndpoint>, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<ModelEndpoint>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an endpoint id (a string naming an `endpoints` entry) or an inline endpoint object")
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
                d.deserialize_any(V)
            }
            fn visit_str<E: de::Error>(self, id: &str) -> Result<Self::Value, E> {
                Ok(Some(ModelEndpoint::reference(id)))
            }
            fn visit_map<M: MapAccess<'de>>(self, m: M) -> Result<Self::Value, M::Error> {
                ModelEndpoint::deserialize(de::value::MapAccessDeserializer::new(m)).map(Some)
            }
        }
        d.deserialize_option(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProfileModel;

    fn pm(json: &str) -> ProfileModel {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn kind_follows_managed_then_the_legacy_url_rule() {
        let absent = pm(r#"{"id":"m","n_ctx":1}"#);
        assert_eq!(absent.endpoint_kind().unwrap(), EndpointKind::Managed(ManagedBackend::Lmstudio));
        let no_url = pm(r#"{"id":"m","n_ctx":1,"endpoint":{"reasoning_effort":"high"}}"#);
        assert_eq!(no_url.endpoint_kind().unwrap(), EndpointKind::Managed(ManagedBackend::Lmstudio));
        let url = pm(r#"{"id":"m","endpoint":{"url":"http://localhost:1234/v1"}}"#);
        assert_eq!(url.endpoint_kind().unwrap(), EndpointKind::Unmanaged, "a url means darkmux only sends");
        let explicit = pm(r#"{"id":"m","n_ctx":1,"endpoint":{"managed":"lmstudio"}}"#);
        assert_eq!(explicit.endpoint_kind().unwrap(), EndpointKind::Managed(ManagedBackend::Lmstudio));
        assert!(explicit.is_managed());
        assert!(!url.is_managed());
    }

    #[test]
    fn an_id_reference_parses_serializes_back_as_the_id_and_is_refused_until_defined() {
        let m = pm(r#"{"id":"gpt-4o","endpoint":"azure-east"}"#);
        let ep = m.endpoint.as_ref().unwrap();
        assert_eq!(ep.source, EndpointSource::Unresolved("azure-east".into()));
        let err = m.endpoint_kind().unwrap_err().to_string();
        assert!(err.contains("azure-east") && err.contains("endpoints"), "names the id and the map: {err}");
        assert!(!m.is_managed(), "an undefined reference is never treated as the managed default");
        let out = serde_json::to_value(&m).unwrap();
        assert_eq!(out["endpoint"], "azure-east");
    }

    #[test]
    fn an_inline_object_keeps_field_level_parse_errors() {
        let err = serde_json::from_str::<ProfileModel>(r#"{"id":"m","endpoint":{"url":5}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid type"), "serde's own field error survives: {err}");
    }

    #[test]
    fn dialect_defaults_by_kind_and_can_be_declared() {
        let managed = ModelEndpoint::default();
        assert_eq!(managed.resolved_dialect().unwrap(), Dialect::ChatCompletionsMaxTokens);
        let hosted = ModelEndpoint { url: Some("https://h/v1".into()), ..Default::default() };
        assert_eq!(hosted.resolved_dialect().unwrap(), Dialect::ChatCompletions);
        let declared = ModelEndpoint {
            url: Some("https://h/v1".into()),
            dialect: Some(Dialect::ChatCompletionsMaxTokens),
            ..Default::default()
        };
        assert_eq!(declared.resolved_dialect().unwrap(), Dialect::ChatCompletionsMaxTokens);
        assert_eq!(serde_json::to_value(Dialect::ChatCompletionsMaxTokens).unwrap(), "chat-completions-max-tokens");
    }

    #[test]
    #[serial_test::serial] // reads DARKMUX_LMSTUDIO_URL
    fn chat_url_is_built_one_way_per_kind() {
        let prev = std::env::var("DARKMUX_LMSTUDIO_URL").ok();
        unsafe { std::env::set_var("DARKMUX_LMSTUDIO_URL", "http://127.0.0.1:4321/v1/") };
        let managed = ModelEndpoint::default().chat_url().unwrap();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LMSTUDIO_URL", v),
                None => std::env::remove_var("DARKMUX_LMSTUDIO_URL"),
            }
        }
        assert_eq!(managed, "http://127.0.0.1:4321/v1/chat/completions");
        let azure = ModelEndpoint {
            url: Some("https://r.example/openai/deployments/d/".into()),
            api_version: Some("2025-01-01-preview".into()),
            ..Default::default()
        };
        assert_eq!(
            azure.chat_url().unwrap(),
            "https://r.example/openai/deployments/d/chat/completions?api-version=2025-01-01-preview"
        );
        assert!(ModelEndpoint::reference("nope").chat_url().is_err());
    }

    #[test]
    fn host_is_the_authority_without_userinfo_and_none_when_managed() {
        let ep = ModelEndpoint { url: Some("https://tok@proxy.example:8443/v1/x".into()), ..Default::default() };
        assert_eq!(ep.host().as_deref(), Some("proxy.example:8443"));
        assert_eq!(ModelEndpoint::default().host(), None);
        assert_eq!(url_host("no-scheme.example/v1"), None);
    }

    #[test]
    fn validate_refuses_what_cannot_work_and_names_it() {
        let bad = ModelEndpoint { url: Some("example.azure.com".into()), ..Default::default() };
        assert!(bad.validate().unwrap_err().contains("http://"));
        let managed_with_url = ModelEndpoint {
            url: Some("http://localhost:1234".into()),
            managed: Some(ManagedBackend::Lmstudio),
            ..Default::default()
        };
        assert!(managed_with_url.validate().unwrap_err().contains("lmstudio_url"));
        let effort_ignored = ModelEndpoint {
            url: Some("https://h/v1".into()),
            dialect: Some(Dialect::ChatCompletionsMaxTokens),
            reasoning_effort: Some("high".into()),
            ..Default::default()
        };
        assert!(effort_ignored.validate().unwrap_err().contains("reasoning_effort"));
        let no_source = ModelEndpoint {
            url: Some("https://h/v1".into()),
            auth: Some(EndpointAuth { auth_type: Some(EndpointAuthType::Bearer), ..Default::default() }),
            ..Default::default()
        };
        assert!(no_source.validate().is_err());
        let bad_limits = ModelEndpoint {
            url: Some("https://h/v1".into()),
            limits: Some(UsageLimits {
                window: Some(UsageWindow { period: Some("weekly".into()), tokens: Some(1), ..Default::default() }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(bad_limits.validate().unwrap_err().contains("period"));
        assert!(ModelEndpoint::reference("x").validate().unwrap_err().contains("endpoints"));
        let ok = ModelEndpoint {
            url: Some("https://h/v1".into()),
            auth: Some(EndpointAuth {
                auth_type: Some(EndpointAuthType::Bearer),
                keychain: Some("item".into()),
                ..Default::default()
            }),
            limits: Some(UsageLimits { tokens_per_dispatch: Some(5), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(ok.validate(), Ok(()));
    }

    #[test]
    #[serial_test::serial] // sets and clears a test-only env var
    fn credential_source_prefers_a_present_env_var_then_the_keychain() {
        let var = "DMX_2902_CRED_SOURCE_TEST";
        let auth = EndpointAuth {
            auth_type: Some(EndpointAuthType::ApiKey),
            keychain: Some("item".into()),
            key_env: Some(var.into()),
            ..Default::default()
        };
        unsafe { std::env::remove_var(var) };
        assert_eq!(auth.credential_source(), CredentialSource::Keychain("item"));
        unsafe { std::env::set_var(var, "x") };
        assert_eq!(auth.credential_source(), CredentialSource::Env(var));
        unsafe { std::env::set_var(var, "") };
        assert_eq!(auth.credential_source(), CredentialSource::Keychain("item"), "an empty var is absent");
        unsafe { std::env::remove_var(var) };
        let env_only = EndpointAuth { keychain: None, ..auth.clone() };
        assert_eq!(env_only.credential_source(), CredentialSource::Missing { key_env: Some(var) });
        let untyped = EndpointAuth { auth_type: None, ..auth };
        assert_eq!(untyped.credential_source(), CredentialSource::NoHeader);
    }

    fn registry(json: &str) -> crate::ProfileRegistry {
        let mut r: crate::ProfileRegistry = serde_json::from_str(json).unwrap();
        r.materialize_endpoints();
        r
    }

    #[test]
    fn a_named_endpoint_materializes_to_its_definition_and_round_trips_as_the_id() {
        let r = registry(
            r#"{"profiles":{"p":{"models":[{"id":"gpt-4o","endpoint":"azure"}]}},
                "endpoints":{"azure":{"url":"https://h.example/v1","api_version":"v1",
                    "auth":{"type":"api-key","keychain":"item"},"limits":{"tokens_per_dispatch":5}}}}"#,
        );
        let ep = r.profiles["p"].models[0].endpoint.as_ref().unwrap();
        assert_eq!(ep.source, EndpointSource::Named("azure".into()));
        assert_eq!(ep.url.as_deref(), Some("https://h.example/v1"));
        assert_eq!(ep.kind().unwrap(), EndpointKind::Unmanaged);
        assert_eq!(ep.limits.as_ref().unwrap().tokens_per_dispatch, Some(5));
        let out = serde_json::to_value(&r).unwrap();
        assert_eq!(out["profiles"]["p"]["models"][0]["endpoint"], "azure", "written back as the id");
        assert_eq!(out["endpoints"]["azure"]["url"], "https://h.example/v1");
        assert!(r.validate().is_empty(), "{:?}", r.validate());
    }

    #[test]
    fn registry_validation_is_one_pass_over_every_rule() {
        let r = registry(
            r#"{"profiles":{"p":{"models":[
                    {"id":"a","endpoint":"missing"},
                    {"id":"b","endpoint":{"url":"https://inline.example/v1"}},
                    {"id":"c"},
                    {"id":"d","n_ctx":1,"endpoint":"bad"}]}},
                "endpoints":{"bad":{"url":"ftp://nope"}}}"#,
        );
        let issues = r.validate();
        let errors: Vec<&str> = issues
            .iter()
            .filter(|i| i.severity == crate::IssueSeverity::Error)
            .map(|i| i.message.as_str())
            .collect();
        let advice: Vec<&str> = issues
            .iter()
            .filter(|i| i.severity == crate::IssueSeverity::Advice)
            .map(|i| i.message.as_str())
            .collect();
        assert!(errors.iter().any(|m| m.contains("\"missing\"") && m.contains("does not define")), "{errors:?}");
        assert!(errors.iter().any(|m| m.contains("endpoint \"bad\"") && m.contains("http://")), "{errors:?}");
        assert!(errors.iter().any(|m| m.contains("model \"c\"") && m.contains("n_ctx")), "{errors:?}");
        assert!(!errors.iter().any(|m| m.contains("model \"a\"") && m.contains("n_ctx")), "an unresolved endpoint is not a managed model missing n_ctx");
        assert_eq!(advice.len(), 1, "{advice:?}");
        assert!(advice[0].contains("model \"b\"") && advice[0].contains("endpoints.\"inline.example\""), "{advice:?}");
    }

    #[test]
    fn limits_summary_names_each_set_limit() {
        let l = UsageLimits {
            tokens_per_dispatch: Some(500000),
            concurrent_calls: Some(2),
            window: Some(UsageWindow {
                period: Some("1d".into()),
                tokens: Some(2000000),
                calls: Some(400),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(l.summary(), "500000 tokens/dispatch · 2 concurrent calls · 2000000 tokens, 400 calls per 1d");
        assert_eq!(UsageLimits::default().summary(), "");
    }
}
