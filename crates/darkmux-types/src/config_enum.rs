//! (#2947) Enum-typed settings: one declaration, one registry, one rule.
//!
//! **The rule.** An unregistered value in an enum-typed setting is bad
//! config. It is never resolved to a fallback, in either direction. Every
//! entry point that COULD consume it refuses at preflight (whether or not
//! one particular run through it would read the value), naming the raw
//! value, where it was set, and the valid values; `darkmux doctor` reports it
//! as Fail; `darkmux config set` refuses it; and help lists the valid values
//! with their meanings.
//!
//! **Reading stays lenient** (contract 7). Values are stored as strings in
//! `config.json` and parsed here, at the accessor, so one typo never discards
//! the whole document (the CONFIG 1.27 lesson: a derived enum's hard error
//! on an unknown variant made `DarkmuxConfig::load_from`'s
//! `unwrap_or_default()` drop every other setting). What this module changes
//! is consumption: it refuses instead of guessing.
//!
//! **Two pieces, and everything else derives from them:**
//!
//! 1. [`ConfigEnum`], implemented through [`config_enum!`]. One declarative
//!    block per enum gives each value its token and a one-line meaning. The
//!    macro also generates an exhaustive `match` from variant to token, so a
//!    variant added to the Rust enum without a row here fails to COMPILE
//!    rather than drifting from the value list.
//! 2. [`ENUM_SETTINGS`], the registry: each entry names the dotted
//!    `config.json` key, the env var (if any), the enum, the shipped value,
//!    and the entry-point [`Scope`]s that could consume it.
//!
//! From those two: the typed accessors (`config_access::fleet_mode` and
//! friends) return the value or a [`BadEnumValue`]; [`preflight`] runs one
//! pass per entry-point scope; `darkmux doctor` runs one generic check over
//! [`bad_values`]; `darkmux config set` validates any registered key
//! through [`EnumSetting::canonical`]; and help renders [`help_block`].
//!
//! **Adding an enum setting** is: implement [`ConfigEnum`] with
//! [`config_enum!`], add an [`EnumSetting::of`] entry to [`ENUM_SETTINGS`],
//! and write the typed accessor as a one-line call to
//! `config_access::resolve_enum`. Nothing else. The per-step cap's
//! `remote.step_budget_policy` (#2902 step 5) is exactly those three lines
//! plus a `Scope` list. The conformance tests below iterate the
//! registry, so the new entry inherits the preflight/doctor/config-set/help
//! assertions without a test of its own, and `every_enum_in_the_config_schema_is_registered`
//! fails until an enum declared in the config schema is registered.

use crate::config::DarkmuxConfig;
use crate::config_access::Source;

/// An enum whose values are tokens in a setting. Implement it with
/// [`config_enum!`], never by hand: the macro is what ties the token table
/// to the Rust variants with an exhaustive `match`.
pub trait ConfigEnum: Copy + Eq + std::fmt::Debug + 'static {
    /// The Rust type name, so the unregistered-enum test can match a
    /// declaration in the schema source to its registry entry.
    const RUST_NAME: &'static str;
    /// What one value of this enum IS, for messages ("thermal state").
    const KIND: &'static str;
    /// `(token, meaning)` for every value, in declaration order.
    const TABLE: &'static [(&'static str, &'static str)];
    /// Just the tokens, in declaration order.
    const TOKENS: &'static [&'static str];
    /// Retired spellings: `(old token, the token it was renamed to)`. An old
    /// token is still refused (it is not a value), but the refusal names
    /// the new word instead of only listing the valid ones.
    const RETIRED: &'static [(&'static str, &'static str)];

    /// The canonical token (lowercase, as stored in `config.json`).
    fn token(self) -> &'static str;
    /// Exact-match lookup of a canonical token.
    fn from_token(canonical: &str) -> Option<Self>;

    /// The operator-facing parse: trimmed and case-insensitive, so `Serious`
    /// and ` serious ` mean `serious`. `None` for anything not in
    /// [`Self::TABLE`]; the caller reports it, it never guesses.
    fn parse(raw: &str) -> Option<Self> {
        Self::from_token(&raw.trim().to_ascii_lowercase())
    }

    /// The one-line meaning of this value.
    fn meaning(self) -> &'static str {
        let t = self.token();
        Self::TABLE.iter().find(|(k, _)| *k == t).map(|(_, m)| *m).unwrap_or("")
    }
}

/// Implement [`ConfigEnum`] for an existing fieldless enum.
///
/// ```ignore
/// config_enum!(FleetMode, "fleet position", [
///     Standalone = "standalone" => "a single machine that coordinates nothing",
///     Hub = "hub" => "the always-on coordinator",
///     Peer = "peer" => "points at a hub",
/// ]);
/// ```
///
/// The generated `token` is an exhaustive `match`, so a variant with no row
/// here is a compile error, and a row naming no variant is one too.
#[macro_export]
macro_rules! config_enum {
    ($ty:ident, $kind:literal, [ $( $variant:ident = $token:literal => $meaning:literal ),+ $(,)? ]) => {
        $crate::config_enum!($ty, $kind, [ $( $variant = $token => $meaning ),+ ], retired: []);
    };
    (
        $ty:ident, $kind:literal,
        [ $( $variant:ident = $token:literal => $meaning:literal ),+ $(,)? ],
        retired: [ $( $old:literal => $new:ident ),* $(,)? ]
    ) => {
        impl $ty {
            #[doc(hidden)]
            #[allow(dead_code)]
            const fn __config_enum_token(self) -> &'static str {
                match self { $( $ty::$variant => $token ),+ }
            }
        }
        impl $crate::config_enum::ConfigEnum for $ty {
            const RUST_NAME: &'static str = stringify!($ty);
            const KIND: &'static str = $kind;
            const TABLE: &'static [(&'static str, &'static str)] = &[ $( ($token, $meaning) ),+ ];
            const TOKENS: &'static [&'static str] = &[ $( $token ),+ ];
            // A retired spelling names the VARIANT it became, and its token
            // comes from the same exhaustive match as `token()`, so a rename
            // can never point at a word that is not a value.
            const RETIRED: &'static [(&'static str, &'static str)] =
                &[ $( ($old, $ty::$new.__config_enum_token()) ),* ];
            fn token(self) -> &'static str {
                self.__config_enum_token()
            }
            fn from_token(canonical: &str) -> Option<Self> {
                match canonical { $( $token => Some($ty::$variant), )+ _ => None }
            }
        }
    };
}

