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
use crate::eureka;
use crate::hardware;
use crate::heuristics;
use crate::lms;
use crate::profiles;
use crate::types::ModelRole;
use anyhow::Result;
use std::env;
use std::io::{Read, Write};
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
    let mut checks = vec![
        check_profile_registry(),
        check_lms_binary(),
        check_models_loaded(),
        check_profile_loaded_match(),
        check_runtime_command(),
        check_runtime_version(),
        check_daemon_reachable(),
        check_ram_headroom(),
        check_ram_headroom_load_projection(),
        check_power_state(),
        check_platform_and_provider(),
        check_agent_role_definitions(),
    ];
    checks.extend(eureka_checks());
    DoctorReport { checks }
}

/// Minimum OpenClaw version darkmux has been validated against. Older
/// versions may produce subtle feature regressions on agent config
/// (`systemPromptOverride` in particular — observed during 2026-05-11
/// cross-machine testing on an M1 Max Studio running OpenClaw 2026.3.13).
///
/// CalVer (YYYY, MM, DD). Bump this when:
///   - You introduce a darkmux feature that depends on a newer OpenClaw
///   - You confirm via cross-machine testing that an older OpenClaw breaks
///     a current darkmux feature
const MIN_OPENCLAW_VERSION: (u32, u32, u32) = (2026, 5, 4);

/// Run the eureka rule set and map each verdict to a doctor `Check`.
/// Each rule produces one check row so the user sees which specific
/// patterns matched/didn't match their setup.
fn eureka_checks() -> Vec<Check> {
    let ctx = eureka::Context::collect();
    eureka::evaluate_all(&ctx)
        .into_iter()
        .map(|(def, verdict)| match verdict {
            eureka::Verdict::Pass => Check {
                name: format!("eureka: {}", def.id),
                status: Status::Pass,
                message: def.name.clone(),
                hint: None,
            },
            eureka::Verdict::Fire { severity, message } => Check {
                name: format!("eureka: {}", def.id),
                status: match severity {
                    eureka::Severity::Warn => Status::Warn,
                    eureka::Severity::Fail => Status::Fail,
                },
                message: format!("{}: {message}", def.name),
                hint: Some(def.fix_hint),
            },
            eureka::Verdict::Skipped(reason) => Check {
                name: format!("eureka: {}", def.id),
                status: Status::Pass,
                message: format!("(skipped: {reason})"),
                hint: None,
            },
        })
        .collect()
}

// ─── Individual checks ──────────────────────────────────────────────────

fn check_daemon_reachable() -> Check {
    // Check if the darkmux daemon is reachable at 127.0.0.1:8765/health.
    // Pass when reachable, Warn otherwise (daemon being off doesn't break
    // end-to-end; it just disables live viewing).
    check_daemon_reachable_impl("127.0.0.1", 8765)
}

