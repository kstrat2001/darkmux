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
//! by id from each profile model (`"endpoint": "<id>"`). A profile model has
//! no inline endpoint object: 4.0 refuses one, naming the rewrite
//! ([`crate::ProfileRegistry::inline_endpoint_rewrites`]). Everything that
//! decides a URL, a dialect, a host or a credential source reads a
//! [`ModelEndpoint`] through the methods here, so the rules live in one
//! place.

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

/// What darkmux does at an endpoint: its own action, never the endpoint's
/// location, owner or cost.
///
/// An enum so a later kind is additive: every `match` on this type then names
/// what darkmux does for the new one. (A fleet machine is not a kind: running
/// on another machine is a property of the profile address.)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ManagedBackend {
    /// LM Studio, loaded and unloaded through `lms`.
    Lmstudio,
}

/// The request shape an endpoint accepts. Both are OpenAI-compatible chat
/// completions; they differ in the fields around the messages, which are
/// assembled identically for both (contract 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
        crate::config_enum::ConfigEnum::token(self)
    }
}

/// (#2947) `"a"`, `"a" and "b"`, `"a", "b" and "c"`: a value list for an
/// endpoint refusal, generated from the enum's own table.
fn quoted_tokens<T: crate::config_enum::ConfigEnum>() -> String {
    let q: Vec<String> = T::TOKENS.iter().map(|t| format!("\"{t}\"")).collect();
    match q.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        Some((last, _)) => last.clone(),
        None => String::new(),
    }
}

// (#2947) The value tables for the two per-endpoint enums. They stay
// `serde`-deserialized through `Lenient<T>` (an unknown value is kept and
// refused by name at use); these give them the same token + meaning table
// every config enum has, and `serde_spelling_matches_the_config_enum_token`
// pins that `serde`'s spelling and the table cannot drift apart.
crate::config_enum!(ManagedBackend, "managed backend", [
    Lmstudio = "lmstudio" => "LM Studio, loaded and unloaded through `lms`",
]);
crate::config_enum!(Dialect, "endpoint dialect", [
    ChatCompletions = "chat-completions" =>
        "`max_completion_tokens` + optional `reasoning_effort`, no `temperature` (unmanaged default)",
    ChatCompletionsMaxTokens = "chat-completions-max-tokens" =>
        "`max_tokens` + `temperature`, no `reasoning_effort` (managed LM Studio default)",
]);

/// (#2902 step 5) What darkmux does when an endpoint's budget is reached.
/// Registered on the #2947 `ConfigEnum` rule: an unregistered value is
/// refused at preflight, never resolved to a fallback. `wait` governs the
/// rolling `window` only (a dispatch's own spend never expires, so nothing
/// would free room under `tokens_per_dispatch`); there the cap warns.
///
/// There is deliberately no action that stops a run: a hard stop is the
/// operator's own `darkmux mission abort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BudgetPolicy {
    /// Nothing is counted.
    Off,
    /// A breach is surfaced (CLI line, flow record, doctor) and the work
    /// keeps going.
    Warn,
    /// Calls to the endpoint pause until the budget has room again (for a
    /// rolling window: until enough of it has expired), then resume. The
    /// run is never killed and no work is lost.
    Wait,
}

crate::config_enum!(BudgetPolicy, "budget policy", [
    Off = "off" => "nothing is counted",
    Warn = "warn" => "a breach is surfaced and the work keeps going (the default once a budget is set)",
    Wait = "wait" => "calls pause until the budget has room again, then resume; the run is never stopped",
]);

impl BudgetPolicy {
    /// True when the policy counts spend at all.
    pub fn counts(self) -> bool {
        !matches!(self, BudgetPolicy::Off)
    }
}

/// Standard usage limits for one endpoint, managed or not (#3035: "remote"
/// was the wrong axis, so the old `remote.*` knobs live here, per endpoint).
///
/// **Spend limits apply to any endpoint.** `tokens_per_dispatch` caps what
/// ONE dispatch (one role execution) may spend there; `window` is a rolling
/// budget (`tokens`, `calls`, or both, over `period`) summed from this
/// machine's usage records, which carry the endpoint's id (`endpoint_id`),
/// so the endpoint must be declared in the `endpoints` map and named by id.
/// `policy` (`off` / `warn` / `wait`, see [`BudgetPolicy`]) says what a
/// breach does, and absent means `warn` once a budget is set; with none,
/// nothing is counted. `warn_at` is an early warning at a fraction of the
/// `window` budget. Under `wait` a dispatch that finds the window full
/// pauses in place, holding its seat, until the window has room.
///
/// **`concurrent_calls` is for an endpoint darkmux does NOT manage.** It is
/// how many calls to it run at once; absent, they run one at a time and
/// darkmux says so once per process (it never guesses a number). On a managed
/// endpoint darkmux's scheduler owns parallelism (residency and the
/// backend's parallel slots), so declaring it there is refused
/// ([`ModelEndpoint::validate`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UsageLimits {
    /// Tokens one dispatch (one role execution) may spend at this endpoint,
    /// managed or not. Reaching it is surfaced once (`warn`) and the dispatch
    /// keeps going; `0` is no cap (a `0` bound is unbounded, never
    /// "instantly"). A whole-run budget is `window`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_dispatch: Option<u64>,
    /// Calls in flight at once, on an endpoint darkmux does not manage;
    /// refused on a managed one. Absent: one at a time, within ONE darkmux
    /// process (two missions, radio or a fleet job at the same endpoint are not
    /// serialized together). `0` is unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrent_calls: Option<u32>,
    /// A budget over a rolling period of time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<UsageWindow>,
    /// What a breach does. Absent: `warn` once a budget is set (with no
    /// budget set, nothing is counted either way). `wait` needs a `window`.
    /// Read leniently: an unknown value is kept and refused by name at
    /// preflight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Lenient<BudgetPolicy>>,
    /// An early warning at this fraction of the budget (e.g. `0.8`), ahead
    /// of the at-limit one. Unset: only the at-limit warning fires; darkmux
    /// never picks a threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_at: Option<f64>,
    /// Forward-compat overflow.
    #[serde(flatten)]
    #[schemars(skip)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// A usage budget over a ROLLING period (`"period": "1d"` is the last 24
