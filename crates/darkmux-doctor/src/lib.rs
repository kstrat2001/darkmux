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
// (#2112) Battery / Low Power Mode / thermal-state / thermal-emergency
// doctor check — see the module doc for why it shares `power_posture`'s
// probe with the mission pre-flight rather than re-reading `pmset` itself.
mod checks_power;
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
        // (#1461) Staleness: what is RUNNING vs what is INSTALLED vs the source.
        check_daemon_freshness(),
        check_binary_vs_source(),
        check_runtime_image_freshness(),
        check_runtime_binary_cache(),
        check_ram_headroom(),
        check_ram_headroom_load_projection(),
        check_power_state(),
        check_platform_and_provider(),
        check_crew_role_prompt_coverage(),
        check_rules_registry(),
        check_flow_sink_health(),
        check_machine_id_resolution(),
        check_fleet_mode(),
        check_openai_base_url_conflict(),
        check_redis_config(),
        check_gh_allowlist(),
        check_review_judge_exhaustion_policy(),
        check_step_command_timeout(),
        check_dispatch_free_concurrency(),
        check_turn_delay(),
        check_reasoning_checkpoint_interval(),
        check_max_stall_recoveries(),
        check_host_sampler_interval(),
        check_telemetry_record_every_samples(),
        check_generation_checkpoint_interval(),
        check_thermal_governor(),
        check_host_probe(),
        check_quarantined_mirrors(),
        checks_power::check_power_posture(),
        check_remote_endpoint_credentials(),
        check_env_masks_config(),
        check_binary_split_brain(),
        check_audit_integrity(),
        check_audit_write_drops(),
        check_daemon_auth(),
        check_utility_model_binding(),
        check_unpriceable_residents(),
        check_role_profiles(),
        check_role_tool_vocab_typos(),
        check_beat33_legacy_crew_dir(),
        check_legacy_mission_layout(),
        check_legacy_compaction_extras(),
        check_mission_envelope_readability(),
    ];
    let checks = [checks, check_hooks(), eureka_checks()].concat();
    DoctorReport { checks }
}

/// Name of the installed-skills freshness check (#1426).
const SKILLS_FRESHNESS_CHECK_NAME: &str = "darkmux skills freshness"; // drift-guard:allow darkmux skills — noun (the installed skills), the doctor-check name, not the retired verb (#1469)

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
                "AuditFileSink is opt-in: set DARKMUX_AUDIT_DIR to enable a BLAKE3 hash-chained audit log whose edits `darkmux flow integrity-check` detects (absent a full re-chain — the chain is un-anchored), alongside the casual LocalFile sink."
                    .into(),
            ),
        };
    }

    summarize_audit_reports(&reports)
}

/// Turn a set of `flow integrity-check` reports into one doctor `Check`.
/// Pure (no I/O), split out of `check_audit_integrity` so the three-way
/// split — a genuine chain break, a legacy-format file (#1769), or a fully
/// verified walk — is unit-testable without touching the filesystem or
/// `DARKMUX_AUDIT_DIR`.
///
/// The three statuses map to doctor's own exit code (`main.rs`: `Fail` → 1,
/// everything else → 0): a genuine break is the only thing that flips it.
/// A legacy-format file is readable but was NOT content-verified at all
/// (see `IntegrityReport::legacy_format`) — that is neither "everything
/// verified" (Pass would overclaim) nor "tampering" (Fail would be a false
/// accusation), so it gets its own `Warn`.
fn summarize_audit_reports(reports: &[darkmux_flow::IntegrityReport]) -> Check {
    let broken: Vec<&darkmux_flow::IntegrityReport> =
        reports.iter().filter(|r| !r.chain_valid).collect();
    if !broken.is_empty() {
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
        return Check {
            name: "audit integrity".into(),
            status: Status::Fail,
            message: summary,
            hint: Some(
                "Audit log has been edited or a write was interleaved. Run `darkmux flow integrity-check` for the full per-file breakdown. If tampering is suspected, the chain break locates the affected line; records before that line link consistently, which is evidence they are unmodified — not proof."
                    .into(),
            ),
        };
    }

    // (#1769) Every chain either links cleanly or is a legacy-format file
    // this binary does not attempt to content-verify (recomputing a
    // struct-hash would repeat the exact lossy round trip #1768/#1769
    // exploited). That's a format boundary, not tampering, so it stays out
    // of the `Fail` branch above — but it's also not the same claim as
    // "everything verified", so it doesn't fold silently into `Pass`
    // either. `Warn` is the honest middle: exit code stays 0 (doctor only
    // flips to 1 on `Fail`), and the caveat is loud.
    let legacy: Vec<&darkmux_flow::IntegrityReport> =
        reports.iter().filter(|r| r.legacy_format).collect();
    if !legacy.is_empty() {
        let total_unverified: u64 = legacy.iter().map(|r| r.records_checked).sum();
        let note = legacy[0]
            .note
            .clone()
            .unwrap_or_else(|| "written in a legacy format this binary cannot re-verify".into());
        return Check {
            name: "audit integrity".into(),
            status: Status::Warn,
            message: format!(
                "{}/{} file(s) in the legacy pre-2.6.0 format — {total_unverified} record(s) \
                 NOT content-verified (readable only). {note}",
                legacy.len(),
                reports.len(),
            ),
            hint: Some(
                "Rotate legacy audit files (move/rename so a fresh chain starts under the byte-hash format, #1769) if you want them re-verifiable. This is not evidence of tampering — run `darkmux flow integrity-check` for the full per-file breakdown."
                    .into(),
            ),
        };
    }

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
                 in today's flow file as `action=audit.write_failed`."
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
                "To expose the daemon across your fleet \
                 (e.g. `fleet status --deep`), set ONE shared bearer token on every machine: \
                 `security add-generic-password -U -a \"$USER\" -s darkmux-serve-token -w` (macOS) + \
                 `daemon_auth_enabled: true` in ~/.darkmux/config.json, or export DARKMUX_SERVE_TOKEN."
                    .into(),
            ),
        )
    }
}

/// `serve daemon token`: reports whether a shared fleet token is configured
/// (#881). Both arms return `Pass` by design — a loopback-only daemon with no
/// token is the ordinary single-machine state, and the bind gate refuses the
/// unsafe combination at runtime, so there is nothing here to cry wolf about.
///
/// Named for the STATE it reports, not for a posture (#1839). It was
/// `serve daemon auth`, and a check that (a) names a security concern and
/// (b) is structurally incapable of any status but ✓ reads, inside doctor's
/// `● ok — every check passed` headline, as a security check that cleared.
/// It never checked anything of the sort. `token` says what it actually
/// looks at: whether one is set.
fn check_daemon_auth() -> Check {
    let (status, message, hint) = daemon_auth_status(darkmux_flow::serve_token_present());
    Check { name: "serve daemon token".into(), status, message, hint }
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
                    // (#1676/#1616) This hint has now been wrong twice, in
                    // opposite directions, so the mechanism is worth stating.
                    //
                    // Originally it claimed compaction would FAIL without a
                    // manual load, and suggested a bare `lms load <id>`. Both
                    // aged badly: #1616 made the internal dispatch path
                    // self-load the compactor at its own declared `n_ctx`
                    // under the `darkmux:` namespace, and a BARE `lms load`
                    // produces the non-namespaced resident the namespace
                    // contract calls the #1135 ghost (unknown load config,
                    // never reused, invisible to `machine eject`) — so
                    // following it could CREATE the problem the namespace
                    // exists to prevent.
                    //
                    // The first correction then drew a contrast that does not
                    // exist: "not needed for dispatch, but needed for the
                    // utility-agent verbs". `utility_model_id()` has exactly
                    // three consumers — this check, a serve-side display read,
                    // and `apply_utility_model`, which sets `compactor_model`.
                    // That is ALL the binding does. `mission propose` and
                    // `lab notebook draft` resolve their own model from the
                    // profile and reach the SAME self-loading dispatch path,
                    // so no verb needs this resident first.
                    //
                    // What remains true is only that a hand-load moves the
                    // cost earlier. Say that and nothing more.
                    hint: Some(
                        "No verb needs this loaded first — the binding's only job is to name the compactor, and every dispatch path self-loads it at its declared `n_ctx` under the `darkmux:` namespace (#1616). Loading it by hand just pays that cost now instead of during the first dispatch; if you do, keep the namespace and the context — `lms load <id> --context-length <n> --identifier darkmux:<id>` — since a bare `lms load` creates a resident darkmux won't reuse and `machine eject` can't reclaim. (#590, #1616, #1675)".into(),
                    ),
                }
            }
        }
    }
}

/// (#1819, narrowed by #1820) Names resident models the memory ledger
/// genuinely CANNOT price — no readable `config.json` arch facts, no
/// readable GGUF header either, AND no catalog size for the #1819
/// size-based fallback to work from. This is the narrower, worse case than
/// "estimated": an estimated resident still gets a labeled potential; this
/// check is about the residents that get none at all, and are the reason
/// `machine.state` stays UNKNOWN forever while they're loaded
/// (`model_ledger.rs`'s cascade — `unpriced_models > 0` blocks Green even
/// when the priced sum fits).
///
/// The live trace this check originally existed for (#1819's issue body):
/// `microsoft/phi-4` resolving to a GGUF download
/// (`lmstudio-community/phi-4-GGUF/phi-4-Q4_K_M.gguf`) with no sidecar
/// `config.json`. #1820 closed that specific gap — `GgufFactsReader` now
/// reads the architecture directly out of the GGUF binary's own metadata
/// header, so a phi-4-shaped GGUF prices as a MEASUREMENT today, not an
/// estimate and not unpriceable. What still lands here: a corrupt or
/// truncated GGUF download, an ambiguous multi-file directory the GGUF
/// reader declines to guess a shard from (see `gguf_facts`'s module docs),
/// or a weights format neither reader understands. The MLX-build remedy
/// below still applies whenever one exists.
///
/// Calls the SAME `model_ledger::gather()` the machine page's `/machine/
/// resources` endpoint uses, rather than re-deriving "unpriceable" from
/// `lms ps`/`lms ls` directly — the ledger's own compute is the one source
/// of truth for what counts as unpriceable (arch AND size fallback both
/// failed), so this check can never drift from what the page shows.
fn check_unpriceable_residents() -> Check {
    let ledger = darkmux_profiles::model_ledger::gather();
    unpriceable_residents_status(&ledger.models)
}

/// Pure decision for [`check_unpriceable_residents`], split out so every arm
/// is unit-testable without a live LMStudio / `vm_stat` / `sysctl` round
/// trip (same split as `utility_binding_status`).
fn unpriceable_residents_status(models: &[darkmux_profiles::model_ledger::ModelRow]) -> Check {
    let name = "resident pricing".to_string();
    let unpriceable: Vec<&str> = models
        .iter()
        .filter(|m| m.potential_bytes.is_none())
        .map(|m| m.model_key.as_str())
        .collect();
    if unpriceable.is_empty() {
        return Check {
            name,
            status: Status::Pass,
            message: "every resident model is priceable (measured arch facts or the #1819 size-based estimate)".into(),
            hint: None,
        };
    }
    Check {
        name,
        status: Status::Warn,
        message: format!(
            "{} resident model(s) genuinely unpriceable — no readable config.json, no readable GGUF header, AND no catalog size, so even the size-based estimate has nothing to work from: {} — the machine's fit verdict stays UNKNOWN while any of these are loaded",
            unpriceable.len(),
            unpriceable.join(", ")
        ),
        hint: Some(
            "darkmux tried a config.json, then the GGUF header, then a catalog-size estimate — none of the three had anything to work from. A corrupt/truncated download, an ambiguous multi-file GGUF directory (no unambiguous -00001-of- shard to read), or an unusual weights format neither reader understands are the likely causes. If an MLX build of the same model exists (check the LMStudio catalog for a `-mlx`/`-bit` variant), load that instead — MLX builds ship a config.json and price normally. Otherwise this resident's commitment is invisible to the machine page's totals and its fit verdict for the whole machine stays UNKNOWN for as long as it's loaded.".into(),
        ),
    }
}

/// (#1475 packet 1, #1547) Coherence of the machine-local role->profile map:
/// every role BOUND in `role_profiles` (config.json) must name BOTH a real role
/// id (#1547 — previously only the profile half was checked, so a binding on a
/// role id that doesn't exist reported Pass) AND a profile the registry
/// defines. A dangling binding WARNs, naming the offending role->profile pair +
/// the fix — so the operator learns a seat won't assemble BEFORE a dispatch
/// resolves it and fails, per the config-leniency contract (semantic
/// validation at resolution + doctor, never the hot load path — the same
/// discipline as `resolve_role_profile`'s loud error). An UNMAPPED role is NOT
/// a finding: it's the fresh-user floor (falls back to `default_profile`).
fn check_role_profiles() -> Check {
    let map = darkmux_types::config_access::role_profiles();
    // Only load the registry/role library when there's a binding to verify.
    if map.is_empty() {
        return role_profiles_status(
            &map,
            &std::collections::BTreeMap::new(),
            &std::collections::BTreeSet::new(),
            &std::collections::BTreeSet::new(),
        );
    }
    // (#1547) The role half: every role id darkmux can actually dispatch to
    // (user-defined + built-in), so a binding on a role id that doesn't exist
    // — e.g. the pre-#1547 doc examples' bare `judge`/`verify`/`probe-high`,
    // none of which are real role ids (the real ones are `review-judge`,
    // `review-verify`, `review-probe-high`) — is flagged instead of certified.
    let known_roles: std::collections::BTreeSet<String> = match darkmux_crew::loader::load_roles() {
        Ok(roles) => roles.into_iter().map(|r| r.id).collect(),
        Err(e) => {
            return Check {
                name: "role profiles".into(),
                status: Status::Warn,
                message: format!("can't verify the role->profile map (role library load failed: {e})"),
                hint: Some("Fix the role library (`darkmux role list`), then re-run.".into()),
            };
        }
    };
    match profiles::load_registry(None) {
        Ok(l) => {
            // A binding can be off-target for two DIFFERENT reasons that need
            // different fixes: the profile is genuinely absent (add it), or it
            // IS in profiles.json but its entry failed to parse and was
            // quarantined (fix the entry). The full load result holds both, so
            // pass the quarantined profile names through. (#1475)
            let quarantined: std::collections::BTreeSet<String> = l
                .registry
                .quarantined
                .iter()
                .filter(|q| q.kind == darkmux_types::QuarantinedEntryKind::Profile)
                .map(|q| q.name.clone())
                .collect();
            role_profiles_status(&map, &l.registry.profiles, &quarantined, &known_roles)
        }
        Err(e) => Check {
            name: "role profiles".into(),
            status: Status::Warn,
            message: format!("can't verify the role->profile map (profile registry load failed: {e})"),
            hint: Some("Fix the profile registry (`darkmux doctor` profile-registry check), then re-run.".into()),
        },
    }
}

/// Pure decision for `check_role_profiles`, split out so every arm is
/// unit-testable without a real config.json / registry. `known_profiles` is the
/// registry's DEFINED profiles; `quarantined` is the set of profile names whose
/// entry failed to parse (#1282 — absent from `known_profiles` but present in
/// profiles.json). `known_roles` is every role id darkmux can dispatch to
/// (#1547 — the role half of the pair, previously unchecked). A binding is
/// split by WHY it's off-target: an unknown ROLE (checked first — a binding on
/// a role id that doesn't exist can't resolve regardless of the profile side);
/// else a quarantined profile target gets a "fix the entry" hint (the profile
/// IS there, just broken); else a genuinely absent profile target keeps the
/// "add it / re-point it" hint. (#1475, #1547)
fn role_profiles_status(
    map: &std::collections::BTreeMap<String, String>,
    known_profiles: &std::collections::BTreeMap<String, darkmux_types::Profile>,
    quarantined: &std::collections::BTreeSet<String>,
    known_roles: &std::collections::BTreeSet<String>,
) -> Check {
    let name = "role profiles".to_string();
    if map.is_empty() {
        return Check {
            name,
            status: Status::Pass,
            message: "no role->profile bindings configured; unmapped roles use default_profile".into(),
            hint: Some(
                "Optional: bind a role to a profile with `darkmux config set role_profiles.<role> <profile>` (e.g. `role_profiles.review-judge qwen35b`). Profiles stay role-agnostic; the map welds a role to one on this machine. (#1475)".into(),
            ),
        };
    }
    // An off-target binding names a role id that doesn't exist, OR a profile
    // the registry doesn't DEFINE. Split by why: unknown role (checked first —
    // no profile-side wording is useful when the role itself can't resolve),
    // then quarantined (in profiles.json but broken) vs genuinely undefined.
    let mut unknown_role_pairs: Vec<(&String, &String)> = Vec::new();
    let mut quarantined_pairs: Vec<(&String, &String)> = Vec::new();
    let mut undefined_pairs: Vec<(&String, &String)> = Vec::new();
    for (role, profile) in map.iter() {
        // known_roles is empty only when the caller had no bindings to check
        // (the empty-map arm above returns before reaching here) — so an
        // empty set here means the role library itself was unavailable, which
        // check_role_profiles already turns into its own Warn before calling
        // this function; a real known_roles is always non-empty in practice.
        if !known_roles.contains(role.as_str()) {
            unknown_role_pairs.push((role, profile));
            continue;
        }
        if known_profiles.contains_key(profile.as_str()) {
            continue; // both halves defined + healthy
        }
        if quarantined.contains(profile.as_str()) {
            quarantined_pairs.push((role, profile));
        } else {
            undefined_pairs.push((role, profile));
        }
    }
    if unknown_role_pairs.is_empty() && quarantined_pairs.is_empty() && undefined_pairs.is_empty() {
        return Check {
            name,
            status: Status::Pass,
            message: format!(
                "{} role->profile binding{} — all name a real role and a defined profile",
                map.len(),
                if map.len() == 1 { "" } else { "s" }
            ),
            hint: None,
        };
    }
    let fmt_pairs = |pairs: &[(&String, &String)]| {
        pairs
            .iter()
            .map(|(role, profile)| format!("{role} -> {profile}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    // Compose message + hint over whichever kinds are present. The undefined
    // wording (and its "add the profile" hint) is preserved verbatim for
    // genuinely-absent targets; quarantined + unknown-role targets get their
    // own flavor.
    let mut msg_parts: Vec<String> = Vec::new();
    let mut hint_parts: Vec<String> = Vec::new();
    if !unknown_role_pairs.is_empty() {
        msg_parts.push(format!(
            "binding{} on an unknown role id: {}",
            if unknown_role_pairs.len() == 1 { "" } else { "s" },
            fmt_pairs(&unknown_role_pairs)
        ));
        hint_parts.push(
            "Check the role id against `darkmux role list` — a binding on a role id that doesn't exist can never resolve, no matter what profile it names.".into(),
        );
    }
    if !undefined_pairs.is_empty() {
        msg_parts.push(format!(
            "binding{} to an undefined profile: {}",
            if undefined_pairs.len() == 1 { "" } else { "s" },
            fmt_pairs(&undefined_pairs)
        ));
        hint_parts.push(
            "Point each undefined binding at a profile in `darkmux profile list`, or add the profile to profiles.json — `darkmux config set role_profiles.<role> <profile>`.".into(),
        );
    }
    if !quarantined_pairs.is_empty() {
        msg_parts.push(format!(
            "binding{} to a quarantined profile: {}",
            if quarantined_pairs.len() == 1 { "" } else { "s" },
            fmt_pairs(&quarantined_pairs)
        ));
        hint_parts.push(
            "A quarantined target IS in profiles.json but its entry failed to parse — fix the profile entry (see the profile-registry check), don't re-point the binding.".into(),
        );
    }
    hint_parts.push(
        "Until fixed, resolving that role errors (it does NOT silently fall back to default_profile). (#1475)".into(),
    );
    Check {
        name,
        status: Status::Warn,
        message: format!("role->profile {}", msg_parts.join("; ")),
        hint: Some(hint_parts.join(" ")),
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
            hint: Some("If your Redis requires auth, store the password: `security add-generic-password -a $USER -s darkmux-redis -w` (URL-safe).".into()),
        },
    }
}

/// (#1685) Surface the `gh`-verb allowlist gate's resolved state —
/// `enabled` + the allowed verb list, with provenance (env vs config vs
/// default), so an operator wondering "why did `pr-merge` refuse to run"
/// can see the answer from `darkmux doctor` without reading
/// `~/.darkmux/config.json` by hand. Never touches GitHub or `gh` itself —
/// this only reads darkmux's OWN config surface (`CmdConfig`'s doc).
fn check_gh_allowlist() -> Check {
    let name = "gh verb allowlist";
    let env_enabled = std::env::var("DARKMUX_CMD_ENABLED").ok().filter(|s| !s.trim().is_empty()).is_some();
    let env_allowed = std::env::var("DARKMUX_CMD_ALLOWED").ok().filter(|s| !s.trim().is_empty()).is_some();
    let enabled = darkmux_types::config_access::cmd_enabled();
    let allowed = darkmux_types::config_access::cmd_allowed_verbs();
    let provenance = if env_enabled || env_allowed { "env" } else { "config.json" };
    if !enabled {
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: format!("disabled ({provenance}) — every cmd-declaring panel command refuses to run"),
            hint: None,
        };
    }
    if allowed.is_empty() {
        return Check {
            name: name.into(),
            status: Status::Warn,
            message: "cmd.enabled=true but cmd.allowed is empty — every cmd-declaring panel command still refuses (a verb absent from the list is blocked even with the gate on)".into(),
            hint: Some("`darkmux config set cmd.allowed <comma-separated-verb-list>` — e.g. pr-list,pr-info,pr-approve,pr-merge — matching each config's own `cmd` field.".into()),
        };
    }
    Check {
        name: name.into(),
        status: Status::Pass,
        message: format!("enabled ({provenance}) — allowed: {}", allowed.join(", ")),
        hint: None,
    }
}

/// (#2093) Surface the flow-record hook sink's resolved state: whether it's
/// enabled (with provenance), and — when it is — every configured rule's
/// match + URL + undelivered-line count, flagging a rule whose match is
/// empty (Warn — matches nothing, likely an operator forgot to fill it in)
/// or whose URL isn't loopback (Fail — `HookSink::new` refuses the whole
/// sink over this, so it's a hard block, not a suggestion).
///
/// (#2093 merge-gate finding 14) Returns ONE `Check` per flagged rule
/// (`hooks.rule.<index>`), not a single aggregate — so a flag attaches
/// to the rule it names in the checks list itself, the same shape
/// `eureka_checks()` already established for a check family with more
/// than one member. Provenance distinguishes `env` / `config.json` /
/// `default` (mirrors `check_review_judge_exhaustion_policy`'s own
/// three-way provenance) — previously any non-`env` case was reported as
/// `config.json` even when NEITHER tier actually set it.
fn check_hooks() -> Vec<Check> {
    let env_set = std::env::var("DARKMUX_HOOKS_ENABLED").ok().filter(|s| !s.trim().is_empty()).is_some();
    let config_set = darkmux_types::config::DarkmuxConfig::load_resolved().hooks.as_ref().and_then(|h| h.enabled).is_some();
    let provenance = if env_set {
        "env"
    } else if config_set {
        "config.json"
    } else {
        "default"
    };
    let enabled = darkmux_types::config_access::hooks_enabled();
    let rules = darkmux_types::config_access::hooks_rules();
    let outbox_dir = darkmux_types::config_access::hooks_outbox_dir();
    build_hooks_check(enabled, provenance, &rules, &outbox_dir)
}

/// (#2093 merge-gate finding 17) True when a rule's match risks the
/// observer joining the observed (this project's own doctrine,
/// CLAUDE.md's "The observer must not join the observed") — matching
/// `telemetry.*` / category `telemetry`, or a bare `*` action that
/// (among everything else) would also catch every telemetry record.
/// String-matching against `describe_match`'s rendered form since
/// `HookRuleSummary` carries only the description, not the structured
/// `HookMatch` — good enough for a doctor Warn, not a security boundary.
fn hooks_match_risks_observing_the_observer(match_desc: &str) -> bool {
    match_desc.contains("category=telemetry") || match_desc.contains("action=telemetry.") || match_desc == "action=*"
}

/// (#2093 merge-gate finding 15) `*.outbox.jsonl` files in `outbox_dir`
/// whose key (the content-hash `rule_key` — see `darkmux_flow::hooks`'
/// own doc) matches no CURRENTLY-configured rule. Belongs to a rule
/// since removed from config (or edited enough to change its
/// `match`/`http`) — the outbox still holds whatever was undelivered
/// when that happened, and nothing will ever drain it again unless the
/// rule comes back verbatim.
/// (fix-round finding 6) One stray `*.outbox.jsonl` file — one whose
/// owning rule no longer exists in current config — plus the detail an
/// operator deciding "safe to delete?" actually needs: how many lines
/// were never delivered, and which sibling sidecar files (all sharing
/// the same content-hash key) go with it.
struct StrayOutbox {
    path: std::path::PathBuf,
    undelivered: usize,
    siblings: Vec<String>,
}

/// Sibling sidecar suffixes a stray outbox's key can carry — see
/// `darkmux_flow::hooks`'s per-rule file layout (`outbox_paths`,
/// `last_status_path`, `dropped_appends_path`, `drain_lock_path`,
/// `quarantine_path`).
const HOOK_SIDECAR_SUFFIXES: &[&str] = &[".cursor", ".last", ".dropped", ".drain.lock", ".outbox.jsonl.quarantine"];

fn stray_outbox_files(rules: &[darkmux_types::config::HookRule], outbox_dir: &std::path::Path) -> Vec<StrayOutbox> {
    // (#2183) Reuse `summarize_configured_rules`'s OWN key derivation
    // (`.key`) rather than recomputing `rule_key` by hand from `r.http`
    // alone — a `file`-transport rule has no `http`, so hand-rolling this
    // from `r.http.unwrap_or_default()` would key every `file` rule on
    // the empty string and misreport its real outbox as stray.
    let current_keys: std::collections::HashSet<String> =
        darkmux_flow::hooks::summarize_configured_rules(rules, outbox_dir).into_iter().map(|s| s.key).collect();
    let Ok(entries) = std::fs::read_dir(outbox_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter_map(|path| {
            let name = path.file_name().and_then(|n| n.to_str())?;
            let key = name.strip_suffix(".outbox.jsonl")?;
            if current_keys.contains(key) {
                return None;
            }
            // The stray file's own `.cursor` sidecar (if it survived
            // alongside it) still names the true last-delivered offset;
            // falling back to 0 (nothing ever delivered) only overcounts
            // when that sidecar is itself missing.
            let cursor = std::fs::read_to_string(outbox_dir.join(format!("{key}.cursor")))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            let undelivered = darkmux_flow::hooks::undelivered_line_count(&path, cursor);
            let siblings: Vec<String> = HOOK_SIDECAR_SUFFIXES
                .iter()
                .map(|suffix| format!("{key}{suffix}"))
                .filter(|sibling_name| outbox_dir.join(sibling_name).exists())
                .collect();
            Some(StrayOutbox { path, undelivered, siblings })
        })
        .collect()
}

/// The pure rollup `check_hooks()` delegates to — split out so it's testable
/// against synthetic rules without the global config/env tier (#811 empties
/// `config()` in test builds, so there's no way to inject `hooks.rules`
/// through the real accessor path in a unit test).
fn build_hooks_check(
    enabled: bool,
    provenance: &str,
    rules: &[darkmux_types::config::HookRule],
    outbox_dir: &std::path::Path,
) -> Vec<Check> {
    let name = "hooks";
    if !enabled {
        return vec![Check { name: name.into(), status: Status::Pass, message: format!("disabled ({provenance})"), hint: None }];
    }
    if rules.is_empty() {
        return vec![Check {
            name: name.into(),
            status: Status::Warn,
            message: format!("enabled ({provenance}) but no rules configured — outbox_dir={}", outbox_dir.display()),
            hint: Some(
                "Add a rule to config.json's `hooks.rules`, e.g. `darkmux config set hooks.rules \
                 '[{\"match\":{\"action\":\"dispatch.tool\",\"payload.tool_name\":\"create_finding\",\
                 \"payload.ok\":true},\"http\":\"http://127.0.0.1:8790/events\"}]'`."
                    .into(),
            ),
        }];
    }

    let summaries = darkmux_flow::hooks::summarize_configured_rules(rules, outbox_dir);
    let mut worst = Status::Pass;
    let mut overview_lines = Vec::with_capacity(summaries.len());
    let mut rule_checks = Vec::with_capacity(summaries.len());

    for s in &summaries {
        let mut flags = Vec::new();
        let mut rule_status = Status::Pass;
        if s.is_empty_match {
            flags.push("EMPTY MATCH — matches nothing".to_string());
            rule_status = Status::Warn;
        }
        // (#2135 option 2) A URL satisfying NEITHER the loopback nor the
        // tailnet policy is what `HookSink::new` refuses the whole sink
        // over — a valid tailnet rule (`is_tailnet: true`) is NOT this
        // case and must not read as broken.
        if s.is_refused {
            flags.push("URL REFUSED — neither loopback nor a Tailscale address; refused at load".to_string());
            rule_status = Status::Fail;
        }
        // (#2135 option 2) An unsigned TAILNET target is fine inside the
        // tailnet (WireGuard already authenticates + encrypts the peer),
        // but the receiver has no way to attribute the record's sender
        // beyond the body itself — worth a Warn, not a Fail.
        if s.is_tailnet && !s.signed {
            flags.push(
                "TAILNET TARGET, UNSIGNED — attribution is unsigned; fine inside the tailnet, required beyond it"
                    .to_string(),
            );
            if rule_status == Status::Pass {
                rule_status = Status::Warn;
            }
        }
        // (#2093 merge-gate finding 9) A rule that's been dropping writes
        // (over the outbox cap, or an append failure) is a Warn — not a
        // Fail, since delivery for every OTHER pending line keeps working.
        if s.dropped_appends > 0 {
            flags.push(format!(
                "{} write(s) dropped so far (over the outbox cap, or an append failure)",
                s.dropped_appends
            ));
            if rule_status == Status::Pass {
                rule_status = Status::Warn;
            }
        }
        // (fix-round finding 1) A STALLED rule has stopped attempting
        // deliveries entirely — surfaced loudly, same severity as the
        // other operational (not config-validation) flags here.
        if s.stalled {
            flags.push(format!(
                "STALLED — {} consecutive cursor-write failure(s); the drainer has stopped attempting new \
                 deliveries for this rule until its cursor file becomes writable again",
                s.cursor_write_failures
            ));
            if rule_status == Status::Pass {
                rule_status = Status::Warn;
            }
        }
        // (fix-round finding 7) Quarantined (invalid-JSON) lines are
        // never redelivered — worth naming, same as a dropped append.
        if s.quarantined_lines > 0 {
            flags.push(format!("{} line(s) quarantined (invalid JSON — never redelivered)", s.quarantined_lines));
            if rule_status == Status::Pass {
                rule_status = Status::Warn;
            }
        }
        if hooks_match_risks_observing_the_observer(&s.match_desc) {
            flags.push(
                "matches telemetry / a bare `*` action — the observer must not join the observed".to_string(),
            );
            if rule_status == Status::Pass {
                rule_status = Status::Warn;
            }
        }
        if worst == Status::Pass && rule_status != Status::Pass {
            worst = rule_status;
        } else if rule_status == Status::Fail {
            worst = Status::Fail;
        }

        // (#2183) A `transform` that failed to load is a load-time
        // refusal SCOPED TO THIS RULE (`HookSink::new` disables just this
        // rule, the rest of the sink keeps running) — Fail here too, so
        // the row that's actually broken is the one operator sees red,
        // without the whole `hooks` check reading as catastrophic.
        let transform_suffix = match (&s.transform_name, &s.transform_status) {
            (Some(name), Some(Ok(hash))) => format!(", transform: {name} (sha256:{hash})"),
            (Some(name), Some(Err(reason))) => {
                flags.push(format!("TRANSFORM `{name}` FAILED TO LOAD — {reason}"));
                rule_status = Status::Fail;
                format!(", transform: {name} [FAILED]")
            }
            _ => String::new(),
        };
        if worst == Status::Pass && rule_status != Status::Pass {
            worst = rule_status;
        } else if rule_status == Status::Fail {
            worst = Status::Fail;
        }

        let flag_str = if flags.is_empty() { String::new() } else { format!(" [{}]", flags.join("; ")) };
        // (#2135 option 2) `loopback`/`tailnet`/`refused` + `signed`/
        // `unsigned` — the visibility the operator's design asked for in
        // place of a config gate: the URL is the decision, this row is
        // what makes it legible. (#2183) `file` names the no-network
        // testing-tier transport instead — there's no URL policy or
        // signature to report for it.
        let target_kind = if s.is_file {
            "file"
        } else if s.is_loopback {
            "loopback"
        } else if s.is_tailnet {
            "tailnet"
        } else {
            "refused"
        };
        let signed = if s.is_file { "n/a" } else if s.signed { "signed" } else { "unsigned" };
        let message = format!(
            "{} -> {} [{target_kind}, {signed}]{transform_suffix} (undelivered: {}){flag_str}",
            s.match_desc, s.url, s.undelivered
        );
        overview_lines.push(format!("  #{}: {message}", s.index));
        rule_checks.push(Check {
            name: format!("hooks.rule.{}", s.index),
            status: rule_status,
            message,
            hint: if flags.is_empty() {
                None
            } else {
                Some("Fix this rule in ~/.darkmux/config.json (or `darkmux config set hooks.rules ...`).".into())
            },
        });
    }

    let overview = Check {
        name: name.into(),
        status: worst,
        message: format!(
            "enabled ({provenance}) — {} rule(s), outbox_dir={}\n{}",
            summaries.len(),
            outbox_dir.display(),
            overview_lines.join("\n")
        ),
        hint: if worst != Status::Pass {
            Some("See the individual `hooks.rule.*` checks below for which rule(s).".into())
        } else {
            None
        },
    };
    let mut out = vec![overview];
    out.extend(rule_checks);

    // (#2093 merge-gate finding 15) A file that belongs to no CURRENT
    // rule — named so, rather than silently taking up disk forever.
    let stray = stray_outbox_files(rules, outbox_dir);
    if !stray.is_empty() {
        // (fix-round finding 6) Name each stray file's undelivered line
        // count and its sibling sidecars — an operator deciding whether
        // it's "safe to delete" needs both, not just the outbox name.
        let details: Vec<String> = stray
            .iter()
            .map(|s| {
                let name = s.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let siblings =
                    if s.siblings.is_empty() { String::new() } else { format!("; siblings: {}", s.siblings.join(", ")) };
                format!("{name} ({} undelivered line(s){siblings})", s.undelivered)
            })
            .collect();
        out.push(Check {
            name: "hooks.stray".into(),
            status: Status::Warn,
            message: format!("{} outbox file(s) belong to no currently-configured rule: {}", stray.len(), details.join(", ")),
            hint: Some(
                "A rule was removed or edited since these were written. `darkmux flow drain --file <path> \
                 --to <loopback url>` delivers a stray file's undelivered lines before you delete it; once \
                 undelivered is 0, it (and its sibling sidecars) are safe to remove."
                    .into(),
            ),
        });
    }

    out
}

/// (#1876/#1877; #2310 P4d; #2404 P4d round 3) The `review{}` config block
/// (`judge_fail_on_any_skip` / `judge_concurrency`) was REMOVED from
/// `DarkmuxConfig` in CONFIG_SCHEMA_VERSION 1.22 — the review funnel those
/// knobs tuned was deleted in #2310 P4d, and darkmux is pre-1.0 (no
/// deprecate-in-place; remove outright). Because the field is gone, a
/// `config.json` still carrying a `review` key lands it in the top-level
/// `extras` overflow (lenient-on-read) instead of a typed field — this
/// check looks THERE, not at a typed accessor that no longer exists.
///
/// `Pass` when `extras` has no `review` key at all (the common case, and
/// the case `DarkmuxConfig::with_defaults()` must produce — a regression
/// here is exactly what let the round-2 field survive one review pass).
/// `Warn`, naming the key, when an old config still has it.
fn check_review_judge_exhaustion_policy() -> Check {
    let name = "review.judge_* (removed)";
    let cfg = darkmux_types::config::DarkmuxConfig::load_resolved();
    if !cfg.extras.contains_key("review") {
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: "not present".into(),
            hint: None,
        };
    }
    Check {
        name: name.into(),
        status: Status::Warn,
        message: format!(
            "config.json has a `review` key — removed in CONFIG {}; delete it from config.json",
            darkmux_types::config::CONFIG_SCHEMA_VERSION
        ),
        hint: Some(
            "the review funnel this block configured was deleted in #2310 P4d; remove the \
             `review` block from ~/.darkmux/config.json — it is read leniently but has no effect"
                .into(),
        ),
    }
}

/// (#2361, swarm S4-4) Informational: the bound on ONE operator-supplied
/// shell command a step runs — `mods.gate`'s `test_command` and
/// `procedural.shell`'s `command`. Always `Pass` (a preference, not a
/// health signal); surfaces the resolved value with provenance so an
/// operator whose gate reported `test_command exceeded <n>s` can see which
/// tier set that number without reading `config.json`. Mirrors
/// `check_review_judge_exhaustion_policy`'s provenance-display shape.
fn check_step_command_timeout() -> Check {
    let name = "runtime.step_command_timeout_seconds";
    let env_set = std::env::var("DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS")
        .ok()
        .is_some_and(|s| !s.trim().is_empty());
    let cfg_set = darkmux_types::config::DarkmuxConfig::load_resolved()
        .runtime
        .and_then(|r| r.step_command_timeout_seconds)
        .is_some();
    let provenance = if env_set {
        "from DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS env"
    } else if cfg_set {
        "from config.json"
    } else {
        "default"
    };
    let seconds = darkmux_types::config_access::step_command_timeout_seconds();
    // (#2310 fix-loop E2, from the loop-D review) `0` is UNBOUNDED, the same
    // reading every other darkmux zero-knob has — see
    // `darkmux_crew::bounded_command::configured_timeout`. Said out loud
    // here because the previous behavior was the opposite ("kill instantly"),
    // and an operator who set `0` deserves to see which one they got.
    if seconds == 0 {
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: format!(
                "0s ({provenance}) — unbounded; a step's shell command (mods.gate's \
                 test_command, procedural.shell) runs until it exits or darkmux is interrupted"
            ),
            hint: None,
        };
    }
    Check {
        name: name.into(),
        status: Status::Pass,
        message: format!(
            "{seconds}s ({provenance}) — a step's shell command (mods.gate's test_command, \
             procedural.shell) is killed at this bound, process group and all"
        ),
        hint: None,
    }
}

