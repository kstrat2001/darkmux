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

use anyhow::Result;
use darkmux_agent_roles as agent_roles;
use darkmux_eureka as eureka;
use darkmux_hardware as hardware;
use darkmux_heuristics as heuristics;
use darkmux_profiles::lms;
use darkmux_profiles::profiles;
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

    pub(crate) fn pass_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == Status::Pass)
            .count()
    }
    pub(crate) fn warn_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == Status::Warn)
            .count()
    }
    pub(crate) fn fail_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .count()
    }
}

/// (#1129) Identity line — WHICH build is running + the flow-schema version it
/// renders. `build_version()` carries the git short SHA (the package version
/// alone doesn't change between releases, so it can't tell an operator whether
/// a daemon has their latest code). Always Pass — informational, leads the
/// report so the answer to "which version is this?" is the first thing shown.
/// (#1129/#1130) Name of the build identity check — the one Pass row that
/// always prints (it answers "which version is this?", not a health question),
/// so it bypasses the issues-only consolidation in `print_report`.
const BUILD_CHECK_NAME: &str = "build";

fn check_build_info() -> Check {
    Check {
        name: BUILD_CHECK_NAME.into(),
        status: Status::Pass,
        message: format!(
            "darkmux {} · flow schema {}",
            darkmux_types::build_version(),
            darkmux_flow::FLOW_SCHEMA_VERSION,
        ),
        hint: None,
    }
}

pub fn run(include_openclaw: bool) -> DoctorReport {
    let mut checks = vec![
        check_build_info(),
        check_profile_registry(),
        check_lms_binary(),
        check_docker_runtime(),
        check_models_loaded(),
        check_profile_loaded_match(),
        check_darkmux_version_vs_latest_release(),
        check_daemon_reachable(),
        check_ram_headroom(),
        check_ram_headroom_load_projection(),
        check_power_state(),
        check_platform_and_provider(),
        check_crew_role_prompt_coverage(),
        check_flow_sink_health(),
        check_machine_id_resolution(),
        check_orchestrator_declared(),
        check_fleet_mode(),
        check_openai_base_url_conflict(),
        check_redis_config(),
        check_audit_integrity(),
        check_audit_write_drops(),
        check_daemon_auth(),
        check_recommendation_drift(),
        check_recommended_profile_name_not_shadowed(),
        check_utility_model_binding(),
        check_role_tool_vocab_typos(),
        check_beat33_legacy_crew_dir(),
        check_legacy_mission_layout(),
        check_legacy_compaction_extras(),
    ];
    if include_openclaw {
        checks.push(check_role_model_pin_drift());
        checks.push(check_runtime_command());
        checks.push(check_runtime_version());
        checks.push(check_agent_role_definitions());
    }
    checks.extend(eureka_checks(include_openclaw));
    DoctorReport { checks }
}

/// Surface profiles whose `runtime.compaction.extras` map still carries
/// legacy openclaw-shape passthrough keys that darkmux no longer consumes.
/// The internal runtime now reads typed fields (`custom_instructions`,
/// `threshold_ratio`, etc.) — legacy extras keys are silently ignored.
///
/// This is a Warn (not Fail) because darkmux's loader preserves
/// back-compat parsing of the `extras` map (`serde_json::Map<String,
/// Value>` via `#[serde(flatten)]`); the check only reads, never
/// mutates. Operators who also use `~/.openclaw/openclaw.json` may still
/// need those keys there — darkmux's default output stays neutral and
/// internal-runtime-only. (#380)
fn check_legacy_compaction_extras() -> Check {
    let registry = match profiles::load_registry(None) {
        Ok(r) => r,
        Err(e) => {
            return Check {
                name: "legacy compaction extras".into(),
                status: Status::Warn,
                message: format!(
                    "can't check compaction extras (profile registry load failed: {e})"
                ),
                hint: None,
            };
        }
    };

    let legacy_keys: std::collections::HashSet<&str> = [
        "mode",
        "maxHistoryShare",
        "recentTurnsPreserve",
        "customInstructions",
    ]
    .into_iter()
    .collect();

    let mut offending_profiles: Vec<(String, Vec<String>)> = Vec::new();

    for (name, profile) in &registry.registry.profiles {
        let extras = profile
            .runtime
            .as_ref()
            .and_then(|r| r.compaction.as_ref())
            .map(|c| &c.extras);

        if let Some(extras) = extras {
            let found: Vec<String> = legacy_keys
                .iter()
                .filter(|k| extras.contains_key(**k))
                .map(|s| s.to_string())
                .collect();

            if !found.is_empty() {
                offending_profiles.push((name.clone(), found));
            }
        }
    }

    if offending_profiles.is_empty() {
        Check {
            name: "legacy compaction extras".into(),
            status: Status::Pass,
            message: "no legacy compaction extras found".into(),
            hint: None,
        }
    } else {
        let details = offending_profiles
            .iter()
            .map(|(name, keys)| {
                let key_list = keys.join(", ");
                format!(
                    "profile `{name}` has fields not consumed by the internal runtime: {key_list}"
                )
            })
            .collect::<Vec<_>>()
            .join("; ");

        // Tailored hint: name the typed migration target where one
        // exists (customInstructions → custom_instructions, from
        // PR #384); name "remove" for the three keys with no typed
        // replacement (mode / maxHistoryShare / recentTurnsPreserve —
        // darkmux's typed schema deliberately doesn't expose these;
        // see DESIGN.md "Schema isolation: each runtime owns its own
        // config"). Operators who hit the warning ONLY because of
        // one of the three see "remove" not "migrate", which is the
        // accurate guidance.
        let any_has_custom = offending_profiles
            .iter()
            .any(|(_, keys)| keys.iter().any(|k| k == "customInstructions"));
        let any_has_other = offending_profiles
            .iter()
            .any(|(_, keys)| keys.iter().any(|k| k != "customInstructions"));
        let hint = match (any_has_custom, any_has_other) {
            (true, true) => "Migrate `customInstructions` to typed `custom_instructions` field; remove `mode` / `maxHistoryShare` / `recentTurnsPreserve` (darkmux's typed schema doesn't expose these — see DESIGN.md Schema isolation).".to_string(),
            (true, false) => "Migrate `customInstructions` to typed `custom_instructions` field (see PR #384).".to_string(),
            (false, true) => "Remove `mode` / `maxHistoryShare` / `recentTurnsPreserve` from profile (darkmux's typed schema deliberately doesn't expose these — see DESIGN.md Schema isolation).".to_string(),
            (false, false) => unreachable!("offending_profiles is non-empty by the outer if"),
        };

        Check {
            name: "legacy compaction extras".into(),
            status: Status::Warn,
            message: details,
            hint: Some(hint),
        }
    }
}

/// Detect operators still on the pre-Beat-33 `<root>/crew/{roles,
/// missions,sprints,crews,skills,role-model-pins.json}` layout
/// and emit an mv-script they can copy-paste to flatten. The loader's
/// dual-read keeps the legacy layout working, so this is a Warn (not
/// Fail) — operator-sovereignty: doctor proposes, operator runs.
///
/// The script writes to stderr-friendly stdout (the hint field), so a
/// fresh-Claude session can read it back and offer to execute. Doctor
/// itself never mutates operator state.
fn check_beat33_legacy_crew_dir() -> Check {
    use darkmux_crew::loader::user_state_root;
    let root = user_state_root();
    let legacy_dir = root.join("crew");
    if !legacy_dir.is_dir() {
        return Check {
            name: "beat-33 crew/ layout".into(),
            status: Status::Pass,
            message: "user state already on the flattened layout".into(),
            hint: None,
        };
    }

    // Inventory what's actually under <root>/crew/ so the message is
    // specific. We only care about the post-Beat-33 promoted subdirs +
    // the pinned file; anything else under crew/ is operator-authored
    // territory we won't recommend moving.
    let promoted_subdirs = ["roles", "missions", "sprints", "crews", "skills"];
    let promoted_file = "role-model-pins.json";
    let mut present_subdirs: Vec<&str> = promoted_subdirs
        .iter()
        .filter(|s| legacy_dir.join(s).is_dir())
        .copied()
        .collect();
    let pins_present = legacy_dir.join(promoted_file).is_file();
    present_subdirs.sort();

    if present_subdirs.is_empty() && !pins_present {
        // <root>/crew/ exists but is empty / has no promoted content.
        // Likely a directory the operator created themselves — leave alone.
        return Check {
            name: "beat-33 crew/ layout".into(),
            status: Status::Pass,
            message: format!(
                "{} exists but holds no promoted subdirs — leaving alone",
                legacy_dir.display()
            ),
            hint: None,
        };
    }

    // Build the mv-script. One line per existing promoted subdir + the
    // pins file. `mv -n` (no-clobber) is deliberate: if the operator has
    // partial state at both locations, we never overwrite the canonical
    // side; they merge manually.
    let mut script_lines: Vec<String> = Vec::new();
    for subdir in &present_subdirs {
        script_lines.push(format!(
            "mv -n {legacy}/{subdir} {root}/{subdir}",
            legacy = legacy_dir.display(),
            root = root.display(),
            subdir = subdir
        ));
    }
    if pins_present {
        script_lines.push(format!(
            "mv -n {legacy}/{file} {root}/{file}",
            legacy = legacy_dir.display(),
            root = root.display(),
            file = promoted_file
        ));
    }
    script_lines.push(format!(
        "rmdir {} 2>/dev/null || true",
        legacy_dir.display()
    ));

    let mut listed = present_subdirs
        .iter()
        .map(|s| (*s).to_string())
        .collect::<Vec<_>>();
    if pins_present {
        listed.push(promoted_file.to_string());
    }
    let listed_str = listed.join(", ");

    Check {
        name: "beat-33 crew/ layout".into(),
        status: Status::Warn,
        message: format!(
            "operator state still under {}/ (found: {listed_str}); flattening is recommended",
            legacy_dir.display()
        ),
        hint: Some(format!(
            "darkmux still reads the legacy layout via the loader's dual-read fallback — no \
             rush. When you're ready to flatten, copy-paste this (uses `mv -n` so existing \
             canonical files are never overwritten):\n\n{script}\n\n\
             Note: if you set DARKMUX_CREW_DIR explicitly, this check assumes the env var \
             points at the post-flatten root (e.g. `~/.darkmux/`). If you instead set it \
             at the legacy `crew/` dir (`~/.darkmux/crew/`), the dual-read keeps working \
             but this script's paths are computed from the env var value as-given.",
            script = script_lines.join("\n")
        )),
    }
}

/// Warn when any role manifest declares unknown tool-vocab tokens
/// (typos like "exce" for "exec", future tokens not yet wired).
///
/// Without this check, the only operator-visible signal of a typo
/// was the `darkmux crew dispatch: tool_palette filtered to []`
/// line at dispatch time — easy to miss, and only surfaces AFTER
/// the operator tried to use the role. Doctor walks every role
/// manifest proactively. (#340)
fn check_role_tool_vocab_typos() -> Check {
    let roles = match darkmux_crew::loader::load_roles() {
        Ok(rs) => rs,
        Err(e) => {
            return Check {
                name: "role tool-vocab".into(),
                status: Status::Warn,
                message: format!("could not load role manifests: {e:#}"),
                hint: None,
            };
        }
    };

    // Collect (role_id, [unknown tokens]) pairs for roles with any
    // unknowns. Sorted by role id for stable output.
    let mut findings: Vec<(String, Vec<String>)> = Vec::new();
    for role in &roles {
        let unknowns =
            darkmux_crew::dispatch_internal::unknown_role_vocab_tokens(&role.tool_palette);
        if !unknowns.is_empty() {
            findings.push((role.id.clone(), unknowns));
        }
    }
    findings.sort_by(|a, b| a.0.cmp(&b.0));

    if findings.is_empty() {
        return Check {
            name: "role tool-vocab".into(),
            status: Status::Pass,
            message: format!(
                "all {} role manifest(s) use known tool-vocab tokens",
                roles.len()
            ),
            hint: None,
        };
    }

    let summary = findings
        .iter()
        .map(|(role, unknowns)| format!("`{role}`: [{}]", unknowns.join(", ")))
        .collect::<Vec<_>>()
        .join("; ");
    Check {
        name: "role tool-vocab".into(),
        status: Status::Warn,
        message: format!(
            "{} role(s) declare unknown tool-vocab tokens: {summary}",
            findings.len()
        ),
        hint: Some(format!(
            "Edit the offending role manifest(s) — likely typos. Known tokens: {}.",
            darkmux_crew::dispatch_internal::known_role_vocab_csv()
        )),
    }
}

