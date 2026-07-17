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
//! Checks are intentionally scoped to what darkmux can verify natively.

use anyhow::Result;
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

/// (#1426) A darkmux skill compiled into the binary, threaded into the doctor
/// from the root crate (which owns the `include_str!` embed) so this crate
/// stays a pure evaluator. `content` is the reference `SKILL.md` body; the
/// freshness check byte-compares it against the installed copy, so there is no
/// hash-algorithm agreement to keep in sync between the producer (the root
/// crate) and the evaluator (this crate).
///
/// This is the caller-supplied-check-input pattern for doctor: `run()` gathers
/// everything doctor can read for itself, and a check that needs root-crate
/// state (which this crate cannot depend on) is invoked separately by `main.rs`
/// with the state passed in and its result appended to the report. It is the
/// same shape as `probe_remote_endpoints`, but taking an input.
#[derive(Debug, Clone)]
pub struct EmbeddedSkill {
    pub name: String,
    pub content: String,
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

/// Name of the daemon-reachability check. Like the build line, a PASSING
/// daemon-reachable row bypasses the issues-only consolidation — its message
/// is the viewer's locator (loopback + tailnet URLs), which the operator runs
/// `doctor` to find; collapsing it into "N more checks passed" would hide the
/// one thing they came for. A Warn/Fail (daemon down) prints via the normal
/// problem path regardless.
const DAEMON_CHECK_NAME: &str = "daemon reachable";

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

pub fn run() -> DoctorReport {
    let checks = vec![
        check_build_info(),
        check_profile_registry(),
        check_crews_residue(),
        check_mission_config_registry(),
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
        check_remote_endpoint_credentials(),
        check_env_masks_config(),
        check_binary_split_brain(),
        check_audit_integrity(),
        check_audit_write_drops(),
        check_daemon_auth(),
        check_utility_model_binding(),
        check_role_tool_vocab_typos(),
        check_beat33_legacy_crew_dir(),
        check_legacy_mission_layout(),
        check_legacy_compaction_extras(),
    ];
    let checks = [checks, eureka_checks()].concat();
    DoctorReport { checks }
}

/// Name of the installed-skills freshness check (#1426).
const SKILLS_FRESHNESS_CHECK_NAME: &str = "darkmux skills freshness";

/// (#1426) Compare the installed `darkmux-*` skill directories against the
/// binary's embedded copies and warn when they drift, so an operator who
/// upgraded darkmux but never re-ran `darkmux init` learns their skills are
/// stale from the structural surface rather than by memory. This closes the
/// upgrade loop: `brew upgrade` then doctor warns then `darkmux init` then
/// clean.
///
/// Scope is the `darkmux-*` namespace ONLY. A non-darkmux entry in the skills
/// directory is the operator's own state and is never inspected or reported
/// (the namespace contract). Two conditions now drive the WARN:
///   1. an installed `darkmux-*` skill whose content differs from the embedded
///      copy (stale — an older darkmux installed it), and
///   2. an installed `darkmux-*` skill the binary no longer bundles (RETIRED —
///      a dead skill left on disk).
///
/// (#1449) The retired case was previously surfaced informationally but did NOT
/// warn, on the rationale that `darkmux init` couldn't fix it so a warning would
/// be noise. That reversed once `init` gained a prune pass (same #1449 batch):
/// the fix IS now actionable (`darkmux init` removes the retired dir), and these
/// artifacts are NOT inert — `darkmux-swap-stack` is a LIVE skill teaching
/// `darkmux swap` + `darkmux status`, both retired verbs an agent will invoke.
/// This is exactly #1449's class: the generator was fixed, but the installed
/// artifact still teaches dead verbs. So a retired skill now warns, naming it,
/// with `darkmux init` as the fix.
///
/// One condition stays informational (no warn): an embedded skill that is not
/// installed — a minimal install is a legitimate operator choice, not drift, and
/// nothing is actively wrong on disk.
///
/// Pure evaluator: `targets` (the install directories) and `embedded` (the
/// reference set) are supplied by the caller (`main.rs`, the root crate that
/// owns the `include_str!` embed), because this crate cannot depend on the root
/// binary crate where the skills live.
/// `maintainer_exclusions` (#1449): `darkmux-*` skills that are deliberately not
/// embedded (maintainer-only, e.g. `darkmux-point-release`) and so must NOT be
/// reported retired when a maintainer has them installed from a source checkout.
/// Supplied by `main.rs` from `skills::MAINTAINER_ONLY_SKILLS` — the doctor crate
/// can't depend on the root binary crate where that list lives.
pub fn check_installed_skills_freshness(
    targets: &[PathBuf],
    embedded: &[EmbeddedSkill],
    maintainer_exclusions: &[String],
) -> Check {
    let mut matched = 0usize;
    let mut stale: Vec<String> = Vec::new();
    let mut not_installed: Vec<String> = Vec::new();

    for skill in embedded {
        // Defense in depth: the embedded set is `darkmux-*` by construction,
        // but assert the namespace contract at the point of read anyway.
        if !skill.name.starts_with("darkmux-") {
            continue;
        }
        match installed_skill_content(targets, &skill.name) {
            Some(content) if content == skill.content => matched += 1,
            Some(_) => stale.push(skill.name.clone()),
            None => not_installed.push(skill.name.clone()),
        }
    }

    // Installed `darkmux-*` skills the binary no longer ships (retired). Scanned
    // straight from disk, filtered HARD to the `darkmux-*` namespace so a
    // non-darkmux user skill is never even looked at.
    let embedded_names: std::collections::HashSet<&str> =
        embedded.iter().map(|s| s.name.as_str()).collect();
    let mut retired: Vec<String> = Vec::new();
    for target in targets {
        let Ok(entries) = std::fs::read_dir(target) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // NEVER inspect non-`darkmux-*` entries — user state, off-limits.
            if !name.starts_with("darkmux-") {
                continue;
            }
            if !path.is_dir() || !path.join("SKILL.md").exists() {
                continue;
            }
            // (#1449) Maintainer-only skills are deliberately not embedded but
            // legitimately installed on a source checkout — never "retired".
            if maintainer_exclusions.iter().any(|e| e == name) {
                continue;
            }
            let owned = name.to_string();
            if !embedded_names.contains(name) && !retired.contains(&owned) {
                retired.push(owned);
            }
        }
    }

    stale.sort();
    not_installed.sort();
    retired.sort();

    let mut detail: Vec<String> = vec![format!("{matched} up to date")];
    if !stale.is_empty() {
        detail.push(format!("{} stale ({})", stale.len(), stale.join(", ")));
    }
    if !not_installed.is_empty() {
        detail.push(format!(
            "{} embedded but not installed ({})",
            not_installed.len(),
            not_installed.join(", ")
        ));
    }
    if !retired.is_empty() {
        detail.push(format!(
            "{} installed but no longer bundled ({})",
            retired.len(),
            retired.join(", ")
        ));
    }
    let message = format!("darkmux-* skills: {}", detail.join("; "));

    // (#1449) WARN when EITHER a skill is stale (older darkmux installed it) OR a
    // retired skill is left on disk (a live dead-verb skill an agent will
    // invoke). Both are now fixed by `darkmux init` — stale ones refresh, retired
    // ones prune. The "up to date" pass path holds when neither is present.
    if stale.is_empty() && retired.is_empty() {
        Check {
            name: SKILLS_FRESHNESS_CHECK_NAME.into(),
            status: Status::Pass,
            message,
            hint: None,
        }
    } else {
        let hint = if !retired.is_empty() && stale.is_empty() {
            format!(
                "retired skill(s) still installed ({}) teach dead verbs; run `darkmux init` to prune",
                retired.join(", ")
            )
        } else {
            "installed from an older darkmux; run `darkmux init` to refresh (and prune retired skills)"
                .into()
        };
        Check {
            name: SKILLS_FRESHNESS_CHECK_NAME.into(),
            status: Status::Warn,
            message,
            hint: Some(hint),
        }
    }
}

