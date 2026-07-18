use darkmux_types::{Profile, ProfileRegistry, QuarantinedEntry, QuarantinedEntryKind};
use anyhow::{Context, Result, anyhow, bail};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct LoadedRegistry {
    pub registry: ProfileRegistry,
    pub path: PathBuf,
}

/// Default search locations when neither `--profiles` nor `DARKMUX_PROFILES` is
/// set. `DARKMUX_PROFILES`, if set, short-circuits this list — see `load_registry`.
pub fn default_locations() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    out.push(cwd.join(".darkmux.json"));
    out.push(cwd.join(".darkmux").join("profiles.json"));
    if let Some(home) = dirs::home_dir() {
        out.push(home.join(".darkmux").join("profiles.json"));
        out.push(home.join(".config").join("darkmux").join("profiles.json"));
    }
    out
}

pub fn load_registry(explicit: Option<&str>) -> Result<LoadedRegistry> {
    // Precedence: explicit --profiles flag > DARKMUX_PROFILES env var > default
    // search locations. The two override paths fail-fast on missing files
    // so that a typo'd path doesn't silently pick up an unrelated registry.
    if let Some(p) = explicit {
        return load_from(PathBuf::from(p), "--profiles flag");
    }
    if let Ok(p) = env::var("DARKMUX_PROFILES") {
        if !p.is_empty() {
            return load_from(PathBuf::from(p), "DARKMUX_PROFILES env var");
        }
    }
    let candidates = default_locations();
    for path in &candidates {
        if path.exists() {
            return load_from(path.clone(), "default search");
        }
    }
    let listed = candidates
        .iter()
        .map(|p| format!("  {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    bail!(
        "darkmux: no profile registry found.\n\
         Looked in:\n{}\n\
         Run `darkmux init` to bootstrap one, or create it manually \
         (see profiles.example.json in the darkmux repo).",
        listed
    );
}

fn load_from(path: PathBuf, source: &str) -> Result<LoadedRegistry> {
    if !path.exists() {
        bail!(
            "darkmux: registry not found at {} (specified via {}). \
             The file must exist when this override is used.",
            path.display(),
            source
        );
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("reading registry at {}", path.display()))?;
    let registry = parse_registry_lenient(&raw)
        .with_context(|| format!("parsing JSON at {}", path.display()))?;
    if !registry.quarantined.is_empty() {
        // One warning line per load (matching the loud-but-brief style of
        // this crate's other operator warnings); full per-entry detail lives
        // in `darkmux doctor`.
        let listed = registry
            .quarantined
            .iter()
            .map(|q| format!("{} \"{}\" ({})", q.kind, q.name, q.error))
            .collect::<Vec<_>>()
            .join("; ");
        eprintln!(
            "darkmux: warning: {} registry entr{} quarantined at {} — {} — \
             other entries are unaffected; run `darkmux doctor` for details. (#1282)",
            registry.quarantined.len(),
            if registry.quarantined.len() == 1 { "y" } else { "ies" },
            path.display(),
            listed
        );
    }
    validate_registry(&registry, &path)?;
    Ok(LoadedRegistry { registry, path })
}

/// (#1282) Two-stage registry parse — the per-entry quarantine that keeps one
/// structurally-broken profile entry from blasting the whole file
/// (config-leniency contract #1269, applied one layer down):
///
/// 1. **Whole-file JSON syntax** — a syntax error can't be scoped to one
///    entry, so it still fails the file (with serde_json's line/column).
/// 2. **Per-entry typed conversion** — each `profiles{}` entry is converted to
///    its typed form individually; a failure quarantines THAT entry (name +
///    serde's field-level message, e.g. ``missing field `id```) and is removed
///    from the raw tree.
/// 3. **Shell parse** — the remaining tree (registry shell + healthy entries)
///    goes through the normal typed parse. A failure here is shell-level
///    breakage (e.g. `profiles` isn't an object) and fails the file.
///
/// (#1426 ship-2) The `crews` map retired from the schema, so it is no longer
/// quarantined per-entry — a `crews` key overflows into `ProfileRegistry.extras`
/// (lenient-on-read) and is preserved verbatim as harmless residue.
fn parse_registry_lenient(raw: &str) -> Result<ProfileRegistry> {
    let mut root: serde_json::Value = serde_json::from_str(raw)?;

    let mut quarantined: Vec<QuarantinedEntry> = Vec::new();
    let kind = QuarantinedEntryKind::Profile;
    if let Some(map) = root.get_mut("profiles").and_then(|v| v.as_object_mut()) {
        for (name, entry) in map.iter() {
            if let Some(e) = serde_json::from_value::<Profile>(entry.clone()).err() {
                quarantined.push(QuarantinedEntry {
                    kind,
                    name: name.clone(),
                    error: e.to_string(),
                });
            }
        }
        for q in &quarantined {
            map.remove(&q.name);
        }
    }

    let mut registry: ProfileRegistry = serde_json::from_value(root)?;
    registry.quarantined = quarantined;
    Ok(registry)
}

fn validate_registry(reg: &ProfileRegistry, path: &Path) -> Result<()> {
    // (#1282) A registry whose only profiles are quarantined still LOADS
    // (empty healthy set) — failing here would restore the whole-file blast
    // the quarantine exists to prevent, and would hide the per-entry errors
    // from `darkmux doctor`.
    if reg.profiles.is_empty() && reg.quarantined.is_empty() {
        bail!("{}: registry has no profiles", path.display());
    }
    for (name, profile) in &reg.profiles {
        validate_profile(name, profile, path)?;
    }
    // (#1426 ship-2) The `crews` map retired from the schema — review staffing
    // is now DERIVED by the resourcing resolver (`darkmux_crew::resourcing`)
    // from the active profile's models plus launch-param seat pins, never a
    // declared crew. A legacy `crews` key is harmless residue in `extras`.
    Ok(())
}

fn validate_profile(name: &str, profile: &Profile, path: &Path) -> Result<()> {
    if profile.models.is_empty() {
        bail!(
            "{}: profile \"{}\" must have at least one model",
            path.display(),
            name
        );
    }
    // (#590) The old "exactly one primary" rule is gone with `ModelRole`. The
    // default model is `default_model` (or the first model when unset). The
    // only new failure mode worth catching: a `default_model` that names a
    // model not present in `models[]` — an operator typo, surfaced loudly.
    if let Some(default_id) = profile.default_model.as_deref() {
        if !profile.models.iter().any(|m| m.id == default_id) {
            bail!(
                "{}: profile \"{}\" sets default_model \"{}\", which is not one of its models[]",
                path.display(),
                name,
                default_id
            );
        }
    }
    Ok(())
}

pub fn get_profile<'a>(reg: &'a ProfileRegistry, name: &str) -> Result<&'a Profile> {
    reg.profiles.get(name).ok_or_else(|| {
        // (#1282) A quarantined name gets the entry's own parse error, not a
        // misleading "not found" — the profile IS in the file, it's broken.
        // Shared message shape: `ProfileRegistry::quarantine_error_for`, the
        // same text the dispatch resolvers raise.
        if let Some(msg) = reg.quarantine_error_for(name) {
            return anyhow!(msg);
        }
        let available: Vec<&String> = reg.profiles.keys().collect();
        let listed = available
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow!(
            "darkmux: profile \"{}\" not found. Available: {}",
            name,
            if listed.is_empty() { "(none)" } else { &listed }
        )
    })
}

