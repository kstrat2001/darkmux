//! `darkmux lab run <workload> [opts]` — execute a workload and capture output.

use crate::lab::paths::{self, ResolveScope};
use crate::profiles::{get_profile, load_registry};
use crate::workloads::load::{list_available, load};
use crate::workloads::registry::with_provider;
use anyhow::{Context, Result, anyhow};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct RunOpts {
    pub workload_id: String,
    pub profile_name: Option<String>,
    pub runs: u32,
    pub config_path: Option<String>,
    pub quiet: bool,
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub run_id: String,
    pub run_dir: std::path::PathBuf,
    /// Did the dispatch itself complete successfully (runtime exit 0, reply
    /// payload received)? Distinct from `verify_passed` — a dispatch can
    /// succeed but its reply may not pass keyword/test verification.
    pub ok: bool,
    /// Did the workload's verify spec pass? `None` if no verify was defined.
    pub verify_passed: Option<bool>,
    pub duration_ms: u128,
    pub notes: Vec<String>,
}

pub fn lab_run(opts: RunOpts) -> Result<Vec<RunOutcome>> {
    let paths = paths::resolve(ResolveScope::Auto);
    paths::ensure(&paths)?;

    let user_dir = if paths.scope == paths::Scope::Project || paths.scope == paths::Scope::User {
        Some(paths.root.as_path())
    } else {
        None
    };
    let loaded_workload = load(&opts.workload_id, user_dir)?;

    let registry_loaded = load_registry(opts.config_path.as_deref())?;
    let profile_name = opts
        .profile_name
        .clone()
        .or_else(|| registry_loaded.registry.default_profile.clone())
        .ok_or_else(|| anyhow!("no profile specified and no default_profile in registry"))?;
    let profile = get_profile(&registry_loaded.registry, &profile_name)?;

    let runs = opts.runs.max(1);
    let mut outcomes: Vec<RunOutcome> = Vec::new();

    for i in 1..=runs {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let run_id = format!("{}-{}-{}-{}", opts.workload_id, profile_name, stamp, i);
        let run_dir = paths.runs.join(&run_id);
        let sandbox_dir = resolve_sandbox_dir(&opts.workload_id, &paths);

        if !opts.quiet {
            println!(
                "[lab] run {i}/{runs} — workload={} profile={} → {}",
                opts.workload_id, profile_name, run_id
            );
        }

        fs::create_dir_all(&run_dir)
            .with_context(|| format!("creating {}", run_dir.display()))?;

        let provider_id = loaded_workload.manifest.workload.provider.clone();
        let result = with_provider(&provider_id, |p| {
            p.setup(&loaded_workload, &run_dir, &sandbox_dir)?;
            p.run(&loaded_workload, &run_dir, &sandbox_dir, profile, &profile_name)
        })??;

        let mut notes = vec![
            format!("provider={}", provider_id),
            format!("wall={}s", result.duration_ms / 1000),
            if result.ok {
                "ok".to_string()
            } else {
                format!("error: {}", result.error.as_deref().unwrap_or("unknown"))
            },
        ];
        if let Some(v) = result.verify.as_ref() {
            notes.push(format!(
                "verify={} ({})",
                if v.passed { "pass" } else { "fail" },
                v.details
            ));
        }

        if !opts.quiet {
            println!("  {}", notes.join(" | "));
        }

        outcomes.push(RunOutcome {
            run_id,
            run_dir,
            ok: result.ok,
            verify_passed: result.verify.as_ref().map(|v| v.passed),
            duration_ms: result.duration_ms,
            notes,
        });
    }

    Ok(outcomes)
}

pub fn lab_workloads() -> Vec<String> {
    let paths = paths::resolve(ResolveScope::Auto);
    list_available(Some(&paths.root))
}

/// Resolve the sandbox directory for a workload. By default, lives under
/// `<paths.sandboxes>/<workload-id>/`. Override with the env var
/// `DARKMUX_SANDBOX_<WORKLOAD-ID-UPPERCASED-WITH-DASHES-AS-UNDERSCORES>`,
/// which is how a user points at an existing on-disk Node project (etc.)
/// without having to ship the project as a sandbox seed in the workload.
///
/// Example: workload `long-agentic` looks at
/// `DARKMUX_SANDBOX_LONG_AGENTIC`. If set and the path exists, that's the
/// sandbox; otherwise the default path is used.
pub fn resolve_sandbox_dir(
    workload_id: &str,
    paths: &paths::DarkmuxPaths,
) -> std::path::PathBuf {
    let env_key = format!(
        "DARKMUX_SANDBOX_{}",
        workload_id.replace('-', "_").to_ascii_uppercase()
    );
    if let Ok(p) = std::env::var(&env_key) {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    paths.sandboxes.join(workload_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn workloads_returns_strings_without_panicking() {
        // Just verify the function doesn't panic on a fresh user dir.
        let _ = lab_workloads();
    }

    #[serial_test::serial]
    #[test]
    fn run_errors_when_no_default_profile_and_no_explicit() {
        let tmp = TempDir::new().unwrap();
        // Set up a registry without default_profile.
        let cfg = tmp.path().join("profiles.json");
        fs::write(
            &cfg,
            r#"{"profiles":{"fast":{"models":[{"id":"model-a","n_ctx":32000,"role":"primary"}]}}}"#,
        )
        .unwrap();
        // Set up a workload manifest in the user dir.
        let darkmux_dir = tmp.path().join(".darkmux");
        fs::create_dir_all(darkmux_dir.join("workloads")).unwrap();
        fs::write(
            darkmux_dir.join("workloads/q.json"),
            r#"{"workload":{"id":"q","provider":"prompt","prompt":"hi"}}"#,
        )
        .unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let err = lab_run(RunOpts {
            workload_id: "q".into(),
            profile_name: None,
            runs: 1,
            config_path: Some(cfg.to_str().unwrap().into()),
            quiet: true,
        })
        .unwrap_err();
        std::env::set_current_dir(prev).unwrap();
        assert!(err.to_string().contains("default_profile"));
    }
}