/// An entry point that consumes settings before it starts work. Each one
/// calls [`preflight`] with its own scope before minting a run, a session,
/// or a mission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every dispatch: the `darkmux dispatch` verb, and the dispatch
    /// primitives (`crew::dispatch::dispatch`, `dispatch_local_single_shot`)
    /// that radio, acp, mission steps, crawl units, the lab providers and
    /// the fleet runner all route through.
    Dispatch,
    /// `darkmux mission launch` (and ACP panel launches, which call it).
    MissionLaunch,
    /// `darkmux lab run` and the lab verbs built on `lab::run::lab_run`
    /// (`loop`, `characterize`, `tune`), plus `lab eval`.
    LabRun,
    /// Fleet work submission: the daemon's work-submission listener and the
    /// sending side, both of which build the identity provider.
    FleetSubmission,
}

impl Scope {
    pub const ALL: [Scope; 4] = [Scope::Dispatch, Scope::MissionLaunch, Scope::LabRun, Scope::FleetSubmission];

    /// How the refusal names the entry point.
    pub fn label(self) -> &'static str {
        match self {
            Scope::Dispatch => "dispatch",
            Scope::MissionLaunch => "mission launch",
            Scope::LabRun => "lab run",
            Scope::FleetSubmission => "fleet work submission",
        }
    }
}

/// How a registered setting's raw value(s) are read from the config.
#[derive(Clone, Copy)]
pub enum Read {
    /// One value: a scalar key, with the env tier above it when the entry
    /// has an env var.
    One(fn(&DarkmuxConfig) -> Option<&str>),
    /// One value per item of a list (`hooks.rules[].match.level`): each as
    /// `(its concrete path, raw value)`. No env tier and no shipped value:
    /// an absent field means the item does not constrain it.
    Each(fn(&DarkmuxConfig) -> Vec<(String, String)>),
}

/// One enum-typed setting. Built only through [`EnumSetting::of`] /
/// [`EnumSetting::each`], which is what keeps `values` bound to a real
/// [`ConfigEnum`].
#[derive(Clone, Copy)]
pub struct EnumSetting {
    /// The dotted `config.json` key (for a per-item setting, the pattern,
    /// e.g. `hooks.rules[].match.level`).
    pub key: &'static str,
    /// The env var that overrides it, if it has one.
    pub env: Option<&'static str>,
    /// [`ConfigEnum::RUST_NAME`] of the enum.
    pub rust_name: &'static str,
    /// [`ConfigEnum::KIND`].
    pub kind: &'static str,
    /// [`ConfigEnum::TABLE`].
    pub values: &'static [(&'static str, &'static str)],
    /// [`ConfigEnum::RETIRED`].
    pub retired: &'static [(&'static str, &'static str)],
    /// The value an absent setting resolves to (a token of `values`,
    /// pinned by `every_shipped_value_is_a_registered_token`). `None` for a
    /// per-item setting, where absent means "no constraint".
    pub shipped: Option<&'static str>,
    /// The entry points that could consume this setting, and so refuse on
    /// a bad value, even for a run that would not read it (conservative on
    /// purpose: bad config is bad config). Empty only with a `no_scope_reason`.
    pub scopes: &'static [Scope],
    /// Why no entry point preflights this setting, when `scopes` is empty.
    pub no_scope_reason: Option<&'static str>,
    /// Reads the stored (raw, unparsed) value(s) from the config document.
    pub read: Read,
}

impl EnumSetting {
    pub const fn of<T: ConfigEnum>(
        key: &'static str,
        env: Option<&'static str>,
        shipped: &'static str,
        scopes: &'static [Scope],
        read: fn(&DarkmuxConfig) -> Option<&str>,
    ) -> Self {
        EnumSetting {
            key,
            env,
            rust_name: T::RUST_NAME,
            kind: T::KIND,
            values: T::TABLE,
            retired: T::RETIRED,
            shipped: Some(shipped),
            scopes,
            no_scope_reason: None,
            read: Read::One(read),
        }
    }

    /// A per-item setting: one value per list item, no env tier.
    pub const fn each<T: ConfigEnum>(
        key: &'static str,
        scopes: &'static [Scope],
        read: fn(&DarkmuxConfig) -> Vec<(String, String)>,
    ) -> Self {
        EnumSetting {
            key,
            env: None,
            rust_name: T::RUST_NAME,
            kind: T::KIND,
            values: T::TABLE,
            retired: T::RETIRED,
            shipped: None,
            scopes,
            no_scope_reason: None,
            read: Read::Each(read),
        }
    }

    /// The same entry, stating why no entry point consumes it.
    pub const fn with_no_scope_reason(mut self, reason: &'static str) -> Self {
        self.no_scope_reason = Some(reason);
        self
    }

    /// Whether this is a per-item (list) setting.
    pub fn is_per_item(&self) -> bool {
        matches!(self.read, Read::Each(_))
    }

    /// The canonical token for `raw` (trimmed, case-insensitive), or `None`.
    /// What `darkmux config set` stores. A retired spelling is `None`: it
    /// is not a value.
    pub fn canonical(&self, raw: &str) -> Option<&'static str> {
        let lower = raw.trim().to_ascii_lowercase();
        self.values.iter().map(|(t, _)| *t).find(|t| *t == lower)
    }

    /// The token a retired spelling was renamed to, if `raw` is one.
    pub fn renamed(&self, raw: &str) -> Option<&'static str> {
        let lower = raw.trim().to_ascii_lowercase();
        self.retired.iter().find(|(old, _)| *old == lower).map(|(_, new)| *new)
    }

    /// `a, b, c`.
    pub fn tokens_joined(&self) -> String {
        self.values.iter().map(|(t, _)| *t).collect::<Vec<_>>().join(", ")
    }

    /// One line per value: `  <token>  <meaning>`, the shipped one marked,
    /// then one line per retired spelling.
    pub fn values_help(&self, indent: &str) -> String {
        let width = self.values.iter().map(|(t, _)| t.len()).max().unwrap_or(0);
        let mut lines: Vec<String> = self
            .values
            .iter()
            .map(|(t, m)| {
                let mark = if Some(*t) == self.shipped { " (default)" } else { "" };
                format!("{indent}{t:<width$}  {m}{mark}")
            })
            .collect();
        for (old, new) in self.retired {
            lines.push(format!("{indent}(`{old}` was renamed to `{new}` in 4.0 and is refused)"));
        }
        lines.join("\n")
    }

    /// The `docs/ENVIRONMENT.md` phrase its row must carry, exactly:
    /// ``Valid values: `a`, `b`, `c`.`` (the drift test).
    pub fn docs_phrase(&self) -> String {
        let list = self.values.iter().map(|(t, _)| format!("`{t}`")).collect::<Vec<_>>().join(", ");
        format!("Valid values: {list}.")
    }

    fn bad(&self, raw: &str, set_in: SetIn) -> BadEnumValue {
        BadEnumValue {
            key: self.key,
            kind: self.kind,
            raw: raw.to_string(),
            renamed_to: self.renamed(raw),
            set_in,
            values: self.values,
        }
    }
}