/// Core implementation that takes host/port so tests can inject mock servers.
fn check_daemon_reachable_impl(host: &str, port: u16) -> Check {
    let addr = format!("{}:{}", host, port);

    // Use a short timeout since this is local loopback.
    let addr_parsed = match addr.parse() {
        Ok(a) => a,
        Err(_) => {
            return Check {
                name: "daemon reachable".into(),
                status: Status::Warn,
                message: format!("invalid address {}", addr),
                hint: None,
            };
        }
    };

    let mut stream = match std::net::TcpStream::connect_timeout(
        &addr_parsed,
        std::time::Duration::from_millis(500),
    ) {
        Ok(s) => s,
        Err(_e) => {
            return Check {
                name: "daemon reachable".into(),
                status: Status::Warn,
                message: format!("daemon not reachable at {} (connection refused)", addr),
                hint: Some(
                    "run `darkmux serve` to start the daemon for live viewing features"
                        .into(),
                ),
            };
        }
    };

    // Set read/write timeouts for the HTTP exchange. If the OS won't
    // honor them (rare on macOS/Linux but possible on stripped builds
    // or unusual sockets), bail with Warn rather than risk a hang in
    // the subsequent stream.read() — this is the surface area #104
    // review flagged ("silent error on stream timeout configuration").
    let to = std::time::Duration::from_millis(1000);
    if stream.set_read_timeout(Some(to)).is_err()
        || stream.set_write_timeout(Some(to)).is_err()
    {
        return Check {
            name: "daemon reachable".into(),
            status: Status::Warn,
            message: format!(
                "daemon at {} answered TCP but the probe couldn't set socket timeouts — skipping read to avoid hang",
                addr
            ),
            hint: Some(
                "system may not support socket timeouts on this socket type; probe will work after daemon restart or OS update"
                    .into(),
            ),
        };
    }

    // Send minimal HTTP/1.1 request.
    let request = format!(
        "GET /health HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        addr
    );
    stream.write_all(request.as_bytes()).ok();
    stream.flush().ok(); // Ensure the request is sent

    // Read response.
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).ok().unwrap_or(0);

    let response = String::from_utf8_lossy(&buf[..n]);
    if n == 0 {
        return Check {
            name: "daemon reachable".into(),
            status: Status::Warn,
            message: format!("daemon at {} not responding to HTTP", addr),
            hint: Some(
                "run `darkmux serve` to start the daemon for live viewing features"
                    .into(),
            ),
        };
    }

    if response.starts_with("HTTP/1.1 200") {
        Check {
            name: "daemon reachable".into(),
            status: Status::Pass,
            message: format!("daemon reachable at {} (health check OK)", addr),
            hint: None,
        }
    } else {
        // Port is open but not darkmux (or wrong endpoint).
        let first_line = response.lines().next().unwrap_or("");
        Check {
            name: "daemon reachable".into(),
            status: Status::Warn,
            message: format!(
                "daemon not responding correctly at {}: {}",
                addr,
                first_line
            ),
            hint: Some(
                "ensure `darkmux serve` is running (port 8765 may be held by another process)"
                    .into(),
            ),
        }
    }
}

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

/// Parse OpenClaw's `--version` output to a CalVer tuple. Accepts forms
/// like `OpenClaw 2026.5.4 (325df3e)`; the leading word and trailing
/// commit hash are tolerated. Returns `None` if the YYYY.MM.DD segment
/// can't be located.
fn parse_openclaw_version(raw: &str) -> Option<(u32, u32, u32)> {
    // Scan tokens for the first one that splits into three numeric parts.
    for token in raw.split_whitespace() {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            continue;
        }
        let ymd: Result<Vec<u32>, _> = parts.iter().map(|p| p.parse::<u32>()).collect();
        if let Ok(v) = ymd {
            return Some((v[0], v[1], v[2]));
        }
    }
    None
}

fn check_runtime_version() -> Check {
    let cmd = env::var("DARKMUX_RUNTIME_CMD").unwrap_or_else(|_| "openclaw".to_string());
    // Only check version for openclaw — other runtimes have their own
    // version conventions. When DARKMUX_RUNTIME_CMD is set to something
    // else (aider, cline), skip rather than guess at parsing.
    if cmd != "openclaw" {
        return Check {
            name: "runtime version".into(),
            status: Status::Pass,
            message: format!("(skipped — runtime is `{cmd}`, version-check is openclaw-specific)"),
            hint: None,
        };
    }

    let output = Command::new(&cmd).arg("--version").output();
    let raw = match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).to_string()
                + &String::from_utf8_lossy(&o.stderr)
        }
        _ => {
            return Check {
                name: "runtime version".into(),
                status: Status::Warn,
                message: "could not run `openclaw --version`".into(),
                hint: Some("ensure openclaw is on PATH and executable".into()),
            };
        }
    };

    let parsed = parse_openclaw_version(&raw);
    let (y, m, d) = match parsed {
        Some(v) => v,
        None => {
            return Check {
                name: "runtime version".into(),
                status: Status::Warn,
                message: format!("could not parse version from: {}", first_line(&raw)),
                hint: None,
            };
        }
    };

    let (min_y, min_m, min_d) = MIN_OPENCLAW_VERSION;
    let installed = format!("{y}.{m}.{d}");
    let minimum = format!("{min_y}.{min_m}.{min_d}");

    if (y, m, d) >= (min_y, min_m, min_d) {
        Check {
            name: "runtime version".into(),
            status: Status::Pass,
            message: format!("OpenClaw {installed} (>= validated minimum {minimum})"),
            hint: None,
        }
    } else {
        Check {
            name: "runtime version".into(),
            status: Status::Warn,
            message: format!(
                "OpenClaw {installed} is older than darkmux's validated minimum ({minimum}). \
                 `systemPromptOverride` and some compaction config may regress silently."
            ),
            hint: Some(
                "upgrade OpenClaw to >= 2026.5.4. From your openclaw source checkout: \
                 `git pull && <build/install steps per openclaw's own README>`. \
                 If a feature appears not to work after this doctor warning, surface the \
                 version mismatch loudly — don't silently roll back configs (see CLAUDE.md anti-patterns)."
                    .into(),
            ),
        }
    }
}