/// hours from now, with no calendar reset), in tokens, calls, or both.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UsageWindow {
    /// `<n>m`, `<n>h` or `<n>d`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls: Option<u64>,
    #[serde(flatten)]
    #[schemars(skip)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#3035) How many calls to an unmanaged endpoint run at once, given its declared
/// `limits.concurrent_calls`: the number (`0` is unbounded, the darkmux bound
/// convention), else one (darkmux never guesses a number). The one rule; the
/// scheduler's batches and a fleet receiver's seat book both ask here.
pub fn concurrent_width(declared: Option<u32>) -> usize {
    declared.map_or(1, |n| crate::config_access::jobs_at_once(n as usize))
}

impl UsageWindow {
    /// True when a number is set: a window with neither `tokens` nor
    /// `calls` (the shipped example's all-null shape) is no budget.
    pub fn is_set(&self) -> bool {
        self.tokens.is_some() || self.calls.is_some()
    }

    /// The period in seconds, when it parses.
    pub fn period_secs(&self) -> Option<u64> {
        self.period.as_deref().and_then(period_secs)
    }
}

/// A window budget ready to enforce: the parsed period and the numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowBudget {
    pub period_secs: u64,
    pub tokens: Option<u64>,
    pub calls: Option<u64>,
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

    /// True when any spend budget is set: the rolling `window`, or a
    /// per-dispatch cap (a `0` cap is no cap).
    pub fn has_budget(&self) -> bool {
        self.window.as_ref().is_some_and(UsageWindow::is_set) || self.dispatch_cap().is_some()
    }

    /// The per-dispatch token cap, when one is set (`0` is none).
    pub fn dispatch_cap(&self) -> Option<u64> {
        self.tokens_per_dispatch.filter(|n| *n > 0)
    }

    /// The window budget, when one is set and its period parses.
    pub fn window_budget(&self) -> Option<WindowBudget> {
        let w = self.window.as_ref().filter(|w| w.is_set())?;
        Some(WindowBudget { period_secs: w.period_secs()?, tokens: w.tokens, calls: w.calls })
    }

    /// The policy in force. Written: that value (an unregistered one is an
    /// error naming it). Absent: `warn` when a budget is set (a window or a
    /// per-dispatch cap), `off` otherwise, so a budget the operator writes
    /// warns by default and no budget counts nothing.
    pub fn resolved_policy(&self) -> Result<BudgetPolicy, String> {
        match &self.policy {
            Some(Lenient::Known(p)) => Ok(*p),
            Some(Lenient::Unrecognized(raw)) => Err(raw.as_str().map(str::to_string).unwrap_or_else(|| raw.to_string())),
            None if self.has_budget() => Ok(BudgetPolicy::Warn),
            None => Ok(BudgetPolicy::Off),
        }
    }

    /// A counting policy needs a budget to govern, and `wait` needs a rolling
    /// window (a dispatch's own spend never expires, so nothing would free
    /// room under `tokens_per_dispatch` alone).
    fn validate_policy_has_a_budget(&self, p: BudgetPolicy) -> Result<(), String> {
        if p.counts() && !self.has_budget() {
            return Err(format!(
                "limits.policy `{}` is set but no budget is: a policy with no budget governs \
                 nothing. Set `tokens_per_dispatch` or `window` (`period` and `tokens` or `calls`), \
                 or drop `policy`",
                crate::config_enum::ConfigEnum::token(p)
            ));
        }
        if p == BudgetPolicy::Wait && !self.window.as_ref().is_some_and(UsageWindow::is_set) {
            return Err("limits.policy `wait` needs a rolling `window` budget: a dispatch's own spend never \
                 expires, so nothing would free room under `tokens_per_dispatch` alone. Set `window`, \
                 or use `warn`"
                .to_string());
        }
        Ok(())
    }

    /// Shape checks: the window's period, `warn_at`'s range, and the
    /// policy's value.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(Lenient::Known(p)) = &self.policy {
            self.validate_policy_has_a_budget(*p)?;
        }
        if let Some(w) = &self.window {
            // (zero doctrine) A `0` on a darkmux bound means unbounded,
            // never "instantly"; a zero budget would be an eternal wait, so
            // it is refused rather than read either way. Under `off` the
            // number is inert (nothing is counted), so it is left alone:
            // `off` is exactly what the refusal tells the operator to set.
            let off = matches!(&self.policy, Some(Lenient::Known(p)) if !p.counts());
            for (field, n) in [("tokens", w.tokens), ("calls", w.calls)] {
                if !off && n == Some(0) {
                    return Err(format!(
                        "limits.window.{field} is 0: 0 is not a budget; set policy off to turn it off"
                    ));
                }
            }
            if !w.is_set() && w.period.is_some() {
                return Err("limits.window sets neither `tokens` nor `calls`".to_string());
            }
            if w.is_set() {
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
        }
        if let Some(f) = self.warn_at {
            if !(f.is_finite() && f > 0.0 && f < 1.0) {
                return Err(format!(
                    "limits.warn_at must be a fraction between 0 and 1 (e.g. 0.8), got {f}"
                ));
            }
        }
        if let Err(raw) = self.resolved_policy() {
            return Err(format!(
                "limits.policy `{raw}` is not a budget policy; valid: {}",
                <BudgetPolicy as crate::config_enum::ConfigEnum>::TOKENS.join(", ")
            ));
        }
        Ok(())
    }
}