/// Where a bad value was set. Always one of the two operator-written
/// tiers: the built-in tier is a registered token by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetIn {
    Env(&'static str),
    /// The concrete `config.json` path (for a per-item setting, with its
    /// index: `hooks.rules[2].match.level`).
    Config(String),
    /// (#2902 step 5) A path in the profile registry (`profiles.json`),
    /// e.g. `endpoints.azure.limits.policy`: a per-endpoint enum.
    Profiles(String),
}

impl std::fmt::Display for SetIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetIn::Env(var) => write!(f, "env var {var}"),
            SetIn::Config(key) => write!(f, "config.json key `{key}`"),
            SetIn::Profiles(path) => write!(f, "profiles.json key `{path}`"),
        }
    }
}

/// An unregistered value in an enum-typed setting. Its `Display` is the
/// whole operator message: the raw value, where it was set, the valid
/// values with their meanings, and the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadEnumValue {
    pub key: &'static str,
    pub kind: &'static str,
    pub raw: String,
    /// When `raw` is a retired spelling: the token it was renamed to.
    pub renamed_to: Option<&'static str>,
    pub set_in: SetIn,
    pub values: &'static [(&'static str, &'static str)],
}

impl BadEnumValue {
    /// The one-line summary (no value list), for a doctor row's message.
    pub fn summary(&self) -> String {
        let base = format!("`{}` (from {}) is not a valid {} for `{}`", self.raw, self.set_in, self.kind, self.key);
        match self.renamed_to {
            Some(new) => format!("{base}: `{}` was renamed to `{new}` in 4.0", self.raw.trim().to_ascii_lowercase()),
            None => base,
        }
    }

    /// The fix, for a doctor hint or the tail of a refusal.
    pub fn fix(&self) -> String {
        let tokens = match self.renamed_to {
            Some(new) => new.to_string(),
            None => format!("<{}>", self.values.iter().map(|(t, _)| *t).collect::<Vec<_>>().join("|")),
        };
        match &self.set_in {
            SetIn::Env(var) => format!("export {var}={tokens}, or unset {var} to use config.json"),
            // A per-item path (`hooks.rules[2]...`) has no `config set` form:
            // hook rules are set as a whole.
            SetIn::Config(path) if path.contains('[') => {
                format!("edit `{path}` in ~/.darkmux/config.json to {tokens}")
            }
            SetIn::Config(key) => format!("darkmux config set {key} {tokens}"),
            SetIn::Profiles(path) => format!("edit `{path}` in profiles.json to {tokens}"),
        }
    }

    /// `valid: a (meaning), b (meaning)`.
    pub fn valid_line(&self) -> String {
        let list = self.values.iter().map(|(t, m)| format!("{t} ({m})")).collect::<Vec<_>>().join("; ");
        format!("valid: {list}")
    }
}

impl std::fmt::Display for BadEnumValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}. {}. Fix: {}", self.summary(), self.valid_line(), self.fix())
    }
}

impl std::error::Error for BadEnumValue {}

/// Resolve one SCALAR setting against an explicit config and env lookup:
/// `env > config.json > shipped`. Pure, so the precedence and the refusal
/// are unit-tested without the process-wide config tier. An empty or
/// whitespace-only value at either tier is unset (falls through), the same
/// rule every other accessor in `config_access` follows.
///
/// Scalar-only by construction: a per-item setting has no single value to
/// resolve, and asking for one is a programming error (it panics naming the
/// key). Validate a per-item setting with [`bad_in`].
pub fn resolve_in(
    setting: &EnumSetting,
    cfg: &DarkmuxConfig,
    env: impl Fn(&str) -> Option<String>,
) -> Result<(&'static str, Source), BadEnumValue> {
    let Read::One(read) = setting.read else {
        panic!("`{}` is a per-item enum setting; resolve_in is for scalar settings (#2947)", setting.key)
    };
    if let Some(var) = setting.env {
        if let Some(raw) = env(var).filter(|s| !s.trim().is_empty()) {
            return setting.canonical(&raw).map(|t| (t, Source::Env)).ok_or_else(|| setting.bad(&raw, SetIn::Env(var)));
        }
    }
    if let Some(raw) = read(cfg).filter(|s| !s.trim().is_empty()) {
        return setting
            .canonical(raw)
            .map(|t| (t, Source::Config))
            .ok_or_else(|| setting.bad(raw, SetIn::Config(setting.key.to_string())));
    }
    Ok((setting.shipped.expect("a scalar setting has a shipped value"), Source::BuiltIn))
}

/// Every bad value of one setting, scalar or per-item. The general
/// validator: [`preflight`], [`bad_values`] and `darkmux doctor` use it.
pub fn bad_in(setting: &EnumSetting, cfg: &DarkmuxConfig, env: impl Fn(&str) -> Option<String>) -> Vec<BadEnumValue> {
    match setting.read {
        Read::One(_) => resolve_in(setting, cfg, env).err().into_iter().collect(),
        Read::Each(read) => read(cfg)
            .into_iter()
            .filter(|(_, raw)| !raw.trim().is_empty() && setting.canonical(raw).is_none())
            .map(|(path, raw)| setting.bad(&raw, SetIn::Config(path)))
            .collect(),
    }
}

/// (#2947 review C2) The bad `match.level` / `match.category` values in a
/// set of hook rules: what `HookSink::new` refuses (the whole hooks sink,
/// the established rule for a bad hook rule). Reads every per-item registry
/// entry under `hooks.rules[]`, so a new hook enum field registered there is
/// checked here with no change.
pub fn bad_hook_rule_values(rules: &[crate::config::HookRule]) -> Vec<BadEnumValue> {
    let cfg = DarkmuxConfig {
        hooks: Some(crate::config::HooksConfig { rules: Some(rules.to_vec()), ..Default::default() }),
        ..Default::default()
    };
    ENUM_SETTINGS
        .iter()
        .filter(|s| s.key.starts_with("hooks.rules[]."))
        .flat_map(|s| bad_in(s, &cfg, |_| None))
        .collect()
}