/// Default headroom we reserve outside the AI working set — covers macOS
/// itself, Finder, lightweight background processes. Empirical: 1–2 GB is
/// the right shape on Apple Silicon idle.
const RAM_SAFETY_MARGIN_GB: u64 = 2;
const RAM_PASS_THRESHOLD_GB: u64 = 25;
const RAM_WARN_THRESHOLD_GB: u64 = 10;

fn check_ram_headroom() -> Check {
    let reclaimable_gb = match read_reclaimable_gb() {
        Some(g) => g,
        None => {
            return Check {
                name: "RAM headroom".into(),
                status: Status::Warn,
                message: "could not read vm_stat (non-macOS?)".into(),
                hint: None,
            };
        }
    };

    // What's already mapped to AI counts toward the real budget — it's
    // memory the operator has *already chosen* to spend on AI, not a
    // contention pressure to subtract. See issue #67.
    let loaded_models_size_gb = lms::list_loaded()
        .map(|models| {
            models
                .iter()
                .filter_map(|m| eureka::parse_size_gb(&m.size))
                .sum::<f64>()
        })
        .unwrap_or(0.0);

    classify_ram_headroom(
        reclaimable_gb,
        loaded_models_size_gb,
        RAM_SAFETY_MARGIN_GB,
    )
}

/// Pure verdict logic for the RAM headroom check. Extracted so the
/// formula can be unit-tested without an `lms` / `vm_stat` round-trip.
///
/// `real_headroom = reclaimable + resident − safety_margin` — the budget
/// available to the operator for AI work, including memory already
/// committed to a loaded model.
fn classify_ram_headroom(
    reclaimable_gb: u64,
    loaded_models_size_gb: f64,
    safety_margin_gb: u64,
) -> Check {
    let real_headroom_f = (reclaimable_gb as f64)
        + loaded_models_size_gb
        - (safety_margin_gb as f64);
    let real_headroom_gb = real_headroom_f.max(0.0).round() as u64;
    let resident_round = loaded_models_size_gb.round() as u64;

    let breakdown = if loaded_models_size_gb >= 0.5 {
        format!(
            "{real_headroom_gb} GB available for AI ({reclaimable_gb} GB reclaimable + ~{resident_round} GB resident − {safety_margin_gb} GB safety)"
        )
    } else {
        format!(
            "{real_headroom_gb} GB available for AI ({reclaimable_gb} GB reclaimable − {safety_margin_gb} GB safety, no model resident)"
        )
    };

    if real_headroom_gb >= RAM_PASS_THRESHOLD_GB {
        Check {
            name: "RAM headroom".into(),
            status: Status::Pass,
            message: breakdown,
            hint: None,
        }
    } else if real_headroom_gb >= RAM_WARN_THRESHOLD_GB {
        Check {
            name: "RAM headroom".into(),
            status: Status::Warn,
            message: breakdown,
            hint: Some(
                "close apps or shrink ctx before measurement-grade lab runs"
                    .into(),
            ),
        }
    } else {
        Check {
            name: "RAM headroom".into(),
            status: Status::Fail,
            message: format!("{breakdown} — model may swap"),
            hint: Some(
                "free memory or unload models before running darkmux lab; \
                 swap pollutes wall-clock"
                    .into(),
            ),
        }
    }
}