/// `<n>m`, `<n>h` or `<n>d` in seconds, `n >= 1`.
pub fn period_secs(p: &str) -> Option<u64> {
    let unit = p.chars().last()?;
    let n: u64 = p[..p.len() - unit.len_utf8()].parse().ok().filter(|n| *n >= 1)?;
    let per = match unit {
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        _ => return None,
    };
    n.checked_mul(per)
}

/// `<n>m`, `<n>h` or `<n>d` with `n >= 1`.
fn is_period(p: &str) -> bool {
    period_secs(p).is_some()
}

/// (#2902 review M1) A value read leniently: the known shape, or whatever was
/// written when it does not match (a newer darkmux's value, a typo). An
/// unknown value never fails the registry parse (contract 7); the consumer
/// that needs the value refuses it by name, and `darkmux doctor` reports it.
/// It serializes back as the JSON value that was read (a string or object
/// verbatim; a number outside the integer range comes back in float form,
/// e.g. `1e+23`). Nothing writes `profiles.json` today, so this matters only
/// to `profile list --json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Lenient<T> {
    Known(T),
    Unrecognized(serde_json::Value),
}

impl<T> From<T> for Lenient<T> {
    fn from(v: T) -> Self {
        Lenient::Known(v)
    }
}

impl<T> Lenient<T> {
    /// The known value, or the raw one as a compact string for a message.
    pub fn known(&self) -> Result<&T, String> {
        match self {
            Lenient::Known(v) => Ok(v),
            Lenient::Unrecognized(raw) => Err(raw.to_string()),
        }
    }
}