/// (#2394) Informational: how many DISPATCH-FREE steps the scheduler runs
/// at once — every step whose `StepKind::seat` claims `SeatClaim::NoModel`
/// (`procedural.shell`, `procedural.noop`, `mods.gate`, `records.gather`,
/// `deliver.github_review`). Always `Pass` (a preference, not a health
/// signal); surfaces the resolved value with provenance so an operator
/// watching a wave of shell steps can see which tier set that number
/// without reading `config.json`. Mirrors `check_step_command_timeout`'s
/// provenance-display shape exactly.
///
/// Said out loud in the message: this is NOT `remote.concurrent_cap`. The
/// two were the same number before #2394 only because dispatch-free steps
/// had no seat class of their own, which is the bug.
fn check_dispatch_free_concurrency() -> Check {
    let name = "runtime.dispatch_free_concurrency";
    let env_set = std::env::var("DARKMUX_DISPATCH_FREE_CONCURRENCY")
        .ok()
        .is_some_and(|s| !s.trim().is_empty());
    let cfg_set = darkmux_types::config::DarkmuxConfig::load_resolved()
        .runtime
        .and_then(|r| r.dispatch_free_concurrency)
        .is_some();
    let provenance = if env_set {
        "from DARKMUX_DISPATCH_FREE_CONCURRENCY env"
    } else if cfg_set {
        "from config.json"
    } else {
        "default"
    };
    let n = darkmux_types::config_access::dispatch_free_concurrency();
    Check {
        name: name.into(),
        status: Status::Pass,
        message: format!(
            "{n} ({provenance}) — dispatch-free steps (procedural.shell/noop, mods.gate, \
             records.gather, deliver.github_review) run this many at a time, on their own \
             track; the hosted-endpoint cap (remote.concurrent_cap) does not govern them"
        ),
        hint: None,
    }
}

/// (#2094) Surface the resolved `runtime.turn_delay_ms` with provenance —
/// the global inter-turn rest, in milliseconds, the internal runtime
/// sleeps between inference turns on every local dispatch (GPU thermal /
/// power relief for sustained runs). Always Pass at `0` (informational —
/// the pre-existing no-rest behavior, not a defect) and at any value below
/// the inactivity timeout.
///
/// Warns (advice, never a gate — operator sovereignty #44) when the
/// configured value is AT OR ABOVE HALF the inactivity timeout (#2094
/// second round, finding 4 — the runtime's own clamp band, widened from
/// "at the full timeout" so a rest plus a real turn's latency plus the
/// tailer's polling overhead can never approach the deadline): the
/// runtime clamps it to half the timeout rather than honoring it verbatim
/// (see `runtime/src/loop_runner.rs`), so a value the operator actually
/// meant would otherwise silently become a different number with nothing
/// here to say so before the first dispatch discovers it via a stderr
/// line.
///
/// No laptop-class hardware signal exists in `darkmux-hardware` today (only
/// `Platform` + `RamTier`, no chassis/battery detection) — the issue's
/// "warn on a laptop with 0" clause is deliberately not implemented; the
/// issue itself names this as conditional ("if the hardware crate exposes
/// that cheaply"). Showing the resolved value is what's left.
fn check_turn_delay() -> Check {
    let name = "runtime.turn_delay_ms";
    // (#2094 finding 9) `env_raw` is the RAW string, if the env var is set
    // to anything non-empty at all — distinct from whether it actually
    // PARSED. `config_access::turn_delay_ms()` (below) silently falls
    // through to the config/default tier on a parse failure
    // (`pick_parsed`'s contract), so a set-but-garbage env var was
    // previously reported as `"from DARKMUX_TURN_DELAY_MS env"` while the
    // resolved `ms` value actually came from a LOWER tier — provenance
    // and value disagreeing with nothing here to say so.
    let env_raw = std::env::var("DARKMUX_TURN_DELAY_MS")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let env_parses = env_raw.as_deref().is_some_and(|s| s.trim().parse::<u64>().is_ok());
    let cfg_set = darkmux_types::config::DarkmuxConfig::load_resolved()
        .runtime
        .and_then(|r| r.turn_delay_ms)
        .is_some();
    let provenance = if env_parses {
        "from DARKMUX_TURN_DELAY_MS env"
    } else if cfg_set {
        "from config.json"
    } else {
        "default"
    };
    let ms = darkmux_types::config_access::turn_delay_ms();
    let timeout_ms = darkmux_types::config_access::inactivity_timeout_seconds().saturating_mul(1000);
    // An env var IS set but did not parse as an integer — this is a
    // config mistake, not silence, and it must say so rather than quietly
    // reporting whatever lower tier resolved instead.
    if let Some(raw) = env_raw.as_deref() {
        if !env_parses {
            return Check {
                name: name.into(),
                status: Status::Warn,
                message: format!(
                    "DARKMUX_TURN_DELAY_MS=`{raw}` is not an integer; using {ms}ms ({provenance})"
                ),
                hint: Some(
                    "Set DARKMUX_TURN_DELAY_MS to a plain integer number of milliseconds \
                     (e.g. `3000`), or unset it to fall through to config.json / the default."
                        .into(),
                ),
            };
        }
    }
    if ms == 0 {
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: format!("0ms ({provenance}) — no inter-turn rest"),
            hint: None,
        };
    }
    if ms.saturating_mul(2) >= timeout_ms {
        let clamped = timeout_ms / 2;
        return Check {
            name: name.into(),
            status: Status::Warn,
            message: format!(
                "{ms}ms ({provenance}) is at or above half the inactivity timeout ({timeout_ms}ms) — \
                 the runtime clamps it to {clamped}ms (half the timeout) rather than honoring it verbatim"
            ),
            hint: Some(
                "Lower `runtime.turn_delay_ms` well below the inactivity timeout, or raise \
                 `runtime.inactivity_timeout_seconds` if the longer rest is intentional."
                    .into(),
            ),
        };
    }
    Check {
        name: name.into(),
        status: Status::Pass,
        message: format!("{ms}ms ({provenance}) — rest between inference turns on every local dispatch"),
        hint: None,
    }
}

/// (#2165) Surface the resolved `runtime.reasoning_checkpoint_interval_tokens`
/// with provenance — the #1221 mid-turn check-in rate the internal runtime
/// samples a thought against, distinct from `runtime.max_tokens_per_call`
/// (which bounds an ANSWER and wants to be LARGE; this one samples a
/// THOUGHT and wants to be SMALL). Every other runtime knob the container
/// receives already has a doctor row (`runtime.turn_delay_ms` above,
/// `runtime.inactivity_timeout_seconds` folded into that check's `timeout_ms`
/// read, `runtime.host_sampler_interval_ms`, `runtime.thermal.*`) — this one
/// didn't, so a fresh dispatch's cap-hit stderr line ("hit the reasoning
/// check-in interval (built-in 1000)", #2165) named a knob `doctor` couldn't
/// confirm the resolved value or tier for.
///
/// Always Pass — informational, like `check_turn_delay`'s `0ms` case. There
/// is no bad value here (the runtime clamps nothing, unlike the turn-delay/
/// inactivity-timeout interaction), so this is a "know your own knobs" row,
/// not a health gate.
fn check_reasoning_checkpoint_interval() -> Check {
    let name = "runtime.reasoning_checkpoint_interval_tokens";
    let (value, source) =
        darkmux_types::config_access::reasoning_checkpoint_interval_tokens_with_source();
    let (shown, provenance) = match value {
        Some(n) => (n, source.as_str()),
        // `None` means the runtime's own built-in literal governs (
        // `REASONING_CHECKPOINT_INTERVAL = 1000`, `runtime/src/loop_runner.rs`)
        // — darkmux-doctor can't import the runtime crate (it's outside the
        // workspace, see `runtime/Cargo.toml`'s own doc), so the built-in
        // value is named here rather than re-derived from a shared constant.
        None => (1000, "built-in"),
    };
    Check {
        name: name.into(),
        status: Status::Pass,
        message: format!(
            "{shown} tokens ({provenance}) — how far the model reasons between the \
             runtime's mid-turn check-ins (#1221)"
        ),
        hint: None,
    }
}

/// (#2190) Surface the resolved `runtime.max_stall_recoveries` with
/// provenance — the budget of intra-turn stall recoveries (empty
/// `tool_calls`, or a runaway-reasoning cut) the internal runtime spends
/// before escalating out of local-tier. Live evidence for why this needed a
/// doctor row: a Devstral dispatch hit the same "finish_reason=tool_calls
/// with no tool_calls" shape on three consecutive turns at ~19k context and
/// died with a hard-coded budget of 2 that no config surface could show or
/// override.
///
/// Always Pass — informational, same shape as
/// `check_reasoning_checkpoint_interval` above.
fn check_max_stall_recoveries() -> Check {
    let name = "runtime.max_stall_recoveries";
    let (value, source) = darkmux_types::config_access::max_stall_recoveries_with_source();
    let (shown, provenance) = match value {
        Some(n) => (n, source.as_str()),
        // `None` means the runtime's own built-in literal governs
        // (`MAX_STALL_RECOVERIES = 2`, `runtime/src/loop_runner.rs`) —
        // darkmux-doctor can't import the runtime crate (outside the
        // workspace), so the built-in value is named here rather than
        // re-derived from a shared constant.
        None => (2, "built-in"),
    };
    Check {
        name: name.into(),
        status: Status::Pass,
        message: format!(
            "{shown} recoveries ({provenance}) — how many useless turns (empty tool_calls, \
             or a runaway-reasoning cut) the runtime tolerates before escalating out of \
             local-tier (#2190)"
        ),
        hint: None,
    }
}

/// (#2107, #1833) Surface the resolved `runtime.host_sampler_interval_ms`
/// with provenance — the cadence `darkmux serve`'s daemon-side continuous
/// host sampler runs at, feeding the machine stats drawer's live
/// `/machine/resources` `load` block between dispatches. Always Pass: `0`
/// is an honest opt-out (the sampler simply doesn't start, same convention
/// as `runtime.turn_delay_ms`'s `0`), not a defect. Mirrors
/// `check_turn_delay`'s provenance-first shape exactly, minus that check's
/// clamp-warn branch (this knob isn't clamped against anything else).
fn check_host_sampler_interval() -> Check {
    let name = "runtime.host_sampler_interval_ms";
    let env_raw = std::env::var("DARKMUX_HOST_SAMPLER_INTERVAL_MS")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let env_parses = env_raw.as_deref().is_some_and(|s| s.trim().parse::<u64>().is_ok());
    let cfg_set = darkmux_types::config::DarkmuxConfig::load_resolved()
        .runtime
        .and_then(|r| r.host_sampler_interval_ms)
        .is_some();
    let provenance = if env_parses {
        "from DARKMUX_HOST_SAMPLER_INTERVAL_MS env"
    } else if cfg_set {
        "from config.json"
    } else {
        "default"
    };
    let ms = darkmux_types::config_access::host_sampler_interval_ms();
    if let Some(raw) = env_raw.as_deref() {
        if !env_parses {
            return Check {
                name: name.into(),
                status: Status::Warn,
                message: format!(
                    "DARKMUX_HOST_SAMPLER_INTERVAL_MS=`{raw}` is not an integer; using {ms}ms ({provenance})"
                ),
                hint: Some(
                    "Set DARKMUX_HOST_SAMPLER_INTERVAL_MS to a plain integer number of \
                     milliseconds (e.g. `5000`), or unset it to fall through to config.json / \
                     the default."
                        .into(),
                ),
            };
        }
    }
    if ms == 0 {
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: format!(
                "0ms ({provenance}) — daemon host sampler disabled; the machine stats drawer \
                 shows live numbers only while a dispatch's own per-dispatch sampler is running"
            ),
            hint: None,
        };
    }
    Check {
        name: name.into(),
        status: Status::Pass,
        message: format!(
            "{ms}ms ({provenance}) — darkmux serve's daemon-side host sampler cadence for the \
             machine stats drawer"
        ),
        hint: None,
    }
}

/// (#2111) Surface the resolved `runtime.telemetry_record_every_samples`
/// with provenance — how many dispatch-sampler ticks (2s cadence) between
/// `machine.telemetry` periodic SAMPLE flow records, alongside
/// `machine.thermal`'s TRANSITION events. Always Pass: `0` is an honest
/// opt-out (the periodic curve simply isn't written; the sampler itself,
/// the thermal governor, and `dispatch complete`'s `host_window` summary
/// are all unaffected), not a defect — same shape as
/// `check_host_sampler_interval`'s `0` case.
fn check_telemetry_record_every_samples() -> Check {
    let name = "runtime.telemetry_record_every_samples";
    let (value, source) =
        darkmux_types::config_access::telemetry_record_every_samples_with_source();
    let provenance = source.as_str();
    if value == 0 {
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: format!(
                "0 ({provenance}) — the periodic machine.telemetry curve is disabled; \
                 machine.thermal transitions and dispatch complete's host_window are unaffected"
            ),
            hint: None,
        };
    }
    // (#2111 review finding) Derived from the sampler's own constant
    // rather than a hardcoded literal, so this message can't silently
    // drift from the real tick if that constant ever changes.
    let cadence_ms = value.saturating_mul(darkmux_crew::dispatch_internal::TELEMETRY_SAMPLE_INTERVAL_MS);
    Check {
        name: name.into(),
        status: Status::Pass,
        message: format!(
            "every {value} sample(s) ({provenance}) — the machine.telemetry periodic \
             host-pressure curve's cadence (≈{}s at the dispatch sampler's own tick)",
            cadence_ms / 1000
        ),
        hint: None,
    }
}

/// (#2171) Surface the resolved `runtime.generation_checkpoint_interval_tokens`
/// with provenance — the GENERATION check-in that bounds every dispatch call
/// that does NOT carry the reasoning bound (`reasoning_checkpoint_interval_tokens`),
/// not just reasoning ones. Fixes the Devstral inactivity-timeout kill: a
/// non-thinking model's whole answer/tool-call turn used to carry the raw
/// 10000-token answer bound with no check-in at all once #2164 gated the
/// reasoning check-in on the dispatch having proven it reasons.
///
/// (merge-gate review, item 1) Unlike its siblings, this knob DOES have real
/// too-high/too-low hazards, cross-checked against two OTHER resolved
/// settings:
///
/// 1. `0` is not a "disabled" value — the runtime CLI rejects it outright
///    (`--generation-checkpoint-interval` requires `n > 0`, `std::process::
///    exit(2)`) — so setting it silently breaks EVERY dispatch rather than
///    opting out of anything. The real off-switch is setting the interval
///    at or above `max_tokens_per_call` (case 2), which the cap-selection
///    logic already treats as "not actually the binding cap."
/// 2. At or above `max_tokens_per_call` (the answer bound) means the
///    generation check-in can never be the tighter cap — it's silently
///    disabled, and the ORIGINAL failure this PR fixes (a non-thinking
///    model's call outlasting the inactivity budget) is back.
/// 3. A generation interval large enough that a single call could plausibly
///    run silently (no streamed chunks — the LMStudio buffering shape
///    that caused the #2171 incident) longer than the inactivity budget.
///    `interval_tokens / 10 tok/s` is a CONSERVATIVE (slow) generation-rate
///    floor — the #2171 incident measured Devstral at ~10-20 tok/s on an
///    M1 Max, and a dense 70B on the same hardware runs slower still — so
///    this warns before an operator's own hardware/model choice reproduces
///    the incident even with the fix merged.
fn check_generation_checkpoint_interval() -> Check {
    let name = "runtime.generation_checkpoint_interval_tokens";
    let env_raw = std::env::var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let env_parses = env_raw.as_deref().is_some_and(|s| s.trim().parse::<u32>().is_ok());
    let cfg_set = darkmux_types::config::DarkmuxConfig::load_resolved()
        .runtime
        .and_then(|r| r.generation_checkpoint_interval_tokens)
        .is_some();
    let provenance = if env_parses {
        "from DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL env"
    } else if cfg_set {
        "from config.json"
    } else {
        "default"
    };
    // (#2171) No runtime built-in constant is reachable from this crate (it
    // lives in the separate, non-workspace `runtime/` crate) — 4000 is
    // mirrored from `runtime::loop_runner::GENERATION_CHECKPOINT_INTERVAL`.
    // Keep the two in sync by hand if that constant ever changes. 10000
    // mirrors `runtime::loop_runner::MAX_TOKENS_PER_CALL` the same way —
    // `max_tokens_per_call()` resolves `None` = that same built-in.
    const RUNTIME_BUILTIN_DEFAULT: u32 = 4000;
    const ANSWER_BOUND_BUILTIN_DEFAULT: u32 = 10_000;
    // (merge-gate review, item 1) The conservative tokens/sec floor this
    // knob is cross-checked against — see the fn doc's point 3.
    const CONSERVATIVE_TOKENS_PER_SECOND: f64 = 10.0;
    let tokens = darkmux_types::config_access::generation_checkpoint_interval_tokens()
        .unwrap_or(RUNTIME_BUILTIN_DEFAULT);
    if let Some(raw) = env_raw.as_deref() {
        if !env_parses {
            return Check {
                name: name.into(),
                status: Status::Warn,
                message: format!(
                    "DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL=`{raw}` is not a positive \
                     integer; using {tokens} ({provenance})"
                ),
                hint: Some(
                    "Set DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL to a positive integer \
                     token count (e.g. `4000`), or unset it to fall through to config.json / \
                     the default."
                        .into(),
                ),
            };
        }
    }
    if tokens == 0 {
        return Check {
            name: name.into(),
            status: Status::Warn,
            message: format!(
                "{tokens} ({provenance}) is not a valid interval — the runtime CLI rejects a \
                 zero generation-checkpoint interval outright, so every dispatch that reaches \
                 this value exits with code 2 before doing any work"
            ),
            hint: Some(
                "0 is not an off-switch. To disable the generation check-in (fall back to the \
                 raw answer bound), set `runtime.generation_checkpoint_interval_tokens` to a \
                 value at or above `runtime.max_tokens_per_call` instead — e.g. match \
                 `max_tokens_per_call` exactly."
                    .into(),
            ),
        };
    }
    let answer_bound = darkmux_types::config_access::max_tokens_per_call()
        .unwrap_or(ANSWER_BOUND_BUILTIN_DEFAULT);
    if tokens >= answer_bound {
        return Check {
            name: name.into(),
            status: Status::Warn,
            message: format!(
                "{tokens} tokens ({provenance}) is at or above `max_tokens_per_call` \
                 ({answer_bound}) — the generation check-in can never be the tighter cap, so it \
                 is silently disabled and every non-reasoning call reverts to carrying the raw \
                 answer bound with no check-in, the exact shape the #2171 incident fixed"
            ),
            hint: Some(
                "Lower `runtime.generation_checkpoint_interval_tokens` below \
                 `runtime.max_tokens_per_call`, or raise `max_tokens_per_call` if the larger \
                 answer budget is intentional and disabling the check-in is a deliberate choice."
                    .into(),
            ),
        };
    }
    let inactivity_timeout_seconds = darkmux_types::config_access::inactivity_timeout_seconds();
    let seconds_to_generate = tokens as f64 / CONSERVATIVE_TOKENS_PER_SECOND;
    if seconds_to_generate >= inactivity_timeout_seconds as f64 {
        let approx_seconds = seconds_to_generate.round() as u64;
        return Check {
            name: name.into(),
            status: Status::Warn,
            message: format!(
                "{tokens} tokens ({provenance}) at a conservative {CONSERVATIVE_TOKENS_PER_SECOND:.0} \
                 tok/s could take ~{approx_seconds}s to generate — at or above the \
                 {inactivity_timeout_seconds}s inactivity budget (`runtime.\
                 inactivity_timeout_seconds`)"
            ),
            hint: Some(format!(
                "a single call may generate silently for ~{approx_seconds}s against an \
                 {inactivity_timeout_seconds}s inactivity budget; lower the interval or raise \
                 runtime.inactivity_timeout_seconds"
            )),
        };
    }
    Check {
        name: name.into(),
        status: Status::Pass,
        message: format!(
            "{tokens} tokens ({provenance}) — the generation check-in bounding every dispatch \
             call that doesn't carry the reasoning check-in"
        ),
        hint: None,
    }
}

