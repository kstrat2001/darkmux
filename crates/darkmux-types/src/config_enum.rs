//! (#2947) Enum-typed settings: one declaration, one registry, one rule.
//!
//! **The rule.** An unregistered value in an enum-typed setting is bad
//! config. It is never resolved to a fallback, in either direction. Every
//! entry point that would consume it refuses at preflight, naming the raw
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
//!    and the entry-point [`Scope`]s that consume it.
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
//! `config_access::resolve_enum`. Nothing else. A budget `policy` of
//! `off`/`observe`/`enforce` (#2902 step 5) would be exactly those three
//! lines plus a `Scope` list. The conformance tests below iterate the
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
        impl $crate::config_enum::ConfigEnum for $ty {
            const RUST_NAME: &'static str = stringify!($ty);
            const KIND: &'static str = $kind;
            const TABLE: &'static [(&'static str, &'static str)] = &[ $( ($token, $meaning) ),+ ];
            const TOKENS: &'static [&'static str] = &[ $( $token ),+ ];
            fn token(self) -> &'static str {
                match self { $( $ty::$variant => $token ),+ }
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

/// One enum-typed setting. Built only through [`EnumSetting::of`], which is
/// what keeps `values` bound to a real [`ConfigEnum`].
#[derive(Clone, Copy)]
pub struct EnumSetting {
    /// The dotted `config.json` key.
    pub key: &'static str,
    /// The env var that overrides it, if it has one.
    pub env: Option<&'static str>,
    /// [`ConfigEnum::RUST_NAME`] of the enum.
    pub rust_name: &'static str,
    /// [`ConfigEnum::KIND`].
    pub kind: &'static str,
    /// [`ConfigEnum::TABLE`].
    pub values: &'static [(&'static str, &'static str)],
    /// The value an absent setting resolves to. A token of `values`
    /// (`every_shipped_value_is_a_registered_token` pins it).
    pub shipped: &'static str,
    /// The entry points that consume this setting and refuse on a bad
    /// value. Empty only with a `no_scope_reason`.
    pub scopes: &'static [Scope],
    /// Why no entry point preflights this setting, when `scopes` is empty.
    pub no_scope_reason: Option<&'static str>,
    /// Reads the stored (raw, unparsed) string from the config document.
    pub read: fn(&DarkmuxConfig) -> Option<&str>,
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
            shipped,
            scopes,
            no_scope_reason: None,
            read,
        }
    }

    /// The same entry, stating why no entry point consumes it.
    pub const fn with_no_scope_reason(mut self, reason: &'static str) -> Self {
        self.no_scope_reason = Some(reason);
        self
    }

    /// The canonical token for `raw` (trimmed, case-insensitive), or `None`.
    /// What `darkmux config set` stores.
    pub fn canonical(&self, raw: &str) -> Option<&'static str> {
        let lower = raw.trim().to_ascii_lowercase();
        self.values.iter().map(|(t, _)| *t).find(|t| *t == lower)
    }

    /// `a, b, c`.
    pub fn tokens_joined(&self) -> String {
        self.values.iter().map(|(t, _)| *t).collect::<Vec<_>>().join(", ")
    }

    /// One line per value: `  <token>  <meaning>`, the shipped one marked.
    pub fn values_help(&self, indent: &str) -> String {
        let width = self.values.iter().map(|(t, _)| t.len()).max().unwrap_or(0);
        self.values
            .iter()
            .map(|(t, m)| {
                let mark = if *t == self.shipped { " (default)" } else { "" };
                format!("{indent}{t:<width$}  {m}{mark}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The `docs/ENVIRONMENT.md` phrase its row must carry, exactly:
    /// ``Valid values: `a`, `b`, `c`.`` (the drift test).
    pub fn docs_phrase(&self) -> String {
        let list = self.values.iter().map(|(t, _)| format!("`{t}`")).collect::<Vec<_>>().join(", ");
        format!("Valid values: {list}.")
    }
}

/// Where a bad value was set. Always one of the two operator-written
/// tiers: the built-in tier is a registered token by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetIn {
    Env(&'static str),
    Config(&'static str),
}

impl std::fmt::Display for SetIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetIn::Env(var) => write!(f, "env var {var}"),
            SetIn::Config(key) => write!(f, "config.json key `{key}`"),
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
    pub set_in: SetIn,
    pub values: &'static [(&'static str, &'static str)],
}

impl BadEnumValue {
    /// The one-line summary (no value list), for a doctor row's message.
    pub fn summary(&self) -> String {
        format!(
            "`{}` (from {}) is not a valid {} for `{}`",
            self.raw, self.set_in, self.kind, self.key
        )
    }

    /// The fix, for a doctor hint or the tail of a refusal.
    pub fn fix(&self) -> String {
        let tokens = self.values.iter().map(|(t, _)| *t).collect::<Vec<_>>().join("|");
        match &self.set_in {
            SetIn::Env(var) => format!("export {var}=<{tokens}>, or unset {var} to use config.json"),
            SetIn::Config(key) => format!("darkmux config set {key} <{tokens}>"),
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

/// Resolve one setting against an explicit config and env lookup:
/// `env > config.json > shipped`. Pure, so the precedence and the refusal
/// are unit-tested without the process-wide config tier. An empty or
/// whitespace-only value at either tier is unset (falls through), the same
/// rule every other accessor in `config_access` follows.
pub fn resolve_in(
    setting: &EnumSetting,
    cfg: &DarkmuxConfig,
    env: impl Fn(&str) -> Option<String>,
) -> Result<(&'static str, Source), BadEnumValue> {
    let bad = |raw: &str, set_in: SetIn| BadEnumValue {
        key: setting.key,
        kind: setting.kind,
        raw: raw.to_string(),
        set_in,
        values: setting.values,
    };
    if let Some(var) = setting.env {
        if let Some(raw) = env(var).filter(|s| !s.trim().is_empty()) {
            return setting.canonical(&raw).map(|t| (t, Source::Env)).ok_or_else(|| bad(&raw, SetIn::Env(var)));
        }
    }
    if let Some(raw) = (setting.read)(cfg).filter(|s| !s.trim().is_empty()) {
        return setting.canonical(raw).map(|t| (t, Source::Config)).ok_or_else(|| bad(raw, SetIn::Config(setting.key)));
    }
    Ok((setting.shipped, Source::BuiltIn))
}

/// Every bad value in the registry right now, resolved through the live
/// tiers. `darkmux doctor`'s generic check reports each as Fail.
pub fn bad_values() -> Vec<BadEnumValue> {
    ENUM_SETTINGS.iter().filter_map(|s| crate::config_access::resolve_enum_token(s).err()).collect()
}

/// The refusal a [`preflight`] returns: every bad value the scope
/// consumes, all at once, so one run of the command reports the whole
/// problem rather than one value per attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightRefusal {
    pub scope: Scope,
    pub bad: Vec<BadEnumValue>,
}

impl std::fmt::Display for PreflightRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}: refusing to start: bad config (#2947)", self.scope.label())?;
        for (i, b) in self.bad.iter().enumerate() {
            write!(f, "  {}\n    {}\n    fix: {}", b.summary(), b.valid_line(), b.fix())?;
            if i + 1 < self.bad.len() {
                writeln!(f)?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for PreflightRefusal {}

/// The preflight pass for one entry point: every registered setting that
/// `scope` consumes is resolved, and any bad value refuses. Call it before
/// minting anything. Not skippable by `--skip-preflight`: that flag skips a
/// Docker probe, and bad config is not a probe result that can be stale.
pub fn preflight(scope: Scope) -> Result<(), PreflightRefusal> {
    let bad: Vec<BadEnumValue> = ENUM_SETTINGS
        .iter()
        .filter(|s| s.scopes.contains(&scope))
        .filter_map(|s| crate::config_access::resolve_enum_token(s).err())
        .collect();
    if bad.is_empty() {
        Ok(())
    } else {
        Err(PreflightRefusal { scope, bad })
    }
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
        out.push_str(&format!("  {}{env}\n{}\n", s.key, s.values_help("      ")));
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
fn read_fleet_mode(c: &DarkmuxConfig) -> Option<&str> {
    c.fleet.as_ref()?.mode.as_deref()
}
fn read_fleet_identity_provider(c: &DarkmuxConfig) -> Option<&str> {
    c.fleet.as_ref()?.identity.as_ref()?.provider.as_deref()
}

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
        "enforce",
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
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DetectionPolicy, FleetMode, IdentityProvider, ThermalState};

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn cfg_with(key: &str, value: &str) -> DarkmuxConfig {
        let mut root = serde_json::json!({});
        let parts: Vec<&str> = key.split('.').collect();
        let mut cur = &mut root;
        for p in &parts[..parts.len() - 1] {
            cur = cur.as_object_mut().unwrap().entry(p.to_string()).or_insert(serde_json::json!({}));
        }
        cur[parts[parts.len() - 1]] = serde_json::Value::String(value.to_string());
        serde_json::from_value(root).expect("a string at an enum key always deserializes (lenient read)")
    }

    #[test]
    fn every_shipped_value_is_a_registered_token() {
        for s in ENUM_SETTINGS {
            assert!(
                s.values.iter().any(|(t, _)| *t == s.shipped),
                "{}: shipped value `{}` is not one of its own values ({})",
                s.key,
                s.shipped,
                s.tokens_joined()
            );
        }
    }

    #[test]
    fn every_value_has_a_meaning_and_a_canonical_token() {
        for s in ENUM_SETTINGS {
            for (t, m) in s.values {
                assert!(!m.trim().is_empty(), "{}: `{t}` has no meaning", s.key);
                assert_eq!(*t, t.trim().to_ascii_lowercase(), "{}: `{t}` is not canonical", s.key);
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

    /// The config tier: a present value resolves canonically, a bad one is
    /// refused naming the config key, an absent one is the shipped value.
    #[test]
    fn resolve_in_config_tier() {
        for s in ENUM_SETTINGS {
            let (first, _) = s.values[0];
            let cfg = cfg_with(s.key, &format!("  {}  ", first.to_ascii_uppercase()));
            assert_eq!(resolve_in(s, &cfg, no_env), Ok((first, Source::Config)), "{}", s.key);

            let cfg = cfg_with(s.key, "definitely-not-a-value");
            let err = resolve_in(s, &cfg, no_env).unwrap_err();
            assert_eq!(err.set_in, SetIn::Config(s.key));
            assert_eq!(err.raw, "definitely-not-a-value");

            assert_eq!(resolve_in(s, &DarkmuxConfig::default(), no_env), Ok((s.shipped, Source::BuiltIn)));
            // Blank is unset, not bad.
            let cfg = cfg_with(s.key, "   ");
            assert_eq!(resolve_in(s, &cfg, no_env), Ok((s.shipped, Source::BuiltIn)), "{}", s.key);
        }
    }

    /// The env tier beats the config tier, and a bad env value is refused
    /// even when the config value is good: it is never skipped in favor of
    /// the next tier.
    #[test]
    fn resolve_in_env_tier_refuses_rather_than_falling_through() {
        for s in ENUM_SETTINGS.iter().filter(|s| s.env.is_some()) {
            let var = s.env.unwrap();
            let good_cfg = cfg_with(s.key, s.values[0].0);
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
            let err = resolve_in(s, &cfg_with(s.key, "zzz-typo"), no_env).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("`zzz-typo`"), "{msg}");
            assert!(msg.contains(&format!("config.json key `{}`", s.key)), "{msg}");
            for (t, m) in s.values {
                assert!(msg.contains(t) && msg.contains(m), "{}: `{t}` / its meaning missing: {msg}", s.key);
            }
        }
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
        check::<crate::endpoint::ManagedBackend>();
        check::<crate::endpoint::Dialect>();
    }

    /// Every enum declared in the config schema must be registered here,
    /// or be listed in `NOT_SETTINGS` with the reason it is not a setting
    /// value. A new enum setting that skips the registry fails this test.
    ///
    /// **What it detects:** an `enum` declaration in `config.rs` or
    /// `endpoint.rs` (the `config.json` schema and the `profiles.json`
    /// endpoint schema). **What it cannot detect:** a setting that is
    /// enum-like in meaning but typed as a plain `String` with no Rust enum
    /// behind it (`runtime.log_level` is one: its only reader compares
    /// against `"debug"`). A string compared against literals has no
    /// declaration to find; the drift guard for that shape would have to
    /// be a review question, and this doc is where that is said.
    #[test]
    fn every_enum_in_the_config_schema_is_registered() {
        /// Enums in the schema files that are not the value of one
        /// setting, with the reason.
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
        ];
        /// Per-endpoint `profiles.json` enums. They are ConfigEnums (one
        /// value table), but not `config.json` settings: each endpoint
        /// carries its own, read through `Lenient<T>` and refused by name
        /// at use (`ModelEndpoint::kind` / `resolved_dialect`) and by
        /// `darkmux doctor`'s endpoints check.
        const PROFILE_ENUMS: &[&str] = &["ManagedBackend", "Dialect"];

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut declared = Vec::new();
        for file in ["config.rs", "endpoint.rs"] {
            let src = std::fs::read_to_string(root.join(file)).unwrap();
            for line in src.lines() {
                let Some(rest) = line.trim_start().strip_prefix("pub enum ") else { continue };
                let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                declared.push(name);
            }
        }
        assert!(declared.len() >= 8, "the scan found too few enums ({declared:?}); is it still reading the schema?");
        for name in &declared {
            let registered = ENUM_SETTINGS.iter().any(|s| s.rust_name == name);
            let excused = NOT_SETTINGS.iter().any(|(n, _)| n == name) || PROFILE_ENUMS.contains(&name.as_str());
            assert!(
                registered || excused,
                "`{name}` is declared in the config schema but is not in config_enum::ENUM_SETTINGS. \
                 Implement ConfigEnum with config_enum! and register it, or add it to NOT_SETTINGS \
                 with the reason it is not a setting value (#2947)"
            );
        }
        // And every ConfigEnum implementation in the schema is either
        // registered or a declared profile enum.
        for file in ["config.rs", "endpoint.rs"] {
            let src = std::fs::read_to_string(root.join(file)).unwrap();
            for line in src.lines() {
                let Some(rest) = line.trim_start().strip_prefix("config_enum!(") else { continue };
                let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                assert!(
                    ENUM_SETTINGS.iter().any(|s| s.rust_name == name) || PROFILE_ENUMS.contains(&name.as_str()),
                    "`{name}` implements ConfigEnum but no registry entry uses it (#2947)"
                );
            }
        }
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
