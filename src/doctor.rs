//! `darkmux doctor` — pre-flight diagnostic checks for a darkmux setup.
//!
//! Answers the question every new user has after running `darkmux init`:
//! *"Did I set this up right?"* — without making them run a real lab dispatch
//! and interpret the output.
//!
//! Each check returns a `Check` with one of three statuses:
//!   - **Pass** — green-light: nothing the user needs to do.
//!   - **Warn** — non-blocking but worth knowing (e.g. on battery, RAM tight).
//!   - **Fail** — `darkmux` won't work end-to-end until this is resolved.
//!
//! Process exit codes (consumed by main.rs):
//!   0 — all checks passed (warnings allowed)
//!   1 — at least one check failed
//!
//! Checks are intentionally scoped to what darkmux can verify natively. Some
//! agent-runtime-specific checks (gateway port, openclaw config sanity) belong
//! to the runtime, not to darkmux.

use crate::agent_roles;
use crate::hardware;
use crate::heuristics;
use crate::lms;
use crate::profiles;
use crate::types::ModelRole;
use anyhow::Result;
use std::env;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub message: String,
    pub hint: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
}

impl DoctorReport {
    pub fn worst_status(&self) -> Status {
        let mut worst = Status::Pass;
        for c in &self.checks {
            match (c.status, worst) {
                (Status::Fail, _) => return Status::Fail,
                (Status::Warn, Status::Pass) => worst = Status::Warn,
                _ => {}
            }
        }
        worst
    }

    pub fn pass_count(&self) -> usize {
        self.checks.iter().filter(|c| c.status == Status::Pass).count()
    }
    pub fn warn_count(&self) -> usize {
        self.checks.iter().filter(|c| c.status == Status::Warn).count()
    }
    pub fn fail_count(&self) -> usize {
        self.checks.iter().filter(|c| c.status == Status::Fail).count()
    }
}

pub fn run() -> DoctorReport {
    DoctorReport {
        checks: vec![
            check_profile_registry(),
            check_lms_binary(),
            check_models_loaded(),
            check_profile_loaded_match(),
            check_runtime_command(),
            check_ram_headroom(),
            check_power_state(),
            check_platform_and_provider(),
            check_agent_role_definitions(),
        ],
    }
}

// ─── Individual checks ──────────────────────────────────────────────────

fn check_profile_registry() -> Check {
    match profiles::load_registry(None) {
        Ok(loaded) => {
            let n = loaded.registry.profiles.len();
            Check {
                name: "profile registry".into(),
                status: Status::Pass,
                message: format!("{} profile(s) at {}", n, loaded.path.display()),
                hint: None,
            }
        }
        Err(e) => Check {
            name: "profile registry".into(),
            status: Status::Fail,
            message: e.to_string().lines().next().unwrap_or("load failed").to_string(),
            hint: Some("run `darkmux init` to create one".into()),
        },
    }
}

fn check_lms_binary() -> Check {
    let bin = env::var("DARKMUX_LMS_BIN").unwrap_or_else(|_| "lms".to_string());
    if which(&bin).is_some() {
        Check {
            name: "lms binary".into(),
            status: Status::Pass,
            message: format!("found `{bin}` on PATH"),
            hint: None,
        }
    } else {
        Check {
            name: "lms binary".into(),
            status: Status::Fail,
            message: format!("`{bin}` not found on PATH"),
            hint: Some(
                "install LMStudio (https://lmstudio.ai/) and ensure `lms` is on PATH, \
                 or set DARKMUX_LMS_BIN to override"
                    .into(),
            ),
        }
    }
}