/// How a profile model's endpoint was written. Runtime-only: set by the
/// deserializer and the registry loader, never serialized itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum EndpointSource {
    /// An `endpoints` entry itself, or an inline object on a mission step's
    /// `config.endpoint`. Never a profile model's own endpoint.
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelEndpoint {
    /// Base URL of an UNMANAGED OpenAI-compatible server, up to (not
    /// including) `/chat/completions`, e.g. `https://api.openai.com/v1`.
    /// A managed endpoint has none: its address is `lmstudio_url`. An
    /// endpoint with a `url` is one darkmux only sends requests to; one with
    /// neither `url` nor `managed` is refused (there is no implicit kind).
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
    /// [`EndpointKind`]). An endpoint declares `managed` or a `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<Lenient<ManagedBackend>>,
    /// The request shape, when it differs from the kind's default
    /// ([`EndpointKind::default_dialect`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<Lenient<Dialect>>,
    /// Usage limits and the budget policy (see [`UsageLimits`] for which
    /// are enforced).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Lenient<UsageLimits>>,
    /// How this value was written. Runtime-only.
    #[serde(skip)]
    pub source: EndpointSource,
    /// Forward-compat overflow.
    #[serde(flatten)]
    #[schemars(skip)]
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
    /// The managed LM Studio endpoint a model with no `endpoint` runs on.
    pub fn managed_lmstudio() -> Self {
        ModelEndpoint { managed: Some(ManagedBackend::Lmstudio.into()), ..Default::default() }
    }

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

    /// (#3035) The key this endpoint's calls claim a seat under, wherever a seat is
    /// counted (the scheduler's batches, a fleet receiver's seat book): its
    /// `endpoints` id, else (an inline endpoint, which has none) its URL, else a
    /// placeholder. The one derivation; no consumer spells its own.
    pub fn seat_key(&self) -> String {
        self.named_id()
            .map(str::to_string)
            .or_else(|| self.url.clone())
            .unwrap_or_else(|| "(unnamed endpoint)".to_string())
    }

    /// THE classification: what darkmux does at this endpoint. An unresolved
    /// reference is an error, never a guess (a dangling id treated as the
    /// managed default would send the request to LM Studio unannounced).
    pub fn kind(&self) -> Result<EndpointKind, EndpointError> {
        if let EndpointSource::Unresolved(id) = &self.source {
            return Err(EndpointError(format!(
                "darkmux: endpoint `{id}` is named by id but the profile registry's `endpoints` map \
                 does not define it, or its entry failed to parse and was quarantined. Add or fix \
                 `\"endpoints\": {{ \"{id}\": {{ ... }} }}` in profiles.json (or fix the id); \
                 `darkmux doctor` names which. (#2902)"
            )));
        }
        let kind = match (&self.managed, &self.url) {
            (Some(Lenient::Known(backend)), _) => EndpointKind::Managed(*backend),
            (Some(Lenient::Unrecognized(raw)), _) => {
                return Err(EndpointError(format!(
                    "darkmux: endpoint `managed` is {raw}, which this darkmux does not know (it knows \
                     {}, or no `managed` for an endpoint darkmux only sends requests to). \
                     A newer darkmux may have written it; `darkmux doctor` names the entry. (#2902)",
                    quoted_tokens::<ManagedBackend>()
                )))
            }
            (None, None) => {
                return Err(EndpointError(
                    "darkmux: an endpoint declares what darkmux does at it: `\"managed\": \"lmstudio\"` for a \
                     server darkmux loads models into, or a `url` for one it only sends requests to. \
                     This one declares neither (4.0 has no implicit kind). (#2902)"
                        .to_string(),
                ))
            }
            (None, Some(_)) => EndpointKind::Unmanaged,
        };
        // (#2902 review M3, C5) A managed endpoint's address is the machine's
        // `lmstudio_url` and its request shape is LM Studio's. Declaring
        // either otherwise is refused here, at use, rather than silently
        // sent to the local LM Studio. Only an EXPLICIT `managed` refuses a
        // `url`/`api_version` (without one, a `url` means unmanaged).
        if kind.is_managed() {
            let explicit = self.managed.is_some();
            if explicit && self.url.is_some() {
                return Err(EndpointError(format!(
                    "darkmux: a managed endpoint has no `url` of its own; darkmux sends to the \
                     machine's LM Studio at `lmstudio_url`. Drop `url` (and set \
                     `darkmux config set lmstudio_url <url>` if the server moved), or drop `managed` \
                     if darkmux should only send requests to {}. (#2902)",
                    self.url.as_deref().unwrap_or_default()
                )));
            }
            if explicit && self.api_version.is_some() {
                return Err(EndpointError(
                    "darkmux: `api_version` belongs to an unmanaged endpoint's URL; a managed \
                     endpoint never sends it. Drop `api_version`, or drop `managed`. (#2902)"
                        .to_string(),
                ));
            }
            match &self.dialect {
                None | Some(Lenient::Known(Dialect::ChatCompletionsMaxTokens)) => {}
                Some(d) => {
                    let named = match d {
                        Lenient::Known(k) => format!("\"{}\"", k.as_str()),
                        Lenient::Unrecognized(raw) => raw.to_string(),
                    };
                    return Err(EndpointError(format!(
                        "darkmux: a managed (LM Studio) endpoint speaks \"{}\"; its declared \
                         `dialect` {named} would not be honored. Drop `dialect`. (#2902)",
                        Dialect::ChatCompletionsMaxTokens.as_str()
                    )));
                }
            }
        }
        Ok(kind)
    }

    /// The request shape: declared, else the kind's default.
    pub fn resolved_dialect(&self) -> Result<Dialect, EndpointError> {
        let kind = self.kind()?;
        match &self.dialect {
            None => Ok(kind.default_dialect()),
            Some(Lenient::Known(d)) => Ok(*d),
            Some(Lenient::Unrecognized(raw)) => Err(EndpointError(format!(
                "darkmux: endpoint `dialect` is {raw}, which this darkmux does not know (it knows \
                 {}). (#2902)",
                quoted_tokens::<Dialect>()
            ))),
        }
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

    /// The limits line for doctor: the summary, `(unreadable)` when the
    /// value did not parse, empty when none are set.
    pub fn limits_summary(&self) -> String {
        match &self.limits {
            None => String::new(),
            Some(Lenient::Known(l)) => l.summary(),
            Some(Lenient::Unrecognized(_)) => "(unreadable)".to_string(),
        }
    }

    /// (#2902 step 5) The endpoint's readable limits, `None` when absent or
    /// unreadable (an unreadable `limits` is `validate`'s finding).
    pub fn known_limits(&self) -> Option<&UsageLimits> {
        self.limits.as_ref().and_then(|l| l.known().ok())
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
        let dialect = self.resolved_dialect().map_err(|e| e.0)?;
        if let Some(u) = &self.url {
            if !(u.starts_with("http://") || u.starts_with("https://")) {
                return Err(format!("endpoint.url must start with http:// or https:// (got {u:?})"));
            }
        }
        if self.api_version.is_some() && kind.is_managed() {
            return Err("`api_version` applies to an unmanaged endpoint's URL; a managed endpoint never sends it".to_string());
        }
        if self.reasoning_effort.is_some() && dialect == Dialect::ChatCompletionsMaxTokens && self.dialect.is_some() {
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
                return Err("endpoint.auth.type is set but no credential source is declared: set \
                     endpoint.auth.keychain (a macOS Keychain item name) or endpoint.auth.key_env \
                     (the NAME of an env var holding the key)"
                    .to_string());
            }
        }
        self.validate_limits()
    }

    /// THE rule (#3035) that `limits.concurrent_calls` is for an endpoint
    /// darkmux does not manage: `Err` with the operator's message when a
    /// managed endpoint declares it. Preflight, doctor and the runtime gate
    /// all ask here.
    pub fn concurrent_calls_allowed(&self) -> Result<(), String> {
        let declared = self.known_limits().is_some_and(|l| l.concurrent_calls.is_some());
        if declared && self.kind().is_ok_and(EndpointKind::is_managed) {
            return Err(CONCURRENT_CALLS_ON_MANAGED.to_string());
        }
        Ok(())
    }

    /// The endpoint's `limits` as written: readable, every shape rule, and no
    /// `concurrent_calls` on a managed endpoint (#3035).
    fn validate_limits(&self) -> Result<(), String> {
        match &self.limits {
            None => Ok(()),
            Some(Lenient::Known(limits)) => {
                limits.validate()?;
                self.concurrent_calls_allowed()
            }
            Some(Lenient::Unrecognized(raw)) => Err(format!(
                "`limits` could not be read ({raw}): expect `tokens_per_dispatch` and \
                 `concurrent_calls` as numbers and a `window` object"
            )),
        }
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

/// (#3035) Why `limits.concurrent_calls` is refused on a managed endpoint.
const CONCURRENT_CALLS_ON_MANAGED: &str = "`limits.concurrent_calls` is for an endpoint darkmux does not \
     manage: on a managed endpoint darkmux's scheduler owns parallelism (residency and the backend's parallel \
     slots), so it is not yours to set. Remove it; the spend limits (`tokens_per_dispatch`, `window`, `policy`, \
     `warn_at`) do apply here";

/// Auth for an endpoint. The secret is **never** stored here — only where it
/// lives: a macOS Keychain item *name* and/or an environment variable's
/// *name*. The machine holding the item is the endpoint's keymaster.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    #[schemars(skip)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// The header mechanics for an endpoint's auth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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

/// The schema of `ProfileModel.endpoint` (read through [`endpoint_field`]): an
/// id naming an `endpoints` entry. Used only as `#[schemars(with)]`.
pub struct EndpointFieldSchema;

impl schemars::JsonSchema for EndpointFieldSchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "EndpointField".into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let _ = generator;
        schemars::json_schema!({"type": "string"})
    }
}