/// Warn when openclaw's `agents.list[].model` for each `darkmux/<role>`
/// agent doesn't match the active pin table. Drift means `darkmux crew
/// sync` hasn't been run since the pin table changed (or the operator
/// hand-edited openclaw.json). Recovery is one command: `darkmux crew
/// sync`. (#160)
fn check_role_model_pin_drift() -> Check {
    let pins = match darkmux_crew::pins::load_pins() {
        Ok(t) => t,
        Err(e) => {
            return Check {
                name: "role-model pin drift".into(),
                status: Status::Warn,
                message: format!("could not load pin table: {e:#}"),
                hint: Some(
                    "Check the user override at <crew_root>/role-model-pins.json parses, or remove it to fall back to the embedded default."
                        .into(),
                ),
            };
        }
    };

    // Resolve through dispatch::default_openclaw_config so the doctor
    // reads the same path sync writes to — respects DARKMUX_OPENCLAW_CONFIG.
    // Without this, an operator using the env override sees doctor
    // pass-silently while sync is actually writing to a different file.
    let openclaw_path = darkmux_crew::dispatch::default_openclaw_config();
    if !openclaw_path.exists() {
        return Check {
            name: "role-model pin drift".into(),
            status: Status::Pass,
            message: format!(
                "(no openclaw config at {} — skipping; run `darkmux crew sync` once openclaw is configured)",
                openclaw_path.display()
            ),
            hint: None,
        };
    }
    let raw = match std::fs::read_to_string(&openclaw_path) {
        Ok(r) => r,
        // (#906) TOCTOU: the file existed at the `exists()` check above but
        // could be deleted before this read. A NotFound here is the same
        // "no config" state as the early return — Pass, not a spurious Warn.
        // Other IO errors (perms, etc.) are real problems → Warn.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Check {
                name: "role-model pin drift".into(),
                status: Status::Pass,
                message: format!(
                    "(no openclaw config at {} — skipping; run `darkmux crew sync` once openclaw is configured)",
                    openclaw_path.display()
                ),
                hint: None,
            };
        }
        Err(e) => {
            return Check {
                name: "role-model pin drift".into(),
                status: Status::Warn,
                message: format!("could not read {}: {e}", openclaw_path.display()),
                hint: None,
            };
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => {
            return Check {
                name: "role-model pin drift".into(),
                status: Status::Warn,
                message: "openclaw.json failed to parse — skipping".into(),
                hint: None,
            };
        }
    };

    let Some(agents) = parsed
        .get("agents")
        .and_then(|a| a.get("list"))
        .and_then(|l| l.as_array())
    else {
        return Check {
            name: "role-model pin drift".into(),
            status: Status::Pass,
            message: "(no agents.list array in openclaw.json)".into(),
            hint: None,
        };
    };

    // Collect drift: (role_id, expected_pin, actual_value_or_None) per
    // darkmux/-namespaced agent whose model field doesn't match the pin.
    let mut drifts: Vec<(String, String, Option<String>)> = Vec::new();
    let mut checked: u32 = 0;
    for agent in agents {
        let id = agent.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let Some(role_id) = id.strip_prefix("darkmux/") else {
            continue;
        };
        checked += 1;
        let expected = pins.pin_for(role_id);
        let actual = agent
            .get("model")
            .and_then(|v| v.as_str())
            .map(String::from);
        let matches = actual.as_deref() == Some(expected);
        if !matches {
            drifts.push((role_id.to_string(), expected.to_string(), actual));
        }
    }

    if checked == 0 {
        return Check {
            name: "role-model pin drift".into(),
            status: Status::Pass,
            message:
                "(no darkmux/* agents in openclaw.json — run `darkmux crew sync` to register them)"
                    .into(),
            hint: None,
        };
    }

    if drifts.is_empty() {
        Check {
            name: "role-model pin drift".into(),
            status: Status::Pass,
            message: format!("{checked} darkmux/* agent(s) pinned correctly"),
            hint: None,
        }
    } else {
        let summary = drifts
            .iter()
            .take(3)
            .map(|(role, expected, actual)| {
                let actual_str = actual.as_deref().unwrap_or("(no model field)");
                format!("`{role}` expected `{expected}` got `{actual_str}`")
            })
            .collect::<Vec<_>>()
            .join("; ");
        let more = if drifts.len() > 3 {
            format!(" (+{} more)", drifts.len() - 3)
        } else {
            String::new()
        };
        Check {
            name: "role-model pin drift".into(),
            status: Status::Warn,
            message: format!(
                "{} of {checked} darkmux/* agent(s) drift from pin table: {summary}{more}",
                drifts.len()
            ),
            hint: Some(
                "Run `darkmux crew sync` to re-write the agent entries with their pinned models. The pin table lives at templates/builtin/role-model-pins.json (or <crew_root>/role-model-pins.json for operator overrides)."
                    .into(),
            ),
        }
    }
}

/// Walk the audit directory and roll up the integrity-check results
/// into a single doctor check. Pass when every file's chain validates.
/// Warn when no audit files exist (operator hasn't enabled AuditFileSink,
/// or hasn't written through it yet). Fail when ANY chain is broken —
/// chain break is the audit substrate's tampering signal, not a
/// recoverable warning. (#163)
fn check_audit_integrity() -> Check {
    let reports = match darkmux_flow::integrity_check_all() {
        Ok(r) => r,
        Err(e) => {
            return Check {
                name: "audit integrity".into(),
                status: Status::Warn,
                message: format!("could not walk audit dir: {e:#}"),
                hint: Some(
                    "Check DARKMUX_AUDIT_DIR or the default `~/.darkmux/audit/` is readable."
                        .into(),
                ),
            };
        }
    };

    if reports.is_empty() {
        let dir = darkmux_flow::audit_dir().display().to_string();
        return Check {
            name: "audit integrity".into(),
            status: Status::Warn,
            message: format!("no audit files under {dir}"),
            hint: Some(
                "AuditFileSink is opt-in: set DARKMUX_AUDIT_DIR to enable a BLAKE3 hash-chained audit log whose edits `darkmux flow integrity-check` detects (absent a full re-chain — the chain is un-anchored), alongside the casual LocalFile sink. Useful for compliance deployments (ISO 27001, AI Act, etc.)."
                    .into(),
            ),
        };
    }

    let broken: Vec<&darkmux_flow::IntegrityReport> =
        reports.iter().filter(|r| !r.chain_valid).collect();
    if broken.is_empty() {
        let total_records: u64 = reports.iter().map(|r| r.records_checked).sum();
        Check {
            name: "audit integrity".into(),
            status: Status::Pass,
            // "verified at this check" makes the point-in-time nature
            // explicit — bare "verified" reads as a stronger claim than
            // the implementation supports (#189). Verification is per
            // `flow integrity-check` walk, not a continuous property
            // of the artifact.
            message: format!(
                "{} file(s), {total_records} record(s), all chains pass the integrity walk at this check",
                reports.len()
            ),
            hint: None,
        }
    } else {
        let first = broken[0];
        let summary = format!(
            "{}/{} file(s) BROKEN — {} at line {} ({})",
            broken.len(),
            reports.len(),
            first.path,
            first.break_at_line.unwrap_or(0),
            first
                .break_reason
                .clone()
                .unwrap_or_else(|| "no reason captured".into()),
        );
        Check {
            name: "audit integrity".into(),
            status: Status::Fail,
            message: summary,
            hint: Some(
                "Audit log has been edited or a write was interleaved. Run `darkmux flow integrity-check` for the full per-file breakdown. If tampering is suspected, the chain break locates the affected line; older records before that line are still trustworthy."
                    .into(),
            ),
        }
    }
}

/// (#877) Surface DROPPED audit writes. An `AuditFileSink` write failure leaves
/// a durable `audit.write_failed` breadcrumb in the local flow sink — the hash
/// chain itself still validates clean (the next record re-seeds `prev_hash`
/// from the file tail), so `integrity-check` cannot see the gap. Counting
/// today's breadcrumbs makes the dropped write DETECTABLE: the audit log is
/// INCOMPLETE for those records even though the surviving chain passes.
fn check_audit_write_drops() -> Check {
    let n = darkmux_flow::count_audit_write_failures_today();
    if n == 0 {
        Check {
            name: "audit write integrity".into(),
            status: Status::Pass,
            message: "no dropped audit writes recorded today".into(),
            hint: None,
        }
    } else {
        Check {
            name: "audit write integrity".into(),
            status: Status::Warn,
            message: format!(
                "{n} audit write(s) FAILED today — the hash chain is INCOMPLETE for those records (the surviving chain still passes integrity-check)"
            ),
            hint: Some(
                "An AuditFileSink write failed (audit dir unwritable / ENOSPC / flock contention). \
                 Confirm DARKMUX_AUDIT_DIR (or ~/.darkmux/audit) is writable; the dropped records are \
                 in today's flow file as `action=audit.write_failed`. For compliance, treat a dropped \
                 audit write as a record-keeping incident, not a silent event."
                    .into(),
            ),
        }
    }
}

/// Pure decision for `check_daemon_auth` (#881) — split out so both arms are
/// testable without touching the Keychain/env. Always informational (never a
/// Warn): a loopback-only daemon with no token is the SAFE default, and the
/// refuse-to-bind gate already blocks the unsafe non-loopback-without-token
/// state at runtime, so there's nothing to cry wolf about here.
fn daemon_auth_status(token_present: bool) -> (Status, String, Option<String>) {
    if token_present {
        (
            Status::Pass,
            "serve token configured — non-loopback bind allowed; remote reads + /diff require the bearer token".into(),
            None,
        )
    } else {
        (
            Status::Pass,
            "no serve token — the daemon is loopback-only (a non-loopback `--bind` is refused)".into(),
            Some(
                "Safe as-is for a single machine. To expose the daemon across your fleet \
                 (e.g. `fleet status --deep`), set ONE shared bearer token on every machine: \
                 `security add-generic-password -U -a \"$USER\" -s darkmux-serve-token -w` (macOS) + \
                 `daemon_auth_enabled: true` in ~/.darkmux/config.json, or export DARKMUX_SERVE_TOKEN."
                    .into(),
            ),
        )
    }
}

/// `serve daemon auth`: surfaces the bearer-auth posture (#881). Informational
/// — the bind gate enforces safety at runtime; this just reports whether a
/// shared fleet token is set.
fn check_daemon_auth() -> Check {
    let (status, message, hint) = daemon_auth_status(darkmux_flow::serve_token_present());
    Check { name: "serve daemon auth".into(), status, message, hint }
}