/// (#2110/#2109) Surface the resolved thermal-governor/breaker knobs with
/// `enabled`'s provenance. Always Pass — this is informational (what the
/// governor will do), never a gate; the on-machine state machine that
/// actually watches thermal samples lives in
/// `darkmux_crew::thermal_governor` and is exercised by its own tests, not
/// by doctor.
fn check_thermal_governor() -> Check {
    let name = "runtime.thermal";
    let env_raw = std::env::var("DARKMUX_THERMAL_ENABLED")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let cfg_set = darkmux_types::config::DarkmuxConfig::load_resolved()
        .runtime
        .and_then(|r| r.thermal)
        .and_then(|t| t.enabled)
        .is_some();
    let provenance = if env_raw.is_some() {
        "from DARKMUX_THERMAL_ENABLED env"
    } else if cfg_set {
        "from config.json"
    } else {
        "default"
    };
    let enabled = darkmux_types::config_access::thermal_enabled();
    if !enabled {
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: format!("disabled ({provenance}) — no thermal pausing or breaking"),
            hint: None,
        };
    }
    let pause_at = darkmux_types::config_access::thermal_pause_at();
    let resume_at = darkmux_types::config_access::thermal_resume_at();
    let resume_hold_ms = darkmux_types::config_access::thermal_resume_hold_ms();
    let max_pause_ms = darkmux_types::config_access::thermal_max_pause_ms();
    let min_cpu = darkmux_types::config_access::thermal_min_cpu_speed_limit_pct();
    let speed_limit_hold_samples = darkmux_types::config_access::thermal_speed_limit_hold_samples();

    // (#2110/#2109 review finding 6) `darkmux config set` rejects an
    // unrecognized thermal-state token going forward, but a hand-edited
    // config.json or a value written before that validation existed can
    // still carry one — and, per `Ty::ThermalState`'s own doc, a typo here
    // silently INVERTS the governor's intent rather than erroring, so this
    // is worth a loud Warn rather than folding into the Pass message above.
    let states = darkmux_crew::host_probe::thermal::THERMAL_STATES;
    let bad_pause_at = !states.contains(&pause_at.to_ascii_lowercase().as_str());
    let bad_resume_at = !states.contains(&resume_at.to_ascii_lowercase().as_str());
    if bad_pause_at || bad_resume_at {
        let mut bad = Vec::new();
        if bad_pause_at {
            bad.push(format!("pause_at=`{pause_at}`"));
        }
        if bad_resume_at {
            bad.push(format!("resume_at=`{resume_at}`"));
        }
        return Check {
            name: name.into(),
            status: Status::Warn,
            message: format!(
                "unrecognized thermal state: {} — valid: {}. An unrecognized pause_at silently \
                 disables the governor's soft pause; an unrecognized resume_at defeats the \
                 hysteresis hold and clears a pause almost immediately regardless of actual \
                 temperature.",
                bad.join(", "),
                states.join(", ")
            ),
            hint: Some(format!(
                "darkmux config set runtime.thermal.pause_at <{}>",
                states.join("|")
            )),
        };
    }

    // (N2, final re-check) An explicit `0` doesn't achieve "disable"
    // semantics — it's silently coerced to `1` by
    // `thermal_speed_limit_hold_samples`'s own `.max(1)` floor (a naive
    // `streak >= 0` would trip on EVERY sample instead, the opposite of
    // disable). Warn so the operator knows their `0` didn't do what it
    // looked like it would.
    let speed_limit_hold_samples_raw =
        darkmux_types::config_access::thermal_speed_limit_hold_samples_raw();
    if speed_limit_hold_samples_raw == 0 {
        return Check {
            name: name.into(),
            status: Status::Warn,
            message: "runtime.thermal.speed_limit_hold_samples is 0 — coerced to 1 (trips on the                        first low sample). There is no way to disable this signal via 0; disable                        the thermal governor overall (runtime.thermal.enabled) if that's the intent."
                .to_string(),
            hint: Some(
                "darkmux config set runtime.thermal.speed_limit_hold_samples 1".to_string(),
            ),
        };
    }

    Check {
        name: name.into(),
        status: Status::Pass,
        message: format!(
            "enabled ({provenance}) — pause at `{pause_at}`, resume at `{resume_at}` held \
             {resume_hold_ms}ms, breaker after {max_pause_ms}ms of one pause episode or \
             {speed_limit_hold_samples} consecutive samples with cpu_speed_limit_pct < {min_cpu}%"
        ),
        hint: None,
    }
}

/// (#2108) Which host-probe SOURCES actually resolved on this machine, and
/// what one sample costs.
///
/// The probe reads four independent sources (mach kernel counters, the
/// private IOReport framework, the SoC's DVFS frequency tables, the OS
/// thermal state, and the `IOAccelerator` IORegistry node) and each degrades
/// to null fields on its own. Without this check an operator looking at a
/// drawer with no power numbers cannot tell "this Mac does not expose
/// IOReport" from "darkmux forgot to read it" — the exact
/// operator-sovereignty failure (#44: never wonder where a decision came
/// from) that a silent degradation path invites.
///
/// **Takes TWO samples, deliberately.** CPU percent and every power rail are
/// counter DELTAS, so the first sample a probe takes only seeds them; a
/// one-sample check would report the cost of the seeding read and a null CPU
/// figure. The reported cost is the SECOND sample's own self-stamp — the
/// number the operator should compare against the sampler cadence.
///
/// Costs ~120 ms total (a one-time probe construction plus two samples);
/// `doctor` is a diagnostic command, not a hot path.
/// (#2399) List the mirrors `workspace_spec::materialize` has quarantined
/// — `<darkmux-root>/workspaces/<name>/mirror/<id>.git.corrupt-<unix-ts>`.
///
/// A quarantine happens when an existing mirror fails materialize's
/// self-check (not bare, or pointing at an origin the spec doesn't name):
/// the directory is MOVED aside, never deleted, because it is evidence of
/// whatever wrote into darkmux's own cache — the live 2026-09-05 case was
/// most likely an external `git` run inside it. Moved-aside is also
/// invisible: nothing else in darkmux ever mentions those directories
/// again, and they hold a full clone's worth of bytes. So this check is
/// informational and always `Pass` — reporting disk that darkmux
/// deliberately kept is not a defect, and deciding when evidence has
/// served its purpose is the operator's call (#44), not doctor's.
fn check_quarantined_mirrors() -> Check {
    let workspaces = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto)
        .root
        .join("workspaces");
    quarantined_mirrors_check_at(&workspaces)
}

/// The body of [`check_quarantined_mirrors`], against an explicit
/// workspaces root so a test can point it at a fixture instead of the
/// operator's real darkmux home.
fn quarantined_mirrors_check_at(workspaces_root: &std::path::Path) -> Check {
    let name = "workspaces.quarantined-mirrors";
    let mut found: Vec<(std::path::PathBuf, u64)> = Vec::new();
    if let Ok(workspaces) = std::fs::read_dir(workspaces_root) {
        for ws in workspaces.flatten() {
            let Ok(mirrors) = std::fs::read_dir(ws.path().join("mirror")) else { continue };
            for entry in mirrors.flatten() {
                if entry.file_name().to_string_lossy().contains(".corrupt-") {
                    let bytes = dir_size_bytes(&entry.path());
                    found.push((entry.path(), bytes));
                }
            }
        }
    }
    found.sort();

    if found.is_empty() {
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: format!("none under {}", workspaces_root.display()),
            hint: None,
        };
    }
    let total: u64 = found.iter().map(|(_, b)| *b).sum();
    let listing = found
        .iter()
        .map(|(p, b)| format!("{} ({})", p.display(), human_bytes(*b)))
        .collect::<Vec<_>>()
        .join("; ");
    Check {
        name: name.into(),
        status: Status::Pass,
        message: format!(
            "{} quarantined mirror(s), {} total — {listing}",
            found.len(),
            human_bytes(total)
        ),
        hint: Some(
            "each one is a repository that failed `workspace_spec::materialize`'s bare/origin \
             self-check (#2399) and was moved aside rather than deleted. Inspect it \
             (`git -C <path> log -1`, `git -C <path> config --list`) to see what wrote into \
             darkmux's cache, then remove it when you're done with the evidence."
                .into(),
        ),
    }
}

/// Recursive byte total of a directory, symlinks never followed. Only ever
/// called on a quarantined mirror, which is normally a set of zero.
fn dir_size_bytes(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            total += dir_size_bytes(&entry.path());
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.1} GB", b / (KB * KB * KB))
    }
}

fn check_host_probe() -> Check {
    let mut probe = darkmux_crew::host_probe::HostProbe::new();
    let _seed = probe.sample();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let s = probe.sample();
    describe_host_probe(probe.sources(), s.cost_ms)
}

/// Render [`check_host_probe`]'s verdict from an already-taken reading.
///
/// Split out so every degradation combination is testable — most notably
/// "IOReport did not load", which on a healthy Apple Silicon machine cannot
/// be produced by running the real probe, and which is precisely the
/// combination worth pinning: a private framework whose path has already
/// moved once between macOS releases will move again. Pure.
fn describe_host_probe(
    src: darkmux_crew::host_probe::HostProbeSources,
    cost_ms: u64,
) -> Check {
    let name = "host probe";
    let all = [
        ("mach", src.mach),
        ("ioreport", src.ioreport),
        ("freq-tables", src.freq_tables),
        ("thermal", src.thermal),
        ("ioreg-gpu", src.ioreg_gpu),
    ];
    let resolved: Vec<&str> = all.iter().filter_map(|(n, ok)| ok.then_some(*n)).collect();
    let missing: Vec<&str> = all.iter().filter_map(|(n, ok)| (!ok).then_some(*n)).collect();

    let cost = format!("{cost_ms}ms/sample");
    if resolved.is_empty() {
        return Check {
            name: name.into(),
            status: Status::Warn,
            message: format!(
                "no host sources resolved ({cost}) — cpu/mem/gpu/power/thermal all report null"
            ),
            hint: Some(
                "The host probe is implemented for Apple Silicon macOS. On any other platform \
                 the machine stats drawer and the dispatch envelope's `host` block are \
                 expected to be empty."
                    .into(),
            ),
        };
    }
    let msg = if missing.is_empty() {
        format!("{} ({cost})", resolved.join(" + "))
    } else {
        format!("{} ({cost}); unavailable: {}", resolved.join(" + "), missing.join(", "))
    };
    // Anything short of mach is a real gap worth surfacing — without tick
    // counters there is no CPU figure at all. A missing IOReport is a
    // property of the host, reported without alarm but always NAMED.
    let status = if src.mach { Status::Pass } else { Status::Warn };
    Check {
        name: name.into(),
        status,
        message: msg,
        hint: (!missing.is_empty()).then(|| {
            "Sources are read independently and each degrades to null on its own. `ioreport` \
             and `freq-tables` are Apple-Silicon-only (and IOReport is a private framework \
             whose path has moved between macOS releases); a host without them still reports \
             cpu/mem/gpu."
                .into()
        }),
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
/// **Why only Redis** (and not machine_id / lmstudio_url / fleet.mode): a
/// useful masking warning needs a signal that the operator *intentionally*
/// configured the field, else it fires on every post-`init` machine (init
/// writes a default for nearly every field, so "config has a value" is
/// always true). `config.redis.enabled == Some(true)` is that signal —
/// the operator turned the block ON — and it matches `redis_url()`'s Tier-2
/// condition exactly (the default `init` config is `enabled:false` + a default
/// host → assembles NO config Redis → not masked). The other fields lack such a
/// signal: machine_id is env-PRIMARY by design (the docs recommend setting
/// it via env — env-over-config is intended, not a trap), and lmstudio_url /
/// fleet.mode would need default-comparison to tell an operator value from
/// the init default (a later refinement).
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

/// (#1959) The rule registry check — mirrors `check_crew_role_prompt_coverage`'s
/// shape (built-in coverage + provenance, warn-not-fail on a thin config).
/// Loads every rule (embedded + the `<darkmux root>/rules` user tier),
/// reports the count and where they came from, and surfaces
/// `darkmux_crew::rules::load_all`'s own warnings (a malformed user file,
/// naming it) plus a check over EVERY loaded rule — not just one
/// manifest's resolved subset, since doctor is asking "is the whole
/// registry healthy" — for an empty `applies_to` or a `site` rule with no
/// `prefilter` (either makes the rule inert: `warn_on_thin_rules` in
/// `crew::rules` only runs over a manifest's resolved ids, so a rule
/// nobody's manifest currently references would otherwise go unchecked
/// forever).
fn check_rules_registry() -> Check {
    let user_dir = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::Auto)
        .root
        .join("rules");
    build_rules_check(Some(&user_dir))
}

fn build_rules_check(user_dir: Option<&std::path::Path>) -> Check {
    let (embedded_only, _) = darkmux_crew::rules::load_all(None);
    let (map, mut warnings) = darkmux_crew::rules::load_all(user_dir);

    // (#2310 P4c review round 2, MUST FIX 2) Was a hand-duplicated copy of
    // `crew::rules::warn_on_thin_rules`'s own four checks — the exact
    // drift `crew::rules::thin_rule_warnings`'s own doc names as the
    // reason it exists. Both call sites now share ONE definition.
    for rule in map.values() {
        warnings.extend(darkmux_crew::rules::thin_rule_warnings(rule));
    }

    let user_file_count = user_dir
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                .count()
        })
        .unwrap_or(0);

    let provenance = match user_dir {
        Some(d) if user_file_count > 0 => format!(
            "{} built-in, {} user-tier file(s) at {}",
            embedded_only.len(),
            user_file_count,
            d.display()
        ),
        _ => format!("{} built-in, no user tier", embedded_only.len()),
    };

    let message = format!("{} rule(s) loaded ({provenance})", map.len());

    if warnings.is_empty() {
        Check { name: "rules".into(), status: Status::Pass, message, hint: None }
    } else {
        Check {
            name: "rules".into(),
            status: Status::Warn,
            message: format!("{message} — {}", warnings.join("; ")),
            hint: Some(
                "Fix the named rule file(s) under `<darkmux root>/rules/`, or drop the empty \
                 `applies_to`/`prefilter` field so the rule actually matches something."
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
            // `hostport` is like "laptop.tailnet-example.ts.net:80" — split the
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
    tailnet_viewer_url_bounded(port, TAILNET_PROBE_TIMEOUT)
}

/// (#1569 packet A gate) How long the `tailscale` probe may take before it is
/// killed and treated as absent. Short by design: this runs on a path an
/// operator is WAITING on, and the fallback (loopback) is always correct —
/// there is nothing to gain by waiting longer for a nicer URL.
const TAILNET_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Bounded `tailscale serve status --json`, killed at `timeout`.
///
/// **The deadline is the point.** `.output()` waits forever, and the
/// `tailscale` CLI blocks on the local API socket — a wedged `tailscaled`
/// (sleep/wake, mid-upgrade) hangs it indefinitely. That was tolerable while
/// only `doctor` called this: doctor is a diagnostics verb an operator runs
/// deliberately. `mission status` is the every-session housekeeping read, so
/// #1569 packet A put this on a hot path and made the hang reachable — the
/// same wedged-external-dependency class #1570/#1573 just removed for Redis,
/// and which `check_daemon_reachable_impl` below already guards against with
/// explicit socket timeouts.
///
/// Poll-and-kill rather than a watchdog thread: `try_wait` keeps ownership of
/// the child so the kill is guaranteed on every exit path, and the cost is a
/// few 25ms sleeps in the rare slow case.
fn tailnet_viewer_url_bounded(port: u16, timeout: std::time::Duration) -> Option<String> {
    let mut child = std::process::Command::new("tailscale")
        .args(["serve", "status", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            // Still running — give up at the deadline and reap, so a wedged
            // tailscaled leaves no orphan behind us.
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }

    let out = child.wait_with_output().ok()?;
    parse_tailnet_viewer_url(&String::from_utf8_lossy(&out.stdout), port)
}

/// (#1569 packet A) The base URL that linkified CLI output points at — the
/// one place THAT choice lives, so every command emitting a link picks the
/// same target.
///
/// **Scope, stated precisely because the first draft of this comment
/// overclaimed** (#1593 gate): this is the single source for *links*, not for
/// every URL darkmux prints. `check_daemon_reachable_impl` below deliberately
/// formats its own line and is the known second formatter — and it is not a
/// twin, because it answers a different question. Doctor takes an INVENTORY
/// ("here is the viewer, and here is the phone URL, both labeled"); this makes
/// a PICK ("the one URL a click should go to"). A standalone machine with
/// `tailscale serve` running will therefore see doctor advertise a phone URL
/// while `mission status` links loopback — correct in both cases, and not a
/// divergence to reconcile.
///
/// **Routes on whether the wrong-machine ambiguity exists, not on a
/// preference between two URLs** (operator call, #1569):
///
/// - `standalone` → **loopback**. There is no second daemon a link could open
///   by mistake, so loopback carries no ambiguity — and a fresh install that
///   never set up tailscale must still get working links. The docs are the
///   setup acceptance test; a clean brew-only machine following the guide
///   cannot be handed a URL it can't resolve.
/// - `hub` / `peer` → **tailnet when available**, loopback otherwise. Here a
///   second daemon exists, so a `127.0.0.1` link clicked from an SSH session
///   opens the WRONG machine's daemon and shows plausible-looking data for
///   the wrong box. That failure is silent; an unreachable tailnet URL is
///   loud. Prefer the loud one.
///
/// `fleet.mode` is the right input because it is operator-DECLARED, never
/// detected (#933) — this reads a stated intent rather than sniffing the
/// environment.
///
/// Resolving the tailnet URL spawns `tailscale serve status --json`, so it is
/// gated on `colorize_enabled()`: piped, redirected, and `--json` output emit
/// no links at all, and therefore pay no subprocess. A standalone machine
/// never spawns it regardless.
pub fn viewer_link_base(port: u16) -> String {
    let loopback = format!("http://127.0.0.1:{port}/");
    if !darkmux_types::style::colorize_enabled() {
        return loopback;
    }
    match darkmux_types::config_access::fleet_mode() {
        darkmux_types::config::FleetMode::Standalone => loopback,
        _ => tailnet_viewer_url(port).unwrap_or(loopback),
    }
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

// ─── (#1461) staleness checks — running vs installed vs source vs image ───
//
// `cargo install --path .` refreshes the binary ON DISK, but a long-running
// `darkmux serve` daemon keeps its OLD code in memory, and nothing connects the
// two. The operator (or an agent) then tests against a stale daemon and
// diagnoses a phantom bug — which is exactly what happened on the 2.0 pre-tag
// smoke: a pre-2.0 daemon serving post-2.0 data produced a "no mission with id"
// error that was never a bug at all.
//
// The rule already existed and did not fire. This is the structural-over-
// procedural answer: the system surfaces staleness instead of depending on
// anyone remembering it. Same shape as the installed-skills freshness check
// (#1426) — compare what is RUNNING against what is INSTALLED, name both
// resolved values, hand back a copy-pasteable fix.
//
// All three are WARN, never Fail: a deliberately-old daemon is a legitimate
// operator choice (sovereignty, #44). None of them mutate anything — doctor
// surfaces and suggests; the operator runs the command.

/// Name of the daemon-freshness check (#1461). Distinct from
/// `DAEMON_CHECK_NAME` (reachability): a daemon can be perfectly reachable and
/// still be serving code from three releases ago.
const DAEMON_FRESHNESS_CHECK_NAME: &str = "daemon freshness";

/// Name of the binary-vs-source check (#1461).
const BINARY_SOURCE_CHECK_NAME: &str = "binary vs source";

/// Name of the runtime-image freshness check (#1461).
const RUNTIME_IMAGE_CHECK_NAME: &str = "runtime image freshness";

/// A `Pass` row that exists only to say "this check does not apply to you".
/// Brew users must never see a source-tree warning, and the vast majority of
/// users never run a daemon — a warning whose fix_hint cannot fix anything is
/// noise, so those cases resolve to a silent Pass carrying the reason.
fn not_applicable(name: &str, reason: &str) -> Check {
    Check {
        name: name.into(),
        status: Status::Pass,
        message: format!("(not applicable: {reason})"),
        hint: None,
    }
}

/// Run `cmd` with a hard wall-clock bound, killing the child at expiry.
/// Returns `None` on spawn failure (binary absent) or timeout — both of which
/// every caller here treats as "not applicable", never as an error. Doctor must
/// never hang on a wedged `docker`, so the bound is mandatory rather than
/// optional (the same reasoning as the bounded host load/unload phase, #1276).
///
/// Dep-free by design (CLAUDE.md: "don't add dependencies casually") — poll
/// `try_wait` on a short tick rather than pulling in an async runtime.
fn bounded_output(cmd: &mut Command, timeout: std::time::Duration) -> Option<std::process::Output> {
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {}
            Err(_) => return None,
        }
        if std::time::Instant::now() >= deadline {
            // Hard-kill and report absence. A hung docker is indistinguishable
            // from an absent one for our purposes, and both mean "skip".
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

// ─── A. daemon freshness ──────────────────────────────────────────────────

/// GET `path` from a loopback HTTP server and return the response BODY.
/// `None` on any failure (nothing listening, timeout, malformed response) —
/// a dead socket is "not running", never an error.
///
/// Reads to EOF rather than taking a single `read()`: the body is what we came
/// for, and one read is only guaranteed to deliver the headers.
fn loopback_http_body(host: &str, port: u16, path: &str) -> Option<String> {
    let addr: std::net::SocketAddr = format!("{host}:{port}").parse().ok()?;
    let mut stream =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500)).ok()?;
    let to = std::time::Duration::from_millis(1000);
    // Bail rather than risk an unbounded read if the OS won't honor timeouts
    // on this socket (same defense as `check_daemon_reachable_impl`).
    stream.set_read_timeout(Some(to)).ok()?;
    stream.set_write_timeout(Some(to)).ok()?;

    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    stream.flush().ok()?;

    // `Connection: close` makes the server hang up at the end of the body, so
    // read_to_end terminates. Cap the buffer: doctor is not obliged to read an
    // unbounded response from whatever happens to hold the port.
    let mut buf = Vec::new();
    std::io::Read::by_ref(&mut stream)
        .take(64 * 1024)
        .read_to_end(&mut buf)
        .ok()?;
    let response = String::from_utf8_lossy(&buf).into_owned();
    if !response.starts_with("HTTP/1.1 200") {
        return None;
    }
    // Split headers from body on the blank line.
    response.split_once("\r\n\r\n").map(|(_, b)| b.to_string())
}

/// What a running daemon told us about itself on `/health`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DaemonBuild {
    /// The daemon reports its full build identity (#1461+).
    Modern {
        /// `build` — package version PLUS git short SHA, so two daemons built
        /// from different commits at the same package version are
        /// distinguishable.
        build: String,
        /// `binary_mtime` — when the binary the daemon loaded was last written.
        /// `None` from a daemon that couldn't stat its own exe.
        binary_mtime: Option<u64>,
    },
    /// A daemon predating #1461 reports only `darkmux_version`. Comparing that
    /// bare version against this binary's build-tagged one would not be an
    /// apples-to-apples comparison — but no comparison is needed: a daemon
    /// with no `build` field was necessarily compiled before this code existed,
    /// so it is stale by construction.
    Legacy(String),
}

/// Pull the running daemon's build identity out of a `/health` body.
fn parse_daemon_build(health_body: &str) -> Option<DaemonBuild> {
    let v: serde_json::Value = serde_json::from_str(health_body).ok()?;
    if let Some(build) = v.get("build").and_then(|b| b.as_str()) {
        return Some(DaemonBuild::Modern {
            build: build.to_string(),
            binary_mtime: v.get("binary_mtime").and_then(|m| m.as_u64()),
        });
    }
    v.get("darkmux_version")
        .and_then(|b| b.as_str())
        .map(|s| DaemonBuild::Legacy(s.to_string()))
}

/// Modification time of the darkmux binary doctor is running from, in whole
/// seconds since the Unix epoch. `None` when the exe can't be resolved or
/// stat'd — read as "nothing to compare", never as a finding.
fn installed_binary_mtime() -> Option<u64> {
    let path = env::current_exe().ok()?;
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs(),
    )
}

/// Render a whole-second span as a short human duration (`"45s"`, `"12m"`,
/// `"3h 4m"`, `"2d 5h"`). Dep-free — doctor has no time crate, and pulling one
/// in for this would violate the small-dep-set convention.
fn fmt_age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => match (secs / 3600, (secs % 3600) / 60) {
            (h, 0) => format!("{h}h"),
            (h, m) => format!("{h}h {m}m"),
        },
        _ => match (secs / 86400, (secs % 86400) / 3600) {
            (d, 0) => format!("{d}d"),
            (d, h) => format!("{d}d {h}h"),
        },
    }
}

fn check_daemon_freshness() -> Check {
    // Same locator the reachability check uses (127.0.0.1:8765) — there is no
    // port resolver in the codebase to reuse; the daemon's port is a `serve`
    // flag with this default, and both checks hardcode it identically.
    let running = loopback_http_body("127.0.0.1", 8765, "/health")
        .as_deref()
        .and_then(parse_daemon_build);
    classify_daemon_freshness(
        running,
        &darkmux_types::build_version(),
        installed_binary_mtime(),
    )
}

/// The fix for every stale-daemon verdict. Restart is the operator's to run —
/// doctor never touches a running process (#44).
fn restart_daemon_hint() -> Option<String> {
    Some(
        "restart it: stop the running `darkmux serve` (Ctrl-C in its terminal, or \
         `pkill -f 'darkmux serve'`) and start it again"
            .into(),
    )
}

/// Pure classifier — `running` is the daemon's self-reported identity (`None`
/// when no daemon answered), `installed_build` is this binary's
/// `build_version()`, `installed_mtime` is when this binary was last written.
///
/// Two independent signals, because neither catches the other's case:
///
///   * **build tag** — catches a daemon compiled from a different COMMIT.
///   * **binary mtime** — catches a reinstall at the SAME commit. This is the
///     one that matters on a dev box: `cargo install --path .` from a tree with
///     uncommitted edits yields a binary whose build tag is byte-identical to
///     the running daemon's (same SHA, same `✱` dirty marker), so the build tag
///     alone silently never fires on the loop the operator actually runs. The
///     mtime moves on every install regardless of commit.
fn classify_daemon_freshness(
    running: Option<DaemonBuild>,
    installed_build: &str,
    installed_mtime: Option<u64>,
) -> Check {
    let Some(running) = running else {
        // No daemon is the common case — most users never run one. Silent.
        return not_applicable(
            DAEMON_FRESHNESS_CHECK_NAME,
            "no darkmux serve daemon running on this machine",
        );
    };
    let warn = |message: String| Check {
        name: DAEMON_FRESHNESS_CHECK_NAME.into(),
        status: Status::Warn,
        message,
        hint: restart_daemon_hint(),
    };
    match running {
        DaemonBuild::Legacy(v) => warn(format!(
            "a darkmux serve daemon is running an OLDER build than this binary \
             ({installed_build}) — it reports darkmux {v} with no build id, which only a daemon \
             started before this check shipped does, so it cannot have your latest code"
        )),
        DaemonBuild::Modern { build, .. } if build != installed_build => warn(format!(
            "a darkmux serve daemon is running a DIFFERENT build ({build}) than this binary \
             ({installed_build}) — it serves its in-memory code until restarted, so anything you \
             verify against it is testing that build, not this one"
        )),
        // Same build tag. That is NOT yet a pass: on a dev box the tag is the
        // same commit-plus-dirty-marker before and after a reinstall from an
        // uncommitted tree, so the binary can have been replaced underneath a
        // still-running daemon without the tag moving at all.
        DaemonBuild::Modern {
            build,
            binary_mtime: Some(daemon_mtime),
        } if installed_mtime.is_some_and(|installed| installed != daemon_mtime) => {
            let installed = installed_mtime.unwrap_or(daemon_mtime);
            if installed > daemon_mtime {
                // The on-disk binary was reinstalled AFTER the daemon started —
                // the case that bit (#1461). Restart is the fix.
                warn(format!(
                    "a darkmux serve daemon is running the binary as it was {} ago, but the \
                     darkmux on disk was reinstalled since ({build} both times — the build id \
                     cannot tell them apart, the install time can). The daemon serves its \
                     in-memory code until restarted, so anything you verify against it is \
                     testing the PREVIOUS build",
                    fmt_age(installed.saturating_sub(daemon_mtime))
                ))
            } else {
                // The daemon's binary is NEWER than the one doctor is running:
                // the daemon was started from a fresher build than the darkmux
                // on this PATH. A restart would make it load the OLDER on-disk
                // binary — the wrong direction — so the fix is to refresh THIS
                // CLI, not restart the daemon. Say what is true and point at the
                // right action (#44).
                Check {
                    name: DAEMON_FRESHNESS_CHECK_NAME.into(),
                    status: Status::Warn,
                    message: format!(
                        "a darkmux serve daemon is running a binary written {} AFTER the darkmux \
                         you just ran ({build} both times) — they are different files, so the \
                         daemon is not serving the code THIS CLI is built from",
                        fmt_age(daemon_mtime.saturating_sub(installed))
                    ),
                    hint: Some(
                        "the daemon is newer than this CLI — rebuild + install this tree if you \
                         meant to catch up to it: `cargo install --path .` (restarting the daemon \
                         would instead load the OLDER on-disk binary)"
                            .into(),
                    ),
                }
            }
        }
        DaemonBuild::Modern { build, .. } => Check {
            name: DAEMON_FRESHNESS_CHECK_NAME.into(),
            status: Status::Pass,
            message: format!("running daemon matches this binary ({build})"),
            hint: None,
        },
    }
}