/// Predictive sibling to `check_ram_headroom`: answers *"will loading the
/// rest of the active profile fit, or will it swap?"* Skips quietly when
/// there's nothing meaningful to predict (no profile, no match, profile
/// already fully resident). See issue #70 thread A for the operator-facing
/// motivation — pre-#68 doctor under-reported drift after a swap-load
/// sequence; post-#68 we can call it out before the operator hits it.
fn check_ram_headroom_load_projection() -> Check {
    const NAME: &str = "RAM headroom (load projection)";
    let skip = |reason: &str| Check {
        name: NAME.into(),
        status: Status::Pass,
        message: format!("(skipped: {reason})"),
        hint: None,
    };

    let registry = match profiles::load_registry(None) {
        Ok(r) => r,
        Err(_) => return skip("no profile registry"),
    };
    let loaded = match lms::list_loaded() {
        Ok(l) => l,
        Err(_) => return skip("could not query lms"),
    };
    if loaded.is_empty() {
        return skip("no models loaded — nothing to project against");
    }

    let Some((profile_name, profile)) = pick_active_profile(&registry, &loaded) else {
        return skip("no profile matches loaded state");
    };

    let unloaded: Vec<&crate::types::ProfileModel> = profile
        .models
        .iter()
        .filter(|pm| {
            let ns = crate::swap::namespaced_identifier(pm);
            !loaded.iter().any(|l| {
                l.identifier == pm.id || l.model == pm.id || l.identifier == ns
            })
        })
        .collect();
    if unloaded.is_empty() {
        return Check {
            name: NAME.into(),
            status: Status::Pass,
            message: format!("active profile `{profile_name}` fully resident"),
            hint: None,
        };
    }

    // Catalog lookup for the unloaded models' on-disk sizes. Best-effort:
    // we don't error if the catalog query fails — the projection just
    // reports "size unknown" for those entries and the operator sees the
    // partial picture rather than a missing check.
    let catalog = lms::list_available().unwrap_or_default();
    let mut total_unloaded_gb = 0.0_f64;
    let mut pending: Vec<String> = Vec::new();
    for pm in &unloaded {
        let size_gb = catalog
            .iter()
            .find(|m| m.model_key == pm.id)
            .map(|m| m.size_bytes as f64 / 1_000_000_000.0)
            .unwrap_or(0.0);
        total_unloaded_gb += size_gb;
        if size_gb > 0.0 {
            pending.push(format!("{} ~{:.1} GB", pm.id, size_gb));
        } else {
            pending.push(format!("{} (size unknown)", pm.id));
        }
    }

    let reclaimable_gb = match read_reclaimable_gb() {
        Some(g) => g as f64,
        None => return skip("could not read vm_stat (non-macOS?)"),
    };

    classify_load_projection(
        reclaimable_gb,
        total_unloaded_gb,
        &pending,
        profile_name,
    )
}

/// Pure verdict logic for the load-projection check. Extracted so the
/// formula can be unit-tested without `lms` / `vm_stat` / registry I/O.
///
/// Compares `reclaimable_gb` against `total_unloaded_gb + safety_margin`:
/// - Fail when reclaimable < unloaded total (load *will* swap or OOM)
/// - Warn when reclaimable - unloaded total < safety margin (load fits
///   but leaves no breathing room for KV growth)
/// - Pass otherwise
fn classify_load_projection(
    reclaimable_gb: f64,
    total_unloaded_gb: f64,
    pending: &[String],
    profile_name: &str,
) -> Check {
    const NAME: &str = "RAM headroom (load projection)";
    let safety = RAM_SAFETY_MARGIN_GB as f64;
    let post_load_reclaimable = reclaimable_gb - total_unloaded_gb;
    let summary = format!(
        "loading rest of profile `{profile_name}` would consume ~{:.1} GB \
         ({}); leaves ~{:.1} GB reclaimable",
        total_unloaded_gb,
        pending.join(", "),
        post_load_reclaimable.max(0.0)
    );

    if post_load_reclaimable < 0.0 {
        Check {
            name: NAME.into(),
            status: Status::Fail,
            message: format!("{summary} — load would swap or OOM"),
            hint: Some(
                "active profile demands more memory than is currently free; \
                 close apps, unload other models, or pick a profile with \
                 a smaller compactor / lower n_ctx"
                    .into(),
            ),
        }
    } else if post_load_reclaimable < safety {
        Check {
            name: NAME.into(),
            status: Status::Warn,
            message: format!(
                "{summary} — within {RAM_SAFETY_MARGIN_GB} GB safety margin"
            ),
            hint: Some(
                "load will likely succeed but leaves little headroom for KV \
                 cache growth; watch for swap during long-context dispatches"
                    .into(),
            ),
        }
    } else {
        Check {
            name: NAME.into(),
            status: Status::Pass,
            message: summary,
            hint: None,
        }
    }
}