/// Warn when the operator's profile registry contains a profile literally
/// named `recommended` — that name is reserved by `darkmux swap
/// recommended`, so the operator-defined profile is shadowed and
/// unreachable via the `swap` verb. (#159)
fn check_recommended_profile_name_not_shadowed() -> Check {
    let loaded = match darkmux_profiles::profiles::load_registry(None) {
        Ok(l) => l,
        Err(_) => {
            // Registry load failures are surfaced by check_profile_registry;
            // this check passes here to avoid duplicate noise.
            return Check {
                name: "recommended profile name not shadowed".into(),
                status: Status::Pass,
                message: "registry not loaded; check skipped".into(),
                hint: None,
            };
        }
    };
    if darkmux_recommendations::operator_has_shadowed_recommended_profile(&loaded.registry) {
        Check {
            name: "recommended profile name not shadowed".into(),
            status: Status::Warn,
            message: "`recommended` is a reserved profile name; the literal profile in your registry is unreachable via `darkmux swap`".into(),
            hint: Some(
                "Rename the `recommended` profile in ~/.darkmux/profiles.json to something else (e.g. `my-recommended`). The reserved name routes through the bake-off recommendation registry — `darkmux swap recommended` resolves to the validated profile for your hardware tier."
                    .into(),
            ),
        }
    } else {
        Check {
            name: "recommended profile name not shadowed".into(),
            status: Status::Pass,
            message: "no shadowing — `recommended` is free to route to the recommendation registry"
                .into(),
            hint: None,
        }
    }
}

/// `utility model`: surfaces the machine-level `internal.utility` binding
/// (#590) — the standing support model the runtime summons for compaction
/// (and future estimation / mission-compile). When it's registered the model
/// must be LOADED, because compaction fires mid-dispatch and a missing
/// utility model makes the compactor call fail. This is the operator-facing
/// half of the silent-eviction guard (the dispatch-time check lands with the
/// wiring); doctor flags "registered but not loaded" before you dispatch.
fn check_utility_model_binding() -> Check {
    let registry_util = darkmux_profiles::profiles::load_registry(None)
        .ok()
        .and_then(|l| l.registry.utility_model_id().map(str::to_string));
    // Only query LMStudio when there's a binding to check.
    let loaded = if registry_util.is_some() {
        darkmux_profiles::lms::list_loaded().ok()
    } else {
        None
    };
    utility_binding_status(registry_util.as_deref(), loaded.as_deref())
}

/// Pure decision for `check_utility_model_binding`, split out so every arm is
/// unit-testable without a live LMStudio. `loaded` is `None` when the binding
/// is set but `lms ps` couldn't be queried.
fn utility_binding_status(
    registry_util: Option<&str>,
    loaded: Option<&[darkmux_types::LoadedModel]>,
) -> Check {
    let name = "utility model".to_string();
    let Some(id) = registry_util else {
        return Check {
            name,
            status: Status::Pass,
            message: "no machine utility model registered; compaction uses the runtime default"
                .into(),
            hint: Some(
                "Optional: register a small fast model as this machine's utility model in ~/.darkmux/profiles.json — `\"internal\": { \"utility\": \"<model-id>\" }`. It serves compaction (and future estimation/mission-compile) for every role, decoupled from your profiles. (#590)".into(),
            ),
        };
    };
    match loaded {
        None => Check {
            name,
            status: Status::Warn,
            message: format!(
                "utility model `{id}` registered; couldn't query LMStudio to confirm it's loaded"
            ),
            hint: Some("Start LMStudio and ensure `lms ps` returns successfully.".into()),
        },
        Some(models) => {
            let is_loaded = models.iter().any(|m| m.model == id || m.identifier == id);
            if is_loaded {
                Check {
                    name,
                    status: Status::Pass,
                    message: format!("utility model `{id}` registered and loaded"),
                    hint: None,
                }
            } else {
                Check {
                    name,
                    status: Status::Warn,
                    message: format!("utility model `{id}` registered but NOT loaded"),
                    hint: Some(
                        "Load it before dispatching — compaction summons the utility model mid-dispatch; if it isn't resident the compactor call fails. Run `lms load <id>`, or include it in the profile you `darkmux swap` to. (#590)".into(),
                    ),
                }
            }
        }
    }
}

/// Warn (not fail) when the active LMStudio loads don't match the
/// recommendation registry's pick for this hardware tier. The operator
/// may have swapped intentionally — doctor surfaces the drift; doesn't
/// block dispatches. (#159)
fn check_recommendation_drift() -> Check {
    let rec = match darkmux_recommendations::for_active_hardware() {
        Ok(r) => r,
        Err(e) => {
            return Check {
                name: "recommendation drift".into(),
                status: Status::Warn,
                message: format!("could not resolve recommendation: {e:#}"),
                hint: None,
            };
        }
    };

    // Only `Validated` tiers have a recommendation to drift from. For
    // pending-bake-off / no-recommendation tiers, the check warns
    // because the operator's tier has no opinion they can align with —
    // a passive "drift check inactive" would read as all-clear in
    // doctor's red/yellow/green summary glance, hiding the gap.
    if rec.status != darkmux_recommendations::RecommendationStatus::Validated {
        return Check {
            name: "recommendation drift".into(),
            status: Status::Warn,
            message: format!(
                "tier `{}` has no validated recommendation (status: {:?}) — pick a profile manually",
                rec.tier, rec.status
            ),
            hint: Some(rec.rationale.clone()),
        };
    }

    let required = rec.required_model_ids();
    let loaded = match darkmux_profiles::lms::list_loaded() {
        Ok(l) => l,
        Err(_) => {
            return Check {
                name: "recommendation drift".into(),
                status: Status::Warn,
                message: "could not query LMStudio for loaded models — `lms` unreachable".into(),
                hint: Some(
                    "Start LMStudio and ensure `lms ps` returns successfully. The drift check needs to know what's loaded to compare against the recommendation."
                        .into(),
                ),
            };
        }
    };

    // Match against the LMStudio `modelKey` regardless of the namespaced
    // identifier (which may carry `darkmux:` prefix). `LoadedModel.model`
    // holds the model_key in current lms ps --json output.
    let loaded_keys: std::collections::HashSet<&str> =
        loaded.iter().map(|m| m.model.as_str()).collect();
    let missing: Vec<&String> = required
        .iter()
        .filter(|id| !loaded_keys.contains(id.as_str()))
        .collect();

    if missing.is_empty() {
        Check {
            name: "recommendation drift".into(),
            status: Status::Pass,
            message: format!(
                "tier `{}` — all {} recommended model(s) loaded",
                rec.tier,
                required.len()
            ),
            hint: None,
        }
    } else {
        let missing_list: Vec<String> = missing.iter().map(|s| s.to_string()).collect();
        Check {
            name: "recommendation drift".into(),
            status: Status::Warn,
            message: format!(
                "tier `{}` — {} of {} recommended model(s) not loaded: {}",
                rec.tier,
                missing.len(),
                required.len(),
                missing_list.join(", ")
            ),
            hint: Some(format!(
                "Run `darkmux swap recommended` to align with the bake-off pick ({}). Ignore if you swapped intentionally.",
                rec.bake_off_url.as_deref().unwrap_or("see registry for rationale")
            )),
        }
    }
}

/// Surface the machine_id that flow records will be tagged with. Always
/// passes — this is informational, since the operator can leave it at
/// the hostname default. The check names the source (env vs hostname
/// vs unknown) so operators can see whether their `DARKMUX_MACHINE_ID`
/// override is taking effect. (#167)
fn check_machine_id_resolution() -> Check {
    let env_set = std::env::var("DARKMUX_MACHINE_ID")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let resolved = darkmux_flow::resolve_machine_id();
    match (env_set, resolved) {
        (Some(_), Some(id)) => Check {
            name: "machine_id".into(),
            status: Status::Pass,
            message: format!("`{id}` (from DARKMUX_MACHINE_ID env)"),
            hint: None,
        },
        (None, Some(id)) => Check {
            name: "machine_id".into(),
            status: Status::Pass,
            message: format!("`{id}` (from hostname)"),
            hint: Some(
                "Set DARKMUX_MACHINE_ID for a logical fleet name (e.g. `studio`, `mini-1`) — operator-named identifiers read better in the topology view than DNS-style hostnames.".into(),
            ),
        },
        (_, None) => Check {
            name: "machine_id".into(),
            status: Status::Warn,
            message: "could not resolve a machine_id — flow records will lack machine provenance".into(),
            hint: Some(
                "Set DARKMUX_MACHINE_ID to a logical fleet name (e.g. `studio`, `mini-1`), or install `hostname(1)` on PATH.".into(),
            ),
        },
    }
}

/// Surface whether the operator has declared an orchestrator for flow
/// records. Warns when absent — the field is operator-explicit by design
/// (#167 + #49) but the operator needs to know it exists.
fn check_orchestrator_declared() -> Check {
    match darkmux_flow::resolve_orchestrator() {
        Some(name) => Check {
            name: "orchestrator".into(),
            status: Status::Pass,
            message: format!("`{name}` (from DARKMUX_ORCHESTRATOR env)"),
            hint: None,
        },
        None => Check {
            name: "orchestrator".into(),
            status: Status::Warn,
            message: "not declared — flow records won't carry orchestrator provenance".into(),
            hint: Some(
                "Export DARKMUX_ORCHESTRATOR=<harness-name> in the shell driving darkmux (e.g. `claude-code`, `antigravity`, `cursor`). Operator-explicit by design (#49 cultivation discipline).".into(),
            ),
        },
    }
}