// ─── B. binary vs source ──────────────────────────────────────────────────

/// The git short SHA this binary was built from, or `None` when that is not a
/// meaningful question: a packaged release (`(release)`) or a source-tarball
/// build (no tag) has no commit to compare against.
///
/// Parses the tag `darkmux_types::build_version()` renders — `"2.0.0 (a1b2c3d)"`
/// or `"2.0.0 (a1b2c3d✱)"` (`✱` = built from a dirty tree). The dirty marker is
/// stripped: it says the tree had uncommitted edits at build time, which does
/// not change WHICH commit the binary came from.
fn built_from_sha(build_version: &str) -> Option<String> {
    let start = build_version.find('(')? + 1;
    let end = build_version.rfind(')')?;
    let tag = build_version.get(start..end)?.trim();
    if tag.is_empty() || tag == "release" {
        return None;
    }
    Some(tag.trim_end_matches('\u{2731}').to_string())
}

/// Walk up from `start` looking for the darkmux SOURCE tree root: a directory
/// holding BOTH a `.git` and a `Cargo.toml` that declares the darkmux
/// workspace. Both halves are load-bearing — a brew user with some other Rust
/// checkout as cwd must not get a darkmux staleness warning, and a darkmux
/// tarball with no `.git` has no HEAD to compare against.
fn find_darkmux_source_root(start: &std::path::Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let manifest = dir.join("Cargo.toml");
        if !dir.join(".git").exists() || !manifest.exists() {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        if body.contains("[workspace]") && body.contains("darkmux-types") {
            return Some(dir.to_path_buf());
        }
    }
    None
}

/// `git rev-parse --short HEAD` in `root`. `None` on any failure — an empty or
/// detached repo is "nothing to compare", not an error.
fn source_head_sha(root: &std::path::Path) -> Option<String> {
    let out = bounded_output(
        Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(root),
        std::time::Duration::from_secs(5),
    )?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

fn check_binary_vs_source() -> Check {
    let built = built_from_sha(&darkmux_types::build_version());
    let head = env::current_dir()
        .ok()
        .as_deref()
        .and_then(find_darkmux_source_root)
        .as_deref()
        .and_then(source_head_sha);
    classify_binary_vs_source(built.as_deref(), head.as_deref())
}

/// Pure classifier — `built` is the commit this binary was compiled from,
/// `head` is the darkmux source tree's current HEAD (`None` = cwd is not a
/// darkmux source tree, the case for every brew/installed user).
fn classify_binary_vs_source(built: Option<&str>, head: Option<&str>) -> Check {
    let Some(head) = head else {
        return not_applicable(
            BINARY_SOURCE_CHECK_NAME,
            "not running from a darkmux source tree",
        );
    };
    let Some(built) = built else {
        // A packaged release or tarball build carries no commit. Nothing to
        // compare, and a release binary sitting in a source tree is a normal
        // thing to do (that is what `brew install` + `git clone` looks like).
        return not_applicable(
            BINARY_SOURCE_CHECK_NAME,
            "this binary is a packaged build with no source commit to compare",
        );
    };
    if built == head {
        return Check {
            name: BINARY_SOURCE_CHECK_NAME.into(),
            status: Status::Pass,
            message: format!("running binary was built from this tree's HEAD ({head})"),
            hint: None,
        };
    }
    Check {
        name: BINARY_SOURCE_CHECK_NAME.into(),
        status: Status::Warn,
        message: format!(
            "the darkmux you are running was built from {built}, but this source tree's HEAD is \
             {head} — your latest code is NOT in the binary under test"
        ),
        hint: Some("rebuild + install it: `cargo install --path .`".into()),
    }
}

// ─── C. runtime image freshness ───────────────────────────────────────────

/// What doctor could learn about the local `darkmux-runtime:latest` image.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeImageProbe {
    /// Docker absent, wedged, daemon down, or no such image. Never a warning:
    /// docker is not a hard dependency of doctor, and plenty of users have none.
    NotApplicable(String),
    /// The image carries `org.opencontainers.image.version`.
    Labeled(String),
    /// The image exists but carries no version label — built before the label
    /// shipped, or built without the build-arg. Nothing to compare.
    Unlabeled,
}

/// The OCI label the runtime image stamps its darkmux version into.
const RUNTIME_IMAGE_VERSION_LABEL: &str = "org.opencontainers.image.version";

fn probe_runtime_image() -> RuntimeImageProbe {
    use darkmux_crew::dispatch_internal::RUNTIME_IMAGE;
    let format = format!("{{{{index .Config.Labels \"{RUNTIME_IMAGE_VERSION_LABEL}\"}}}}");
    let Some(out) = bounded_output(
        Command::new("docker").args(["image", "inspect", RUNTIME_IMAGE, "--format", &format]),
        std::time::Duration::from_secs(5),
    ) else {
        return RuntimeImageProbe::NotApplicable("`docker` not available".into());
    };
    if !out.status.success() {
        // Covers both "no such image" and "daemon not reachable". Neither is a
        // staleness finding, and `docker runtime` already reports daemon health.
        return RuntimeImageProbe::NotApplicable(format!("no local `{RUNTIME_IMAGE}` image"));
    }
    let label = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // Docker renders a missing map key as `<no value>`.
    if label.is_empty() || label == "<no value>" {
        return RuntimeImageProbe::Unlabeled;
    }
    RuntimeImageProbe::Labeled(label)
}

fn check_runtime_image_freshness() -> Check {
    classify_runtime_image_freshness(probe_runtime_image(), env!("CARGO_PKG_VERSION"))
}

/// Pure classifier — compares the image's stamped version against this binary's
/// package version.
fn classify_runtime_image_freshness(probe: RuntimeImageProbe, installed: &str) -> Check {
    use darkmux_crew::dispatch_internal::RUNTIME_IMAGE;
    match probe {
        RuntimeImageProbe::NotApplicable(reason) => {
            not_applicable(RUNTIME_IMAGE_CHECK_NAME, &reason)
        }
        RuntimeImageProbe::Unlabeled => Check {
            name: RUNTIME_IMAGE_CHECK_NAME.into(),
            status: Status::Pass,
            message: format!(
                "local `{RUNTIME_IMAGE}` carries no version label — nothing to compare"
            ),
            hint: None,
        },
        RuntimeImageProbe::Labeled(version) if version == installed => Check {
            name: RUNTIME_IMAGE_CHECK_NAME.into(),
            status: Status::Pass,
            message: format!("local `{RUNTIME_IMAGE}` matches this binary ({installed})"),
            hint: None,
        },
        RuntimeImageProbe::Labeled(version) => Check {
            name: RUNTIME_IMAGE_CHECK_NAME.into(),
            status: Status::Warn,
            message: format!(
                "local `{RUNTIME_IMAGE}` was built for darkmux {version}, but this binary is \
                 {installed} — dispatches prefer the local image, so they run the OLD runtime"
            ),
            hint: Some(format!(
                "rebuild it from a source checkout: `docker build --build-arg \
                 DARKMUX_VERSION={installed} -t {RUNTIME_IMAGE} runtime/` — or drop the local tag \
                 (`docker rmi {RUNTIME_IMAGE}`) to let darkmux pull the version-pinned image"
            )),
        },
    }
}

const RUNTIME_BINARY_CACHE_CHECK_NAME: &str = "runtime binary cache";

/// (#2386 review, MUST FIX) The OTHER direction of the runtime/binary
/// staleness pair.
///
/// `check_runtime_image_freshness` above covers "the IMAGE is older than this
/// binary". This one covers the cache that `dispatch --image <your image>`
/// injects: darkmux extracts its static runtime binary out of the darkmux
/// image once and keeps it at `~/.darkmux/runtime/darkmux-runtime`. That copy
/// had no version key and no invalidation, so it was reused forever — and the
/// moment the host starts passing a flag the cached (old) runtime does not
/// know, the container exits 2 with `unknown flag` on every such dispatch,
/// with nothing on the host saying why. The cache is version-stamped now and
/// self-invalidates; this check is the surface that lets an operator SEE a
/// stale one instead of discovering it as a failed dispatch.
fn check_runtime_binary_cache() -> Check {
    let dir = darkmux_types::config_access::runtime_cache_dir();
    classify_runtime_binary_cache(
        darkmux_crew::dispatch_internal::runtime_binary_file_exists_at(&dir),
        darkmux_crew::dispatch_internal::cached_runtime_binary_stamp_at(&dir),
        &darkmux_types::build_version(),
    )
}

/// Pure classifier. `binary_exists` and `stamp` are read separately
/// (#2386 C8) so an operator can tell "nothing cached yet" apart from "a
/// binary is cached but predates version stamping" — `stamp` alone collapses
/// both to `None`, and the two read very differently: the first needs no
/// action, the second is exactly the pre-#2386 upgrade case every existing
/// install passes through once.
fn classify_runtime_binary_cache(
    binary_exists: bool,
    stamp: Option<darkmux_crew::dispatch_internal::RuntimeBinaryStamp>,
    installed: &str,
) -> Check {
    match (binary_exists, stamp) {
        (false, _) => Check {
            name: RUNTIME_BINARY_CACHE_CHECK_NAME.into(),
            status: Status::Pass,
            message: "no cached runtime binary — the next `dispatch --image` extracts one".into(),
            hint: None,
        },
        // (#2386 C8) A binary IS there, just unstamped — never say "no
        // cached runtime binary", which reads as "nothing here" to an
        // operator who can see the file on disk.
        (true, None) => Check {
            name: RUNTIME_BINARY_CACHE_CHECK_NAME.into(),
            status: Status::Pass,
            message: "a cached runtime binary predates version stamping — the next \
                      `dispatch --image` re-extracts it"
                .into(),
            hint: None,
        },
        (true, Some(s)) if s.version == installed => Check {
            name: RUNTIME_BINARY_CACHE_CHECK_NAME.into(),
            status: Status::Pass,
            // (#2386 C4) Report both fields the stamp now carries.
            message: match s.image_id {
                Some(id) => format!(
                    "cached runtime binary matches this binary ({installed}), extracted from \
                     image {id}"
                ),
                None => format!(
                    "cached runtime binary matches this binary ({installed}); source image id \
                     unknown (`docker image inspect` was unavailable at extraction)"
                ),
            },
            hint: None,
        },
        (true, Some(s)) => Check {
            name: RUNTIME_BINARY_CACHE_CHECK_NAME.into(),
            status: Status::Warn,
            message: format!(
                "cached runtime binary was extracted for darkmux {}, but this binary is \
                 {installed} — it is injected into every `dispatch --image <your image>`",
                s.version
            ),
            hint: Some(
                "the next such dispatch re-extracts it automatically — no manual `rm` needed"
                    .into(),
            ),
        },
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
                "the `crews` map retired in 2.0 (#1426) — review staffing is now the role→profile \
                 rollup (#1475): each review role resolves via a `--param <role>=<profile>` launch \
                 override, else the `role_profiles` map in config.json, else `default_profile`. The \
                 key is harmless residue; delete it from ~/.darkmux/profiles.json."
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
    // (#2003) (id, explanation) pairs, so identical explanations can be
    // grouped at render time instead of repeated once per document.
    let mut blocking: Vec<(String, String)> = Vec::new();
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
                    blocking.push((id.clone(), joined));
                }
                if !version_drift.is_empty() {
                    let joined =
                        version_drift.iter().map(|f| f.to_string()).collect::<Vec<_>>().join("; ");
                    blocking.push((id.clone(), joined));
                }
                // (#1284 review round 2, consider 7; fixed for #1917) A
                // USER-tier copy whose schema MINOR trails the binary's MAY
                // be silently missing an additive field newer launchers rely
                // on. Two things this finding must get right, both broken
                // before #1917:
                //
                // 1. **The remedy must not assume a fallback exists.** The
                //    ORIGINAL text unconditionally said "delete it to fall
                //    back to the embedded tier" — true only when `id` HAS an
                //    embedded/on-disk counterpart. Every `pr-*` GitHub-verb
                //    config (and thirteen others, on the reporting
                //    operator's machine) is user-only: `templates/builtin/
                //    mission-configs/` holds exactly `coder-phase` and
                //    `review`. Following the old advice on a user-only
                //    document deletes it with nothing to fall back to.
                //    `has_non_user_fallback` is the same user → on-disk →
                //    embedded resolution `mission launch` uses, so the check
                //    already KNOWS the answer before it speaks.
                // 2. **The severity wording must scale with the actual gap.**
                //    The ORIGINAL text quoted a fixed illustration — a
                //    pre-1.4 "review" copy losing `reads` (#1619) — on EVERY
                //    minor-trailing hit, regardless of how far the document
                //    actually trails. A one-minor additive gap (2.2 → 2.3
                //    just added the optional `cmd`) then reads exactly
                //    like a data-loss hazard it is not.
                //
                // (#1550 cluster item 2: an earlier illustration named the
                // `expand` primitive, but `expand` was retired in schema
                // 2.0 — a MAJOR bump, see `MISSION_CONFIG_SCHEMA`'s doc — so
                // pointing at it here would itself be a stale reference to a
                // field that no longer exists.) Major drift is `validate()`'s
                // job (either direction); this minor-trailing check is
                // user-tier-only because the embedded/on-disk built-ins ship
                // with the binary and can't trail it.
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
                            let gap = bin_minor - doc_minor;
                            // Scoped to the actual gap: a one-minor trail is
                            // advisory by the schema's own versioning rule
                            // ("a future consumer can safely ignore what it
                            // can't yet evaluate" — MISSION_CONFIG_SCHEMA's
                            // doc); a wider trail is where the concrete
                            // reads/#1619 hazard actually applies.
                            let severity = if gap <= 1 {
                                // (#1919 review) Deliberately does NOT say "likely a
                                // no-op". `gap <= 1` measures DISTANCE, not harm, and
                                // this schema's history refutes the equivalence twice:
                                // 1.3 -> 1.4 added `reads` (cross-phase delivery
                                // silently stops) and 2.2 -> 2.3 added `cmd` (the
                                // allowlist gate stops applying). The only reachable
                                // gap-1 case on a 2.3 binary today is a 2.2 document,
                                // which is exactly the `pr-*` configs that mutate
                                // GitHub state and are currently ungated because of it.
                                // Telling the operator that is harmless would be
                                // backwards for the one case that prompted the check.
                                "trailing by one minor is additive by this schema's own \
                                 versioning rule, so the document still LOADS — but its \
                                 fields predate additive fields this binary now reads, and \
                                 absent is not the same as harmless. Confirm none of the \
                                 fields this binary's schema added since your document's \
                                 version apply to it: 1.3 -> 1.4 added `reads` (cross-phase \
                                 data delivery silently stops), 2.2 -> 2.3 added `cmd` (a \
                                 config that mutates GitHub state runs ungated), 3.0 -> 3.1 \
                                 added `enabled` (a step you meant to leave out still runs)"
                                    .to_string()
                            } else {
                                format!(
                                    "trailing by {gap} minors is far enough that the user copy \
                                     may predate additive fields newer launchers rely on (e.g. a \
                                     pre-1.4 \"review\" copy has no `reads` field on any task, so \
                                     cross-phase data delivery that relies on it — see schema \
                                     1.4's #1619 — silently doesn't happen)"
                                )
                            };
                            let remedy = if mission_config::has_non_user_fallback(id) {
                                "re-derive it from the current built-in, or delete it to fall \
                                 back to the on-disk/embedded built-in tier"
                                    .to_string()
                            } else {
                                "this document has no on-disk or embedded counterpart to fall \
                                 back to — deleting it loses it; update it in place against the \
                                 current schema instead"
                                    .to_string()
                            };
                            blocking.push((
                                id.clone(),
                                format!(
                                    "user-tier copy declares schema {doc_major}.{doc_minor}, \
                                     but this binary's mission-config schema is \
                                     {bin_major}.{bin_minor} — {severity}; {remedy}"
                                ),
                            ));
                        }
                        // (#1648) The MIRROR direction, and the more dangerous
                        // one. A doc on a NEWER minor parses cleanly here —
                        // `TaskConfig`'s `#[serde(flatten)] extras` swallows
                        // every field this binary doesn't know — so an
                        // additive field silently stops existing. The schema's
                        // minor-bump rule assumes a consumer "can SAFELY
                        // IGNORE what it can't yet evaluate", but #1619's
                        // `reads` breaks that assumption: ignoring it drops
                        // both the data delivery AND the execution ordering,
                        // and since the scheduler is not phase-gated, a review
                        // variant's judge task then has no remaining
                        // dependency, dispatches at launch against an empty
                        // docket, and the run completes GREEN WITH ZERO
                        // FINDINGS. A false-green review is the worst failure
                        // this project has; better to refuse to guess.
                        //
                        // Honest about reach: this only helps binaries that
                        // HAVE the check, so it cannot retroactively protect
                        // an already-shipped older binary. It closes the
                        // window from here forward, which is the only window
                        // a code change can close.
                        if doc_major == bin_major && doc_minor > bin_minor {
                            blocking.push((
                                id.clone(),
                                format!(
                                    "user-tier copy declares schema {doc_major}.{doc_minor}, \
                                     NEWER than this binary's {bin_major}.{bin_minor} — it may \
                                     declare additive fields this binary silently ignores rather \
                                     than rejects (a `reads` relation, for one, carries both data \
                                     and ordering: dropping it can let a stage run early against \
                                     empty input and finish green with no findings). Upgrade \
                                     darkmux, or re-author this copy against \
                                     {bin_major}.{bin_minor}"
                                ),
                            ));
                        }
                    }
                }
                if !kind_warnings.is_empty() {
                    kind_warning_ids.push(id.clone());
                }
            }
            Err(e) => blocking.push((id.clone(), format!("failed to parse — {e}"))),
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
            message: summarize_findings(ids.len(), &blocking),
            hint: Some(
                "fix the named document(s) under `~/.darkmux/mission-configs/<id>.json` (or the \
                 checked-out `templates/builtin/mission-configs/<id>.json` for a built-in) — a \
                 dangling depends_on, an empty id, or a schema_version your darkmux build \
                 doesn't recognize. These documents DO execute — `darkmux mission launch <id>` \
                 runs any config whose graph names step kinds this build can construct, so a \
                 finding here is a config that MAY fail at launch, not a dormant one — an \
                 Error-tier finding bails the launch, a Warning-tier one (a schema_version \
                 drift, say) only prints."
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

/// Name of the mission-envelope readability check (#1881).
const MISSION_ENVELOPE_READABILITY_CHECK_NAME: &str = "mission envelope readability";

/// (#1881) Every mission dir's `envelope.json`, read the same way
/// `crates/darkmux-serve/src/runs.rs`'s `mission_run_status` reads it
/// (`darkmux_crew::lifecycle::load_envelope`), and named LOUDLY when this
/// binary cannot fully resolve one. This is exactly the contract-registry
/// item 5 obligation ("complaints are LOUD in `darkmux doctor`") applied to
/// the specific failure `mission_run_status` used to swallow: an envelope a
/// NEWER darkmux wrote (a `status`/`outcome` value this binary's enums
/// don't recognize yet) rendered as a silent, completed, green run on the
/// dashboard instead of surfacing anywhere. Doctor is where schema drift
/// between fleet machines (CLAUDE.md's "cross-system contracts" — the
/// laptop on a `cargo install`ed main, the Studio on brew/stable) is
/// supposed to be named.
///
/// Scope: this scans every directory under `missions_dir()`, regardless of
/// the owning mission's `MissionStatus` — a mission's `mission.json` isn't
/// even loaded here, only its sibling `envelope.json`. A mission with no
/// `envelope.json` at all (`Ok(None)`) is not drift — see `load_envelope`'s
/// own doc — and is not reported here. In practice this means an envelope
/// left behind by an Aborted or still-Active mission can also be named
/// (`mission_run_status` never reads an Aborted mission's envelope at
/// all — `crates/darkmux-serve/src/runs.rs`'s `MissionStatus::Aborted`
/// arm — so that specific case warns here without ever affecting the
/// dashboard); harmless, since this check only ever WARNS, never fails,
/// but worth knowing before assuming every name here is dashboard-visible.
///
/// (#1881, second half) `MissionOutcomeStatus`/`RunOutcome` later gained
/// `#[serde(other)]` catch-alls so a genuinely NEW variant no longer fails
/// the whole `serde_json::from_str` — an envelope carrying one now returns
/// `Ok(Some(envelope))`, not `Err`. That's real progress, but the two
/// fields mean different things for the dashboard (see
/// `crates/darkmux-crew/src/envelope.rs`'s own doc on `MissionOutcomeStatus`
/// vs. `RunOutcome` leniency), so this check reports them as two SEPARATE
/// buckets rather than folding both into "could not parse":
///   - `status: Unknown` — `mission_run_status` renders this
///     `RunStatus::Unparseable`, same severity as a hard `Err`. Reported
///     alongside hard parse failures.
///   - `outcome: Some(RunOutcome::Unknown)` with a KNOWN `status` — the
///     dashboard renders this run correctly (by its real, known status);
///     only the docket-coverage DETAIL is unrecognized. Reported
///     separately, so the message never implies a dashboard row is wrong
///     when it isn't.
fn check_mission_envelope_readability() -> Check {
    let missions_root = darkmux_crew::loader::missions_dir();
    // Renders `RunStatus::Unparseable` on the dashboard — a hard parse
    // `Err`, or an `Ok(Some(_))` whose `status` itself is the catch-all.
    let mut unparseable: Vec<String> = Vec::new();
    // Renders correctly (by its known `status`) but carries an
    // unrecognized `outcome` detail — narrower drift, no dashboard impact.
    let mut outcome_drift: Vec<String> = Vec::new();
    let mut readable_count = 0u32;
    if let Ok(entries) = std::fs::read_dir(&missions_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(mission_id) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            match darkmux_crew::lifecycle::load_envelope(mission_id) {
                // (#1881) `status` itself is now lenient on read
                // (`MissionOutcomeStatus`'s `#[serde(other)]` catch-all —
                // see `crates/darkmux-crew/src/envelope.rs`) so a `status`
                // value this binary doesn't recognize no longer fails this
                // `Ok(Some(_))` arm the way it did before that leniency
                // landed. It is still exactly the schema-drift condition
                // this check exists to name — the JSON parsed, but this
                // binary genuinely does not understand what the run's
                // outcome was — so it's reported the same way a hard parse
                // failure is, not silently folded into "readable."
                Ok(Some(envelope)) if envelope.status == darkmux_crew::envelope::MissionOutcomeStatus::Unknown => {
                    unparseable.push(format!("{mission_id} (unrecognized status)"));
                }
                Ok(Some(envelope)) => {
                    readable_count += 1;
                    // (#1881, QA-caught) `outcome` has its OWN, separate
                    // leniency (`RunOutcome::Unknown`) that a known `status`
                    // does not cover — without this arm, an envelope with a
                    // real status but an unrecognized outcome DETAIL was
                    // silently counted as fully clean, the one shape of
                    // drift this check couldn't name.
                    // `RunOutcome::is_unknown` exists for exactly this read.
                    if envelope.outcome.as_ref().is_some_and(|o| o.is_unknown()) {
                        outcome_drift.push(mission_id.to_string());
                    }
                }
                Ok(None) => {}
                Err(e) => unparseable.push(format!("{mission_id} ({e})")),
            }
        }
    }
    if unparseable.is_empty() && outcome_drift.is_empty() {
        return Check {
            name: MISSION_ENVELOPE_READABILITY_CHECK_NAME.into(),
            status: Status::Pass,
            message: format!("{readable_count} mission envelope(s) parsed cleanly"),
            hint: None,
        };
    }
    let mut parts: Vec<String> = vec![format!("{readable_count} fully clean")];
    if !unparseable.is_empty() {
        parts.push(format!(
            "{} this binary could not resolve a status for: {}",
            unparseable.len(),
            unparseable.join(", ")
        ));
    }
    if !outcome_drift.is_empty() {
        parts.push(format!(
            "{} render correctly but carry an unrecognized outcome detail: {}",
            outcome_drift.len(),
            outcome_drift.join(", ")
        ));
    }
    Check {
        name: MISSION_ENVELOPE_READABILITY_CHECK_NAME.into(),
        status: Status::Warn,
        message: parts.join("; "),
        hint: Some(
            "Likely schema drift between fleet machines — a newer darkmux wrote a \
             status/outcome value this binary's release doesn't recognize yet. A run with an \
             unrecognized STATUS renders \"unparseable\" on the dashboard, never a false \
             completed/green run; a run with only an unrecognized OUTCOME detail still renders \
             correctly by its known status — only the docket-coverage detail is unreadable. \
             Compare `darkmux --version` here against the machine that wrote the envelope, and \
             upgrade this machine if it's behind."
                .into(),
        ),
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

/// (#2003) Render the registry's findings, stating each distinct explanation
/// ONCE and naming every document it applies to.
///
/// Measured on a real machine: 15 mission configs each trailing the binary's
/// schema by one minor produced 15 copies of the same ~600-character
/// explanation — a 9,726-character check message that wrapped to roughly
/// sixty lines of near-identical prose. That is one fact about fifteen
/// documents, not fifteen facts, and rendering it per-document buries the
/// single thing the operator has to act on.
///
/// The issue COUNT stays per-document (a group of five is still five issues),
/// because that is what the operator is being told to fix; only the prose is
/// shared. Groups keep first-seen order so the output is stable between runs.
fn summarize_findings(registered: usize, findings: &[(String, String)]) -> String {
    let mut order: Vec<&str> = Vec::new();
    let mut groups: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for (id, text) in findings {
        let entry = groups.entry(text.as_str()).or_insert_with(|| {
            order.push(text.as_str());
            Vec::new()
        });
        entry.push(id.as_str());
    }

    let rendered: Vec<String> = order
        .iter()
        .map(|text| {
            let ids = &groups[*text];
            match ids.len() {
                // A lone finding reads better as `"id": explanation` — the
                // shape this check has always used, kept for the common case.
                1 => format!("\"{}\": {text}", ids[0]),
                n => format!("{text} — affects {n} config(s): {}", ids.join(", ")),
            }
        })
        .collect();

    format!(
        "{registered} mission config(s) registered, {} issue(s): {}",
        findings.len(),
        rendered.join(" | ")
    )
}

/// The width `doctor` renders to (#1995).
///
/// `COLUMNS` is what `crates/darkmux-serve/src/panel.rs` sets from the
/// client's OWN measured panel width, and every other panel verb already
/// honors it — `run list` went from a 60-char longest line at `COLUMNS=56` to
/// 159 at `COLUMNS=200` while doctor emitted 2031 characters at both. That is
/// why doctor was the one panel whose output ran off the screen: not a CSS
/// problem, a verb that never answered the question it was asked.
///
/// Clamped rather than trusted: the band matches the daemon's own
/// `clamp_cols`, with a 40 floor because this renderer reserves 27 columns
/// for the marker and the name before the message even starts.
fn output_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|w| w.clamp(40, 200))
        .unwrap_or(100)
}