/// Pick the active profile from a registry given currently-loaded models.
/// Prefers the registry's `default_profile` when it matches; otherwise the
/// first profile whose primary model is loaded. Mirrors the matching shape
/// in `check_profile_loaded_match` so the two checks agree on what
/// "active" means.
fn pick_active_profile<'a>(
    registry: &'a crate::profiles::LoadedRegistry,
    loaded: &[crate::types::LoadedModel],
) -> Option<(&'a str, &'a crate::types::Profile)> {
    let matches: Vec<(&str, &crate::types::Profile)> = registry
        .registry
        .profiles
        .iter()
        .filter(|(_, p)| {
            p.models
                .iter()
                .filter(|m| matches!(m.role, ModelRole::Primary))
                .any(|pm| {
                    let ns = crate::swap::namespaced_identifier(pm);
                    loaded.iter().any(|l| {
                        l.identifier == pm.id || l.model == pm.id || l.identifier == ns
                    })
                })
        })
        .map(|(name, p)| (name.as_str(), p))
        .collect();
    if matches.is_empty() {
        return None;
    }
    if let Some(default) = registry.registry.default_profile.as_deref() {
        if let Some(m) = matches.iter().find(|(n, _)| *n == default) {
            return Some(*m);
        }
    }
    Some(matches[0])
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

// ─── --fix path: auto-apply known-safe fixes ────────────────────────────

/// Outcome of attempting an auto-fix for one rule. `applied=false` with a
/// message starting "skipped:" means the handler reached a known-safe
/// no-op (e.g., no openclaw config path resolvable); a non-skip
/// `applied=false` means the handler ran but found nothing to change.
#[derive(Debug, Clone)]
pub struct FixOutcome {
    pub rule_id: String,
    pub applied: bool,
    pub message: String,
}

/// Attempt to auto-apply known-safe fixes for failing checks in a doctor
/// report. Only rules with a registered handler are touched; everything
/// else is left for the operator. Each handler is responsible for its own
/// idempotency — running `--fix` when nothing's broken is a no-op.
///
/// Returns the list of outcomes (one per fix attempt). Empty when no
/// failing check had a registered handler.
pub fn try_fix(report: &DoctorReport) -> Result<Vec<FixOutcome>> {
    let mut outcomes = Vec::new();
    for check in &report.checks {
        if !matches!(check.status, Status::Fail | Status::Warn) {
            continue;
        }
        // Eureka check names follow the convention `eureka: <rule-id>` —
        // strip the prefix and dispatch by rule-id constant from the rules
        // engine so a typo or rename can't silently break the handler.
        if let Some(rule_id) = check.name.strip_prefix("eureka: ") {
            if rule_id == eureka::RULE_ID_CTX_WINDOW_MISMATCH {
                outcomes.push(fix_ctx_window_mismatch()?);
            }
            // Future fix handlers slot in here; keep additions narrow and
            // documented in the issue tracker so operators can audit what
            // `--fix` will and won't touch.
        }
    }
    Ok(outcomes)
}

fn fix_ctx_window_mismatch() -> Result<FixOutcome> {
    let Some(path) = crate::runtime::resolve_openclaw_config_path(None) else {
        return Ok(FixOutcome {
            rule_id: "ctx-window-mismatch".into(),
            applied: false,
            message: "skipped: no openclaw config path resolvable (set \
                      DARKMUX_OPENCLAW_CONFIG or use a profile with \
                      runtime.config_path)"
                .into(),
        });
    };
    let loaded = match lms::list_loaded() {
        Ok(l) => l,
        Err(e) => {
            return Ok(FixOutcome {
                rule_id: "ctx-window-mismatch".into(),
                applied: false,
                message: format!(
                    "skipped: could not query lms ps — {}",
                    first_line(&e.to_string())
                ),
            });
        }
    };
    let changes = crate::runtime::fix_ctx_window_to_loaded(&path, &loaded)?;
    if changes.is_empty() {
        Ok(FixOutcome {
            rule_id: "ctx-window-mismatch".into(),
            applied: false,
            message: "no contextWindow entries needed adjustment".into(),
        })
    } else {
        let summary = changes
            .iter()
            .map(|c| format!("{}: {} → {}", c.model_id, c.from, c.to))
            .collect::<Vec<_>>()
            .join("; ");
        Ok(FixOutcome {
            rule_id: "ctx-window-mismatch".into(),
            applied: true,
            message: format!(
                "aligned {} contextWindow entry/entries — {summary}",
                changes.len()
            ),
        })
    }
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

    // ─── classify_ram_headroom ─────────────────────────────────────────
    // Verdicts must follow `real_headroom = reclaimable + resident − safety`,
    // not raw reclaimable. Calibrated against the issue #67 table.

    #[test]
    fn ram_headroom_pass_when_real_budget_at_or_above_pass_threshold() {
        // 64 GB tier, 12 GB model resident, 25 GB reclaimable, 2 safety
        //   → 25 + 12 − 2 = 35 GB → Pass
        let c = classify_ram_headroom(25, 12.0, 2);
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("35 GB available"));
        assert!(c.message.contains("resident"));
    }

    #[test]
    fn ram_headroom_warn_on_32gb_tier_with_20b_resident() {
        // Issue #67 regression case: 32 GB Apple Silicon, gpt-oss-20b (12 GB)
        // loaded, 7 GB reclaimable, 2 safety → 7 + 12 − 2 = 17 GB → Warn
        // (was Fail under the old absolute-reclaimable formula).
        let c = classify_ram_headroom(7, 12.0, 2);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("17 GB available"));
        assert!(c.message.contains("12 GB resident"));
    }

    #[test]
    fn ram_headroom_fail_when_real_budget_below_warn_threshold() {
        // 32 GB tier, no model loaded, 8 GB reclaimable, 2 safety
        //   → 8 − 2 = 6 GB → Fail
        let c = classify_ram_headroom(8, 0.0, 2);
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("may swap"));
        assert!(c.message.contains("no model resident"));
    }

    #[test]
    fn ram_headroom_no_negative_real_budget() {
        // Pathological: safety margin exceeds available memory. Real budget
        // floors at 0 rather than wrapping/panicking.
        let c = classify_ram_headroom(0, 0.0, 2);
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("0 GB available"));
    }

    #[test]
    fn ram_headroom_treats_already_loaded_model_as_part_of_budget() {
        // Same reclaimable, different residency: the resident-aware verdict
        // should be *more permissive* than a model-blind one. Demonstrates
        // the asymmetry that #67 fixes.
        let with_model = classify_ram_headroom(7, 12.0, 2);
        let no_model = classify_ram_headroom(7, 0.0, 2);
        // 7 + 12 − 2 = 17 (Warn) vs 7 − 2 = 5 (Fail)
        assert_eq!(with_model.status, Status::Warn);
        assert_eq!(no_model.status, Status::Fail);
    }

    // ─── classify_load_projection (issue #70 thread A) ─────────────────────

    fn pending(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn load_projection_pass_when_reclaimable_covers_unloaded_plus_safety() {
        // 32 GB tier, 8 GB free, 3 GB compactor pending. 8 − 3 = 5 GB
        // remaining, > 2 GB safety → Pass.
        let c = classify_load_projection(
            8.0,
            3.0,
            &pending(&["google/gemma-3-4b ~3.0 GB"]),
            "balanced",
        );
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("`balanced`"));
        assert!(c.message.contains("google/gemma-3-4b ~3.0 GB"));
    }

    #[test]
    fn load_projection_warn_when_load_eats_into_safety_margin() {
        // 8 GB free, 7 GB pending. 8 − 7 = 1 GB < 2 GB safety → Warn (load
        // fits but leaves no headroom for KV cache growth mid-dispatch).
        let c = classify_load_projection(
            8.0,
            7.0,
            &pending(&["big/model ~7.0 GB"]),
            "deep",
        );
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("safety margin"));
    }

    #[test]
    fn load_projection_fail_when_load_exceeds_reclaimable() {
        // 4 GB free, 8 GB compactor pending. Can't fit; would swap or OOM.
        let c = classify_load_projection(
            4.0,
            8.0,
            &pending(&["compactor ~8.0 GB"]),
            "balanced",
        );
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("swap or OOM"));
        // Surfaces the actionable fix (close apps / smaller compactor /
        // lower n_ctx) so the operator can recover without consulting the
        // issue tracker.
        assert!(c.hint.as_deref().unwrap_or("").contains("smaller compactor"));
    }

    #[test]
    fn load_projection_includes_unknown_size_models_in_summary() {
        // A profile model that doesn't appear in the lms catalog (yet)
        // shouldn't poison the verdict — but its presence should still
        // surface in the summary so the operator knows it'll load too.
        let c = classify_load_projection(
            10.0,
            3.0,
            &pending(&[
                "google/gemma-3-4b ~3.0 GB",
                "fresh-download (size unknown)",
            ]),
            "balanced",
        );
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("size unknown"));
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
    fn run_returns_static_plus_eureka_checks() {
        let r = run();
        // 12 baseline checks (incl. runtime version + load projection + daemon reachable) +
        // one per active eureka rule. Every check should appear regardless
        // of environment — even if the underlying probe couldn't read state.
        let expected = 12 + crate::eureka::all_rules().len();
        assert_eq!(r.checks.len(), expected);
    }

    #[test]
    fn parse_openclaw_version_handles_canonical_form() {
        assert_eq!(
            parse_openclaw_version("OpenClaw 2026.5.4 (325df3e)"),
            Some((2026, 5, 4))
        );
        assert_eq!(
            parse_openclaw_version("OpenClaw 2026.3.13 (61d171a)"),
            Some((2026, 3, 13))
        );
    }

    #[test]
    fn parse_openclaw_version_handles_bare_version() {
        // Edge case: just the version string with no leading word
        assert_eq!(parse_openclaw_version("2026.5.4"), Some((2026, 5, 4)));
    }

    #[test]
    fn parse_openclaw_version_rejects_garbage() {
        assert_eq!(parse_openclaw_version("not a version"), None);
        assert_eq!(parse_openclaw_version(""), None);
        // Two-segment versions are not CalVer YYYY.MM.DD shaped
        assert_eq!(parse_openclaw_version("OpenClaw 2026.5"), None);
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

    // ─── check_daemon_reachable tests ──────────────────────────────────────

    #[test]
    fn daemon_reachable_check_passes_when_health_returns_200() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;
        use std::time::Duration;

        // Start a simple blocking TCP server that returns HTTP 200 on /health
        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind test server");
        let port = listener.local_addr().unwrap().port();

        let server_handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Read the request (we don't really need to parse it)
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);

                // Send HTTP 200 response
                let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
                let _ = stream.write_all(response.as_bytes());
            }
        });

        // Give the server a moment to start
        thread::sleep(Duration::from_millis(50));

        // Run the check against our mock server
        let check = check_daemon_reachable_impl("127.0.0.1", port);

        // Assert Pass status
        assert_eq!(check.status, Status::Pass, "daemon reachable check should pass when health returns 200. Got message: {}", check.message);
        assert!(check.message.contains("health check OK"));

        // Shutdown the server by dropping the listener (via a separate scope)
        drop(server_handle);
    }

    #[test]
    fn daemon_reachable_check_warns_when_unreachable() {
        // Point at a high ephemeral port where nothing will be listening
        let check = check_daemon_reachable_impl("127.0.0.1", 59999);

        // Assert Warn status with appropriate message
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("connection refused"));
        assert!(check.hint.as_ref().unwrap_or(&String::new()).contains("darkmux serve"));
    }
}