fn check_models_loaded() -> Check {
    match lms::list_loaded() {
        Ok(models) if !models.is_empty() => Check {
            name: "models loaded".into(),
            status: Status::Pass,
            message: format!(
                "{} model(s) loaded: {}",
                models.len(),
                models
                    .iter()
                    .map(|m| m.identifier.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            hint: None,
        },
        Ok(_) => Check {
            name: "models loaded".into(),
            status: Status::Warn,
            message: "no models loaded in LMStudio".into(),
            hint: Some(
                "load a model via the LMStudio GUI or `lms load <id> --context-length <N>`, \
                 or run `darkmux swap <profile>` to load a profile's models automatically"
                    .into(),
            ),
        },
        Err(e) => Check {
            name: "models loaded".into(),
            status: Status::Warn,
            message: format!("could not query lms: {}", first_line(&e.to_string())),
            hint: Some("ensure LMStudio is running and reachable".into()),
        },
    }
}

fn check_profile_loaded_match() -> Check {
    let registry = match profiles::load_registry(None) {
        Ok(r) => r,
        Err(_) => {
            return Check {
                name: "profile match".into(),
                status: Status::Warn,
                message: "no profile registry — can't check match".into(),
                hint: None,
            };
        }
    };
    let loaded = match lms::list_loaded() {
        Ok(l) => l,
        Err(_) => {
            return Check {
                name: "profile match".into(),
                status: Status::Warn,
                message: "could not enumerate loaded models".into(),
                hint: None,
            };
        }
    };

    if loaded.is_empty() {
        return Check {
            name: "profile match".into(),
            status: Status::Warn,
            message: "no models loaded — nothing to match against".into(),
            hint: None,
        };
    }

    let mut matching: Vec<&str> = Vec::new();
    for (name, profile) in &registry.registry.profiles {
        let primaries = profile
            .models
            .iter()
            .filter(|m| matches!(m.role, ModelRole::Primary));
        let primary_match = primaries.clone().any(|p| {
            loaded
                .iter()
                .any(|l| l.identifier == p.id || l.model == p.id)
        });
        if primary_match {
            matching.push(name);
        }
    }

    if matching.is_empty() {
        Check {
            name: "profile match".into(),
            status: Status::Warn,
            message: "loaded models don't match any profile".into(),
            hint: Some(
                "edit ~/.darkmux/profiles.json so a profile's primary model id matches what \
                 LMStudio is serving (compare `darkmux status` and `darkmux profiles`)"
                    .into(),
            ),
        }
    } else {
        Check {
            name: "profile match".into(),
            status: Status::Pass,
            message: format!("loaded state matches profile(s): {}", matching.join(", ")),
            hint: None,
        }
    }
}

fn check_runtime_command() -> Check {
    let cmd = env::var("DARKMUX_RUNTIME_CMD").unwrap_or_else(|_| "openclaw".to_string());
    if which(&cmd).is_some() {
        Check {
            name: "runtime command".into(),
            status: Status::Pass,
            message: format!("found `{cmd}` on PATH (used by `darkmux lab`)"),
            hint: None,
        }
    } else {
        Check {
            name: "runtime command".into(),
            status: Status::Warn,
            message: format!("`{cmd}` not on PATH"),
            hint: Some(
                "install your agent runtime, or set DARKMUX_RUNTIME_CMD to override. \
                 `darkmux swap`/`status`/`profiles` work without a runtime; only `lab` features need it."
                    .into(),
            ),
        }
    }
}

fn check_ram_headroom() -> Check {
    match read_reclaimable_gb() {
        Some(gb) if gb >= 25 => Check {
            name: "RAM headroom".into(),
            status: Status::Pass,
            message: format!("{} GB reclaimable", gb),
            hint: None,
        },
        Some(gb) if gb >= 10 => Check {
            name: "RAM headroom".into(),
            status: Status::Warn,
            message: format!("only {} GB reclaimable", gb),
            hint: Some("close apps before measurement-grade lab runs".into()),
        },
        Some(gb) => Check {
            name: "RAM headroom".into(),
            status: Status::Fail,
            message: format!("{} GB reclaimable — model may swap", gb),
            hint: Some("free memory before running darkmux lab; swap pollutes wall-clock".into()),
        },
        None => Check {
            name: "RAM headroom".into(),
            status: Status::Warn,
            message: "could not read vm_stat (non-macOS?)".into(),
            hint: None,
        },
    }
}

/// Read openclaw.json and flag agents whose names match a shipped role
/// template (qa, scribe, engineer) but don't have a systemPromptOverride.
/// These are the cases where the user *probably* wants to adopt a darkmux
/// scaffold — without one, the agent's behavior is driven by the
/// runtime's default preamble, which may not fit a narrow role.
///
/// The check is deliberately silent when no agents match shipped role
/// names. Custom-named agents (e.g. "my-app-bot") can have any shape;
/// darkmux doesn't have an opinion about those.
fn check_agent_role_definitions() -> Check {
    let openclaw_path = match dirs::home_dir() {
        Some(h) => h.join(".openclaw").join("openclaw.json"),
        None => {
            return Check {
                name: "agent role scaffolds".into(),
                status: Status::Pass,
                message: "(no home directory — skipping)".into(),
                hint: None,
            };
        }
    };
    if !openclaw_path.exists() {
        return Check {
            name: "agent role scaffolds".into(),
            status: Status::Pass,
            message: "(no ~/.openclaw/openclaw.json — skipping)".into(),
            hint: None,
        };
    }
    let raw = match std::fs::read_to_string(&openclaw_path) {
        Ok(r) => r,
        Err(_) => {
            return Check {
                name: "agent role scaffolds".into(),
                status: Status::Warn,
                message: format!("could not read {}", openclaw_path.display()),
                hint: None,
            };
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => {
            return Check {
                name: "agent role scaffolds".into(),
                status: Status::Warn,
                message: "openclaw.json failed to parse — skipping".into(),
                hint: None,
            };
        }
    };

    let known_role_ids: Vec<&str> = agent_roles::list_role_ids();
    let agents = parsed
        .get("agents")
        .and_then(|a| a.get("list"))
        .and_then(|l| l.as_array());
    let Some(agents) = agents else {
        return Check {
            name: "agent role scaffolds".into(),
            status: Status::Pass,
            message: "(no agents.list array in openclaw.json)".into(),
            hint: None,
        };
    };

    let mut missing_overrides: Vec<String> = Vec::new();
    for agent in agents {
        let id = agent.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if !known_role_ids.contains(&id) {
            continue;
        }
        let has_override = agent
            .get("systemPromptOverride")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !has_override {
            missing_overrides.push(id.to_string());
        }
    }

    if missing_overrides.is_empty() {
        Check {
            name: "agent role scaffolds".into(),
            status: Status::Pass,
            message: "no agent role-definition gaps detected".into(),
            hint: None,
        }
    } else {
        let suggestions = missing_overrides
            .iter()
            .map(|id| format!("`darkmux agent template {id}`"))
            .collect::<Vec<_>>()
            .join(", ");
        Check {
            name: "agent role scaffolds".into(),
            status: Status::Warn,
            message: format!(
                "agent(s) {} have no systemPromptOverride — relying on runtime defaults",
                missing_overrides
                    .iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            hint: Some(format!(
                "darkmux ships validated scaffolds for these roles. Try: {suggestions}"
            )),
        }
    }
}

fn check_platform_and_provider() -> Check {
    let hw = hardware::detect();
    let provider = heuristics::active_provider(&hw);
    let summary = hw.one_line_summary();
    // Pass when a non-generic provider claims the hardware (i.e. we have
    // validated rules for it). Warn when only generic matched — heuristics
    // will work but suggestions are unvalidated for this platform.
    if provider.id() == "generic" {
        Check {
            name: "platform / heuristics".into(),
            status: Status::Warn,
            message: format!("{summary} → provider=`generic` (unvalidated)"),
            hint: Some(
                "darkmux ships rules for Apple Silicon at 64GB and 128GB+. Your hardware \
                 doesn't match a validated provider; profile draft suggestions will use \
                 conservative defaults. Consider opening a PR with measured rules for \
                 your platform — see src/heuristics/ for the trait + existing examples."
                    .into(),
            ),
        }
    } else {
        Check {
            name: "platform / heuristics".into(),
            status: Status::Pass,
            message: format!("{summary} → provider=`{}`", provider.id()),
            hint: None,
        }
    }
}

fn check_power_state() -> Check {
    match read_power_source() {
        Some(PowerSource::Ac) => Check {
            name: "power state".into(),
            status: Status::Pass,
            message: "AC power".into(),
            hint: None,
        },
        Some(PowerSource::Battery) => Check {
            name: "power state".into(),
            status: Status::Warn,
            message: "on battery".into(),
            hint: Some(
                "Apple Silicon throttles CPU/GPU/ANE on battery; identical dispatches can \
                 vary 2-4× depending on power state. Plug in for measurement-grade runs."
                    .into(),
            ),
        },
        None => Check {
            name: "power state".into(),
            status: Status::Pass,
            message: "n/a (non-Apple Silicon? skipping)".into(),
            hint: None,
        },
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────

fn which(cmd: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let full = dir.join(cmd);
        if full.is_file() && is_executable(&full) {
            return Some(full);
        }
    }
    None
}

fn is_executable(p: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(p) {
            Ok(md) => md.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        true
    }
}

fn read_reclaimable_gb() -> Option<u64> {
    let out = Command::new("vm_stat").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut free_pages: u64 = 0;
    let mut inactive_pages: u64 = 0;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Pages free:") {
            free_pages = parse_pages_field(rest)?;
        } else if let Some(rest) = line.strip_prefix("Pages inactive:") {
            inactive_pages = parse_pages_field(rest)?;
        }
    }
    // macOS: page size is 16K on Apple Silicon, 4K on Intel. Read it.
    let page_size = read_page_size().unwrap_or(16_384);
    let bytes = (free_pages + inactive_pages).saturating_mul(page_size);
    Some(bytes / (1024 * 1024 * 1024))
}

fn parse_pages_field(s: &str) -> Option<u64> {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    cleaned.parse().ok()
}

fn read_page_size() -> Option<u64> {
    let out = Command::new("pagesize").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PowerSource {
    Ac,
    Battery,
}

fn read_power_source() -> Option<PowerSource> {
    let out = Command::new("pmset").args(["-g", "batt"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if text.contains("AC Power") {
        Some(PowerSource::Ac)
    } else if text.contains("Battery Power") {
        Some(PowerSource::Battery)
    } else {
        None
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

// ─── Result rendering ───────────────────────────────────────────────────

pub fn print_report(r: &DoctorReport) -> Result<()> {
    println!("darkmux doctor — {} checks", r.checks.len());
    println!();
    for c in &r.checks {
        let marker = match c.status {
            Status::Pass => "✓",
            Status::Warn => "⚠",
            Status::Fail => "✗",
        };
        println!("  {} {:<22} {}", marker, c.name, c.message);
        if let Some(hint) = c.hint.as_ref() {
            for line in hint.lines() {
                println!("        → {line}");
            }
        }
    }
    println!();
    let summary = match r.worst_status() {
        Status::Pass => format!(
            "all {} checks passed{}",
            r.pass_count(),
            if r.warn_count() > 0 {
                format!(" ({} warning(s))", r.warn_count())
            } else {
                "".into()
            }
        ),
        Status::Warn => format!(
            "{} pass, {} warn — workable but worth a look",
            r.pass_count(),
            r.warn_count()
        ),
        Status::Fail => format!(
            "{} pass, {} warn, {} fail — fix failures before running darkmux end-to-end",
            r.pass_count(),
            r.warn_count(),
            r.fail_count()
        ),
    };
    println!("{summary}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, status: Status) -> Check {
        Check {
            name: name.into(),
            status,
            message: "x".into(),
            hint: None,
        }
    }

    #[test]
    fn worst_status_promotes_correctly() {
        let r = DoctorReport {
            checks: vec![check("a", Status::Pass), check("b", Status::Pass)],
        };
        assert_eq!(r.worst_status(), Status::Pass);

        let r = DoctorReport {
            checks: vec![check("a", Status::Pass), check("b", Status::Warn)],
        };
        assert_eq!(r.worst_status(), Status::Warn);

        let r = DoctorReport {
            checks: vec![
                check("a", Status::Warn),
                check("b", Status::Fail),
                check("c", Status::Pass),
            ],
        };
        assert_eq!(r.worst_status(), Status::Fail);
    }

    #[test]
    fn counts_match_checks() {
        let r = DoctorReport {
            checks: vec![
                check("a", Status::Pass),
                check("b", Status::Pass),
                check("c", Status::Warn),
                check("d", Status::Fail),
            ],
        };
        assert_eq!(r.pass_count(), 2);
        assert_eq!(r.warn_count(), 1);
        assert_eq!(r.fail_count(), 1);
    }

    #[test]
    fn parse_pages_field_handles_commas_and_dot() {
        // vm_stat lines look like "Pages free:                  1234567."
        assert_eq!(parse_pages_field("                  1234567."), Some(1234567));
        assert_eq!(parse_pages_field(" 1.234.567."), Some(1234567));
        assert_eq!(parse_pages_field("        ."), None);
    }

    #[test]
    fn first_line_works() {
        assert_eq!(first_line("foo\nbar"), "foo");
        assert_eq!(first_line(""), "");
        assert_eq!(first_line("just one"), "just one");
    }

    #[test]
    fn which_finds_real_binary() {
        // sh exists on every unix system we'll be tested on.
        assert!(which("sh").is_some());
    }

    #[test]
    fn which_rejects_garbage() {
        assert!(which("definitely-not-a-real-binary-zzzz").is_none());
    }

    #[test]
    fn run_returns_nine_checks() {
        let r = run();
        // Every check we declare in `run()` should appear regardless of
        // environment — even if the underlying probe couldn't read state.
        assert_eq!(r.checks.len(), 9);
    }

    #[test]
    fn platform_check_always_present() {
        let r = run();
        assert!(r.checks.iter().any(|c| c.name.contains("platform")));
    }

    #[test]
    fn agent_role_check_always_present() {
        let r = run();
        assert!(r.checks.iter().any(|c| c.name.contains("agent role")));
    }
}