/// Read the installed `SKILL.md` body for a skill named `name`, searching each
/// install target in order and returning the first hit. `None` = not installed
/// in any target (or the directory exists but its `SKILL.md` does not).
/// An existing-but-unreadable `SKILL.md` returns `Some("")`, which will not
/// match the embedded copy and is therefore reported as stale (a broken or
/// partial install that `darkmux init` fixes). (#1426)
fn installed_skill_content(targets: &[PathBuf], name: &str) -> Option<String> {
    for target in targets {
        let skill_md = target.join(name).join("SKILL.md");
        if skill_md.exists() {
            return Some(std::fs::read_to_string(&skill_md).unwrap_or_default());
        }
    }
    None
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
/// missions,phases,crews,skills,role-model-pins.json}` layout
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
    let promoted_subdirs = ["roles", "missions", "phases", "crews", "skills"];
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
/// was the `darkmux dispatch: tool_palette filtered to []`
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
                        "Load it before dispatching — compaction summons the utility model mid-dispatch; if it isn't resident the compactor call fails. Run `lms load <id>`, or register it as `internal.utility` so a dispatch loads it automatically. (#590)".into(),
                    ),
                }
            }
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
    // Provenance is presence-only (env-set / config-set / neither); the displayed
    // token comes from `raw`. (#934 will centralize this env/config/default
    // attribution into a config_access helper so every finding shares it.)
    let env_set = std::env::var("DARKMUX_FLEET_MODE")
        .ok()
        .is_some_and(|s| !s.trim().is_empty());
    let cfg_set = DarkmuxConfig::load_resolved()
        .fleet
        .and_then(|f| f.mode)
        .is_some_and(|s| !s.trim().is_empty());
    let raw = darkmux_types::config_access::fleet_mode_raw();
    let provenance = if env_set {
        "from DARKMUX_FLEET_MODE env"
    } else if cfg_set {
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
            format!("OPENAI_BASE_URL points at darkmux's LMStudio ({lms_url}) — darkmux's loaded models reach downstream agents"),
            None,
        ),
        Some(b) => (
            Status::Warn,
            format!("OPENAI_BASE_URL={b} does not point at darkmux's LMStudio ({lms_url})"),
            Some(
                "darkmux doesn't set or manage OPENAI_BASE_URL — darkmux loads models into the LMStudio at lmstudio_url. OpenAI-compatible agents reading this env var talk to the other endpoint, so they won't see the models darkmux loaded. Point OPENAI_BASE_URL at darkmux's LMStudio (or unset it) if you want those agents to reach darkmux's models. (If it's a reverse proxy fronting the SAME LMStudio, this warning is benign.) (#5)".into(),
            ),
        ),
    }
}

/// (#5) Warn when a shell-exported `OPENAI_BASE_URL` would defeat darkmux's model loading
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

/// (#85/#91) Surface profile models declaring a remote endpoint
/// (`ModelEndpoint`, #1187/#1177) whose auth credential isn't actually
/// resolvable. Without this check, a missing or misconfigured Keychain item
/// only surfaces at runtime — the FIRST dispatch using that profile model
/// bails loud (see `remote_auth_header` in darkmux-crew), which is correct
/// but late; a new-user setup mistake sits invisible until they happen to
/// dispatch against it. Read-only: never touches the secret VALUE, only
/// whether the named Keychain item exists (mirrors `remote_auth_header`'s
/// own `security find-generic-password -s <keychain>` invocation exactly,
/// so this validates the SAME lookup the real dispatch path performs, not
/// an approximation of it — no `-a $USER`, no `-w`).
fn check_remote_endpoint_credentials() -> Check {
    let name = "remote endpoint credentials";
    let registry = match profiles::load_registry(None) {
        Ok(r) => r,
        Err(e) => {
            return Check {
                name: name.into(),
                status: Status::Warn,
                message: format!(
                    "can't check remote endpoint credentials (profile registry load failed: {e})"
                ),
                hint: None,
            };
        }
    };

    let mut problems: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for (profile_name, profile) in &registry.registry.profiles {
        for model in &profile.models {
            let Some(ep) = model.endpoint.as_ref() else {
                continue;
            };
            let Some(auth) = ep.auth.as_ref() else {
                continue;
            };
            if auth.auth_type.is_none() {
                continue;
            }
            checked += 1;
            // (#1312) A declared env var (`key_env`) that is PRESENT in this
            // process's environment satisfies the credential — the headless
            // runner sets it from its secret store and the dispatch never reads
            // the Keychain. Present env var ⇒ satisfied, regardless of keychain.
            let env_present = auth
                .key_env
                .as_deref()
                .filter(|v| !v.is_empty())
                .and_then(|v| std::env::var(v).ok())
                .is_some_and(|v| !v.is_empty());
            if env_present {
                continue;
            }
            match auth.keychain.as_deref() {
                None | Some("") => {
                    let via = auth
                        .key_env
                        .as_deref()
                        .filter(|v| !v.is_empty())
                        .map(|v| format!(" (declared env var `{v}` is not set in this environment)"))
                        .unwrap_or_default();
                    problems.push(format!(
                        "profile `{profile_name}` model `{}`: endpoint.auth.type is set \
                         but no credential source resolved — set endpoint.auth.keychain or \
                         export endpoint.auth.key_env{via}",
                        model.id
                    ));
                }
                Some(keychain) if !keychain_item_present(keychain) => {
                    problems.push(format!(
                        "profile `{profile_name}` model `{}`: Keychain item `{keychain}` \
                         not found on this machine",
                        model.id
                    ));
                }
                Some(_) => {}
            }
        }
    }

    if checked == 0 {
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: "no profile models declare a remote endpoint with auth".into(),
            hint: None,
        };
    }

    if problems.is_empty() {
        Check {
            name: name.into(),
            status: Status::Pass,
            message: format!(
                "{checked} remote-endpoint model(s) checked — all credentials resolved \
                 (Keychain item present, or a declared key_env var set)"
            ),
            hint: None,
        }
    } else {
        Check {
            name: name.into(),
            status: Status::Warn,
            message: problems.join("; "),
            hint: Some(
                "Add the missing credential: `security add-generic-password -s <keychain-item-name> -w` \
                 (paste the API key/secret when prompted, matching the item name in endpoint.auth.keychain). \
                 Without it, the FIRST dispatch using that profile model bails loud rather than \
                 failing silently — this check just surfaces it sooner."
                    .into(),
            ),
        }
    }
}