/// (#1475 packet 1/3) How a role's profile was resolved — a per-run launch
/// override, the machine's `role_profiles` map, or a fall-through to
/// `default_profile` (the fresh-user single-model floor). Lets a caller (and
/// `darkmux doctor`) tell an override from a deliberate map binding from the
/// default fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleProfileSource {
    /// (#1475 packet 3) The role was bound by a per-run launch override
    /// (`mission launch review --param <role>=<profile>`) — highest precedence,
    /// above the `role_profiles` map and `default_profile`.
    Overridden,
    /// The role was bound to this profile by the `role_profiles` map.
    Mapped,
    /// The role was unmapped; it fell back to the registry's `default_profile`.
    DefaultFallback,
}

/// (#1475 packet 3) The origin of a role→profile binding, supplied to
/// [`resolve_role_profile_with`] so it can stamp the right [`RoleProfileSource`]
/// without collapsing a per-run launch override into a plain map binding.
/// Precedence (override > map > unmapped) is decided by the CALLER building this
/// value; the resolver only reads which tier won. Owns its profile name so a
/// caller can build it from `config_access`'s owned `Option<String>` return
/// without lifetime gymnastics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleBinding {
    /// A per-run launch override (`--param <role>=<profile>`). Highest precedence.
    Overridden(String),
    /// The machine-local `role_profiles` map binding in `config.json`.
    Mapped(String),
    /// Unmapped — resolve through `default_profile` (the fresh-user floor).
    Unmapped,
}