/// Surface the machine's declared fleet position (#933) with provenance, and
/// flag an unrecognized `fleet.mode`. `standalone` (default), `hub`, and `peer`
/// are Pass; a typo is a Warn that names the bad token + the valid set (treated
/// as `standalone` until corrected). Local-machine only — cross-machine fleet
/// coherence (two-hub split-brain etc.) is `doctor --fleet` (#935).
fn check_fleet_mode() -> Check {
    use darkmux_types::config::{DarkmuxConfig, FleetMode};
    let name = "fleet.mode";
    let env_set = std::env::var("DARKMUX_FLEET_MODE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let cfg_set = DarkmuxConfig::load_resolved()
        .fleet
        .and_then(|f| f.mode)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let raw = darkmux_types::config_access::fleet_mode_raw();
    let provenance = if env_set.is_some() {
        "from DARKMUX_FLEET_MODE env"
    } else if cfg_set.is_some() {
        "from config.json"
    } else {
        "default"
    };
    match FleetMode::parse(&raw) {
        Some(_) => Check {
            name: name.into(),
            status: Status::Pass,
            message: format!("`{raw}` ({provenance})"),
            hint: None,
        },
        None => Check {
            name: name.into(),
            status: Status::Warn,
            message: format!("`{raw}` ({provenance}) is not a recognized fleet.mode — treated as `standalone`"),
            hint: Some(
                "Valid values: `standalone` (single machine), `hub` (always-on coordinator), `peer` (points at a hub). Set `fleet.mode` in ~/.darkmux/config.json, or export DARKMUX_FLEET_MODE.".into(),
            ),
        },
    }
}

/// Normalize an OpenAI-style base URL for comparison: strip a trailing `/v1`
/// (clients append it) and any trailing slash, so `http://h:1234/v1` and
/// `http://h:1234` compare equal.
fn normalize_openai_base(s: &str) -> String {
    let s = s.trim().trim_end_matches('/');
    let s = s.strip_suffix("/v1").unwrap_or(s);
    s.trim_end_matches('/').to_string()
}

/// (#5) Decide the `OPENAI_BASE_URL` check outcome from the env value + the
/// LMStudio base darkmux manages. Pure (no env / IO) so it's unit-testable.
fn classify_openai_base_url(base: Option<&str>, lms_url: &str) -> (Status, String, Option<String>) {
    match base {
        None => (
            Status::Pass,
            "OPENAI_BASE_URL unset — downstream agents aren't pinned to a non-darkmux endpoint".into(),
            None,
        ),
        Some(b) if normalize_openai_base(b) == normalize_openai_base(lms_url) => (
            Status::Pass,
            format!("OPENAI_BASE_URL points at darkmux's LMStudio ({lms_url}) — swaps reach downstream agents"),
            None,
        ),
        Some(b) => (
            Status::Warn,
            format!("OPENAI_BASE_URL={b} does not point at darkmux's LMStudio ({lms_url})"),
            Some(
                "darkmux doesn't set or manage OPENAI_BASE_URL — `darkmux swap` loads models into the LMStudio at lmstudio_url. OpenAI-compatible agents reading this env var talk to the other endpoint, so a swap won't change the model they see. Point OPENAI_BASE_URL at darkmux's LMStudio (or unset it) if you want swaps to reach those agents. (If it's a reverse proxy fronting the SAME LMStudio, this warning is benign.) (#5)".into(),
            ),
        ),
    }
}

/// (#5) Warn when a shell-exported `OPENAI_BASE_URL` would defeat `darkmux swap`
/// for downstream OpenAI-compatible agents (they read the env var, not darkmux).
fn check_openai_base_url_conflict() -> Check {
    let base = std::env::var("OPENAI_BASE_URL").ok();
    let lms = darkmux_types::config_access::lmstudio_url();
    let (status, message, hint) = classify_openai_base_url(base.as_deref(), &lms);
    Check {
        name: "openai endpoint".into(),
        status,
        message,
        hint,
    }
}

/// Surface a config-assembled Redis that would connect WITHOUT a password —
/// `config.redis.enabled` is set but neither the Keychain item `darkmux-redis`
/// nor `DARKMUX_REDIS_URL` supplies credentials. Password-less is fine for a
/// local/Tailnet-trusted Redis but fails against an auth-required one, so this
/// warns (never fails). The env-URL path (password inline) is self-contained,
/// and a disabled config Redis is a no-op — both Pass. (#661 Slice 5)
fn check_redis_config() -> Check {
    let name = "redis config";
    let env_url = std::env::var("DARKMUX_REDIS_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .is_some();
    if env_url {
        return Check { name: name.into(), status: Status::Pass, message: "Redis via DARKMUX_REDIS_URL".into(), hint: None };
    }
    if !darkmux_types::config_access::redis_enabled() {
        return Check { name: name.into(), status: Status::Pass, message: "config Redis disabled".into(), hint: None };
    }
    // enabled + no env URL → the config-assembled (tier-2) path is active.
    match darkmux_types::config_access::redis_host() {
        None => Check {
            name: name.into(),
            status: Status::Warn,
            message: "config.redis.enabled=true but no config.redis.host — Redis can't be assembled".into(),
            hint: Some("Set `config.redis.host` (and `port`) in ~/.darkmux/config.json, or set DARKMUX_REDIS_URL.".into()),
        },
        Some(host) if darkmux_flow::redis_keychain_password_present() => Check {
            name: name.into(),
            status: Status::Pass,
            message: format!("config Redis enabled → {host} (password from Keychain)"),
            hint: None,
        },
        Some(host) => Check {
            name: name.into(),
            status: Status::Warn,
            message: format!("config Redis enabled → {host}, but no password (Keychain item `darkmux-redis` absent, no DARKMUX_REDIS_URL) — connecting password-less"),
            hint: Some("If your Redis requires auth, store the password: `security add-generic-password -a $USER -s darkmux-redis -w` (URL-safe). Password-less is fine for a local/Tailnet-trusted Redis.".into()),
        },
    }
}

/// Roll up `flow::collect_status()` into a single doctor check. Pass when
/// `overall_state=ok`; warn when warn (with the reasons listed); fail
/// when fail. The full diagnostic detail lives in `darkmux flow status`;
/// this check is the operator-glance signal that something needs a
/// closer look. (#170)
fn check_flow_sink_health() -> Check {
    let status = darkmux_flow::collect_status();
    let composition = status.sinks.composition.clone();
    match status.overall_state {
        darkmux_flow::HealthState::Ok => Check {
            name: "flow sink health".into(),
            status: Status::Pass,
            message: format!(
                "{composition} healthy · schema {} · {} day file(s)",
                status.schema_version, status.disk.day_files
            ),
            hint: None,
        },
        darkmux_flow::HealthState::Warn => {
            let reasons = if status.warn_reasons.is_empty() {
                "(no specific warn reasons captured)".to_string()
            } else {
                status.warn_reasons.join(", ")
            };
            Check {
                name: "flow sink health".into(),
                status: Status::Warn,
                message: format!("{composition} · warnings: {reasons}"),
                hint: Some(
                    "Run `darkmux flow status` for full detail. Common fixes: \
                     start Redis (`brew services start redis`) if `redis_unreachable`; \
                     raise `DARKMUX_REDIS_MAXLEN` if `redis_stream_near_maxlen`; \
                     upgrade the lagging writer in the fleet if `schema_skew_detected`."
                        .into(),
                ),
            }
        }
        darkmux_flow::HealthState::Fail => {
            let reasons = if status.fail_reasons.is_empty() {
                "(no specific failure reasons captured)".to_string()
            } else {
                status.fail_reasons.join(", ")
            };
            Check {
                name: "flow sink health".into(),
                status: Status::Fail,
                message: format!("{composition} · failures: {reasons}"),
                hint: Some(
                    "Run `darkmux flow status` for diagnostic detail. Sink configuration is broken — \
                     flow records may be silently dropped."
                        .into(),
                ),
            }
        }
    }
}

/// Verify every embedded crew-role manifest has a sibling `.md` prompt
/// embedded too. The dispatcher errors at runtime when a manifest exists
/// without a prompt (`crew dispatch <role>` fails with *"role X has no
/// .md system prompt"*); this check surfaces the gap pre-dispatch so
/// operators don't discover it by failing a dispatch.
///
/// Surfaced empirically during the 2026-05-15 100%-local engagement
/// experiment, when 6 dispatches to `analyst` failed instantly because
/// the manifest existed but the prompt didn't. See
/// kstrat2001/darkmux#141 for context.
fn check_crew_role_prompt_coverage() -> Check {
    use darkmux_crew::loader::{builtin_role_prompt_ids, builtin_roles_ids};
    let manifests = builtin_roles_ids();
    let prompts: std::collections::HashSet<&str> = builtin_role_prompt_ids().into_iter().collect();
    let missing: Vec<&str> = manifests
        .into_iter()
        .filter(|id| !prompts.contains(id))
        .collect();
    if missing.is_empty() {
        Check {
            name: "crew role prompt coverage".into(),
            status: Status::Pass,
            message: "every builtin role manifest has a `.md` prompt".into(),
            hint: None,
        }
    } else {
        let list = missing
            .iter()
            .map(|id| format!("`{id}`"))
            .collect::<Vec<_>>()
            .join(", ");
        Check {
            name: "crew role prompt coverage".into(),
            status: Status::Warn,
            message: format!(
                "{} role manifest(s) ship without `.md` prompts and cannot be dispatched: {list}",
                missing.len()
            ),
            hint: Some(
                "Author the missing prompts at `templates/builtin/roles/<id>.md` and \
                 add them to `BUILTIN_ROLE_PROMPTS` in `src/crew/loader.rs`. Operators can \
                 override at `~/.darkmux/roles/<id>.md`."
                    .into(),
            ),
        }
    }
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
fn eureka_checks(include_openclaw: bool) -> Vec<Check> {
    let ctx = eureka::Context::collect();
    eureka::evaluate_all(&ctx)
        .into_iter()
        // (#1010) Suppress OC-path eureka rules from default output unless
        // `--include-openclaw`, keyed on each rule's DECLARED runtime
        // (RuleKind::runtime), not by substring-matching "openclaw" in the
        // human-facing message. The old substring filter leaked any OC rule that
        // named the config by field path (e.g. `agents.defaults.compaction.model`)
        // without the literal word "openclaw" — compactor-not-loaded,
        // agents-default-model-resolves, n-ctx-exceeds-model-max all slipped
        // through. Filtering here (before the Check map) keys on the rule, not its
        // wording, per the schema-isolation doctrine.
        .filter(|(def, _)| include_openclaw || def.kind.runtime() != eureka::RuleRuntime::OpenClaw)
        .map(|(def, verdict)| match verdict {
            eureka::Verdict::Pass => Check {
                name: format!("eureka: {}", def.id),
                status: Status::Pass,
                message: def.name.clone(),
                hint: None,
            },
            // Pass-tier diagnostic: the rule passed but carries an
            // informational message the operator should see (e.g. the
            // JIT-load hint from #101). Renders with a `·` separator so
            // it visually distinguishes from the harder Fire path — the
            // operator sees a green checkmark with a follow-on sentence
            // rather than just the rule name.
            eureka::Verdict::PassWith(message) => Check {
                name: format!("eureka: {}", def.id),
                status: Status::Pass,
                message: format!("{} · {message}", def.name),
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
                    "run `darkmux serve` to start the daemon for live viewing features".into(),
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
    if stream.set_read_timeout(Some(to)).is_err() || stream.set_write_timeout(Some(to)).is_err() {
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
            hint: Some("run `darkmux serve` to start the daemon for live viewing features".into()),
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
                addr, first_line
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
            message: e
                .to_string()
                .lines()
                .next()
                .unwrap_or("load failed")
                .to_string(),
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
        // (#590) The profile's default model (default_model, or first model)
        // is the load-bearing match — the old Primary-role check.
        let default_id = profile.default_model_id();
        let primaries = profile
            .models
            .iter()
            .filter(|m| Some(m.id.as_str()) == default_id);
        // (#544) Use the shared matcher so doctor agrees with the lab
        // surfaces — crucially, this also matches a `darkmux:`-namespaced
        // load, which the old inline check (`identifier == id || model ==
        // id`) silently missed.
        let primary_match = primaries.clone().any(|p| {
            loaded
                .iter()
                .any(|l| darkmux_profiles::envelope::loaded_matches(l, p))
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

/// Sprint-G: openclaw-as-active gate.
///
/// Returns true when openclaw is configured on this machine — defined
/// as: `~/.openclaw/openclaw.json` (or the path the dispatch resolver
/// reports, honoring `DARKMUX_OPENCLAW_CONFIG`) exists on disk.
///
/// The gate is intentionally "config-on-disk" rather than "binary on
/// PATH": post-Beat-36 openclaw is opt-in per dispatch, and the
/// operator's choice to leave openclaw uninstalled / unconfigured IS
/// the signal that they don't intend to use it. An operator who has
/// `openclaw` on PATH but no config (fresh install, partial setup)
/// gets a silent skip — when they configure openclaw, doctor surfaces
/// the binary/version checks automatically.
fn openclaw_active() -> bool {
    darkmux_crew::dispatch::default_openclaw_config().exists()
}

/// (#680) The internal Docker-bounded runtime is the DEFAULT for `crew
/// dispatch` and `lab run`, but nothing else in doctor surfaces it — a fresh
/// operator otherwise gets an all-green doctor and only learns the Docker
/// requirement when their first dispatch bails at the dispatch-time preflight.
/// Reuses that preflight's probe (`dispatch_internal::docker_runtime_status`)
/// so the image tag + probe logic have one home. Warn (not Fail):
/// `--runtime openclaw` users legitimately need no Docker, so this is a
/// heads-up, not a hard error.
fn check_docker_runtime() -> Check {
    docker_status_to_check(darkmux_crew::dispatch_internal::docker_runtime_status())
}

/// Pure status → Check mapping (unit-testable without Docker on the host).
fn docker_status_to_check(status: darkmux_crew::dispatch_internal::DockerRuntimeStatus) -> Check {
    use darkmux_crew::dispatch_internal::{
        ghcr_runtime_image, DockerRuntimeStatus as S, RUNTIME_IMAGE,
    };
    let name = "docker runtime".to_string();
    match status {
        S::Ready => Check {
            name,
            status: Status::Pass,
            message: "Docker daemon up · darkmux runtime image present — internal runtime ready"
                .to_string(),
            hint: None,
        },
        S::BinaryMissing => Check {
            name,
            status: Status::Warn,
            message: "`docker` not on PATH — darkmux's default internal runtime can't dispatch"
                .into(),
            hint: Some(
                "Install Docker Desktop (https://www.docker.com/products/docker-desktop) to use \
                 darkmux's default container-bounded runtime."
                    .into(),
            ),
        },
        S::DaemonUnreachable(_) => Check {
            name,
            status: Status::Warn,
            message:
                "Docker is installed but the daemon isn't reachable — the default internal runtime \
                 can't dispatch"
                    .into(),
            hint: Some("Start Docker Desktop, then re-run `darkmux doctor`.".into()),
        },
        S::ImageMissing => Check {
            name,
            status: Status::Warn,
            message: "Docker is up; no local runtime image — darkmux will pull it on the first \
                      dispatch"
                .to_string(),
            hint: Some(format!(
                "darkmux pulls `{}` from GHCR on demand (#759). Pre-pull now with \
                 `docker pull {}`, or build locally from a source checkout: \
                 `docker build -t {RUNTIME_IMAGE} runtime/`.",
                ghcr_runtime_image(),
                ghcr_runtime_image()
            )),
        },
        S::ProbeError(e) => Check {
            name,
            status: Status::Warn,
            message: format!("couldn't probe the Docker runtime image: {e}"),
            hint: None,
        },
    }
}

fn check_runtime_command() -> Check {
    // Sprint-G: skip when no openclaw config on disk. The internal
    // runtime is the default and needs no external binary; checking
    // for `openclaw` on PATH only matters when the operator has
    // declared OC is part of their setup (config file present).
    if !openclaw_active() {
        return Check {
            name: "runtime command".into(),
            status: Status::Pass,
            message: format!(
                "(skipped — no {} on disk; openclaw not configured on this machine)",
                darkmux_crew::dispatch::default_openclaw_config().display()
            ),
            hint: None,
        };
    }
    let cmd = "openclaw";
    if which(cmd).is_some() {
        Check {
            name: "runtime command".into(),
            status: Status::Pass,
            message: format!("found `{cmd}` on PATH (used when `--runtime openclaw` opt-in fires)"),
            hint: None,
        }
    } else {
        Check {
            name: "runtime command".into(),
            status: Status::Warn,
            message: format!("`{cmd}` not on PATH despite openclaw config being present"),
            hint: Some(
                "your openclaw config exists but the binary isn't on PATH. \
                 Either install openclaw, or remove the config if you don't \
                 intend to use openclaw (darkmux's internal runtime is the default \
                 and needs no external binary)."
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
    // Sprint-G: skip when openclaw not configured on this machine (no
    // config file on disk). Same gate as check_runtime_command — when
    // OC isn't active, version-checking it is noise.
    if !openclaw_active() {
        return Check {
            name: "runtime version".into(),
            status: Status::Pass,
            message: format!(
                "(skipped — no {} on disk; openclaw not configured on this machine)",
                darkmux_crew::dispatch::default_openclaw_config().display()
            ),
            hint: None,
        };
    }
    let cmd = "openclaw";
    let output = Command::new(cmd).arg("--version").output();
    let raw = match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr)
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

// ─── darkmux version vs latest GitHub release (issue #13) ─────────────

const DARKMUX_RELEASES_URL: &str =
    "https://api.github.com/repos/kstrat2001/darkmux/releases/latest";
/// curl timeout in seconds. Short so the check doesn't stall `darkmux
/// doctor` on a flaky network — `(skipped: offline)` is the right
/// outcome here, not a long block.
const DARKMUX_RELEASE_FETCH_TIMEOUT_SECS: &str = "5";

/// Operator-facing doctor check: is the installed `darkmux` behind the
/// latest GitHub release? Network-touched; opt-out via
/// `DARKMUX_CHECK_UPDATES=0` for offline/CI environments.
///
/// Verdict tiers (per issue #13's spec):
///   - Pass — installed == latest, or installed > latest (dev build)
///   - Warn — installed < latest (minor / patch behind)
///   - Fail — installed < latest (major behind — schema break possible)
///   - Pass (skipped) — opt-out, offline, no releases tagged yet, or
///     the response was unparseable
fn check_darkmux_version_vs_latest_release() -> Check {
    const NAME: &str = "darkmux version vs latest release";
    let skip = |reason: &str| Check {
        name: NAME.into(),
        status: Status::Pass,
        message: format!("(skipped: {reason})"),
        hint: None,
    };
    let installed = env!("CARGO_PKG_VERSION");

    // Operator-respect: explicit opt-out beats the network call. Resolves
    // env(DARKMUX_CHECK_UPDATES, opt-out) > config.runtime.check_updates > true
    // (#661 Slice 4).
    if !darkmux_types::config_access::check_updates() {
        return skip("update check disabled (DARKMUX_CHECK_UPDATES / config)");
    }

    match fetch_latest_release_tag() {
        Ok(latest) => classify_version_vs_latest(installed, &latest, NAME),
        Err(reason) => skip(&reason),
    }
}

/// Shell out to `curl` for the GitHub releases API. Avoids adding a
/// reqwest-class dep for a single GET — `curl` is on every macOS and
/// most Linux installs by default. CLAUDE.md: "Don't add dependencies
/// casually."
fn fetch_latest_release_tag() -> Result<String, String> {
    let output = Command::new("curl")
        .args([
            "-sL",
            "--max-time",
            DARKMUX_RELEASE_FETCH_TIMEOUT_SECS,
            "-H",
            "User-Agent: darkmux-doctor",
            "-H",
            "Accept: application/vnd.github+json",
            DARKMUX_RELEASES_URL,
        ])
        .output()
        .map_err(|e| format!("couldn't invoke `curl`: {e}"))?;
    if !output.status.success() {
        return Err(format!("curl exit {}", output.status.code().unwrap_or(-1)));
    }
    let body = String::from_utf8_lossy(&output.stdout);
    if body.trim().is_empty() {
        return Err("offline / empty response".into());
    }
    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("response parse: {e}"))?;
    // GitHub returns `{"message": "Not Found"}` for repos that have no
    // releases tagged. Match it explicitly so the operator sees an
    // honest "no releases tagged yet" rather than a parse error.
    if let Some(msg) = json.get("message").and_then(|v| v.as_str()) {
        if msg.eq_ignore_ascii_case("not found") {
            return Err("no releases tagged yet".into());
        }
        return Err(format!("github api: {msg}"));
    }
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing `tag_name` in response".to_string())?;
    Ok(tag.trim_start_matches('v').to_string())
}

/// Pure verdict logic — extracted so tests pin the matrix without a
/// network round-trip. `installed` and `latest` are the bare semver
/// strings (no `v` prefix); `name` is the doctor-check label so the
/// function can build a fully-shaped `Check` directly.
fn classify_version_vs_latest(installed: &str, latest: &str, name: &str) -> Check {
    let (Some(inst), Some(lat)) = (parse_semver(installed), parse_semver(latest)) else {
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: format!(
                "(skipped: couldn't parse semver — installed={installed}, latest={latest})"
            ),
            hint: None,
        };
    };
    match inst.cmp(&lat) {
        std::cmp::Ordering::Equal | std::cmp::Ordering::Greater => Check {
            name: name.into(),
            status: Status::Pass,
            message: format!("v{installed} (latest released: v{latest})"),
            hint: None,
        },
        std::cmp::Ordering::Less => {
            let major_behind = inst.0 < lat.0;
            let (status, label) = if major_behind {
                (Status::Fail, "major version behind — schema break possible")
            } else {
                (Status::Warn, "minor/patch behind")
            };
            Check {
                name: name.into(),
                status,
                message: format!("v{installed} → v{latest} ({label})"),
                hint: Some(
                    "update with `git pull && cargo install --path . --force` in your darkmux checkout, \
                     or grab the latest release tarball from \
                     https://github.com/kstrat2001/darkmux/releases/latest. \
                     (set DARKMUX_CHECK_UPDATES=0 to silence this check.)"
                        .to_string(),
                ),
            }
        }
    }
}

/// Tolerant semver parser — drops `v` prefix, parses major.minor.patch
/// as `u32`, ignores any pre-release / build-metadata suffix on the
/// patch segment. `0.4.0-beta.1` parses as `(0, 4, 0)`.
fn parse_semver(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut parts = s.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch_seg = parts.next()?;
    // Strip pre-release / build-metadata so e.g. `0-beta.1` reads as `0`.
    let patch_digits: String = patch_seg
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let patch = patch_digits.parse().ok()?;
    Some((major, minor, patch))
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

    classify_ram_headroom(reclaimable_gb, loaded_models_size_gb, RAM_SAFETY_MARGIN_GB)
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
    let real_headroom_f =
        (reclaimable_gb as f64) + loaded_models_size_gb - (safety_margin_gb as f64);
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
            hint: Some("close apps or shrink ctx before measurement-grade lab runs".into()),
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

    let unloaded: Vec<&darkmux_types::ProfileModel> = profile
        .models
        .iter()
        .filter(|pm| {
            let ns = darkmux_profiles::swap::namespaced_identifier(pm);
            !loaded
                .iter()
                .any(|l| l.identifier == pm.id || l.model == pm.id || l.identifier == ns)
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

    classify_load_projection(reclaimable_gb, total_unloaded_gb, &pending, profile_name)
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
            message: format!("{summary} — within {RAM_SAFETY_MARGIN_GB} GB safety margin"),
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
    registry: &'a darkmux_profiles::profiles::LoadedRegistry,
    loaded: &[darkmux_types::LoadedModel],
) -> Option<(&'a str, &'a darkmux_types::Profile)> {
    let matches: Vec<(&str, &darkmux_types::Profile)> = registry
        .registry
        .profiles
        .iter()
        .filter(|(_, p)| {
            let default_id = p.default_model_id();
            p.models
                .iter()
                .filter(|m| Some(m.id.as_str()) == default_id)
                .any(|pm| {
                    let ns = darkmux_profiles::swap::namespaced_identifier(pm);
                    loaded
                        .iter()
                        .any(|l| l.identifier == pm.id || l.model == pm.id || l.identifier == ns)
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
    // #332 — use the canonical openclaw-config resolver so the
    // `DARKMUX_OPENCLAW_CONFIG` env var is honored consistently
    // with every other OC-touching surface (dispatcher, swap's
    // apply_runtime, Sprint-G's openclaw-active gate). Pre-fix this
    // check hardcoded `~/.openclaw/openclaw.json` and silently
    // probed the wrong file when the operator pointed the env var
    // somewhere else.
    let openclaw_path = darkmux_crew::dispatch::default_openclaw_config();
    if !openclaw_path.exists() {
        return Check {
            name: "agent role scaffolds".into(),
            status: Status::Pass,
            message: format!("(no {} on disk — skipping)", openclaw_path.display()),
            hint: None,
        };
    }
    let raw = match std::fs::read_to_string(&openclaw_path) {
        Ok(r) => r,
        // (#906) TOCTOU: deleted between `exists()` and this read → NotFound
        // is the same "no config" state as the early return (Pass). Other IO
        // errors are real problems → Warn.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Check {
                name: "agent role scaffolds".into(),
                status: Status::Pass,
                message: format!("(no {} on disk — skipping)", openclaw_path.display()),
                hint: None,
            };
        }
        Err(e) => {
            return Check {
                name: "agent role scaffolds".into(),
                status: Status::Warn,
                message: format!("could not read {}: {e}", openclaw_path.display()),
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
            .map(|id| format!("`oc-scaffold.sh template {id}`"))
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
                "darkmux ships validated scaffolds for these roles. From your darkmux \
                 source checkout, run `integrations/openclaw/oc-scaffold.sh template <role>` \
                 (e.g. {suggestions}) and paste the snippet into openclaw.json."
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

/// Warn when legacy flat mission/sprint files exist in the pre-#148 layout.
/// Pass when neither legacy_missions_dir nor legacy_sprints_dir contain any
/// top-level .json files. Fail never — legacy files don't break the system,
/// but they're a signal that `darkmux mission migrate --apply` should be run
/// to consolidate into the per-mission layout. (#148)
fn check_legacy_mission_layout() -> Check {
    let missions_dir = darkmux_crew::lifecycle::legacy_missions_dir();
    let sprints_dir = darkmux_crew::lifecycle::legacy_sprints_dir();

    let mut legacy_count = 0u32;

    // Count legacy flat .json files in missions dir
    if let Ok(entries) = std::fs::read_dir(&missions_dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_file() {
                    if let Some(ext) = entry.path().extension() {
                        if ext == "json" {
                            legacy_count += 1;
                        }
                    }
                }
            }
        }
    }

    // Count legacy flat .json files in sprints dir
    if let Ok(entries) = std::fs::read_dir(&sprints_dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_file() {
                    if let Some(ext) = entry.path().extension() {
                        if ext == "json" {
                            legacy_count += 1;
                        }
                    }
                }
            }
        }
    }

    if legacy_count > 0 {
        // Display the actual dirs the legacy files live under (resolved
        // through dual-read so the path shown is the one the operator
        // can cd into, regardless of canonical vs Beat-33-legacy layout).
        let missions = darkmux_crew::loader::missions_dir();
        let sprints = darkmux_crew::loader::sprints_dir();
        Check {
            name: "legacy mission layout".into(),
            status: Status::Warn,
            message: format!(
                "{legacy_count} legacy flat file(s) at {}/<id>.json or {}/<id>.json",
                missions.display(),
                sprints.display()
            ),
            hint: Some(
                "Run `darkmux mission migrate --apply` to move them to the per-mission layout (#148)."
                    .into(),
            ),
        }
    } else {
        Check {
            name: "legacy mission layout".into(),
            status: Status::Pass,
            message: "no legacy flat files".into(),
            hint: None,
        }
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

/// Shim exposing `read_reclaimable_gb` to other modules — kept narrow
/// (just the GB count, no doctor framing) so `serve` can read RAM
/// headroom for /machine/specs without depending on the doctor's
/// classify-into-status flow. (#275)
pub fn reclaimable_gb_for_specs() -> Option<u64> {
    read_reclaimable_gb()
}

/// Same shim shape for the safety-margin constant — exposes the
/// doctor's per-machine reserve so callers compute the same
/// real-headroom expression. (#275)
pub const RAM_SAFETY_MARGIN_GB_FOR_SPECS: u64 = RAM_SAFETY_MARGIN_GB;

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
    let Some(path) = darkmux_profiles::runtime::resolve_openclaw_config_path(None) else {
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
    let changes = darkmux_profiles::runtime::fix_ctx_window_to_loaded(&path, &loaded)?;
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

/// Print one check line + its hint lines. Shared by the verbose and
/// issues-only render paths so they format identically.
fn print_check_line(c: &Check) {
    let marker = match c.status {
        Status::Pass => darkmux_types::style::success("✓"),
        Status::Warn => darkmux_types::style::warn("⚠"),
        Status::Fail => darkmux_types::style::error("✗"),
    };
    println!("  {} {:<22} {}", marker, c.name, c.message);
    if let Some(hint) = c.hint.as_ref() {
        for line in hint.lines() {
            println!("        → {}", darkmux_types::style::dim(line));
        }
    }
}

/// Render the doctor report.
///
/// (#1130) Default (`verbose=false`) is **issues-only**: the build identity
/// line + every Warn/Fail (with hints), and the passing checks collapsed to a
/// count — in most runs the operator only cares about problems. `verbose=true`
/// (`darkmux doctor -v`) prints every check, the old behavior.
pub fn print_report(r: &DoctorReport, verbose: bool) -> Result<()> {
    println!("{}", darkmux_types::style::header(&format!("darkmux doctor — {} checks", r.checks.len())));
    println!();
    if verbose {
        for c in &r.checks {
            print_check_line(c);
        }
    } else {
        // The build identity line always shows (it answers "which version?",
        // not a health question) — it bypasses pass-consolidation.
        for c in r.checks.iter().filter(|c| c.name == BUILD_CHECK_NAME) {
            print_check_line(c);
        }
        // The passing checks collapse to a count — `-v` for the full list.
        let collapsed = r
            .checks
            .iter()
            .filter(|c| c.status == Status::Pass && c.name != BUILD_CHECK_NAME)
            .count();
        if collapsed > 0 {
            println!(
                "  {} {}",
                darkmux_types::style::success("✓"),
                darkmux_types::style::dim(&format!("{collapsed} more checks passed — `-v` for detail")),
            );
        }
        // Warnings + failures in full — the part the operator acts on, placed
        // last so they sit right above the summary line.
        for c in r.checks.iter().filter(|c| c.status != Status::Pass) {
            print_check_line(c);
        }
    }
    println!();
    let summary = match r.worst_status() {
        Status::Pass => darkmux_types::style::success(&format!(
            "all {} checks passed{}",
            r.pass_count(),
            if r.warn_count() > 0 {
                format!(" ({} warning(s))", r.warn_count())
            } else {
                "".into()
            }
        )),
        Status::Warn => darkmux_types::style::warn(&format!(
            "{} pass, {} warn — workable but worth a look",
            r.pass_count(),
            r.warn_count()
        )),
        Status::Fail => darkmux_types::style::error(&format!(
            "{} pass, {} warn, {} fail — fix failures before running darkmux end-to-end",
            r.pass_count(),
            r.warn_count(),
            r.fail_count()
        )),
    };
    println!("{summary}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_base_url_classify_covers_unset_match_and_divergence() {
        let lms = "http://localhost:1234";
        // Unset → Pass, no hint.
        let (s, _, h) = classify_openai_base_url(None, lms);
        assert_eq!(s, Status::Pass);
        assert!(h.is_none());
        // Set + points at darkmux's LMStudio (with the /v1 clients append) → Pass.
        let (s, _, h) = classify_openai_base_url(Some("http://localhost:1234/v1"), lms);
        assert_eq!(s, Status::Pass, "matching endpoint (modulo /v1) must pass");
        assert!(h.is_none());
        // Trailing slash also normalizes equal.
        let (s, _, _) = classify_openai_base_url(Some("http://localhost:1234/"), lms);
        assert_eq!(s, Status::Pass);
        // A trailing slash AFTER /v1 must also normalize equal (exercises the
        // second trim).
        let (s, _, _) = classify_openai_base_url(Some("http://localhost:1234/v1/"), lms);
        assert_eq!(s, Status::Pass);
        // Set + diverges → Warn with an actionable hint naming the conflict.
        let (s, msg, h) = classify_openai_base_url(Some("https://api.openai.com/v1"), lms);
        assert_eq!(s, Status::Warn, "a non-darkmux endpoint must warn (#5)");
        assert!(msg.contains("api.openai.com"));
        assert!(h.unwrap().contains("OPENAI_BASE_URL"));
    }

    fn check(name: &str, status: Status) -> Check {
        Check {
            name: name.into(),
            status,
            message: "x".into(),
            hint: None,
        }
    }

    // ─── #680: docker runtime status → Check mapping ───────────────────

    #[test]
    fn docker_status_ready_passes_no_hint() {
        use darkmux_crew::dispatch_internal::DockerRuntimeStatus;
        let c = docker_status_to_check(DockerRuntimeStatus::Ready);
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("internal runtime ready"), "{}", c.message);
        assert!(c.hint.is_none());
    }

    #[test]
    fn docker_status_binary_missing_warns_not_fails() {
        // Warn, never Fail — openclaw-only operators legitimately have no Docker.
        use darkmux_crew::dispatch_internal::DockerRuntimeStatus;
        let c = docker_status_to_check(DockerRuntimeStatus::BinaryMissing);
        assert_eq!(c.status, Status::Warn);
        assert!(c.hint.unwrap().contains("Install Docker Desktop"));
    }

    #[test]
    fn docker_status_image_missing_warns_with_build_cmd() {
        use darkmux_crew::dispatch_internal::DockerRuntimeStatus;
        let c = docker_status_to_check(DockerRuntimeStatus::ImageMissing);
        assert_eq!(c.status, Status::Warn);
        assert!(
            c.hint
                .unwrap()
                .contains("docker build -t darkmux-runtime:latest runtime/")
        );
    }

    #[test]
    fn docker_status_daemon_unreachable_warns() {
        use darkmux_crew::dispatch_internal::DockerRuntimeStatus;
        let c = docker_status_to_check(DockerRuntimeStatus::DaemonUnreachable("x".into()));
        assert_eq!(c.status, Status::Warn);
        assert!(c.hint.unwrap().contains("Start Docker Desktop"));
    }

    #[test]
    fn docker_status_probe_error_warns_no_hint() {
        use darkmux_crew::dispatch_internal::DockerRuntimeStatus;
        let c = docker_status_to_check(DockerRuntimeStatus::ProbeError("boom".into()));
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("boom"), "{}", c.message);
        assert!(c.hint.is_none());
    }

    #[test]
    fn docker_checks_never_mention_openclaw() {
        // #393 schema-isolation: doctor's default-mode checks must not surface
        // openclaw (enforced by `doctor_default_skips_openclaw_checks`). The
        // docker-runtime check runs in default mode, so NONE of its outputs —
        // across every probe result — may mention openclaw in name/message/hint.
        // This guards it directly, without needing a Docker-absent host.
        use darkmux_crew::dispatch_internal::DockerRuntimeStatus as S;
        for status in [
            S::Ready,
            S::BinaryMissing,
            S::DaemonUnreachable("x".into()),
            S::ImageMissing,
            S::ProbeError("x".into()),
        ] {
            let c = docker_status_to_check(status);
            let blob = format!(
                "{} {} {}",
                c.name,
                c.message,
                c.hint.unwrap_or_default()
            )
            .to_lowercase();
            assert!(!blob.contains("openclaw"), "docker check leaked openclaw: {blob}");
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
        let c = classify_load_projection(8.0, 7.0, &pending(&["big/model ~7.0 GB"]), "deep");
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("safety margin"));
    }

    #[test]
    fn load_projection_fail_when_load_exceeds_reclaimable() {
        // 4 GB free, 8 GB compactor pending. Can't fit; would swap or OOM.
        let c = classify_load_projection(4.0, 8.0, &pending(&["compactor ~8.0 GB"]), "balanced");
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("swap or OOM"));
        // Surfaces the actionable fix (close apps / smaller compactor /
        // lower n_ctx) so the operator can recover without consulting the
        // issue tracker.
        assert!(c
            .hint
            .as_deref()
            .unwrap_or("")
            .contains("smaller compactor"));
    }

    #[test]
    fn load_projection_includes_unknown_size_models_in_summary() {
        // A profile model that doesn't appear in the lms catalog (yet)
        // shouldn't poison the verdict — but its presence should still
        // surface in the summary so the operator knows it'll load too.
        let c = classify_load_projection(
            10.0,
            3.0,
            &pending(&["google/gemma-3-4b ~3.0 GB", "fresh-download (size unknown)"]),
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
        assert_eq!(
            parse_pages_field("                  1234567."),
            Some(1234567)
        );
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
        let r = run(true);
        // 32 static checks via run(true) (28 always-on + 4 openclaw), incl.
        // build-identity [#1129] + docker-runtime [#680] + runtime version + load projection +
        // daemon reachable + darkmux-version-vs-latest-release [#13] +
        // crew-role-prompt-coverage [#141] + flow-sink-health [#170] +
        // machine_id + orchestrator [#167] + openai-base-url-conflict [#5] +
        // audit-integrity [#163] + recommendation-drift +
        // recommended-profile-not-shadowed [#159] + utility-model-binding
        // [#590] + role-model-pin-drift [#160] + legacy-mission-layout [#148]
        // + beat-33-crew-dir [Beat 33 directory flatten] + role-tool-vocab
        // [#340] + legacy-compaction-extras [#380] + redis-config [#661] +
        // docker-runtime [#680] + audit-write-drops [#877] + serve-daemon-auth
        // [#881] + fleet.mode [#933]) + one per active eureka rule. Every check
        // should appear regardless of environment — even if the underlying
        // probe couldn't read state.
        let expected = 33 + darkmux_eureka::all_rules().len();
        assert_eq!(r.checks.len(), expected);
    }

    // ─── check_daemon_auth (#881) ─────────────────────────────────────
    #[test]
    fn daemon_auth_status_arms() {
        // Token set → Pass, no hint.
        let (s, _msg, hint) = daemon_auth_status(true);
        assert_eq!(s, Status::Pass);
        assert!(hint.is_none());
        // No token → still Pass (loopback-only is the SAFE default; the bind
        // gate enforces safety), but with an actionable enabling hint.
        let (s, _msg, hint) = daemon_auth_status(false);
        assert_eq!(s, Status::Pass, "no-token is not a Warn — don't cry wolf on the safe default");
        let h = hint.expect("the no-token arm gives an enabling hint");
        assert!(
            h.contains("darkmux-serve-token") || h.contains("DARKMUX_SERVE_TOKEN"),
            "hint should name how to set the token: {h}"
        );
    }

    // ─── check_utility_model_binding (#590) ───────────────────────────
    fn lm(identifier: &str, model: &str) -> darkmux_types::LoadedModel {
        darkmux_types::LoadedModel {
            identifier: identifier.into(),
            model: model.into(),
            status: "loaded".into(),
            size: "3 GB".into(),
            context: 4096,
        }
    }

    #[test]
    fn utility_binding_unregistered_passes_with_setup_hint() {
        let c = super::utility_binding_status(None, None);
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("no machine utility model"));
        assert!(c.hint.unwrap().contains("internal"));
    }

    #[test]
    fn utility_binding_registered_but_lms_unreachable_warns() {
        let c = super::utility_binding_status(Some("darkmux:util-4b"), None);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("couldn't query LMStudio"));
    }

    #[test]
    fn utility_binding_registered_and_loaded_passes() {
        // Match by modelKey...
        let loaded = vec![lm("darkmux:util-4b", "util-4b"), lm("worker", "worker-35b")];
        let c = super::utility_binding_status(Some("util-4b"), Some(&loaded));
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("registered and loaded"));
        // ...or by the namespaced identifier.
        let c2 = super::utility_binding_status(Some("darkmux:util-4b"), Some(&loaded));
        assert_eq!(c2.status, Status::Pass);
    }

    #[test]
    fn utility_binding_registered_but_not_loaded_warns() {
        let loaded = vec![lm("worker", "worker-35b")];
        let c = super::utility_binding_status(Some("util-4b"), Some(&loaded));
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("registered but NOT loaded"));
        assert!(c.hint.unwrap().contains("Load it before dispatching"));
    }

    // ─── parse_semver / classify_version_vs_latest (issue #13) ───────────
    const VERSION_CHECK_NAME: &str = "darkmux version vs latest release";

    #[test]
    fn parse_semver_strips_v_prefix_and_metadata() {
        assert_eq!(parse_semver("0.4.0"), Some((0, 4, 0)));
        assert_eq!(parse_semver("v0.4.0"), Some((0, 4, 0)));
        assert_eq!(parse_semver("v1.2.3"), Some((1, 2, 3)));
        // Pre-release suffix on patch is stripped to the leading digits.
        assert_eq!(parse_semver("0.4.0-beta.1"), Some((0, 4, 0)));
        assert_eq!(parse_semver("1.0.5-rc1+build.42"), Some((1, 0, 5)));
        // Trim whitespace, tolerate "v" + spaces.
        assert_eq!(parse_semver("  v0.4.0\n"), Some((0, 4, 0)));
        // Malformed inputs → None (caller renders a skipped check).
        assert_eq!(parse_semver("not-a-version"), None);
        assert_eq!(parse_semver("0.4"), None);
        assert_eq!(parse_semver(""), None);
    }

    #[test]
    fn version_vs_latest_passes_when_installed_matches_latest() {
        let c = classify_version_vs_latest("0.4.0", "0.4.0", VERSION_CHECK_NAME);
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("v0.4.0"));
        assert!(c.message.contains("latest released: v0.4.0"));
        assert!(c.hint.is_none());
    }

    #[test]
    fn version_vs_latest_passes_when_installed_is_ahead() {
        // Dev build ahead of last release — Pass (no upgrade nag).
        let c = classify_version_vs_latest("0.5.0", "0.4.0", VERSION_CHECK_NAME);
        assert_eq!(c.status, Status::Pass);
    }

    #[test]
    fn version_vs_latest_warns_when_minor_behind() {
        let c = classify_version_vs_latest("0.3.5", "0.4.0", VERSION_CHECK_NAME);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("minor/patch"));
        let hint = c.hint.as_deref().unwrap_or("");
        assert!(hint.contains("git pull"));
        assert!(hint.contains("DARKMUX_CHECK_UPDATES=0"));
    }

    #[test]
    fn version_vs_latest_warns_when_patch_behind() {
        let c = classify_version_vs_latest("0.4.0", "0.4.3", VERSION_CHECK_NAME);
        assert_eq!(c.status, Status::Warn);
    }

    #[test]
    fn version_vs_latest_fails_when_major_behind() {
        let c = classify_version_vs_latest("0.4.0", "1.0.0", VERSION_CHECK_NAME);
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("major version behind"));
        assert!(c.message.contains("schema break"));
    }

    #[test]
    fn version_vs_latest_skips_when_either_side_unparseable() {
        let c = classify_version_vs_latest("not-a-version", "0.4.0", VERSION_CHECK_NAME);
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("skipped"));
        assert!(c.message.contains("couldn't parse semver"));
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
        let r = run(true);
        assert!(r.checks.iter().any(|c| c.name.contains("platform")));
    }

    #[test]
    fn agent_role_check_always_present() {
        let r = run(true);
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
                let response =
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
                let _ = stream.write_all(response.as_bytes());
            }
        });

        // Give the server a moment to start
        thread::sleep(Duration::from_millis(50));

        // Run the check against our mock server
        let check = check_daemon_reachable_impl("127.0.0.1", port);

        // Assert Pass status
        assert_eq!(
            check.status,
            Status::Pass,
            "daemon reachable check should pass when health returns 200. Got message: {}",
            check.message
        );
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
        assert!(check
            .hint
            .as_ref()
            .unwrap_or(&String::new())
            .contains("darkmux serve"));
    }

    // ─── check_beat33_legacy_crew_dir ─────────────────────────────────
    //
    // The doctor check detects an operator on the pre-Beat-33
    // `<root>/crew/{subdirs}` layout and emits an mv-script. Tests run
    // serially because they mutate DARKMUX_CREW_DIR — the env var is
    // process-global.

    /// RAII: redirect DARKMUX_CREW_DIR to a TempDir for the test's duration.
    struct CrewRootGuard {
        prev: Option<String>,
        tmp: tempfile::TempDir,
    }

    impl CrewRootGuard {
        fn new() -> Self {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let prev = std::env::var("DARKMUX_CREW_DIR").ok();
            // SAFETY: tests using this guard MUST be #[serial].
            unsafe {
                std::env::set_var("DARKMUX_CREW_DIR", tmp.path());
            }
            Self { prev, tmp }
        }
        fn path(&self) -> &std::path::Path {
            self.tmp.path()
        }
    }

    impl Drop for CrewRootGuard {
        fn drop(&mut self) {
            // SAFETY: tests using this guard MUST be #[serial].
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                    None => std::env::remove_var("DARKMUX_CREW_DIR"),
                }
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn beat33_legacy_crew_dir_passes_when_no_crew_subdir_exists() {
        let _guard = CrewRootGuard::new();
        let check = check_beat33_legacy_crew_dir();
        assert_eq!(check.status, Status::Pass);
        assert!(check.message.contains("flattened layout"));
        assert!(check.hint.is_none());
    }

    #[serial_test::serial]
    #[test]
    fn beat33_legacy_crew_dir_passes_when_crew_dir_is_empty() {
        let guard = CrewRootGuard::new();
        // <root>/crew/ exists but has nothing inside — operator may have
        // created it manually, leave alone.
        std::fs::create_dir_all(guard.path().join("crew")).unwrap();
        let check = check_beat33_legacy_crew_dir();
        assert_eq!(check.status, Status::Pass);
        assert!(check.message.contains("holds no promoted subdirs"));
    }

    #[serial_test::serial]
    #[test]
    fn beat33_legacy_crew_dir_warns_with_mv_script_when_subdirs_present() {
        let guard = CrewRootGuard::new();
        // Seed the legacy layout with the subdirs an upgrading operator
        // would actually have.
        std::fs::create_dir_all(guard.path().join("crew").join("roles")).unwrap();
        std::fs::create_dir_all(guard.path().join("crew").join("missions")).unwrap();
        std::fs::write(guard.path().join("crew").join("role-model-pins.json"), "{}").unwrap();

        let check = check_beat33_legacy_crew_dir();
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("operator state still under"));
        assert!(check.message.contains("missions"));
        assert!(check.message.contains("roles"));
        assert!(check.message.contains("role-model-pins.json"));

        let hint = check
            .hint
            .as_ref()
            .expect("warn must carry an mv-script hint");
        // Script must be operator-runnable: mv -n (no-clobber) for safety,
        // plus a final rmdir to clean up the now-empty parent.
        assert!(hint.contains("mv -n"));
        assert!(hint.contains("/crew/roles"));
        assert!(hint.contains("/crew/missions"));
        assert!(hint.contains("/crew/role-model-pins.json"));
        assert!(hint.contains("rmdir"));
        // Operator-sovereignty: the hint explicitly notes that nothing is
        // urgent (loader's dual-read keeps the legacy layout working).
        // Strip newlines before substring-match so rustfmt re-wrapping
        // doesn't move the assertion's goalposts.
        assert!(hint.replace('\n', " ").contains("no rush"));
    }

    #[serial_test::serial]
    #[test]
    fn beat33_legacy_crew_dir_only_reports_promoted_subdirs() {
        let guard = CrewRootGuard::new();
        // Create only one promoted subdir + one NON-promoted subdir;
        // doctor should only mention the promoted one.
        std::fs::create_dir_all(guard.path().join("crew").join("roles")).unwrap();
        std::fs::create_dir_all(guard.path().join("crew").join("operator-private-stuff")).unwrap();

        let check = check_beat33_legacy_crew_dir();
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("roles"));
        assert!(
            !check.message.contains("operator-private-stuff"),
            "doctor must not recommend touching operator-authored subdirs"
        );
        let hint = check.hint.unwrap();
        assert!(
            !hint.contains("operator-private-stuff"),
            "mv script must not propose moving operator-authored subdirs"
        );
    }

    // ─── Sprint-G: OC-active gate on doctor runtime checks ──

    /// Helper that points `DARKMUX_OPENCLAW_CONFIG` at a non-existent
    /// path for the test's duration so `default_openclaw_config()`
    /// resolves to a missing file (the "openclaw not configured"
    /// signal Sprint-G keys on).
    struct OpenclawConfigGuard {
        prev: Option<String>,
        _tmp: tempfile::TempDir,
    }

    impl OpenclawConfigGuard {
        fn missing() -> Self {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let bogus = tmp.path().join("does-not-exist.json");
            let prev = std::env::var("DARKMUX_OPENCLAW_CONFIG").ok();
            // SAFETY: tests using this guard MUST be #[serial].
            unsafe {
                std::env::set_var("DARKMUX_OPENCLAW_CONFIG", &bogus);
            }
            Self { prev, _tmp: tmp }
        }
    }

    impl Drop for OpenclawConfigGuard {
        fn drop(&mut self) {
            // SAFETY: tests using this guard MUST be #[serial].
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_OPENCLAW_CONFIG", v),
                    None => std::env::remove_var("DARKMUX_OPENCLAW_CONFIG"),
                }
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_runtime_command_skips_when_openclaw_not_configured() {
        let _guard = OpenclawConfigGuard::missing();
        let check = check_runtime_command();
        assert_eq!(
            check.status,
            Status::Pass,
            "no openclaw config → check must pass-with-skip, not warn"
        );
        assert!(
            check.message.contains("skipped"),
            "expected `skipped` in message; got: {}",
            check.message
        );
        assert!(
            check.message.contains("openclaw not configured"),
            "expected `openclaw not configured` framing; got: {}",
            check.message
        );
    }

    #[serial_test::serial]
    #[test]
    fn check_runtime_version_skips_when_openclaw_not_configured() {
        let _guard = OpenclawConfigGuard::missing();
        let check = check_runtime_version();
        assert_eq!(
            check.status,
            Status::Pass,
            "no openclaw config → version check must pass-with-skip"
        );
        assert!(
            check.message.contains("skipped"),
            "expected `skipped` in message; got: {}",
            check.message
        );
    }

    // ─── #380: check_legacy_compaction_extras tests ─────────────

    /// Helper that points `DARKMUX_PROFILES` at a tempdir for the test's
    /// duration so `load_registry()` reads from a controlled path.
    struct ConfigPathGuard {
        prev: Option<String>,
        _tmp: tempfile::TempDir,
    }

    impl ConfigPathGuard {
        fn at_tempfile(filename: &str) -> (Self, std::path::PathBuf) {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let path = tmp.path().join(filename);
            // Ensure parent dir exists
            std::fs::create_dir_all(tmp.path()).unwrap();
            let prev = std::env::var("DARKMUX_PROFILES").ok();
            // SAFETY: tests using this guard MUST be #[serial].
            unsafe {
                std::env::set_var("DARKMUX_PROFILES", &path);
            }
            (Self { prev, _tmp: tmp }, path)
        }
    }

    impl Drop for ConfigPathGuard {
        fn drop(&mut self) {
            // SAFETY: tests using this guard MUST be #[serial].
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_PROFILES", v),
                    None => std::env::remove_var("DARKMUX_PROFILES"),
                }
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_legacy_compaction_extras_warns_when_present() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        // Write a profile with extras.customInstructions set
        let registry_json = r#"{
            "profiles": {
                "test-profile": {
                    "models": [{"id": "primary-x", "n_ctx": 100000, "role": "primary"}],
                    "runtime": {
                        "compaction": {
                            "customInstructions": "some legacy value",
                            "strategy": "narrative"
                        }
                    }
                }
            }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_legacy_compaction_extras();
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("test-profile"));
        assert!(check.message.contains("customInstructions"));
        let hint = check.hint.as_deref().unwrap_or("");
        assert!(
            hint.contains("custom_instructions"),
            "hint must mention typed custom_instructions field"
        );
    }

    #[serial_test::serial]
    #[test]
    fn check_legacy_compaction_extras_passes_when_absent() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        // Write a profile with empty/absent extras
        let registry_json = r#"{
            "profiles": {
                "clean-profile": {
                    "models": [{"id": "primary-x", "n_ctx": 100000, "role": "primary"}],
                    "runtime": {
                        "compaction": {}
                    }
                }
            }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_legacy_compaction_extras();
        assert_eq!(check.status, Status::Pass);
        assert!(check.message.contains("no legacy compaction extras"));
    }

    #[serial_test::serial]
    #[test]
    fn check_legacy_compaction_extras_handles_multiple_keys() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        // Write a profile with multiple legacy keys
        let registry_json = r#"{
            "profiles": {
                "multi-key-profile": {
                    "models": [{"id": "primary-x", "n_ctx": 100000, "role": "primary"}],
                    "runtime": {
                        "compaction": {
                            "mode": "balanced",
                            "maxHistoryShare": 0.7,
                            "customInstructions": "keep important stuff",
                            "strategy": "narrative"
                        }
                    }
                }
            }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_legacy_compaction_extras();
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("multi-key-profile"));
        // All four legacy keys should be listed
        assert!(check.message.contains("mode"));
        assert!(check.message.contains("maxHistoryShare"));
        assert!(check.message.contains("customInstructions"));
    }

    #[serial_test::serial]
    #[test]
    fn check_legacy_compaction_extras_passes_when_no_runtime() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        // Write a profile without runtime section at all
        let registry_json = r#"{
            "profiles": {
                "no-runtime-profile": {
                    "models": [{"id": "primary-x", "n_ctx": 100000, "role": "primary"}]
                }
            }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_legacy_compaction_extras();
        assert_eq!(check.status, Status::Pass);
    }

    #[serial_test::serial]
    #[test]
    fn check_legacy_compaction_extras_passes_when_no_compaction() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        // Write a profile with runtime but no compaction
        let registry_json = r#"{
            "profiles": {
                "no-compaction-profile": {
                    "models": [{"id": "primary-x", "n_ctx": 100000, "role": "primary"}],
                    "runtime": {}
                }
            }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_legacy_compaction_extras();
        assert_eq!(check.status, Status::Pass);
    }

    // ─── #332: doctor OC-config probe normalization ─────────────────

    /// Helper that points `DARKMUX_OPENCLAW_CONFIG` at a path the
    /// caller chooses (so the test can ALSO supply contents for the
    /// resolver to read). Distinct from `OpenclawConfigGuard::missing`
    /// which points at a known-missing path.
    struct OpenclawConfigPointGuard {
        prev: Option<String>,
        _tmp: tempfile::TempDir,
    }

    impl OpenclawConfigPointGuard {
        /// Point env var at a file under a freshly-created tempdir.
        /// Returns the guard + the full path the env var was set to.
        fn at_tempfile(filename: &str) -> (Self, std::path::PathBuf) {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let path = tmp.path().join(filename);
            let prev = std::env::var("DARKMUX_OPENCLAW_CONFIG").ok();
            // SAFETY: tests using this guard MUST be #[serial].
            unsafe {
                std::env::set_var("DARKMUX_OPENCLAW_CONFIG", &path);
            }
            (Self { prev, _tmp: tmp }, path)
        }
    }

    impl Drop for OpenclawConfigPointGuard {
        fn drop(&mut self) {
            // SAFETY: tests using this guard MUST be #[serial].
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_OPENCLAW_CONFIG", v),
                    None => std::env::remove_var("DARKMUX_OPENCLAW_CONFIG"),
                }
            }
        }
    }

    /// #332 — `check_agent_role_definitions` must honor
    /// `DARKMUX_OPENCLAW_CONFIG`. Pre-fix it hardcoded
    /// `~/.openclaw/openclaw.json` and silently probed the wrong
    /// file when the env var was set elsewhere.
    ///
    /// Test: point env var at a custom path that doesn't exist on
    /// disk. The check must report the env-var-resolved path in its
    /// "skipping" message (proving the resolver was consulted), NOT
    /// the hardcoded `~/.openclaw/openclaw.json`.
    #[serial_test::serial]
    #[test]
    fn check_agent_role_definitions_honors_openclaw_config_env_var() {
        let (_guard, env_path) = OpenclawConfigPointGuard::at_tempfile("custom-oc.json");
        // env_path does NOT exist on disk — the missing-config skip
        // path fires. Its message names the resolved path.
        let check = check_agent_role_definitions();
        assert_eq!(
            check.status,
            Status::Pass,
            "missing custom OC config → pass-with-skip"
        );
        let expected_fragment = env_path.display().to_string();
        assert!(
            check.message.contains(&expected_fragment),
            "check message must name the env-var-resolved path `{expected_fragment}` \
             (pre-fix it hardcoded `~/.openclaw/openclaw.json`); got: {}",
            check.message
        );
        // Defensive: ensure the message does NOT mention the
        // hardcoded default path — that would be the pre-fix bug.
        // (Skip this assertion if HOME happens to contain the same
        // tempdir prefix, which can't happen here.)
        let hardcoded = format!(
            "{}/.openclaw/openclaw.json",
            dirs::home_dir().unwrap().display()
        );
        assert!(
            !check.message.contains(&hardcoded),
            "check message must not leak the hardcoded default path; got: {}",
            check.message
        );
    }

    /// Sibling: with the env var pointing at a REAL openclaw.json,
    /// the check reads it (success path). Verifies the resolver isn't
    /// just used for the skip path — the actual file read also goes
    /// through the env-var-resolved location.
    #[serial_test::serial]
    #[test]
    fn check_agent_role_definitions_reads_env_var_path_when_file_exists() {
        let (_guard, env_path) = OpenclawConfigPointGuard::at_tempfile("custom-oc.json");
        // Write a minimal valid openclaw.json (no agents.list means
        // the check returns its "no darkmux/* agents" pass message,
        // not an error).
        std::fs::write(&env_path, r#"{"agents":{"list":[]}}"#).unwrap();
        let check = check_agent_role_definitions();
        // Either Pass with "no darkmux/* agents" message, or some
        // other non-error outcome. The important assertion: the
        // check didn't bail with the missing-config skip message,
        // which would prove it read the env-var path.
        assert!(
            !check.message.contains("on disk — skipping"),
            "with a real file at the env-var path, the missing-config skip path \
             must NOT fire; got: {}",
            check.message
        );
    }

    // ─── #387: --include-openclaw flag gate tests ─────────────

    /// Default `run(false)` must NOT include any OC-specific checks.
    /// The four gated checks are: role-model pin drift, runtime command,
    /// runtime version, agent role scaffolds.
    #[test]
    fn doctor_default_skips_openclaw_checks() {
        let report = run(false);

        // Collect names of checks that match OC-specific patterns.
        let oc_names: Vec<&str> = report
            .checks
            .iter()
            .filter(|c| {
                let n = c.name.to_lowercase();
                n.contains("runtime command")
                    || n.contains("runtime version")
                    || n.contains("role-model pin drift")
                    || n.contains("agent role scaffolds")
            })
            .map(|c| c.name.as_str())
            .collect();

        assert!(
            oc_names.is_empty(),
            "default doctor (include_openclaw=false) must not run OC checks; \
             found: {:?}",
            oc_names
        );

        // (#393) The schema-isolation success criterion also requires that
        // OC-reading eureka rules (ctx-window-mismatch, n-ctx-exceeds-max,
        // etc.) don't emit their "no ~/.openclaw/openclaw.json — skipping"
        // messages in default mode. The eureka_checks filter strips these
        // Skipped("openclaw...") verdicts when include_openclaw=false.
        let openclaw_mentions: Vec<&str> = report
            .checks
            .iter()
            .filter(|c| {
                c.message.to_lowercase().contains("openclaw")
                    || c.name.to_lowercase().contains("openclaw")
                    || c.hint
                        .as_deref()
                        .map(|h| h.to_lowercase().contains("openclaw"))
                        .unwrap_or(false)
            })
            .map(|c| c.name.as_str())
            .collect();

        assert!(
            openclaw_mentions.is_empty(),
            "default doctor must not surface any check that mentions openclaw \
             (schema-isolation success criterion); found: {:?}",
            openclaw_mentions
        );
    }

    /// `run(true)` must include all four OC-specific checks.
    #[test]
    fn doctor_with_include_openclaw_runs_them() {
        let report = run(true);

        // Collect names of checks that match OC-specific patterns.
        let oc_names: Vec<&str> = report
            .checks
            .iter()
            .filter(|c| {
                let n = c.name.to_lowercase();
                n.contains("runtime command")
                    || n.contains("runtime version")
                    || n.contains("role-model pin drift")
                    || n.contains("agent role scaffolds")
            })
            .map(|c| c.name.as_str())
            .collect();

        assert!(
            oc_names.len() == 4,
            "doctor with include_openclaw=true must run all 4 OC checks; \
             found {} (expected 4): {:?}",
            oc_names.len(),
            oc_names
        );
    }
}