/// `serde(with)` for `ProfileModel.endpoint`: a string names an `endpoints`
/// entry by id, and nothing else reads (an inline object is refused, naming
/// the rewrite). A named endpoint serializes back as its id (a quarantined
/// `endpoints` entry is not in the loaded registry at all, like a
/// quarantined profile).
pub(crate) mod endpoint_field {
    use super::*;

    pub fn serialize<S: Serializer>(v: &Option<ModelEndpoint>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            None => s.serialize_none(),
            Some(ep) => match ep.named_id() {
                Some(id) => s.serialize_some(id),
                // Only an in-memory value built without an id has none; a
                // loaded registry's model endpoint always names one.
                None => s.serialize_some(ep),
            },
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<ModelEndpoint>, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<ModelEndpoint>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(
                    "an endpoint id: a string naming an `endpoints` entry (an inline endpoint object was \
                     removed in 4.0: declare it once under `endpoints` and name it by id)",
                )
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
        }
        d.deserialize_option(V)
    }
}

#[cfg(test)]
mod tests {

    /// (#2947) `serde`'s spelling of each endpoint enum value and its
    /// `ConfigEnum` token are the same string, for every variant, so the
    /// help/doctor value list is exactly what the deserializer accepts.
    #[test]
    fn serde_spelling_matches_the_config_enum_token() {
        use crate::config_enum::ConfigEnum;
        fn check<T: ConfigEnum + serde::Serialize + serde::de::DeserializeOwned>() {
            for t in T::TOKENS {
                let v = T::from_token(t).unwrap();
                assert_eq!(serde_json::to_value(v).unwrap(), serde_json::json!(t));
                assert_eq!(serde_json::from_value::<T>(serde_json::json!(t)).unwrap(), v);
            }
        }
        check::<ManagedBackend>();
        check::<Dialect>();
    }

    use super::*;
    use crate::ProfileModel;

    /// (#3035) One width rule: absent is one at a time, `0` is unbounded.
    #[test]
    fn concurrent_width_is_one_unless_declared_and_zero_is_unbounded() {
        assert_eq!(concurrent_width(None), 1);
        assert_eq!(concurrent_width(Some(3)), 3);
        assert_eq!(concurrent_width(Some(0)), usize::MAX);
    }

    /// (#3035) One key rule: the id, else the URL, else a placeholder.
    #[test]
    fn seat_key_is_the_id_else_the_url() {
        assert_eq!(ModelEndpoint::reference("azure").seat_key(), "azure");
        let inline = ModelEndpoint { url: Some("https://x.test/v1".into()), ..Default::default() };
        assert_eq!(inline.seat_key(), "https://x.test/v1");
        assert_eq!(ModelEndpoint::default().seat_key(), "(unnamed endpoint)");
    }