/// (#1475 packet 1) The resolved binding for a role: `role -> profile -> model`.
/// Carries the resolved profile name, a borrow of the profile, and which tier
/// resolved it. The FOUNDATION type packets 2-5 build the review crew rollup on;
/// this packet does not yet wire review to it.
#[derive(Debug)]
pub struct ResolvedRoleProfile<'a> {
    pub role_id: String,
    pub profile_name: String,
    pub profile: &'a Profile,
    pub source: RoleProfileSource,
}

/// (#1475 packet 1) Resolve a role id to its bound profile via the machine-local
/// `role_profiles` map in `config.json` (the production path): `role -> map ->
/// profile -> model`. Reads the mapped name from
/// [`darkmux_types::config_access::role_profile`], then delegates to the pure
/// [`resolve_role_profile_with`] for the registry lookup + validation.
///
/// An UNMAPPED role falls back to `default_profile` SILENTLY (the fresh-user
/// floor — a bare install gets a working single-model review). A role MAPPED to
/// a profile name absent from the registry resolves to a **loud, clear error**
/// (config-leniency contract 7: semantic validation at resolution, never a
/// silent fallback); `darkmux doctor` surfaces the same dangling mapping.
pub fn resolve_role_profile<'a>(
    role_id: &str,
    registry: &'a ProfileRegistry,
) -> Result<ResolvedRoleProfile<'a>> {
    // The config-only production entry knows nothing of launch overrides — it
    // reads the `role_profiles` map (→ `Mapped`) or resolves unmapped
    // (→ `Unmapped`). Override bindings enter only via `resolve_role_profile_with`
    // from a launcher that has the `--param <role>=<profile>` values (#1475 packet 3).
    let binding = match darkmux_types::config_access::role_profile(role_id) {
        Some(p) => RoleBinding::Mapped(p),
        None => RoleBinding::Unmapped,
    };
    resolve_role_profile_with(role_id, &binding, registry)
}

