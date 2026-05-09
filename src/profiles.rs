use crate::types::{Profile, ProfileRegistry};
use anyhow::{Context, Result, anyhow, bail};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct LoadedRegistry {
    pub registry: ProfileRegistry,
    pub path: PathBuf,
}

/// Default search locations when neither `--config` nor `DARKMUX_CONFIG` is
/// set. `DARKMUX_CONFIG`, if set, short-circuits this list — see `load_registry`.
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
    // Precedence: explicit --config flag > DARKMUX_CONFIG env var > default
    // search locations. The two override paths fail-fast on missing files
    // so that a typo'd path doesn't silently pick up an unrelated registry.
    if let Some(p) = explicit {
        return load_from(PathBuf::from(p), "--config flag");
    }
    if let Ok(p) = env::var("DARKMUX_CONFIG") {
        if !p.is_empty() {
            return load_from(PathBuf::from(p), "DARKMUX_CONFIG env var");
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
    let registry: ProfileRegistry = serde_json::from_str(&raw)
        .with_context(|| format!("parsing JSON at {}", path.display()))?;
    validate_registry(&registry, &path)?;
    Ok(LoadedRegistry { registry, path })
}

fn validate_registry(reg: &ProfileRegistry, path: &Path) -> Result<()> {
    if reg.profiles.is_empty() {
        bail!("{}: registry has no profiles", path.display());
    }
    for (name, profile) in &reg.profiles {
        validate_profile(name, profile, path)?;
    }
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
    let primaries = profile
        .models
        .iter()
        .filter(|m| matches!(m.role, crate::types::ModelRole::Primary))
        .count();
    if primaries != 1 {
        bail!(
            "{}: profile \"{}\" must have exactly one primary model (found {})",
            path.display(),
            name,
            primaries
        );
    }
    Ok(())
}

pub fn get_profile<'a>(reg: &'a ProfileRegistry, name: &str) -> Result<&'a Profile> {
    reg.profiles.get(name).ok_or_else(|| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ModelRole;
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
                        {"id": "model-a", "n_ctx": 32000, "role": "primary"}
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
        assert!(msg.contains("--config flag"), "should attribute the source: {msg}");
    }

    /// `DARKMUX_CONFIG` must take precedence over the default search chain
    /// AND fail-fast when set to a non-existent path. Falling through to
    /// `~/.darkmux/profiles.json` would silently mask a typo.
    #[serial_test::serial]
    #[test]
    fn darkmux_config_env_var_fails_fast_on_missing() {
        unsafe { env::set_var("DARKMUX_CONFIG", "/tmp/darkmux-no-such-file.json") };
        let err = load_registry(None).unwrap_err();
        unsafe { env::remove_var("DARKMUX_CONFIG") };
        let msg = err.to_string();
        assert!(msg.contains("registry not found"), "got: {msg}");
        assert!(msg.contains("DARKMUX_CONFIG"), "should attribute the source: {msg}");
    }

    /// `DARKMUX_CONFIG` pointing at a real file should be honored — taking
    /// precedence over default search locations.
    #[serial_test::serial]
    #[test]
    fn darkmux_config_env_var_honored_when_file_exists() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("env-profiles.json");
        write(&p, minimal_json());
        unsafe { env::set_var("DARKMUX_CONFIG", p.to_str().unwrap()) };
        let result = load_registry(None);
        unsafe { env::remove_var("DARKMUX_CONFIG") };
        let loaded = result.unwrap();
        assert_eq!(loaded.path, p);
    }

    /// An empty `DARKMUX_CONFIG` value (e.g. `DARKMUX_CONFIG=`) should be
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
        unsafe { env::set_var("DARKMUX_CONFIG", "") };
        let result = load_registry(None);
        unsafe { env::remove_var("DARKMUX_CONFIG") };
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
    fn validates_two_primaries() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        write(
            &p,
            r#"{"profiles":{"bad":{"models":[
                {"id":"a","n_ctx":1,"role":"primary"},
                {"id":"b","n_ctx":2,"role":"primary"}
            ]}}}"#,
        );
        let err = load_registry(Some(p.to_str().unwrap())).unwrap_err();
        assert!(err.to_string().contains("exactly one primary"));
    }

    #[test]
    fn validates_zero_primaries() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        write(
            &p,
            r#"{"profiles":{"bad":{"models":[
                {"id":"a","n_ctx":1,"role":"compactor"}
            ]}}}"#,
        );
        let err = load_registry(Some(p.to_str().unwrap())).unwrap_err();
        assert!(err.to_string().contains("exactly one primary"));
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

    #[test]
    fn get_profile_finds() {
        let reg: ProfileRegistry = serde_json::from_str(minimal_json()).unwrap();
        let p = get_profile(&reg, "fast").unwrap();
        assert_eq!(p.models.len(), 1);
        assert!(matches!(p.models[0].role, ModelRole::Primary));
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

    #[test]
    /// `default_locations()` is the *fallback* chain only — DARKMUX_CONFIG
    /// short-circuits in `load_registry`, not here. The default chain itself
    /// should not include the env var path.
    #[serial_test::serial]
    fn default_locations_does_not_include_env() {
        unsafe {
            std::env::set_var("DARKMUX_CONFIG", "/tmp/test-darkmux.json");
        }
        let locs = default_locations();
        unsafe {
            std::env::remove_var("DARKMUX_CONFIG");
        }
        assert!(
            !locs.iter().any(|p| p.to_str().unwrap().contains("test-darkmux.json")),
            "default_locations leaked DARKMUX_CONFIG into the fallback chain: {:?}",
            locs
        );
    }
}