/// Word-wrap `text` to `width`, indenting every line AFTER the first by
/// `indent` spaces — a hanging indent, so a wrapped message stays visually
/// attached to the check that owns it instead of running back to column zero.
///
/// Hand-rolled rather than pulling in `textwrap`: the dep set here is
/// deliberately small (see CLAUDE.md), and this is the whole requirement.
/// Operates on whitespace-separated words, so it is safe to run on raw text
/// only — never on a string that already carries ANSI escapes, whose bytes
/// would count toward the width. Every caller below styles AFTER wrapping.
///
/// A word longer than the budget is emitted on its own line and allowed to
/// exceed it. Breaking mid-token would corrupt the paths, model ids and
/// commands doctor prints, and dropping it would be worse; the loop must
/// simply terminate, which it does because each iteration consumes a word.
fn wrap_hanging(text: &str, width: usize, indent: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();

    for word in text.split_whitespace() {
        // The first line is laid out after a prefix the caller has already
        // accounted for; continuations pay the hanging indent instead.
        let budget = if out.is_empty() { width } else { width.saturating_sub(indent) };
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur.chars().count() + 1 + word.chars().count() <= budget {
            cur.push(' ');
            cur.push_str(word);
        } else {
            out.push(std::mem::take(&mut cur));
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        return vec![String::new()];
    }

    let pad = " ".repeat(indent);
    out.into_iter()
        .enumerate()
        .map(|(i, l)| if i == 0 { l } else { format!("{pad}{l}") })
        .collect()
}

/// Render one check as the lines that will be printed, wrapped to `width`.
///
/// Split out from [`print_check_line`] so the WRAP is testable without
/// capturing stdout — the behavior under test is the geometry, not the IO.
fn render_check_block(c: &Check, width: usize) -> Vec<String> {
    const NAME_COL: usize = 22;
    let marker = match c.status {
        Status::Pass => darkmux_types::style::success("✓"),
        Status::Warn => darkmux_types::style::warn("⚠"),
        Status::Fail => darkmux_types::style::error("✗"),
    };
    // "  " + marker + " " + name + " ". Measured from the NAME's real length
    // so an over-long name pushes the wrap column right rather than silently
    // overflowing the budget it was never charged for.
    let head = 2 + 1 + 1 + c.name.chars().count().max(NAME_COL) + 1;
    let body = width.saturating_sub(head).max(20);

    let mut lines = Vec::new();
    let msg = wrap_hanging(&c.message, body, 0);
    lines.push(format!("  {} {:<NAME_COL$} {}", marker, c.name, msg[0]));
    for cont in &msg[1..] {
        lines.push(format!("{}{}", " ".repeat(head), cont));
    }

    if let Some(hint) = c.hint.as_ref() {
        // "        → " — 8 spaces, the arrow, a space.
        const HINT_HEAD: usize = 10;
        for raw in hint.lines() {
            let wrapped = wrap_hanging(raw, width.saturating_sub(HINT_HEAD).max(20), 0);
            lines.push(format!("        → {}", darkmux_types::style::dim(&wrapped[0])));
            for cont in &wrapped[1..] {
                lines.push(format!(
                    "{}{}",
                    " ".repeat(HINT_HEAD),
                    darkmux_types::style::dim(cont)
                ));
            }
        }
    }
    lines
}

/// Print one check line + its hint lines. Shared by the verbose and
/// issues-only render paths so they format identically.
fn print_check_line(c: &Check) {
    for line in render_check_block(c, output_width()) {
        println!("{line}");
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
    verdict_banner_at(r, output_width())
}

/// (#1995) The banner, wrapped to `width`.
///
/// The banner quotes the highest-severity check's whole message, so it is the
/// LONGEST line doctor emits — 276 characters against the real
/// daemon-freshness finding. Wrapping the per-check lines alone left this one
/// running off the screen by itself, which is why the width is threaded here
/// rather than read at the print site.
///
/// The text is wrapped BEFORE styling: `style::warn` and friends wrap the
/// string in ANSI escapes, and those bytes would otherwise be counted against
/// the width, silently over-wrapping every colored line.
fn verdict_banner_at(r: &DoctorReport, width: usize) -> String {
    let headline =
        |s: Status| r.checks.iter().find(|c| c.status == s).map(|c| format!("{}: {}", c.name, c.message));
    // ONE LINE, always — a banner is a headline, not a transcript.
    //
    // It quotes the worst check's whole message, and those are not bounded:
    // the real `mission config registry` finding concatenates one full
    // explanation per affected config, measured at 9,726 characters (the same
    // ~600-char paragraph 15 times). Wrapping that faithfully filled fifty
    // lines with a restatement of the check line printed directly below it.
    // Truncation is not information loss here — the full message is always
    // rendered in that check's own block.
    let render = |text: String| {
        // Collapse any internal newlines first: a multi-line message must not
        // be able to smuggle a second line past the one-line guarantee.
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.chars().count() <= width {
            return flat;
        }
        // Wrap to width-1 and keep the first line, so there is room for the
        // ellipsis that tells the operator something was cut.
        let first = wrap_hanging(&flat, width.saturating_sub(1).max(20), 0)
            .into_iter()
            .next()
            .unwrap_or_default();
        format!("{first}…")
    };
    match r.worst_status() {
        Status::Pass => darkmux_types::style::success("● ok — every check passed"),
        Status::Warn => darkmux_types::style::warn(&render(format!(
            "● needs attention — {}",
            headline(Status::Warn).unwrap_or_else(|| "see the warnings below".into())
        ))),
        Status::Fail => darkmux_types::style::error(&render(format!(
            "● broken — {}",
            headline(Status::Fail).unwrap_or_else(|| "see the failures below".into())
        ))),
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
    /// Visible text only — the marker and hint carry ANSI escapes whose bytes
    /// must not count toward a width assertion.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn the_verdict_banner_is_one_line_however_long_the_worst_message_is() {
        // The banner quotes the highest-severity check's message. The real
        // `mission config registry` finding concatenates one full explanation
        // per affected config — measured at 9,726 characters, the same ~600
        // char paragraph 15 times. Wrapping it faithfully filled fifty lines
        // of the operator's screen with a restatement of what the check line
        // below already says. A banner is a HEADLINE: one line, always.
        let huge = std::iter::repeat("some very wordy finding text about a config")
            .take(200)
            .collect::<Vec<_>>()
            .join(" | ");
        let r = DoctorReport {
            checks: vec![Check {
                name: "mission config registry".into(),
                status: Status::Warn,
                message: huge,
                hint: None,
            }],
        };
        for width in [56usize, 80, 100, 200] {
            let banner = strip_ansi(&verdict_banner_at(&r, width));
            assert_eq!(banner.lines().count(), 1, "width {width}: banner must be ONE line");
            assert!(
                banner.chars().count() <= width,
                "width {width}: banner is {} chars: {banner:?}",
                banner.chars().count()
            );
            assert!(banner.ends_with('…'), "a truncated banner must say so: {banner:?}");
        }
    }

    #[test]
    fn a_short_verdict_banner_is_not_truncated() {
        let r = DoctorReport {
            checks: vec![Check {
                name: "daemon".into(),
                status: Status::Warn,
                message: "not reachable".into(),
                hint: None,
            }],
        };
        let banner = strip_ansi(&verdict_banner_at(&r, 100));
        assert_eq!(banner, "● needs attention — daemon: not reachable");
        assert!(!banner.ends_with('…'), "nothing was cut, so nothing may claim it was");
    }

    // ── (#2003) Grouped registry findings ───────────────────────────────

    #[test]
    fn identical_findings_state_their_explanation_once() {
        // Measured on a real machine: 15 configs each trailing the binary's
        // mission-config schema by one minor produced 15 copies of the SAME
        // ~600-character explanation — a 9,726-character check message that
        // wrapped to roughly sixty lines. The finding is one fact about
        // fifteen documents, not fifteen facts.
        let text = "user-tier copy declares schema 2.2, but this binary's is 2.3 — \
                    a long shared explanation that is identical for every document";
        let findings: Vec<(String, String)> = ["p5-gate-coder", "pr-approve", "pr-list"]
            .iter()
            .map(|id| ((*id).to_string(), text.to_string()))
            .collect();
        let out = summarize_findings(17, &findings);

        assert_eq!(
            out.matches("a long shared explanation").count(),
            1,
            "the shared explanation must appear exactly once: {out}"
        );
        for id in ["p5-gate-coder", "pr-approve", "pr-list"] {
            assert!(out.contains(id), "every affected id must still be named: {out}");
        }
        assert!(out.contains("17 mission config(s) registered"), "{out}");
        assert!(out.contains("3 issue(s)"), "the issue count is per document, not per group: {out}");
    }

    #[test]
    fn distinct_findings_are_each_reported_with_their_own_id() {
        let findings = vec![
            ("alpha".to_string(), "a dangling depends_on".to_string()),
            ("bravo".to_string(), "an empty id".to_string()),
        ];
        let out = summarize_findings(2, &findings);
        assert!(out.contains("alpha") && out.contains("a dangling depends_on"), "{out}");
        assert!(out.contains("bravo") && out.contains("an empty id"), "{out}");
        assert!(out.contains("2 issue(s)"), "{out}");
    }

    #[test]
    fn grouping_collapses_length_not_information() {
        let text = "x".repeat(600);
        let many: Vec<(String, String)> =
            (0..15).map(|i| (format!("cfg-{i}"), text.clone())).collect();
        let grouped = summarize_findings(17, &many);
        let ungrouped: usize = many.iter().map(|(i, t)| i.len() + t.len() + 6).sum();
        assert!(
            grouped.len() < ungrouped / 5,
            "grouping must actually shorten the message: {} vs {ungrouped}",
            grouped.len()
        );
        for i in 0..15 {
            assert!(grouped.contains(&format!("cfg-{i}")), "id cfg-{i} was lost");
        }
    }

    // ── (#1995) Output width ────────────────────────────────────────────
    //
    // `doctor` was the only panel verb that ignored the width the caller
    // asked for. `crates/darkmux-serve/src/panel.rs` sets `COLUMNS` from the
    // client's own measurement, and `run list`/`flow status`/`config list`
    // all honor it; doctor emitted a 2031-character line at every width, so
    // the console panel overflowed its scroller by 533px at a 1440 viewport
    // and 1419px on a phone. These pin the wrap, not the prose.

    #[test]
    fn wrap_hanging_leaves_a_short_line_alone() {
        let out = wrap_hanging("already short", 40, 4);
        assert_eq!(out, vec!["already short".to_string()]);
    }

    #[test]
    fn wrap_hanging_never_exceeds_the_width() {
        let long = "a darkmux serve daemon is running a DIFFERENT build than this binary \
                    so anything you verify against it is testing that build, not this one";
        for width in [40usize, 60, 80, 100, 200] {
            for line in wrap_hanging(long, width, 8) {
                assert!(
                    line.chars().count() <= width,
                    "width {width}: line of {} chars exceeds it: {line:?}",
                    line.chars().count()
                );
            }
        }
    }

    #[test]
    fn wrap_hanging_indents_continuations_but_not_the_first_line() {
        let out = wrap_hanging("one two three four five six seven eight nine", 20, 6);
        assert!(out.len() > 1, "expected a wrap at width 20: {out:?}");
        assert!(!out[0].starts_with(' '), "first line must not be indented: {:?}", out[0]);
        for cont in &out[1..] {
            assert!(cont.starts_with("      "), "continuation must carry the indent: {cont:?}");
        }
    }

    #[test]
    fn wrap_hanging_keeps_every_word_and_their_order() {
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india juliet";
        let joined = wrap_hanging(text, 24, 3)
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(joined, text, "wrapping must not drop, duplicate or reorder words");
    }

    #[test]
    fn wrap_hanging_emits_an_overlong_word_rather_than_looping() {
        // A path or model id with no break opportunity must still terminate
        // and still appear. It may exceed the width; it may not vanish.
        let word = "darkmux:qwen3.6-35b-a3b-turboquant-mlx-with-a-very-long-suffix";
        let out = wrap_hanging(&format!("see {word} now"), 20, 2);
        assert!(out.iter().any(|l| l.contains(word)), "the long word must survive: {out:?}");
        assert!(out.len() < 12, "must not fragment endlessly: {out:?}");
    }

    #[test]
    #[serial_test::serial]
    fn output_width_reads_columns_and_clamps_it() {
        let prev = std::env::var("COLUMNS").ok();
        std::env::set_var("COLUMNS", "72");
        assert_eq!(output_width(), 72, "an explicit COLUMNS must be honored");
        std::env::set_var("COLUMNS", "5");
        assert!(output_width() >= 40, "a nonsense-narrow COLUMNS must clamp up");
        std::env::set_var("COLUMNS", "100000");
        assert!(output_width() <= 200, "a nonsense-wide COLUMNS must clamp down");
        std::env::set_var("COLUMNS", "not-a-number");
        assert_eq!(output_width(), 100, "an unparsable COLUMNS falls back to the default");
        std::env::remove_var("COLUMNS");
        assert_eq!(output_width(), 100, "no COLUMNS falls back to the default");
        if let Some(v) = prev {
            std::env::set_var("COLUMNS", v);
        }
    }

    #[test]
    #[serial_test::serial]
    fn a_doctor_message_the_length_of_the_real_one_is_wrapped() {
        // The regression: the daemon-freshness check's real message. Before
        // the fix this rendered as ONE line regardless of COLUMNS.
        let prev = std::env::var("COLUMNS").ok();
        std::env::set_var("COLUMNS", "100");
        let c = Check {
            name: "daemon freshness".into(),
            status: Status::Warn,
            message: "a darkmux serve daemon is running a DIFFERENT build (2.11.0) than this \
                      binary (2.12.0) — it serves its in-memory code until restarted, so \
                      anything you verify against it is testing that build, not this one"
                .into(),
            hint: Some(
                "restart it: stop the running `darkmux serve` (Ctrl-C in its terminal, or \
                 `pkill -f 'darkmux serve'`) and start it again"
                    .into(),
            ),
        };
        for line in render_check_block(&c, output_width()) {
            let visible = strip_ansi(&line).chars().count();
            assert!(visible <= 100, "line of {visible} visible chars exceeds COLUMNS=100: {line:?}");
        }
        std::env::remove_var("COLUMNS");
        if let Some(v) = prev {
            std::env::set_var("COLUMNS", v);
        }
    }

    use super::*;

    // ─── (#1685) check_gh_allowlist — resolved state + provenance ─────────

    #[serial_test::serial]
    #[test]
    fn check_gh_allowlist_disabled_by_default_is_pass() {
        let prev_e = std::env::var("DARKMUX_CMD_ENABLED").ok();
        let prev_a = std::env::var("DARKMUX_CMD_ALLOWED").ok();
        unsafe {
            std::env::remove_var("DARKMUX_CMD_ENABLED");
            std::env::remove_var("DARKMUX_CMD_ALLOWED");
        }
        let check = check_gh_allowlist();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("disabled"), "{}", check.message);
        unsafe {
            match prev_e {
                Some(v) => std::env::set_var("DARKMUX_CMD_ENABLED", v),
                None => std::env::remove_var("DARKMUX_CMD_ENABLED"),
            }
            match prev_a {
                Some(v) => std::env::set_var("DARKMUX_CMD_ALLOWED", v),
                None => std::env::remove_var("DARKMUX_CMD_ALLOWED"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_gh_allowlist_enabled_with_empty_list_warns() {
        let prev_e = std::env::var("DARKMUX_CMD_ENABLED").ok();
        let prev_a = std::env::var("DARKMUX_CMD_ALLOWED").ok();
        unsafe {
            std::env::set_var("DARKMUX_CMD_ENABLED", "true");
            std::env::remove_var("DARKMUX_CMD_ALLOWED");
        }
        let check = check_gh_allowlist();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("empty"), "{}", check.message);
        assert!(check.hint.is_some());
        unsafe {
            match prev_e {
                Some(v) => std::env::set_var("DARKMUX_CMD_ENABLED", v),
                None => std::env::remove_var("DARKMUX_CMD_ENABLED"),
            }
            match prev_a {
                Some(v) => std::env::set_var("DARKMUX_CMD_ALLOWED", v),
                None => std::env::remove_var("DARKMUX_CMD_ALLOWED"),
            }
        }
    }

    // ─── (#2093) check_hooks — flow-record hooks ───────────────────────────

    #[serial_test::serial]
    #[test]
    fn check_hooks_disabled_by_default_is_pass() {
        let prev = std::env::var("DARKMUX_HOOKS_ENABLED").ok();
        unsafe { std::env::remove_var("DARKMUX_HOOKS_ENABLED"); }
        let checks = check_hooks();
        assert_eq!(checks.len(), 1, "disabled → the one overview check, no per-rule checks");
        let check = &checks[0];
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("disabled"), "{}", check.message);
        // (#2093 merge-gate finding 14) No env, no config tier in test
        // builds (#811) → provenance is `default`, not silently `config.json`.
        assert!(check.message.contains("default"), "{}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOOKS_ENABLED", v),
                None => std::env::remove_var("DARKMUX_HOOKS_ENABLED"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_hooks_enabled_with_no_rules_warns() {
        let prev = std::env::var("DARKMUX_HOOKS_ENABLED").ok();
        unsafe { std::env::set_var("DARKMUX_HOOKS_ENABLED", "true"); }
        let checks = check_hooks();
        assert_eq!(checks.len(), 1);
        let check = &checks[0];
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("no rules"), "{}", check.message);
        assert!(check.hint.is_some());
        // env DID set it here, so provenance must say `env`, not `default`.
        assert!(check.message.contains("env"), "{}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOOKS_ENABLED", v),
                None => std::env::remove_var("DARKMUX_HOOKS_ENABLED"),
            }
        }
    }

    /// (#2093 merge-gate finding 14) `build_hooks_check` now returns ONE
    /// `Check` per flagged rule (`hooks.rule.<index>`) plus one overview
    /// (`hooks`) — so a flag attaches to the RULE it names, not to an
    /// aggregate message an operator has to cross-reference by hand.
    /// Exercised against `build_hooks_check` directly with synthetic
    /// rules, since the global `config()` tier is empty by construction
    /// in test builds (#811) — there is no way to inject a populated
    /// `hooks.rules` through `check_hooks()`'s normal env/config path.
    #[test]
    fn hooks_check_rollup_flags_attach_to_the_right_rule() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![
            HookRule {
                r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
                http: Some("http://127.0.0.1:8790/events".to_string()),
                signing_secret_keychain_item: None,
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
                extras: Default::default(),
            },
            HookRule {
                r#match: None,
                http: Some("http://127.0.0.1:9000/x".to_string()),
                signing_secret_keychain_item: None,
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
                extras: Default::default(),
            },
            HookRule {
                r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
                http: Some("http://10.0.0.5:8790/x".to_string()),
                signing_secret_keychain_item: None,
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
                extras: Default::default(),
            },
        ];
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path());
        assert_eq!(checks.len(), 4, "1 overview + 3 per-rule checks");

        let overview = checks.iter().find(|c| c.name == "hooks").unwrap();
        assert_eq!(overview.status, Status::Fail, "worst of the three rules — a non-loopback rule is a hard block");

        let healthy = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(healthy.status, Status::Pass, "{}", healthy.message);
        assert!(healthy.message.contains("crawl.*"), "{}", healthy.message);
        assert!(healthy.message.contains("undelivered"), "{}", healthy.message);

        let empty_match = checks.iter().find(|c| c.name == "hooks.rule.1").unwrap();
        assert_eq!(empty_match.status, Status::Warn, "{}", empty_match.message);
        assert!(empty_match.message.contains("EMPTY MATCH"), "{}", empty_match.message);
        assert!(!empty_match.message.contains("REFUSED"), "rule 1's own flags only: {}", empty_match.message);

        // 10.0.0.5 is neither loopback nor a Tailscale address (not in
        // 100.64.0.0/10, no `.ts.net` suffix) — refused (#2135 option 2).
        let refused = checks.iter().find(|c| c.name == "hooks.rule.2").unwrap();
        assert_eq!(refused.status, Status::Fail, "{}", refused.message);
        assert!(refused.message.contains("URL REFUSED"), "{}", refused.message);
        assert!(!refused.message.contains("EMPTY MATCH"), "rule 2's own flags only: {}", refused.message);
    }

    /// (#2135 option 2) A tailnet target (`100.64.0.0/10`) is accepted by
    /// URL policy alone — no config gate — and is NOT the `is_refused`
    /// case a plain non-tailnet non-loopback host is. Unsigned (no
    /// `signing_secret_keychain_item`) still Warns.
    #[test]
    fn hooks_check_accepts_tailnet_target_and_warns_when_unsigned() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some("http://100.64.1.2:8790/events".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path());
        let rule = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(rule.status, Status::Warn, "{}", rule.message);
        assert!(!rule.message.contains("URL REFUSED"), "a valid tailnet target is not refused: {}", rule.message);
        assert!(rule.message.contains("[tailnet, unsigned]"), "{}", rule.message);
        assert!(rule.message.contains("TAILNET TARGET, UNSIGNED"), "{}", rule.message);
    }

    /// (#2135 option 2) The same tailnet target, but signed — no Warn.
    #[test]
    fn hooks_check_tailnet_target_signed_is_pass() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some("http://100.64.1.2:8790/events".to_string()),
            signing_secret_keychain_item: Some("darkmux-hook-0".to_string()),
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path());
        let rule = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(rule.status, Status::Pass, "{}", rule.message);
        assert!(rule.message.contains("[tailnet, signed]"), "{}", rule.message);
    }

    /// (#2093 merge-gate finding 17) A rule matching `telemetry.*` (or the
    /// `telemetry` category) or a bare `*` action risks the observer
    /// joining the observed — this project's own doctrine (CLAUDE.md
    /// "The observer must not join the observed"). Doctor names it.
    #[test]
    fn hooks_check_warns_on_telemetry_or_bare_star_match() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![
            HookRule {
                r#match: Some(HookMatch { action: Some("telemetry.tokens".to_string()), ..Default::default() }),
                http: Some("http://127.0.0.1:8790/a".to_string()),
                signing_secret_keychain_item: None,
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
                extras: Default::default(),
            },
            HookRule {
                r#match: Some(HookMatch { action: Some("*".to_string()), ..Default::default() }),
                http: Some("http://127.0.0.1:8790/b".to_string()),
                signing_secret_keychain_item: None,
                file: None,
                transform: None,
                headers: None,
                attribution_headers: None,
                extras: Default::default(),
            },
        ];
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path());
        let telemetry = checks.iter().find(|c| c.name == "hooks.rule.0").unwrap();
        assert_eq!(telemetry.status, Status::Warn, "{}", telemetry.message);
        assert!(telemetry.message.contains("observer must not join the observed"), "{}", telemetry.message);

        let bare_star = checks.iter().find(|c| c.name == "hooks.rule.1").unwrap();
        assert_eq!(bare_star.status, Status::Warn, "{}", bare_star.message);
        assert!(bare_star.message.contains("observer must not join the observed"), "{}", bare_star.message);
    }

    /// (#2093 merge-gate finding 15) A `*.outbox.jsonl` file that belongs
    /// to no CURRENTLY-configured rule — the artifact of a rule since
    /// removed (or, before content-hash keying, silently reassigned by a
    /// reorder) — is named, not silently ignored.
    #[test]
    fn hooks_check_warns_on_stray_outbox_file() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:8790/events".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        // A stray file belonging to a rule that's since been removed from
        // config — its key can't match any CURRENT rule's `rule_key`.
        std::fs::write(tmp.path().join("127.0.0.1-9999-deadbeefdeadbeef.outbox.jsonl"), "").unwrap();

        let checks = build_hooks_check(true, "config.json", &rules, tmp.path());
        let stray = checks.iter().find(|c| c.name == "hooks.stray").expect("a stray-file check must be present");
        assert_eq!(stray.status, Status::Warn, "{}", stray.message);
        assert!(stray.message.contains("127.0.0.1-9999-deadbeefdeadbeef"), "{}", stray.message);
    }

    #[test]
    fn hooks_check_no_stray_file_check_when_nothing_stray() {
        use darkmux_types::config::{HookMatch, HookRule};
        let tmp = tempfile::TempDir::new().unwrap();
        let rules = vec![HookRule {
            r#match: Some(HookMatch { action: Some("crawl.*".to_string()), ..Default::default() }),
            http: Some("http://127.0.0.1:8790/events".to_string()),
            signing_secret_keychain_item: None,
            file: None,
            transform: None,
            headers: None,
            attribution_headers: None,
            extras: Default::default(),
        }];
        let checks = build_hooks_check(true, "config.json", &rules, tmp.path());
        assert!(checks.iter().all(|c| c.name != "hooks.stray"), "no stray files → no stray check emitted");
    }

    #[serial_test::serial]
    #[test]
    fn check_gh_allowlist_enabled_with_verbs_is_pass_and_names_them() {
        let prev_e = std::env::var("DARKMUX_CMD_ENABLED").ok();
        let prev_a = std::env::var("DARKMUX_CMD_ALLOWED").ok();
        unsafe {
            std::env::set_var("DARKMUX_CMD_ENABLED", "true");
            std::env::set_var("DARKMUX_CMD_ALLOWED", "pr-list,pr-merge");
        }
        let check = check_gh_allowlist();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("pr-list"), "{}", check.message);
        assert!(check.message.contains("pr-merge"), "{}", check.message);
        assert!(check.message.contains("env"), "provenance named: {}", check.message);
        unsafe {
            match prev_e {
                Some(v) => std::env::set_var("DARKMUX_CMD_ENABLED", v),
                None => std::env::remove_var("DARKMUX_CMD_ENABLED"),
            }
            match prev_a {
                Some(v) => std::env::set_var("DARKMUX_CMD_ALLOWED", v),
                None => std::env::remove_var("DARKMUX_CMD_ALLOWED"),
            }
        }
    }

    // ─── (#2094) check_turn_delay — resolved state + provenance + clamp warn ─

    #[serial_test::serial]
    #[test]
    fn check_turn_delay_zero_by_default_is_pass() {
        let prev = std::env::var("DARKMUX_TURN_DELAY_MS").ok();
        unsafe { std::env::remove_var("DARKMUX_TURN_DELAY_MS") };
        let check = check_turn_delay();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("0ms"), "{}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_TURN_DELAY_MS", v),
                None => std::env::remove_var("DARKMUX_TURN_DELAY_MS"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_turn_delay_below_timeout_is_pass_and_names_provenance() {
        let prev_d = std::env::var("DARKMUX_TURN_DELAY_MS").ok();
        let prev_t = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
        unsafe {
            std::env::set_var("DARKMUX_TURN_DELAY_MS", "3000");
            std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        }
        let check = check_turn_delay();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("3000ms"), "{}", check.message);
        assert!(check.message.contains("env"), "provenance named: {}", check.message);
        unsafe {
            match prev_d {
                Some(v) => std::env::set_var("DARKMUX_TURN_DELAY_MS", v),
                None => std::env::remove_var("DARKMUX_TURN_DELAY_MS"),
            }
            match prev_t {
                Some(v) => std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v),
                None => std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS"),
            }
        }
    }

    /// (#2094 finding 9) An env var set to garbage must not be silently
    /// reported as `"from ... env"` — `config_access::turn_delay_ms()`
    /// falls through to a lower tier on a parse failure, and provenance
    /// claiming "env" while the resolved value came from config/default
    /// is a doctor surface actively lying about where a number came from.
    #[serial_test::serial]
    #[test]
    fn check_turn_delay_unparseable_env_warns_and_names_the_raw_value() {
        let prev_d = std::env::var("DARKMUX_TURN_DELAY_MS").ok();
        unsafe {
            std::env::set_var("DARKMUX_TURN_DELAY_MS", "3s");
        }
        let check = check_turn_delay();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(
            check.message.contains("DARKMUX_TURN_DELAY_MS") && check.message.contains("3s"),
            "must name the raw unparseable value: {}",
            check.message
        );
        assert!(
            check.message.contains("not an integer"),
            "must say WHY it's rejected, not just show a resolved number: {}",
            check.message
        );
        assert!(
            !check.message.contains("from DARKMUX_TURN_DELAY_MS env"),
            "must NOT claim provenance is env when the env value didn't parse: {}",
            check.message
        );
        unsafe {
            match prev_d {
                Some(v) => std::env::set_var("DARKMUX_TURN_DELAY_MS", v),
                None => std::env::remove_var("DARKMUX_TURN_DELAY_MS"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_turn_delay_at_or_above_timeout_warns_and_names_the_clamp() {
        let prev_d = std::env::var("DARKMUX_TURN_DELAY_MS").ok();
        let prev_t = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
        unsafe {
            // 10s timeout (10000ms); a 10000ms delay is AT the timeout.
            std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", "10");
            std::env::set_var("DARKMUX_TURN_DELAY_MS", "10000");
        }
        let check = check_turn_delay();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("10000ms"), "{}", check.message);
        assert!(check.message.contains("5000ms"), "names the clamped half: {}", check.message);
        assert!(check.hint.is_some());
        unsafe {
            match prev_d {
                Some(v) => std::env::set_var("DARKMUX_TURN_DELAY_MS", v),
                None => std::env::remove_var("DARKMUX_TURN_DELAY_MS"),
            }
            match prev_t {
                Some(v) => std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v),
                None => std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS"),
            }
        }
    }

    /// (#2094 second round, finding 4) The runtime's clamp band widened
    /// from "at or above the full timeout" to "at or above HALF the
    /// timeout" — doctor's own gate must track the same band, or it tells
    /// the operator a value is fine when the runtime is actually about to
    /// clamp it.
    #[serial_test::serial]
    #[test]
    fn check_turn_delay_at_half_the_timeout_warns_though_well_below_the_full_timeout() {
        let prev_d = std::env::var("DARKMUX_TURN_DELAY_MS").ok();
        let prev_t = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
        unsafe {
            // 10s timeout (10000ms); a 6000ms delay is well BELOW the full
            // timeout but AT/ABOVE half of it — the widened band warns.
            std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", "10");
            std::env::set_var("DARKMUX_TURN_DELAY_MS", "6000");
        }
        let check = check_turn_delay();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("6000ms"), "{}", check.message);
        assert!(check.message.contains("5000ms"), "names the clamped half: {}", check.message);
        assert!(check.hint.is_some());
        unsafe {
            match prev_d {
                Some(v) => std::env::set_var("DARKMUX_TURN_DELAY_MS", v),
                None => std::env::remove_var("DARKMUX_TURN_DELAY_MS"),
            }
            match prev_t {
                Some(v) => std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v),
                None => std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS"),
            }
        }
    }

    // ─── (#2165) check_reasoning_checkpoint_interval — resolved value + provenance ─

    #[serial_test::serial]
    #[test]
    fn check_reasoning_checkpoint_interval_unset_is_pass_and_names_built_in() {
        let prev = std::env::var("DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL").ok();
        unsafe { std::env::remove_var("DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL") };
        let check = check_reasoning_checkpoint_interval();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("1000 tokens"), "{}", check.message);
        assert!(check.message.contains("built-in"), "{}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_reasoning_checkpoint_interval_env_override_names_the_value_and_env() {
        let prev = std::env::var("DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL").ok();
        unsafe { std::env::set_var("DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL", "500") };
        let check = check_reasoning_checkpoint_interval();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("500 tokens"), "{}", check.message);
        assert!(check.message.contains("env"), "provenance named: {}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_REASONING_CHECKPOINT_INTERVAL"),
            }
        }
    }

    // ─── (#2190) check_max_stall_recoveries — resolved value + provenance ──

    #[serial_test::serial]
    #[test]
    fn check_max_stall_recoveries_unset_is_pass_and_names_built_in() {
        let prev = std::env::var("DARKMUX_RUNTIME_MAX_STALL_RECOVERIES").ok();
        unsafe { std::env::remove_var("DARKMUX_RUNTIME_MAX_STALL_RECOVERIES") };
        let check = check_max_stall_recoveries();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("2 recoveries"), "{}", check.message);
        assert!(check.message.contains("built-in"), "{}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_MAX_STALL_RECOVERIES", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_MAX_STALL_RECOVERIES"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_max_stall_recoveries_env_override_names_the_value_and_env() {
        let prev = std::env::var("DARKMUX_RUNTIME_MAX_STALL_RECOVERIES").ok();
        unsafe { std::env::set_var("DARKMUX_RUNTIME_MAX_STALL_RECOVERIES", "4") };
        let check = check_max_stall_recoveries();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("4 recoveries"), "{}", check.message);
        assert!(check.message.contains("env"), "provenance named: {}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_MAX_STALL_RECOVERIES", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_MAX_STALL_RECOVERIES"),
            }
        }
    }

    // ─── (#2108) check_host_probe — which sources resolved + the cost ──────

    /// Runs the REAL probe. macOS/aarch64-gated for the same reason the
    /// probe's own live test is: on any other platform every source is
    /// legitimately unavailable and these assertions would be vacuous.
    /// `#[serial]` because it measures the machine.
    ///
    /// The IOReport-dependent assertions (`ioreport`/`freq-tables`/
    /// `ioreg-gpu` named as resolved, no `"unavailable"` clause) are gated
    /// behind `DARKMUX_EXPECT_IOREPORT=1` — a GitHub-hosted macOS runner's
    /// VM genuinely has no IOReport channels / `pmgr` IORegistry node (a
    /// fact about the VM, not a regression), which panicked this test on
    /// every macOS CI run (#2108). `mach`/`thermal` stay unconditional: both
    /// resolve fine in that VM. Documented as a test-only knob in
    /// docs/ENVIRONMENT.md.
    #[test]
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[serial_test::serial]
    fn check_host_probe_names_the_resolved_sources_and_the_measured_cost() {
        let check = check_host_probe();
        assert_eq!(check.name, "host probe");
        assert_eq!(
            check.status,
            Status::Pass,
            "mach counters are always available on macOS: {}",
            check.message
        );
        assert!(
            check.message.contains("mach"),
            "the operator must be able to tell WHICH sources resolved: {}",
            check.message
        );
        assert!(
            check.message.contains("thermal"),
            "ProcessInfo.thermalState resolves in CI too: {}",
            check.message
        );
        assert!(
            check.message.contains("ms/sample"),
            "the observer's own cost is part of the report: {}",
            check.message
        );
        if std::env::var("DARKMUX_EXPECT_IOREPORT").as_deref() == Ok("1") {
            // Apple Silicon + IOReport is the configuration darkmux is
            // marketed for, so a build where the IOReport half silently
            // stopped resolving must FAIL here rather than quietly
            // reporting null power forever. If this fires on a future
            // macOS, the framework moved again — see
            // `host_probe::ioreport::IOREPORT_PATHS`.
            // Substring-matching `"ioreport"` alone would ALSO match the
            // "unavailable: ioreport" clause, so assert on the clause
            // itself: on Apple Silicon every source is expected to
            // resolve, and a build where one silently stopped must fail
            // here rather than reporting null power forever.
            assert!(
                !check.message.contains("unavailable"),
                "every host source is expected to resolve on Apple Silicon: {}",
                check.message
            );
            for src in ["ioreport", "freq-tables", "ioreg-gpu"] {
                assert!(
                    check.message.contains(src),
                    "`{src}` must be named among the resolved sources: {}",
                    check.message
                );
            }
        }
    }

    /// The degradation combinations the live probe cannot produce on a
    /// healthy Mac — and the ones most worth pinning, since a private
    /// framework whose path has already moved once will move again. Pure, so
    /// they run on every platform.
    #[test]
    fn describe_host_probe_names_a_missing_ioreport_rather_than_hiding_it() {
        let src = darkmux_crew::host_probe::HostProbeSources {
            mach: true,
            ioreport: false,
            freq_tables: false,
            thermal: true,
            ioreg_gpu: true,
        };
        let check = describe_host_probe(src, 3);
        assert_eq!(
            check.status,
            Status::Pass,
            "a host without IOReport still reports cpu/mem/gpu"
        );
        assert!(
            check.message.contains("unavailable: ioreport, freq-tables"),
            "the operator must be able to tell 'this Mac has no IOReport' from 'darkmux forgot \
             to read it': {}",
            check.message
        );
        assert!(check.message.contains("mach"), "{}", check.message);
        assert!(check.hint.is_some(), "a missing source comes with an explanation");
    }

    #[test]
    fn describe_host_probe_warns_when_nothing_resolved() {
        let check = describe_host_probe(darkmux_crew::host_probe::HostProbeSources::default(), 0);
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("no host sources resolved"), "{}", check.message);
    }

    #[test]
    fn describe_host_probe_warns_when_mach_itself_is_missing() {
        // Without tick counters there is no CPU figure at all — a real gap
        // even when every other source is fine.
        let src = darkmux_crew::host_probe::HostProbeSources {
            mach: false,
            ioreport: true,
            freq_tables: true,
            thermal: true,
            ioreg_gpu: true,
        };
        let check = describe_host_probe(src, 4);
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("unavailable: mach"), "{}", check.message);
    }

    #[test]
    fn describe_host_probe_omits_the_unavailable_clause_when_all_resolved() {
        let src = darkmux_crew::host_probe::HostProbeSources {
            mach: true,
            ioreport: true,
            freq_tables: true,
            thermal: true,
            ioreg_gpu: true,
        };
        let check = describe_host_probe(src, 9);
        assert_eq!(check.status, Status::Pass);
        assert!(!check.message.contains("unavailable"), "{}", check.message);
        assert!(check.message.contains("9ms/sample"), "{}", check.message);
        assert!(check.hint.is_none(), "nothing missing ⇒ nothing to explain");
    }

    // ─── (#2107, #1833) check_host_sampler_interval — resolved state + provenance ─

    #[serial_test::serial]
    #[test]
    fn check_host_sampler_interval_default_is_pass_and_names_5000ms() {
        let prev = std::env::var("DARKMUX_HOST_SAMPLER_INTERVAL_MS").ok();
        unsafe { std::env::remove_var("DARKMUX_HOST_SAMPLER_INTERVAL_MS") };
        let check = check_host_sampler_interval();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("5000ms"), "{}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOST_SAMPLER_INTERVAL_MS", v),
                None => std::env::remove_var("DARKMUX_HOST_SAMPLER_INTERVAL_MS"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_host_sampler_interval_zero_is_pass_and_says_disabled() {
        let prev = std::env::var("DARKMUX_HOST_SAMPLER_INTERVAL_MS").ok();
        unsafe { std::env::set_var("DARKMUX_HOST_SAMPLER_INTERVAL_MS", "0") };
        let check = check_host_sampler_interval();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("disabled"), "{}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOST_SAMPLER_INTERVAL_MS", v),
                None => std::env::remove_var("DARKMUX_HOST_SAMPLER_INTERVAL_MS"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_host_sampler_interval_env_override_names_provenance() {
        let prev = std::env::var("DARKMUX_HOST_SAMPLER_INTERVAL_MS").ok();
        unsafe { std::env::set_var("DARKMUX_HOST_SAMPLER_INTERVAL_MS", "2000") };
        let check = check_host_sampler_interval();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("2000ms"), "{}", check.message);
        assert!(check.message.contains("env"), "provenance named: {}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOST_SAMPLER_INTERVAL_MS", v),
                None => std::env::remove_var("DARKMUX_HOST_SAMPLER_INTERVAL_MS"),
            }
        }
    }

    // ─── (#2361, #2310 fix-loop E2) check_step_command_timeout — resolved state + provenance ─

    /// Scopes `DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS` for one check and
    /// restores the prior value — the same shape the
    /// `check_host_sampler_interval` siblings above use.
    fn step_command_timeout_check_with(env: Option<&str>) -> Check {
        let k = "DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS";
        let prev = std::env::var(k).ok();
        unsafe {
            match env {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let check = check_step_command_timeout();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        check
    }

    #[serial_test::serial]
    #[test]
    fn check_step_command_timeout_default_is_pass_and_names_600s() {
        let check = step_command_timeout_check_with(None);
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("600s"), "{}", check.message);
        assert!(check.message.contains("default"), "provenance named: {}", check.message);
    }

    #[serial_test::serial]
    #[test]
    fn check_step_command_timeout_env_override_names_the_value_and_its_provenance() {
        let check = step_command_timeout_check_with(Some("30"));
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("30s"), "{}", check.message);
        assert!(check.message.contains("env"), "provenance named: {}", check.message);
    }

    /// (#2310 fix-loop E2, from the loop-D review) `0` is UNBOUNDED, and
    /// doctor says so — the knob's meaning INVERTED in this fix (it used to
    /// kill instantly), so the one surface that reports resolved values has
    /// to report the new reading, not the number alone.
    #[serial_test::serial]
    #[test]
    fn check_step_command_timeout_zero_is_pass_and_says_unbounded() {
        let check = step_command_timeout_check_with(Some("0"));
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("unbounded"), "{}", check.message);
        assert!(!check.message.contains("killed at this bound"), "the old reading must be gone: {}", check.message);
    }

    /// The middle tier the siblings above have no test for: the check sees
    /// `config.json` and SAYS so, with the env tier absent.
    ///
    /// Only the PROVENANCE is asserted, not the resolved value, and that is
    /// a structural limit rather than an omission: `config_access::config()`
    /// is EMPTY by construction in every test build (#811 — a process-wide
    /// `OnceLock` a test could never reliably control, and a populated real
    /// config silently flaked default assertions), so a test build's
    /// resolved value is always the built-in default no matter what file
    /// exists. The value half of this tier is covered where it CAN be —
    /// `config_access`'s own `pick_parsed` tier tests, which take the config
    /// value as an explicit argument.
    #[serial_test::serial]
    #[test]
    fn check_step_command_timeout_reads_config_json_when_env_is_unset() {
        let home = tempfile::TempDir::new().unwrap();
        std::fs::write(
            home.path().join("config.json"),
            r#"{"schema_version":"1.2","runtime":{"step_command_timeout_seconds":45}}"#,
        )
        .unwrap();
        let prev_home = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", home.path()) };
        let check = step_command_timeout_check_with(None);
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("from config.json"), "provenance named: {}", check.message);
        assert!(!check.message.contains("env"), "the env tier is absent here: {}", check.message);
    }

    // ─── (#2394) check_dispatch_free_concurrency — resolved state + provenance ─

    /// Scopes `DARKMUX_DISPATCH_FREE_CONCURRENCY` for one check and restores
    /// the prior value — the same shape `step_command_timeout_check_with`
    /// above uses.
    fn dispatch_free_concurrency_check_with(env: Option<&str>) -> Check {
        let k = "DARKMUX_DISPATCH_FREE_CONCURRENCY";
        let prev = std::env::var(k).ok();
        unsafe {
            match env {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let check = check_dispatch_free_concurrency();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        check
    }

    #[serial_test::serial]
    #[test]
    fn check_dispatch_free_concurrency_default_is_pass_and_names_8() {
        let check = dispatch_free_concurrency_check_with(None);
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains('8'), "{}", check.message);
        assert!(check.message.contains("default"), "provenance named: {}", check.message);
        assert!(
            check.message.contains("remote.concurrent_cap"),
            "the message must say WHICH cap does not govern these steps — that confusion IS \
             the #2394 bug: {}",
            check.message
        );
    }

    #[serial_test::serial]
    #[test]
    fn check_dispatch_free_concurrency_env_override_names_the_value_and_its_provenance() {
        let check = dispatch_free_concurrency_check_with(Some("3"));
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains('3'), "{}", check.message);
        assert!(check.message.contains("env"), "provenance named: {}", check.message);
    }

    /// The middle tier — see `check_step_command_timeout_reads_config_json_
    /// when_env_is_unset`'s own doc for why only the PROVENANCE is asserted
    /// here and not the resolved value.
    #[serial_test::serial]
    #[test]
    fn check_dispatch_free_concurrency_reads_config_json_when_env_is_unset() {
        let home = tempfile::TempDir::new().unwrap();
        std::fs::write(
            home.path().join("config.json"),
            r#"{"schema_version":"1.21","runtime":{"dispatch_free_concurrency":3}}"#,
        )
        .unwrap();
        let prev_home = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", home.path()) };
        let check = dispatch_free_concurrency_check_with(None);
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("from config.json"), "provenance named: {}", check.message);
        assert!(!check.message.contains("env"), "the env tier is absent here: {}", check.message);
    }

    // ─── (#2404 P4d round 3) check_review_judge_exhaustion_policy — removed field ─

    #[serial_test::serial]
    #[test]
    fn check_review_judge_removed_passes_when_review_key_absent() {
        let home = tempfile::TempDir::new().unwrap();
        std::fs::write(home.path().join("config.json"), r#"{"schema_version":"1.22"}"#).unwrap();
        let prev_home = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", home.path()) };
        let check = check_review_judge_exhaustion_policy();
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert_eq!(check.status, Status::Pass, "{}", check.message);
    }

    /// The test that would have caught round 2's regression: a config
    /// produced by `DarkmuxConfig::with_defaults()` itself — the exact
    /// shape `darkmux init` writes — must Pass this check. Round 2 shipped
    /// `with_defaults()` still populating a `review` block, which this
    /// check (had it existed then) would have flagged as Warn on a
    /// brand-new, never-hand-edited config.
    #[serial_test::serial]
    #[test]
    fn check_review_judge_removed_passes_against_with_defaults() {
        use darkmux_types::config::DarkmuxConfig;
        let home = tempfile::TempDir::new().unwrap();
        let contents = serde_json::to_string_pretty(&DarkmuxConfig::with_defaults()).unwrap();
        std::fs::write(home.path().join("config.json"), contents).unwrap();
        let prev_home = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", home.path()) };
        let check = check_review_judge_exhaustion_policy();
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert_eq!(
            check.status,
            Status::Pass,
            "with_defaults() must never itself trip the removed-key warning: {}",
            check.message
        );
    }

    #[serial_test::serial]
    #[test]
    fn check_review_judge_removed_warns_and_names_the_key_when_present() {
        let home = tempfile::TempDir::new().unwrap();
        std::fs::write(
            home.path().join("config.json"),
            r#"{"schema_version":"1.21","review":{"judge_concurrency":1}}"#,
        )
        .unwrap();
        let prev_home = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", home.path()) };
        let check = check_review_judge_exhaustion_policy();
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("review"), "names the key: {}", check.message);
        assert!(
            check.message.contains(darkmux_types::config::CONFIG_SCHEMA_VERSION),
            "names the schema version it was removed in: {}",
            check.message
        );
    }

    // ─── (#2111) check_telemetry_record_every_samples — resolved state + provenance ─

    #[serial_test::serial]
    #[test]
    fn check_telemetry_record_every_samples_default_is_pass_and_names_5() {
        let prev = std::env::var("DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES").ok();
        unsafe { std::env::remove_var("DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES") };
        let check = check_telemetry_record_every_samples();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("every 5 sample"), "{}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_telemetry_record_every_samples_zero_is_pass_and_says_disabled() {
        let prev = std::env::var("DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES").ok();
        unsafe { std::env::set_var("DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES", "0") };
        let check = check_telemetry_record_every_samples();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("disabled"), "{}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_telemetry_record_every_samples_env_override_names_provenance() {
        let prev = std::env::var("DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES").ok();
        unsafe { std::env::set_var("DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES", "10") };
        let check = check_telemetry_record_every_samples();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("every 10 sample"), "{}", check.message);
        assert!(check.message.contains("env"), "provenance named: {}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_TELEMETRY_RECORD_EVERY_SAMPLES"),
            }
        }
    }

    // ─── (#2171 test e) check_generation_checkpoint_interval — resolved state ─

    #[serial_test::serial]
    #[test]
    fn check_generation_checkpoint_interval_default_is_pass_and_names_4000() {
        let prev = std::env::var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL").ok();
        let prev_max = std::env::var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL").ok();
        let prev_timeout = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
        unsafe {
            std::env::remove_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL");
            // (merge-gate review, item 1) Determinism: this test asserts
            // Pass, which now also depends on the answer-bound + inactivity
            // cross-checks — pin both to their built-in defaults so a
            // machine with a custom config.json can't flip this Warn.
            std::env::remove_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL");
            std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        }
        let check = check_generation_checkpoint_interval();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("4000"), "{}", check.message);
        assert!(check.message.contains("default"), "{}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL"),
            }
            match prev_max {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL"),
            }
            match prev_timeout {
                Some(v) => std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v),
                None => std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_generation_checkpoint_interval_env_override_names_provenance() {
        let prev = std::env::var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL").ok();
        let prev_max = std::env::var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL").ok();
        let prev_timeout = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
        unsafe {
            std::env::set_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL", "2500");
            std::env::remove_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL");
            std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        }
        let check = check_generation_checkpoint_interval();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("2500"), "{}", check.message);
        assert!(check.message.contains("env"), "provenance named: {}", check.message);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL"),
            }
            match prev_max {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL"),
            }
            match prev_timeout {
                Some(v) => std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v),
                None => std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS"),
            }
        }
    }

    /// (merge-gate review, item 1) `0` is not an off-switch — the runtime
    /// CLI rejects it and every dispatch that reaches it exits with code 2.
    #[serial_test::serial]
    #[test]
    fn check_generation_checkpoint_interval_zero_warns_and_names_the_real_off_switch() {
        let prev_gen = std::env::var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL").ok();
        let prev_max = std::env::var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL").ok();
        let prev_timeout = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
        unsafe {
            std::env::set_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL", "0");
            std::env::remove_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL");
            std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        }
        let check = check_generation_checkpoint_interval();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(
            check.message.contains("exits with code 2") || check.message.contains("rejects"),
            "must explain WHY 0 is dangerous, not just flag it: {}",
            check.message
        );
        let hint = check.hint.as_deref().unwrap_or_default();
        assert!(
            hint.contains("at or above") && hint.contains("max_tokens_per_call"),
            "must name the REAL off-switch (>= max_tokens_per_call), not imply 0 works: {hint}"
        );
        unsafe {
            match prev_gen {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL"),
            }
            match prev_max {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL"),
            }
            match prev_timeout {
                Some(v) => std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v),
                None => std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS"),
            }
        }
    }

    /// (merge-gate review, item 1) At or above `max_tokens_per_call`, the
    /// generation check-in can never be the tighter cap — silently
    /// disabled, reproducing the #2171 incident even with the fix merged.
    #[serial_test::serial]
    #[test]
    fn check_generation_checkpoint_interval_at_or_above_max_tokens_per_call_warns_disabled() {
        let prev_gen = std::env::var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL").ok();
        let prev_max = std::env::var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL").ok();
        let prev_timeout = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
        unsafe {
            // Equal, the boundary case (>=) — must still warn, not just >.
            std::env::set_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL", "5000");
            std::env::set_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", "5000");
            std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS");
        }
        let check = check_generation_checkpoint_interval();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(
            check.message.contains("silently disabled"),
            "must name the failure mode: {}",
            check.message
        );
        unsafe {
            match prev_gen {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL"),
            }
            match prev_max {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL"),
            }
            match prev_timeout {
                Some(v) => std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v),
                None => std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS"),
            }
        }
    }

    /// (merge-gate review, item 1) A generation interval that could plausibly
    /// take longer than the inactivity budget to generate, at a conservative
    /// 10 tok/s floor, must warn — this is the actual #2171 incident
    /// reproduced with a slower model even after the fix ships.
    #[serial_test::serial]
    #[test]
    fn check_generation_checkpoint_interval_close_to_inactivity_timeout_warns() {
        let prev_gen = std::env::var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL").ok();
        let prev_max = std::env::var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL").ok();
        let prev_timeout = std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok();
        unsafe {
            // 6000 tokens / 10 tok/s = 600s, >= a 300s inactivity budget.
            std::env::set_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL", "6000");
            std::env::remove_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL"); // built-in 10000, well above 6000
            std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", "300");
        }
        let check = check_generation_checkpoint_interval();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("300"), "{}", check.message);
        let hint = check.hint.as_deref().unwrap_or_default();
        assert!(
            hint.contains("inactivity budget") && hint.contains("runtime.inactivity_timeout_seconds"),
            "hint must name both the danger AND the two knobs the operator can turn: {hint}"
        );
        unsafe {
            match prev_gen {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL"),
            }
            match prev_max {
                Some(v) => std::env::set_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL", v),
                None => std::env::remove_var("DARKMUX_RUNTIME_MAX_TOKENS_PER_CALL"),
            }
            match prev_timeout {
                Some(v) => std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", v),
                None => std::env::remove_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS"),
            }
        }
    }

    /// Mirrors `check_turn_delay_unparseable_env_warns_and_names_the_raw_value`
    /// — a set-but-garbage env var must not be silently reported as "from
    /// ... env" while the resolved value actually came from a lower tier.
    #[serial_test::serial]
    #[test]
    fn check_host_sampler_interval_unparseable_env_warns_and_names_the_raw_value() {
        let prev = std::env::var("DARKMUX_HOST_SAMPLER_INTERVAL_MS").ok();
        unsafe { std::env::set_var("DARKMUX_HOST_SAMPLER_INTERVAL_MS", "5s") };
        let check = check_host_sampler_interval();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(
            check.message.contains("DARKMUX_HOST_SAMPLER_INTERVAL_MS") && check.message.contains("5s"),
            "must name the raw unparseable value: {}",
            check.message
        );
        assert!(
            !check.message.contains("from DARKMUX_HOST_SAMPLER_INTERVAL_MS env"),
            "must NOT claim provenance is env when the env value didn't parse: {}",
            check.message
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOST_SAMPLER_INTERVAL_MS", v),
                None => std::env::remove_var("DARKMUX_HOST_SAMPLER_INTERVAL_MS"),
            }
        }
    }

    // ─── (#1769) summarize_audit_reports — Fail / Warn / Pass split ────────
    //
    // Pure-function tests: no filesystem, no `DARKMUX_AUDIT_DIR`. Each test
    // constructs the `IntegrityReport`(s) `flow integrity-check` would have
    // produced and checks which doctor `Status` (and therefore which exit
    // code, per `main.rs`'s `Fail => 1, _ => 0`) they map to.

    fn mk_clean_report(records_checked: u64) -> darkmux_flow::IntegrityReport {
        darkmux_flow::IntegrityReport {
            path: "2026-08-11.jsonl".into(),
            records_checked,
            chain_valid: true,
            break_at_line: None,
            break_reason: None,
            legacy_format: false,
            note: None,
            writer_schema_version: Some("1.19.0".into()),
        }
    }

    #[test]
    fn summarize_audit_reports_broken_chain_is_fail() {
        let broken = darkmux_flow::IntegrityReport {
            chain_valid: false,
            break_at_line: Some(4),
            break_reason: Some(
                "hash mismatch: stored `a` != recomputed `b` (record content has been edited)"
                    .into(),
            ),
            ..mk_clean_report(3)
        };
        let check = summarize_audit_reports(&[mk_clean_report(2), broken]);
        assert_eq!(
            check.status,
            Status::Fail,
            "a genuine chain break must FAIL the check — this is the only status that flips \
             doctor's exit code to 1, and it must not be softened by the legacy-format case"
        );
        assert!(check.message.contains("BROKEN"));
    }

    #[test]
    fn summarize_audit_reports_legacy_format_is_warn_not_fail() {
        let legacy = darkmux_flow::IntegrityReport {
            chain_valid: true,
            legacy_format: true,
            note: Some(
                "written in the legacy struct-hash format (pre-2.6.0); not re-verifiable under \
                 byte-hash verification (#1769) — the stored hash was computed over a \
                 re-serialization of the parsed record, which this binary cannot reproduce \
                 byte-for-byte. This is a format boundary, not evidence of editing."
                    .into(),
            ),
            writer_schema_version: Some("1.18.0".into()),
            ..mk_clean_report(5)
        };
        let check = summarize_audit_reports(&[legacy]);
        assert_eq!(
            check.status,
            Status::Warn,
            "#1769: a legacy-format file was never content-verified — it must never reach \
             Fail (doctor's only exit-code-flipping status), but it also must not silently \
             fold into Pass, or the caveat disappears"
        );
        assert!(
            !check.message.to_lowercase().contains("edited") && !check.message.contains("BROKEN"),
            "wording must not assert editing; got {:?}",
            check.message
        );
        assert!(
            check.message.contains("5 record"),
            "the unverified count must be surfaced; got {:?}",
            check.message
        );
    }

    #[test]
    fn summarize_audit_reports_clean_chain_is_pass() {
        let check = summarize_audit_reports(&[mk_clean_report(3), mk_clean_report(7)]);
        assert_eq!(check.status, Status::Pass);
        assert!(check.message.contains("10 record"));
    }

    // ─── (#1569 packet A) viewer_link_base routing ─────────────────────────
    //
    // The routing IS the feature — this function exists to make one choice —
    // so both non-spawning branches are pinned. The tailnet branch spawns a
    // real subprocess and stays untested here; its parser has its own tests
    // against a captured fixture, and its DEADLINE is the part that matters,
    // covered separately below.
    //
    // `#[serial]`: mutates the process-global colorize override and env.

    #[test]
    #[serial_test::serial]
    fn thermal_governor_warns_on_unrecognized_pause_at() {
        // (#2110/#2109 review finding 6) A typo'd pause_at silently
        // inverts the governor's intent (see Ty::ThermalState's doc in
        // src/config_cmd.rs) — this must surface as a loud Warn, not fold
        // silently into the informational Pass message.
        let prev = std::env::var("DARKMUX_THERMAL_PAUSE_AT").ok();
        unsafe { std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", "seroius") };

        let check = check_thermal_governor();
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("seroius"), "{}", check.message);
        assert!(check.message.contains("unrecognized thermal state"), "{}", check.message);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", v),
                None => std::env::remove_var("DARKMUX_THERMAL_PAUSE_AT"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn thermal_governor_warns_on_zero_speed_limit_hold_samples() {
        // (N2, final re-check) An explicit 0 is silently coerced to 1 by
        // the accessor (see thermal_speed_limit_hold_samples's own doc) —
        // this must surface as a Warn so the operator knows their 0
        // didn't achieve "disable" semantics.
        let prev = std::env::var("DARKMUX_THERMAL_SPEED_LIMIT_HOLD_SAMPLES").ok();
        unsafe { std::env::set_var("DARKMUX_THERMAL_SPEED_LIMIT_HOLD_SAMPLES", "0") };

        let check = check_thermal_governor();
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("speed_limit_hold_samples"), "{}", check.message);
        assert!(check.message.contains("coerced to 1"), "{}", check.message);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_THERMAL_SPEED_LIMIT_HOLD_SAMPLES", v),
                None => std::env::remove_var("DARKMUX_THERMAL_SPEED_LIMIT_HOLD_SAMPLES"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn viewer_link_base_returns_loopback_without_a_tty() {
        // No TTY -> no links are emitted at all, so there is nothing to
        // resolve and (critically) no `tailscale` subprocess to spawn. This
        // is what keeps `| grep` and `--json` free of both escapes and cost.
        let prev = std::env::var("DARKMUX_FLEET_MODE").ok();
        unsafe { std::env::set_var("DARKMUX_FLEET_MODE", "hub") };
        darkmux_types::style::set_colorize_override(Some(false));

        assert_eq!(viewer_link_base(8765), "http://127.0.0.1:8765/");

        darkmux_types::style::set_colorize_override(None);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLEET_MODE", v),
                None => std::env::remove_var("DARKMUX_FLEET_MODE"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn viewer_link_base_standalone_is_loopback_even_at_a_tty() {
        // The operator-agreed rule (#1569): a single-machine install has no
        // second daemon a link could open by mistake, so loopback carries no
        // ambiguity — and a fresh install that never set up tailscale must
        // still get working links. Also proves the standalone path never
        // spawns `tailscale`, since a machine without it installed must not
        // pay for a failed spawn on every board render.
        let prev = std::env::var("DARKMUX_FLEET_MODE").ok();
        unsafe { std::env::set_var("DARKMUX_FLEET_MODE", "standalone") };
        darkmux_types::style::set_colorize_override(Some(true));

        assert_eq!(viewer_link_base(8765), "http://127.0.0.1:8765/");
        assert_eq!(viewer_link_base(9999), "http://127.0.0.1:9999/", "port is honored");

        darkmux_types::style::set_colorize_override(None);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLEET_MODE", v),
                None => std::env::remove_var("DARKMUX_FLEET_MODE"),
            }
        }
    }

    /// (#1593 gate, MUST FIX) The probe must not outlive its deadline. A
    /// wedged `tailscaled` used to hang `mission status` forever — the same
    /// unbounded-external-dependency class #1570/#1573 removed for Redis.
    /// `sleep 30` stands in for the wedge; the call must return promptly and
    /// degrade to `None` rather than wait.
    #[test]
    fn tailnet_probe_is_bounded_and_degrades_to_none() {
        // Shadow `tailscale` with a script that never answers.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("tailscale");
        std::fs::write(&fake, "#!/bin/sh\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prev_path = std::env::var("PATH").unwrap_or_default();
        unsafe { std::env::set_var("PATH", format!("{}:{prev_path}", dir.path().display())) };

        let started = std::time::Instant::now();
        let got = tailnet_viewer_url_bounded(8765, std::time::Duration::from_millis(300));
        let elapsed = started.elapsed();

        unsafe { std::env::set_var("PATH", prev_path) };

        assert!(got.is_none(), "a wedged probe resolves to no tailnet URL, never a hang");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "probe must be bounded; took {elapsed:?}"
        );
    }

    // ─── (#1461) staleness: daemon / binary-vs-source / runtime image ───────
    //
    // Every check is exercised as a pure function over injected inputs — no
    // live daemon, no docker, no git. The probes that gather those inputs are
    // thin and deliberately total (every failure resolves to "not applicable").

    /// A daemon reporting `build` and the mtime of the binary it loaded.
    fn modern(build: &str, mtime: u64) -> Option<DaemonBuild> {
        Some(DaemonBuild::Modern {
            build: build.into(),
            binary_mtime: Some(mtime),
        })
    }

    #[test]
    fn daemon_freshness_passes_when_build_and_binary_mtime_both_match() {
        let c = classify_daemon_freshness(modern("2.0.0 (a1b2c3d)", 1000), "2.0.0 (a1b2c3d)", Some(1000));
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.hint.is_none());
    }

    #[test]
    fn daemon_freshness_warns_naming_both_builds_when_they_differ() {
        let c = classify_daemon_freshness(
            modern("1.18.5 (0ldc0de)", 1000),
            "2.0.0 (a1b2c3d)",
            Some(2000),
        );
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        // Provenance: the operator must see BOTH resolved values, not just that
        // "something is stale" (#44 — never wonder where a decision came from).
        assert!(c.message.contains("1.18.5 (0ldc0de)"), "{}", c.message);
        assert!(c.message.contains("2.0.0 (a1b2c3d)"), "{}", c.message);
        let hint = c.hint.as_deref().unwrap();
        assert!(hint.contains("darkmux serve"), "restart fix: {hint}");
    }

    #[test]
    fn daemon_freshness_warns_on_a_reinstall_at_the_same_commit() {
        // THE case that bit (#1461). `cargo install --path .` from a tree with
        // uncommitted edits produces a binary whose build tag is byte-identical
        // to the running daemon's — same SHA, same dirty marker. Only the mtime
        // moved. A build-string comparison alone would report this as fresh and
        // the operator would go on testing the previous build.
        let c = classify_daemon_freshness(
            modern("2.0.0 (a1b2c3d\u{2731})", 1_000_000),
            "2.0.0 (a1b2c3d\u{2731})",
            Some(1_000_000 + 900),
        );
        assert_eq!(
            c.status,
            Status::Warn,
            "identical build tags must NOT be treated as fresh: {}",
            c.message
        );
        assert!(c.message.contains("15m"), "names the age: {}", c.message);
        assert!(c.message.contains("reinstalled"), "{}", c.message);
        assert!(c.hint.as_deref().unwrap().contains("darkmux serve"));
    }

    #[test]
    fn daemon_freshness_warns_when_the_daemon_binary_is_newer_than_this_cli() {
        // The reverse skew: the daemon was started from a fresher build than the
        // darkmux on this PATH. A restart is not the fix, so the message says
        // what is true rather than prescribing the wrong action (#44).
        let c = classify_daemon_freshness(
            modern("2.0.0 (a1b2c3d)", 5_000),
            "2.0.0 (a1b2c3d)",
            Some(2_000),
        );
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert!(c.message.contains("AFTER"), "{}", c.message);
        assert!(c.message.contains("50m"), "names the skew: {}", c.message);
        // Restart is the WRONG primary fix here (it would load the older on-disk
        // binary) — the hint must lead with refreshing this CLI instead.
        let hint = c.hint.as_deref().unwrap();
        assert!(hint.contains("cargo install --path ."), "{hint}");
        assert!(
            hint.contains("newer than this CLI"),
            "hint frames the skew, not a plain restart: {hint}"
        );
    }

    #[test]
    fn daemon_freshness_passes_when_mtimes_are_unknowable() {
        // A daemon that couldn't stat its own exe, or a doctor that can't stat
        // its own: fall back to the build tag alone rather than inventing a
        // finding out of a missing input.
        let c = classify_daemon_freshness(
            Some(DaemonBuild::Modern {
                build: "2.0.0 (a1b2c3d)".into(),
                binary_mtime: None,
            }),
            "2.0.0 (a1b2c3d)",
            Some(1000),
        );
        assert_eq!(c.status, Status::Pass, "{}", c.message);

        let c = classify_daemon_freshness(modern("2.0.0 (a1b2c3d)", 1000), "2.0.0 (a1b2c3d)", None);
        assert_eq!(c.status, Status::Pass, "{}", c.message);
    }

    #[test]
    fn daemon_freshness_warns_when_the_daemon_predates_the_build_field() {
        // A daemon with no `build` field was compiled before this check shipped,
        // so it is stale by construction — no version comparison needed (and
        // none is made: a bare version vs a build-tagged one is not comparable).
        let c = classify_daemon_freshness(
            Some(DaemonBuild::Legacy("1.18.5".into())),
            "2.0.0 (a1b2c3d)",
            Some(1000),
        );
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert!(c.message.contains("1.18.5"), "{}", c.message);
        assert!(c.message.contains("2.0.0 (a1b2c3d)"), "{}", c.message);
        assert!(c.hint.as_deref().unwrap().contains("darkmux serve"));
    }

    #[test]
    fn daemon_freshness_not_applicable_when_no_daemon_running() {
        // The common case — most users never run a daemon. Never a warning.
        let c = classify_daemon_freshness(None, "2.0.0 (a1b2c3d)", Some(1000));
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.message.contains("not applicable"), "{}", c.message);
        assert!(c.hint.is_none());
    }

    #[test]
    fn fmt_age_renders_short_human_spans() {
        assert_eq!(fmt_age(45), "45s");
        assert_eq!(fmt_age(900), "15m");
        assert_eq!(fmt_age(7200), "2h");
        assert_eq!(fmt_age(7380), "2h 3m");
        assert_eq!(fmt_age(172_800), "2d");
        assert_eq!(fmt_age(180_000), "2d 2h");
    }

    #[test]
    fn daemon_build_parses_build_and_binary_mtime() {
        let body = r#"{"darkmux_version":"2.0.0","build":"2.0.0 (a1b2c3d)","binary_mtime":1700}"#;
        assert_eq!(
            parse_daemon_build(body),
            Some(DaemonBuild::Modern {
                build: "2.0.0 (a1b2c3d)".into(),
                binary_mtime: Some(1700)
            })
        );
    }

    #[test]
    fn daemon_build_tolerates_a_daemon_that_could_not_stat_its_own_exe() {
        // `binary_mtime: null` is a real shape the daemon emits — it must parse
        // as Modern-without-mtime, not fall through to Legacy.
        let body = r#"{"darkmux_version":"2.0.0","build":"2.0.0 (a1b2c3d)","binary_mtime":null}"#;
        assert_eq!(
            parse_daemon_build(body),
            Some(DaemonBuild::Modern {
                build: "2.0.0 (a1b2c3d)".into(),
                binary_mtime: None
            })
        );
    }

    #[test]
    fn daemon_build_reads_a_pre_build_field_daemon_as_legacy() {
        // A daemon older than #1461 has no `build` field — classified as Legacy
        // rather than silently compared against a build-tagged string.
        let body = r#"{"darkmux_version":"1.18.5","flow_schema_version":"1.4"}"#;
        assert_eq!(
            parse_daemon_build(body),
            Some(DaemonBuild::Legacy("1.18.5".into()))
        );
    }

    #[test]
    fn daemon_build_is_none_on_garbage() {
        assert!(parse_daemon_build("not json").is_none());
        assert!(parse_daemon_build(r#"{"unrelated":true}"#).is_none());
    }

    #[test]
    fn built_from_sha_extracts_git_tag_and_strips_the_dirty_marker() {
        assert_eq!(built_from_sha("2.0.0 (a1b2c3d)").as_deref(), Some("a1b2c3d"));
        // `✱` means the tree was dirty at build time — it does not change WHICH
        // commit the binary came from, so it must not defeat the comparison.
        assert_eq!(
            built_from_sha("2.0.0 (a1b2c3d\u{2731})").as_deref(),
            Some("a1b2c3d")
        );
    }

    #[test]
    fn built_from_sha_is_none_for_release_and_tarball_builds() {
        // A packaged release has no commit to compare against...
        assert!(built_from_sha("2.0.0 (release)").is_none());
        // ...and neither does a bare source-tarball build.
        assert!(built_from_sha("2.0.0").is_none());
    }

    #[test]
    fn binary_vs_source_passes_when_binary_was_built_from_head() {
        let c = classify_binary_vs_source(Some("a1b2c3d"), Some("a1b2c3d"));
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.hint.is_none());
    }

    #[test]
    fn binary_vs_source_warns_naming_both_commits_when_they_differ() {
        let c = classify_binary_vs_source(Some("0ldc0de"), Some("a1b2c3d"));
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert!(c.message.contains("0ldc0de"), "{}", c.message);
        assert!(c.message.contains("a1b2c3d"), "{}", c.message);
        assert!(
            c.hint.as_deref().unwrap().contains("cargo install --path ."),
            "fix_hint points at the reinstall: {:?}",
            c.hint
        );
    }

    #[test]
    fn binary_vs_source_not_applicable_without_a_source_tree() {
        // A brew user must NEVER see this check fire. No source tree = silent.
        let c = classify_binary_vs_source(Some("a1b2c3d"), None);
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.message.contains("not applicable"), "{}", c.message);
    }

    #[test]
    fn binary_vs_source_not_applicable_for_a_release_binary_in_a_source_tree() {
        // `brew install darkmux` + `git clone darkmux` is a normal thing to do.
        let c = classify_binary_vs_source(None, Some("a1b2c3d"));
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.message.contains("not applicable"), "{}", c.message);
    }

    #[test]
    fn source_root_found_only_for_a_darkmux_workspace_with_git() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\".\", \"crates/darkmux-types\"]\n",
        )
        .unwrap();
        let nested = root.join("crates").join("darkmux-doctor");
        std::fs::create_dir_all(&nested).unwrap();
        // Found from the root and from anywhere beneath it.
        assert_eq!(find_darkmux_source_root(root).as_deref(), Some(root));
        assert_eq!(find_darkmux_source_root(&nested).as_deref(), Some(root));
    }

    #[test]
    fn source_root_rejects_a_foreign_rust_checkout() {
        // Someone else's Cargo workspace is not a darkmux source tree.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"app\"]\n").unwrap();
        assert!(find_darkmux_source_root(root).is_none());
    }

    #[test]
    fn source_root_rejects_a_darkmux_tarball_with_no_git() {
        // No `.git` = no HEAD to compare against.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/darkmux-types\"]\n",
        )
        .unwrap();
        assert!(find_darkmux_source_root(root).is_none());
    }

    #[test]
    fn runtime_image_passes_when_the_label_matches_the_binary() {
        let c = classify_runtime_image_freshness(
            RuntimeImageProbe::Labeled("2.0.0".into()),
            "2.0.0",
        );
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.hint.is_none());
    }

    #[test]
    fn runtime_image_warns_naming_both_versions_when_the_label_is_older() {
        let c = classify_runtime_image_freshness(
            RuntimeImageProbe::Labeled("1.18.5".into()),
            "2.0.0",
        );
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert!(c.message.contains("1.18.5"), "{}", c.message);
        assert!(c.message.contains("2.0.0"), "{}", c.message);
        let hint = c.hint.as_deref().unwrap();
        assert!(hint.contains("docker build"), "build fix: {hint}");
        // The hint must name a version the operator can paste, not a placeholder.
        assert!(hint.contains("DARKMUX_VERSION=2.0.0"), "{hint}");
    }

    #[test]
    fn runtime_image_not_applicable_when_docker_or_the_image_is_absent() {
        // Docker is NOT a hard dependency of doctor — many users have none.
        let c = classify_runtime_image_freshness(
            RuntimeImageProbe::NotApplicable("`docker` not available".into()),
            "2.0.0",
        );
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.message.contains("not applicable"), "{}", c.message);
        assert!(c.hint.is_none());
    }

    #[test]
    fn runtime_image_unlabeled_is_informational_not_a_warning() {
        // An image built before the label shipped has nothing to compare.
        let c = classify_runtime_image_freshness(RuntimeImageProbe::Unlabeled, "2.0.0");
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.message.contains("no version label"), "{}", c.message);
    }

    // ─── (#2386 review) the injected runtime binary's own cache ─────────

    fn cache_stamp(version: &str, image_id: Option<&str>) -> darkmux_crew::dispatch_internal::RuntimeBinaryStamp {
        darkmux_crew::dispatch_internal::RuntimeBinaryStamp {
            version: version.to_string(),
            image_id: image_id.map(String::from),
        }
    }

    #[test]
    fn a_runtime_binary_cache_stamped_for_another_build_warns_and_names_both() {
        let c = classify_runtime_binary_cache(true, Some(cache_stamp("3.5.0", None)), "3.6.0");
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert!(c.message.contains("3.5.0") && c.message.contains("3.6.0"), "{}", c.message);
        assert!(c.hint.is_some(), "and says what to do about it");
    }

    #[test]
    fn a_matching_or_absent_runtime_binary_cache_passes() {
        assert_eq!(
            classify_runtime_binary_cache(true, Some(cache_stamp("3.6.0", None)), "3.6.0").status,
            Status::Pass
        );
        let none = classify_runtime_binary_cache(false, None, "3.6.0");
        assert_eq!(none.status, Status::Pass, "{}", none.message);
        assert!(none.message.contains("no cached runtime binary"), "{}", none.message);
    }

    /// (#2386 C4) The matching-version Pass names the image id when the
    /// stamp carries one — the surface the operator uses to confirm which
    /// image build a cached binary was actually extracted from.
    #[test]
    fn a_matching_runtime_binary_cache_names_its_image_id_when_known() {
        let c = classify_runtime_binary_cache(true, Some(cache_stamp("3.6.0", Some("sha256:aaa"))), "3.6.0");
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("sha256:aaa"), "{}", c.message);
    }

    /// (#2386 C8) An UNSTAMPED cached binary (the file is there, but there is
    /// no readable version line) must not read as "no cached runtime
    /// binary" — that wording is reserved for the case where the file is
    /// genuinely absent, and an operator who can see the file on disk would
    /// otherwise read the check as contradicting reality.
    #[test]
    fn an_unstamped_cached_binary_is_worded_differently_from_no_binary_at_all() {
        let absent = classify_runtime_binary_cache(false, None, "3.6.0");
        let unstamped = classify_runtime_binary_cache(true, None, "3.6.0");
        assert_eq!(absent.status, Status::Pass);
        assert_eq!(unstamped.status, Status::Pass);
        assert!(absent.message.contains("no cached runtime binary"), "{}", absent.message);
        assert!(
            !unstamped.message.contains("no cached runtime binary"),
            "an unstamped binary is not the same as no binary: {}",
            unstamped.message
        );
        assert!(unstamped.message.contains("predates"), "{}", unstamped.message);
        assert_ne!(absent.message, unstamped.message);
    }

    #[test]
    fn staleness_checks_never_fail_only_warn() {
        // Sovereignty (#44): a deliberately-old daemon/image is a legitimate
        // operator choice. These checks surface; they never block.
        let all = [
            classify_daemon_freshness(modern("old", 1000), "new", Some(2000)),
            // Same build tag, reinstalled binary — the dev-box case.
            classify_daemon_freshness(modern("same", 1000), "same", Some(2000)),
            classify_daemon_freshness(Some(DaemonBuild::Legacy("1.18.5".into())), "new", Some(1000)),
            classify_binary_vs_source(Some("0ldc0de"), Some("a1b2c3d")),
            classify_runtime_image_freshness(RuntimeImageProbe::Labeled("1.0.0".into()), "2.0.0"),
            classify_runtime_binary_cache(true, Some(cache_stamp("1.0.0", None)), "2.0.0"),
        ];
        for c in all {
            assert_ne!(c.status, Status::Fail, "{} must never fail", c.name);
        }
    }

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
        // family; #1758 removed orchestrator-declared, a write-only field's
        // check), incl. build-identity [#1129] + docker-runtime [#680] +
        // load projection + daemon reachable +
        // darkmux-version-vs-latest-release [#13] +
        // crew-role-prompt-coverage [#141] + flow-sink-health [#170] +
        // machine_id [#167] + openai-base-url-conflict [#5] +
        // audit-integrity [#163] + utility-model-binding
        // [#590] + legacy-mission-layout [#148] + beat-33-crew-dir [Beat 33
        // directory flatten] + role-tool-vocab [#340] +
        // legacy-compaction-extras [#380] + redis-config [#661] +
        // remote-endpoint-credentials [#85/#91] + audit-write-drops [#877] +
        // serve-daemon-auth [#881] + fleet.mode [#933] + env-masks-config
        // [#934] + binary-split-brain [#934] + crew-validation [#1269] +
        // mission-config-registry [#1284] + daemon-freshness +
        // binary-vs-source + runtime-image-freshness [#1461] + role-profiles
        // [#1475] + cmd-gate-allowlist [#1685] + unpriceable-residents
        // [#1819] + review-judge-exhaustion-policy [#1876/#1877] +
        // turn-delay [#2094] + reasoning-checkpoint-interval [#2165] +
        // host-sampler-interval [#2107, #1833] +
        // telemetry-record-every-samples [#2111] +
        // generation-checkpoint-interval [#2171] +
        // thermal-governor [#2110/#2109] +
        // mission-envelope-readability [#1881] + hooks [#2093] +
        // rules [#1959] + host-probe [#2107] + power-posture [#2112,
        // battery/Low-Power-Mode/thermal-state/thermal-emergency] +
        // max-stall-recoveries [#2190] +
        // step-command-timeout [#2361] +
        // dispatch-free-concurrency [#2394] +
        // quarantined-mirrors [#2399] +
        // runtime-binary-cache [#2386 — the injected runtime binary's own
        // version-keyed cache, the mirror of runtime-image-freshness]) +
        // hooks [#2093, ALWAYS exactly 1 check — the overview row; per-rule
        // hooks checks are a different, disabled-by-default surface] + one
        // per active eureka rule.
        //
        // (round-3 merge fix) The constant here is 54, not 53: the static
        // array above literally has 53 entries (recount it before touching
        // this number — `grep -c` inside the `let checks = vec![...]`
        // block), `check_hooks()` always contributes exactly 1 more
        // (disabled by default → the single overview check), and only
        // THEN does `eureka_checks()` add one per active rule. A prior
        // rebase kept an origin/main-side "53" that predated this branch's
        // own `check_runtime_binary_cache` addition to the static array,
        // silently undercounting by exactly the one check the OTHER side
        // of that same merge conflict had just added — proof that a
        // colliding-file rebase needs its literal counts re-derived, not
        // just its prose reconciled.
        //
        // Every check should appear regardless of environment — even if the
        // underlying probe couldn't read state.
        let expected = 54 + darkmux_eureka::all_rules().len();
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
        let json = r#"{"TCP":{"80":{"HTTP":true}},"Web":{"laptop.tailnet-example.ts.net:80":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8765"}}}}}"#;
        assert_eq!(
            parse_tailnet_viewer_url(json, 8765).as_deref(),
            Some("http://laptop.tailnet-example.ts.net/")
        );
        // A different daemon port → not our proxy → None.
        assert_eq!(parse_tailnet_viewer_url(json, 9000), None);
        // Served on 443 → https scheme.
        let j443 = r#"{"Web":{"tailnet-example.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8765"}}}}}"#;
        assert_eq!(parse_tailnet_viewer_url(j443, 8765).as_deref(), Some("https://tailnet-example.ts.net/"));
        // localhost proxy target is accepted too.
        let jlocal = r#"{"Web":{"example.ts.net:80":{"Handlers":{"/":{"Proxy":"http://localhost:8765"}}}}}"#;
        assert_eq!(parse_tailnet_viewer_url(jlocal, 8765).as_deref(), Some("http://example.ts.net/"));
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

    /// (#1839) darkmux describes its own state; it does not adjudicate the
    /// operator's posture. Every string here is printed by `doctor`, and
    /// `doctor` output is republished verbatim by the viewer's console lens —
    /// two surfaces, one source, so the rule is enforced at the source.
    ///
    /// The specific regression this pins: the no-token hint opened with
    /// "Safe as-is for a single machine." Conditionally true, and false for
    /// the setup the project actually recommends — a loopback daemon behind a
    /// Tailscale reverse proxy, where the same daemon is reachable by the
    /// whole tailnet and this check never looked at the proxy.
    #[test]
    fn daemon_auth_and_redis_hints_state_facts_without_rendering_a_verdict() {
        let verdicts = ["safe as-is", "is fine", "secure", "protected", "no risk", "for compliance"];
        let (_, msg_t, hint_t) = daemon_auth_status(true);
        let (_, msg_f, hint_f) = daemon_auth_status(false);
        for text in [msg_t, msg_f, hint_t.unwrap_or_default(), hint_f.unwrap_or_default()] {
            let low = text.to_lowercase();
            for v in verdicts {
                assert!(!low.contains(v), "doctor must not adjudicate the operator's posture ({v:?}): {text}");
            }
        }
        // Still says the useful part: what is configured, and how to change it.
        let (_, msg, hint) = daemon_auth_status(false);
        assert!(msg.contains("loopback-only"), "still reports the actual state: {msg}");
        assert!(hint.unwrap().contains("darkmux-serve-token"), "still actionable");
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

    /// (#1676) The warning stands — an unloaded utility model still matters
    /// for the verbs that read the global `internal.utility` binding — but its
    /// REMEDY must not describe the pre-#1616 world.
    ///
    /// Two claims were wrong and one was actively harmful. Wrong: that
    /// compaction fails without a manual load (`dispatch_internal` self-loads
    /// the compactor at its declared `n_ctx`). Harmful: suggesting a bare `lms
    /// load <id>`, which creates the non-namespaced resident the namespace
    /// contract calls the #1135 ghost — unknown load config, never reused by
    /// darkmux, unreachable by `machine eject`. Following the hint could cause
    /// the thing the namespace exists to prevent.
    #[test]
    fn utility_binding_not_loaded_hint_does_not_prescribe_a_bare_manual_load() {
        let loaded = vec![lm("worker", "worker-35b")];
        let c = super::utility_binding_status(Some("util-4b"), Some(&loaded));
        assert_eq!(c.status, Status::Warn, "an unloaded utility binding is still worth surfacing");
        assert!(c.message.contains("registered but NOT loaded"));
        let hint = c.hint.expect("a warn carries a remedy");
        assert!(
            !hint.contains("the compactor call fails"),
            "the dispatch path self-loads the compactor since #1616: {hint}"
        );
        assert!(
            !hint.contains("Load it before dispatching"),
            "dispatch needs nothing done first: {hint}"
        );
        // (#1675 gate finding) Pin the COMMAND, not the prose. A bare
        // `contains("darkmux:")` was satisfied by the sentence "under the
        // `darkmux:` namespace" even if the suggested invocation regressed to
        // a namespace-dropping `lms load <id>` — i.e. the assertion whose
        // comment promised the namespace didn't actually check it.
        assert!(
            hint.contains("--identifier darkmux:"),
            "a suggested manual load must carry the namespace flag, or it makes a #1135 ghost: {hint}"
        );
        assert!(
            hint.contains("--context-length"),
            "and the declared context, or the hand-load lands at the model default: {hint}"
        );
        // The hint must not resurrect the false contrast that replaced the
        // original false claim: `utility_model_id()` only ever names the
        // compactor, and `mission propose` / `lab notebook draft` reach the
        // same self-loading path as every other verb.
        for verb in ["mission propose", "lab notebook draft"] {
            assert!(
                !hint.contains(verb),
                "no verb needs this resident first — naming {verb:?} implies one does: {hint}"
            );
        }
    }

    // ─── check_unpriceable_residents (#1819) ──────────────────────────────

    /// Minimal `ModelRow` builder for these tests — every field but the two
    /// this check reads (`model_key`, `potential_bytes`) is filler, matching
    /// the fixture-construction style already used for `ArchFacts` above.
    fn row(model_key: &str, potential_bytes: Option<u64>) -> darkmux_profiles::model_ledger::ModelRow {
        use darkmux_profiles::model_ledger::{LedgerState, ModelRow, Owner};
        ModelRow {
            identifier: model_key.to_string(),
            model_key: model_key.to_string(),
            owner: Owner::User,
            loaded_ctx: 8_192,
            weights_bytes: None,
            kv_per_token_bytes: None,
            kv_bytes_at_ctx: None,
            potential_bytes,
            potential_source: None,
            current_bytes: None,
            state: LedgerState::Unknown,
            over_price_bytes: None,
            shrink_hint: None,
        }
    }

    #[test]
    fn unpriceable_residents_empty_ledger_passes() {
        let c = super::unpriceable_residents_status(&[]);
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("priceable"));
    }

    #[test]
    fn unpriceable_residents_all_priced_passes() {
        let rows = [row("qwen3.6-35b-a3b", Some(24_000_000_000)), row("phi-4-gguf", Some(10_000_000_000))];
        let c = super::unpriceable_residents_status(&rows);
        assert_eq!(c.status, Status::Pass);
    }

    /// The live #1819 trace: `microsoft/phi-4` has no `potential_bytes` at
    /// all (neither arch facts nor a resolvable catalog size) — WARN, name
    /// it, and hint the MLX-build remedy.
    #[test]
    fn unpriceable_residents_names_the_gguf_case_and_hints_the_mlx_remedy() {
        let rows = [row("qwen3.6-35b-a3b", Some(24_000_000_000)), row("microsoft/phi-4-Q4_K_M", None)];
        let c = super::unpriceable_residents_status(&rows);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("microsoft/phi-4-Q4_K_M"), "names the unpriceable model: {}", c.message);
        assert!(
            !c.message.contains("qwen3.6-35b-a3b"),
            "a priced sibling is not swept into the warning: {}",
            c.message
        );
        let hint = c.hint.expect("a warn carries a remedy");
        assert!(hint.to_lowercase().contains("mlx"), "hint names the concrete remedy (an MLX build): {hint}");
    }

    #[test]
    fn unpriceable_residents_counts_every_unpriceable_model_not_just_the_first() {
        let rows = [row("a", None), row("b", Some(1)), row("c", None)];
        let c = super::unpriceable_residents_status(&rows);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("2 resident model"), "counts both unpriceable rows: {}", c.message);
        assert!(c.message.contains('a') && c.message.contains('c'));
    }

    // ─── role_profiles coherence (#1475 packet 1, #1547) ─────────────────
    // The pure `role_profiles_status` takes the config map + the registry's
    // defined profiles + the role library's known ids explicitly, so every arm
    // is testable with no config.json / registry / role library on disk. A
    // dangling binding (role -> undefined profile, or an unknown role id)
    // WARNs; an all-resolving map (and the empty map) Pass. Bindings use REAL
    // review-pipeline role ids (`review-judge`, `review-verify`,
    // `review-probe-high`, `review-probe-low`) — the bare `judge`/`verify`/
    // `probe-high` this suite used pre-#1547 are not real role ids and were
    // themselves an instance of the trap #1547 fixes (a doc/test example that
    // reads as live but no-ops).
    fn known(names: &[&str]) -> std::collections::BTreeMap<String, darkmux_types::Profile> {
        names
            .iter()
            .map(|n| (n.to_string(), darkmux_types::Profile::default()))
            .collect()
    }
    fn bindings(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs.iter().map(|(r, p)| (r.to_string(), p.to_string())).collect()
    }
    fn quarantined(names: &[&str]) -> std::collections::BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }
    fn roles(ids: &[&str]) -> std::collections::BTreeSet<String> {
        ids.iter().map(|n| n.to_string()).collect()
    }
    const REAL_ROLES: &[&str] = &["review-judge", "review-verify", "review-probe-high", "review-probe-low"];

    #[test]
    fn role_profiles_empty_map_passes() {
        let c = super::role_profiles_status(&bindings(&[]), &known(&["qwen35b"]), &quarantined(&[]), &roles(REAL_ROLES));
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("no role->profile bindings"));
    }

    #[test]
    fn role_profiles_all_defined_passes() {
        let map = bindings(&[
            ("review-judge", "qwen35b"),
            ("review-verify", "qwen35b"),
            ("review-probe-low", "qwen4b"),
        ]);
        let c = super::role_profiles_status(&map, &known(&["qwen35b", "qwen4b"]), &quarantined(&[]), &roles(REAL_ROLES));
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("3 role->profile bindings"), "got: {}", c.message);
        assert!(c.message.contains("all name a real role and a defined profile"), "got: {}", c.message);
    }

    #[test]
    fn role_profiles_dangling_binding_warns_and_names_the_pair() {
        let map = bindings(&[("review-judge", "qwen35b"), ("review-probe-high", "ghost27b")]);
        let c = super::role_profiles_status(&map, &known(&["qwen35b", "qwen4b"]), &quarantined(&[]), &roles(REAL_ROLES));
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("review-probe-high -> ghost27b"), "names the dangling pair: {}", c.message);
        assert!(c.message.contains("undefined profile"), "genuinely-absent target reads as undefined: {}", c.message);
        assert!(!c.message.contains("review-judge -> qwen35b"), "the resolving binding is not flagged: {}", c.message);
        let hint = c.hint.unwrap();
        assert!(hint.contains("config set role_profiles"), "hint names the fix: {hint}");
        assert!(hint.contains("does NOT silently fall back"), "hint states the loud-resolution contract: {hint}");
    }

    #[test]
    fn role_profiles_quarantined_binding_warns_with_quarantine_hint() {
        // (#1475) A binding to a QUARANTINED profile (present in profiles.json but
        // its entry failed to parse) must NOT read as "undefined — add it": the
        // profile IS there. Doctor names it quarantined and points at fixing the
        // entry, not adding a new profile.
        let map = bindings(&[("review-judge", "qwen35b"), ("review-verify", "broken")]);
        let c = super::role_profiles_status(
            &map,
            &known(&["qwen35b"]),
            &quarantined(&["broken"]),
            &roles(REAL_ROLES),
        );
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("review-verify -> broken"), "names the quarantined pair: {}", c.message);
        assert!(c.message.contains("quarantined profile"), "flavored quarantined, not undefined: {}", c.message);
        assert!(!c.message.contains("undefined profile"), "not the add-it wording: {}", c.message);
        let hint = c.hint.unwrap();
        assert!(hint.contains("fix the profile entry"), "hint says fix the entry: {hint}");
        assert!(hint.contains("profile-registry check"), "hint points at the registry check: {hint}");
        assert!(!hint.contains("add the profile"), "hint does NOT say add it: {hint}");
        assert!(hint.contains("does NOT silently fall back"), "hint keeps the loud-resolution contract: {hint}");
    }

    #[test]
    fn role_profiles_mixed_undefined_and_quarantined_names_both() {
        // Both kinds present: each gets its own message segment + hint.
        let map = bindings(&[("review-judge", "ghost27b"), ("review-verify", "broken")]);
        let c = super::role_profiles_status(
            &map,
            &known(&["qwen35b"]),
            &quarantined(&["broken"]),
            &roles(REAL_ROLES),
        );
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("review-judge -> ghost27b"), "names the undefined pair: {}", c.message);
        assert!(c.message.contains("review-verify -> broken"), "names the quarantined pair: {}", c.message);
        assert!(c.message.contains("undefined profile"), "undefined segment present: {}", c.message);
        assert!(c.message.contains("quarantined profile"), "quarantined segment present: {}", c.message);
        let hint = c.hint.unwrap();
        assert!(hint.contains("add the profile"), "undefined hint present: {hint}");
        assert!(hint.contains("fix the profile entry"), "quarantined hint present: {hint}");
    }

    /// (#1547) The trap this issue is named for: doctor's + config_cmd's own
    /// worked examples (and this very test file, pre-#1547) bound bare
    /// `judge`/`verify`/`probe-high` — none of which are real role ids (the
    /// real ones are `review-judge`/`review-verify`/`review-probe-high`) — and
    /// `role_profiles_status` reported Pass because it never checked the role
    /// half. This is the RED case: an unknown role id must WARN even when the
    /// profile side resolves cleanly.
    #[test]
    fn role_profiles_unknown_role_id_warns_even_with_a_defined_profile() {
        let map = bindings(&[("judge", "qwen35b")]);
        let c = super::role_profiles_status(&map, &known(&["qwen35b"]), &quarantined(&[]), &roles(REAL_ROLES));
        assert_eq!(c.status, Status::Warn, "an unknown role id must not Pass just because the profile resolves");
        assert!(c.message.contains("judge -> qwen35b"), "names the offending pair: {}", c.message);
        assert!(c.message.contains("unknown role id"), "flavored as an unknown role, not a profile problem: {}", c.message);
        let hint = c.hint.unwrap();
        assert!(hint.contains("darkmux role list"), "hint points at the real role list: {hint}");
    }

    #[test]
    fn role_profiles_unknown_role_and_undefined_profile_names_both_segments() {
        let map = bindings(&[("judge", "qwen35b"), ("review-probe-high", "ghost27b")]);
        let c = super::role_profiles_status(&map, &known(&["qwen35b"]), &quarantined(&[]), &roles(REAL_ROLES));
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("unknown role id"), "unknown-role segment present: {}", c.message);
        assert!(c.message.contains("undefined profile"), "undefined-profile segment present: {}", c.message);
        assert!(c.message.contains("judge -> qwen35b"), "names the unknown-role pair: {}", c.message);
        assert!(c.message.contains("review-probe-high -> ghost27b"), "names the undefined-profile pair: {}", c.message);
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

    /// (#1284 review round 2, consider 7 / #1550 cluster item 2) A USER-tier
    /// copy of a built-in authored against an OLDER schema is not silently
    /// accepted. This test originally pinned the SAME-MAJOR-lower-minor path
    /// (`doc_major == bin_major && doc_minor < bin_minor`) with a concrete
    /// hazard: a 1.0-era "review" copy had no typed `expand` block, so its
    /// probe stage interpreted to ZERO probe tasks. `expand` itself retired
    /// in schema 2.0 (a MAJOR bump — see `MISSION_CONFIG_SCHEMA`'s doc), and
    /// 2.0 is the new major's floor, so there is currently no real
    /// SAME-MAJOR-lower-minor schema to fixture (nothing parses below
    /// "2.0"). A user's genuinely stale "1.0" copy now takes the GENERIC
    /// major-mismatch path instead (`validate()`'s own schema_version
    /// check) — still a loud Warn, just the generic message rather than the
    /// specific "predates additive fields" one. This test now pins THAT
    /// behavior (the operator must still be warned); the specific
    /// same-major-lower-minor message becomes reachable — and worth
    /// re-testing directly — again once a real 2.1 exists.
    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_warns_when_user_tier_copy_is_on_an_older_major() {
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        // A stale pre-2.0 user override of the "review" built-in — same
        // scenario the original 1.0-era fixture modeled, now a MAJOR
        // mismatch rather than a same-major minor trail (see the doc above).
        std::fs::write(
            guard.path().join("mission-configs").join("review.json"),
            r#"{"id":"review","name":"PR Review (stale user copy)","schema_version":"1.0"}"#,
        )
        .unwrap();

        let check = check_mission_config_registry();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("schema_version \"1.0\" (major 1)"), "{}", check.message);
        // (#1684) Asserted against the CONSTANT rather than a hardcoded
        // "2.0" literal — the schema bumped to 2.1 in the same change that
        // added this comment's own "once a real 2.1 exists" callout below,
        // and a literal here would have gone stale exactly the way this
        // one did.
        // (#2004) The MAJOR is derived too. The previous version pinned the
        // string against the constant but wrote "(major 2)" as a literal —
        // which is the same staleness the comment above warns about, one
        // field to the right. It went red on the 3.0 bump.
        let bin_major = darkmux_crew::mission_config::MISSION_CONFIG_SCHEMA
            .split('.')
            .next()
            .unwrap();
        assert!(
            check.message.contains(&format!(
                "MISSION_CONFIG_SCHEMA \"{}\" (major {bin_major})",
                darkmux_crew::mission_config::MISSION_CONFIG_SCHEMA
            )),
            "{}",
            check.message
        );
        assert!(check.message.contains("major-version mismatch"), "{}", check.message);
    }

    /// (#1684) The same-major-lower-minor trail this file's own
    /// `check_mission_config_registry_warns_when_user_tier_copy_is_on_an_older_major`
    /// doc comment named as "worth re-testing directly again once a real
    /// 2.1 exists" — #1684's additive `panel` field is exactly that: the
    /// mission-config schema bumped `2.0` -> `2.1` in the SAME change that
    /// introduces this test, making a user-tier "2.0" copy of `review` a
    /// live, reachable same-major-minor-trail case for the first time
    /// since the 2.0 major bump. A pre-2.1 user copy is missing the
    /// `panel` block, so it silently stops being ACP-advertisable — the
    /// concrete hazard this finding exists to name (a same-major
    /// minor-trail finding is a loud `Status::Warn`, same tier as every
    /// other entry `check_mission_config_registry`'s `blocking` vec
    /// collects — see that function's own `if blocking.is_empty()` branch).
    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_blocks_a_user_tier_copy_trailing_the_current_minor() {
        // (#2004) The fixture is DERIVED from the constant: one minor behind
        // the binary, within the same major. A literal "2.0" here meant this
        // test silently changed which BRANCH it exercised when the schema
        // bumped to 3.0 — 2.0 stopped being "trailing by a minor" and became
        // "an older major", a different code path with a different message,
        // so the test failed on an assertion that was no longer even about
        // the case its name claims.
        //
        // At a `.0` schema there is no same-major lower minor, so the case is
        // genuinely unconstructable and this test asserts the neighbouring
        // truth instead: a document AT the current schema draws no drift
        // finding at all. It starts exercising the trailing-minor branch
        // again, without an edit, the moment a 3.1 exists.
        let (bin_major, bin_minor) = {
            let mut it = darkmux_crew::mission_config::MISSION_CONFIG_SCHEMA.split('.');
            (
                it.next().unwrap().parse::<u32>().unwrap(),
                it.next().unwrap_or("0").parse::<u32>().unwrap(),
            )
        };
        let doc_version = format!("{bin_major}.{}", bin_minor.saturating_sub(1));

        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        std::fs::write(
            guard.path().join("mission-configs").join("review.json"),
            format!(
                r#"{{"id":"review","name":"PR Review (pre-panel user copy)","schema_version":"{doc_version}"}}"#
            ),
        )
        .unwrap();

        let check = check_mission_config_registry();

        if bin_minor == 0 {
            assert_eq!(
                check.status,
                Status::Pass,
                "a document at the CURRENT schema must draw no drift finding: {}",
                check.message
            );
            return;
        }

        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(
            check.message.contains(&format!("declares schema {doc_version}")),
            "{}",
            check.message
        );
        assert!(
            // #1917 rescoped this to name the actual gap ("trailing by N
            // minors") rather than a fixed phrase, since the severity text
            // now varies with how far the document trails.
            check.message.contains("predate additive fields"),
            "must name the hazard, not just the version delta: {}",
            check.message
        );
    }

    /// Pull just one id's finding out of `check_mission_config_registry`'s
    /// combined message — findings are joined with `" | "` and each starts
    /// with `"<id>": ...` (see `blocking.join(" | ")`). Lets a test assert
    /// on ONE finding's text without the other finding's text being able to
    /// satisfy the assertion by accident.
    fn finding_for<'a>(message: &'a str, id: &str) -> &'a str {
        let needle = format!("\"{id}\":");
        let start = message
            .find(&needle)
            .unwrap_or_else(|| panic!("no finding for \"{id}\" in: {message}"));
        let rest = &message[start..];
        match rest.find(" | ") {
            Some(end) => &rest[..end],
            None => rest,
        }
    }

    /// #1917 — the remedy text must differ depending on whether `id` HAS a
    /// fallback tier. Same one-minor gap on two ids in the SAME check run:
    /// "review" is embedded (deleting the user copy truly falls back to
    /// something), "totally-custom-1917" is user-only (deleting it loses
    /// the document outright — the exact hazard #1917 reported live, on 15
    /// real configs including every `pr-*` GitHub verb, none of which have
    /// a built-in counterpart).
    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_remedy_differs_between_a_fallback_and_a_user_only_config() {
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        let (major, minor) =
            parse_major_minor(darkmux_crew::mission_config::MISSION_CONFIG_SCHEMA).expect("valid constant");
        // (#1919 review) `minor - 1` underflows and panics in debug at the
        // next MAJOR bump, when minor resets to 0. Saturate: at x.0 there is
        // no one-minor-trailing document to fixture, so the test has nothing
        // to say and skips rather than lying about a gap it cannot build.
        if minor == 0 {
            return;
        }
        let trailing = format!("{major}.{}", minor - 1);
        std::fs::write(
            guard.path().join("mission-configs").join("review.json"),
            format!(r#"{{"id":"review","name":"PR Review (trailing)","schema_version":"{trailing}"}}"#),
        )
        .unwrap();
        std::fs::write(
            guard.path().join("mission-configs").join("totally-custom-1917.json"),
            format!(
                r#"{{"id":"totally-custom-1917","name":"Operator's own verb","schema_version":"{trailing}"}}"#
            ),
        )
        .unwrap();

        let check = check_mission_config_registry();
        assert_eq!(check.status, Status::Warn, "{}", check.message);

        let review_finding = finding_for(&check.message, "review");
        let custom_finding = finding_for(&check.message, "totally-custom-1917");

        assert!(
            review_finding.contains("delete it"),
            "review has an embedded fallback — deleting it is a real, safe option: {review_finding}"
        );
        assert!(
            !custom_finding.contains("delete it"),
            "a user-only config has nothing to fall back to — must never suggest deleting it: {custom_finding}"
        );
        assert!(
            custom_finding.contains("no on-disk or embedded counterpart"),
            "must say what is actually true for a user-only document: {custom_finding}"
        );

        // Both fixtures trail by exactly one minor (2.2 vs the binary's
        // 2.3) — additive, per the schema's own versioning rule. Neither
        // finding should quote the fixed pre-1.4 "review"/`reads` example;
        // that hazard belongs to a document trailing far enough for it to
        // plausibly apply, not to a one-minor additive gap (#1917's second
        // half — a small gap must not read as data loss).
        for finding in [review_finding, custom_finding] {
            assert!(
                !finding.contains("pre-1.4"),
                "a one-minor gap must not quote the fixed pre-1.4 example: {finding}"
            );
            assert!(
                finding.contains("one minor"),
                "the severity text must name the actual (small) gap detected: {finding}"
            );
        }
    }

    /// (#1648) The MIRROR direction — a user-tier copy on a NEWER minor than
    /// the binary. Parses cleanly (the flatten `extras` swallows unknown
    /// fields), which is exactly the hazard: an additive field silently stops
    /// existing. For `reads` (#1619) that drops data AND ordering, letting a
    /// stage run early against empty input and finish GREEN WITH NO FINDINGS.
    /// A false-green review must never be reachable in silence.
    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_warns_when_user_tier_minor_leads_the_binary() {
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        // Same major, minor AHEAD of whatever this binary ships — derived
        // from the constant rather than hardcoded, so the test keeps meaning
        // the same thing after the next minor bump.
        let (major, minor) =
            parse_major_minor(darkmux_crew::mission_config::MISSION_CONFIG_SCHEMA).expect("valid constant");
        let ahead = format!("{major}.{}", minor + 1);
        std::fs::write(
            guard.path().join("mission-configs").join("review.json"),
            format!(
                r#"{{"id":"review","name":"PR Review (from a newer darkmux)","schema_version":"{ahead}"}}"#
            ),
        )
        .unwrap();

        let check = check_mission_config_registry();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(
            check.message.contains(&format!("declares schema {ahead}")),
            "the warning must name the document's own newer version: {}",
            check.message
        );
        assert!(
            check.message.contains("silently ignores"),
            "the warning must say the fields are IGNORED, not rejected — that is the hazard: {}",
            check.message
        );
    }

    /// (#1648) A copy on the SAME minor as the binary must not trip either
    /// direction. Without this, a fix for the leading case could trivially
    /// fire on every well-formed user copy and train the operator to ignore
    /// doctor.
    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_is_quiet_when_user_tier_minor_matches() {
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        std::fs::write(
            guard.path().join("mission-configs").join("review.json"),
            format!(
                r#"{{"id":"review","name":"PR Review (current)","schema_version":"{}"}}"#,
                darkmux_crew::mission_config::MISSION_CONFIG_SCHEMA
            ),
        )
        .unwrap();

        let check = check_mission_config_registry();
        assert!(
            !check.message.contains("declares schema"),
            "a current-schema user copy must not trip any drift warning: {}",
            check.message
        );
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

    // ─── check_mission_envelope_readability (#1881) ────────────────────

    #[serial_test::serial]
    #[test]
    fn mission_envelope_readability_passes_with_no_missions_dir_at_all() {
        let _guard = CrewRootGuard::new();
        let check = check_mission_envelope_readability();
        assert_eq!(check.status, Status::Pass);
        assert!(check.hint.is_none());
    }

    #[serial_test::serial]
    #[test]
    fn mission_envelope_readability_ignores_a_mission_with_no_envelope_written_yet() {
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("missions").join("m-no-envelope")).unwrap();
        let check = check_mission_envelope_readability();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
    }

    #[serial_test::serial]
    #[test]
    fn mission_envelope_readability_passes_on_a_well_formed_envelope() {
        let guard = CrewRootGuard::new();
        let mission_dir = guard.path().join("missions").join("m-good");
        std::fs::create_dir_all(&mission_dir).unwrap();
        std::fs::write(
            mission_dir.join("envelope.json"),
            r#"{"mission_id":"m-good","status":"clean","phases":[]}"#,
        )
        .unwrap();
        let check = check_mission_envelope_readability();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        // (#1881, QA-caught) `contains('1')` would also pass on "10", "11",
        // "21"… — pin the exact string so the count is actually verified.
        assert_eq!(check.message, "1 mission envelope(s) parsed cleanly");
    }

    /// (#1881 RED proof) A malformed `envelope.json` — no leniency of any
    /// kind can rescue this, so it is exactly the case `mission_run_status`
    /// resolves to `RunStatus::Unparseable`. This is the doctor-side half of
    /// the same fix: the operator must be told WHICH mission, not just see
    /// a silently-green dashboard row. Proven failing first by temporarily
    /// treating `Err` the way the pre-#1881 `.ok().flatten()` bug did (fold
    /// it into "nothing to report") — restored immediately below; see the
    /// git history on this test for the red run.
    #[serial_test::serial]
    #[test]
    fn mission_envelope_readability_warns_and_names_the_mission_on_a_malformed_envelope() {
        let guard = CrewRootGuard::new();
        let mission_dir = guard.path().join("missions").join("m-broken");
        std::fs::create_dir_all(&mission_dir).unwrap();
        std::fs::write(mission_dir.join("envelope.json"), "{not valid json at all").unwrap();

        let check = check_mission_envelope_readability();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("m-broken"), "{}", check.message);
        assert!(check.hint.is_some());
    }

    /// (#1881 second half) `MissionOutcomeStatus`'s `#[serde(other)]`
    /// leniency means an envelope with a `status` value this binary
    /// doesn't recognize NOW parses successfully (`Ok(Some(_))`, not the
    /// `Err` the previous test exercises) — but it is still exactly the
    /// schema drift `darkmux doctor` exists to name, so it must still warn.
    #[serial_test::serial]
    #[test]
    fn mission_envelope_readability_warns_on_an_envelope_that_parses_with_an_unrecognized_status() {
        let guard = CrewRootGuard::new();
        let mission_dir = guard.path().join("missions").join("m-future-status");
        std::fs::create_dir_all(&mission_dir).unwrap();
        std::fs::write(
            mission_dir.join("envelope.json"),
            r#"{"mission_id":"m-future-status","status":"throttled","phases":[]}"#,
        )
        .unwrap();

        // Confirm the fixture really does parse (not an Err) — this test's
        // whole point is the leniency path, not the malformed-JSON path
        // the sibling test above already covers.
        let loaded = darkmux_crew::lifecycle::load_envelope("m-future-status");
        assert!(matches!(loaded, Ok(Some(_))), "fixture must parse leniently, got {loaded:?}");

        let check = check_mission_envelope_readability();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("m-future-status"), "{}", check.message);
    }

    /// (#1881, QA-caught) An envelope whose `status` is fully known (renders
    /// correctly on the dashboard) but whose `outcome` detail carries a
    /// `#[serde(other)]`-caught `state` this binary doesn't recognize. This
    /// used to be counted as "fully clean" — `RunOutcome::is_unknown` has no
    /// production caller without this arm — even though it is real,
    /// narrower schema drift the check exists to name.
    #[serial_test::serial]
    #[test]
    fn mission_envelope_readability_warns_on_a_known_status_with_an_unrecognized_outcome_detail() {
        let guard = CrewRootGuard::new();
        let mission_dir = guard.path().join("missions").join("m-outcome-drift");
        std::fs::create_dir_all(&mission_dir).unwrap();
        std::fs::write(
            mission_dir.join("envelope.json"),
            r#"{"mission_id":"m-outcome-drift","status":"degraded","outcome":{"state":"throttled"},"phases":[]}"#,
        )
        .unwrap();

        // Confirm the fixture's status really is known (this test is about
        // the OUTCOME leniency specifically, not the status one).
        let loaded = darkmux_crew::lifecycle::load_envelope("m-outcome-drift").unwrap().unwrap();
        assert_eq!(loaded.status, darkmux_crew::envelope::MissionOutcomeStatus::Degraded, "fixture's status must be known");
        assert!(loaded.outcome.as_ref().unwrap().is_unknown(), "fixture's outcome must be unrecognized");

        let check = check_mission_envelope_readability();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("m-outcome-drift"), "{}", check.message);
    }

    #[serial_test::serial]
    #[test]
    fn mission_envelope_readability_names_every_unreadable_mission_alongside_the_readable_ones() {
        let guard = CrewRootGuard::new();
        let good_dir = guard.path().join("missions").join("m-good2");
        std::fs::create_dir_all(&good_dir).unwrap();
        std::fs::write(
            good_dir.join("envelope.json"),
            r#"{"mission_id":"m-good2","status":"clean","phases":[]}"#,
        )
        .unwrap();
        let bad_dir = guard.path().join("missions").join("m-broken2");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("envelope.json"), "not json").unwrap();

        let check = check_mission_envelope_readability();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("m-broken2"), "{}", check.message);
        assert!(
            !check.message.contains("m-good2"),
            "a readable envelope must not be named among the unreadable ones: {}",
            check.message
        );
    }

    // ─── (#1959) check_rules_registry / build_rules_check ───────────────

    // (#2206) The builtin count is READ from the registry, never pinned as a
    // literal: the old form (`contains('3')`) broke the moment a fourth rule
    // registered, and its sibling below passed only because "5 rule(s) loaded
    // (4 built-in" happens to contain the digit it looked for. What these
    // tests own is the message SHAPE and its internal consistency — total ==
    // built-in when there is no user tier — not how many rules ship.
    #[test]
    fn rules_check_passes_with_only_the_builtins_and_no_user_tier() {
        let n = darkmux_crew::rules::load_all(None).0.len();
        assert!(n >= 4, "expected the builtin rules to be present, got {n}");
        let check = build_rules_check(None);
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        let expected = format!("{n} rule(s) loaded ({n} built-in, no user tier)");
        assert_eq!(check.message, expected);
    }

    #[test]
    fn rules_check_reports_user_tier_provenance() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("custom-rule.json"),
            serde_json::json!({"id": "custom-rule", "kind": "read", "applies_to": ["**/*.py"]})
                .to_string(),
        )
        .unwrap();

        let n = darkmux_crew::rules::load_all(None).0.len();
        let check = build_rules_check(Some(tmp.path()));
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        let expected_prefix = format!("{} rule(s) loaded ({n} built-in, 1 user-tier file(s) at ", n + 1);
        assert!(check.message.starts_with(&expected_prefix), "{}", check.message);
        assert!(check.message.contains("1 user-tier file"), "{}", check.message);
    }

    #[test]
    fn rules_check_warns_on_a_malformed_user_file_naming_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("broken.json"), "{ not json").unwrap();

        let check = build_rules_check(Some(tmp.path()));
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("broken.json"), "{}", check.message);
    }

    #[test]
    fn rules_check_warns_on_empty_applies_to_and_site_with_no_prefilter() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("thin-site.json"),
            serde_json::json!({"id": "thin-site", "kind": "site"}).to_string(),
        )
        .unwrap();

        let check = build_rules_check(Some(tmp.path()));
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("thin-site"), "{}", check.message);
        assert!(check.message.contains("applies_to"), "{}", check.message);
        assert!(check.message.contains("prefilter"), "{}", check.message);
    }

    /// (#2310 P4c) A `confirm: "search"` rule with no `search` recipe and a
    /// `confirm: "question"` rule with no `compare` question both surface
    /// through `darkmux doctor` — over the WHOLE registry, not just a
    /// manifest's resolved subset (same reasoning `rules_check_warns_on_
    /// empty_applies_to_and_site_with_no_prefilter` above already
    /// establishes for the pre-existing thin checks).
    ///
    /// (#2310 swarm G, S1-9) The fixture is deliberately named `thin-alpha`,
    /// NOT `thin-search`: the rule id lands verbatim in every warning that
    /// names the rule, so a `contains("search")` assertion against a
    /// `thin-search` fixture was satisfied by the id itself and could not
    /// tell the search-recipe check firing from any other warning about the
    /// same rule. With a name sharing no substring with the check, the
    /// assertion has to be carried by the warning's OWN words — `recipe`,
    /// which appears only in the search-confirm warning. Same fix, same
    /// reason, as the `darkmux-crew` twin
    /// (`rules::tests::search_confirm_with_no_recipe_warns_and_question_
    /// confirm_with_no_compare_warns`).
    #[test]
    fn rules_check_warns_on_a_search_rule_with_no_recipe() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("thin-alpha.json"),
            serde_json::json!({
                "id": "thin-alpha", "kind": "site", "confirm": "search",
                "applies_to": ["**/*.rs"], "prefilter": ["x"]
            })
            .to_string(),
        )
        .unwrap();

        let check = build_rules_check(Some(tmp.path()));
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("thin-alpha"), "{}", check.message);
        assert!(check.message.contains("recipe"), "{}", check.message);
    }

    /// (#2310 P4c) An invalid `confirm` value (not `mod`/`search`/
    /// `question`) never reaches the thin-rule loop at all — it fails
    /// `Rule::Deserialize` first, and `load_all` folds that parse failure
    /// into the SAME warnings vector this check reports, so it still
    /// surfaces here, named, without a second code path.
    #[test]
    fn rules_check_warns_on_an_unrecognized_confirm_value() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("bad-confirm.json"),
            serde_json::json!({"id": "bad-confirm", "kind": "site", "confirm": "shrug"}).to_string(),
        )
        .unwrap();

        let check = build_rules_check(Some(tmp.path()));
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("bad-confirm"), "{}", check.message);
        assert!(check.message.contains("failed to parse"), "{}", check.message);
    }

    // ─── (#2399) check_quarantined_mirrors ───

    #[test]
    fn quarantined_mirrors_reports_none_on_a_clean_workspaces_root() {
        let home = tempfile::TempDir::new().unwrap();
        let workspaces = home.path().join("workspaces");
        std::fs::create_dir_all(workspaces.join("w1").join("mirror").join("app.git")).unwrap();
        let check = quarantined_mirrors_check_at(&workspaces);
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.starts_with("none"), "{}", check.message);
        assert!(check.hint.is_none(), "{:?}", check.hint);
    }

    #[test]
    fn quarantined_mirrors_lists_each_corrupt_sibling_with_its_size() {
        let home = tempfile::TempDir::new().unwrap();
        let workspaces = home.path().join("workspaces");
        let quarantined = workspaces.join("review-v2-live").join("mirror").join("app.git.corrupt-1788610000");
        std::fs::create_dir_all(quarantined.join("objects")).unwrap();
        std::fs::write(quarantined.join("objects").join("blob"), vec![7u8; 4096]).unwrap();
        // A healthy sibling in the same mirror dir must NOT be listed.
        std::fs::create_dir_all(workspaces.join("review-v2-live").join("mirror").join("app.git")).unwrap();

        let check = quarantined_mirrors_check_at(&workspaces);
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("1 quarantined mirror(s)"), "{}", check.message);
        assert!(check.message.contains("app.git.corrupt-1788610000"), "{}", check.message);
        assert!(check.message.contains("4.0 KB"), "the size is reported: {}", check.message);
        assert!(!check.message.contains("mirror/app.git ("), "a healthy mirror is not listed: {}", check.message);
        assert!(check.hint.as_deref().is_some_and(|h| h.contains("#2399")), "{:?}", check.hint);
    }

}