/// (#2902 step 5) Every unregistered budget `policy` in a profile registry:
/// each `endpoints.<id>.limits.policy`, and each inline endpoint's on a
/// profile model (`profiles.<p>.models[<i>].endpoint.limits.policy`). The
/// same rule as a config enum: never resolved to a fallback, refused at
/// preflight by every entry point that dispatches (`darkmux_profiles::
/// preflight`), Fail in doctor. A per-endpoint enum lives in
/// `profiles.json`, not `config.json`, so it is not an [`EnumSetting`]: this
/// is its registry pass.
pub fn bad_endpoint_budget_policies(reg: &crate::ProfileRegistry) -> Vec<BadEnumValue> {
    use crate::endpoint::{BudgetPolicy, Lenient};
    fn raw_of(ep: &crate::ModelEndpoint) -> Option<String> {
        match ep.limits.as_ref()?.known().ok()?.policy.as_ref()? {
            Lenient::Known(_) => None,
            Lenient::Unrecognized(v) => Some(v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())),
        }
    }
    let bad = |raw: String, path: String| BadEnumValue {
        key: "endpoints.<id>.limits.policy",
        kind: BudgetPolicy::KIND,
        raw,
        renamed_to: None,
        set_in: SetIn::Profiles(path),
        values: BudgetPolicy::TABLE,
    };
    let mut out = Vec::new();
    for (id, ep) in &reg.endpoints {
        if let Some(raw) = raw_of(ep) {
            out.push(bad(raw, format!("endpoints.{id}.limits.policy")));
        }
    }
    for (pname, profile) in &reg.profiles {
        for (i, m) in profile.models.iter().enumerate() {
            let Some(ep) = m.endpoint.as_ref().filter(|e| e.source == crate::endpoint::EndpointSource::Inline) else {
                continue;
            };
            if let Some(raw) = raw_of(ep) {
                out.push(bad(raw, format!("profiles.{pname}.models[{i}].endpoint.limits.policy")));
            }
        }
    }
    out
}

/// (#2902 step 5) The valid shape of an endpoint's `limits`, for a refusal.
pub const LIMITS_SHAPE: &str = "`limits`: {\"window\": {\"period\": \"<n>m|<n>h|<n>d\", \"tokens\": <integer>, \
     \"calls\": <integer>}, \"policy\": \"off\"|\"warn\"|\"wait\", \"warn_at\": <a fraction between 0 and 1>, \
     \"tokens_per_dispatch\": <integer>, \"concurrent_calls\": <integer>}";

/// (#2902 step 5 review M2) Every endpoint `limits` that cannot be used as
/// written: unreadable (one mistyped field, `"tokens": "2M"`, makes the whole
/// value unreadable, and an unreadable budget must never silently count
/// nothing), or readable but invalid (a set window whose `period` does not
/// parse, `warn_at` outside (0, 1)). An unregistered `policy` in readable
/// limits is [`bad_endpoint_budget_policies`]'s, not this. Same coverage:
/// `endpoints.<id>` and inline endpoints on profile models.
pub fn invalid_endpoint_limits(reg: &crate::ProfileRegistry) -> Vec<InvalidSetting> {
    use crate::endpoint::Lenient;
    fn problem(ep: &crate::ModelEndpoint) -> Option<String> {
        match ep.limits.as_ref()? {
            Lenient::Unrecognized(raw) => Some(match serde_json::from_value::<crate::UsageLimits>(raw.clone()) {
                Err(e) => format!("`limits` could not be read ({e})"),
                Ok(_) => "`limits` could not be read".to_string(),
            }),
            Lenient::Known(l) => {
                if l.resolved_policy().is_err() {
                    return None; // bad_endpoint_budget_policies names it
                }
                l.validate().err()
            }
        }
    }
    let mut out = Vec::new();
    let mut push = |path: String, p: String| {
        out.push(InvalidSetting { set_in: SetIn::Profiles(path), problem: p, valid: LIMITS_SHAPE.to_string() })
    };
    for (id, ep) in &reg.endpoints {
        if let Some(p) = problem(ep) {
            push(format!("endpoints.{id}.limits"), p);
        }
    }
    for (pname, profile) in &reg.profiles {
        for (i, m) in profile.models.iter().enumerate() {
            let Some(ep) = m.endpoint.as_ref().filter(|e| e.source == crate::endpoint::EndpointSource::Inline) else {
                continue;
            };
            if let Some(p) = problem(ep) {
                push(format!("profiles.{pname}.models[{i}].endpoint.limits"), p);
            }
        }
    }
    out
}

/// Every bad value in the registry right now, resolved through the live
/// tiers. `darkmux doctor`'s generic check reports each as Fail.
pub fn bad_values() -> Vec<BadEnumValue> {
    ENUM_SETTINGS.iter().flat_map(crate::config_access::enum_bad_values).collect()
}

/// The refusal a [`preflight`] returns: every bad value the scope
/// consumes, all at once, so one run of the command reports the whole
/// problem rather than one value per attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightRefusal {
    pub scope: Scope,
    pub bad: Vec<BadEnumValue>,
    /// (#2902 step 5) Settings that are not a bad ENUM value but cannot be
    /// used as written (an endpoint's unreadable `limits`, a window period
    /// that does not parse). Empty for a config.json-only preflight.
    pub invalid: Vec<InvalidSetting>,
    /// User files the scope consumes that carry a key their schema does not
    /// know, or are not JSON (`crate::user_files`).
    pub files: Vec<crate::user_files::FileProblem>,
}

impl PreflightRefusal {
    /// A refusal with nothing in it yet, for a caller that adds its own
    /// passes.
    pub fn none(scope: Scope) -> Self {
        PreflightRefusal { scope, bad: Vec::new(), invalid: Vec::new(), files: Vec::new() }
    }

    /// Nothing refused.
    pub fn is_empty(&self) -> bool {
        self.bad.is_empty() && self.invalid.is_empty() && self.files.is_empty()
    }

    /// `Ok` when nothing is refused.
    pub fn into_result(self) -> Result<(), PreflightRefusal> {
        if self.is_empty() { Ok(()) } else { Err(self) }
    }
}

/// (#2902 step 5) A setting that cannot be used as written, where it is and
/// what the valid shape is. Its `Display` is the operator line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSetting {
    /// Where it was set (a `profiles.json` path).
    pub set_in: SetIn,
    /// What is wrong, e.g. the parser's own message.
    pub problem: String,
    /// The valid shape.
    pub valid: String,
}

impl std::fmt::Display for InvalidSetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}. valid: {}", self.set_in, self.problem, self.valid)
    }
}

impl std::fmt::Display for PreflightRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}: refusing to start: bad config (#2947)", self.scope.label())?;
        let mut lines: Vec<String> = self
            .bad
            .iter()
            .map(|b| format!("  {}\n    {}\n    fix: {}", b.summary(), b.valid_line(), b.fix()))
            .collect();
        lines.extend(self.invalid.iter().map(|v| format!("  {}: {}\n    valid: {}", v.set_in, v.problem, v.valid)));
        lines.extend(self.files.iter().map(|p| format!("  {p}")));
        write!(f, "{}", lines.join("\n"))
    }
}

impl std::error::Error for PreflightRefusal {}

/// The preflight pass for one entry point: every registered setting that
/// `scope` consumes is resolved, and any bad value refuses. Call it before
/// minting anything. Not skippable by `--skip-preflight`: that flag skips a
/// Docker probe, and bad config is not a probe result that can be stale.
///
/// Every scope consumes `config.json`, so every scope also refuses one that
/// carries a key its schema does not know (`crate::user_files`).
pub fn preflight(scope: Scope) -> Result<(), PreflightRefusal> {
    let bad: Vec<BadEnumValue> = ENUM_SETTINGS
        .iter()
        .filter(|s| s.scopes.contains(&scope))
        .flat_map(crate::config_access::enum_bad_values)
        .collect();
    let files = crate::user_files::config_json_problems();
    PreflightRefusal { scope, bad, invalid: Vec::new(), files }.into_result()
}

