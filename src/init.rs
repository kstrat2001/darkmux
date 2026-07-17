//! `darkmux init` — one-command integration setup.
//!
//! Installs skills + (optional) SessionStart hook + (optional) CLAUDE.md
//! merge so Claude Code knows about darkmux without further configuration.
//!
//! Safe to re-run; refreshes the bundled skills after a darkmux upgrade. The
//! skill-install step passes `refresh_darkmux: true` (#1426), so a re-run
//! overwrites the installed `darkmux-*` skills with this binary's embedded
//! copies — never touching the operator's own non-darkmux skills. This is what
//! the doctor freshness check's fix_hint (`run darkmux init to refresh`) relies
//! on. The profile registry and config file keep their never-overwrite
//! discipline; only the darkmux-owned skills refresh.

use crate::skills;
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct InitOptions {
    pub with_hook: bool,
    pub with_claude_md: Option<PathBuf>,
    pub with_agents_md: Option<PathBuf>,
    pub force: bool,
    pub dry_run: bool,
}

#[derive(Debug, Default)]
pub struct InitReport {
    pub profile_registry_path: Option<PathBuf>,
    pub profile_registry_created: bool,
    pub profile_registry_already_present: bool,
    pub config_path: Option<PathBuf>,
    pub config_created: bool,
    pub config_already_present: bool,
    pub skills_targets: Vec<PathBuf>,
    pub skills_installed: Vec<String>,
    pub skills_overwritten: Vec<String>,
    pub skills_skipped: Vec<String>,
    pub hook_added: Option<PathBuf>,
    pub hook_already_present: bool,
    pub claude_md_path: Option<PathBuf>,
    pub claude_md_appended: bool,
    pub claude_md_already_present: bool,
    pub agents_md_path: Option<PathBuf>,
    pub agents_md_appended: bool,
    pub agents_md_already_present: bool,
}

/// The example profile registry, embedded at compile time. Copied to
/// `~/.darkmux/profiles.json` on `darkmux init` if no registry exists yet.
const EXAMPLE_PROFILES_JSON: &str = include_str!("../profiles.example.json");

/// The committed default config (`config.example.json`) — byte-equal to
/// `DarkmuxConfig::with_defaults()`'s pretty output, i.e. exactly what
/// `darkmux init` writes (modulo the personalized `machine_id`). `#[cfg(test)]`
/// only: not embedded in the release binary; it exists so
/// `example_config_matches_with_defaults` can drift-guard the committed
/// reference against the code (#661).
#[cfg(test)]
const EXAMPLE_CONFIG: &str = include_str!("../config.example.json");

pub fn init(opts: &InitOptions) -> Result<InitReport> {
    let mut report = InitReport::default();

    // 1) Bootstrap the profile registry (the "fresh user has no config" case).
    //    Never overwrites — even with --force — because the user's profile
    //    edits matter more than re-running init.
    let registry_path = user_profile_registry_path()?;
    report.profile_registry_path = Some(registry_path.clone());
    if registry_path.exists() {
        report.profile_registry_already_present = true;
    } else if !opts.dry_run {
        if let Some(parent) = registry_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&registry_path, EXAMPLE_PROFILES_JSON)
            .with_context(|| format!("writing {}", registry_path.display()))?;
        report.profile_registry_created = true;
    } else {
        // dry-run: report what would happen without writing
        report.profile_registry_created = true;
    }

    // 2) Bootstrap the config file (#661). Same never-overwrite discipline as
    //    the profile registry — the operator's config edits outrank a re-run.
    let (config_path, config_created) = bootstrap_config(opts.dry_run)?;
    report.config_already_present = !config_created;
    report.config_path = Some(config_path);
    report.config_created = config_created;

    // 3) Skills install. `refresh_darkmux: true` (#1426) makes a re-run after a
    //    binary upgrade refresh the installed darkmux-* skills (idempotent
    //    refresh); the installer only ever writes darkmux-* names, so the
    //    operator's own skills are never touched.
    let skills_report = skills::install_skills(&skills::InstallOptions {
        target: None,
        force: opts.force,
        dry_run: opts.dry_run,
        refresh_darkmux: true,
    })?;
    report.skills_targets = skills_report.targets;
    report.skills_installed = skills_report.installed;
    report.skills_overwritten = skills_report.overwritten;
    report.skills_skipped = skills_report.skipped;

    // 4) SessionStart hook (optional)
    if opts.with_hook {
        let settings = claude_settings_path()?;
        let result = ensure_session_start_hook(&settings, opts.dry_run, opts.force)?;
        report.hook_added = Some(settings);
        report.hook_already_present = !result;
    }

    // 5) CLAUDE.md merge (optional)
    if let Some(target) = opts.with_claude_md.as_ref() {
        let appended = ensure_claude_md_section(target, opts.dry_run)?;
        report.claude_md_path = Some(target.clone());
        report.claude_md_appended = appended;
        report.claude_md_already_present = !appended;
    }

    // 6) AGENTS.md merge (optional)
    if let Some(target) = opts.with_agents_md.as_ref() {
        let appended = ensure_agents_md_section(target, opts.dry_run)?;
        report.agents_md_path = Some(target.clone());
        report.agents_md_appended = appended;
        report.agents_md_already_present = !appended;
    }

    Ok(report)
}

