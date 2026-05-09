//! `darkmux init` — one-command integration setup.
//!
//! Installs skills + (optional) SessionStart hook + (optional) CLAUDE.md
//! merge so Claude Code knows about darkmux without further configuration.

use crate::skills;
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct InitOptions {
    pub with_hook: bool,
    pub with_claude_md: Option<PathBuf>,
    pub force: bool,
    pub dry_run: bool,
}

#[derive(Debug, Default)]
pub struct InitReport {
    pub profile_registry_path: Option<PathBuf>,
    pub profile_registry_created: bool,
    pub profile_registry_already_present: bool,
    pub skills_target: PathBuf,
    pub skills_installed: Vec<String>,
    pub skills_overwritten: Vec<String>,
    pub skills_skipped: Vec<String>,
    pub hook_added: Option<PathBuf>,
    pub hook_already_present: bool,
    pub claude_md_path: Option<PathBuf>,
    pub claude_md_appended: bool,
    pub claude_md_already_present: bool,
}

/// The example profile registry, embedded at compile time. Copied to
/// `~/.darkmux/profiles.json` on `darkmux init` if no registry exists yet.
const EXAMPLE_PROFILES_JSON: &str = include_str!("../profiles.example.json");

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

    // 2) Skills install
    let skills_report = skills::install_skills(&skills::InstallOptions {
        target: None,
        force: opts.force,
        dry_run: opts.dry_run,
    })?;
    report.skills_target = skills_report.target;
    report.skills_installed = skills_report.installed;
    report.skills_overwritten = skills_report.overwritten;
    report.skills_skipped = skills_report.skipped;

    // 3) SessionStart hook (optional)
    if opts.with_hook {
        let settings = claude_settings_path()?;
        let result = ensure_session_start_hook(&settings, opts.dry_run, opts.force)?;
        report.hook_added = Some(settings);
        report.hook_already_present = !result;
    }

    // 4) CLAUDE.md merge (optional)
    if let Some(target) = opts.with_claude_md.as_ref() {
        let appended = ensure_claude_md_section(target, opts.dry_run)?;
        report.claude_md_path = Some(target.clone());
        report.claude_md_appended = appended;
        report.claude_md_already_present = !appended;
    }

    Ok(report)
}

fn user_profile_registry_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    Ok(home.join(".darkmux").join("profiles.json"))
}

const HOOK_MARKER: &str = "darkmux:session-start";
const CLAUDE_MD_HEADER: &str = "<!-- darkmux:integration:start -->";
const CLAUDE_MD_FOOTER: &str = "<!-- darkmux:integration:end -->";

fn claude_settings_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    Ok(home.join(".claude").join("settings.json"))
}

/// Add a SessionStart hook that runs `darkmux status` so Claude sees the
/// current stack at the start of every session. Returns true if the hook
/// was newly added (false if already present).
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
        "command": format!("# {HOOK_MARKER}\n/usr/bin/env -S sh -c 'darkmux status 2>/dev/null || true'")
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
        r#"{header}

# darkmux

This project uses [darkmux](https://github.com/kstrat2001/darkmux) to multiplex local LLM stacks. Three reference profiles are available: `fast`, `balanced`, and `deep`.

## When to swap stacks

- **`fast`** — single-turn tasks (audits, TODO fills, short Q&A). Slim primary, no compactor.
- **`balanced`** — mid-range tasks. Tuned compaction with a small companion compactor.
- **`deep`** — long agentic tasks (multi-file refactors, exploratory test authoring). Maximum primary context for fewer compactions.

## Available skills

- `/darkmux-status` — what's currently loaded
- `/darkmux-list-stacks` — see all available profiles
- `/darkmux-swap-stack <name>` — switch to a profile
- `/darkmux-list-workloads` / `/darkmux-lab-run` — execute lab workloads
- `/darkmux-list-runs` / `/darkmux-analyze-run` / `/darkmux-compare-runs` — inspect run history

## Dispatch policy

Before starting a long agentic task that may grow context past ~30K tokens, consider swapping to `deep`. Before doing a single-turn audit or short review, consider swapping to `fast` to skip the compactor's idle KV-cache cost. Use `/darkmux-status` to confirm before making the change — swapping is idempotent so a status-matched call is a no-op.

{footer}
"#,
        header = CLAUDE_MD_HEADER,
        footer = CLAUDE_MD_FOOTER,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn claude_md_section_includes_marker_comments() {
        let s = darkmux_claude_md_section();
        assert!(s.contains(CLAUDE_MD_HEADER));
        assert!(s.contains(CLAUDE_MD_FOOTER));
        assert!(s.contains("/darkmux-status"));
        assert!(s.contains("fast"));
        assert!(s.contains("balanced"));
        assert!(s.contains("deep"));
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