/// (#1475 packet 1/3) Pure core of [`resolve_role_profile`] — the binding is
/// supplied explicitly ([`RoleBinding::Overridden`]/[`Mapped`](RoleBinding::Mapped)
/// name the bound profile, [`Unmapped`](RoleBinding::Unmapped) falls through to
/// `default_profile`), so the resolution + validation is unit-testable without
/// the process-wide `config()`. Same layering as `config_access`'s `pick_*`
/// helpers taking explicit args. Override and map bindings resolve identically
/// (both name a profile that must exist); only the recorded
/// [`RoleProfileSource`] differs, so the envelope names the winning tier.
pub fn resolve_role_profile_with<'a>(
    role_id: &str,
    binding: &RoleBinding,
    registry: &'a ProfileRegistry,
) -> Result<ResolvedRoleProfile<'a>> {
    let (bound_name, source) = match binding {
        RoleBinding::Overridden(p) => (Some(p.as_str()), RoleProfileSource::Overridden),
        RoleBinding::Mapped(p) => (Some(p.as_str()), RoleProfileSource::Mapped),
        RoleBinding::Unmapped => (None, RoleProfileSource::DefaultFallback),
    };
    if let Some(profile_name) = bound_name {
        // Loud semantic validation (contract 7): a binding to a profile the
        // registry doesn't define is a clear error naming the role, the bad
        // profile, and the fix — NEVER a silent fallback to default_profile.
        // A quarantined target reuses the shared quarantine message.
        let profile = registry.profiles.get(profile_name).ok_or_else(|| {
            if let Some(msg) = registry.quarantine_error_for(profile_name) {
                return anyhow!(msg);
            }
            let available: Vec<&str> = registry.profiles.keys().map(String::as_str).collect();
            let listed = if available.is_empty() { "(none)".to_string() } else { available.join(", ") };
            // The fix suggestion tracks the binding tier: an override is fixed
            // on the launch line, a map binding via `config set`.
            let (how, fix) = match source {
                RoleProfileSource::Overridden => (
                    "is overridden to",
                    format!("check the `--param {role_id}=<profile>` launch value"),
                ),
                _ => (
                    "is mapped to",
                    format!("`darkmux config set role_profiles.{role_id} <profile>`"),
                ),
            };
            anyhow!(
                "darkmux: role \"{role_id}\" {how} profile \"{profile_name}\", \
                 which is not in the profile registry. Available: {listed}. \
                 Fix the binding — {fix} — or verify with `darkmux doctor`. (#1475)"
            )
        })?;
        return Ok(ResolvedRoleProfile {
            role_id: role_id.to_string(),
            profile_name: profile_name.to_string(),
            profile,
            source,
        });
    }

    // Unmapped role → default_profile, silently (the fresh-user floor).
    let default_name = registry.default_profile.as_deref().ok_or_else(|| {
        anyhow!(
            "darkmux: role \"{role_id}\" is not bound to a profile and the registry has no \
             default_profile — bind it (`darkmux config set role_profiles.{role_id} <profile>`) \
             or set a default_profile in profiles.json. (#1475)"
        )
    })?;
    // Reuse get_profile so a dangling/quarantined default_profile fails with the
    // standard message shape rather than a bespoke one.
    let profile = get_profile(registry, default_name)?;
    Ok(ResolvedRoleProfile {
        role_id: role_id.to_string(),
        profile_name: default_name.to_string(),
        profile,
        source: RoleProfileSource::DefaultFallback,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write(path: &Path, contents: &str) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    fn minimal_json() -> &'static str {
        r#"{
            "profiles": {
                "fast": {
                    "description": "tiny",
                    "models": [
                        {"id": "model-a", "n_ctx": 32000}
                    ]
                }
            },
            "default_profile": "fast"
        }"#
    }

    #[test]
    fn loads_explicit_path() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        write(&p, minimal_json());
        let loaded = load_registry(Some(p.to_str().unwrap())).unwrap();
        assert_eq!(loaded.path, p);
        assert!(loaded.registry.profiles.contains_key("fast"));
        assert_eq!(loaded.registry.default_profile.as_deref(), Some("fast"));
    }

    #[test]
    fn missing_path_errors_clearly() {
        let err = load_registry(Some("/no/such/path.json")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("registry not found") || msg.contains("no profile registry"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("--profiles flag"), "should attribute the source: {msg}");
    }

    /// `DARKMUX_PROFILES` must take precedence over the default search chain
    /// AND fail-fast when set to a non-existent path. Falling through to
    /// `~/.darkmux/profiles.json` would silently mask a typo.
    #[serial_test::serial]
    #[test]
    fn darkmux_config_env_var_fails_fast_on_missing() {
        unsafe { env::set_var("DARKMUX_PROFILES", "/tmp/darkmux-no-such-file.json") };
        let err = load_registry(None).unwrap_err();
        unsafe { env::remove_var("DARKMUX_PROFILES") };
        let msg = err.to_string();
        assert!(msg.contains("registry not found"), "got: {msg}");
        assert!(msg.contains("DARKMUX_PROFILES"), "should attribute the source: {msg}");
    }

    /// `DARKMUX_PROFILES` pointing at a real file should be honored — taking
    /// precedence over default search locations.
    #[serial_test::serial]
    #[test]
    fn darkmux_config_env_var_honored_when_file_exists() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("env-profiles.json");
        write(&p, minimal_json());
        unsafe { env::set_var("DARKMUX_PROFILES", p.to_str().unwrap()) };
        let result = load_registry(None);
        unsafe { env::remove_var("DARKMUX_PROFILES") };
        let loaded = result.unwrap();
        assert_eq!(loaded.path, p);
    }

    /// An empty `DARKMUX_PROFILES` value (e.g. `DARKMUX_PROFILES=`) should be
    /// treated as unset, falling through to the default search chain rather
    /// than failing with an empty-path error.
    #[serial_test::serial]
    #[test]
    fn darkmux_config_empty_falls_through() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(".darkmux.json");
        write(&p, minimal_json());
        let prev = env::current_dir().unwrap();
        env::set_current_dir(tmp.path()).unwrap();
        unsafe { env::set_var("DARKMUX_PROFILES", "") };
        let result = load_registry(None);
        unsafe { env::remove_var("DARKMUX_PROFILES") };
        env::set_current_dir(prev).unwrap();
        let loaded = result.unwrap();
        assert!(loaded.path.ends_with(".darkmux.json"));
    }

    #[test]
    fn validates_no_models() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        write(
            &p,
            r#"{"profiles":{"empty":{"models":[]}}}"#,
        );
        let err = load_registry(Some(p.to_str().unwrap())).unwrap_err();
        assert!(err.to_string().contains("at least one model"));
    }

    #[test]
    fn validates_multiple_models_no_role_constraint() {
        // (#590) The old "exactly one primary" rule is gone with `ModelRole`.
        // A profile may now declare any number of (unroled) models; the
        // default model is the first one when `default_model` is unset.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        write(
            &p,
            r#"{"profiles":{"ok":{"models":[
                {"id":"a","n_ctx":1},
                {"id":"b","n_ctx":2}
            ]}}}"#,
        );
        let reg = load_registry(Some(p.to_str().unwrap())).unwrap().registry;
        let prof = reg.profiles.get("ok").unwrap();
        assert_eq!(prof.models.len(), 2);
        // First model is the implicit default model.
        assert_eq!(prof.default_model_id(), Some("a"));
    }

    #[test]
    fn validates_default_model_must_name_a_real_model() {
        // (#590) The one new failure mode: `default_model` naming an id that
        // isn't in `models[]` (an operator typo) fails loud at load time.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        write(
            &p,
            r#"{"profiles":{"bad":{"default_model":"ghost","models":[
                {"id":"a","n_ctx":1}
            ]}}}"#,
        );
        let err = load_registry(Some(p.to_str().unwrap())).unwrap_err();
        assert!(err.to_string().contains("not one of its models"));
    }

    #[test]
    fn validates_empty_registry() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        write(&p, r#"{"profiles":{}}"#);
        let err = load_registry(Some(p.to_str().unwrap())).unwrap_err();
        assert!(err.to_string().contains("no profiles"));
    }

    #[test]
    fn rejects_invalid_json() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        write(&p, "this is not: valid: json: at all\n");
        let err = load_registry(Some(p.to_str().unwrap())).unwrap_err();
        assert!(err.to_string().contains("parsing JSON") || err.to_string().contains("JSON"));
    }

    // ── per-entry quarantine at parse (#1282) ──────────────────────
    // One structurally-broken entry must never blast the whole registry:
    // the entry is quarantined (name + serde's field-level error), siblings
    // load and work, and lookups on the quarantined name surface the parse
    // error + a doctor pointer instead of a misleading "not found".

    #[test]
    fn quarantined_profile_keeps_siblings_healthy_and_names_the_field() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        // "broken" omits the required `id` on its model — an entry-level
        // structural failure (the #1282 blast-radius class).
        write(
            &p,
            r#"{"profiles":{
                    "fast":{"models":[{"id":"a","n_ctx":1000}]},
                    "broken":{"models":[{"n_ctx":32000}]}
                },
                "default_profile":"fast"}"#,
        );
        let loaded = load_registry(Some(p.to_str().unwrap())).unwrap();

        // Sibling fully usable.
        let fast = get_profile(&loaded.registry, "fast").unwrap();
        assert_eq!(fast.models.len(), 1);

        // The broken entry is quarantined with name + serde's field error.
        assert_eq!(loaded.registry.quarantined.len(), 1);
        let q = &loaded.registry.quarantined[0];
        assert_eq!(q.kind, darkmux_types::QuarantinedEntryKind::Profile);
        assert_eq!(q.name, "broken");
        assert!(q.error.contains("missing field `id`"), "got: {}", q.error);
        assert!(!loaded.registry.profiles.contains_key("broken"));

        // Lookup on the quarantined name carries the reason + doctor pointer.
        let err = get_profile(&loaded.registry, "broken").unwrap_err().to_string();
        assert!(err.contains("quarantined"), "got: {err}");
        assert!(err.contains("missing field `id`"), "got: {err}");
        assert!(err.contains("darkmux doctor"), "got: {err}");
    }

    #[test]
    fn registry_whose_only_profile_is_quarantined_still_loads() {
        // Failing the load here would restore the whole-file blast the
        // quarantine exists to prevent (and hide the error from doctor).
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        write(&p, r#"{"profiles":{"broken":{"models":[{"n_ctx":1}]}}}"#);
        let loaded = load_registry(Some(p.to_str().unwrap())).unwrap();
        assert!(loaded.registry.profiles.is_empty());
        assert_eq!(loaded.registry.quarantined.len(), 1);
        assert_eq!(loaded.registry.quarantined[0].name, "broken");
    }

    /// The #1282 issue's exact repro — an endpoint-bearing model with NO
    /// `n_ctx` — is now simply VALID: it loads, it isn't quarantined, and
    /// the absent `n_ctx` survives a full file round-trip (absent, not
    /// null/0). The local-requires-n_ctx rule lives at resolution time.
    #[test]
    fn endpoint_profile_without_n_ctx_loads_and_round_trips() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        write(
            &p,
            r#"{"profiles":{"azure-x":{"models":[
                    {"id":"gpt-4o","endpoint":{"url":"https://example.azure.com/openai"}}
                ]}}}"#,
        );
        let loaded = load_registry(Some(p.to_str().unwrap())).unwrap();
        assert!(loaded.registry.quarantined.is_empty());
        let prof = get_profile(&loaded.registry, "azure-x").unwrap();
        assert_eq!(prof.models[0].n_ctx, None);
        assert!(prof.models[0].is_remote());

        // Round-trip through real file I/O: the absent field stays absent.
        let round = tmp.path().join("profiles-round.json");
        write(&round, &serde_json::to_string(&loaded.registry).unwrap());
        let reloaded = load_registry(Some(round.to_str().unwrap())).unwrap();
        let prof2 = get_profile(&reloaded.registry, "azure-x").unwrap();
        assert_eq!(prof2.models[0].n_ctx, None);
        assert!(!fs::read_to_string(&round).unwrap().contains("n_ctx"));
    }

    /// A LOCAL model without `n_ctx` is also legal AT PARSE (lenient-on-read
    /// contract #1269) — the error belongs to resolution-time consumers
    /// (swap, dispatch, crew resolution), each of which raises the uniform
    /// `require_n_ctx` error, and to `darkmux doctor`.
    #[test]
    fn local_profile_without_n_ctx_loads_at_parse() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        write(&p, r#"{"profiles":{"ctxless":{"models":[{"id":"local-a"}]}}}"#);
        let loaded = load_registry(Some(p.to_str().unwrap())).unwrap();
        assert!(loaded.registry.quarantined.is_empty());
        let prof = get_profile(&loaded.registry, "ctxless").unwrap();
        assert_eq!(prof.models[0].n_ctx, None);
        let err = prof.models[0].require_n_ctx().unwrap_err().to_string();
        assert!(err.contains("local-a") && err.contains("n_ctx"), "got: {err}");
    }

    #[test]
    fn get_profile_finds() {
        let reg: ProfileRegistry = serde_json::from_str(minimal_json()).unwrap();
        let p = get_profile(&reg, "fast").unwrap();
        assert_eq!(p.models.len(), 1);
        // (#590) The default model is the first model when `default_model` is
        // unset (replaces the old Primary-role designation).
        assert_eq!(p.default_model_id(), Some("model-a"));
    }

    #[test]
    fn get_profile_missing_lists_alternatives() {
        let reg: ProfileRegistry = serde_json::from_str(minimal_json()).unwrap();
        let err = get_profile(&reg, "nonexistent").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"));
        assert!(msg.contains("fast"));
    }

    #[test]
    fn get_profile_missing_with_empty_registry() {
        let reg = ProfileRegistry::default();
        let err = get_profile(&reg, "anything").unwrap_err();
        assert!(err.to_string().contains("(none)"));
    }

    // ── role -> profile resolution (#1475 packet 1) ──────────────────
    // Build a registry with two role-agnostic profiles + a default. The pure
    // `resolve_role_profile_with` takes the mapped profile name explicitly, so
    // every arm (mapped / unmapped / dangling map) is testable with no config().

    fn two_profile_reg() -> ProfileRegistry {
        // `qwen35b` (judge/verify would bind here) and `qwen4b` (a probe), plus
        // a `qwen4b` default so an unmapped role has a floor.
        serde_json::from_str(
            r#"{
                "profiles": {
                    "qwen35b": {"models": [{"id": "qwen3.5-35b", "n_ctx": 40000}]},
                    "qwen4b":  {"models": [{"id": "qwen3-4b", "n_ctx": 16000}]}
                },
                "default_profile": "qwen4b"
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn resolve_mapped_role_binds_to_its_profile() {
        let reg = two_profile_reg();
        let r = resolve_role_profile_with("judge", &RoleBinding::Mapped("qwen35b".into()), &reg).unwrap();
        assert_eq!(r.profile_name, "qwen35b");
        assert_eq!(r.profile.models[0].id, "qwen3.5-35b");
        assert_eq!(r.source, RoleProfileSource::Mapped);
        assert_eq!(r.role_id, "judge");
    }

    #[test]
    fn resolve_overridden_role_binds_and_records_overridden_source() {
        // (#1475 packet 3) A per-run launch override binds identically to a map
        // binding, but the recorded source is `Overridden` (highest precedence)
        // so the envelope names the winning tier.
        let reg = two_profile_reg();
        let r = resolve_role_profile_with("judge", &RoleBinding::Overridden("qwen35b".into()), &reg).unwrap();
        assert_eq!(r.profile_name, "qwen35b");
        assert_eq!(r.source, RoleProfileSource::Overridden);
    }

    #[test]
    fn resolve_overridden_to_nonexistent_profile_names_the_launch_value() {
        // (#1475 packet 3) A bad override profile is a loud error whose fix
        // suggestion points at the launch line, not `config set`.
        let reg = two_profile_reg();
        let err = resolve_role_profile_with("judge", &RoleBinding::Overridden("ghost35b".into()), &reg)
            .unwrap_err()
            .to_string();
        assert!(err.contains("overridden to"), "names the tier: {err}");
        assert!(err.contains("ghost35b"), "names the bad profile: {err}");
        assert!(err.contains("--param judge=<profile>"), "points at the launch value: {err}");
    }

    #[test]
    fn resolve_unmapped_role_falls_back_to_default_profile() {
        let reg = two_profile_reg();
        // An unmapped role degrades to default_profile SILENTLY — the fresh-user
        // single-model floor.
        let r = resolve_role_profile_with("probe-high", &RoleBinding::Unmapped, &reg).unwrap();
        assert_eq!(r.profile_name, "qwen4b", "unmapped → default_profile");
        assert_eq!(r.source, RoleProfileSource::DefaultFallback);
    }

    #[test]
    fn resolve_mapped_to_nonexistent_profile_is_a_loud_error() {
        let reg = two_profile_reg();
        // Contract 7: a mapping to a profile the registry doesn't define is a
        // clear typed error naming the role + the bad profile + the fix, NEVER a
        // silent fallback to default_profile.
        let err = resolve_role_profile_with("judge", &RoleBinding::Mapped("ghost35b".into()), &reg)
            .unwrap_err()
            .to_string();
        assert!(err.contains("judge"), "names the role: {err}");
        assert!(err.contains("ghost35b"), "names the bad profile: {err}");
        assert!(err.contains("not in the profile registry"), "explains the failure: {err}");
        assert!(err.contains("darkmux config set role_profiles.judge"), "names the fix: {err}");
    }

    #[test]
    fn resolve_unmapped_with_no_default_profile_errors() {
        // No default_profile → an unmapped role has no floor; a clear error, not
        // a panic or silent empty resolution.
        let reg: ProfileRegistry = serde_json::from_str(
            r#"{"profiles": {"qwen35b": {"models": [{"id": "m", "n_ctx": 40000}]}}}"#,
        )
        .unwrap();
        let err = resolve_role_profile_with("judge", &RoleBinding::Unmapped, &reg).unwrap_err().to_string();
        assert!(err.contains("not bound"), "names the unbound role: {err}");
        assert!(err.contains("no default_profile"), "explains the missing floor: {err}");
    }

    #[test]
    fn resolve_mapped_to_quarantined_profile_surfaces_quarantine_reason() {
        // A role mapped to a profile that FAILED per-entry parse (quarantined,
        // #1282) surfaces the quarantine reason + doctor pointer, not a generic
        // "not in the registry".
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        write(
            &p,
            r#"{"profiles":{
                    "qwen4b":{"models":[{"id":"a","n_ctx":1000}]},
                    "broken":{"models":[{"n_ctx":32000}]}
                },
                "default_profile":"qwen4b"}"#,
        );
        let reg = load_registry(Some(p.to_str().unwrap())).unwrap().registry;
        let err = resolve_role_profile_with("judge", &RoleBinding::Mapped("broken".into()), &reg)
            .unwrap_err()
            .to_string();
        assert!(err.contains("quarantined"), "got: {err}");
        assert!(err.contains("darkmux doctor"), "points at doctor: {err}");
    }

    #[test]
    /// `default_locations()` is the *fallback* chain only — DARKMUX_PROFILES
    /// short-circuits in `load_registry`, not here. The default chain itself
    /// should not include the env var path.
    #[serial_test::serial]
    fn default_locations_does_not_include_env() {
        unsafe {
            std::env::set_var("DARKMUX_PROFILES", "/tmp/test-darkmux.json");
        }
        let locs = default_locations();
        unsafe {
            std::env::remove_var("DARKMUX_PROFILES");
        }
        assert!(
            !locs.iter().any(|p| p.to_str().unwrap().contains("test-darkmux.json")),
            "default_locations leaked DARKMUX_PROFILES into the fallback chain: {:?}",
            locs
        );
    }

}