fn user_profile_registry_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    Ok(home.join(".darkmux").join("profiles.json"))
}

/// Bootstrap `<root>/config.json` (#661). Writes the **full self-documenting
/// default config** (`DarkmuxConfig::with_defaults`) with `machine_id`
/// personalized to this machine — every common knob visible + editable, so the
/// operator tunes the file rather than hunting hidden code-defaults. The
/// integration features (`redis`, `audit`) are written as `enabled: false`
/// blocks with their sub-defaults populated (the full surface is discoverable
/// and one flip from on). Skip-if-exists (never overwrites, even under
/// `--force`: the operator's config edits outrank a re-run). Returns
/// `(path, created)`; `created` is `true` in dry-run to report the intent.
///
/// The write target routes through `paths::resolve(ForceUser).config`, so it
/// honors the `DARKMUX_HOME` bootstrap pointer and lands where the runtime
/// reads.
fn bootstrap_config(dry_run: bool) -> Result<(PathBuf, bool)> {
    use darkmux_types::config::DarkmuxConfig;
    use darkmux_types::paths::{ResolveScope, resolve};

    let config_path = resolve(ResolveScope::ForceUser).config;
    if config_path.exists() {
        return Ok((config_path, false));
    }
    if dry_run {
        return Ok((config_path, true));
    }
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut cfg = DarkmuxConfig::with_defaults();
    if let Some(name) = computer_name() {
        cfg.machine_id = Some(name);
    }
    let mut json = serde_json::to_string_pretty(&cfg).context("serializing config.json")?;
    json.push('\n');
    fs::write(&config_path, json).with_context(|| format!("writing {}", config_path.display()))?;
    Ok((config_path, true))
}