/// Look up a registered setting by its dotted key.
pub fn setting(key: &str) -> Option<&'static EnumSetting> {
    ENUM_SETTINGS.iter().find(|s| s.key == key)
}

/// The help block every help surface prints: each enum setting, its env
/// var, and its values with their meanings.
pub fn help_block() -> String {
    let mut out = String::from("Enum-valued settings (any other value is refused):\n");
    for s in ENUM_SETTINGS {
        let env = s.env.map(|e| format!(" (env {e})")).unwrap_or_default();
        let per_item = if s.is_per_item() { " (per item; absent = no constraint)" } else { "" };
        out.push_str(&format!("  {}{env}{per_item}\n{}\n", s.key, s.values_help("      ")));
    }
    out
}

fn read_detection_degeneracy_policy(c: &DarkmuxConfig) -> Option<&str> {
    c.runtime.as_ref()?.detection.as_ref()?.degeneracy.as_ref()?.policy.as_deref()
}
fn read_thermal_pause_at(c: &DarkmuxConfig) -> Option<&str> {
    c.runtime.as_ref()?.thermal.as_ref()?.pause_at.as_deref()
}
fn read_thermal_resume_at(c: &DarkmuxConfig) -> Option<&str> {
    c.runtime.as_ref()?.thermal.as_ref()?.resume_at.as_deref()
}
fn read_remote_step_budget_policy(c: &DarkmuxConfig) -> Option<&str> {
    c.remote.as_ref()?.step_budget_policy.as_deref()
}
fn read_fleet_mode(c: &DarkmuxConfig) -> Option<&str> {
    c.fleet.as_ref()?.mode.as_deref()
}
fn read_fleet_identity_provider(c: &DarkmuxConfig) -> Option<&str> {
    c.fleet.as_ref()?.identity.as_ref()?.provider.as_deref()
}
fn read_fleet_busy_policy(c: &DarkmuxConfig) -> Option<&str> {
    c.fleet.as_ref()?.busy_policy.as_deref()
}
/// `(hooks.rules[i].match.<field>, raw)` for every rule that sets `field`.
fn hook_match_values(c: &DarkmuxConfig, field: &str, get: fn(&crate::config::HookMatch) -> Option<&str>) -> Vec<(String, String)> {
    let rules = c.hooks.as_ref().and_then(|h| h.rules.as_ref());
    rules
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, r)| {
            let raw = r.r#match.as_ref().and_then(get)?;
            Some((format!("hooks.rules[{i}].match.{field}"), raw.to_string()))
        })
        .collect()
}
fn read_hook_match_levels(c: &DarkmuxConfig) -> Vec<(String, String)> {
    hook_match_values(c, "level", |m| m.level.as_deref())
}
fn read_hook_match_categories(c: &DarkmuxConfig) -> Vec<(String, String)> {
    hook_match_values(c, "category", |m| m.category.as_deref())
}

/// Why the hook-rule enums preflight no entry point: the hooks sink is
/// built by every process that writes flow records, and refuses there.
const HOOK_RULE_NO_SCOPE: &str = "it is checked where the hooks sink is built (every process that \
     writes flow records): a bad value refuses the WHOLE hooks sink, loudly, and the run continues \
     without it, the established rule for any bad hook rule (#2093)";

/// Every consumer that runs a model: the thermal governor and the
/// degeneracy detector both ride every dispatch, so every entry point that
/// dispatches consumes them.
const DISPATCHING: &[Scope] = &[Scope::Dispatch, Scope::MissionLaunch, Scope::LabRun];

/// THE registry of enum-typed settings. Order is the order help and doctor
/// print in.
pub static ENUM_SETTINGS: &[EnumSetting] = &[
    EnumSetting::of::<crate::config::DetectionPolicy>(
        "runtime.detection.degeneracy.policy",
        Some("DARKMUX_RUNTIME_DETECTION_DEGENERACY_POLICY"),
        "conclude",
        DISPATCHING,
        read_detection_degeneracy_policy,
    ),
    EnumSetting::of::<crate::config::ThermalState>(
        "runtime.thermal.pause_at",
        Some("DARKMUX_THERMAL_PAUSE_AT"),
        "serious",
        DISPATCHING,
        read_thermal_pause_at,
    ),
    EnumSetting::of::<crate::config::ThermalState>(
        "runtime.thermal.resume_at",
        Some("DARKMUX_THERMAL_RESUME_AT"),
        "fair",
        DISPATCHING,
        read_thermal_resume_at,
    ),
    // (#2902 step 5) The per-step cap's policy (`off` / `warn`). Shipped
    // `warn`: with no cap set (the shipped state) nothing is counted.
    EnumSetting::of::<crate::config::StepBudgetPolicy>(
        "remote.step_budget_policy",
        Some("DARKMUX_REMOTE_STEP_BUDGET_POLICY"),
        "warn",
        DISPATCHING,
        read_remote_step_budget_policy,
    ),
    EnumSetting::of::<crate::config::FleetMode>(
        "fleet.mode",
        Some("DARKMUX_FLEET_MODE"),
        "standalone",
        &[],
        read_fleet_mode,
    )
    .with_no_scope_reason(
        "no entry point that starts work reads it: its only consumers are `darkmux doctor` and \
         the viewer-link rendering in `mission status`/`doctor` (`viewer_link_base`), which \
         prints the bad value and uses a direct link",
    ),
    EnumSetting::of::<crate::config::IdentityProvider>(
        "fleet.identity.provider",
        None,
        "tailscale",
        &[Scope::FleetSubmission],
        read_fleet_identity_provider,
    ),
    // (#2916 stage 2) Read by the fleet listener, which runs the fleet
    // submission preflight when it builds its identity provider.
    EnumSetting::of::<crate::config::BusyPolicy>(
        "fleet.busy_policy",
        Some("DARKMUX_FLEET_BUSY_POLICY"),
        "refuse",
        &[Scope::FleetSubmission],
        read_fleet_busy_policy,
    ),
    // (#2947 review C2) A typo here used to match nothing, silently.
    EnumSetting::each::<crate::config::HookLevel>("hooks.rules[].match.level", &[], read_hook_match_levels)
        .with_no_scope_reason(HOOK_RULE_NO_SCOPE),
    EnumSetting::each::<crate::config::HookCategory>("hooks.rules[].match.category", &[], read_hook_match_categories)
        .with_no_scope_reason(HOOK_RULE_NO_SCOPE),
];