    fn pm(json: &str) -> ProfileModel {
        serde_json::from_str(json).unwrap()
    }

    /// What darkmux does at an endpoint is declared, never inferred: `managed`
    /// or a `url`. A model with no endpoint is the managed default; an
    /// endpoint declaring neither is refused.
    #[test]
    fn kind_is_declared_and_never_implied() {
        let r = registry(
            r#"{"profiles":{"p":{"models":[
                    {"id":"absent","n_ctx":1},
                    {"id":"hosted","endpoint":"hosted"},
                    {"id":"lms","n_ctx":1,"endpoint":"lms"},
                    {"id":"bare","n_ctx":1,"endpoint":"bare"}]}},
                "endpoints":{"hosted":{"url":"http://localhost:1234/v1"},
                             "lms":{"managed":"lmstudio"},
                             "bare":{"reasoning_effort":"high"}}}"#,
        );
        let models = &r.profiles["p"].models;
        let managed = EndpointKind::Managed(ManagedBackend::Lmstudio);
        assert_eq!(models[0].endpoint_kind().unwrap(), managed, "no endpoint: the managed default");
        assert_eq!(models[1].endpoint_kind().unwrap(), EndpointKind::Unmanaged, "a url: darkmux only sends");
        assert_eq!(models[2].endpoint_kind().unwrap(), managed);
        assert!(models[2].is_managed() && !models[1].is_managed());
        let err = models[3].endpoint_kind().unwrap_err().to_string();
        assert!(err.contains("neither") && err.contains("managed") && err.contains("url"), "{err}");
        assert!(!models[3].is_managed(), "an endpoint of no declared kind is never loaded on a guess");
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

    /// A profile model's inline endpoint object is refused at parse, naming
    /// the rewrite.
    #[test]
    fn an_inline_object_is_refused_naming_the_rewrite() {
        for object in [r#"{"url":"https://h/v1"}"#, r#"{"url":5}"#, "{}"] {
            let err = serde_json::from_str::<ProfileModel>(&format!(r#"{{"id":"m","endpoint":{object}}}"#))
                .unwrap_err()
                .to_string();
            assert!(err.contains("removed in 4.0") && err.contains("`endpoints`"), "{object}: {err}");
        }
    }

    #[test]
    fn dialect_defaults_by_kind_and_can_be_declared() {
        let managed = ModelEndpoint::managed_lmstudio();
        assert_eq!(managed.resolved_dialect().unwrap(), Dialect::ChatCompletionsMaxTokens);
        let hosted = ModelEndpoint { url: Some("https://h/v1".into()), ..Default::default() };
        assert_eq!(hosted.resolved_dialect().unwrap(), Dialect::ChatCompletions);
        let declared = ModelEndpoint {
            url: Some("https://h/v1".into()),
            dialect: Some(Dialect::ChatCompletionsMaxTokens.into()),
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
        let managed = ModelEndpoint::managed_lmstudio().chat_url().unwrap();
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
        assert_eq!(ModelEndpoint::managed_lmstudio().host(), None);
        assert_eq!(url_host("no-scheme.example/v1"), None);
    }

    #[test]
    fn validate_refuses_what_cannot_work_and_names_it() {
        let bad = ModelEndpoint { url: Some("example.azure.com".into()), ..Default::default() };
        assert!(bad.validate().unwrap_err().contains("http://"));
        let managed_with_url = ModelEndpoint {
            url: Some("http://localhost:1234".into()),
            managed: Some(ManagedBackend::Lmstudio.into()),
            ..Default::default()
        };
        assert!(managed_with_url.validate().unwrap_err().contains("lmstudio_url"));
        let effort_ignored = ModelEndpoint {
            url: Some("https://h/v1".into()),
            dialect: Some(Dialect::ChatCompletionsMaxTokens.into()),
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
            limits: Some(
                UsageLimits {
                    window: Some(UsageWindow { period: Some("weekly".into()), tokens: Some(1), ..Default::default() }),
                    ..Default::default()
                }
                .into(),
            ),
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
            limits: Some(UsageLimits { tokens_per_dispatch: Some(5), ..Default::default() }.into()),
            ..Default::default()
        };
        assert_eq!(ok.validate(), Ok(()));
    }

    /// (#2902 step 5, zero doctrine) A window of 0 tokens or 0 calls is not
    /// a budget: refused, naming the field and the way to turn a budget
    /// off. Unset stays fine, and so does `policy: off` with a real number.
    #[test]
    fn a_zero_window_is_refused_and_names_policy_off() {
        let with_policy = |policy: Option<BudgetPolicy>, tokens: Option<u64>, calls: Option<u64>| ModelEndpoint {
            url: Some("https://h/v1".into()),
            limits: Some(
                UsageLimits {
                    policy: policy.map(Lenient::Known),
                    window: Some(UsageWindow { period: Some("1d".into()), tokens, calls, ..Default::default() }),
                    ..Default::default()
                }
                .into(),
            ),
            ..Default::default()
        };
        let with = |tokens: Option<u64>, calls: Option<u64>| with_policy(None, tokens, calls);
        let t = with(Some(0), None).validate().unwrap_err();
        assert!(t.contains("limits.window.tokens is 0") && t.contains("set policy off"), "{t}");
        let c = with(None, Some(0)).validate().unwrap_err();
        assert!(c.contains("limits.window.calls is 0") && c.contains("set policy off"), "{c}");
        assert_eq!(with(Some(1), Some(1)).validate(), Ok(()));
        // (5th review MF2) Under `off` the number is inert: the operator did
        // what the refusal asks, so it is not refused. `warn` and `wait` are.
        assert_eq!(with_policy(Some(BudgetPolicy::Off), Some(0), Some(0)).validate(), Ok(()), "off + 0 is fine");
        for p in [BudgetPolicy::Warn, BudgetPolicy::Wait] {
            assert!(with_policy(Some(p), Some(0), None).validate().unwrap_err().contains("is 0"), "{p:?} + 0");
        }
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
        assert_eq!(ep.limits.as_ref().unwrap().known().unwrap().tokens_per_dispatch, Some(5));
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
                    {"id":"c"},
                    {"id":"d","n_ctx":1,"endpoint":"bad"}]}},
                "endpoints":{"bad":{"url":"ftp://nope"}}}"#,
        );
        let issues = r.validate();
        let errors: Vec<&str> = issues.iter().map(String::as_str).collect();
        assert!(errors.iter().any(|m| m.contains("\"missing\"") && m.contains("does not define")), "{errors:?}");
        assert!(errors.iter().any(|m| m.contains("endpoint \"bad\"") && m.contains("http://")), "{errors:?}");
        assert!(errors.iter().any(|m| m.contains("model \"c\"") && m.contains("n_ctx")), "{errors:?}");
        assert!(!errors.iter().any(|m| m.contains("model \"a\"") && m.contains("n_ctx")), "an unresolved endpoint is not a managed model missing n_ctx");
        assert_eq!(errors.len(), 3, "{errors:?}");
    }

    /// (#2902 review M1) The three new fields read leniently: a value this
    /// binary does not know (a NEWER darkmux's `"managed": "machine"`, a
    /// typo'd dialect, a limit written as a string) never fails the parse,
    /// serializes back as the value read, and is refused where it matters: at use for
    /// `managed`/`dialect` (they decide routing), in `validate` for all three.
    #[test]
    fn unknown_values_in_the_new_fields_read_leniently_and_are_refused_at_use() {
        let r = registry(
            r#"{"profiles":{"p":{"models":[
                    {"id":"dialect","endpoint":"dialect"},
                    {"id":"managed","n_ctx":1,"endpoint":"managed"},
                    {"id":"limits","endpoint":"limits"}]}},
                "endpoints":{
                    "dialect":{"url":"https://h/v1","dialect":"responses"},
                    "managed":{"managed":"machine"},
                    "limits":{"url":"https://h/v1","limits":{"tokens_per_dispatch":"500k"}}}}"#,
        );
        let models = &r.profiles["p"].models;
        let dialect = &models[0];
        let ep = dialect.endpoint.as_ref().unwrap();
        assert_eq!(ep.kind().unwrap(), EndpointKind::Unmanaged);
        assert!(ep.resolved_dialect().unwrap_err().to_string().contains("responses"));
        assert!(ep.validate().unwrap_err().contains("responses"));
        assert_eq!(serde_json::to_value(&r).unwrap()["endpoints"]["dialect"]["dialect"], "responses", "written back as read");

        let err = models[1].endpoint_kind().unwrap_err().to_string();
        assert!(err.contains("machine") && err.contains("lmstudio"), "{err}");
        assert!(!models[1].is_managed(), "an unknown kind is never loaded on a guess");

        let ep = models[2].endpoint.as_ref().unwrap();
        assert_eq!(ep.kind().unwrap(), EndpointKind::Unmanaged, "unreadable limits never decide routing");
        assert!(ep.validate().unwrap_err().contains("limits"), "{:?}", ep.validate());
        assert_eq!(ep.limits_summary(), "(unreadable)");
    }

    /// (#2902 review M3, C5) A managed endpoint's address is `lmstudio_url`
    /// and its request shape is LM Studio's: an explicit `managed` that also
    /// declares a `url`, an `api_version` or another dialect is refused at
    /// use (never silently sent to the local LM Studio). An endpoint that
    /// declares neither `managed` nor `url` is refused too.
    #[test]
    fn a_managed_endpoint_that_declares_an_address_or_dialect_is_refused_at_use() {
        for (json, needle) in [
            (r#"{"managed":"lmstudio","url":"http://h:1234"}"#, "lmstudio_url"),
            (r#"{"managed":"lmstudio","api_version":"v1"}"#, "api_version"),
            (r#"{"managed":"lmstudio","dialect":"chat-completions"}"#, "dialect"),
            (r#"{"dialect":"chat-completions"}"#, "neither"),
            (r#"{"api_version":"v1"}"#, "neither"),
        ] {
            let ep: ModelEndpoint = serde_json::from_str(json).unwrap();
            let err = ep.kind().unwrap_err().to_string();
            assert!(err.contains(needle), "{json}: {err}");
            assert!(ep.chat_url().is_err(), "{json}");
        }
        let ok: ModelEndpoint = serde_json::from_str(r#"{"managed":"lmstudio","dialect":"chat-completions-max-tokens"}"#).unwrap();
        assert_eq!(ok.kind().unwrap(), EndpointKind::Managed(ManagedBackend::Lmstudio));
    }

    /// (#2902 review C6) Each distinct inline endpoint gets a UNIQUE
    /// suggested id: two deployments on one host are told apart by the
    /// deployment, the same definition shared by two models gets one id, and
    /// an id `endpoints` already defines is never suggested again.
    #[test]
    fn inline_rewrites_suggest_a_unique_id_per_distinct_endpoint() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"profiles":{"p":{"models":[
                    {"id":"a","endpoint":{"url":"https://r.example/openai/deployments/gpt-4o","api_version":"v1"}},
                    {"id":"b","endpoint":{"url":"https://r.example/openai/deployments/gpt-5","api_version":"v1"}},
                    {"id":"c","endpoint":{"url":"https://r.example/openai/deployments/gpt-5","api_version":"v1"}},
                    {"id":"d","endpoint":{"url":"https://api.x.ai/v1"}},
                    {"id":"e","endpoint":"named"}]}},
                "endpoints":{"api.x.ai":{"url":"https://other.example/v1"}}}"#,
        )
        .unwrap();
        let rewrites = crate::ProfileRegistry::inline_endpoint_rewrites(&doc);
        let suggested: Vec<(&str, &str)> = rewrites.iter().map(|r| (r.path.as_str(), r.suggested_id.as_str())).collect();
        assert_eq!(
            suggested,
            [
                ("profiles.p.models[0].endpoint", "r.example-gpt-4o"),
                ("profiles.p.models[1].endpoint", "r.example-gpt-5"),
                ("profiles.p.models[2].endpoint", "r.example-gpt-5"),
                ("profiles.p.models[3].endpoint", "api.x.ai-2"),
            ],
            "one definition, one id; never an id `endpoints` already defines; a named endpoint is not inline"
        );
        let line = rewrites[3].line();
        assert!(line.contains("endpoints.\"api.x.ai-2\"") && line.contains("\"endpoint\": \"api.x.ai-2\""), "{line}");
    }

    fn ep(json: &str) -> ModelEndpoint {
        serde_json::from_str(json).unwrap()
    }

    /// (#3035) `concurrent_calls` is a limit on an endpoint darkmux does NOT
    /// manage. On a managed one the scheduler owns parallelism (residency and
    /// the backend's parallel slots), so declaring it is refused by
    /// `validate`, naming the scheduler; the spend limits stay legal there.
    /// The decision is `is_managed()`, never "is LM Studio".
    #[test]
    fn concurrent_calls_on_a_managed_endpoint_is_refused_naming_the_scheduler() {
        let managed = ep(r#"{"managed":"lmstudio","limits":{"concurrent_calls":2}}"#);
        let err = managed.validate().unwrap_err();
        assert!(err.contains("concurrent_calls") && err.contains("scheduler"), "{err}");
        let spend = ep(
            r#"{"managed":"lmstudio","limits":{"tokens_per_dispatch":9000,"policy":"warn","warn_at":0.8,
                "window":{"period":"1d","tokens":100000}}}"#,
        );
        assert!(spend.validate().is_ok(), "spend limits apply to a managed endpoint: {:?}", spend.validate());
        let unmanaged = ep(r#"{"url":"https://h/v1","limits":{"concurrent_calls":2}}"#);
        assert!(unmanaged.validate().is_ok(), "{:?}", unmanaged.validate());
    }

    /// (#3035) The per-dispatch cap is a budget in its own right: with only
    /// `tokens_per_dispatch` set the policy defaults to `warn`; `wait` needs a
    /// rolling `window` (a dispatch's own spend never expires, so nothing
    /// would free room); a policy with no budget at all still refuses.
    #[test]
    fn the_per_dispatch_cap_counts_by_default_and_wait_needs_a_window() {
        let cap_only: UsageLimits = serde_json::from_str(r#"{"tokens_per_dispatch":9000}"#).unwrap();
        assert_eq!(cap_only.resolved_policy(), Ok(BudgetPolicy::Warn));
        assert!(cap_only.validate().is_ok());
        let off: UsageLimits = serde_json::from_str(r#"{"tokens_per_dispatch":9000,"policy":"off"}"#).unwrap();
        assert_eq!(off.resolved_policy(), Ok(BudgetPolicy::Off));
        let wait_no_window: UsageLimits = serde_json::from_str(r#"{"tokens_per_dispatch":9000,"policy":"wait"}"#).unwrap();
        let err = wait_no_window.validate().unwrap_err();
        assert!(err.contains("wait") && err.contains("window"), "{err}");
        let nothing: UsageLimits = serde_json::from_str(r#"{"policy":"warn"}"#).unwrap();
        assert!(nothing.validate().is_err(), "a policy with no budget governs nothing");
        assert_eq!(UsageLimits::default().resolved_policy(), Ok(BudgetPolicy::Off), "no budget, nothing counted");
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