/// The friendly machine name to seed `machine_id` with, so the operator gets a
/// visible, editable identity in their config rather than a silent hostname
/// fallback at every record write. macOS `scutil --get LocalHostName` (the
/// Bonjour name — friendly *and* identifier-safe, no spaces) → `hostname`
/// (with a trailing `.local` trimmed) → `None`. Operator-sovereignty: the
/// seed is a starting point they'll likely rename to `studio`/`laptop`.
fn computer_name() -> Option<String> {
    fn run(cmd: &str, args: &[&str]) -> Option<String> {
        let out = std::process::Command::new(cmd).args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if name.is_empty() { None } else { Some(name) }
    }

    #[cfg(target_os = "macos")]
    if let Some(name) = run("scutil", &["--get", "LocalHostName"]) {
        return Some(name);
    }
    // Trim a trailing `.local`, then re-check for empty — a host named literally
    // `.local` trims to `""`, which must stay `None` (cleanly omitted), never a
    // `machine_id: ""` written into the config.
    run("hostname", &[]).and_then(|h| {
        let trimmed = h.trim_end_matches(".local");
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

const HOOK_MARKER: &str = "darkmux:session-start";
const CLAUDE_MD_HEADER: &str = "<!-- darkmux:integration:start -->";
const CLAUDE_MD_FOOTER: &str = "<!-- darkmux:integration:end -->";
const AGENTS_MD_HEADER: &str = "<!-- darkmux:integration:agents:start -->";
const AGENTS_MD_FOOTER: &str = "<!-- darkmux:integration:agents:end -->";

/// Shared body of the integration section that `--with-claude-md` /
/// `--with-agents-md` emit into users' project docs. One constant feeds both
/// templates so they cannot drift from each other or from the 2.0 identity
/// (#1449).
const INTEGRATION_SECTION_BODY: &str = r#"# darkmux

This project uses [darkmux](https://github.com/kstrat2001/darkmux), a mission orchestrator and lab for local AI. You dispatch roles and launch missions to a crew of local-AI seats; each seat runs local (your own models, off the meter) or cloud (a hosted endpoint when a role needs frontier weights). darkmux keeps the right models resident at the right context under your RAM budget — you don't manage residency by hand.

## Available skills

- `/darkmux-status` — what's currently loaded
- `/darkmux-list-stacks` — see all available profiles
- `/darkmux-list-workloads` / `/darkmux-lab-run` — execute lab workloads
- `/darkmux-list-runs` / `/darkmux-analyze-run` / `/darkmux-compare-runs` — inspect run history

## Dispatch policy

Launch a config-defined mission with `darkmux mission launch <config>` and watch it run as a live task graph, gated on your sign-off; each run finalizes into a typed envelope. For a single turn, `darkmux dispatch <role> "<text>"` sends work to one seat. Before relying on a config, measure it with `darkmux lab run <workload>` (wall clock, compaction events, verify outcome) so your choices rest on numbers, not guesses."#;

fn claude_settings_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    Ok(home.join(".claude").join("settings.json"))
}

/// Add a SessionStart hook that runs `darkmux machine status` so Claude sees
/// the current stack at the start of every session. Returns true if the hook
/// was newly added (false if already present). (#1426 — `status` folded into
/// the `machine` family.)
fn ensure_session_start_hook(settings_path: &Path, dry_run: bool, force: bool) -> Result<bool> {
    if !settings_path.exists() {
        if dry_run {
            return Ok(true);
        }
        if let Some(parent) = settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(settings_path, "{}\n")?;
    }

    let raw = fs::read_to_string(settings_path)
        .with_context(|| format!("reading {}", settings_path.display()))?;
    let mut value: Value = serde_json::from_str(&raw).unwrap_or_else(|_| Value::Object(Default::default()));

    let hooks = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json is not a JSON object"))?
        .entry("hooks".to_string())
        .or_insert(Value::Object(Default::default()));
    let hooks_obj = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json hooks is not an object"))?;
    let arr = hooks_obj
        .entry("SessionStart".to_string())
        .or_insert(Value::Array(Vec::new()));
    let arr = arr
        .as_array_mut()
        .ok_or_else(|| anyhow!("settings.json hooks.SessionStart is not an array"))?;

    let already_present = arr.iter().any(|h| {
        h.get("command")
            .and_then(|c| c.as_str())
            .map(|c| c.contains(HOOK_MARKER))
            .unwrap_or(false)
    });
    if already_present && !force {
        return Ok(false);
    }
    if already_present && force {
        arr.retain(|h| {
            !h.get("command")
                .and_then(|c| c.as_str())
                .map(|c| c.contains(HOOK_MARKER))
                .unwrap_or(false)
        });
    }

    arr.push(serde_json::json!({
        "type": "command",
        "command": format!("# {HOOK_MARKER}\n/usr/bin/env -S sh -c 'darkmux machine status 2>/dev/null || true'")
    }));

    if !dry_run {
        let pretty = serde_json::to_string_pretty(&value)?;
        fs::write(settings_path, pretty + "\n")?;
    }
    Ok(true)
}

/// Append (or replace, with --force) a darkmux integration section into a
/// CLAUDE.md file. Idempotent via the marker comments — running twice
/// without --force is a no-op.
fn ensure_claude_md_section(target: &Path, dry_run: bool) -> Result<bool> {
    let existing = if target.exists() {
        fs::read_to_string(target)?
    } else {
        String::new()
    };

    if existing.contains(CLAUDE_MD_HEADER) {
        return Ok(false);
    }

    let section = darkmux_claude_md_section();
    let new_contents = if existing.is_empty() {
        section
    } else {
        format!("{}\n\n{}", existing.trim_end(), section)
    };

    if !dry_run {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, new_contents)?;
    }
    Ok(true)
}

fn darkmux_claude_md_section() -> String {
    format!(
        "{header}\n\n{body}\n\n{footer}\n",
        header = CLAUDE_MD_HEADER,
        body = INTEGRATION_SECTION_BODY,
        footer = CLAUDE_MD_FOOTER,
    )
}

/// Append (or replace, with --force) a darkmux integration section into an
/// AGENTS.md file. Idempotent via the marker comments — running twice
/// without --force is a no-op.
fn ensure_agents_md_section(target: &Path, dry_run: bool) -> Result<bool> {
    let existing = if target.exists() {
        fs::read_to_string(target)?
    } else {
        String::new()
    };

    if existing.contains(AGENTS_MD_HEADER) {
        return Ok(false);
    }

    let section = darkmux_agents_md_section();
    let new_contents = if existing.is_empty() {
        section
    } else {
        format!("{}\n\n{}", existing.trim_end(), section)
    };

    if !dry_run {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, new_contents)?;
    }
    Ok(true)
}

fn darkmux_agents_md_section() -> String {
    format!(
        "{header}\n\n{body}\n\n{footer}\n",
        header = AGENTS_MD_HEADER,
        body = INTEGRATION_SECTION_BODY,
        footer = AGENTS_MD_FOOTER,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn example_config_matches_with_defaults() {
        // The committed `config.example.json` is the single source of truth's
        // output: it must equal `to_string_pretty(with_defaults())` + a trailing
        // newline (exactly what `init` writes, modulo the personalized
        // machine_id). This is the drift guard — change a default in code and
        // this test fails until the example is regenerated.
        use darkmux_types::config::DarkmuxConfig;
        let expected = format!(
            "{}\n",
            serde_json::to_string_pretty(&DarkmuxConfig::with_defaults()).unwrap()
        );
        assert_eq!(
            EXAMPLE_CONFIG, expected,
            "config.example.json drifted from DarkmuxConfig::with_defaults() — regenerate it"
        );
        // And the integration features are present as `enabled: false` blocks
        // (visible surface, off), not absent.
        let cfg: DarkmuxConfig = serde_json::from_str(EXAMPLE_CONFIG).unwrap();
        assert_eq!(cfg.redis.as_ref().and_then(|r| r.enabled), Some(false));
        assert_eq!(cfg.audit.as_ref().and_then(|a| a.enabled), Some(false));
        assert!(cfg.extras.is_empty(), "example must use only documented keys");
    }

    #[serial_test::serial]
    #[test]
    fn bootstrap_config_writes_full_personalized_and_skips_if_exists() {
        use darkmux_types::config::{CONFIG_SCHEMA_VERSION, DarkmuxConfig};
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()); }

        // First run writes the full self-documenting default config.
        let (path, created) = bootstrap_config(false).unwrap();
        assert!(created, "fresh config is created");
        assert_eq!(path, tmp.path().join("config.json"));
        let cfg: DarkmuxConfig = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.schema_version.as_deref(), Some(CONFIG_SCHEMA_VERSION));
        assert!(cfg.machine_id.is_some(), "machine_id seeded from the computer name");
        // Full, visible surface: the feature blocks are present + off, and the
        // scalar defaults are written (not left to invisible code-defaults).
        assert_eq!(cfg.redis.as_ref().and_then(|r| r.enabled), Some(false));
        assert_eq!(cfg.redis.as_ref().and_then(|r| r.maxlen), Some(10_000));
        assert_eq!(cfg.audit.as_ref().and_then(|a| a.enabled), Some(false));
        assert_eq!(
            cfg.runtime.as_ref().and_then(|r| r.inactivity_timeout_seconds),
            Some(600)
        );
        // (#1276) The bounded model-load phase ships visible, same pattern.
        assert_eq!(
            cfg.runtime.as_ref().and_then(|r| r.model_load_timeout_seconds),
            Some(600)
        );
        // The written config personalizes machine_id away from the placeholder.
        assert_ne!(cfg.machine_id.as_deref(), Some("my-machine"));
        // Derived/advanced fields stay absent (dirs derived; caps = uncapped).
        assert!(cfg.dirs.is_none(), "derived dirs are surfaced by doctor, not frozen");

        // Second run never overwrites — returns (same path, false).
        let (path2, created2) = bootstrap_config(false).unwrap();
        assert_eq!(path2, path);
        assert!(!created2, "existing config is left untouched");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn bootstrap_config_dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()); }

        let (path, created) = bootstrap_config(true).unwrap();
        assert!(created, "dry-run reports the intent to create");
        assert!(!path.exists(), "dry-run writes nothing");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    #[test]
    fn claude_md_section_includes_marker_comments() {
        let s = darkmux_claude_md_section();
        assert!(s.contains(CLAUDE_MD_HEADER));
        assert!(s.contains(CLAUDE_MD_FOOTER));
        assert!(s.contains(INTEGRATION_SECTION_BODY));
    }

    #[test]
    fn agents_md_section_includes_marker_comments() {
        let s = darkmux_agents_md_section();
        assert!(s.contains(AGENTS_MD_HEADER));
        assert!(s.contains(AGENTS_MD_FOOTER));
        assert!(s.contains(INTEGRATION_SECTION_BODY));
    }

    #[test]
    fn integration_section_body_teaches_current_identity() {
        // Both templates share this body, so one guard covers both.
        let s = INTEGRATION_SECTION_BODY;
        assert!(s.contains("/darkmux-status"));
        assert!(s.contains("mission orchestrator and lab"));
        assert!(s.contains("mission launch"));
        assert!(s.contains("lab run"));
        // The retired multiplexer/swap identity must not resurface (#1449).
        assert!(!s.contains("multiplex"));
        assert!(!s.contains("swap"));
    }

    #[test]
    fn ensure_claude_md_appends_to_existing() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("CLAUDE.md");
        fs::write(&p, "# Project\n\nExisting content here.\n").unwrap();
        let appended = ensure_claude_md_section(&p, false).unwrap();
        assert!(appended);
        let after = fs::read_to_string(&p).unwrap();
        assert!(after.contains("Existing content here"));
        assert!(after.contains(CLAUDE_MD_HEADER));
        assert!(after.contains("# darkmux"));
    }

    #[test]
    fn ensure_claude_md_creates_when_missing() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("CLAUDE.md");
        let appended = ensure_claude_md_section(&p, false).unwrap();
        assert!(appended);
        assert!(p.exists());
    }

    #[test]
    fn ensure_claude_md_idempotent() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("CLAUDE.md");
        ensure_claude_md_section(&p, false).unwrap();
        let second = ensure_claude_md_section(&p, false).unwrap();
        assert!(!second);
        // Verify the section appears exactly once.
        let after = fs::read_to_string(&p).unwrap();
        assert_eq!(after.matches(CLAUDE_MD_HEADER).count(), 1);
    }

    #[test]
    fn ensure_agents_md_idempotent() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("AGENTS.md");
        ensure_agents_md_section(&p, false).unwrap();
        let second = ensure_agents_md_section(&p, false).unwrap();
        assert!(!second);
        let after = fs::read_to_string(&p).unwrap();
        assert_eq!(after.matches(AGENTS_MD_HEADER).count(), 1);
    }

    #[test]
    fn ensure_claude_md_dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("CLAUDE.md");
        let appended = ensure_claude_md_section(&p, true).unwrap();
        assert!(appended);
        assert!(!p.exists());
    }

    #[test]
    fn ensure_session_start_hook_creates_settings() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(".claude/settings.json");
        let added = ensure_session_start_hook(&p, false, false).unwrap();
        assert!(added);
        let raw = fs::read_to_string(&p).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        let arr = parsed["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert!(arr[0]["command"].as_str().unwrap().contains(HOOK_MARKER));
    }

    #[test]
    fn ensure_session_start_hook_idempotent() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(".claude/settings.json");
        ensure_session_start_hook(&p, false, false).unwrap();
        let second = ensure_session_start_hook(&p, false, false).unwrap();
        assert!(!second);
        let raw = fs::read_to_string(&p).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        let arr = parsed["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
    }

    #[test]
    fn ensure_session_start_hook_force_replaces() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(".claude/settings.json");
        ensure_session_start_hook(&p, false, false).unwrap();
        let second = ensure_session_start_hook(&p, false, true).unwrap();
        assert!(second);
        let arr = serde_json::from_str::<Value>(&fs::read_to_string(&p).unwrap()).unwrap()
            ["hooks"]["SessionStart"]
            .as_array()
            .cloned()
            .unwrap();
        assert_eq!(arr.len(), 1); // not duplicated
    }

    /// The embedded example registry parses as valid JSON. Caught at build time
    /// via `include_str!`, but verifying serde-shaped is a cheap belt-and-braces.
    #[test]
    fn embedded_example_profiles_parses_as_json() {
        let parsed: serde_json::Value =
            serde_json::from_str(EXAMPLE_PROFILES_JSON).expect("profiles.example.json must parse");
        assert!(parsed.get("profiles").is_some(), "missing 'profiles' field");
    }

    /// `darkmux init` writes `EXAMPLE_PROFILES_JSON` VERBATIM to
    /// `~/.darkmux/profiles.json` — the next `darkmux` invocation that reads
    /// the registry runs it through `load_registry`'s validation pass
    /// (profile-level only since #1269: crew content is no longer validated
    /// at load time). A fresh `darkmux init` must never hand the operator a
    /// registry that fails that pass, AND its bundled `review-deep` crew
    /// must still resolve cleanly via `resolve_crew` — the example is meant
    /// to be a working starting point for `dispatch` /
    /// `review-bench --funnel`, not just load-valid.
    ///
    /// Also drift-guards the example's stamped `schema_version` against
    /// `PROFILES_SCHEMA_VERSION` (the same committed-reference-vs-code
    /// discipline as `example_config_matches_with_defaults`) — a schema bump
    /// that forgets to restamp the example fails here, not on an operator's
    /// machine.
    #[test]
    fn embedded_example_profiles_passes_full_registry_validation() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        fs::write(&p, EXAMPLE_PROFILES_JSON).unwrap();
        let loaded = darkmux_profiles::profiles::load_registry(Some(p.to_str().unwrap()))
            .expect("embedded example registry must pass load_registry's validation");
        assert!(loaded.registry.crews.contains_key("review-deep"));
        darkmux_profiles::crews::resolve_crew(&loaded.registry, "review-deep")
            .expect("embedded example's review-deep crew must resolve cleanly (#1269)");
        assert_eq!(
            loaded.registry.schema_version.as_deref(),
            Some(darkmux_types::PROFILES_SCHEMA_VERSION),
            "profiles.example.json schema_version must match PROFILES_SCHEMA_VERSION"
        );
    }

    #[test]
    fn ensure_session_start_hook_preserves_existing_settings() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(".claude/settings.json");
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(
            &p,
            r#"{"theme":"dark","hooks":{"SessionStart":[{"type":"command","command":"echo prior"}]}}"#,
        )
        .unwrap();
        ensure_session_start_hook(&p, false, false).unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(parsed["theme"], "dark");
        let arr = parsed["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 2); // existing preserved + ours appended
    }
}