/// A config carrying `raw` at `setting`'s location: its dotted key for a
/// scalar setting, or one list item for a per-item one
/// (`hooks.rules[].match.level` becomes `{"hooks":{"rules":[{"match":
/// {"level": raw}}]}}`). Built through JSON, so it goes through the same
/// lenient read a hand-edited `config.json` does. For conformance tests in
/// any crate that iterate [`ENUM_SETTINGS`].
#[cfg(any(test, feature = "test-support"))]
pub fn config_with_value(setting: &EnumSetting, raw: &str) -> DarkmuxConfig {
    fn nest(path: &str, leaf: serde_json::Value) -> serde_json::Value {
        path.rsplit('.').fold(leaf, |acc, seg| serde_json::json!({ (seg): acc }))
    }
    let root = match setting.key.split_once("[].") {
        Some((list, item)) => nest(list, serde_json::json!([nest(item, serde_json::json!(raw))])),
        None => nest(setting.key, serde_json::json!(raw)),
    };
    serde_json::from_value(root).expect("a string at an enum key always deserializes (lenient read)")
}

/// Where [`config_with_value`] puts the value, as a bad value names it.
#[cfg(any(test, feature = "test-support"))]
pub fn config_path_of(setting: &EnumSetting) -> String {
    setting.key.replace("[].", "[0].")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DetectionPolicy, FleetMode, HookCategory, HookLevel, IdentityProvider, ThermalState};

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn every_shipped_value_is_a_registered_token() {
        for s in ENUM_SETTINGS {
            match s.shipped {
                Some(v) => assert!(
                    s.values.iter().any(|(t, _)| *t == v),
                    "{}: shipped value `{v}` is not one of its own values ({})",
                    s.key,
                    s.tokens_joined()
                ),
                None => assert!(s.is_per_item(), "{}: a scalar setting needs a shipped value", s.key),
            }
        }
    }

    #[test]
    fn every_value_has_a_meaning_and_a_canonical_token() {
        for s in ENUM_SETTINGS {
            for (t, m) in s.values {
                assert!(!m.trim().is_empty(), "{}: `{t}` has no meaning", s.key);
                assert_eq!(*t, t.trim().to_ascii_lowercase(), "{}: `{t}` is not canonical", s.key);
            }
            for (old, new) in s.retired {
                assert!(s.canonical(old).is_none(), "{}: retired `{old}` is still a value", s.key);
                assert!(s.canonical(new).is_some(), "{}: `{old}` renamed to non-value `{new}`", s.key);
            }
        }
    }

    #[test]
    fn every_entry_has_a_scope_or_says_why_not() {
        for s in ENUM_SETTINGS {
            assert!(
                !s.scopes.is_empty() || s.no_scope_reason.is_some(),
                "{}: no consuming scope and no stated reason",
                s.key
            );
            assert!(
                s.scopes.is_empty() || s.no_scope_reason.is_none(),
                "{}: has scopes AND a no-scope reason",
                s.key
            );
        }
    }

    #[test]
    fn keys_are_unique() {
        let mut keys: Vec<&str> = ENUM_SETTINGS.iter().map(|s| s.key).collect();
        keys.sort();
        let n = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), n, "duplicate registry key");
    }

    /// The config tier: a present value is valid in any case, a bad one is
    /// refused naming its concrete path, and absent / blank is not bad.
    #[test]
    fn bad_in_config_tier() {
        for s in ENUM_SETTINGS {
            let (first, _) = s.values[0];
            let good = config_with_value(s, &format!("  {}  ", first.to_ascii_uppercase()));
            assert!(bad_in(s, &good, no_env).is_empty(), "{}", s.key);
            if !s.is_per_item() {
                assert_eq!(resolve_in(s, &good, no_env), Ok((first, Source::Config)), "{}", s.key);
                assert_eq!(
                    resolve_in(s, &DarkmuxConfig::default(), no_env),
                    Ok((s.shipped.unwrap(), Source::BuiltIn))
                );
            }
            let bad = bad_in(s, &config_with_value(s, "definitely-not-a-value"), no_env);
            assert_eq!(bad.len(), 1, "{}: {bad:?}", s.key);
            assert_eq!(bad[0].set_in, SetIn::Config(config_path_of(s)));
            assert_eq!(bad[0].raw, "definitely-not-a-value");
            assert!(bad_in(s, &DarkmuxConfig::default(), no_env).is_empty());
            assert!(bad_in(s, &config_with_value(s, "   "), no_env).is_empty(), "{}: blank is unset", s.key);
        }
    }

    /// The env tier beats the config tier, and a bad env value is refused
    /// even when the config value is good: it is never skipped in favor of
    /// the next tier.
    #[test]
    fn resolve_in_env_tier_refuses_rather_than_falling_through() {
        for s in ENUM_SETTINGS.iter().filter(|s| s.env.is_some()) {
            let var = s.env.unwrap();
            let good_cfg = config_with_value(s, s.values[0].0);
            let env_bad = |k: &str| (k == var).then(|| "nope".to_string());
            let err = resolve_in(s, &good_cfg, env_bad).unwrap_err();
            assert_eq!(err.set_in, SetIn::Env(var), "{}", s.key);
            let last = s.values[s.values.len() - 1].0;
            let env_good = |k: &str| (k == var).then(|| last.to_string());
            assert_eq!(resolve_in(s, &good_cfg, env_good), Ok((last, Source::Env)));
        }
    }

    /// The message carries all three things the rule names: the raw value,
    /// where it was set, and every valid value with its meaning.
    #[test]
    fn a_bad_value_message_names_value_source_and_valid_values() {
        for s in ENUM_SETTINGS {
            let err = bad_in(s, &config_with_value(s, "zzz-typo"), no_env).remove(0);
            let msg = err.to_string();
            assert!(msg.contains("`zzz-typo`"), "{msg}");
            assert!(msg.contains(&format!("config.json key `{}`", config_path_of(s))), "{msg}");
            for (t, m) in s.values {
                assert!(msg.contains(t) && msg.contains(m), "{}: `{t}` / its meaning missing: {msg}", s.key);
            }
        }
    }

    /// (#2947 rename) Conformance for retired spellings, generic over the
    /// registry: every retired spelling of every entry is refused (it is not
    /// a value) at both tiers, and the refusal names the word it was renamed
    /// to, in its summary and in its fix.
    #[test]
    fn every_retired_spelling_is_refused_naming_its_replacement() {
        let mut exercised = 0;
        for s in ENUM_SETTINGS {
            for (old, new) in s.retired {
                let mut bads = bad_in(s, &config_with_value(s, &old.to_ascii_uppercase()), no_env);
                if let Some(var) = s.env {
                    let env = |k: &str| (k == var).then(|| old.to_string());
                    bads.extend(bad_in(s, &DarkmuxConfig::default(), env));
                }
                assert!(!bads.is_empty(), "{}: retired `{old}` accepted", s.key);
                for b in bads {
                    assert_eq!(b.renamed_to, Some(*new));
                    let msg = b.to_string();
                    assert!(msg.contains(&format!("`{old}` was renamed to `{new}` in 4.0")), "{msg}");
                    assert!(b.fix().contains(new), "{}", b.fix());
                }
                exercised += 1;
            }
        }
        assert!(exercised >= 2, "no retired spellings exercised");
    }

    /// The degeneracy policy's rename, stated as the operator decided it.
    #[test]
    fn the_detection_policy_vocabulary_names_the_action() {
        assert_eq!(DetectionPolicy::TOKENS, &["off", "record", "warn", "conclude"]);
        assert_eq!(DetectionPolicy::RETIRED, &[("enforce", "conclude"), ("observe", "record")]);
        assert_eq!(DetectionPolicy::default(), DetectionPolicy::Conclude);
        let s = setting("runtime.detection.degeneracy.policy").unwrap();
        assert_eq!(s.shipped, Some("conclude"));
        assert!(DetectionPolicy::Conclude.measures() && DetectionPolicy::Conclude.acts() && !DetectionPolicy::Conclude.warns());
        assert!(DetectionPolicy::Warn.measures() && !DetectionPolicy::Warn.acts() && DetectionPolicy::Warn.warns());
        assert!(DetectionPolicy::Record.measures() && !DetectionPolicy::Record.acts() && !DetectionPolicy::Record.warns());
        assert!(!DetectionPolicy::Off.measures() && !DetectionPolicy::Off.acts() && !DetectionPolicy::Off.warns());
    }

    #[test]
    fn the_macro_round_trips_every_token() {
        fn check<T: ConfigEnum>() {
            assert_eq!(T::TABLE.len(), T::TOKENS.len());
            for t in T::TOKENS {
                let v = T::from_token(t).unwrap();
                assert_eq!(v.token(), *t);
                assert!(!v.meaning().is_empty());
                assert_eq!(T::parse(&format!(" {} ", t.to_ascii_uppercase())), Some(v));
            }
            assert_eq!(T::parse("not-a-token"), None);
        }
        check::<DetectionPolicy>();
        check::<ThermalState>();
        check::<FleetMode>();
        check::<IdentityProvider>();
        check::<HookLevel>();
        check::<HookCategory>();
        check::<crate::endpoint::ManagedBackend>();
        check::<crate::endpoint::Dialect>();
        check::<crate::endpoint::BudgetPolicy>();
        check::<crate::config::StepBudgetPolicy>();
    }

    /// The production half of a source file: everything before its test
    /// module, so an enum or a match arm written in a TEST (a review probe,
    /// a fixture) never counts.
    fn production(src: &str) -> &str {
        src.find("#[cfg(test)]\nmod tests").map_or(src, |i| &src[..i])
    }

    fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if p.is_dir() {
                if !matches!(name, "target" | ".git" | "node_modules" | "ui" | ".darkmux" | "tests") {
                    rs_files(&p, out);
                }
            } else if p.extension().is_some_and(|x| x == "rs") && !name.ends_with("_tests.rs") {
                out.push(p);
            }
        }
    }

    /// The name declared by an `enum` line of any visibility (`enum X`,
    /// `pub enum X`, `pub(crate) enum X`), or `None`.
    fn enum_decl(line: &str) -> Option<String> {
        let t = line.trim_start();
        let rest = ["pub enum ", "pub(crate) enum ", "pub(super) enum ", "enum "]
            .iter()
            .find_map(|p| t.strip_prefix(p))?;
        let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        (!name.is_empty()).then_some(name)
    }

    /// A `match` arm whose patterns are all string literals (`"hub" =>`,
    /// `"a" | "b" =>`): a hand-rolled token parse.
    fn is_literal_arm(t: &str) -> bool {
        t.starts_with('"') && t.contains("=>") && {
            let lhs = t.split("=>").next().unwrap_or("");
            lhs.split('|').all(|p| {
                let p = p.trim();
                p.len() >= 2 && p.starts_with('"') && p.ends_with('"')
            })
        }
    }

    /// (#2947 review C2) The reviewer's three probes, committed: each shape
    /// that stayed green before the scan was widened is recognized.
    #[test]
    fn the_scan_recognizes_the_review_probe_shapes() {
        assert_eq!(enum_decl("pub enum ZzProbeBudgetPolicy { Off, Warn }").as_deref(), Some("ZzProbeBudgetPolicy"));
        assert_eq!(enum_decl("pub(crate) enum ZzProbeMode { A, B }").as_deref(), Some("ZzProbeMode"));
        assert_eq!(enum_decl("    enum Private {").as_deref(), Some("Private"));
        assert_eq!(enum_decl("/// an enum in prose"), None);
        assert!(is_literal_arm(r#""fast" => 1,"#));
        assert!(is_literal_arm(r#""a" | "b" => Some(true),"#));
        assert!(!is_literal_arm(r#""msg" + x => y"#.split(" + ").next().unwrap()), "no arrow, no arm");
        assert!(!is_literal_arm(r#"Some(x) => 1,"#));
    }

    /// (#2947, widened in review C2) An enum-valued setting must be in the
    /// registry. Three structural scans, each failing on a new unregistered
    /// shape:
    ///
    /// 1. **Every `enum` in darkmux-types' production source, of ANY
    ///    visibility** (the crate that owns the config schema and its
    ///    accessors) is registered, is a per-endpoint `profiles.json` enum,
    ///    or is on `NOT_SETTINGS` with the reason it is not a setting value.
    ///    Review probes `pub enum ZzProbeBudgetPolicy` in `config_access.rs`
    ///    and `pub(crate) enum ZzProbeMode` in `config.rs` both fail here.
    /// 2. **Every `config_enum!` invocation in the WHOLE workspace** is
    ///    used by a registry entry (or is a profile enum): a ConfigEnum
    ///    declared in another crate cannot skip the registry.
    /// 3. **No hand-rolled token parse at an accessor**: a string-literal
    ///    match arm (`"hub" =>`) in `config_access.rs` / `config.rs`
    ///    production code must be on `LITERAL_ARMS_ALLOWED` with a reason.
    ///    That is the shape the three drifted settings had before #2947.
    ///
    /// **Limits, stated plainly.** A setting whose value is compared with
    /// `==` against a literal OUTSIDE those two files (`runtime.log_level`'s
    /// reader checks `== "debug"`), or an enum declared outside
    /// darkmux-types that is matched on a config string without
    /// `config_enum!`, is not seen: text scans find declarations and match
    /// arms, not meaning. Those remain a review question.
    #[test]
    fn every_enum_in_the_config_schema_is_registered() {
        /// Enums in darkmux-types that are not the value of one setting.
        const NOT_SETTINGS: &[(&str, &str)] = &[
            ("HeaderValue", "a hook header's value shape (literal string or Keychain item), not a token set"),
            ("EndpointKind", "derived from `managed` + `url`, never written"),
            ("Lenient", "the lenient-read wrapper itself"),
            ("EndpointSource", "runtime-only provenance, never serialized"),
            ("CredentialSource", "runtime-only resolution result, never serialized"),
            (
                "EndpointAuthType",
                "profiles.json `auth.type`, strictly deserialized: an unknown value fails the \
                 registry load loudly, never a fallback",
            ),
            (
                "Capability",
                "profiles.json capability names, strictly deserialized (an unknown one fails the load)",
            ),
            (
                "CompactionStrategy",
                "profiles.json `compaction.strategy`, strictly deserialized (an unknown one fails the load)",
            ),
            ("UtilityBinding", "the `internal.utility` value's shape (id or object), not a token set"),
            ("QuarantinedEntryKind", "a registry-load diagnostic, never written"),
            ("IssueSeverity", "a registry-load diagnostic, never written"),
            ("GitdirPointerKind", "a workspace probe result, never written"),
            ("Scope", "`paths::Scope` / `config_enum::Scope`: code-side enums, not setting values"),
            ("ResolveScope", "a path-resolution mode chosen by code, not a setting"),
            ("Source", "provenance of a resolved value, not a setting"),
            ("Read", "how the registry reads a value, not a setting"),
            ("SetIn", "where a bad value was set, not a setting"),
            ("UserFileKind", "which kind of user file the unknown-key gate checked, not a setting"),
            ("Issue", "what the user-file key gate found at one key, not a setting"),
            ("Problem", "what the unknown-key gate found wrong with a file, not a setting"),
            ("Others", "how a schema treats keys it does not name, internal to the gate"),
        ];
        const PROFILE_ENUMS: &[&str] = &["ManagedBackend", "Dialect", "BudgetPolicy"];
        const LITERAL_ARMS_ALLOWED: &[(&str, &str)] = &[(
            "parse_bool_token",
            "the one boolean-token vocabulary (1/true/yes/on, 0/false/no/off), not an enum setting",
        )];

        let crate_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rs_files(&crate_src, &mut files);
        assert!(files.len() >= 10, "the darkmux-types scan found {} files", files.len());
        let registered = |n: &str| ENUM_SETTINGS.iter().any(|s| s.rust_name == n) || PROFILE_ENUMS.contains(&n);

        // 1. Enums of any visibility in darkmux-types.
        let mut declared = Vec::new();
        for f in &files {
            let src = std::fs::read_to_string(f).unwrap();
            for line in production(&src).lines() {
                if let Some(name) = enum_decl(line) {
                    declared.push((f.file_name().unwrap().to_string_lossy().to_string(), name));
                }
            }
        }
        assert!(declared.len() >= 15, "the scan found too few enums ({declared:?})");
        for (file, name) in &declared {
            assert!(
                registered(name) || NOT_SETTINGS.iter().any(|(n, _)| n == name),
                "`{name}` ({file}) is an enum in the config crate but is not in \
                 config_enum::ENUM_SETTINGS. Implement ConfigEnum with config_enum! and register it, \
                 or add it to NOT_SETTINGS with the reason it is not a setting value (#2947)"
            );
        }

        // 2. Every `config_enum!` in the workspace is registered.
        let root = crate_src.join("../../..");
        let mut all = Vec::new();
        for d in ["src", "crates", "runtime/src"] {
            rs_files(&root.join(d), &mut all);
        }
        assert!(all.len() >= 100, "the workspace scan found only {} files", all.len());
        let mut invocations = 0;
        for f in &all {
            let src = std::fs::read_to_string(f).unwrap();
            for line in production(&src).lines() {
                let t = line.trim_start();
                let Some(rest) = t.strip_prefix("crate::config_enum!(").or_else(|| t.strip_prefix("darkmux_types::config_enum!(")) else {
                    continue;
                };
                let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                invocations += 1;
                assert!(registered(&name), "`{name}` ({}) implements ConfigEnum but no registry entry uses it (#2947)", f.display());
            }
        }
        assert!(invocations >= 8, "found only {invocations} config_enum! invocations");

        // 3. No hand-rolled token parse in the accessor / schema files.
        let mut literal_arms_seen = 0;
        for file in ["config_access.rs", "config.rs"] {
            let src = std::fs::read_to_string(crate_src.join(file)).unwrap();
            let mut current_fn = String::new();
            let mut in_macro = false;
            for line in production(&src).lines() {
                let t = line.trim_start();
                if let Some(rest) = t.strip_prefix("pub fn ").or_else(|| t.strip_prefix("fn ")) {
                    current_fn = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                }
                if t.starts_with("crate::config_enum!(") {
                    in_macro = true;
                }
                if in_macro {
                    if t.starts_with("]);") {
                        in_macro = false;
                    }
                    continue;
                }
                let literal_arm = is_literal_arm(t);
                if literal_arm {
                    literal_arms_seen += 1;
                    assert!(
                        LITERAL_ARMS_ALLOWED.iter().any(|(f, _)| *f == current_fn),
                        "{file}: `{current_fn}` matches a config string against literals (`{t}`): \
                         register an enum setting instead of parsing tokens by hand (#2947)"
                    );
                }
            }
        }
        // Anti-vacuity: the scan must see the allowed boolean vocabulary's
        // own arms, or it is not recognizing match arms at all.
        assert!(literal_arms_seen >= 2, "the literal-arm scan saw {literal_arms_seen} arms");
    }

    /// `docs/ENVIRONMENT.md`'s row for each setting carries the value list
    /// generated from the registry, verbatim, so the doc cannot list a value
    /// the parser refuses or omit one it accepts.
    #[test]
    fn environment_doc_value_lists_match_the_registry() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/ENVIRONMENT.md");
        let doc = std::fs::read_to_string(path).unwrap();
        for s in ENUM_SETTINGS {
            // The row whose FIRST cell names the env var (or, for a
            // setting with no env var, any row naming the config key).
            let needle = match s.env {
                Some(var) => format!("`{var}`"),
                None => format!("`{}`", s.key),
            };
            let first_cell = |l: &str| l.split('|').nth(1).unwrap_or("").to_string();
            let rows: Vec<&str> = doc
                .lines()
                .filter(|l| l.starts_with('|'))
                .filter(|l| if s.env.is_some() { first_cell(l).contains(&needle) } else { l.contains(&needle) })
                .collect();
            assert!(!rows.is_empty(), "{}: no ENVIRONMENT.md row contains {needle}", s.key);
            let phrase = s.docs_phrase();
            assert!(
                rows.iter().any(|r| r.contains(&phrase)),
                "{}: no ENVIRONMENT.md row for {needle} carries `{phrase}`",
                s.key
            );
        }
    }
}