/// Read-only Keychain presence check — never reads the secret VALUE (no
/// `-w`), only whether the named item exists. Deliberately matches
/// `remote_auth_header`'s exact invocation shape (no `-a $USER`) rather
/// than the different `-a $USER -s ...` pattern used elsewhere (e.g. the
/// Redis password check) — this validates what the real dispatch path
/// will actually find, not a differently-scoped lookup.
fn keychain_item_present(name: &str) -> bool {
    Command::new("security")
        .args(["find-generic-password", "-s", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// (#1177) Live endpoint probes — NOT part of [`run`]'s offline check set.
/// Opt-in via `darkmux doctor --probe` because each probe is a real API
/// call: a paid endpoint bills a few tokens per probe. The offline
/// `remote endpoint credentials` check proves the Keychain item EXISTS;
/// this proves the whole chain WORKS — DNS, TLS, credential validity,
/// deployment routing, api-version — by driving one minimal chat
/// completion through the exact URL/auth/POST path a real hosted
/// dispatch uses. One probe per distinct (url, model) pair: profiles
/// that share an endpoint declaration are probed once, not billed once
/// per profile.
pub fn probe_remote_endpoints() -> Vec<Check> {
    const PROBE_TIMEOUT_SECONDS: u32 = 30;
    let registry = match profiles::load_registry(None) {
        Ok(r) => r,
        Err(e) => {
            return vec![Check {
                name: "probe: remote endpoints".into(),
                status: Status::Warn,
                message: format!(
                    "can't probe remote endpoints (profile registry load failed: {e})"
                ),
                hint: None,
            }];
        }
    };

    let mut checks = Vec::new();
    let mut seen: std::collections::HashSet<(String, String, String, String)> =
        std::collections::HashSet::new();

    for (profile_name, profile) in &registry.registry.profiles {
        for model in &profile.models {
            let Some(ep) = model.endpoint.as_ref() else {
                continue;
            };
            if !ep.is_remote() {
                continue;
            }
            // Dedup on EVERYTHING that changes what a probe would verify:
            // url + model + api_version + keychain item. Two profiles hitting
            // the same deployment with DIFFERENT credentials must both probe —
            // credential validity is the feature's whole point.
            let key = (
                ep.url.clone().unwrap_or_default(),
                model.id.clone(),
                ep.api_version.clone().unwrap_or_default(),
                ep.auth
                    .as_ref()
                    .and_then(|a| a.keychain.clone())
                    .unwrap_or_default(),
            );
            if !seen.insert(key) {
                continue; // identical endpoint declaration already probed this run
            }
            let name = format!("probe: {profile_name}/{}", model.id);
            match darkmux_crew::dispatch_internal::probe_remote_endpoint(
                ep,
                &model.id,
                PROBE_TIMEOUT_SECONDS,
            ) {
                Ok(r) => {
                    let served = r
                        .served_model
                        .map(|m| format!(" · served by `{m}`"))
                        .unwrap_or_default();
                    let cost = r
                        .total_tokens
                        .map(|t| format!(" · probe cost {t} tokens"))
                        .unwrap_or_default();
                    checks.push(Check {
                        name,
                        status: Status::Pass,
                        message: format!(
                            "{} — round-trip ok in {}ms{served}{cost}",
                            r.label, r.wall_ms
                        ),
                        hint: None,
                    });
                }
                Err(e) => checks.push(Check {
                    name,
                    status: Status::Fail,
                    message: format!("probe failed: {e:#}"),
                    hint: Some(
                        "The endpoint's own error above is the diagnosis: an auth message means \
                         the Keychain credential is wrong or rotated (re-add with \
                         `security add-generic-password -s <item> -w`); a not-found means the \
                         URL / deployment / api-version is off; a timeout means network. Fix \
                         and re-run `darkmux doctor --probe`."
                            .into(),
                    ),
                }),
            }
        }
    }

    if checks.is_empty() {
        checks.push(Check {
            name: "probe: remote endpoints".into(),
            status: Status::Pass,
            message: "no profile models declare a remote endpoint — nothing to probe".into(),
            hint: None,
        });
    }
    checks
}

/// (#934) Cross-setting coherence: a `DARKMUX_*` env var set in the shell wins
/// LIVE over the matching `config.json` field, so a stale export can silently
/// shadow what the operator configured. We flag ONLY the case with a clean
/// "the operator intentionally configured this" signal — `DARKMUX_REDIS_URL`
/// shadowing an **enabled** `config.redis` block (the #932 trap) — to avoid
/// crying wolf on the common setup (see the rationale on the core below).
fn check_env_masks_config() -> Check {
    env_masks_config_check(&darkmux_types::config::DarkmuxConfig::load_resolved())
}

/// Testable core: the env tier is read live, the config tier is the passed
/// `cfg` — so a serial test drives it with `set_var` + a constructed cfg.
///
/// **Why only Redis** (and not machine_id / orchestrator / lmstudio_url /
/// fleet.mode): a useful masking warning needs a signal that the operator
/// *intentionally* configured the field, else it fires on every post-`init`
/// machine (init writes a default for nearly every field, so "config has a
/// value" is always true). `config.redis.enabled == Some(true)` is that signal —
/// the operator turned the block ON — and it matches `redis_url()`'s Tier-2
/// condition exactly (the default `init` config is `enabled:false` + a default
/// host → assembles NO config Redis → not masked). The other fields lack such a
/// signal: machine_id / orchestrator are env-PRIMARY by design (the docs
/// recommend setting them via env — env-over-config is intended, not a trap),
/// and lmstudio_url / fleet.mode would need default-comparison to tell an
/// operator value from the init default (a later refinement).
fn env_masks_config_check(cfg: &darkmux_types::config::DarkmuxConfig) -> Check {
    let name = "env vs config";
    let env_set = std::env::var("DARKMUX_REDIS_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .is_some_and(|s| !s.is_empty());
    let masked = env_set && cfg.redis.as_ref().is_some_and(|r| r.enabled == Some(true));
    if !masked {
        Check {
            name: name.into(),
            status: Status::Pass,
            message: "no env var is shadowing an enabled config.json block".into(),
            hint: None,
        }
    } else {
        Check {
            name: name.into(),
            status: Status::Warn,
            message: "DARKMUX_REDIS_URL shadows your enabled config.redis block (env wins live — the config Redis settings are silently ignored)".into(),
            hint: Some(
                "The shell DARKMUX_REDIS_URL wins over config.redis at every access, so your config Redis block is inert. Fix EITHER way (darkmux can't infer intent): unset DARKMUX_REDIS_URL to use config.redis, OR set config.redis.enabled=false and rely on the env URL. `darkmux doctor -v` shows the resolved Redis source.".into(),
            ),
        }
    }
}

/// (#934) Cross-setting coherence: `which -a darkmux` resolving to more than one
/// binary at DIFFERENT versions is the brew/cargo split-brain — an interactive
/// shell may run `~/.cargo/bin/darkmux` while a launchd daemon runs
/// `/opt/homebrew/bin/darkmux`, so the daemon serves a different (often older)
/// flow-schema than the CLI. Compares the semver token only (a same-version,
/// different-SHA pair is not a schema split). Best-effort: a probe failure is a
/// Pass (skipped), never a false alarm.
fn check_binary_split_brain() -> Check {
    let name = "darkmux binary";
    let pass = |msg: String| Check {
        name: name.into(),
        status: Status::Pass,
        message: msg,
        hint: None,
    };
    let Ok(out) = std::process::Command::new("which").arg("-a").arg("darkmux").output() else {
        return pass("could not enumerate darkmux on PATH (skipped)".into());
    };
    let mut uniq: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let p = line.trim().to_string();
        if !p.is_empty() && !uniq.contains(&p) {
            uniq.push(p);
        }
    }
    if uniq.len() < 2 {
        return pass(format!(
            "single darkmux on PATH{}",
            uniq.first().map(|p| format!(" ({p})")).unwrap_or_default()
        ));
    }
    // Probe each binary's semver (the `X.Y.Z` token of `darkmux --version`).
    let semver = |p: &str| -> String {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("?")
                    .to_string()
            })
            .unwrap_or_else(|| "?".into())
    };
    let versions: Vec<(String, String)> = uniq.iter().map(|p| (p.clone(), semver(p))).collect();
    let distinct: std::collections::HashSet<&str> =
        versions.iter().map(|(_, v)| v.as_str()).collect();
    if distinct.len() <= 1 {
        return pass(format!("{} darkmux binaries on PATH, same version", uniq.len()));
    }
    let listing = versions
        .iter()
        .map(|(p, v)| format!("{p} = {v}"))
        .collect::<Vec<_>>()
        .join("; ");
    Check {
        name: name.into(),
        status: Status::Warn,
        message: format!(
            "brew/cargo split-brain — {} darkmux binaries at different versions: {}",
            uniq.len(),
            listing
        ),
        hint: Some(
            "An interactive shell and a launchd/service daemon can resolve different darkmux binaries (PATH order differs), so the daemon may serve an older flow-schema than the CLI. Align them: reinstall the stale one (`cargo install --path .` or `brew upgrade darkmux`), or remove the duplicate so one version is on PATH.".into(),
        ),
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
/// without a prompt (`dispatch <role>` fails with *"role X has no
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

/// Parse `tailscale serve status --json` for the tailnet URL that proxies to
/// the local daemon on `port` — i.e. where the viewer is reachable from a phone
/// or other tailnet device. Pure (the JSON is fetched by the caller) so it's
/// unit-tested against a captured fixture. Returns `None` when nothing on the
/// tailnet proxies to our port (tailscale not serving, or serving something
/// else). The serve-status JSON shape: `.Web["<host>:<port>"].Handlers["/"]
/// .Proxy == "http://127.0.0.1:<our-port>"`; the served port picks the scheme
/// (443 → https, else http).
fn parse_tailnet_viewer_url(json: &str, port: u16) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let web = v.get("Web")?.as_object()?;
    let want_loopback = format!("http://127.0.0.1:{port}");
    let want_localhost = format!("http://localhost:{port}");
    for (hostport, cfg) in web {
        let proxies_to_us = cfg
            .get("Handlers")
            .and_then(|h| h.as_object())
            .map(|handlers| {
                handlers.values().any(|h| {
                    h.get("Proxy")
                        .and_then(|p| p.as_str())
                        .map(|p| p == want_loopback || p == want_localhost)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if proxies_to_us {
            // `hostport` is like "macbook-pro.taild82cbb.ts.net:80" — split the
            // trailing port to pick the scheme; default to the bare host on no
            // colon (shouldn't happen, but stay total).
            let (host, served_port) = hostport
                .rsplit_once(':')
                .unwrap_or((hostport.as_str(), "80"));
            let scheme = if served_port == "443" { "https" } else { "http" };
            return Some(format!("{scheme}://{host}/"));
        }
    }
    None
}

/// Best-effort: run `tailscale serve status --json` and parse for the tailnet
/// URL proxying to the local daemon on `port`. `None` on any failure (tailscale
/// absent, not serving, or a non-zero/garbage response) — a missing tailnet URL
/// is never an error, just an absent line in the doctor message.
fn tailnet_viewer_url(port: u16) -> Option<String> {
    let out = std::process::Command::new("tailscale")
        .args(["serve", "status", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_tailnet_viewer_url(&String::from_utf8_lossy(&out.stdout), port)
}

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
                name: DAEMON_CHECK_NAME.into(),
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
                name: DAEMON_CHECK_NAME.into(),
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
            name: DAEMON_CHECK_NAME.into(),
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
            name: DAEMON_CHECK_NAME.into(),
            status: Status::Warn,
            message: format!("daemon at {} not responding to HTTP", addr),
            hint: Some("run `darkmux serve` to start the daemon for live viewing features".into()),
        };
    }

    if response.starts_with("HTTP/1.1 200") {
        // Surface WHERE to open the viewer, not just that the daemon answers:
        // the loopback URL (this machine) + the tailnet URL (phone / other
        // tailnet device) when `tailscale serve` is proxying to this daemon.
        let mut message = format!("reachable · viewer http://{addr}/");
        if let Some(tn) = tailnet_viewer_url(port) {
            message.push_str(&format!(" · phone {tn}"));
        }
        Check {
            name: DAEMON_CHECK_NAME.into(),
            status: Status::Pass,
            message,
            hint: None,
        }
    } else {
        // Port is open but not darkmux (or wrong endpoint).
        let first_line = response.lines().next().unwrap_or("");
        Check {
            name: DAEMON_CHECK_NAME.into(),
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

            // (#1282) The loud surface for what the lenient loader tolerated:
            //   1. entries quarantined at parse (structurally broken — each
            //      with serde's exact field-level error), and
            //   2. LOCAL models missing `n_ctx` (legal at parse; a resolution
            //      error the moment anything tries to load them).
            let mut findings: Vec<String> = loaded
                .registry
                .quarantined
                .iter()
                .map(|q| format!("quarantined {} \"{}\": {}", q.kind, q.name, q.error))
                .collect();
            for (pname, profile) in &loaded.registry.profiles {
                for m in &profile.models {
                    if !m.is_remote() && m.n_ctx.is_none() {
                        findings.push(format!(
                            "profile \"{pname}\" model \"{}\" is local (no endpoint) but \
                             declares no n_ctx — swap/dispatch on it will fail at resolution",
                            m.id
                        ));
                    }
                }
            }

            if findings.is_empty() {
                Check {
                    name: "profile registry".into(),
                    status: Status::Pass,
                    message: format!("{} profile(s) at {}", n, loaded.path.display()),
                    hint: None,
                }
            } else {
                Check {
                    name: "profile registry".into(),
                    status: Status::Warn,
                    message: format!(
                        "{} profile(s) at {}; {}",
                        n,
                        loaded.path.display(),
                        findings.join("; ")
                    ),
                    hint: Some(
                        "fix the named entries in the registry file — healthy entries keep \
                         working; a quarantined or n_ctx-less local entry fails at use with \
                         the same error (#1282)"
                            .into(),
                    ),
                }
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

/// (#1426 ship-2) The `crews` map retired from the profiles schema — a crew is
/// now a DERIVED view of a mission's resourcing, staffed by
/// `darkmux_crew::resourcing`, never declared. A profiles.json still carrying a
/// `crews` key parses fine (the key overflows into `ProfileRegistry.extras`,
/// lenient-on-read) and is harmless residue. This check just NOTES that residue
/// so an operator upgrading from a pre-2.0 profiles.json knows the map no
/// longer does anything and can delete it at leisure. Cheap: it inspects the
/// already-parsed `extras`, no per-entry work.
fn check_crews_residue() -> Check {
    let registry = match profiles::load_registry(None) {
        Ok(r) => r,
        Err(e) => {
            return Check {
                name: "crews residue".into(),
                status: Status::Warn,
                message: format!("can't inspect the registry (load failed: {e})"),
                hint: None,
            };
        }
    };

    if registry.registry.extras.contains_key("crews") {
        Check {
            name: "crews residue".into(),
            // WARN, not Pass-with-hint (gate CONSIDER): a config block that no
            // longer does anything merits the warn tier — the operator should
            // learn their declared crews stopped being read, not skim past it.
            status: Status::Warn,
            message: "a legacy `crews` map is present and DOES NOTHING — it stopped being read \
                      in 2.0"
                .into(),
            hint: Some(
                "the `crews` map retired in 2.0 (#1426) — review staffing is now derived from \
                 the roster profile plus launch-param seat pins (probe_models / judge_model / \
                 verify_model / k). The key is harmless residue; delete it from \
                 ~/.darkmux/profiles.json."
                    .into(),
            ),
        }
    } else {
        Check {
            name: "crews residue".into(),
            status: Status::Pass,
            message: "no legacy crews residue".into(),
            hint: None,
        }
    }
}

/// Parse a `"MAJOR.MINOR"` schema string into its two components — `None`
/// for anything that doesn't fit that shape (extra segments beyond the
/// second are tolerated and ignored, matching `mission_config`'s own
/// lenient major-parse).
fn parse_major_minor(v: &str) -> Option<(u32, u32)> {
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// (#1284 Packet 1) Registered mission configs — enumerates every
/// discoverable mission-config document (`darkmux_crew::mission_config::
/// list_ids()`, unioned user → on-disk → embedded), loads + `validate()`s
/// each, and reports id / source tier / schema_version for all of them.
///
/// Two DISTINCT finding classes surface differently, on purpose:
///
/// - **Structural findings** (`FindingSeverity::Error` — dangling
///   `depends_on`, empty ids, duplicate ids) and **schema_version drift**
///   (`FindingSeverity::Warning` on the `schema_version` path) are real,
///   actionable problems — either one flips this check to `Warn` and names
///   the offending document(s).
/// - **Unrecognized step-kind references** are checked ONLY against
///   `StepKindRegistry::with_builtins()`'s four Tier 1 ids and are
///   deliberately treated as INFORMATIONAL, never blocking: Tier 3 kinds
///   (`review.*`, `mission.*`, #1352) register into their OWN per-mission
///   registry at COMPOSITION time (`build_review_graph`,
///   `default_phase_graph`), which this document-level check has no way
///   to see. Both built-in configs shipped in this packet reference ONLY
///   Tier 3 kinds, so an "unknown kind" hit is the EXPECTED steady state,
///   not a sign anything is broken — surfaced in the message for
///   visibility, but never flips the check's status on its own (a
///   permanent Warn for an expected, unfixable-by-design condition would
///   just teach operators to ignore this check).
fn check_mission_config_registry() -> Check {
    use darkmux_crew::mission_config::{self, FindingSeverity};
    use darkmux_crew::step_kinds::StepKindRegistry;

    let ids = mission_config::list_ids();
    if ids.is_empty() {
        return Check {
            name: "mission config registry".into(),
            status: Status::Pass,
            message: "no mission configs registered".into(),
            hint: None,
        };
    }

    let known_kinds = StepKindRegistry::with_builtins().ids();
    let known_kind_refs: Vec<&str> = known_kinds.iter().map(String::as_str).collect();

    let mut summary_lines: Vec<String> = Vec::new();
    let mut blocking: Vec<String> = Vec::new();
    let mut kind_warning_ids: Vec<String> = Vec::new();

    for id in &ids {
        match mission_config::load(id) {
            Ok(loaded) => {
                let findings = loaded.config.validate(&known_kind_refs);
                let errors: Vec<_> =
                    findings.iter().filter(|f| f.severity == FindingSeverity::Error).collect();
                let version_drift: Vec<_> = findings
                    .iter()
                    .filter(|f| f.severity == FindingSeverity::Warning && f.path == "schema_version")
                    .collect();
                let kind_warnings: Vec<_> = findings
                    .iter()
                    .filter(|f| f.severity == FindingSeverity::Warning && f.path.ends_with(".kind"))
                    .collect();

                let version = loaded.config.schema_version.as_deref().unwrap_or("(unset)");
                summary_lines.push(format!("{id} ({}, schema {version})", loaded.source.label()));

                if !errors.is_empty() {
                    let joined = errors.iter().map(|f| f.to_string()).collect::<Vec<_>>().join("; ");
                    blocking.push(format!("\"{id}\": {joined}"));
                }
                if !version_drift.is_empty() {
                    let joined =
                        version_drift.iter().map(|f| f.to_string()).collect::<Vec<_>>().join("; ");
                    blocking.push(format!("\"{id}\": {joined}"));
                }
                // (#1284 review round 2, consider 7) A USER-tier copy whose
                // schema MINOR trails the binary's is silently missing
                // additive fields newer launchers rely on — concretely, a
                // 1.0-era user copy of "review" has no typed `expand` block,
                // so the probe stage interprets to ZERO probe tasks. Major
                // drift is `validate()`'s job (either direction); this
                // minor-trailing check is user-tier-only because the
                // embedded/on-disk built-ins ship with the binary and can't
                // trail it.
                if loaded.source == mission_config::MissionConfigSource::User {
                    if let Some((doc_major, doc_minor)) = loaded
                        .config
                        .schema_version
                        .as_deref()
                        .and_then(parse_major_minor)
                    {
                        let (bin_major, bin_minor) =
                            parse_major_minor(mission_config::MISSION_CONFIG_SCHEMA)
                                .expect("MISSION_CONFIG_SCHEMA is a valid MAJOR.MINOR constant");
                        if doc_major == bin_major && doc_minor < bin_minor {
                            blocking.push(format!(
                                "\"{id}\": user-tier copy declares schema {doc_major}.{doc_minor}, \
                                 but this binary's mission-config schema is \
                                 {bin_major}.{bin_minor} — the user copy predates additive \
                                 fields newer launchers rely on (e.g. a 1.0-era \"review\" \
                                 copy has no `expand` block, so its probe stage interprets \
                                 to zero probe tasks); re-derive it from the current \
                                 built-in or delete it to fall back to the embedded tier"
                            ));
                        }
                    }
                }
                if !kind_warnings.is_empty() {
                    kind_warning_ids.push(id.clone());
                }
            }
            Err(e) => blocking.push(format!("\"{id}\": failed to parse — {e}")),
        }
    }

    if blocking.is_empty() {
        let mut message =
            format!("{} mission config(s) registered: {}", ids.len(), summary_lines.join(", "));
        if !kind_warning_ids.is_empty() {
            message.push_str(&format!(
                "; {} reference step kinds outside this process's Tier 1 registry (expected — \
                 Tier 3 kinds register at composition time, so this check can't see them): {}",
                kind_warning_ids.len(),
                kind_warning_ids.join(", ")
            ));
        }
        Check {
            name: "mission config registry".into(),
            status: Status::Pass,
            message,
            hint: None,
        }
    } else {
        Check {
            name: "mission config registry".into(),
            status: Status::Warn,
            // `blocking` holds one entry per FINDING GROUP, not per config
            // (one document can contribute a structural-error entry AND a
            // schema-drift entry), so the count is worded as issues, never
            // as a config count (#1284 review round 1).
            message: format!(
                "{} mission config(s) registered, {} issue(s): {}",
                ids.len(),
                blocking.len(),
                blocking.join(" | ")
            ),
            hint: Some(
                "fix the named document(s) under `~/.darkmux/mission-configs/<id>.json` (or the \
                 checked-out `templates/builtin/mission-configs/<id>.json` for a built-in) — a \
                 dangling depends_on, an empty id, or a schema_version your darkmux build \
                 doesn't recognize. This packet only validates configs; nothing executes them \
                 yet (#1284 Packet 3)."
                    .into(),
            ),
        }
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
                "load a model via the LMStudio GUI or `lms load <id> --context-length <N>` — \
                 or just dispatch: a `darkmux dispatch` / `mission launch` loads what its \
                 staffing needs, under the resident budget"
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
                 LMStudio is serving (compare `darkmux machine status` and `darkmux profile list`)"
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

/// (#680) The internal Docker-bounded runtime is the ONLY dispatch path for
/// `dispatch` and `lab run` (#1405 removed the legacy `openclaw`
/// shell-out runtime), but nothing else in doctor surfaces it — a fresh
/// operator otherwise gets an all-green doctor and only learns the Docker
/// requirement when their first dispatch bails at the dispatch-time preflight.
/// Reuses that preflight's probe (`dispatch_internal::docker_runtime_status`)
/// so the image tag + probe logic have one home. Warn (not Fail) so a
/// `swap`/`status`/`profiles`-only operator (no dispatching yet) isn't
/// blocked by a doctor check for a capability they haven't used.
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
                .filter_map(|m| darkmux_types::size::parse_size_gb(&m.size))
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

/// Warn when legacy flat mission/phase files exist in the pre-#148 layout.
/// Pass when neither legacy_missions_dir nor legacy_phases_dir contain any
/// top-level .json files. Fail never — legacy files don't break the system,
/// but they're a signal that `darkmux mission migrate --apply` should be run
/// to consolidate into the per-mission layout. (#148)
fn check_legacy_mission_layout() -> Check {
    let missions_dir = darkmux_crew::lifecycle::legacy_missions_dir();
    let phases_dir = darkmux_crew::lifecycle::legacy_phases_dir();

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

    // Count legacy flat .json files in phases dir
    if let Ok(entries) = std::fs::read_dir(&phases_dir) {
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
        let phases = darkmux_crew::loader::phases_dir();
        Check {
            name: "legacy mission layout".into(),
            status: Status::Warn,
            message: format!(
                "{legacy_count} legacy flat file(s) at {}/<id>.json or {}/<id>.json",
                missions.display(),
                phases.display()
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

/// (#934) The at-a-glance verdict banner: maps `worst_status()` → a
/// plain-language headline (`ok` / `needs attention` / `broken`) so the operator
/// reads one line instead of scanning ~35 rows. The headline names the
/// highest-severity finding — the first Fail, else the first Warn — i.e. the
/// thing to act on. (Tie-break by blast radius is a future refinement; first-of-
/// severity is the shippable L1.) Plain-language verdict words by operator lean
/// (#932 Q1); the per-check markers stay ✓/⚠/✗.
fn verdict_banner(r: &DoctorReport) -> String {
    let headline =
        |s: Status| r.checks.iter().find(|c| c.status == s).map(|c| format!("{}: {}", c.name, c.message));
    match r.worst_status() {
        Status::Pass => darkmux_types::style::success("● ok — every check passed"),
        Status::Warn => darkmux_types::style::warn(&format!(
            "● needs attention — {}",
            headline(Status::Warn).unwrap_or_else(|| "see the warnings below".into())
        )),
        Status::Fail => darkmux_types::style::error(&format!(
            "● broken — {}",
            headline(Status::Fail).unwrap_or_else(|| "see the failures below".into())
        )),
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
    // (#934) Lead with the verdict so the operator gets the answer before the
    // detail — the L1 "isn't drowned in flat checks" goal.
    println!("{}", verdict_banner(r));
    println!();
    if verbose {
        for c in &r.checks {
            print_check_line(c);
        }
    } else {
        // The build identity line always shows (it answers "which version?",
        // not a health question), and a PASSING daemon-reachable row always
        // shows too (its message is the viewer's locator URLs — the thing the
        // operator ran `doctor` to find). Both bypass pass-consolidation. A
        // daemon that's down is a Warn and prints via the problem path below.
        let always_show = |c: &&Check| {
            c.name == BUILD_CHECK_NAME || (c.name == DAEMON_CHECK_NAME && c.status == Status::Pass)
        };
        for c in r.checks.iter().filter(always_show) {
            print_check_line(c);
        }
        // The remaining passing checks collapse to a count — `-v` for the full list.
        let collapsed = r
            .checks
            .iter()
            .filter(|c| c.status == Status::Pass && !always_show(c))
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

    // ─── (#1426) installed darkmux-* skills freshness ───────────────────────

    /// Write an installed `SKILL.md` for `name` under `target/<name>/`.
    fn write_installed_skill(target: &std::path::Path, name: &str, body: &str) {
        let dir = target.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), body).unwrap();
    }

    fn embedded(name: &str, content: &str) -> EmbeddedSkill {
        EmbeddedSkill {
            name: name.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn skills_freshness_passes_when_all_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().to_path_buf();
        write_installed_skill(&target, "darkmux-alpha", "body-a");
        write_installed_skill(&target, "darkmux-beta", "body-b");
        let embedded_set = vec![embedded("darkmux-alpha", "body-a"), embedded("darkmux-beta", "body-b")];

        let c = check_installed_skills_freshness(&[target], &embedded_set, &[]);
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.hint.is_none());
        assert!(c.message.contains("2 up to date"), "{}", c.message);
    }

    #[test]
    fn skills_freshness_warns_when_a_file_differs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().to_path_buf();
        write_installed_skill(&target, "darkmux-alpha", "body-a");
        // Stale copy of beta — content drifted from the embedded reference.
        write_installed_skill(&target, "darkmux-beta", "OLD-body-b");
        let embedded_set = vec![embedded("darkmux-alpha", "body-a"), embedded("darkmux-beta", "body-b")];

        let c = check_installed_skills_freshness(&[target], &embedded_set, &[]);
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert!(c.message.contains("darkmux-beta"), "{}", c.message);
        assert!(
            c.hint.as_deref().unwrap().contains("darkmux init"),
            "fix_hint points at the refresh command: {:?}",
            c.hint
        );
    }

    #[test]
    fn skills_freshness_ignores_non_darkmux_dirs_entirely() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().to_path_buf();
        write_installed_skill(&target, "darkmux-alpha", "body-a");
        // A decoy operator-owned skill that DIFFERS from nothing darkmux ships —
        // and whose content would look "stale" if it were ever compared. It must
        // be invisible to the check.
        write_installed_skill(&target, "my-personal-skill", "user-owned content");
        let embedded_set = vec![embedded("darkmux-alpha", "body-a")];

        let c = check_installed_skills_freshness(&[target], &embedded_set, &[]);
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(
            !c.message.contains("my-personal-skill"),
            "non-darkmux entries are never reported: {}",
            c.message
        );
    }

    #[test]
    fn skills_freshness_informational_when_embedded_not_installed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().to_path_buf();
        write_installed_skill(&target, "darkmux-alpha", "body-a");
        // beta is embedded but not installed — a minimal install, not drift.
        let embedded_set = vec![embedded("darkmux-alpha", "body-a"), embedded("darkmux-beta", "body-b")];

        let c = check_installed_skills_freshness(&[target], &embedded_set, &[]);
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.hint.is_none());
        assert!(
            c.message.contains("embedded but not installed") && c.message.contains("darkmux-beta"),
            "the not-installed skill is noted informationally: {}",
            c.message
        );
    }

    #[test]
    fn skills_freshness_warns_on_retired_installed_skill() {
        // (#1449) A darkmux-* skill the binary no longer bundles is now a WARN
        // (was informational-only). `init`'s prune pass makes the fix actionable,
        // and a retired skill like darkmux-swap-stack is a live dead-verb teacher.
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().to_path_buf();
        write_installed_skill(&target, "darkmux-alpha", "body-a");
        write_installed_skill(&target, "darkmux-retired", "leftover");
        let embedded_set = vec![embedded("darkmux-alpha", "body-a")];

        let c = check_installed_skills_freshness(&[target], &embedded_set, &[]);
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert!(
            c.message.contains("no longer bundled") && c.message.contains("darkmux-retired"),
            "{}",
            c.message
        );
        let hint = c.hint.as_deref().unwrap();
        assert!(
            hint.contains("darkmux init") && hint.contains("darkmux-retired"),
            "fix_hint names the retired skill + the prune command: {hint:?}"
        );
    }

    #[test]
    fn skills_freshness_excludes_maintainer_only_from_retired() {
        // (#1449) A maintainer-only skill (not embedded, installed from a source
        // checkout) must NOT be reported retired.
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().to_path_buf();
        write_installed_skill(&target, "darkmux-alpha", "body-a");
        write_installed_skill(&target, "darkmux-point-release", "maintainer skill");
        let embedded_set = vec![embedded("darkmux-alpha", "body-a")];

        let c = check_installed_skills_freshness(
            &[target],
            &embedded_set,
            &["darkmux-point-release".to_string()],
        );
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(
            !c.message.contains("darkmux-point-release"),
            "maintainer-only skill is never flagged retired: {}",
            c.message
        );
    }

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
        // Warn, never Fail — swap-only operators (profile multiplexing, no
        // dispatches) legitimately have no Docker.
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
        let r = run();
        // 32 static checks via run() (#1405 removed the 4 openclaw-gated
        // checks; #1426 removed recommendation-drift +
        // recommended-profile-not-shadowed with the retired recommendations
        // family), incl. build-identity [#1129] + docker-runtime [#680] +
        // load projection + daemon reachable +
        // darkmux-version-vs-latest-release [#13] +
        // crew-role-prompt-coverage [#141] + flow-sink-health [#170] +
        // machine_id + orchestrator [#167] + openai-base-url-conflict [#5] +
        // audit-integrity [#163] + utility-model-binding
        // [#590] + legacy-mission-layout [#148] + beat-33-crew-dir [Beat 33
        // directory flatten] + role-tool-vocab [#340] +
        // legacy-compaction-extras [#380] + redis-config [#661] +
        // remote-endpoint-credentials [#85/#91] + audit-write-drops [#877] +
        // serve-daemon-auth [#881] + fleet.mode [#933] + env-masks-config
        // [#934] + binary-split-brain [#934] + crew-validation [#1269] +
        // mission-config-registry [#1284]) + one per active eureka rule.
        // Every check should appear regardless of environment — even if the
        // underlying probe couldn't read state.
        let expected = 32 + darkmux_eureka::all_rules().len();
        assert_eq!(r.checks.len(), expected);
    }

    // ─── #934 doctor L1 ───────────────────────────────────────────────
    #[serial_test::serial]
    #[test]
    fn env_masks_config_flags_redis_url_over_enabled_block() {
        use darkmux_types::config::{DarkmuxConfig, RedisConfig};
        unsafe { std::env::remove_var("DARKMUX_REDIS_URL") };
        // An ENABLED config.redis block — the operator intentionally turned it on.
        let enabled = DarkmuxConfig {
            redis: Some(RedisConfig { enabled: Some(true), host: Some("h".into()), ..Default::default() }),
            ..Default::default()
        };
        // No env → nothing masked.
        assert_eq!(env_masks_config_check(&enabled).status, Status::Pass);
        // A stale DARKMUX_REDIS_URL over the enabled block → Warn naming config.redis.
        unsafe { std::env::set_var("DARKMUX_REDIS_URL", "redis://other:6379") };
        let c = env_masks_config_check(&enabled);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("config.redis"), "{}", c.message);
        // The DEFAULT init shape (enabled:false + a host) must NOT warn even with
        // the env set — it assembles no config Redis, so nothing is masked. This
        // pins the false-positive-on-fresh-install regression out.
        let init_default = DarkmuxConfig {
            redis: Some(RedisConfig { enabled: Some(false), host: Some("127.0.0.1".into()), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(
            env_masks_config_check(&init_default).status,
            Status::Pass,
            "default init config (enabled:false) is not masked"
        );
        unsafe { std::env::remove_var("DARKMUX_REDIS_URL") };
    }

    #[test]
    fn verdict_banner_maps_severity_and_names_the_finding() {
        let mk = |name: &str, s: Status| Check { name: name.into(), status: s, message: format!("{name}-msg"), hint: None };
        let ok = DoctorReport { checks: vec![mk("a", Status::Pass)] };
        assert!(verdict_banner(&ok).contains("ok"));
        let warn = DoctorReport { checks: vec![mk("a", Status::Pass), mk("redis", Status::Warn)] };
        let b = verdict_banner(&warn);
        assert!(b.contains("needs attention") && b.contains("redis"), "{b}");
        let fail = DoctorReport { checks: vec![mk("redis", Status::Warn), mk("daemon", Status::Fail)] };
        let b = verdict_banner(&fail);
        assert!(b.contains("broken") && b.contains("daemon"), "highest severity wins: {b}");
    }

    // ─── tailnet viewer URL (doctor surfaces where to open the viewer) ───
    #[test]
    fn parse_tailnet_viewer_url_matches_the_proxy_to_our_port() {
        // The real `tailscale serve status --json` shape (captured live).
        let json = r#"{"TCP":{"80":{"HTTP":true}},"Web":{"macbook-pro.taild82cbb.ts.net:80":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8765"}}}}}"#;
        assert_eq!(
            parse_tailnet_viewer_url(json, 8765).as_deref(),
            Some("http://macbook-pro.taild82cbb.ts.net/")
        );
        // A different daemon port → not our proxy → None.
        assert_eq!(parse_tailnet_viewer_url(json, 9000), None);
        // Served on 443 → https scheme.
        let j443 = r#"{"Web":{"host.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8765"}}}}}"#;
        assert_eq!(parse_tailnet_viewer_url(j443, 8765).as_deref(), Some("https://host.ts.net/"));
        // localhost proxy target is accepted too.
        let jlocal = r#"{"Web":{"h.ts.net:80":{"Handlers":{"/":{"Proxy":"http://localhost:8765"}}}}}"#;
        assert_eq!(parse_tailnet_viewer_url(jlocal, 8765).as_deref(), Some("http://h.ts.net/"));
        // Not serving / empty / garbage → None (best-effort, never an error).
        assert_eq!(parse_tailnet_viewer_url("{}", 8765), None);
        assert_eq!(parse_tailnet_viewer_url("not json", 8765), None);
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
    fn platform_check_always_present() {
        let r = run();
        assert!(r.checks.iter().any(|c| c.name.contains("platform")));
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
        // (viewer-url) Pass message now surfaces the loopback viewer URL; the
        // tailnet/phone URL is absent here (nothing proxies to this random test
        // port).
        assert!(
            check.message.contains(&format!("viewer http://127.0.0.1:{port}/")),
            "Pass message should surface the loopback viewer URL. Got: {}",
            check.message
        );

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

    // ─── #1426 ship-2: check_crews_residue tests ─────────────────────

    #[serial_test::serial]
    #[test]
    fn check_crews_residue_passes_clean_when_no_crews_key() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        std::fs::write(
            &config_path,
            r#"{"profiles":{"fast":{"models":[{"id":"a","n_ctx":1000}]}}}"#,
        )
        .unwrap();

        let check = check_crews_residue();
        assert_eq!(check.status, Status::Pass);
        assert!(check.message.contains("no legacy crews residue"));
        assert!(check.hint.is_none());
    }

    /// A pre-2.0 profiles.json still carrying a `crews` map parses fine (the
    /// key overflows into `extras`) and surfaces as a WARN — a config block
    /// that no longer does anything merits the warn tier, so the operator
    /// learns their declared crews stopped being read. Never an error (the
    /// residue is harmless to every code path).
    #[serial_test::serial]
    #[test]
    fn check_crews_residue_warns_on_legacy_crews_key() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        std::fs::write(
            &config_path,
            r#"{"profiles":{"fast":{"models":[{"id":"a","n_ctx":1000}]}},
                "crews":{"review-deep":{"seats":{"review-probe":[{"profile":"fast"}]}}}}"#,
        )
        .unwrap();

        let check = check_crews_residue();
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("DOES NOTHING"), "got: {}", check.message);
        assert!(check.hint.as_deref().unwrap().contains("retired in 2.0"));
    }

    // ─── #1284 Packet 1: check_mission_config_registry ───────────────

    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_passes_on_embedded_builtins_only() {
        // Empty user dir — only the two embedded built-ins (`review`,
        // `coder-phase`) resolve. Both reference exclusively Tier 3 step
        // kinds, so the check must still PASS (unknown-kind warnings are
        // informational, never blocking — see the check's own doc).
        let _guard = CrewRootGuard::new();
        let check = check_mission_config_registry();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("review"), "{}", check.message);
        assert!(check.message.contains("coder-phase"), "{}", check.message);
        assert!(check.message.contains("embedded"), "{}", check.message);
        // The Tier-3-kind caveat is still surfaced for visibility, even
        // though it doesn't flip status.
        assert!(check.message.contains("Tier 3"), "{}", check.message);
    }

    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_warns_on_dangling_depends_on() {
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        std::fs::write(
            guard.path().join("mission-configs").join("broken-deps.json"),
            r#"{
                "id": "broken-deps",
                "name": "Broken Deps",
                "phases": [
                    {"id": "p1", "tasks": [
                        {"id": "t1", "depends_on": ["ghost-task"], "steps": [
                            {"id": "s1", "kind": "dispatch.internal"}
                        ]}
                    ]}
                ]
            }"#,
        )
        .unwrap();

        let check = check_mission_config_registry();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("broken-deps"), "{}", check.message);
        assert!(check.message.contains("ghost-task"), "{}", check.message);
        assert!(check.hint.is_some());
    }

    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_warns_on_schema_version_drift() {
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        std::fs::write(
            guard.path().join("mission-configs").join("future.json"),
            r#"{"id":"future","name":"Future","schema_version":"99.0"}"#,
        )
        .unwrap();

        let check = check_mission_config_registry();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("future"), "{}", check.message);
        assert!(check.message.contains("schema_version"), "{}", check.message);
    }

    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_warns_on_malformed_json() {
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        std::fs::write(guard.path().join("mission-configs").join("busted.json"), "{not valid json").unwrap();

        let check = check_mission_config_registry();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("busted"), "{}", check.message);
        assert!(check.message.contains("failed to parse"), "{}", check.message);
    }

    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_reports_only_the_bad_config_when_mixed() {
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        std::fs::write(
            guard.path().join("mission-configs").join("good.json"),
            r#"{"id":"good","name":"Good"}"#,
        )
        .unwrap();
        std::fs::write(
            guard.path().join("mission-configs").join("bad.json"),
            r#"{"id":"","name":"Bad"}"#,
        )
        .unwrap();

        let check = check_mission_config_registry();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("\"bad\""), "{}", check.message);
    }

    /// (#1284 review round 2, consider 7) A USER-tier copy of a built-in
    /// whose schema MINOR trails the binary's warns loudly — the concrete
    /// hazard: a 1.0-era user copy of "review" has no typed `expand` block,
    /// so its probe stage interprets to ZERO probe tasks; doctor should say
    /// so BEFORE a launch does. Same-major-lower-minor only — a same-version
    /// copy (or major drift, which `validate()` already covers) doesn't
    /// trip this.
    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_warns_when_user_tier_minor_trails_the_binary() {
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        // A structurally-valid 1.0-era user override of the "review"
        // built-in (schema major matches the binary's, minor trails it).
        std::fs::write(
            guard.path().join("mission-configs").join("review.json"),
            r#"{"id":"review","name":"PR Review (stale user copy)","schema_version":"1.0"}"#,
        )
        .unwrap();

        let check = check_mission_config_registry();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("user-tier copy declares schema 1.0"), "{}", check.message);
        assert!(check.message.contains("zero probe tasks"), "{}", check.message);
    }

    #[test]
    fn parse_major_minor_accepts_two_part_versions_and_rejects_garbage() {
        assert_eq!(parse_major_minor("1.1"), Some((1, 1)));
        assert_eq!(parse_major_minor("1.0.5"), Some((1, 0)), "extra segments tolerated");
        assert_eq!(parse_major_minor("1"), None, "no minor segment");
        assert_eq!(parse_major_minor("not-a-version"), None);
    }

    // ─── #1282: check_profile_registry quarantine + n_ctx surface ───

    /// The exact #1282 scenario: one profile entry missing a required field
    /// (`id`) is quarantined at parse — doctor names the entry and serde's
    /// field-level error while the sibling profile stays healthy.
    #[serial_test::serial]
    #[test]
    fn check_profile_registry_warns_and_names_quarantined_entry() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        std::fs::write(
            &config_path,
            r#"{"profiles":{
                    "fast":{"models":[{"id":"a","n_ctx":1000}]},
                    "broken":{"models":[{"n_ctx":32000}]}
                }}"#,
        )
        .unwrap();

        let check = check_profile_registry();
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("quarantined profile \"broken\""), "{}", check.message);
        assert!(check.message.contains("missing field `id`"), "{}", check.message);
        assert!(!check.message.contains("quarantined profile \"fast\""));
        assert!(check.hint.is_some());
    }

    /// (#1282) A LOCAL model without `n_ctx` parses (lenient) but doctor
    /// flags it — the resolution error waiting to happen, surfaced loud.
    #[serial_test::serial]
    #[test]
    fn check_profile_registry_warns_on_local_model_without_n_ctx() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        std::fs::write(
            &config_path,
            r#"{"profiles":{"ctxless":{"models":[{"id":"local-a"}]}}}"#,
        )
        .unwrap();

        let check = check_profile_registry();
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("ctxless"), "{}", check.message);
        assert!(check.message.contains("local-a"), "{}", check.message);
        assert!(check.message.contains("n_ctx"), "{}", check.message);
    }

    /// (#1282) An endpoint-bearing model without `n_ctx` is fully valid —
    /// no warning: hosted models have no local context to declare.
    #[serial_test::serial]
    #[test]
    fn check_profile_registry_passes_on_endpoint_model_without_n_ctx() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        std::fs::write(
            &config_path,
            r#"{"profiles":{"cloud":{"models":[
                    {"id":"gpt-4o","endpoint":{"url":"https://example.azure.com/openai"}}
                ]}}}"#,
        )
        .unwrap();

        let check = check_profile_registry();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
    }

    // ─── #85/#91: check_remote_endpoint_credentials tests ───────

    #[serial_test::serial]
    #[test]
    fn check_remote_endpoint_credentials_passes_when_no_endpoint_declared() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        let registry_json = r#"{
            "profiles": {
                "local-profile": {
                    "models": [{"id": "primary-x", "n_ctx": 100000}]
                }
            }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_remote_endpoint_credentials();
        assert_eq!(check.status, Status::Pass);
        assert!(check.message.contains("no profile models declare a remote endpoint"));
    }

    #[serial_test::serial]
    #[test]
    fn check_remote_endpoint_credentials_passes_when_endpoint_has_no_auth() {
        // A remote endpoint with no auth block at all (e.g. an
        // unauthenticated proxy) is valid and must not be flagged —
        // `auth_type.is_none()` skips it entirely (not even counted).
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        let registry_json = r#"{
            "profiles": {
                "proxy-profile": {
                    "models": [{
                        "id": "proxy-model",
                        "n_ctx": 32768,
                        "endpoint": { "url": "http://localhost:8080/v1" }
                    }]
                }
            }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_remote_endpoint_credentials();
        assert_eq!(check.status, Status::Pass);
        assert!(check.message.contains("no profile models declare a remote endpoint"));
    }

    #[serial_test::serial]
    #[test]
    fn check_remote_endpoint_credentials_warns_when_keychain_field_missing() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        let registry_json = r#"{
            "profiles": {
                "azure-profile": {
                    "models": [{
                        "id": "gpt-4o",
                        "n_ctx": 128000,
                        "endpoint": {
                            "url": "https://x.cognitiveservices.azure.com/openai/deployments/gpt-4o",
                            "auth": { "type": "api-key" }
                        }
                    }]
                }
            }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_remote_endpoint_credentials();
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("azure-profile"));
        assert!(check.message.contains("gpt-4o"));
        // (#1312) The message now names BOTH credential sources (keychain OR
        // key_env), since either satisfies the auth.
        assert!(check.message.contains("no credential source resolved"), "{}", check.message);
        assert!(check.message.contains("endpoint.auth.keychain"), "{}", check.message);
        assert!(check.message.contains("key_env"), "{}", check.message);
    }

    #[serial_test::serial]
    #[test]
    fn check_remote_endpoint_credentials_warns_when_keychain_item_absent() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        let registry_json = r#"{
            "profiles": {
                "azure-profile": {
                    "models": [{
                        "id": "gpt-4o",
                        "n_ctx": 128000,
                        "endpoint": {
                            "url": "https://x.cognitiveservices.azure.com/openai/deployments/gpt-4o",
                            "auth": {
                                "type": "api-key",
                                "keychain": "darkmux-doctor-test-definitely-nonexistent-item-xyz123"
                            }
                        }
                    }]
                }
            }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_remote_endpoint_credentials();
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("not found on this machine"));
        let hint = check.hint.as_deref().unwrap_or("");
        assert!(hint.contains("security add-generic-password"));
    }

    #[serial_test::serial]
    #[test]
    fn check_remote_endpoint_credentials_satisfied_by_present_key_env() {
        // (#1312) A declared `key_env` var that is PRESENT in the environment
        // satisfies the credential — even with a bogus/absent keychain item.
        let var = "DARKMUX_DOCTOR_TEST_KEY_ENV_1312";
        let prev = std::env::var(var).ok();
        unsafe { std::env::set_var(var, "present-value"); }

        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        let registry_json = format!(
            r#"{{
            "profiles": {{
                "azure-profile": {{
                    "models": [{{
                        "id": "gpt-4o",
                        "n_ctx": 128000,
                        "endpoint": {{
                            "url": "https://x.cognitiveservices.azure.com/openai/deployments/gpt-4o",
                            "auth": {{
                                "type": "api-key",
                                "keychain": "darkmux-doctor-test-definitely-nonexistent-item-xyz123",
                                "key_env": "{var}"
                            }}
                        }}
                    }}]
                }}
            }}
        }}"#
        );
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_remote_endpoint_credentials();
        assert_eq!(check.status, Status::Pass, "present key_env should satisfy: {}", check.message);

        unsafe {
            match prev {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
    }

    #[test]
    fn keychain_item_present_returns_false_for_nonexistent_item() {
        assert!(!keychain_item_present(
            "darkmux-doctor-test-definitely-nonexistent-item-xyz123"
        ));
    }

    // ─── #1177: doctor --probe (probe_remote_endpoints) ─────────────

    #[serial_test::serial]
    #[test]
    fn probe_remote_endpoints_reports_nothing_to_probe() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        let registry_json = r#"{
            "profiles": {
                "local-profile": {
                    "models": [{"id": "primary-x", "n_ctx": 100000}]
                }
            }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let checks = probe_remote_endpoints();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Pass);
        assert!(checks[0].message.contains("nothing to probe"));
    }

    #[serial_test::serial]
    #[test]
    fn probe_remote_endpoints_probes_once_per_distinct_endpoint_and_reports_cost() {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // Mock accepting up to 2 connections, counting them — if the dedup
        // ever regresses, the second profile's identical declaration would
        // land a SECOND billed call; the counter catches it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = hits.clone();
        std::thread::spawn(move || {
            let body = r#"{"model":"served-y","usage":{"total_tokens":9},"choices":[{"message":{"content":"ok"}}]}"#;
            for stream in listener.incoming().take(2) {
                let Ok(mut stream) = stream else { break };
                hits_srv.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf); // request fits one read for this body size
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        // TWO profiles declaring the SAME endpoint + model (no auth ⇒
        // Keychain untouched; the probe still exercises URL + round-trip).
        let registry_json = format!(
            r#"{{
            "profiles": {{
                "review-a": {{
                    "models": [{{
                        "id": "gpt-probe",
                        "n_ctx": 128000,
                        "endpoint": {{ "url": "http://127.0.0.1:{port}/v1" }}
                    }}]
                }},
                "review-b": {{
                    "models": [{{
                        "id": "gpt-probe",
                        "n_ctx": 128000,
                        "endpoint": {{ "url": "http://127.0.0.1:{port}/v1" }}
                    }}]
                }}
            }}
        }}"#
        );
        std::fs::write(&config_path, registry_json).unwrap();

        let checks = probe_remote_endpoints();
        assert_eq!(checks.len(), 1, "shared endpoint+model probes exactly once");
        assert_eq!(checks[0].status, Status::Pass);
        assert!(checks[0].message.contains("round-trip ok"), "{}", checks[0].message);
        assert!(checks[0].message.contains("served by `served-y`"), "{}", checks[0].message);
        assert!(checks[0].message.contains("probe cost 9 tokens"), "{}", checks[0].message);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "exactly one billed call");
    }

}
