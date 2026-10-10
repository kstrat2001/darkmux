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
// (#2093) The flow-record hook sink's rows — see the module doc.
mod checks_hooks;
use checks_hooks::check_hooks;
mod fleet_submission;
pub use fleet_submission::{
    fleet_submission_checks, BusyFacts, BusySettings, FleetSubmissionFacts, ProviderReport, TrustView,
};
use darkmux_eureka as eureka;
use darkmux_hardware as hardware;
use darkmux_heuristics as heuristics;
use darkmux_profiles::lms;
use darkmux_profiles::profiles;
use std::env;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;

/// Ordered by severity (`Pass < Warn < Fail`), so the worst of several is
/// their `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
/// same shape as `probe_unmanaged_endpoints`, but taking an input.
#[derive(Debug, Clone)]
pub struct EmbeddedSkill {
    pub name: String,
    pub content: String,
}

impl DoctorReport {
    pub fn worst_status(&self) -> Status {
        self.checks.iter().map(|c| c.status).max().unwrap_or(Status::Pass)
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
    let mut checks = vec![
        check_build_info(),
        check_profile_registry(),
        // (#2707) Read-only: counts what darkmux left in the temp root.
        check_temp_residue(),
        check_lms_binary(),
        check_docker_runtime(),
        check_models_loaded(),
        check_profile_loaded_match(),
        check_darkmux_version_vs_latest_release(),
        // (#2765) The CONFIGURED address, before the reachability probe that
        // uses it — so a reader sees where darkmux is looking, then whether
        // anything answered there. The pair is what makes a port mismatch
        // readable in one command instead of by probing ports by hand.
        check_serve_address(),
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
        machine_uid_check(hardware::machine_uid()),
        check_openai_base_url_conflict(),
        check_redis_config(),
        check_gh_allowlist(),
        check_removed_radio_router_staffing(),
        check_renamed_budget_settings(),
        check_role_skill_references(),
        check_inactivity_timeout(),
        check_step_command_timeout(),
        check_dispatch_free_concurrency(),
        check_turn_delay(),
        check_reasoning_checkpoint_interval(),
        check_max_stall_recoveries(),
        check_host_sampler_interval(),
        check_live_channel(),
        check_host_sampler(),
        check_liveness_retention(),
        check_generation_checkpoint_interval(),
    ];
    // (#2947) `None` when a thermal threshold is bad config; that value's
    // own enum-settings row reports it.
    checks.extend(check_thermal_governor());
    checks.extend(vec![
        check_host_probe(),
        check_quarantined_mirrors(),
        checks_power::check_power_posture(),
        check_unmanaged_endpoint_credentials(),
        check_endpoints(),
        check_env_masks_config(),
        check_binary_split_brain(),
        check_audit_integrity(),
        check_audit_write_drops(),
        check_unknown_flow_actions(),
        check_state_file_permissions(),
        check_serve_daemon_token(),
        check_serve_reads(),
        check_utility_model_binding(),
        check_utility_model_in_profiles(),
        check_unpriceable_residents(),
        check_unreachable_darkmux_residents(),
        check_role_profiles(),
        check_radio_peer_seat(),
        check_fleet_routes(),
        check_role_tool_vocab_typos(),
        check_ignored_project_darkmux(),
        check_mission_envelope_readability(),
    ]);
    let checks = [checks, check_enum_settings(), check_user_file_keys(), check_hooks(), eureka_checks()].concat();
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
/// (the namespace contract). An installed `darkmux-*` skill whose content
/// differs from the embedded copy (stale: an older darkmux installed it, or it
/// was edited) WARNs. An embedded skill that is not installed stays
/// informational: a minimal install is a legitimate operator choice, not
/// drift. An installed skill the binary no longer bundles is not inspected
/// (5.0 dropped the pruning of retired skills, so a leftover is the
/// operator's to delete).
///
/// Pure evaluator: `targets` (the install directories) and `embedded` (the
/// reference set) are supplied by the caller (`main.rs`, the root crate that
/// owns the `include_str!` embed), because this crate cannot depend on the root
/// binary crate where the skills live.
pub fn check_installed_skills_freshness(targets: &[PathBuf], embedded: &[EmbeddedSkill]) -> Check {
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

    stale.sort();
    not_installed.sort();

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
    let message = format!("darkmux-* skills: {}", detail.join("; "));

    // A stale skill is only SOMETIMES fixed by `darkmux init` (#1927): the
    // installer can't tell "stale" (an older bundled version, safe to
    // refresh) apart from "edited" from content alone, so the hint below
    // describes what `init` actually does rather than promising a refresh:
    // describe, don't adjudicate (#1839).
    if stale.is_empty() {
        Check {
            name: SKILLS_FRESHNESS_CHECK_NAME.into(),
            status: Status::Pass,
            message,
            hint: None,
        }
    } else {
        Check {
            name: SKILLS_FRESHNESS_CHECK_NAME.into(),
            status: Status::Warn,
            message,
            hint: Some(
                "content differs from this binary's bundled copy — an older darkmux installed it, or \
                 it was edited locally. `darkmux init` refreshes a copy it can prove darkmux itself \
                 wrote (a provenance stamp recorded at install time) and leaves every other one alone, \
                 so a skill installed before darkmux started recording provenance stays listed here \
                 even after `init` runs. `darkmux init --force` takes this binary's version of ALL of \
                 them, discarding any local edit, and records provenance so later runs refresh \
                 silently; to take just one, delete that skill's directory and re-run `init`"
                    .into(),
            ),
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

/// (4.0) State in the working directory that darkmux no longer reads. Earlier
/// releases adopted a `./.darkmux/` as the darkmux root (splitting flows, lab
/// runs and profiles from missions) and a `./.darkmux.json` as the profile
/// registry; 4.0 ignores both. A repo's `.darkmux/` is normal: it holds the
/// per-repo `lessons.db` and `conventions.json`, which darkmux still reads,
/// so only what else is in it is stranded. Warn, naming each stranded entry.
fn check_ignored_project_darkmux() -> Check {
    ignored_project_status(darkmux_types::paths::ignored_project_state())
}

/// Pure decision for [`check_ignored_project_darkmux`]. The `DARKMUX_HOME`
/// relocation is offered only when the directory holds a `config.json` or
/// `profiles.json` to relocate; a leftover registry file is moved into the root.
fn ignored_project_status(state: Option<darkmux_types::paths::IgnoredProjectState>) -> Check {
    let name = "project-local .darkmux".to_string();
    let Some(state) = state else {
        return Check { name, status: Status::Pass, message: "nothing stranded in the working directory".into(), hint: None };
    };
    let listed = state.stranded.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ");
    let hint = match &state.adoptable_dir {
        Some(dir) => format!(
            "to use that directory as the root, run darkmux with DARKMUX_HOME={}; otherwise move what you need to ~/.darkmux (a registry is ~/.darkmux/profiles.json)",
            dir.display()
        ),
        None => "move what you need to ~/.darkmux (a registry is ~/.darkmux/profiles.json) and delete the rest".into(),
    };
    Check {
        name,
        status: Status::Warn,
        message: format!("stranded, darkmux no longer reads a project-local root or registry: {listed}"),
        hint: Some(hint),
    }
}

const RETIRED_ENV_CHECK_NAME: &str = "retired env vars";

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
/// Pure (no I/O), split out of `check_audit_integrity` so the split — a
/// chain break (including a file not in the byte-hash format), a torn tail,
/// or a fully verified walk — is unit-testable without touching the filesystem or
/// `DARKMUX_AUDIT_DIR`.
///
/// The statuses map to doctor's own exit code (`main.rs`: `Fail` → 1,
/// everything else → 0): a break is the only thing that flips it.
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
        let unrecognized = first.break_at_line == Some(1) && first.records_checked == 0;
        let hint = if unrecognized {
            "A file from before 2.6.0 (or in an unrecognized format), not verified: nothing in it was checked, and this is not evidence of editing. Archive it (move it aside) so a fresh chain starts. Run `darkmux flow integrity-check` for the full per-file breakdown."
        } else {
            "Audit log has been edited or a write was interleaved. Run `darkmux flow integrity-check` for the full per-file breakdown. If tampering is suspected, the chain break locates the affected line; records before that line link consistently, which is evidence they are unmodified — not proof."
        };
        return Check {
            name: "audit integrity".into(),
            status: Status::Fail,
            message: summary,
            hint: Some(hint.into()),
        };
    }

    // A crash mid-append left an incomplete last line; a later append set it
    // aside in a sidecar. The remaining chain verifies, so this is a caveat
    // (the cut bytes are not part of the chain), not a break.
    let torn: Vec<&str> = reports
        .iter()
        .flat_map(|r| r.torn_tails.iter().map(String::as_str))
        .collect();
    if !torn.is_empty() {
        let restarted = reports.iter().filter(|r| r.chain_restarted).count();
        let restart_note = if restarted == 0 {
            String::new()
        } else {
            format!(" ({restarted} restarted its chain: the verified chain covers only what was written after)")
        };
        return Check {
            name: "audit integrity".into(),
            status: Status::Warn,
            message: format!(
                "{} torn audit tail(s) set aside after an interrupted write{restart_note}: {}",
                torn.len(),
                torn.join(", ")
            ),
            hint: Some("A write was interrupted mid-line; the incomplete bytes were moved to the named sidecar file and the chain continues from the last complete line. Inspect or delete the sidecar once reviewed. The chain check does not detect records removed from the end of a day file.".into()),
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

/// How many of the newest day files [`check_unknown_flow_actions`] reads.
const UNKNOWN_ACTION_SCAN_DAYS: usize = 7;

/// Records in the recent flow archive whose action this darkmux does not
/// know. They still read (lenient on read, contract 5), and they are never
/// taken as vocabulary: a newer darkmux wrote them, or something outside
/// darkmux did. Named here so neither case goes unseen.
fn check_unknown_flow_actions() -> Check {
    let dir = darkmux_types::config_access::flows_dir();
    unknown_flow_actions_check(&darkmux_flow::reader::unknown_actions_in(&dir, UNKNOWN_ACTION_SCAN_DAYS))
}

fn unknown_flow_actions_check(tally: &darkmux_flow::reader::UnknownActions) -> Check {
    let name = "flow action vocabulary".to_string();
    if tally.total() == 0 {
        return Check {
            name,
            status: Status::Pass,
            message: format!("every action in the last {UNKNOWN_ACTION_SCAN_DAYS} day file(s) is one this darkmux knows"),
            hint: None,
        };
    }
    let names: Vec<String> = tally.by_name().iter().map(|(a, n)| format!("{a} ({n})")).collect();
    Check {
        name,
        status: Status::Warn,
        message: format!(
            "{} record(s) in the last {UNKNOWN_ACTION_SCAN_DAYS} day file(s) carry an action this darkmux does not know: {}",
            tally.total(),
            names.join(", ")
        ),
        hint: Some(
            "They read as-is and nothing acts on them. A newer darkmux on this machine or a peer \
             writes actions this build does not know; upgrading this binary names them. A record \
             from a 3.x archive, or of an action a release retired, is unknown here for good: \
             nothing maps an old spelling."
                .into(),
        ),
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
                "{n} audit write(s) FAILED today — the hash chain is INCOMPLETE for those records (the records that were written still link)"
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

/// Name of the state-file-permissions check (#2452).
const STATE_FILE_PERMS_CHECK_NAME: &str = "state file permissions";

/// Total files this check will `stat` across every store it walks, per
/// `darkmux doctor` invocation. `findings/**` and `mods/**` can hold years
/// of an operator's history — stat-ing all of it on every run would make
/// `doctor` slow exactly where it should stay instant. Once the budget is
/// spent the scan stops and the check SAYS SO, rather than silently
/// reporting a partial sweep as a clean one.
///
/// Measured (#2452 review, warm page cache, M5 Max): 772 files → ~17 ms;
/// a 4,921-file tree clamped by this budget → ~54 ms, against a ~2.5 s
/// `doctor` baseline. ~2% — not material, so the budget stays where it is
/// rather than being tightened or made lazy.
#[cfg(unix)]
const STATE_FILE_SCAN_BUDGET: usize = 2_000;

/// One darkmux-owned state location this check inspects — a human `label`
/// (the ONLY thing this check ever prints about an offending file, see
/// [`build_state_file_permissions_check`]), the root (a single file, e.g.
/// `fleet.json`, or a directory), and whether darkmux nests content in
/// subdirectories under that root (`findings/**`, `mods/**`) versus keeping
/// it flat (the hooks outbox, the flow day-files).
#[cfg(unix)]
struct ScanRoot {
    label: &'static str,
    path: std::path::PathBuf,
    recursive: bool,
}

/// What one [`ScanRoot`]'s walk found. Counts and modes only — deliberately
/// NO paths (see [`build_state_file_permissions_check`] for why a file name
/// must not reach this check's output).
#[cfg(unix)]
#[derive(Default)]
struct RootTally {
    /// Regular files actually stat'd.
    checked: usize,
    /// Of those, how many were group- or world-readable.
    violations: usize,
    /// The distinct offending modes, for the report. A mode is darkmux's
    /// own fact about the file, never operator content.
    modes: std::collections::BTreeSet<u32>,
    /// Symlinks encountered and deliberately NOT followed (see
    /// [`check_one_state_file`]). Reported so the blind spot is disclosed
    /// rather than silent.
    symlinks_skipped: usize,
}

/// Stat one file and record a violation when the group or other READ bit is
/// set.
///
/// A symlink is COUNTED as skipped and never followed. Two reasons, and the
/// count exists because neither of them makes the file uninteresting:
/// following one would let a link planted inside the store aim this check —
/// and the `chmod` remedy it prints — at an arbitrary path outside it, the
/// same confused-deputy shape `brief_refs`'s own `attachments/` symlink
/// refusal exists to stop (#2295); and a symlink's own mode is not the mode
/// that governs the data. So the target's exposure is real but unreported,
/// which is exactly why the count is surfaced instead of dropped.
#[cfg(unix)]
fn check_one_state_file(path: &std::path::Path, tally: &mut RootTally) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(sym_md) = std::fs::symlink_metadata(path) else { return };
    if sym_md.file_type().is_symlink() {
        tally.symlinks_skipped += 1;
        return;
    }
    if !sym_md.file_type().is_file() {
        return;
    }
    tally.checked += 1;
    let mode = sym_md.permissions().mode() & 0o777;
    // 0o044 = the group-read and other-read bits. #2259/#2451 create these
    // files at 0o600, so ANY of these bits is drift from what darkmux
    // itself would have written: 0o640 (group-read) and 0o604 (other-read)
    // are both caught, pinned by `state_file_perms_mask_catches_every_read_bit`.
    // Deliberately NOT 0o077: "group- or world-READABLE" is the literal
    // criterion #2452 names. A file at 0o620 — group-writable but not
    // readable — is therefore NOT reported here; see that test's own note.
    if mode & 0o044 != 0 {
        tally.violations += 1;
        tally.modes.insert(mode);
    }
}

/// Walk one [`ScanRoot`] into `tally`, stat-ing at most `allowance` files.
/// Returns `true` when that allowance was exhausted mid-walk, so the caller
/// can say the root's sweep was partial. Iterative (an explicit stack, not
/// recursion) so a very deep mission/finding tree can't blow the stack.
///
/// A root that is ITSELF a symlink is skipped whole, for the same reason a
/// symlinked entry is — and counted the same way. Before #2452's review this
/// split incoherently: `Path::is_file()` follows links, so a symlinked
/// single-file root (`fleet.json` → elsewhere) fell into the file arm and was
/// then dropped by `check_one_state_file` without a trace, while a symlinked
/// DIRECTORY root fell into `read_dir`, which follows too, and was walked in
/// full.
#[cfg(unix)]
fn scan_state_root(root: &ScanRoot, tally: &mut RootTally, allowance: usize) -> bool {
    let Ok(root_md) = std::fs::symlink_metadata(&root.path) else { return false };
    if root_md.file_type().is_symlink() {
        tally.symlinks_skipped += 1;
        return false;
    }
    if root_md.file_type().is_file() {
        if tally.checked >= allowance {
            return true;
        }
        check_one_state_file(&root.path, tally);
        return false;
    }
    let mut stack = vec![root.path.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if tally.checked >= allowance {
                return true;
            }
            let Ok(file_type) = entry.file_type() else { continue };
            if file_type.is_symlink() {
                tally.symlinks_skipped += 1;
                continue;
            }
            if file_type.is_dir() {
                if root.recursive {
                    stack.push(entry.path());
                }
                continue;
            }
            if file_type.is_file() {
                check_one_state_file(&entry.path(), tally);
            }
        }
    }
    false
}

/// Render one root's violations for the message — its LABEL, the count, and
/// the distinct modes. Never a file name; see
/// [`build_state_file_permissions_check`].
#[cfg(unix)]
fn describe_root_violations(label: &str, tally: &RootTally) -> String {
    let modes: Vec<String> = tally.modes.iter().map(|m| format!("{m:o}")).collect();
    let word = if modes.len() == 1 { "mode" } else { "modes" };
    format!("{label} ({}, {word} {})", tally.violations, modes.join(", "))
}

/// Pure builder — every root is caller-supplied, so this is testable
/// against synthetic tempdir fixtures with no `config_access` involved at
/// all (mirrors `build_hooks_check`'s own split). [`check_state_file_permissions`]
/// is the thin production wrapper that supplies the REAL darkmux-owned
/// roots, resolved through `config_access`.
///
/// **This check never prints a file name, deliberately (#2452 review).** It
/// would be the more actionable report if it did, and the first draft did.
/// But `darkmux doctor`'s stdout is not a private surface: `/panel/doctor`
/// runs this verb and the viewer's console lens renders its output verbatim
/// in a `<pre>`, and under the documented `tailscale serve` phone-dashboard
/// pattern every tailnet peer reaches that daemon as loopback — the one case
/// bearer auth is exempt from (see `panel::PANEL_HEADER`'s own doc). The
/// names in these stores are operator content, not darkmux's: a mod
/// attachment keeps its source basename verbatim (#2457 was filed because
/// that is how a `prod-credentials.diff` gets there), a hooks outbox file is
/// keyed by its destination host and port, and a crawl mission's plan files
/// are named for the operator's own rules. A check written to report a
/// disclosure risk must not become one, so the message carries the ROOT
/// LABEL, a count and the modes; the hint carries the root PATHS — darkmux's
/// own directory names, which `check_hooks` and the workspaces check already
/// print — plus the two commands that enumerate and fix the files locally.
/// The operator can still act; the names stay on their machine.
///
/// (#1839) Describes, never adjudicates: the message states counts and
/// modes; the hint states the remedy. Neither ever characterizes the
/// operator's exposure ("safe", "at risk", "for compliance") — see
/// `serve_token_status`'s own doc for the same rule applied to the
/// serve-token check.
#[cfg(unix)]
fn build_state_file_permissions_check(roots: &[ScanRoot], budget: usize) -> Check {
    let mut checked = 0usize;
    let mut violations = 0usize;
    let mut symlinks_skipped = 0usize;
    let mut per_root: Vec<(&'static str, RootTally)> = Vec::new();
    let mut truncated: Vec<&str> = Vec::new();

    // A SHARED budget spent in declaration order starves whatever comes
    // last: measured on a 4,921-file tree, `findings` alone consumed all
    // 2,000 stats and `mods` — the store #2457 is actually about — was
    // never reached, on every run, deterministically. So each root gets an
    // equal share and any share it doesn't spend rolls forward. The total
    // stayed bounded by `budget` either way; what changes is that no root
    // is permanently invisible behind a larger sibling.
    let n = roots.len().max(1);
    let share = budget / n;
    let mut carry = budget % n;
    for root in roots {
        let allowance = share + carry;
        let mut tally = RootTally::default();
        if scan_state_root(root, &mut tally, allowance) {
            truncated.push(root.label);
        }
        carry = allowance.saturating_sub(tally.checked);
        checked += tally.checked;
        violations += tally.violations;
        symlinks_skipped += tally.symlinks_skipped;
        if tally.violations > 0 {
            per_root.push((root.label, tally));
        }
    }

    let mut suffix = String::new();
    if !truncated.is_empty() {
        suffix.push_str(&format!(
            " — the {budget}-file scan budget ran out in: {}; those store(s) are not exhaustive",
            truncated.join(", ")
        ));
    }
    if symlinks_skipped > 0 {
        suffix.push_str(&format!(
            "{} {symlinks_skipped} symlink(s) skipped — their targets are not inspected",
            if suffix.is_empty() { " —" } else { ";" }
        ));
    }

    if violations == 0 {
        let status = if truncated.is_empty() {
            Status::Pass
        } else {
            Status::Warn
        };
        return Check {
            name: STATE_FILE_PERMS_CHECK_NAME.into(),
            status,
            message: format!(
                "{checked} darkmux state file(s) checked; none are group- or world-readable{suffix}"
            ),
            hint: None,
        };
    }

    let breakdown: Vec<String> =
        per_root.iter().map(|(label, t)| describe_root_violations(label, t)).collect();
    let message = format!(
        "{violations} of {checked} darkmux state file(s) are group- or world-readable — {}{suffix}",
        breakdown.join(", ")
    );

    // Only the roots that actually have violations, so the commands the
    // operator pastes do exactly what the message just said.
    let offending_paths: Vec<String> = roots
        .iter()
        .filter(|r| per_root.iter().any(|(label, _)| *label == r.label))
        .map(|r| format!("\"{}\"", r.path.display()))
        .collect();
    let targets = offending_paths.join(" ");
    let hint = Some(format!(
        "darkmux sets the mode only when IT creates a file — one written by an older binary, or \
         loosened by hand afterward, keeps whatever mode it already had. File NAMES are omitted \
         from this report on purpose: `darkmux doctor` output is republished verbatim by the \
         viewer's console lens, and the names in these stores are yours, not darkmux's. List them \
         on this machine, then restrict them:\n  \
         find {targets} -type f \\( -perm -g+r -o -perm -o+r \\) -print\n  \
         find {targets} -type f \\( -perm -g+r -o -perm -o+r \\) -exec chmod go-rwx {{}} +"
    ));

    Check { name: STATE_FILE_PERMS_CHECK_NAME.into(), status: Status::Warn, message, hint }
}

/// `state file permissions` (#2452): #2259/#2451 made darkmux create its
/// own state files owner-only (`0o600`), but `.mode()` only applies AT
/// CREATION — a file already on disk from an older binary, or an operator
/// who loosened one on purpose, keeps whatever mode it had. Silently
/// leaving that alone is only defensible if something reports it; this is
/// that report.
///
/// Every root is resolved through `darkmux_types::config_access` (and
/// `darkmux_crew::loader::missions_dir`, which resolves through the same
/// `paths::resolve` ladder) — never `DarkmuxConfig::load_resolved()`
/// directly and never `dirs::home_dir()` — so this check inherits the SAME
/// env>config>default precedence, and the SAME #811/#994 test-build
/// isolation, every sibling accessor already has. `check_hooks`'s own doc
/// names the failure this avoids: a check with its own copy of the config
/// ladder reads the developer's REAL `~/.darkmux` from inside a unit test.
///
/// Two of these six roots have NO test-build isolation of their own, and
/// that is the reason `run_returns_static_plus_eureka_checks` and
/// `platform_check_always_present` now pin `DARKMUX_HOME` to a tempdir:
/// `fleet_file()` is unguarded deliberately (see `fleet_file_default`'s own
/// doc — the guard broke a `HOME`-relocating CI test) and
/// `darkmux_crew::loader::missions_dir()` resolves through
/// `user_state_root()`, whose crate exposes an EMPTY `test-support` feature.
/// Both resolve to the developer's real `~/.darkmux` in an un-isolated test
/// build, verified by probe. Every other check in `run()` was already
/// reading that tree; this one would have WALKED it.
#[cfg(unix)]
fn check_state_file_permissions() -> Check {
    let roots = vec![
        ScanRoot { label: "hooks outbox", path: darkmux_types::config_access::hooks_outbox_dir(), recursive: false },
        ScanRoot { label: "fleet roster", path: darkmux_types::config_access::fleet_file(), recursive: false },
        ScanRoot { label: "mission/phase state", path: darkmux_crew::loader::missions_dir(), recursive: true },
        ScanRoot { label: "findings", path: darkmux_types::config_access::findings_dir(), recursive: true },
        ScanRoot { label: "mods", path: darkmux_types::config_access::mods_dir(), recursive: true },
        ScanRoot { label: "flow records", path: darkmux_types::config_access::flows_dir(), recursive: false },
    ];
    build_state_file_permissions_check(&roots, STATE_FILE_SCAN_BUDGET)
}

/// Windows file ACLs are a separate story (same posture as
/// `tests/state_files_owner_only_mode.rs`'s own `#![cfg(unix)]` gate) — no
/// POSIX mode bits to read, so there is nothing this check can say.
#[cfg(not(unix))]
fn check_state_file_permissions() -> Check {
    not_applicable(STATE_FILE_PERMS_CHECK_NAME, "POSIX file modes only — Windows ACLs are a separate story")
}

/// Pure decision for the `serve daemon token` row (#881) — split out so both
/// arms are testable without touching the Keychain/env. Always informational
/// (never a Warn): a loopback-only daemon with no token is the ordinary
/// single-machine state, and `serve` refuses the non-loopback bind itself.
/// The token is the EXECUTION credential (#2988); what reads need is the
/// separate `serve reads` row.
fn serve_token_status(token_present: bool) -> (Status, String, Option<String>) {
    if token_present {
        (
            Status::Pass,
            "serve token resolves: fleet work submission requires it (with a verified sender); reads need it only \
             when serve.read_auth is on"
                .into(),
            None,
        )
    } else {
        (
            Status::Pass,
            "no serve token: the daemon is loopback-only and this machine takes and sends no fleet work".into(),
            Some(
                "To take or send fleet work, set ONE shared bearer token on every machine: \
                 `security add-generic-password -U -a \"$USER\" -s darkmux-serve-token -w` (macOS) + \
                 `darkmux config set serve.token_keychain true`, or export DARKMUX_SERVE_TOKEN."
                    .into(),
            ),
        )
    }
}

/// Pure decision for the `serve reads` row (#2988): what a READ from off
/// this machine needs, independent of whether a token exists. Read auth on
/// with no token is a `Fail` because `darkmux serve` refuses to start in it.
fn serve_reads_status(read_auth: bool, token_present: bool) -> (Status, String, Option<String>) {
    match (read_auth, token_present) {
        (false, _) => (
            Status::Pass,
            "open to whatever reaches the daemon, including tailnet peers through `tailscale serve` \
             (serve.read_auth off)"
                .into(),
            Some("To require the serve token for reads from off this machine: `darkmux config set serve.read_auth true`.".into()),
        ),
        (true, true) => (
            Status::Pass,
            "a read not from this machine needs the serve token, proxied requests included (serve.read_auth on)".into(),
            None,
        ),
        (true, false) => (
            Status::Fail,
            "serve.read_auth is on but no serve token resolves: `darkmux serve` refuses to start".into(),
            Some(
                "Store the token: `security add-generic-password -U -a \"$USER\" -s darkmux-serve-token -w` + \
                 `darkmux config set serve.token_keychain true` (or export DARKMUX_SERVE_TOKEN), or \
                 `darkmux config set serve.read_auth false`."
                    .into(),
            ),
        ),
    }
}

/// `serve daemon token`: reports whether the shared fleet token resolves
/// (#881). Named for the STATE it reports, not for a posture (#1839): a
/// check that names a security concern and can only ever pass reads as a
/// security check that cleared.
fn check_serve_daemon_token() -> Check {
    let (status, message, hint) = serve_token_status(darkmux_flow::serve_token_present());
    Check { name: "serve daemon token".into(), status, message, hint }
}

/// `serve reads`: the read posture (#2988), beside the token row so doctor
/// shows both.
fn check_serve_reads() -> Check {
    let (status, message, hint) =
        serve_reads_status(darkmux_types::config_access::serve_read_auth(), darkmux_flow::serve_token_present());
    Check { name: "serve reads".into(), status, message, hint }
}

/// `utility model`: surfaces the machine-level `internal.utility` binding
/// (#590) — the standing support model the runtime summons for compaction
/// (and future estimation / mission-compile). When it's registered the model
/// must be LOADED, because compaction fires mid-dispatch and a missing
/// utility model makes the compactor call fail. This is the operator-facing
/// half of the silent-eviction guard (the dispatch-time check lands with the
/// wiring); doctor flags "registered but not loaded" before you dispatch.
fn check_utility_model_binding() -> Check {
    let registry = darkmux_profiles::profiles::load_registry(None).ok().map(|l| l.registry);
    let registry_util = registry.as_ref().and_then(|r| r.utility_model_id().map(str::to_string));
    let n_ctx = registry.as_ref().and_then(|r| r.utility_model_n_ctx());
    // Only query LMStudio when there's a binding to check.
    let loaded = if registry_util.is_some() {
        darkmux_profiles::lms::list_loaded().ok()
    } else {
        None
    };
    utility_binding_status(registry_util.as_deref(), n_ctx, loaded.as_deref())
}

/// Pure decision for `check_utility_model_binding`, split out so every arm is
/// unit-testable without a live LMStudio. `loaded` is `None` when the binding
/// is set but `lms ps` couldn't be queried. (#2914) `n_ctx` is the window the
/// binding declares (`internal.utility.n_ctx`); `None` when none is declared,
/// which still works but gets nudged to declare one, since that window
/// is now the ONLY source of the compactor's own context (a profile entry
/// no longer counts).
fn utility_binding_status(
    registry_util: Option<&str>,
    n_ctx: Option<u32>,
    loaded: Option<&[darkmux_types::LoadedModel]>,
) -> Check {
    let name = "utility model".to_string();
    let Some(id) = registry_util else {
        return Check {
            name,
            // (Third review round) Bumped from Pass to Warn. Two reasons,
            // both about this row's own internal consistency rather than
            // the underlying fact (which was already accurate): first, the
            // sibling arm below already uses Warn for the NARROWER problem
            // of a binding that's registered but not loaded — a fully
            // unbound machine (compaction off for every dispatch) is at
            // least as consequential and can't rank below it. Second, a
            // Pass row's message is invisible in the default view (doctor's
            // banner collapses passing checks into a bare count; this
            // sentence only renders under --verbose), so leaving it at Pass
            // buried the exact fact the MUST FIX 1 disclosures exist to
            // surface loudly at dispatch time — the two would agree in
            // wording but disagree in how loudly either was said. Warn
            // doesn't mean "wrong" — deliberately running without
            // compaction is a legitimate operator choice (operator
            // sovereignty) — it means "worth a second look," which a count
            // of passing checks can't convey. The hint no longer opens with
            // "Optional:" for the same reason: that word read as downgrading
            // a message that, two sentences later, says compaction is off
            // machine-wide. It now says plainly that no action is needed if
            // this is deliberate, before describing how to change it.
            status: Status::Warn,
            message: "no machine utility model registered; compaction is OFF for every \
                      dispatch on this machine (no runtime fallback since #2571)"
                .into(),
            hint: Some(
                "No action needed for compaction if you are deliberately running without it; radio and ACP routing NEED this binding (a message cannot be routed without a utility model). To set it: register a small fast model as this machine's utility model in ~/.darkmux/profiles.json: `\"internal\": { \"utility\": { \"id\": \"<model-id>\", \"n_ctx\": <window> } }`. It serves compaction and radio routing for every role, decoupled from your profiles, and is never selectable for a task. (#590, #2571, #2914)".into(),
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
                    message: match n_ctx {
                        Some(n) => format!("utility model `{id}` registered and loaded (n_ctx {n})"),
                        None => format!("utility model `{id}` registered and loaded (no window declared)"),
                    },
                    hint: bare_binding_window_hint(id, n_ctx),
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
                    // That is ALL the binding does. Every other verb resolves
                    // its own model from the profile and reaches the SAME
                    // self-loading dispatch path, so no verb needs this
                    // resident first.
                    //
                    // What remains true is only that a hand-load moves the
                    // cost earlier. Say that and nothing more.
                    hint: Some(
                        "No verb needs this loaded first: the binding names the model darkmux's own jobs run on (compaction, and radio/ACP routing since #2914), and every path that uses it self-loads it at the binding's `n_ctx` under the `darkmux:` namespace (#1616). Loading it by hand just pays that cost now instead of during the first dispatch or route; if you do, keep the namespace and the context: `lms load <id> --context-length <n> --identifier darkmux:<id>`: since a bare `lms load` creates a resident darkmux won't reuse and `machine eject` can't reclaim. (#590, #1616, #1675, #2914)".into(),
                    ),
                }
            }
        }
    }
}

/// (#2914) The nudge for a binding that declares no window: since #2914 the window in
/// `internal.utility` is the only source of the compactor's own context (a
/// profile entry no longer counts), so an undeclared window falls back to
/// the primary's for compaction and to a fixed 16K for radio routing, both
/// named on stderr when they apply. `None` when a window is declared.
fn bare_binding_window_hint(id: &str, n_ctx: Option<u32>) -> Option<String> {
    n_ctx.is_none().then(|| {
        format!(
            "Declare the utility model's window once, in the binding: `\"internal\": {{ \"utility\": \
             {{ \"id\": \"{id}\", \"n_ctx\": <window> }} }}`. Without it, compaction loads the model at \
             the primary's window and radio routing at 16384, each saying so at dispatch time. (#2914)"
        )
    })
}

/// (#2914) `utility model in profiles`: a profile that still lists the
/// machine's utility model in its `models[]` is a pre-4.0 leftover. It is
/// harmless to a task (every selection path sets the utility model aside)
/// but misleading: the entry looks like a work model, and its `n_ctx` no
/// longer does anything (the window comes from `internal.utility` alone).
/// Warn naming each profile and the window it declared, so the operator can
/// move that number into the binding and drop the entry.
fn check_utility_model_in_profiles() -> Check {
    match darkmux_profiles::profiles::load_registry(None) {
        Ok(l) => utility_in_profiles_status(&l.registry),
        Err(_) => Check {
            name: "utility model in profiles".into(),
            status: Status::Warn,
            message: "the profile registry did not load (see the `profile registry` row): can't check whether a profile lists the utility model".into(),
            hint: None,
        },
    }
}

/// Pure decision for [`check_utility_model_in_profiles`].
fn utility_in_profiles_status(registry: &darkmux_types::ProfileRegistry) -> Check {
    let name = "utility model in profiles".to_string();
    let Some(utility) = registry.utility_model_id() else {
        return Check { name, status: Status::Pass, message: "no machine utility model registered".into(), hint: None };
    };
    // Match on the bare model key in either spelling, the same comparison
    // every utility-model check in darkmux-crew uses.
    let bare = |id: &str| darkmux_gestalt::bare_model_key(id).to_string();
    let utility_key = bare(utility);
    let mut offenders: Vec<(String, Option<u32>)> = registry
        .profiles
        .iter()
        .filter_map(|(profile_name, profile)| {
            // (C2) A hosted model sharing the id is served elsewhere, never
            // the local utility instance: not a leftover.
            profile
                .models
                .iter()
                .find(|m| m.is_managed() && bare(&m.id) == utility_key)
                .map(|m| (profile_name.clone(), m.n_ctx))
        })
        .collect();
    offenders.sort();
    if offenders.is_empty() {
        return Check {
            name,
            status: Status::Pass,
            message: format!("no profile lists the utility model `{utility}`; profiles hold work models only"),
            hint: None,
        };
    }
    let listed: Vec<String> = offenders
        .iter()
        .map(|(p, n)| match n {
            Some(n) => format!("{p} (n_ctx {n})"),
            None => p.clone(),
        })
        .collect();
    let window = match (registry.utility_model_n_ctx(), offenders.iter().filter_map(|(_, n)| *n).max()) {
        (Some(declared), _) => format!("The binding already declares n_ctx {declared}"),
        (None, Some(largest)) => format!(
            "Move the window into the binding: `\"internal\": {{ \"utility\": {{ \"id\": \"{utility}\", \
             \"n_ctx\": {largest} }} }}` (the largest a profile declared for it)"
        ),
        (None, None) => format!(
            "Declare its window in the binding: `\"internal\": {{ \"utility\": {{ \"id\": \"{utility}\", \
             \"n_ctx\": <window> }} }}`"
        ),
    };
    Check {
        name,
        status: Status::Warn,
        message: format!(
            "profile{} list{} the utility model `{utility}` as a work model: {}",
            if offenders.len() == 1 { "" } else { "s" },
            if offenders.len() == 1 { "s" } else { "" },
            listed.join(", ")
        ),
        hint: Some(format!(
            "A task never runs on the utility model (#2914), so these entries are inert and their \
             n_ctx is ignored; the window comes from `internal.utility` alone. {window}, then remove \
             `{utility}` from each profile's `models[]` in ~/.darkmux/profiles.json. A profile with \
             no other model needs a work model added."
        )),
    }
}

/// (#2914, CONFIG 1.28) The removed radio ROUTING-seat staffing that is not a
/// config key or an env var: `role_profiles.radio-router`, a binding in the
/// dynamic map. Routing runs on the machine's utility model now, so it is
/// inert; `Warn` naming it, with the one fix. (A leftover `radio.router_profile`
/// key is a retired key, refused by the user-file keys row, and
/// `DARKMUX_RADIO_ROUTER_PROFILE` a retired env var, refused at CLI entry and
/// failed by the retired-env row: both are `config::RETIRED_SETTINGS`.)
fn check_removed_radio_router_staffing() -> Check {
    let role_binding = darkmux_types::config_access::role_profile("radio-router");
    removed_radio_router_staffing_status(role_binding.as_deref())
}

/// Pure decision for [`check_removed_radio_router_staffing`].
fn removed_radio_router_staffing_status(role_binding: Option<&str>) -> Check {
    let name = "radio router staffing (removed)".to_string();
    let Some(profile) = role_binding else {
        return Check { name, status: Status::Pass, message: "not present".into(), hint: None };
    };
    let paths = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser);
    Check {
        name,
        status: Status::Warn,
        message: format!(
            "config.json binds `role_profiles.radio-router` to `{profile}`: the router has no profile; \
             delete the `radio-router` entry from the `role_profiles` block in {} by hand",
            home_display(&paths.config)
        ),
        hint: Some(format!(
            "Since 5.0 (#2914) radio routing runs on the machine's utility model, declared once as \
             `internal.utility` in {} (with its `n_ctx`), never on a profile. \
             This binding has no effect; a profile that existed only for the router can \
             be deleted. The answering seat is still staffed by `radio.answerer_profile` / \
             `role_profiles.radio-host`.",
            home_display(&paths.profiles)
        )),
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

/// (#1944) Surface — never touch — a `darkmux:`-namespaced resident that no
/// current profile can address. Reconcile (`AcquireScope::Exclusive`,
/// `ensure_wave_loaded`) already evicts a darkmux-owned orphan on the NEXT
/// dispatch that reaches it — but only then. Between the moment a profile
/// changes (or a review-staffing seat like `qwen38-probe` is retired) and the
/// next dispatch, that orphan sits resident with nothing telling the operator
/// it's there; #1944's own report was found by hand-reading `lms ps`, not by
/// any darkmux surface. This check closes that gap: read-only, never
/// auto-unloads (operator sovereignty, #44) — it names the stranded
/// identifier and suggests the single surgical `lms unload`, exactly the move
/// the issue's operator made by hand.
///
/// "Addressable" means the resident's namespaced identifier
/// (`darkmux_profiles::ownership::namespaced_identifier`) matches either (a) some
/// model entry in some profile in the registry, or (b) the machine's
/// `internal.utility` binding (#590) — the ONE darkmux-owned identifier that
/// is legitimately never listed in any profile's `models[]`. A non-namespaced
/// resident is user state and is never inspected here — same filter
/// `is_darkmux_owned` applies everywhere else (`machine status`/`eject`,
/// dispatch preflight, `plan_acquire`'s own Exclusive pass).
///
/// Deliberately OUT OF SCOPE (#1944 CONSIDER 5): matching is identity-only —
/// a resident whose identifier matches a declared model but whose LOADED
/// context is smaller than that model's `n_ctx` (so gestalt refuses to
/// reuse it and would reload rather than dispatch to it — the #1135 shape)
/// still reads as `Pass` here. That's a real accumulation path, just not
/// the one #1944 reported; widen this check to compare
/// `LoadedModel.context` against the declared `n_ctx` if that shape shows
/// up in practice.
fn check_unreachable_darkmux_residents() -> Check {
    let (registry, registry_path) = match darkmux_profiles::profiles::load_registry(None) {
        Ok(r) => (r.registry, r.path),
        Err(_) => {
            return Check {
                name: "unreachable residents".into(),
                status: Status::Warn,
                message: "the profile registry did not load (see the `profile registry` row): can't check resident reachability".into(),
                hint: None,
            };
        }
    };
    let loaded = match darkmux_profiles::lms::list_loaded() {
        Ok(l) => l,
        Err(_) => {
            return Check {
                name: "unreachable residents".into(),
                status: Status::Warn,
                message: "could not enumerate loaded models".into(),
                hint: None,
            };
        }
    };
    unreachable_residents_status(&loaded, &registry, &registry_path)
}

/// Pure decision for [`check_unreachable_darkmux_residents`], split out so
/// every arm is unit-testable without a live LMStudio / profile file round
/// trip (same split as `utility_binding_status` / `unpriceable_residents_status`).
fn unreachable_residents_status(
    loaded: &[darkmux_types::LoadedModel],
    registry: &darkmux_types::ProfileRegistry,
    registry_path: &std::path::Path,
) -> Check {
    let name = "unreachable residents".to_string();

    let mut addressable: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for profile in registry.profiles.values() {
        for m in &profile.models {
            addressable.insert(darkmux_profiles::ownership::namespaced_identifier(m));
        }
    }
    if let Some(util_id) = registry.utility_model_id() {
        // `internal.utility` is a bare id, never an explicit-identifier
        // override (the schema has no field for one there) — the same
        // namespaced form `apply_utility_model` loads it under (#590,
        // #1616). Route through the same `ProfileModel`-shaped helper the
        // profile loop above uses rather than adding a `darkmux-gestalt`
        // dependency just for its two-arg twin.
        let util_pm = darkmux_types::ProfileModel { id: util_id.to_string(), ..Default::default() };
        addressable.insert(darkmux_profiles::ownership::namespaced_identifier(&util_pm));
    }

    let unreachable: Vec<&str> = loaded
        .iter()
        .filter(|l| darkmux_profiles::ownership::is_darkmux_owned(&l.identifier))
        .filter(|l| !addressable.contains(&l.identifier))
        .map(|l| l.identifier.as_str())
        .collect();

    if unreachable.is_empty() {
        return Check {
            name,
            status: Status::Pass,
            message: "every darkmux-owned resident is addressable by some profile".into(),
            hint: None,
        };
    }

    // (#1944 CONSIDER 4) A profile whose entry failed to parse is
    // quarantined out of `registry.profiles` entirely (#1282) — invisible
    // to the `addressable` set above. A resident loaded from a profile
    // that's currently quarantined by a hand-edit typo would otherwise
    // read identically to a genuine orphan; name the quarantine explicitly
    // rather than let the operator draw the wrong conclusion from the
    // warning alone.
    let quarantine_note = if registry.quarantined.is_empty() {
        String::new()
    } else {
        format!(
            " Note: {} registry entr{} {} currently quarantined (failed to parse: see the profile-registry check): {}. If one of the residents above was loaded from a quarantined profile, or a profile naming a quarantined endpoint, it may simply be waiting on that entry to be fixed, not genuinely orphaned.",
            registry.quarantined.len(),
            if registry.quarantined.len() == 1 { "y" } else { "ies" },
            if registry.quarantined.len() == 1 { "is" } else { "are" },
            // (#2902 re-review C2) Profiles and endpoints both quarantine;
            // each is named with its kind.
            registry
                .quarantined
                .iter()
                .map(|q| format!("{} \"{}\"", q.kind, q.name))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    Check {
        name,
        status: Status::Warn,
        message: format!(
            "{} darkmux-owned resident(s) unreachable — no profile in this registry declares them right now: {}",
            unreachable.len(),
            unreachable.join(", ")
        ),
        hint: Some(format!(
            "darkmux never auto-unloads a resident outside a reconcile it's already planning (operator sovereignty, #44). A reconcile pass DOES evict a darkmux-owned orphan like this — but only on the next dispatch that reaches it, and only through THIS same registry (this registry was loaded from {}). To reclaim the RAM now rather than wait: `lms unload <identifier>` for each one listed (e.g. `lms unload {}`), or `darkmux machine eject` to sweep every darkmux-owned resident if nothing is running. This is usually a leftover from a superseded profile version or review-staffing seat — no data loss either way.{}",
            home_display(registry_path),
            unreachable[0],
            quarantine_note
        )),
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
    // none of which are real role ids (the real ones are `pr-reviewer`,
    // `code-reviewer`, `analyst`) — is flagged instead of certified.
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
            message: format!("can't verify the role->profile map (profile registry load failed: {e:#})"),
            hint: Some("Fix the profile registry (`darkmux doctor` profile-registry check), then re-run.".into()),
        },
    }
}

/// The check behind `radio_peer_seat_status`: reads the two places radio's
/// answering seat can be written (`radio.answerer_profile`, then
/// `role_profiles.radio-host`) and the machines this one can reach: the
/// roster's ids plus this machine's own id (an address naming this machine
/// runs here).
fn check_radio_peer_seat() -> Check {
    let mut seats = Vec::new();
    if let Some(reference) = darkmux_types::config_access::radio_answerer_profile() {
        seats.push(("radio.answerer_profile".to_string(), reference));
    }
    if let Some(reference) = darkmux_types::config_access::role_profile("radio-host") {
        seats.push(("role_profiles.radio-host".to_string(), reference));
    }
    // Only a seat written as `<profile>@<machine>` (or a malformed one) needs the roster.
    let needs_roster = seats.iter().any(|(_, reference)| {
        darkmux_types::profile_address::ProfileAddress::parse(reference).map_or(true, |a| a.machine.is_some())
    });
    if !needs_roster {
        return radio_peer_seat_status(&seats, &std::collections::BTreeSet::new());
    }
    let mut known: std::collections::BTreeSet<String> = match darkmux_fleet::load_roster() {
        Ok(roster) => roster.machines.into_keys().collect(),
        Err(e) => {
            return Check {
                name: "radio peer seat".into(),
                status: Status::Warn,
                message: format!("can't verify the answering seat's machine (the fleet roster did not load: {e:#})"),
                hint: Some("Fix the roster file (`darkmux machine list`), then re-run.".into()),
            };
        }
    };
    known.extend(darkmux_flow::resolve_machine_id());
    radio_peer_seat_status(&seats, &known)
}

/// Pure decision for [`check_radio_peer_seat`]. `seats` is each written seat
/// as (where it is written, its reference); `known_machines` is every
/// machine an address can name. Only a well-formed `<profile>@<machine>` is
/// this check's business: a bare name is the registry's and a malformed
/// address is `role_profiles`'s.
fn radio_peer_seat_status(seats: &[(String, String)], known_machines: &std::collections::BTreeSet<String>) -> Check {
    let name = "radio peer seat".to_string();
    let unknown: Vec<String> = seats
        .iter()
        .filter_map(|(source, reference)| {
            let machine = darkmux_types::profile_address::ProfileAddress::parse(reference).ok()?.machine?;
            let known = known_machines.iter().any(|k| darkmux_fleet::same_machine(k, &machine));
            (!known).then(|| format!("{source} = `{reference}` names machine `{machine}`"))
        })
        .collect();
    if unknown.is_empty() {
        return Check { name, status: Status::Pass, message: "no radio answering seat names an unknown machine".into(), hint: None };
    }
    Check {
        name,
        status: Status::Warn,
        message: format!("{}, which is not in this machine's fleet roster", unknown.join("; ")),
        hint: Some(
            "The answering seat is sent to that machine, so every question fails until it is reachable. \
             Add it with `darkmux machine add <id> <address>`, or correct the machine name in the seat."
                .into(),
        ),
    }
}

/// One role binding (or the radio seat) written as `<profile>@<machine>`: where
/// it is written, the role that would run there, and the address.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FleetRoute {
    source: String,
    role: String,
    address: String,
}

/// The routes to ask a peer about: `radio.answerer_profile` (which runs the
/// `radio-host` role) and every `role_profiles.<role>` value that is a
/// well-formed `<profile>@<machine>` naming a machine other than `local`
/// (an address naming this machine runs here, so there is no peer to ask).
/// A bare profile name is the registry's, and a malformed address is
/// `role_profiles`'s.
fn fleet_routes(
    answerer: Option<String>,
    bindings: &std::collections::BTreeMap<String, String>,
    local: Option<&str>,
) -> Vec<FleetRoute> {
    let written = answerer
        .map(|a| ("radio.answerer_profile".to_string(), "radio-host".to_string(), a))
        .into_iter()
        .chain(bindings.iter().map(|(role, a)| (format!("role_profiles.{role}"), role.clone(), a.clone())));
    written
        .filter(|(_, _, address)| {
            darkmux_types::profile_address::ProfileAddress::parse(address)
                .ok()
                .and_then(|a| a.machine)
                .is_some_and(|m| !local.is_some_and(|l| darkmux_fleet::same_machine(l, &m)))
        })
        .map(|(source, role, address)| FleetRoute { source, role, address })
        .collect()
}

/// A refusal code as its wire word (`not_listed`, `role_not_allowed`).
fn refusal_word(code: darkmux_fleet::RefusalCode) -> String {
    serde_json::to_value(code).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "unknown".into())
}

/// What one route's check found, in a clause.
fn route_finding(route: &FleetRoute, check: &darkmux_fleet::ReadOnlyCheck) -> (bool, String) {
    use darkmux_fleet::{CheckOutcome, EndpointClass, ReadOnlyCheck, SeatOutlook};
    let head = format!("{} = `{}`", route.source, route.address);
    let outcome = match check {
        ReadOnlyCheck::NotPinned { machine } => {
            return (
                false,
                format!(
                    "{head}: not pinned yet: the first `dispatch` or radio answer to {machine} pins its node \
                     (doctor does not pin, and sent nothing to it)"
                ),
            )
        }
        ReadOnlyCheck::Asked(outcome) => outcome,
    };
    match outcome {
        CheckOutcome::Routable { profile, report } => {
            let endpoint = match report.endpoint {
                EndpointClass::Managed => "managed",
                EndpointClass::Unmanaged => "unmanaged",
                EndpointClass::Unknown => "endpoint unknown",
            };
            let seat = match report.seat {
                SeatOutlook::Free => "seat free",
                SeatOutlook::WouldQueue => "seat busy, a run would queue",
                SeatOutlook::Unknown => "seat unknown",
            };
            (true, format!("{head}: the receiver would run role {} on {profile} ({endpoint}; {seat})", route.role))
        }
        CheckOutcome::Refused { code, reason } => {
            (false, format!("{head}: refused by the receiver ({}): {reason}", refusal_word(*code)))
        }
        CheckOutcome::Unanswered { detail } => (false, format!("{head}: the receiver could not be asked: {detail}")),
    }
}

/// Pure decision for [`check_fleet_routes`]. A route the receiver refuses, or
/// that could not be asked (an unreachable peer), is a Warn that says which
/// and why, never a Fail: doctor reports the route, it does not depend on the
/// peer being up.
fn fleet_routes_status(results: &[(FleetRoute, darkmux_fleet::ReadOnlyCheck)]) -> Check {
    let name = "fleet routes".to_string();
    if results.is_empty() {
        return Check {
            name,
            status: Status::Pass,
            message: "no role binding or radio seat is addressed to a fleet peer".into(),
            hint: None,
        };
    }
    let findings: Vec<(bool, String)> = results.iter().map(|(r, o)| route_finding(r, o)).collect();
    let message = findings.iter().map(|(_, m)| m.as_str()).collect::<Vec<_>>().join("; ");
    if findings.iter().all(|(ok, _)| *ok) {
        return Check { name, status: Status::Pass, message, hint: None };
    }
    Check {
        name,
        status: Status::Warn,
        message,
        hint: Some(
            "Each route was checked with its receiver, which runs the same gates a real job meets and \
             answers without running anything. Its sentence above names the fix: on the receiver, \
             `darkmux machine trust` for a sender it does not list or a role or profile it does not \
             grant; or fix the address, the fleet token, or the receiver's listener."
                .into(),
        ),
    }
}

/// The check behind [`fleet_routes_status`]: asks each peer route's receiver
/// whether it would take the job ([`darkmux_fleet::check_route_read_only`],
/// which never pins a node or writes the roster), all at
/// once, each bounded by the fleet client's own timeout.
fn check_fleet_routes() -> Check {
    let routes = fleet_routes(
        darkmux_types::config_access::radio_answerer_profile(),
        &darkmux_types::config_access::role_profiles(),
        darkmux_flow::resolve_machine_id().as_deref(),
    );
    let results: Vec<(FleetRoute, darkmux_fleet::ReadOnlyCheck)> = std::thread::scope(|s| {
        let asked: Vec<_> = routes
            .into_iter()
            .map(|r| {
                s.spawn(move || {
                    let outcome = darkmux_fleet::check_route_read_only(&r.address, &r.role, None);
                    (r, outcome)
                })
            })
            .collect();
        asked.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    fleet_routes_status(&results)
}

/// A `radio-host` binding written as a well-formed `<profile>@<machine>`
/// address: radio's answering seat runs on that fleet peer, whose registry
/// (not this one) defines the profile.
fn is_peer_binding(role: &str, profile: &str) -> bool {
    role == "radio-host"
        && darkmux_types::profile_address::ProfileAddress::parse(profile).is_ok_and(|a| a.machine.is_some())
}

/// The role->profile bindings that are off target, sorted by why (see
/// [`role_profiles_status`]).
#[derive(Default)]
struct BindingSort<'a> {
    unknown_role_pairs: Vec<(&'a String, &'a String)>,
    quarantined_pairs: Vec<(&'a String, &'a String)>,
    undefined_pairs: Vec<(&'a String, &'a String)>,
}

fn sort_role_bindings<'a>(
    map: &'a std::collections::BTreeMap<String, String>,
    known_profiles: &std::collections::BTreeMap<String, darkmux_types::Profile>,
    quarantined: &std::collections::BTreeSet<String>,
    known_roles: &std::collections::BTreeSet<String>,
) -> BindingSort<'a> {
    let mut sorted = BindingSort::default();
    for (role, profile) in map.iter() {
        // known_roles is empty only when the caller had no bindings to check
        // (the empty-map arm above returns before reaching here) — so an
        // empty set here means the role library itself was unavailable, which
        // check_role_profiles already turns into its own Warn before calling
        // this function; a real known_roles is always non-empty in practice.
        if !known_roles.contains(role.as_str()) {
            sorted.unknown_role_pairs.push((role, profile));
            continue;
        }
        if known_profiles.contains_key(profile.as_str()) || is_peer_binding(role, profile) {
            continue; // both halves defined + healthy, or the profile lives on a fleet peer
        }
        if quarantined.contains(profile.as_str()) {
            sorted.quarantined_pairs.push((role, profile));
        } else {
            sorted.undefined_pairs.push((role, profile));
        }
    }
    sorted
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
                "Optional: bind a role to a profile with `darkmux config set role_profiles.<role> <profile>` (e.g. `role_profiles.coder qwen35b`). Profiles stay role-agnostic; the map welds a role to one on this machine. (#1475)".into(),
            ),
        };
    }
    // An off-target binding names a role id that doesn't exist, OR a profile
    // the registry doesn't DEFINE. Split by why: unknown role (checked first —
    // no profile-side wording is useful when the role itself can't resolve),
    // then quarantined (in profiles.json but broken) vs genuinely undefined.
    let BindingSort { unknown_role_pairs, quarantined_pairs, undefined_pairs } =
        sort_role_bindings(map, known_profiles, quarantined, known_roles);
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
/// passes when a value resolves — this is informational. The check names the
/// tier the value actually came from (`DARKMUX_MACHINE_ID` env >
/// `config.json` `machine_id` > hostname), so operators can see which layer
/// is in effect. (#167; #2924 fixed a config value being labeled "from
/// hostname".)
///
/// The machine_id is the ONE name a machine goes by: flow records, presence
/// beats, and — by #2924 — the fleet roster all key on it.
fn check_machine_id_resolution() -> Check {
    use darkmux_flow::MachineIdSource;
    match darkmux_flow::resolve_machine_id_with_source() {
        Some((id, MachineIdSource::Env)) => Check {
            name: "machine_id".into(),
            status: Status::Pass,
            message: format!("`{id}` (from DARKMUX_MACHINE_ID env)"),
            hint: None,
        },
        Some((id, MachineIdSource::Config)) => Check {
            name: "machine_id".into(),
            status: Status::Pass,
            message: format!("`{id}` (from config.json machine_id)"),
            hint: None,
        },
        Some((id, MachineIdSource::Hostname)) => Check {
            name: "machine_id".into(),
            status: Status::Pass,
            message: format!("`{id}` (from hostname)"),
            hint: Some(
                "Set a logical fleet name (e.g. `studio`, `mini-1`) with `darkmux config set machine_id <name>`: this name is what flow records, presence and the fleet roster all join on, and an operator-chosen one survives a hostname change.".into(),
            ),
        },
        None => Check {
            name: "machine_id".into(),
            status: Status::Warn,
            message: "could not resolve a machine_id — flow records will lack machine provenance".into(),
            hint: Some(
                "Set a logical fleet name with `darkmux config set machine_id <name>` (e.g. `studio`, `mini-1`), or install `hostname(1)` on PATH.".into(),
            ),
        },
    }
}

/// (5.0) Whether this machine can read its own hardware uid. A job that names
/// a hardware uid is refused here when it cannot (it never falls back to the
/// machine name), so an unreadable uid is a warning with its consequence. The
/// uid itself is never printed.
fn machine_uid_check(uid: Option<&str>) -> Check {
    match uid {
        Some(_) => Check {
            name: "machine uid".into(),
            status: Status::Pass,
            message: "this machine's hardware uid is readable".into(),
            hint: None,
        },
        None => Check {
            name: "machine uid".into(),
            status: Status::Warn,
            message: "this machine's own hardware uid is unreadable, so fleet jobs that name a hardware uid are refused here (misaddressed)".into(),
            hint: Some(
                "The uid is read from the platform (`ioreg` on macOS); check that it runs and reports IOPlatformUUID. Jobs addressed by machine name alone are still accepted.".into(),
            ),
        },
    }
}

/// Name of the user-file keys rows (one Fail row per bad file, suffixed
/// with its path; one Pass row when every file is clean).
const USER_FILE_KEYS_CHECK_NAME: &str = "user file keys";

/// (4.0) One Fail row per user file (`darkmux_types::user_files`) that
/// carries a key its schema does not know or is not JSON, with the same
/// message the preflight refuses with; one Warn row per retired `config.json`
/// key still at a value ignoring which changes nothing (#3057, it refuses
/// nothing); and one Pass row when there is neither.
/// Loading ignores an unknown key, so this and the preflight are where it
/// surfaces. The crawl's workspace spec has no fixed location; its launch
/// refuses it where it is loaded.
pub fn check_user_file_keys() -> Vec<Check> {
    use darkmux_types::user_files::UserFileKind;
    let problems: Vec<darkmux_types::user_files::FileProblem> =
        UserFileKind::ALL.into_iter().flat_map(user_file_problems).collect();
    let warnings = darkmux_types::user_files::config_json_warnings();
    user_file_key_rows(&problems, &warnings)
}

/// Every problem in the files of `kind`, from the crate that owns its type.
fn user_file_problems(kind: darkmux_types::user_files::UserFileKind) -> Vec<darkmux_types::user_files::FileProblem> {
    use darkmux_types::user_files::UserFileKind;
    match kind {
        UserFileKind::Config => darkmux_types::user_files::config_json_problems(),
        // The file itself, not the loaded registry: a file the typed load
        // refuses is the one whose every refused shape must be named.
        UserFileKind::Profiles => darkmux_profiles::profiles::registry_path(None)
            .and_then(|path| darkmux_profiles::profiles::user_file_problem(&path))
            .into_iter()
            .collect(),
        UserFileKind::Role
        | UserFileKind::Skill
        | UserFileKind::Crew
        | UserFileKind::MissionConfig
        | UserFileKind::Rule => darkmux_crew::user_files::problems(kind, darkmux_types::user_files::Reach::Every),
        UserFileKind::Workload | UserFileKind::LabFixture => {
            darkmux_lab::user_files::problems(kind, darkmux_types::user_files::Reach::Every)
        }
        UserFileKind::WorkspaceSpec => Vec::new(),
    }
}

/// What to do about one bad user file, and what it does meanwhile: an
/// unknown key does nothing, but broken JSON, a wrong-type value or a
/// missing required key makes the whole file fail to load.
fn user_file_hint(p: &darkmux_types::user_files::FileProblem) -> String {
    use darkmux_types::user_files::{Issue, Problem, UserFileKind};
    let unloaded = match p.kind {
        UserFileKind::Config => "the whole file fails to load, so every setting falls back to its default",
        UserFileKind::Profiles => "the entry is quarantined, or the whole registry fails to load",
        UserFileKind::Role | UserFileKind::Skill | UserFileKind::Crew | UserFileKind::Rule => {
            "the file fails to load and is skipped, so a builtin of the same id runs in its place"
        }
        UserFileKind::MissionConfig | UserFileKind::Workload | UserFileKind::LabFixture | UserFileKind::WorkspaceSpec => {
            "the file fails to load"
        }
    };
    match &p.problem {
        Problem::Unreadable(_) => format!("make it readable (and under the size cap); until then {unloaded}"),
        Problem::NotJson(_) => format!("fix the JSON syntax; until then {unloaded}"),
        Problem::Newer { .. } => {
            "upgrade darkmux to the version that wrote it; until then every command that starts work refuses it at preflight".to_string()
        }
        Problem::Keys(keys) if keys.iter().any(|k| matches!(k.issue, Issue::WrongType { .. } | Issue::Missing { .. })) => {
            format!("fix each value named and add each missing key; until then {unloaded}")
        }
        Problem::Keys(keys) if keys.iter().any(|k| matches!(k.issue, Issue::Removed(_))) => {
            format!("make each rewrite named (and rename or delete any other key listed); until then {unloaded}")
        }
        Problem::Keys(_) => "rename each key to the valid one named, or delete it; until then it does nothing".to_string(),
    }
}

/// Pure row builder for [`check_user_file_keys`].
fn user_file_key_rows(
    problems: &[darkmux_types::user_files::FileProblem],
    warnings: &[darkmux_types::user_files::LeftoverWarning],
) -> Vec<Check> {
    if problems.is_empty() && warnings.is_empty() {
        return vec![Check {
            name: USER_FILE_KEYS_CHECK_NAME.into(),
            status: Status::Pass,
            message: "every user file's keys are known".into(),
            hint: None,
        }];
    }
    problems
        .iter()
        .map(|p| {
            let refused_by: Vec<&str> = p.kind.scopes().iter().map(|s| s.label()).collect();
            let consequence = match (&p.note, refused_by.is_empty()) {
                (Some(_), _) => String::new(),
                (None, true) => ". Nothing that starts work reads this file, so nothing refuses to start over it".to_string(),
                (None, false) => format!(". Refused at preflight by: {}", refused_by.join(", ")),
            };
            Check {
                // The file name keeps the row's name column narrow; the
                // message carries the full path.
                name: format!(
                    "{USER_FILE_KEYS_CHECK_NAME}: {}",
                    darkmux_types::user_files::escape_text(
                        &p.path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
                    )
                ),
                status: Status::Fail,
                // A refusal sentence already ends in a period (`Upgrade
                // darkmux.`), and the consequence starts with one.
                message: format!("{}{consequence}", p.to_string().trim_end_matches('.')),
                hint: Some(user_file_hint(p)),
            }
        })
        .chain(warnings.iter().map(leftover_warning_row))
        .collect()
}

/// A retired `config.json` key at a value ignoring which changes nothing: a
/// Warn row, since nothing refuses to start over it (#3057).
fn leftover_warning_row(w: &darkmux_types::user_files::LeftoverWarning) -> Check {
    Check {
        name: format!("{USER_FILE_KEYS_CHECK_NAME}: config.json"),
        status: Status::Warn,
        message: format!("{}. It holds its old default, so nothing refuses to start over it", w.to_string().trim_end_matches('.')),
        hint: Some("delete it from config.json; it is safe to remove".to_string()),
    }
}

/// (#2947) THE doctor check for enum-typed settings: one row per entry of
/// `darkmux_types::config_enum::ENUM_SETTINGS`, with no per-setting code.
/// A registered value is Pass, naming the value, its tier and its meaning;
/// an unregistered one is **Fail**, naming the raw value, where it was set,
/// every valid value with its meaning, and the fix. Replaces the
/// hand-written `fleet.mode` (#933/#934) and detection-policy (#2846)
/// checks, which each reported a typo as a Warn over a silent fallback.
///
/// A new enum setting gets this row by being registered: nothing here
/// changes.
pub fn check_enum_settings() -> Vec<Check> {
    use darkmux_types::config_enum::ENUM_SETTINGS;
    let mut rows = Vec::new();
    for s in ENUM_SETTINGS {
        let bad = darkmux_types::config_access::enum_bad_values(s);
        if bad.is_empty() {
            rows.push(enum_setting_pass_row(s));
            continue;
        }
        // One Fail row per bad value. A per-item setting's row is named by
        // the item's concrete path (`hooks.rules[2].match.level`), a
        // scalar's by its key.
        for b in bad {
            let name = match &b.set_in {
                darkmux_types::config_enum::SetIn::Config(path) if s.is_per_item() => path.clone(),
                _ => s.key.to_string(),
            };
            rows.push(Check {
                name,
                status: Status::Fail,
                // (#2947 review C6) Say what actually happens: which entry
                // points refuse, or, for a setting no work-starting entry
                // point reads, the registry's own stated reason.
                message: match s.no_scope_reason {
                    None => format!(
                        "{}. Refused at preflight by: {} (#2947)",
                        b.summary(),
                        s.scopes.iter().map(|sc| sc.label()).collect::<Vec<_>>().join(", ")
                    ),
                    Some(reason) => format!("{}. Nothing refuses to start over it: {reason} (#2947)", b.summary()),
                },
                hint: Some(format!("{}. {}", b.valid_line(), b.fix())),
            });
        }
    }
    rows
}

/// The Pass row for an enum setting with no bad value.
fn enum_setting_pass_row(s: &darkmux_types::config_enum::EnumSetting) -> Check {
    if s.is_per_item() {
        return Check {
            name: s.key.into(),
            status: Status::Pass,
            message: "every value set is valid (or none is set)".into(),
            hint: None,
        };
    }
    let (token, source) = darkmux_types::config_access::resolve_enum_token(s)
        .expect("enum_bad_values found no bad value, so the scalar resolves");
    let meaning = s.values.iter().find(|(t, _)| *t == token).map(|(_, m)| *m).unwrap_or("");
    let from = match (source, s.env) {
        (darkmux_types::config_access::Source::Env, Some(var)) => format!("from {var}"),
        (darkmux_types::config_access::Source::Config, _) => "from config.json".to_string(),
        _ => "default".to_string(),
    };
    Check { name: s.key.into(), status: Status::Pass, message: format!("`{token}` ({from}): {meaning}"), hint: None }
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

/// The hub's two streams with the retention cap each resolves to (`0` is
/// unbounded), so an operator sees where work records and machine samples
/// go and how many each keeps. (#2101)
fn redis_streams_summary() -> String {
    use darkmux_types::config_access as c;
    let cap = |n: usize| if n == 0 { "unbounded".to_string() } else { n.to_string() };
    format!(
        "work `{}` (maxlen {}), telemetry `{}` (maxlen {})",
        c::redis_stream(),
        cap(c::redis_maxlen()),
        c::redis_telemetry_stream(),
        cap(c::redis_telemetry_maxlen()),
    )
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
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: format!("Redis via DARKMUX_REDIS_URL · {}", redis_streams_summary()),
            hint: None,
        };
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
            message: format!("config Redis enabled → {host} (password from Keychain) · {}", redis_streams_summary()),
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

/// The `config.json` that `DarkmuxConfig::load_resolved()` reads — the
/// file a removed-key hint has to name. Honors `DARKMUX_HOME`, so an
/// operator whose root is not `~/.darkmux` is told the file that actually
/// holds the leftover key (#2913 review C4).
fn resolved_config_path() -> std::path::PathBuf {
    darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).config
}

/// Settings RENAMED or RETIRED with no alias
/// (`darkmux_types::config::RETIRED_SETTINGS`: the
/// `remote.*` limits 5.0 moved to each endpoint, `DARKMUX_CREW_DIR`). A
/// leftover env var is read by nothing; one whose loss would change behavior
/// is refused by every command but `doctor` and `config` and fails this row,
/// and one whose loss changes nothing is ignored with a warning and warns
/// here, each naming the replacement. A leftover old `config.json` key is an
/// unknown key, which the user-file keys row fails, or a warning there when it
/// holds its old default (#3057).
fn check_renamed_budget_settings() -> Check {
    renamed_settings_status(&|k| std::env::var(k).ok())
}

/// Pure decision for [`check_renamed_budget_settings`].
fn renamed_settings_status(env: &dyn Fn(&str) -> Option<String>) -> Check {
    let name = RETIRED_ENV_CHECK_NAME;
    let leftovers = darkmux_types::config::retired_env_leftovers(env);
    if leftovers.is_empty() {
        return Check { name: name.into(), status: Status::Pass, message: "none present".into(), hint: None };
    }
    // Fail only for a leftover darkmux refuses to start with; one it ignores
    // (nothing reads it, nothing is lost) is a warning, as at CLI entry.
    let refuses = leftovers.iter().any(|l| l.policy == darkmux_types::config::LeftoverPolicy::Refuse);
    Check {
        name: name.into(),
        status: if refuses { Status::Fail } else { Status::Warn },
        message: leftovers.iter().map(|l| l.line.clone()).collect::<Vec<_>>().join("; "),
        hint: Some(
            if refuses {
                "darkmux refuses to start with these set; remove the export from your shell rc."
            } else {
                "Nothing reads these any more and darkmux runs with them set; remove the export from your shell rc to quiet the warning."
            }
            .into(),
        ),
    }
}

/// (#2912 review M1) Every role → skill reference resolves to a skill
/// manifest. A dangling one is reachable on upgrade: 4.0 deleted the builtin
/// `mission-compiling` skill, and a pre-4.0 user-tier
/// `roles/mission-compiler.json` still names it. The crew index skips such a
/// link (and warns once, on the rebuild) rather than failing — this check is
/// the persistent surface that names the file, the missing skill, and the
/// edit (lenient on read, loud in doctor — contract 7).
fn check_role_skill_references() -> Check {
    let name = "role skill references";
    let refs = match darkmux_crew::index::dangling_skill_refs() {
        Ok(r) => r,
        Err(e) => {
            return Check {
                name: name.into(),
                status: Status::Warn,
                message: format!("could not load the role/skill manifests: {e:#}"),
                hint: None,
            }
        }
    };
    if refs.is_empty() {
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: "every role's skills resolve to a skill manifest".into(),
            hint: None,
        };
    }
    let found: Vec<String> = refs
        .iter()
        .map(|r| match &r.manifest {
            Some(p) => format!(
                "role `{}` ({}) names skill `{}`",
                r.role_id,
                p.display(),
                r.skill_id
            ),
            None => format!("builtin role `{}` names skill `{}`", r.role_id, r.skill_id),
        })
        .collect();
    let steps: Vec<String> = refs
        .iter()
        .map(|r| match &r.manifest {
            Some(p) => format!(
                "remove `{}` from the `skills` list in {} (or delete the file if it is a retired role you \
                 never customized)",
                r.skill_id,
                p.display()
            ),
            None => format!(
                "builtin role `{}` names a skill this binary does not embed; please file an issue",
                r.role_id
            ),
        })
        .collect();
    Check {
        name: name.into(),
        status: Status::Warn,
        message: format!(
            "{}: no skill manifest defines it; the crew index skips that link",
            found.join("; ")
        ),
        hint: Some(steps.join("; ")),
    }
}

/// (#2361, swarm S4-4) Informational: the bound on ONE operator-supplied
/// shell command a step runs — `mods.gate`'s `test_command` and
/// `procedural.shell`'s `command`. Always `Pass` (a preference, not a
/// health signal); surfaces the resolved value with provenance so an
/// operator whose gate reported `test_command exceeded <n>s` can see which
/// tier set that number without reading `config.json`.
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

/// (#3074) Informational: the per-dispatch inactivity budget, resolved, with
/// the tier that set it. `0` is UNBOUNDED (no watchdog deadline, no soft
/// warning), the reading every darkmux zero-knob has; it used to kill the
/// container on the first poll, so the row says which one the operator got.
fn check_inactivity_timeout() -> Check {
    let name = "runtime.inactivity_timeout_seconds";
    let (seconds, source) = darkmux_types::config_access::inactivity_timeout_seconds_with_source();
    let provenance = match source {
        darkmux_types::config_access::Source::Env => "from DARKMUX_INACTIVITY_TIMEOUT_SECONDS env",
        darkmux_types::config_access::Source::Config => "from config.json",
        darkmux_types::config_access::Source::BuiltIn => "default",
    };
    let message = if seconds == 0 {
        format!(
            "0s ({provenance}) — unbounded; a dispatch runs until it finishes or darkmux is \
             interrupted, and the runtime sends no inactivity warning"
        )
    } else {
        format!(
            "{seconds}s ({provenance}) — a dispatch with no proof-of-work signal for this long \
             is killed by the host watchdog"
        )
    };
    Check { name: name.into(), status: Status::Pass, message, hint: None }
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
/// Said out loud in the message: this is NOT an endpoint's
/// `limits.concurrent_calls`. The two were the same number before #2394 only
/// because dispatch-free steps had no seat class of their own, which is the
/// bug.
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
            "{n} ({provenance}): dispatch-free steps (procedural.shell/noop, mods.gate, \
             records.gather, deliver.github_review) run this many at a time, on their own \
             track; an endpoint's limits.concurrent_calls does not govern them"
        ),
        hint: None,
    }
}

/// (#2928 review, C3) The live socket a running daemon reports on `/health`
/// (`live.ingest`): a fingerprint, the port it is keyed by, and whether it
/// is still that daemon's.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonLiveSocket {
    socket_id: String,
    socket_port: u16,
    bound: bool,
}

fn parse_daemon_live_socket(health_body: &str) -> Option<DaemonLiveSocket> {
    let v: serde_json::Value = serde_json::from_str(health_body.split("\r\n\r\n").last()?).ok()?;
    let i = v.get("live")?.get("ingest")?;
    Some(DaemonLiveSocket {
        socket_id: i.get("socket_id")?.as_str()?.to_string(),
        socket_port: u16::try_from(i.get("socket_port")?.as_u64()?).ok()?,
        bound: i.get("bound")?.as_bool()?,
    })
}

/// (#2928) Surface the live channel's resolved cadence with provenance, the
/// clamp when one applied, and whether this machine's dispatches and its
/// daemon agree on the socket. Cadence is a recorded knob, never
/// adaptive-silent: a value outside 100..=1000 is clamped and this row says
/// so (Warn). (#2928 review, C3) Also names a stale socket (a daemon killed
/// without cleaning up), a daemon bound to a DIFFERENT socket than the one
/// dispatches send to (a `--port` that differs from `serve.port`), and a
/// daemon that lost its socket to another process.
fn check_live_channel() -> Check {
    let c = darkmux_types::config_access::live_cadence();
    let socket = darkmux_flow::live::local_socket_path();
    let state = socket.as_deref().map(darkmux_flow::live::probe_socket);
    let addr = darkmux_types::config_access::serve_client_addr();
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or("127.0.0.1").to_string();
    let port = darkmux_types::config_access::serve_client_port();
    let daemon = loopback_http_body(&host, port, "/health")
        .as_deref()
        .and_then(parse_daemon_live_socket);
    // (#2928 re-review, MF-B) A daemon started with `--port X` binds a socket
    // keyed to X, which `/health` on `serve.port` never sees: list the
    // home's sockets and probe each.
    let elsewhere: Vec<u16> = darkmux_flow::live::sockets_for_home(&darkmux_flow::live::live_home())
        .into_iter()
        .filter(|(p, path)| *p != port && darkmux_flow::live::probe_socket(path) == darkmux_flow::live::SocketState::Listening)
        .map(|(p, _)| p)
        .collect();
    classify_live_channel(c, socket.as_deref(), state, daemon.as_ref(), port, &elsewhere)
}

fn classify_live_channel(
    c: darkmux_types::config_access::LiveCadence,
    socket: Option<&std::path::Path>,
    state: Option<darkmux_flow::live::SocketState>,
    daemon: Option<&DaemonLiveSocket>,
    dispatch_port: u16,
    // Ports of OTHER sockets in this home a daemon is receiving on.
    elsewhere: &[u16],
) -> Check {
    use darkmux_flow::live::SocketState;
    let name = "live channel";
    let provenance = match c.source {
        darkmux_types::config_access::Source::Env => "from DARKMUX_LIVE_SAMPLE_MS env",
        darkmux_types::config_access::Source::Config => "from config.json",
        darkmux_types::config_access::Source::BuiltIn => "default",
    };
    let warn = |message: String, hint: String| Check { name: name.into(), status: Status::Warn, message, hint: Some(hint) };
    if !c.enabled() {
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: format!(
                "off ({provenance}): viewers see model state at the durable heartbeat \
                 cadence (2 s); runtime.live_sample_ms 0 turns the channel off"
            ),
            hint: None,
        };
    }
    let Some(socket) = socket else {
        return warn(
            format!("{} ms ({provenance}), but no private socket path is available: the darkmux home path is too long for a socket and the fallback directory is not private to this user. Viewers see 2 s heartbeats", c.effective_ms),
            "use a shorter DARKMUX_HOME, or set XDG_RUNTIME_DIR to a directory only you can read".into(),
        );
    };
    let listening = match state {
        Some(SocketState::Listening) => "a daemon is receiving",
        Some(SocketState::Stale) => "STALE: a socket file nobody reads",
        _ => "no daemon socket yet (start `darkmux serve`)",
    };
    let base = format!(
        "{} ms ({provenance}): model state and utility jobs reach this machine's viewers at \
         this cadence through the local daemon ({}, {listening}); never written to the flow \
         log, Redis or the audit chain. Durable heartbeats stay at 2 s",
        c.effective_ms,
        socket.display()
    );
    if state == Some(SocketState::Stale) {
        return warn(
            format!("{base}. A daemon was killed without removing its socket; every live sample is dropped"),
            "start `darkmux serve` (it replaces the stale socket), or delete the socket file named above".into(),
        );
    }
    if state != Some(SocketState::Listening) {
        if let Some(other) = elsewhere.first() {
            return warn(
                format!(
                    "{base}. A daemon IS receiving live samples, on a socket keyed to port {other} (it was started with `--port {other}`), but dispatches send to port {dispatch_port} (`serve.port`): its viewers get no live samples"
                ),
                format!(
                    "start the daemon without `--port` (it then uses serve.port {dispatch_port}), or make {other} the configured port: `darkmux config set serve.port {other}`"
                ),
            );
        }
    }
    if let Some(d) = daemon {
        // Same port (that is where doctor asked), different socket: the
        // daemon resolves a different darkmux home than this shell.
        if d.socket_id != darkmux_flow::live::socket_fingerprint(socket) {
            return warn(
                format!(
                    "{base}. The daemon on port {} bound a DIFFERENT socket: it runs with a different darkmux home (DARKMUX_HOME) than this shell, so dispatches from here reach no viewer",
                    d.socket_port
                ),
                "run the daemon and your dispatches with the same DARKMUX_HOME".into(),
            );
        }
        if !d.bound {
            return warn(
                format!("{base}. The daemon lost its socket to another process and receives no live samples"),
                "restart `darkmux serve`; if a second daemon runs on the same port, stop one".into(),
            );
        }
    }
    if c.clamped() {
        return warn(
            format!("{base}. Configured {} ms was clamped to {} ms", c.configured_ms, c.effective_ms),
            format!(
                "set runtime.live_sample_ms between {} and {} (or 0 for off): \
                 `darkmux config set runtime.live_sample_ms 250`",
                darkmux_types::config_access::LIVE_SAMPLE_MS_MIN,
                darkmux_types::config_access::LIVE_SAMPLE_MS_MAX
            ),
        );
    }
    Check { name: name.into(), status: Status::Pass, message: base, hint: None }
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
    // (#3074) `timeout_ms == 0` is an unbounded inactivity timeout: there is
    // no deadline for a rest to approach, and the runtime does not clamp.
    if timeout_ms != 0 && ms.saturating_mul(2) >= timeout_ms {
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

/// (#2413) Surface the singleton host-sampler lock's state —
/// `<darkmux-home>/liveness/host-sampler.lock` — the coordination file that
/// keeps exactly one machine.telemetry emitter alive per machine (the
/// daemon, or a dispatch process when no daemon runs).
///
/// Two outcomes, checked in this order:
/// 1. No lock file at all → Pass: nothing has sampled yet on this machine
///    (fresh install, or no daemon/dispatch has started one).
/// 2. Otherwise: stale (heartbeat older than 3x its own declared interval,
///    or its pid is dead) → Warn; else → Pass, naming the live holder.
///
/// (#2413 round 3 MF1) A THIRD outcome — Warn "two live pids", fed by a
/// contention marker every declined `try_acquire` used to write — is
/// RETIRED. It measured the wrong thing: a decline against a fresh lock
/// held by the DESIGNED sole emitter is the correct, healthy steady state,
/// not contention, and every acquisition attempt (including a caller's
/// very first one) is exactly that whenever a daemon already runs — so
/// the Warn fired on every dispatch start under a running daemon, reading
/// a healthy install as faulty. The file-based lock could only ever show
/// ONE current holder either way, so the marker never actually proved a
/// second emitter was active; deleting the channel loses no real signal.
/// (#2653) Surface `<darkmux-home>/liveness/`'s current heartbeat-file count
/// and the retention window pruning it. The growth this reports on (10,249
/// files, 40 MB on one laptop, oldest two months old) was invisible until an
/// operator went and looked by hand; this makes it visible from `darkmux
/// doctor` without leaving the machine. Counts only `<pid>.log` files (the
/// `host-sampler.lock` file living in the same directory is a different
/// mechanism, see `check_host_sampler` below, and is never counted here).
fn check_liveness_retention() -> Check {
    let name = "liveness retention";
    // (#2653 MUST FIX 2) One call resolves BOTH the number and its
    // provenance, so they can no longer disagree — this replaced two
    // separate hand-rolled reads (a raw env peek + a strict
    // `DarkmuxConfig::load_resolved()` field check) that used to go out of
    // sync the moment an UNRELATED known field elsewhere in `config.json`
    // was wrong-typed: the strict parse failed the WHOLE document, so this
    // check printed "(default)" while the actual prune pass — reading the
    // same file through `dispatch_liveness`'s own raw peek — kept pruning
    // on the operator's real, correctly-typed value the entire time.
    let (hours, source) = darkmux_types::config_access::liveness_retention_hours_with_source();
    let provenance = match source {
        darkmux_types::config_access::Source::Env => "from DARKMUX_LIVENESS_RETENTION_HOURS env",
        darkmux_types::config_access::Source::Config => "from config.json",
        darkmux_types::config_access::Source::BuiltIn => "default",
    };
    let dir = darkmux_types::config_access::liveness_dir();
    // (#2653 CONSIDER 10) `is_pid_log_file` is the SAME predicate
    // `dispatch_liveness`'s own prune pass applies — sharing it here means
    // this count can never drift from what actually gets pruned the way
    // MUST FIX 2/3's two resolvers already had.
    let count = std::fs::read_dir(&dir)
        .map(|entries| {
            entries.filter_map(|e| e.ok()).filter(|e| darkmux_types::dispatch_liveness::is_pid_log_file(&e.path())).count()
        })
        .unwrap_or(0);
    // (#2653 MUST FIX 6) `0` disables pruning entirely (this codebase's own
    // zero-means-off convention — `runtime.host_sampler_interval_ms`,
    // `redis.maxlen` — never "retain nothing"); warn loudly rather than
    // silently letting the directory grow unbounded with a Pass status.
    let (status, hint) = if hours == 0 {
        (
            Status::Warn,
            Some(
                "`runtime.liveness_retention_hours: 0` disables pruning (0 means \"off\", the \
                 same convention as `runtime.host_sampler_interval_ms` / `redis.maxlen` — never \
                 \"retain nothing\"), so this directory will grow unbounded. Set a real window \
                 in hours (e.g. 168 for 7 days) or remove the key for the default."
                    .to_string(),
            ),
        )
    } else {
        (Status::Pass, None)
    };
    Check {
        name: name.into(),
        status,
        message: format!(
            "{count} heartbeat file(s) in {} — retention {hours}h / {:.1}d ({provenance}); \
             pruned automatically as new dispatches write markers",
            dir.display(),
            hours as f64 / 24.0
        ),
        hint,
    }
}

fn check_host_sampler() -> Check {
    let name = "host sampler";
    let now_ms = darkmux_crew::host_sampler_lock::epoch_ms_now();
    let Some(state) = darkmux_crew::host_sampler_lock::read_lock() else {
        return Check {
            name: name.into(),
            status: Status::Pass,
            message: "no sampler active yet (no dispatch or daemon has started one on this machine)".into(),
            hint: None,
        };
    };
    let dead = !darkmux_crew::host_sampler_lock::pid_alive(state.pid);
    let stale = dead || darkmux_crew::host_sampler_lock::is_stale(&state, now_ms);
    if stale {
        Check {
            name: name.into(),
            status: Status::Warn,
            message: format!(
                "stale lock: pid {} ({}), last heartbeat {}ms ago (every {}ms), pid {} — will be \
                 reclaimed by the next sampler to start",
                state.pid,
                state.owner,
                now_ms.saturating_sub(state.heartbeat_ts_ms),
                state.interval_ms,
                if dead { "dead" } else { "alive" },
            ),
            hint: None,
        }
    } else {
        Check {
            name: name.into(),
            status: Status::Pass,
            message: format!("one sampler (pid {}, {}, every {}ms)", state.pid, state.owner, state.interval_ms),
            hint: None,
        }
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
///
///    **(#2836) This case is now NARROWER than it reads, and the narrowing
///    is worth stating rather than leaving the check to imply otherwise.**
///    On the STREAMING path the interval no longer rides out as
///    `max_tokens`: it is an observation cadence, and what bounds how long
///    one call may generate is `max_tokens_per_call`. So on that path this
///    arithmetic describes the wrong quantity — a call can legitimately run
///    to the ceiling regardless of the interval, and silence is caught by
///    the transport's read timeout, which produces an envelope instead of
///    killing the dispatch.
///
///    It is kept, and still computed on the interval, because the case it
///    was written for is the NON-streaming path (`--no-stream`), where the
///    interval IS still the wire cap and the #2171 incident is still
///    reachable exactly as described. A check that warns on the stricter of
///    the two paths is the safe direction: it can advise lowering an
///    interval that a streaming operator did not strictly need to lower,
///    and it cannot stay silent while a non-streaming one reproduces the
///    incident.
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
    // (#3074) `0` is unbounded: no budget for a generation to outlast.
    if inactivity_timeout_seconds != 0 && seconds_to_generate >= inactivity_timeout_seconds as f64 {
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
                "on the non-streaming path a single call may generate silently for \
                 ~{approx_seconds}s against an {inactivity_timeout_seconds}s inactivity \
                 budget; lower the interval or raise runtime.inactivity_timeout_seconds. \
                 (#2836: when streaming — the default — the interval is an observation \
                 cadence and does not bound a call, so this is advisory there)"
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
/// (#2947) `None` when `pause_at`/`resume_at` is bad config: that value is
/// reported ONCE, as Fail, by its generic enum-settings row, and there is no
/// ladder to describe (every run refuses at preflight).
fn check_thermal_governor() -> Option<Check> {
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
        return Some(Check {
            name: name.into(),
            status: Status::Pass,
            message: format!("disabled ({provenance}) — no thermal pausing or breaking"),
            hint: None,
        });
    }
    // (#2947) A bad `pause_at`/`resume_at` is reported ONCE, as Fail, by
    // its generic enum-settings row (`check_enum_settings`), which names the
    // value, where it was set and the valid values. This check describes
    // the ladder a VALID pair produces; with a bad one there is no ladder
    // to describe (every run refuses at preflight), so it adds no row.
    let (pause_at, resume_at) = match (
        darkmux_types::config_access::thermal_pause_at(),
        darkmux_types::config_access::thermal_resume_at(),
    ) {
        (Ok(p), Ok(r)) => (p.as_str().to_string(), r.as_str().to_string()),
        // One row per bad value (review): the enum-settings row is it.
        _ => return None,
    };
    let resume_hold_ms = darkmux_types::config_access::thermal_resume_hold_ms();
    let max_pause_ms = darkmux_types::config_access::thermal_max_pause_ms();
    let min_cpu = darkmux_types::config_access::thermal_min_cpu_speed_limit_pct();
    let speed_limit_hold_samples = darkmux_types::config_access::thermal_speed_limit_hold_samples();
    // (#2774) The escalation ladder's own three tier-2/3/4 knobs — surfaced
    // in the Pass message below so `darkmux doctor` answers "what would
    // actually happen" for the whole ladder, not just the pre-existing
    // pause/breaker half of it.
    let duty_delay_ms = darkmux_types::config_access::thermal_duty_delay_ms();
    let ratchet_factor = darkmux_types::config_access::thermal_ratchet_factor();
    let episode_threshold = darkmux_types::config_access::thermal_episode_threshold();
    let tier4_enabled = darkmux_types::config_access::thermal_tier4_enabled();

    // (#2774 round-8 MF1) The breaker's two CONFIGURABLE triggers, rendered
    // by what they DO at their degenerate values rather than by
    // interpolating the raw number into a sentence that then describes a
    // trigger which cannot fire.
    //
    // Both knobs degenerate, in opposite directions, and both used to be
    // spelled out raw here:
    //
    // - `max_pause_ms = 0` means an UNBOUNDED episode (round-6 MF2 —
    //   `pause_episode_exhausted` returns `false` forever at `0`), so the
    //   handoff never happens. Interpolated raw, doctor said "breaker after
    //   0ms of one pause episode" — the exact opposite, and in the same
    //   sentence where `episode_threshold = 0` IS correctly spelled out as
    //   unbounded.
    // - `min_cpu_speed_limit_pct = 0` disables the floor outright (the
    //   comparison is `pct < floor` and no reading is below zero — the
    //   honest reading of "no floor", and why the knob is not clamped; see
    //   `docs/ENVIRONMENT.md`). Interpolated raw, doctor said "3
    //   consecutive samples with cpu_speed_limit_pct < 0%".
    //
    // An operator who sets either one runs `doctor` to confirm what would
    // actually happen — that is what this surface is for (#44) — and was
    // told a false thing about their own config.
    let episode_handoff_clause = if max_pause_ms == 0 {
        "never from a pause episode (max_pause_ms=0 — unbounded, rest as long as it takes)"
            .to_string()
    } else {
        format!("after {max_pause_ms}ms of one pause episode")
    };
    let cpu_floor_clause = if min_cpu == 0 {
        "never from the CPU floor (min_cpu_speed_limit_pct=0 — no reading is below 0%)".to_string()
    } else if min_cpu > 100 {
        // (#2774 round-9 MF1) The third arm of the same family, and the
        // one whose absence made the DISARMED message below actively
        // misleading. `100` is the "no cap recorded" reading a HEALTHY
        // machine produces, so a floor above it makes `pct < floor` true
        // of every sample: the breaker trips on every dispatch, cold
        // machine included. Rendering the raw number here produced "and
        // after 3 consecutive samples with cpu_speed_limit_pct < 150%"
        // directly after the words "The breaker is unaffected and still
        // runs" — literally true, and read by an operator as ordinary
        // hardware protection. Rendered by what it DOES, exactly as the
        // two arms around it are.
        format!(
            "on EVERY dispatch of any machine, cold included, after its first \
             {speed_limit_hold_samples} sample(s) (min_cpu_speed_limit_pct={min_cpu} is above the \
             100% ceiling of the reading it is compared against, so no sample can be at or above \
             it)"
        )
    } else {
        format!(
            "after {speed_limit_hold_samples} consecutive samples with cpu_speed_limit_pct < \
             {min_cpu}%"
        )
    };

    // (#2774 round-4) EVERY arming verdict comes from the same value the
    // GOVERNOR runs on: `ThermalBands`, whose only band constructor
    // refuses a band no reading can satisfy and one that every reading
    // satisfies. Doctor RENDERS those refusals; it does not re-derive
    // them. Four verdicts this check used to compute for itself now
    // arrive from there:
    //
    // - an unrecognized token in either slot (review finding 6 — a typo
    //   that `darkmux config set` rejects going forward but a hand-edited
    //   config.json can still carry);
    // - `pause_at` not strictly more severe than `resume_at` (F6);
    // - `pause_at = "critical"` (round-3 C4), which never fires a soft
    //   tier because the breaker's own `critical` rule is evaluated FIRST
    //   on every sample, so the reading that would enter tier 3 has
    //   already tripped the breaker — and a Pass message saying "tier 4
    //   enabled after N episodes" under it is affirmatively wrong;
    // - `resume_at = "nominal"` (round-4 MF1), the mirror image at the
    //   other end of the enum, whose absence here is what let round 4's
    //   defect ship.
    //
    // This check used to derive its own verdicts from the raw thresholds,
    // and that is exactly how it came to report **Pass** on `pause_at =
    // fair, resume_at = nominal` — a config round 4 then proved wedges a
    // cold machine into a permanent, ratcheting turn delay — with a
    // committed test asserting the Pass was correct. A second
    // implementation of the arming rule is a second chance to get it
    // wrong, and the two cannot be kept in agreement by intention. There
    // is now one.
    //
    // The raw (un-lowercased) comparison the token check used to make
    // deliberately is preserved: `ThermalBands::resolve` matches
    // `THERMAL_STATES` exactly, so if `config_access`'s normalization ever
    // goes away, this warns (loud) instead of passing (silent).
    let bands = darkmux_crew::thermal_bands::ThermalBands::resolve(&pause_at, &resume_at);

    // (#2774 round-9, the sweep's third item) Tier 2's own degenerate
    // value, rendered by what it DOES like every clause above it.
    // `ThermalGovernor::current_duty_delay_ms` starts at `duty_delay_ms`
    // and the ratchet only ever MULTIPLIES it, so at `duty_delay_ms = 0`
    // the whole escalating back-off is a permanent no-op — `0 * factor`
    // is 0 for the life of the run. "starts at 0ms and ratchets x2 per
    // `serious` recovery" is literally true and reads as a live,
    // escalating tier. A note only: the arithmetic is deliberately left
    // alone (a governor whose duty delay is zero is a governor the
    // operator turned off at tier 2, which is a legitimate thing to want).
    let duty_cycle_clause = if duty_delay_ms == 0 {
        format!(
            "duty-cycle at `{resume_at}` is inert (duty_delay_ms=0 — the ratchet only \
             multiplies, so x{ratchet_factor} of 0 stays 0 for the life of the run; tier 2 adds \
             no delay)"
        )
    } else {
        format!(
            "duty-cycle at `{resume_at}` starts at {duty_delay_ms}ms and ratchets \
             x{ratchet_factor} per `serious` recovery"
        )
    };

    // (#2774 round-9 MF1) The three BREAKER-ONLY degenerate-knob checks,
    // collected HERE — ahead of the band-disarm branch — and emitted
    // together with it below.
    //
    // All three used to sit AFTER that branch, each with its own early
    // `return`, so ANY disarmed band suppressed all three. None of them
    // reads `pause_at` or `resume_at`: they are breaker concerns, and the
    // breaker is precisely the thing a band disarm does NOT touch, which
    // is what the disarm message says in so many words. The worst
    // combination that produced — a hand-edited `pause_at == resume_at`
    // plus `min_cpu_speed_limit_pct > 100` — told the operator "The
    // breaker is unaffected and still runs", which reads as ordinary
    // hardware protection, while withholding that the breaker had become
    // a trip-on-every-dispatch. Measured on that pair: the message named
    // the 150% floor as a live trigger and said nothing about the 100%
    // ceiling, and a `ThermalGovernor` with the same floor fed three
    // `nominal`/100 samples really does emit `Breaker { state: "nominal" }`.
    //
    // Collected rather than merely re-ordered, so neither verdict hides
    // the other in the opposite direction: doctor reports every reason
    // this config is wrong in ONE pass, the way the multi-note hint
    // already does for multiple band disarms (round-6 C2). This is the
    // same fix the `max_pause_ms == 0` / `min_cpu == 0` clauses above
    // already had; the gap was that it had not been applied uniformly
    // across this function's control flow.
    let speed_limit_hold_samples_raw =
        darkmux_types::config_access::thermal_speed_limit_hold_samples_raw();
    let ratchet_factor_raw = darkmux_types::config_access::thermal_ratchet_factor_raw();
    let mut knob_warnings: Vec<(String, String)> = Vec::new();

    // (#2774 round-8) The one knob whose comparison degenerates UPWARD
    // rather than downward — which is why the family had a hole here.
    // `cpu_speed_limit_pct` is a percentage, and `100` is what the probe
    // reports when no cap is recorded at all, so a floor ABOVE 100 makes
    // `pct < floor` true of every reading a healthy machine produces: the
    // breaker trips on the `speed_limit_hold_samples`'th sample of EVERY
    // dispatch and drops a `thermal-critical` STOP file on a cold machine
    // — a state word naming something that never happened, the same
    // failure round-6 MF2 ended at the other end of the range.
    // Warned rather than clamped, consistent with its siblings and with
    // `min_cpu = 0`'s own documented "no floor" reading: the operator owns
    // the value, doctor says what it will do (#44).
    //
    // First in the list because it is the only one of the three that
    // changes what the machine DOES on a cold boot; the other two are
    // knobs that quietly did not take.
    if min_cpu > 100 {
        knob_warnings.push((
            format!(
                "runtime.thermal.min_cpu_speed_limit_pct is {min_cpu}, above the 100% ceiling of \
                 the reading it is compared against (`cpu_speed_limit_pct`, where 100 means no \
                 cap recorded). Every sample is below this floor, so EVERY dispatch trips the \
                 breaker after its first {speed_limit_hold_samples} samples and drops a \
                 `thermal-critical` STOP file on a cold machine. Use a value in 1..=100, or 0 to \
                 disable the floor and leave the `critical`-state check as the only breaker \
                 trigger."
            ),
            "darkmux config set runtime.thermal.min_cpu_speed_limit_pct 50".to_string(),
        ));
    }

    // (N2, final re-check) An explicit `0` doesn't achieve "disable"
    // semantics — it's silently coerced to `1` by
    // `thermal_speed_limit_hold_samples`'s own `.max(1)` floor (a naive
    // `streak >= 0` would trip on EVERY sample instead, the opposite of
    // disable). Warn so the operator knows their `0` didn't do what it
    // looked like it would.
    if speed_limit_hold_samples_raw == 0 {
        knob_warnings.push((
            "runtime.thermal.speed_limit_hold_samples is 0 — coerced to 1 (trips on the first \
             low sample). There is no way to disable this signal via 0; disable the thermal \
             governor overall (runtime.thermal.enabled) if that's the intent."
                .to_string(),
            "darkmux config set runtime.thermal.speed_limit_hold_samples 1".to_string(),
        ));
    }

    // (#2774) Same shape as the speed-limit-hold-samples check above: a
    // configured `0` doesn't achieve "no growth" — it's silently coerced to
    // `1` by `thermal_ratchet_factor`'s own `.max(1)` floor, because a
    // literal `0` would ZERO the duty-cycle delay on the very first
    // `serious` recovery, defeating the ratchet's whole purpose (each
    // recovery should be MORE cautious than the last, never less).
    if ratchet_factor_raw == 0 {
        knob_warnings.push((
            "runtime.thermal.ratchet_factor is 0 — coerced to 1 (the duty-cycle delay holds \
             steady across a `serious` recovery instead of growing). A literal 0 would zero the \
             delay on the first escalation, which is never the intent; use 1 explicitly if \
             \"don't grow it\" is what you want."
                .to_string(),
            "darkmux config set runtime.thermal.ratchet_factor 1".to_string(),
        ));
    }

    if !bands.disarm_notes().is_empty() || !knob_warnings.is_empty() {
        let mut sentences: Vec<String> = Vec::new();
        // (#2774 round-6 C2) EVERY note's remedy, not just the first —
        // and, since round 9, every KNOB's remedy alongside them. The
        // message already concatenates every `why`; handing back one
        // remedy for two problems sends the operator round the loop — on
        // `pause_at = critical, resume_at = nominal` both tiers are
        // disarmed for different reasons, so fixing the first and
        // re-running doctor just produces the second warning. Deduped
        // because two notes can legitimately share a remedy (one `config
        // set` line resolving both), and printing it twice reads as two
        // steps.
        let mut remedies: Vec<String> = Vec::new();

        if !bands.disarm_notes().is_empty() {
            sentences.push(format!(
                "{} DISARMED — {} The breaker is unaffected and still runs: an OS-reported \
                 `critical` state (immediate, always), and {cpu_floor_clause}.",
                bands
                    .disarm_notes()
                    .iter()
                    .map(|n| n.tiers)
                    .collect::<Vec<_>>()
                    .join(" and "),
                bands
                    .disarm_notes()
                    .iter()
                    .map(|n| n.why.as_str())
                    .collect::<Vec<_>>()
                    .join(" Also: "),
            ));
            for note in bands.disarm_notes() {
                remedies.push(note.remedy.clone());
            }
        }
        for (sentence, remedy) in &knob_warnings {
            sentences.push(sentence.clone());
            remedies.push(remedy.clone());
        }

        let mut seen: Vec<String> = Vec::new();
        for remedy in remedies {
            if !seen.contains(&remedy) {
                seen.push(remedy);
            }
        }
        return Some(Check {
            name: name.into(),
            status: Status::Warn,
            message: sentences.join(" "),
            hint: Some(seen.join(" ")),
        });
    }

    Some(Check {
        name: name.into(),
        status: Status::Pass,
        message: format!(
            "enabled ({provenance}) — pause at `{pause_at}`, resume at `{resume_at}` held \
             {resume_hold_ms}ms; breaker on an OS-reported `critical` state (immediate, always), \
             {episode_handoff_clause}, and {cpu_floor_clause}; \
             {duty_cycle_clause}; tier 4 (indefinite, operator-gated pause) {}",
            if tier4_enabled {
                if episode_threshold == 0 {
                    "enabled but unbounded (episode_threshold=0 — never escalates)".to_string()
                } else {
                    format!("enabled after {episode_threshold} `serious` episode(s)")
                }
            } else {
                "disabled".to_string()
            }
        ),
        hint: None,
    })
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
    let workspaces = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser)
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
    // (#2779) The provenance is READ here and PASSED IN, not read inside
    // the renderer — same split, and the same reason, as `src`/`cost_ms`:
    // it resolves from a process-wide `OnceLock` over an env var, so a
    // renderer that read it itself could only ever be tested in the variant
    // the test process happened to resolve (`Real`, always), leaving the
    // one branch that matters pinned by nothing.
    describe_host_probe(probe.sources(), s.cost_ms, darkmux_crew::host_source::provenance().warning())
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
    // (#2779) `Some` when the thermal + battery readings are NOT this
    // machine's, or when a scenario was named and could not be loaded —
    // `host_source::Provenance::warning()` verbatim.
    simulated_warning: Option<String>,
) -> Check {
    let name = "host probe";
    let all = [
        ("mach", src.mach),
        ("ioreport", src.ioreport),
        ("freq-tables", src.freq_tables),
        ("thermal", src.thermal),
        ("ioreg-gpu", src.ioreg_gpu),
        // (#2705) Named like every other source, so a desktop's "no
        // battery" reads as a PROPERTY OF THE HOST rather than as a gap —
        // the same distinction this check exists to draw for `ioreport`.
        ("battery", src.battery),
    ];
    // (#2779) `simulated` QUALIFIES the two readings above rather than
    // being an independent source: on a scripted run `thermal` and
    // `battery` resolve because a scenario file supplied them, which on a
    // host that genuinely has neither (a Linux CI box, a desktop with no
    // battery) would otherwise read as "this machine has a thermal sensor".
    // Naming them in the list is what stops `sources` being quietly wrong
    // about the host while the warning below is right about the run.
    let all: Vec<(&str, bool)> = all
        .iter()
        .map(|(n, ok)| {
            let n = match (src.simulated, *n) {
                (true, "thermal") => "thermal(scenario)",
                (true, "battery") => "battery(scenario)",
                _ => n,
            };
            (n, *ok)
        })
        .collect();
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
    // (#2779) A simulated host source OUTRANKS every other verdict this
    // check can reach, including the healthy all-sources-resolved Pass. A
    // machine reporting `nominal` while it actually cooks is strictly worse
    // than no governor at all, so this is the loudest thing doctor can say
    // about the probe, and it is said FIRST — before a reader gets as far
    // as the source list, which on a scripted run describes a fiction.
    //
    // The message is `Provenance::warning()` verbatim, the same value the
    // dispatch prints at sampler start, so the two surfaces cannot
    // disagree — the shape the thermal ladder's own disarm notes already
    // use. `ScriptedUnavailable` renders here too (nothing is simulated,
    // but a named-and-unloadable scenario is not allowed to be silent
    // either).
    if let Some(warning) = simulated_warning {
        return Check {
            name: name.into(),
            status: Status::Warn,
            message: format!("{warning} (sources: {msg})"),
            hint: Some(
                "Unset DARKMUX_HOST_SOURCE_SCRIPT to read this machine's real thermal and \
                 battery state. The scenario facade exists so the thermal escalation ladder \
                 can be regression-tested against simulated hardware; it is not a runtime \
                 setting."
                    .into(),
            ),
        };
    }
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
fn check_unmanaged_endpoint_credentials() -> Check {
    let name = "unmanaged endpoint credentials";
    let registry = match profiles::load_registry(None) {
        Ok(r) => r,
        Err(e) => {
            return Check {
                name: name.into(),
                status: Status::Warn,
                message: format!(
                    "can't check unmanaged endpoint credentials (profile registry load failed: {e:#})"
                ),
                hint: None,
            };
        }
    };

    let mut problems: Vec<String> = Vec::new();
    let mut checked = 0usize;

    // A named endpoint is checked once, under its id.
    let mut seen_named: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for profile in registry.registry.profiles.values() {
        for model in &profile.models {
            let Some(ep) = model.endpoint.as_ref() else {
                continue;
            };
            let Some(id) = ep.named_id() else { continue };
            if !seen_named.insert(id.to_string()) {
                continue;
            }
            let subject = format!("endpoint `{id}`");
            let Some(auth) = ep.auth.as_ref() else {
                continue;
            };
            // (#2902 step 3) The order is `EndpointAuth::credential_source`,
            // the same one the dispatch reads the secret by: a declared env
            // var PRESENT here satisfies it (the headless runner sets it from
            // its secret store and the Keychain is never read, #1312).
            match auth.credential_source() {
                darkmux_types::CredentialSource::NoHeader => continue,
                darkmux_types::CredentialSource::Env(_) => checked += 1,
                darkmux_types::CredentialSource::Missing { key_env } => {
                    checked += 1;
                    let via = key_env
                        .map(|v| format!(" (declared env var `{v}` is not set in this environment)"))
                        .unwrap_or_default();
                    problems.push(format!(
                        "{subject}: endpoint.auth.type is set \
                         but no credential source resolved: set endpoint.auth.keychain or \
                         export endpoint.auth.key_env{via}"
                    ));
                }
                darkmux_types::CredentialSource::Keychain(keychain) => {
                    checked += 1;
                    if !keychain_item_present(keychain) {
                        problems.push(format!("{subject}: Keychain item `{keychain}` not found on this machine"));
                    }
                }
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

/// (#2902 steps 4 and 5) `endpoints`: what the registry's `endpoints` map
/// declares, one line per endpoint (what darkmux does there, the request
/// dialect, where the credential lives by NAME, its limits and its budget).
/// For a budget that
/// counts, the line shows its policy and the spend so far in its rolling
/// window (this machine's usage records, read through the same window
/// reader the gate uses). An unregistered budget `policy` is Fail: every
/// dispatch, mission launch and lab run refuses it at preflight.
fn check_endpoints() -> Check {
    match profiles::load_registry(None) {
        Ok(l) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let mut ledger = darkmux_crew::budget::Ledger::new(darkmux_types::config_access::flows_dir());
            endpoints_status(&l.registry, &mut |b| ledger.window(&b.endpoint_id, now, b.window.period_secs))
        }
        Err(e) => Check {
            name: "endpoints".into(),
            status: Status::Warn,
            message: format!("can't list endpoints (profile registry load failed: {e:#})"),
            hint: None,
        },
    }
}

/// The per-dispatch cap half of an endpoint's line (#3035), empty when none.
fn dispatch_cap_note(limits: Option<&darkmux_types::UsageLimits>) -> String {
    let Some(cap) = limits.and_then(|l| l.dispatch_cap()) else { return String::new() };
    let policy = limits.and_then(|l| l.resolved_policy().ok()).unwrap_or(darkmux_types::BudgetPolicy::Off);
    format!("; per-dispatch cap {cap} tokens ({})", if policy.counts() { "warn" } else { "off" })
}

/// How many of an endpoint's calls run at once (#3035): the scheduler's call
/// on a managed endpoint, the declared `concurrent_calls` (one at a time when
/// absent) on an unmanaged one.
fn concurrency_note(kind: darkmux_types::EndpointKind, limits: Option<&darkmux_types::UsageLimits>) -> String {
    match (kind.is_managed(), limits.and_then(|l| l.concurrent_calls)) {
        (true, _) => "; parallelism is the scheduler's".to_string(),
        (false, None) => "; calls run one at a time (no limits.concurrent_calls)".to_string(),
        (false, Some(0)) => "; calls run unbounded in parallel (limits.concurrent_calls 0)".to_string(),
        (false, Some(n)) => format!("; up to {n} calls at once"),
    }
}

/// The limits half of one endpoint's line (#3035): the per-dispatch cap, how
/// many of its calls run at once, and the window budget with its spend.
fn endpoint_budget_note(
    id: &str,
    ep: &darkmux_types::ModelEndpoint,
    spend: &mut dyn FnMut(&darkmux_crew::budget::EndpointBudget) -> darkmux_crew::budget::WindowEntries,
) -> String {
    let Ok(kind) = ep.kind() else { return String::new() };
    let limits = ep.known_limits();
    let mut notes = dispatch_cap_note(limits);
    notes.push_str(&concurrency_note(kind, limits));
    let Some(limits) = limits else { return notes };
    let window_set = limits.window.as_ref().is_some_and(|w| w.is_set());
    let mut named = ep.clone();
    named.source = darkmux_types::EndpointSource::Named(id.to_string());
    notes.push_str(&match darkmux_crew::budget::EndpointBudget::of(&named) {
        Err(_) => String::new(), // the Fail row names it
        Ok(None) => match limits.resolved_policy() {
            Ok(darkmux_types::BudgetPolicy::Off) if window_set => "; window budget off (nothing is counted)".to_string(),
            _ if window_set => "; window budget unusable (its period does not parse)".to_string(),
            _ => "; no window budget".to_string(),
        },
        Ok(Some(b)) => {
            let entries = spend(&b);
            let tokens: u64 = entries.iter().map(|(_, s)| s.known).sum();
            // A call with an unknown spend is never read as small: the
            // figure becomes a floor, and says why.
            let unmetered = entries.iter().filter(|(_, s)| !s.metered).count();
            let spent = match unmetered {
                0 => format!("spent {tokens} tokens"),
                n => format!("spent at least {tokens} tokens ({n} with an unknown spend)"),
            };
            let policy = darkmux_types::config_enum::ConfigEnum::token(b.policy);
            let warn_at = b.warn_at.map(|f| format!(", early warning at {:.0}%", f * 100.0)).unwrap_or_default();
            format!("; budget {policy}{warn_at}: {spent} in {} calls over the last {}", entries.len(), b.period)
        }
    });
    notes
}

/// Pure decision for [`check_endpoints`]; `spend` reads one budget's window.
fn endpoints_status(
    registry: &darkmux_types::ProfileRegistry,
    spend: &mut dyn FnMut(&darkmux_crew::budget::EndpointBudget) -> darkmux_crew::budget::WindowEntries,
) -> Check {
    let name = "endpoints".to_string();
    let bad = darkmux_types::config_enum::bad_endpoint_budget_policies(registry);
    let invalid = darkmux_types::config_enum::invalid_endpoint_limits(registry);
    if !bad.is_empty() || !invalid.is_empty() {
        let mut problems: Vec<String> = bad.iter().map(|b| format!("{}; {}", b.summary(), b.valid_line())).collect();
        problems.extend(invalid.iter().map(|v| format!("{}: {}", v.set_in, v.problem)));
        let mut fixes: Vec<String> = bad.iter().map(|b| b.fix()).collect();
        if let Some(v) = invalid.first() {
            fixes.push(format!("write `limits` in the valid shape, {}", v.valid));
        }
        return Check {
            name,
            status: Status::Fail,
            message: problems.join("; "),
            hint: Some(format!(
                "Every dispatch, mission launch and lab run refuses to start until this is fixed: {}. (#2902)",
                fixes.join("; ")
            )),
        };
    }
    let mut lines: Vec<String> = Vec::new();
    for (id, ep) in &registry.endpoints {
        let kind = match ep.kind() {
            Ok(darkmux_types::EndpointKind::Managed(darkmux_types::ManagedBackend::Lmstudio)) => {
                "managed (lmstudio)".to_string()
            }
            Ok(darkmux_types::EndpointKind::Unmanaged) => {
                format!("unmanaged, {}", ep.host().unwrap_or_else(|| "no host".to_string()))
            }
            Err(e) => format!("unusable: {e}"),
        };
        let dialect = ep.resolved_dialect().map(|d| d.as_str()).unwrap_or("?");
        let credential = match ep.auth.as_ref().map(|a| a.credential_source()) {
            None | Some(darkmux_types::CredentialSource::NoHeader) => "no auth header".to_string(),
            Some(darkmux_types::CredentialSource::Env(v)) => format!("credential from env `{v}`"),
            Some(darkmux_types::CredentialSource::Keychain(k)) => format!("credential from Keychain `{k}`"),
            Some(darkmux_types::CredentialSource::Missing { .. }) => "credential unresolved".to_string(),
        };
        let limits = Some(ep.limits_summary())
            .filter(|s| !s.is_empty())
            .map(|s| format!("; limits {s}"))
            .unwrap_or_default();
        let budget = endpoint_budget_note(id, ep, spend);
        lines.push(format!("`{id}`: {kind}, {dialect}, {credential}{limits}{budget}"));
    }
    let listed = if lines.is_empty() {
        "no `endpoints` declared".to_string()
    } else {
        format!("{} endpoint(s): {}", lines.len(), lines.join("; "))
    };
    Check { name, status: Status::Pass, message: listed, hint: None }
}

/// (#1177) Live endpoint probes — NOT part of [`run`]'s offline check set.
/// Opt-in via `darkmux doctor --probe` because each probe is a real API
/// call: a paid endpoint bills a few tokens per probe. The offline
/// `unmanaged endpoint credentials` check proves the Keychain item EXISTS;
/// this proves the whole chain WORKS — DNS, TLS, credential validity,
/// deployment routing, api-version — by driving one minimal chat
/// completion through the exact URL/auth/POST path a real hosted
/// dispatch uses. One probe per distinct (url, model) pair: profiles
/// that share an endpoint declaration are probed once, not billed once
/// per profile.
pub fn probe_unmanaged_endpoints() -> Vec<Check> {
    const PROBE_TIMEOUT_SECONDS: u32 = 30;
    let registry = match profiles::load_registry(None) {
        Ok(r) => r,
        Err(e) => {
            return vec![Check {
                name: "probe: unmanaged endpoints".into(),
                status: Status::Warn,
                message: format!(
                    "can't probe remote endpoints (profile registry load failed: {e:#})"
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
            // (#2902) Only an unmanaged endpoint is probed; an unresolvable
            // one is the `profile registry` check's finding.
            if !matches!(ep.kind(), Ok(darkmux_types::EndpointKind::Unmanaged)) {
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
            match darkmux_crew::dispatch_internal::probe_unmanaged_endpoint(
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
            name: "probe: unmanaged endpoints".into(),
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
        // (#3035) A state a newer darkmux wrote: neither a pass nor a failure.
        darkmux_flow::HealthState::Unknown => Check {
            name: "flow sink health".into(),
            status: Status::Warn,
            message: format!("{composition} · the sink health state is one this darkmux does not know"),
            hint: Some("Upgrade darkmux, then run `darkmux flow status` for full detail.".into()),
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
    role_prompt_coverage_status(&missing)
}

/// Pure decision for [`check_crew_role_prompt_coverage`], split out so the
/// missing-prompt arm (and its hint) is testable while every shipped prompt
/// is present (#3081).
fn role_prompt_coverage_status(missing: &[&str]) -> Check {
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
            hint: Some(format!(
                "Author the missing prompts at `templates/builtin/roles/<id>.md` and \
                 add them to `BUILTIN_ROLE_PROMPTS` in `crates/darkmux-crew/src/loader.rs`. Operators can \
                 override at `{}/<id>.md`.",
                home_display(&darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).root.join("roles"))
            )),
        }
    }
}

/// A path for operator-facing text: the home prefix prints as `~` so doctor
/// output pasted into an issue does not carry the account name (#3081). The
/// resolved location is unchanged, and a path outside home prints in full.
fn home_display(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(&h).ok().map(|r| r.to_path_buf())) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
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
    let user_dir = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser)
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
///
/// (#2782 C4) Three spellings of "us" are accepted, not two: loopback,
/// `localhost`, and the daemon's own resolved `bind` host. An operator who
/// bound one specific interface will have written `tailscale serve` against
/// THAT address, and matching only the loopback spellings would report "no
/// tailnet URL" for a proxy that is in fact pointed straight at this daemon.
/// A wildcard bind collapses back to loopback before it gets here
/// (`format_client_addr`), so it adds no fourth case.
fn parse_tailnet_viewer_url(json: &str, bind: &str, port: u16) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let web = v.get("Web")?.as_object()?;
    let want_loopback = format!("http://127.0.0.1:{port}");
    let want_localhost = format!("http://localhost:{port}");
    let want_bind = format!(
        "http://{}",
        darkmux_types::config_access::format_client_addr(bind, port)
    );
    for (hostport, cfg) in web {
        let proxies_to_us = cfg
            .get("Handlers")
            .and_then(|h| h.as_object())
            .map(|handlers| {
                handlers.values().any(|h| {
                    h.get("Proxy")
                        .and_then(|p| p.as_str())
                        .map(|p| p == want_loopback || p == want_localhost || p == want_bind)
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
fn tailnet_viewer_url(bind: &str, port: u16) -> Option<String> {
    tailnet_viewer_url_bounded(bind, port, TAILNET_PROBE_TIMEOUT)
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
fn tailnet_viewer_url_bounded(
    bind: &str,
    port: u16,
    timeout: std::time::Duration,
) -> Option<String> {
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
    parse_tailnet_viewer_url(&String::from_utf8_lossy(&out.stdout), bind, port)
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
///
/// **The direct fallback honors `serve.bind`, not a hardcoded loopback**
/// (#2782 C4). #2765 made this take the resolved PORT but left the HOST a
/// literal `127.0.0.1`, which closed the defect halfway: with a
/// non-loopback bind, doctor's own `serve address` row named that address
/// while every link this function produced pointed at loopback, so the two
/// rows disagreed and the clickable link went somewhere nothing is
/// listening. `config_access::format_client_addr` is the one place that
/// answers "what does a client on this machine connect to", wildcard rule
/// included (`0.0.0.0` is a bind directive, not a destination, so it still
/// resolves back to loopback here).
pub fn viewer_link_base(port: u16) -> String {
    let bind = darkmux_types::config_access::serve_bind();
    let direct = format!(
        "http://{}/",
        darkmux_types::config_access::format_client_addr(&bind, port)
    );
    if !darkmux_types::style::colorize_enabled() {
        return direct;
    }
    match darkmux_types::config_access::fleet_mode() {
        Ok(darkmux_types::config::FleetMode::Standalone) => direct,
        Ok(_) => tailnet_viewer_url(&bind, port).unwrap_or(direct),
        // (#2947) Not read as `standalone` silently: this renders links, it
        // starts no work, so it says what is wrong and uses the direct
        // address (the one link that is true whatever the fleet position).
        // `darkmux doctor` reports the same value as Fail.
        Err(bad) => {
            eprintln!("{}", darkmux_types::style::warn(&format!("{bad} (viewer links use the direct address)")));
            direct
        }
    }
}

/// (#2765) Where the RESOLVED daemon address came from, rendered for the
/// `serve address` row: `env` names the variable, `config.json` names the
/// key, and the built-in tier says so plainly.
///
/// Both halves are resolved independently because they are independent
/// knobs — an operator with `DARKMUX_SERVE_PORT` exported and `serve.bind`
/// in their config has two different provenances at once, and a single
/// blended label would have to lie about one of them.
fn serve_address_provenance() -> String {
    use darkmux_types::config_access::Source;
    let port_src = match darkmux_types::config_access::serve_port_with_source().1 {
        Source::Env => "port from DARKMUX_SERVE_PORT env",
        Source::Config => "port from config.serve.port",
        Source::BuiltIn => "port default",
    };
    let bind_src = match darkmux_types::config_access::serve_bind_with_source().1 {
        Source::Env => "bind from DARKMUX_SERVE_BIND env",
        Source::Config => "bind from config.serve.bind",
        Source::BuiltIn => "bind default",
    };
    format!("{port_src}, {bind_src}")
}

/// (#2765) The resolved daemon listen address, with the tier each half came
/// from. A pure config read — it never touches the network, so it answers
/// even when the daemon is down, which is exactly when the question gets
/// asked.
///
/// This row exists because the failure it describes is INVISIBLE from the
/// host: on 2026-09-16 a daemon was serving happily on the operator's
/// configured port while every client probed the built-in 8765, and
/// diagnosing it cost several minutes of probing ports by hand. One line
/// naming the resolved address and where it came from answers it outright.
/// Separate from `DAEMON_CHECK_NAME` (reachability) on purpose: "what
/// address is configured" and "is anything answering there" are different
/// questions, and collapsing them is what made the first one unanswerable
/// while the second was failing.
///
/// **It prints the LISTEN address, and appends the client-probe address
/// only when the two differ** (#2782 MF2). The first cut printed
/// `serve_client_addr()` under a label built from `serve_bind_with_source()`
/// — fine while the bind was loopback, and inverted the moment it was not:
/// with `DARKMUX_SERVE_BIND=0.0.0.0` the row rendered
/// `127.0.0.1:8765 (port default, bind from DARKMUX_SERVE_BIND env)`, so
/// the value the operator had just set appeared NOWHERE in the output of
/// the row added specifically so provenance is never in doubt. An operator
/// debugging "why can't the tailnet reach my daemon" reads that as their
/// bind not having taken. Both addresses are real and both are wanted; the
/// row names which is which rather than silently picking one.
fn check_serve_address() -> Check {
    let listen = darkmux_types::config_access::serve_listen_addr();
    let client = darkmux_types::config_access::serve_client_addr();
    let provenance = serve_address_provenance();
    // The common case (a specific bind) has one address and says so once.
    // A wildcard bind has two, and both matter: the first answers "did my
    // bind take", the second answers "where do checks on this machine go".
    let message = if listen == client {
        format!("{listen} ({provenance})")
    } else {
        format!("{listen} ({provenance}) · clients on this machine probe {client}")
    };
    Check {
        name: "serve address".into(),
        status: Status::Pass,
        message,
        hint: None,
    }
}

fn check_daemon_reachable() -> Check {
    // (#2765) Probe the RESOLVED address, not a hardcoded literal. Probing
    // 127.0.0.1:8765 while the daemon listens on the operator's configured
    // port is the exact asymmetry the issue was filed about — doctor would
    // have reported "daemon not reachable" about a perfectly healthy
    // daemon, which is worse than not checking.
    //
    // `serve_client_addr` resolves a wildcard bind to loopback for us, so a
    // daemon bound to every interface is probed somewhere it is actually
    // listening rather than at the unroutable `0.0.0.0`.
    //
    // The resolver prefers the running daemon's own record, so a daemon
    // started with `--port N` is found; the row names which source it used.
    let endpoint = darkmux_types::config_access::serve_client_endpoint();
    let host = endpoint.addr.rsplit_once(':').map(|(h, _)| h).unwrap_or("127.0.0.1");
    let mut check = check_daemon_reachable_impl(host, endpoint.port);
    check.message = format!("{} · address {}", check.message, endpoint.source.describe());
    check
}

/// (#1665) Whether a raw HTTP response's body parses as JSON carrying a
/// `darkmux_version` key — the one field `darkmux_serve::health` always
/// emits. Best-effort on purpose: a malformed/oversized body just reads as
/// "not darkmux" (`false`) rather than panicking the check.
fn response_names_darkmux(response: &str) -> bool {
    let Some(body_start) = response.find("\r\n\r\n") else { return false };
    let body = &response[body_start + 4..];
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("darkmux_version").cloned())
        .is_some()
}

/// (#1665 review CONSIDER 3) Byte offset just past the header/body
/// separator (`\r\n\r\n`), or `None` when the headers haven't fully
/// arrived in `data` yet.
fn http_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Parse `Content-Length` out of a header block (case-insensitive header
/// name, per HTTP/1.1 — real servers vary casing).
fn http_content_length(headers: &str) -> Option<usize> {
    headers
        .lines()
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length")))
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// (#1665 review CONSIDER 3) Read a full HTTP response off `stream`,
/// looping across multiple TCP reads rather than trusting one 1 KiB read
/// to have captured everything. A real server that flushes headers and
/// writes the body moments later (two TCP segments, not one) used to be
/// mistaken for a "200 with no darkmux identity" port squatter — the
/// second read that would have carried the body never happened. Stops
/// when: (a) the peer closes the connection (`Connection: close`, which
/// this probe's request always sends, makes EOF the normal end-of-response
/// signal), (b) the headers are in AND a `Content-Length` says the body is
/// fully buffered, or (c) a total-size/deadline bound is hit — a
/// misbehaving or hostile responder must not hang `darkmux doctor`
/// indefinitely. Whatever arrived before any of those is what gets
/// returned; a read timeout or error is not itself a failure here, since
/// the per-read socket timeout set by the caller already guards against a
/// truly silent peer.
fn read_full_http_response(stream: &mut std::net::TcpStream) -> Vec<u8> {
    const MAX_RESPONSE_BYTES: usize = 64 * 1024;
    const TOTAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
    let deadline = std::time::Instant::now() + TOTAL_BUDGET;

    let mut data: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if std::time::Instant::now() >= deadline || data.len() >= MAX_RESPONSE_BYTES {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break, // peer closed — the normal end for `Connection: close`
            Ok(n) => {
                data.extend_from_slice(&chunk[..n]);
                if let Some(header_end) = http_header_end(&data) {
                    let headers = String::from_utf8_lossy(&data[..header_end]);
                    match http_content_length(&headers) {
                        Some(len) if data.len() >= header_end + len => break,
                        Some(_) => continue, // headers in, body still arriving
                        None => continue,    // no Content-Length — wait for EOF/timeout
                    }
                }
            }
            Err(_) => break, // timeout or error — use whatever arrived so far
        }
    }
    data
}

/// Core implementation that takes host/port so tests can inject mock servers.
fn check_daemon_reachable_impl(host: &str, port: u16) -> Check {
    let addr = format!("{}:{}", host, port);

    // Use a short timeout since this is local loopback.
    //
    // (#2765) A literal IP parses directly; a NAME (`localhost`, a
    // `.tailnet.ts.net` host) needs resolution. Before the bind address
    // became configurable every caller passed `127.0.0.1`, so the parse
    // could not fail; now an operator may legitimately write
    // `serve.bind: "localhost"`, and reporting their own config back as
    // "invalid address" would be a false Warn about a working daemon.
    let addr_parsed = match addr.parse() {
        Ok(a) => a,
        Err(_) => {
            use std::net::ToSocketAddrs;
            match addr.to_socket_addrs().ok().and_then(|mut it| it.next()) {
                Some(a) => a,
                None => {
                    return Check {
                        name: DAEMON_CHECK_NAME.into(),
                        status: Status::Warn,
                        message: format!("invalid address {}", addr),
                        hint: None,
                    };
                }
            }
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

    // (#1665 review CONSIDER 3) Read the FULL response, not just whatever
    // fit in one TCP segment — see `read_full_http_response`'s doc for the
    // headers-then-body-later gap this closes.
    let data = read_full_http_response(&mut stream);
    let n = data.len();
    let response = String::from_utf8_lossy(&data);
    if n == 0 {
        return Check {
            name: DAEMON_CHECK_NAME.into(),
            status: Status::Warn,
            message: format!("daemon at {} not responding to HTTP", addr),
            hint: Some("run `darkmux serve` to start the daemon for live viewing features".into()),
        };
    }

    if response.starts_with("HTTP/1.1 200") && response_names_darkmux(&response) {
        // Surface WHERE to open the viewer, not just that the daemon answers:
        // the loopback URL (this machine) + the tailnet URL (phone / other
        // tailnet device) when `tailscale serve` is proxying to this daemon.
        let mut message = format!("reachable · viewer http://{addr}/");
        if let Some(tn) = tailnet_viewer_url(host, port) {
            message.push_str(&format!(" · phone {tn}"));
        }
        Check {
            name: DAEMON_CHECK_NAME.into(),
            status: Status::Pass,
            message,
            hint: None,
        }
    } else if response.starts_with("HTTP/1.1 200") {
        // (#1665) A 200 alone is not identity: anything answering on this
        // port with a 200 — a dev server, a stray `python -m http.server`,
        // another operator's process that happened to grab 8765 — used to
        // read as "the viewer is reachable" with zero verification that it
        // was actually darkmux on the other end. `/health`'s body always
        // carries `darkmux_version` (`darkmux_serve::health`); its absence
        // means this is a port squatter, not the daemon. Describes what was
        // observed, not a verdict about what's actually listening there.
        Check {
            name: DAEMON_CHECK_NAME.into(),
            status: Status::Warn,
            message: format!(
                "something answered 200 at {addr}/health but the body doesn't look like \
                 darkmux's — no `darkmux_version` field. Possibly another process holding \
                 this port."
            ),
            // (#2765) Name the port that was ACTUALLY probed. A hint telling
            // the operator to `lsof -i :8765` when the probe went to the port
            // their config names sends them to look at the wrong port —
            // the same mismatch this issue is about, one layer down.
            hint: Some(format!(
                "run `darkmux serve` on a free port, or check what's already listening on \
                 {port} (`lsof -i :{port}`)"
            )),
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
            hint: Some(format!(
                "ensure `darkmux serve` is running (port {port} may be held by another process)"
            )),
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

/// What a running daemon told us about itself on `/health` (#1461).
#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonBuild {
    /// `build` — package version PLUS git short SHA, so two daemons built
    /// from different commits at the same package version are
    /// distinguishable.
    build: String,
    /// `binary_mtime` — when the binary the daemon loaded was last written.
    /// `None` from a daemon that couldn't stat its own exe.
    binary_mtime: Option<u64>,
}

/// Pull the running daemon's build identity out of a `/health` body. `None`
/// when there is none to read, including a pre-#1461 daemon that reports no
/// `build` field.
fn parse_daemon_build(health_body: &str) -> Option<DaemonBuild> {
    let v: serde_json::Value = serde_json::from_str(health_body).ok()?;
    let build = v.get("build").and_then(|b| b.as_str())?;
    Some(DaemonBuild {
        build: build.to_string(),
        binary_mtime: v.get("binary_mtime").and_then(|m| m.as_u64()),
    })
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
    // (#2765) Same locator the reachability check uses — and it is now a
    // RESOLVER, not a literal. The comment this replaces said "there is no
    // port resolver in the codebase to reuse; both checks hardcode it
    // identically", which was true and was the bug: identical hardcoding
    // across every client is exactly how a daemon on a configured port
    // became invisible to all of them at once.
    let addr = darkmux_types::config_access::serve_client_addr();
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or("127.0.0.1").to_string();
    let running = loopback_http_body(&host, darkmux_types::config_access::serve_client_port(), "/health")
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
        // A pre-#1461 daemon (no build id) lands here too: the reachability
        // check still reports that it answered.
        return not_applicable(
            DAEMON_FRESHNESS_CHECK_NAME,
            "no darkmux serve daemon reporting a build id on this machine",
        );
    };
    let warn = |message: String| Check {
        name: DAEMON_FRESHNESS_CHECK_NAME.into(),
        status: Status::Warn,
        message,
        hint: restart_daemon_hint(),
    };
    match running {
        DaemonBuild { build, .. } if build != installed_build => warn(format!(
            "a darkmux serve daemon is running a DIFFERENT build ({build}) than this binary \
             ({installed_build}) — it serves its in-memory code until restarted, so anything you \
             verify against it is testing that build, not this one"
        )),
        // Same build tag. That is NOT yet a pass: on a dev box the tag is the
        // same commit-plus-dirty-marker before and after a reinstall from an
        // uncommitted tree, so the binary can have been replaced underneath a
        // still-running daemon without the tag moving at all.
        DaemonBuild {
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
        DaemonBuild { build, .. } => Check {
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

/// One local tag of the `darkmux-runtime` repository and its version label.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalRuntimeTag {
    tag: String,
    label: Option<String>,
}

/// What doctor could learn about the local `darkmux-runtime` images.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeImageProbe {
    /// Docker absent, wedged, daemon down, or no local `darkmux-runtime` tag.
    /// Never a warning: docker is not a hard dependency of doctor, and plenty
    /// of users have none.
    NotApplicable(String),
    /// Every local `darkmux-runtime:<tag>` with its version label.
    Tags(Vec<LocalRuntimeTag>),
}

fn probe_runtime_image() -> RuntimeImageProbe {
    use darkmux_crew::runtime_image::{RUNTIME_IMAGE_REPO, RUNTIME_IMAGE_VERSION_LABEL};
    let timeout = std::time::Duration::from_secs(5);
    let Some(list) = bounded_output(
        Command::new("docker").args(["images", RUNTIME_IMAGE_REPO, "--format", "{{.Repository}}:{{.Tag}}"]),
        timeout,
    ) else {
        return RuntimeImageProbe::NotApplicable("`docker` not available".into());
    };
    if !list.status.success() {
        // Daemon not reachable; `docker runtime` already reports daemon health.
        return RuntimeImageProbe::NotApplicable("the Docker daemon did not answer".into());
    }
    let tags = parse_runtime_image_tags(&String::from_utf8_lossy(&list.stdout));
    if tags.is_empty() {
        return RuntimeImageProbe::NotApplicable(format!("no local `{RUNTIME_IMAGE_REPO}` image"));
    }
    // One inspect for every tag: one output line per ref, in order.
    let format = format!("{{{{index .Config.Labels \"{RUNTIME_IMAGE_VERSION_LABEL}\"}}}}");
    let Some(out) = bounded_output(
        Command::new("docker")
            .args(["image", "inspect", "--format", &format, "--"])
            .args(&tags),
        timeout,
    ) else {
        return RuntimeImageProbe::NotApplicable("`docker image inspect` did not answer".into());
    };
    match pair_runtime_image_labels(&tags, &String::from_utf8_lossy(&out.stdout)) {
        Some(paired) if out.status.success() => RuntimeImageProbe::Tags(paired),
        _ => RuntimeImageProbe::NotApplicable("could not read the local images' labels".into()),
    }
}

/// `docker images <repo> --format '{{.Repository}}:{{.Tag}}'` → the tagged refs.
/// Dangling `<none>` entries have no tag to run and are dropped.
fn parse_runtime_image_tags(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains("<none>"))
        .map(String::from)
        .collect()
}

/// Pair each tag with its line of the batched inspect output. `None` when the
/// line count does not match, so a partial answer is never misattributed.
fn pair_runtime_image_labels(tags: &[String], inspect_stdout: &str) -> Option<Vec<LocalRuntimeTag>> {
    let lines: Vec<&str> = inspect_stdout.lines().collect();
    (lines.len() == tags.len()).then(|| {
        tags.iter()
            .zip(lines)
            .map(|(tag, line)| LocalRuntimeTag {
                tag: tag.clone(),
                label: darkmux_crew::runtime_image::parse_version_label(line),
            })
            .collect()
    })
}

fn check_runtime_image_freshness() -> Check {
    // (#2923) Unit tests that run the whole doctor must never reach the
    // host's Docker; the classifier is tested directly.
    if cfg!(test) {
        return not_applicable(RUNTIME_IMAGE_CHECK_NAME, "Docker is not probed from unit tests");
    }
    classify_runtime_image_freshness(probe_runtime_image(), env!("CARGO_PKG_VERSION"))
}

/// Pure classifier (#1461, #2923). Only `:latest` can be picked without being
/// named, so only a `:latest` that does not match this binary is a warning:
/// this darkmux skips it, but a pre-#2923 darkmux on the same machine (a
/// side-by-side install) still runs it, and naming it is refused. Other
/// unlabeled tags are listed, not warned about: they run only when named, and
/// naming one is refused with the fix.
fn classify_runtime_image_freshness(probe: RuntimeImageProbe, installed: &str) -> Check {
    use darkmux_crew::runtime_image::{
        describe_non_match, image_verdict, pinned_runtime_image, rebuild_command, ImageVerdict,
        RUNTIME_IMAGE,
    };
    let tags = match probe {
        RuntimeImageProbe::NotApplicable(reason) => {
            return not_applicable(RUNTIME_IMAGE_CHECK_NAME, &reason);
        }
        RuntimeImageProbe::Tags(tags) => tags,
    };
    let pinned = pinned_runtime_image(installed);
    let unlabeled_others: Vec<&str> = tags
        .iter()
        .filter(|t| t.tag != RUNTIME_IMAGE && t.label.is_none())
        .map(|t| t.tag.as_str())
        .collect();
    let others_note = match unlabeled_others.len() {
        0 => String::new(),
        n => {
            let shown: Vec<&str> = unlabeled_others.iter().take(3).copied().collect();
            format!(
                " · {n} other local tag(s) carry no version label ({}{}); `--image` naming one \
                 is refused",
                shown.join(", "),
                if n > 3 { ", …" } else { "" }
            )
        }
    };
    let latest = tags.iter().find(|t| t.tag == RUNTIME_IMAGE);
    match latest.map(|t| image_verdict(RUNTIME_IMAGE, t.label.as_deref(), installed)) {
        None => Check {
            name: RUNTIME_IMAGE_CHECK_NAME.into(),
            status: Status::Pass,
            message: format!(
                "no local `{RUNTIME_IMAGE}`: dispatch uses the version-pinned `{pinned}`{others_note}"
            ),
            hint: None,
        },
        Some(ImageVerdict::Matches) => Check {
            name: RUNTIME_IMAGE_CHECK_NAME.into(),
            status: Status::Pass,
            message: format!("local `{RUNTIME_IMAGE}` matches this binary ({installed}){others_note}"),
            hint: None,
        },
        Some(verdict) => Check {
            name: RUNTIME_IMAGE_CHECK_NAME.into(),
            status: Status::Warn,
            message: format!(
                "local {}; this binary is {installed}: dispatch skips it and runs \
                 `{pinned}` (pulling it if absent); a darkmux older than this fix still runs it, \
                 and `--image {RUNTIME_IMAGE}` is refused{others_note}",
                describe_non_match(RUNTIME_IMAGE, &verdict)
            ),
            hint: Some(format!(
                "rebuild it from a darkmux {installed} source checkout: `{}`: or remove it \
                 (`docker rmi {RUNTIME_IMAGE}`) so nothing can pick it up",
                rebuild_command(RUNTIME_IMAGE, installed)
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
            //   2. (#2902 step 4) every error `ProfileRegistry::validate`
            //      finds — the ONE place the registry's rules live: managed
            //      models missing `n_ctx`, endpoints that cannot work as
            //      written, and ids no `endpoints` entry defines.
            let mut findings: Vec<String> = loaded
                .registry
                .quarantined
                .iter()
                .map(|q| format!("quarantined {} \"{}\": {}", q.kind, q.name, q.error))
                .collect();
            findings.extend(loaded.registry.validate());

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
        Err(e) => profile_registry_load_failure(&e),
    }
}

/// The Fail row for a registry that did not load. The whole cause chain is
/// the message; `darkmux init` is advised only when there is no file to fix.
fn profile_registry_load_failure(e: &anyhow::Error) -> Check {
    let present = profiles::registry_path(None).is_some_and(|p| p.exists());
    let hint = if present {
        format!(
            "fix what the message names; the `{USER_FILE_KEYS_CHECK_NAME}: profiles.json` row lists every \
             other shape in the file this release refuses"
        )
    } else {
        "run `darkmux init` to create one".to_string()
    };
    Check {
        name: "profile registry".into(),
        status: Status::Fail,
        message: format!("{e:#}"),
        hint: Some(hint),
    }
}

/// (#2707) The point at which an accumulated temp-root population stops
/// being background noise and becomes worth naming.
///
/// Not tuned against anything — it is a round number chosen so an ordinary
/// machine with a handful of live dispatch out-dirs stays quiet while the
/// shape this check exists to describe (thousands, grown one test process
/// at a time) is impossible to miss. The measured machine that prompted
/// this carried 9,108.
const TEMP_RESIDUE_WARN_AT: usize = 100;

/// An upper bound on how many entries the scan will read.
///
/// A temp root is somebody else's directory and can be arbitrarily large;
/// `doctor` is not entitled to an unbounded walk of it. On a truncated
/// scan the check says so rather than reporting a count it knows is
/// short — an undercount presented as a total is the kind of number an
/// operator would act on.
const TEMP_RESIDUE_SCAN_CAP: usize = 50_000;

/// What [`summarize_temp_residue`] found: a total, the per-family
/// breakdown sorted largest-first, and whether the scan hit its cap.
#[derive(Debug, Default, PartialEq, Eq)]
struct TempResidue {
    total: usize,
    families: Vec<(String, usize)>,
    truncated: bool,
    /// (#2972) Resume-origin records (`<dir>.resume_origin.json`) and
    /// execution lock files (`<dir>.execution.lock`) whose out-dir is gone:
    /// the sibling outlived the directory it describes.
    orphaned_records: usize,
}

/// The stable part of a temp directory's name — the name with every
/// all-digit segment dropped.
///
/// Every darkmux temp directory is `<something>-<pid>`,
/// `<something>-<unix_micros>` or `<something>-<pid>-<nanos>-<counter>`,
/// so dropping the numeric segments collapses a population back to the
/// call site that produced it: `darkmux-out-pr-reviewer-1725000000000000`
/// and `darkmux-flow-test-8261` become `darkmux-out-pr-reviewer` and
/// `darkmux-flow-test`. A name with no numeric segment at all is its own
/// family, unchanged.
fn temp_residue_family(name: &str) -> String {
    let kept: Vec<&str> = name
        .split('-')
        .filter(|seg| !seg.is_empty() && !seg.bytes().all(|b| b.is_ascii_digit()))
        .collect();
    if kept.is_empty() {
        name.to_string()
    } else {
        kept.join("-")
    }
}

/// Count the darkmux-namespaced DIRECTORIES directly under `dir`.
///
/// Scope is the namespace contract, and it is the whole of the claim this
/// check makes. Only entries whose name begins with `darkmux-` or `dmx-`
/// are looked at: those are the ones darkmux itself created and can
/// therefore describe. Everything else in the temp root belongs to some
/// other tool or to the operator, and is neither counted nor mentioned —
/// reporting on a directory darkmux did not make would be adjudicating
/// somebody else's tree.
///
/// Directories only. The temp-root residue darkmux leaves as FILES (a
/// hosted dispatch's `curl` config, for instance) is written and removed
/// within one call and does not accumulate.
fn summarize_temp_residue(dir: &std::path::Path) -> TempResidue {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return TempResidue::default();
    };
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut total = 0usize;
    let mut orphaned_records = 0usize;
    let mut seen = 0usize;
    let mut truncated = false;
    for entry in entries.flatten() {
        seen += 1;
        if seen > TEMP_RESIDUE_SCAN_CAP {
            truncated = true;
            break;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("darkmux-") && !name.starts_with("dmx-") {
            continue;
        }
        // `file_type()` comes straight off the directory entry on the
        // platforms darkmux runs on, so this is not a stat per entry.
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir {
            if darkmux_types::paths::is_orphaned_resume_origin(dir, name) {
                orphaned_records += 1;
            }
            continue;
        }
        total += 1;
        *counts.entry(temp_residue_family(name)).or_default() += 1;
    }
    let mut families: Vec<(String, usize)> = counts.into_iter().collect();
    // Largest first; ties by name so the report is stable run to run.
    families.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    TempResidue { total, families, truncated, orphaned_records }
}

/// (#2707) Abandoned darkmux directories in the system temp root.
///
/// A describable fact, not a verdict: "there are N of these, here is what
/// each family is". Read-only — `doctor` never removes anything here, and
/// two of the three families it can report are not residue at all.
///
/// # Why this exists alongside the creation-path fix
///
/// #2707's fix made the test-scratch directories self-collecting, which
/// bounds the population going forward and drains the backlog for the
/// prefixes that fix owns. It does nothing for two other cases, and those
/// are the ones this check is actually for:
///
/// * A machine carrying a backlog from a family nothing sweeps — chiefly
///   `darkmux-out-<role>-<micros>` and `darkmux-dispatch-<role>-<micros>`,
///   a real dispatch's out-dir and workspace. Those are KEPT ON PURPOSE:
///   the out-dir holds that run's prompt, trajectory and checkpoint, which
///   is exactly what an operator goes looking for afterward. Deliberate
///   retention with no expiry still accumulates, and the operator is the
///   only one who can say which runs they are done with.
/// * A future call site written the old way. This check keys on the
///   NAMESPACE rather than on a source pattern, so it sees a directory
///   nobody has taught it about — which is the property a text scan for
///   "creates a temp dir and abandons it" could not have.
///
/// # (#2777) `darkmux-test-isolated-<pid>` needs no special handling here
///
/// Worth stating so nobody "fixes" it later — and worth stating the SHAPE
/// exactly, because the first draft of this paragraph got it wrong and
/// then reasoned from the wrong shape (#2782 MF3). #2777 moved the
/// test-build scratch fallback from a fixed `/tmp/darkmux-test-isolated`
/// to `<temp>/darkmux-test-isolated-<pid>`: a HYPHEN, so each test process
/// gets its own SIBLING directory directly under the temp root, not a
/// `<pid>` subdirectory one level down. `darkmux_types::paths::
/// test_isolated_root`'s own unit tests pin both halves of that (the
/// pid-bearing name, and the parent being the temp root itself).
///
/// Two consequences, and neither needs code here:
///
/// * **The count is per-process, and that is correct.** Three abandoned
///   roots count 3, not 1 — they ARE three trees, and this row's whole job
///   is making accumulation visible. Collapsing them would be the row
///   lying to keep itself quiet.
/// * **The BREAKDOWN still reads as one line.** [`temp_residue_family`]
///   strips a trailing all-digits segment, so every `darkmux-test-isolated-
///   <pid>` reports under the single family `darkmux-test-isolated` no
///   matter how many processes have run — which is the property that keeps
///   a warn-threshold breakdown readable.
///
/// What this check must NOT grow is recursion: descending into a per-pid
/// root would start counting live processes' working state as though it
/// were abandoned residue.
fn check_temp_residue() -> Check {
    let tmp = std::env::temp_dir();
    let residue = summarize_temp_residue(&tmp);
    let name = "temp residue".to_string();

    let orphans = match residue.orphaned_records {
        0 => String::new(),
        n => format!(", {n} orphaned resume record(s)"),
    };
    if residue.total + residue.orphaned_records < TEMP_RESIDUE_WARN_AT {
        return Check {
            name,
            status: Status::Pass,
            message: format!("{} darkmux director(ies) under {}{orphans}", residue.total, tmp.display()),
            hint: None,
        };
    }

    let breakdown = residue
        .families
        .iter()
        .take(5)
        .map(|(family, n)| format!("{family} x{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let scope = if residue.truncated { "at least " } else { "" };

    Check {
        name,
        status: Status::Warn,
        message: format!(
            "{scope}{} darkmux director(ies) under {} ({breakdown}){orphans}",
            residue.total,
            tmp.display()
        ),
        hint: Some(format!(
            "each one is a directory darkmux created and left. `darkmux-out-*` and \
             `darkmux-dispatch-*` are a dispatch's out-dir and workspace: they hold that \
             run's prompt, trajectory and checkpoint, so they are kept deliberately and \
             removing one discards that run's record. The rest are test scratch, which a \
             test process now collects on its own (#2707); any still here predate that. \
             Each `darkmux-out-*` dir has siblings `<dir>.resume_origin.json` and \
             `<dir>.execution.lock`: delete them together, and delete any sibling whose dir \
             is already gone (an orphan). \
             Nothing here is removed for you: review {} and delete what you are done with.",
            tmp.display()
        )),
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

/// How a user-tier mission config's declared schema compares with this
/// binary's.
enum SchemaDrift {
    /// Same version, or no parseable version (the validate pass reports that).
    None,
    /// An older MAJOR: the 4.0 major broke old documents.
    OlderMajor { doc_major: u32 },
    /// Same major, older minor: the number is stale, the document is fine.
    OlderMinor(String),
}

fn user_schema_drift(declared: Option<&str>) -> SchemaDrift {
    let Some((doc_major, doc_minor)) = declared.and_then(parse_major_minor) else {
        return SchemaDrift::None;
    };
    let (bin_major, bin_minor) = parse_major_minor(darkmux_crew::mission_config::MISSION_CONFIG_SCHEMA)
        .expect("MISSION_CONFIG_SCHEMA is a valid MAJOR.MINOR constant");
    if doc_major < bin_major {
        return SchemaDrift::OlderMajor { doc_major };
    }
    if doc_major != bin_major {
        return SchemaDrift::None;
    }
    // (#3035) A NEWER minor is not judged here: the user-file gate refuses a
    // document whose schema_version is newer than this binary reads (`mission
    // launch` fails at preflight), and that check owns the message.
    if doc_minor < bin_minor {
        SchemaDrift::OlderMinor(format!(
            "declares schema {doc_major}.{doc_minor}, older than this binary's \
             {bin_major}.{bin_minor}: every field it names is one this build understands, so \
             the number alone is stale, not broken"
        ))
    } else {
        SchemaDrift::None
    }
}

/// The informational notes for user-tier configs whose declared schema trails
/// this binary's: an older MAJOR names the file, a same-major older minor is
/// only a stale number.
fn schema_drift_notes(older_major: &[(String, u32)], minor_drift: &[(String, String)]) -> String {
    let mut notes = String::new();
    let bin_major = parse_major_minor(darkmux_crew::mission_config::MISSION_CONFIG_SCHEMA)
        .expect("MISSION_CONFIG_SCHEMA is a valid MAJOR.MINOR constant")
        .0;
    for (file, doc_major) in older_major {
        notes.push_str(&format!(
            "; {file}: schema major ({doc_major}) is older than this darkmux's ({bin_major}); \
             check it for keys this release refuses (a `panel` block) or step config it now \
             validates"
        ));
    }
    if !minor_drift.is_empty() {
        notes.push_str(&format!(
            "; {} declare a same-major schema_version OLDER than this binary's, naming only \
             fields this build understands (informational: the number is stale, and this \
             check has not inspected the documents' contents beyond validation): {}",
            minor_drift.len(),
            minor_drift
                .iter()
                .map(|(id, note)| format!("\"{id}\": {note}"))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    notes
}

/// A user-tier file is the one the operator edits, so a finding about it names
/// the path; a built-in has no file to point at.
fn naming_user_file(message: String, loaded: &darkmux_crew::mission_config::LoadedMissionConfig) -> String {
    match loaded.source {
        darkmux_crew::mission_config::MissionConfigSource::User => {
            format!("{message} (file: {})", loaded.manifest_path.display())
        }
        _ => message,
    }
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
/// - **Step kinds and their wiring** are checked against the `catalog` the
///   caller supplies (#2312). `main.rs` passes the registry `mission launch`
///   itself resolves against (`src/mission_launch.rs::all_step_kinds`: Tier 1
///   plus the coder-phase, crawl and review kinds), which this crate cannot
///   build (the coder-phase kinds live in the root crate), so this check is
///   appended after `run()` like the skills freshness check. That catalog
///   also carries each kind's declared ports, so a config wiring a task to a
///   producer of the wrong kind is an `Error` finding here, and a retired
///   kind id (#2430) is one too. A kind the catalog does not know stays
///   INFORMATIONAL, never blocking: a caller holding only the Tier 1 set sees
///   Tier 3 ids as unknown, and a permanent Warn for that would teach
///   operators to ignore this check.
pub fn check_mission_config_registry(catalog: &darkmux_crew::mission_config::KindCatalog) -> Check {
    use darkmux_crew::mission_config::{self, FindingSeverity};

    let ids = mission_config::list_ids();
    if ids.is_empty() {
        return Check {
            name: "mission config registry".into(),
            status: Status::Pass,
            message: "no mission configs registered".into(),
            hint: None,
        };
    }


    let mut summary_lines: Vec<String> = Vec::new();
    // (#2003) (id, explanation) pairs, so identical explanations can be
    // grouped at render time instead of repeated once per document.
    let mut blocking: Vec<(String, String)> = Vec::new();
    let mut kind_warning_ids: Vec<String> = Vec::new();
    // (#2428) (id, note) pairs for a same-major minor/patch schema_version
    // drift — informational only, never blocking. See the loop body below
    // for why this is no longer in `blocking`.
    let mut minor_drift: Vec<(String, String)> = Vec::new();
    // (file name, declared major) for a user-tier config whose schema MAJOR is
    // older than the binary's: the 4.0 major broke old documents (a refused
    // `panel` key, checked step config), and this points at the file.
    let mut older_major: Vec<(String, u32)> = Vec::new();

    for id in &ids {
        match mission_config::load(id) {
            Ok(loaded) => {
                let findings = loaded.config.validate_with(catalog);
                let errors: Vec<_> =
                    findings.iter().filter(|f| f.severity == FindingSeverity::Error).collect();
                // (#3035) A schema NEWER than this binary reads is refused by the
                // user-file gate and reported by that check, once; this one
                // keeps only the drift that gate does not own.
                let is_newer = loaded.config.schema_version.as_deref().is_some_and(|v| {
                    darkmux_types::data_version::is_newer(
                        &serde_json::Value::String(v.to_string()),
                        mission_config::MISSION_CONFIG_SCHEMA,
                    )
                });
                let version_drift: Vec<_> = findings
                    .iter()
                    .filter(|f| !is_newer && f.severity == FindingSeverity::Warning && f.path == "schema_version")
                    .collect();
                let kind_warnings: Vec<_> = findings
                    .iter()
                    .filter(|f| f.severity == FindingSeverity::Warning && f.path.ends_with(".kind"))
                    .collect();

                let version = loaded.config.schema_version.as_deref().unwrap_or("(unset)");
                summary_lines.push(format!("{id} ({}, schema {version})", loaded.source.label()));

                if !errors.is_empty() {
                    let joined = errors.iter().map(|f| f.to_string()).collect::<Vec<_>>().join("; ");
                    blocking.push((id.clone(), naming_user_file(joined, &loaded)));
                }
                if !version_drift.is_empty() {
                    let joined =
                        version_drift.iter().map(|f| f.to_string()).collect::<Vec<_>>().join("; ");
                    blocking.push((id.clone(), joined));
                }
                // (#2428) A USER-tier copy whose schema MINOR differs from
                // the binary's still LOADS and VALIDATES cleanly — the
                // mission-config schema is deliberately lenient-on-read
                // (all-`Option` fields + `#[serde(flatten)] extras`). The
                // two DIRECTIONS of that gap are NOT the same finding,
                // though, and collapsing them is what #2428's first pass got
                // wrong:
                //
                // TRAILING (`doc_minor < bin_minor`) — the document is OLDER
                // than the binary. Every field it names is one this build
                // already understands; the version number is just stale.
                // #1917/#1648 treated this as blocking and it produced 13
                // FALSE "issues" on a real machine (#2428): operator-authored
                // configs with no built-in counterpart and no retired
                // vocabulary, every one launching clean under `mission launch
                // <id> --dry-run`, read as doctor failures purely because
                // their declared number was old. A doctor line the operator
                // cannot act on is noise (#2425/#2411's same lesson), so this
                // direction is INFORMATIONAL only.
                //
                // LEADING (`doc_minor > bin_minor`) — the document is NEWER
                // than the binary. Not judged here (#3035): the user-file
                // gate refuses it ("written by a newer darkmux ... Upgrade
                // darkmux."), `mission launch` fails at preflight, and the
                // `user file keys` check carries that one message. This
                // check used to warn that a run "would still complete
                // green", which contradicted the refusal.
                if loaded.source == mission_config::MissionConfigSource::User {
                    match user_schema_drift(loaded.config.schema_version.as_deref()) {
                        SchemaDrift::None => {}
                        SchemaDrift::OlderMajor { doc_major } => {
                            let file = loaded
                                .manifest_path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| id.clone());
                            older_major.push((file, doc_major));
                        }
                        SchemaDrift::OlderMinor(note) => minor_drift.push((id.clone(), note)),
                    }
                }
                if !kind_warnings.is_empty() {
                    kind_warning_ids.push(id.clone());
                }
            }
            Err(e) => blocking.push((id.clone(), format!("failed to parse — {e}"))),
        }
    }

    // The informational notes belong to BOTH outcomes. They used to be
    // rendered only inside the `blocking.is_empty()` arm, which dropped them
    // exactly when an operator is most likely reading this check — one
    // unrelated config with an empty `name` erased every other config's note,
    // including the drift note whose stated justification is "so an operator
    // chasing a specific field can still find it".
    let mut notes = String::new();
    if !kind_warning_ids.is_empty() {
        notes.push_str(&format!(
            "; {} reference step kinds outside this process's Tier 1 registry (expected — \
             Tier 3 kinds register at composition time, so this check can't see them): {}",
            kind_warning_ids.len(),
            kind_warning_ids.join(", ")
        ));
    }
    notes.push_str(&schema_drift_notes(&older_major, &minor_drift));

    if blocking.is_empty() {
        let mut message =
            format!("{} mission config(s) registered: {}", ids.len(), summary_lines.join(", "));
        message.push_str(&notes);
        Check {
            name: "mission config registry".into(),
            status: Status::Pass,
            message,
            hint: None,
        }
    } else {
        let mut message = summarize_findings(ids.len(), &blocking);
        message.push_str(&notes);
        Check {
            name: "mission config registry".into(),
            status: Status::Warn,
            // `blocking` holds one entry per FINDING GROUP, not per config
            // (one document can contribute a structural-error entry AND a
            // schema-drift entry), so the count is worded as issues, never
            // as a config count (#1284 review round 1).
            message,
            hint: Some(
                "fix the named document(s) under `~/.darkmux/mission-configs/<id>.json` (or, for \
                 an operator-pointed `DARKMUX_TEMPLATES_DIR`/`config.dirs.templates` override, its \
                 `templates/builtin/mission-configs/<id>.json`): a \
                 dangling depends_on, an empty id, or a schema_version your darkmux build \
                 doesn't recognize. (A document declaring a schema_version NEWER than this \
                 binary's is refused at preflight and reported by the user-file check, not \
                 here.) These documents DO execute: `darkmux mission launch <id>` runs any config whose \
                 graph names step kinds this build can construct, so a finding here is a config \
                 that MAY fail at launch, not a dormant one: an Error-tier finding bails the \
                 launch, a Warning-tier one (a schema_version drift, say) only prints."
                    .into(),
            ),
        }
    }
}

fn check_lms_binary() -> Check {
    // (#2149) Resolved through `config_access::lms_bin()` — the ONE place
    // `env(DARKMUX_LMS_BIN) > config.lms_bin > "lms"` precedence lives.
    // This check previously read `DARKMUX_LMS_BIN` directly and never saw
    // `config.lms_bin` at all, so an operator whose config named an
    // out-of-PATH `lms` (e.g. `~/.lmstudio/bin/lms`) got a false FAIL here
    // even though the daemon itself reached LMStudio fine.
    let (bin, source) = darkmux_types::config_access::lms_bin_with_source();
    let via = match source {
        darkmux_types::config_access::Source::Env => " (via DARKMUX_LMS_BIN)",
        darkmux_types::config_access::Source::Config => " (via config.lms_bin)",
        darkmux_types::config_access::Source::BuiltIn => "",
    };
    // A value containing a path separator is a path, not a PATH-searchable
    // command name — `which()` searches PATH entries; checking a path
    // directly against the filesystem is the correct lookup, and doesn't
    // depend on `PATH` being set at all in the calling process.
    let (found, how) = if bin.contains('/') {
        let p = std::path::Path::new(&bin);
        (p.is_file() && is_executable(p), "at that path")
    } else {
        (which(&bin).is_some(), "on PATH")
    };
    if found {
        Check {
            name: "lms binary".into(),
            status: Status::Pass,
            message: format!("found `{bin}` {how}{via}"),
            hint: None,
        }
    } else {
        Check {
            name: "lms binary".into(),
            status: Status::Fail,
            message: format!("`{bin}`{via} not found {how}"),
            hint: Some(
                "install LMStudio (https://lmstudio.ai/), then run `darkmux config set \
                 lms_bin <path>` — the durable mechanism, visible in `~/.darkmux/config.json`; \
                 DARKMUX_LMS_BIN overrides it for one shell"
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
                message: "the profile registry did not load (see the `profile registry` row): can't check match".into(),
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
    // (#2923) See `check_runtime_image_freshness`: no host Docker from unit
    // tests; `docker_status_to_check` is tested directly.
    if cfg!(test) {
        return not_applicable("docker runtime", "Docker is not probed from unit tests");
    }
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
            message: format!(
                "Docker is up; no local runtime image built for this darkmux ({}): darkmux \
                 will pull it on the first dispatch",
                env!("CARGO_PKG_VERSION")
            ),
            hint: Some(format!(
                "darkmux pulls `{}` from GHCR on demand (#759). Pre-pull now with \
                 `docker pull {}`, or build locally from the root of a darkmux {version} \
                 source checkout: `{}`.",
                ghcr_runtime_image(),
                ghcr_runtime_image(),
                darkmux_crew::runtime_image::rebuild_command(RUNTIME_IMAGE, env!("CARGO_PKG_VERSION")),
                version = env!("CARGO_PKG_VERSION"),
            )),
        },
        S::ProbeError(e) => Check {
            name,
            status: Status::Warn,
            message: format!("couldn't probe the Docker runtime image: {e}"),
            hint: None,
        },
        // (#2923) Dispatch would refuse this image rather than pull, so doctor
        // says the same thing dispatch will.
        S::ImageRefused(refusal) => Check {
            name,
            status: Status::Warn,
            message: "Docker is up, but the runtime image dispatch would use does not match this \
                      darkmux: dispatches will be refused"
                .to_string(),
            hint: Some(refusal),
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
        Err(_) => return skip("the profile registry did not load (see the `profile registry` row)"),
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
            let ns = darkmux_profiles::ownership::namespaced_identifier(pm);
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
                    let ns = darkmux_profiles::ownership::namespaced_identifier(pm);
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

fn platform_and_provider_status(hw: &hardware::HardwareSpec) -> Check {
    let provider = heuristics::active_provider(hw);
    let summary = hw.one_line_summary();
    // Pass when a non-generic provider claims the hardware (i.e. we have
    // validated rules for it). Warn when only generic matched — heuristics
    // will work but suggestions are unvalidated for this platform.
    if provider.is_generic() {
        Check {
            name: "platform / heuristics".into(),
            status: Status::Warn,
            message: format!("{summary} → provider=`generic` (unvalidated)"),
            hint: Some(
                "darkmux ships rules for Apple Silicon at 32GB, 64GB and 128GB+ (the \
                 128GB tier is measured; 32GB and 64GB are extrapolated from it). Your \
                 hardware doesn't match any of them; profile draft suggestions will use \
                 conservative defaults. Consider opening a PR with measured rules for \
                 your platform: see crates/darkmux-heuristics/ for the trait + existing \
                 examples."
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

fn check_platform_and_provider() -> Check {
    platform_and_provider_status(&hardware::detect())
}

fn power_state_status(source: Option<PowerSource>, is_macos: bool) -> Check {
    match source {
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
        None => {
            if is_macos {
                Check {
                    name: "power state".into(),
                    status: Status::Warn,
                    message: "could not read power source (`pmset -g batt` returned no status)".into(),
                    hint: Some("Check pmset permissions or power management daemon.".into()),
                }
            } else {
                Check {
                    name: "power state".into(),
                    status: Status::Pass,
                    message: "n/a (non-Apple Silicon? skipping)".into(),
                    hint: None,
                }
            }
        }
    }
}

fn check_power_state() -> Check {
    power_state_status(read_power_source(), cfg!(target_os = "macos"))
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
        push_hint_lines(&mut lines, hint, width);
    }
    lines
}

/// The line that opens a paste block in a hint: a `bash` heredoc whose
/// terminator is the delimiter it names. The block is a script the user pastes
/// out of the terminal, so it renders verbatim.
const PASTE_BLOCK_OPEN: &str = "bash <<'";

/// Render `hint` under its check: word-wrapped behind the `→` gutter, except a
/// paste block (from a line opening with [`PASTE_BLOCK_OPEN`] through its
/// terminator), which is emitted verbatim at column zero. Wrapping would split
/// a long quoted path across lines, and the gutter would put the terminator off
/// column zero: either breaks the paste.
fn push_hint_lines(lines: &mut Vec<String>, hint: &str, width: usize) {
    // "        → " — 8 spaces, the arrow, a space.
    const HINT_HEAD: usize = 10;
    let mut terminator: Option<String> = None;
    for raw in hint.lines() {
        if let Some(end) = terminator.as_deref() {
            let done = raw == end;
            lines.push(raw.to_string());
            if done {
                terminator = None;
            }
            continue;
        }
        if let Some(rest) = raw.strip_prefix(PASTE_BLOCK_OPEN) {
            terminator = rest.split('\'').next().map(str::to_string);
            lines.push(raw.to_string());
            continue;
        }
        let wrapped = wrap_hanging(raw, width.saturating_sub(HINT_HEAD).max(20), 0);
        lines.push(format!("        → {}", darkmux_types::style::dim(&wrapped[0])));
        for cont in &wrapped[1..] {
            lines.push(format!("{}{}", " ".repeat(HINT_HEAD), darkmux_types::style::dim(cont)));
        }
    }
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

/// Where the one-time `darkmux-upgrade` skill lives.
const UPGRADE_SKILL_URL: &str = "https://github.com/kstrat2001/darkmux/blob/main/docs/upgrade/darkmux-upgrade/SKILL.md";

/// The line doctor prints under its summary when any retired-state row is
/// not passing; `None` when every such row passes.
fn upgrade_skill_pointer(r: &DoctorReport) -> Option<String> {
    let found = r.checks.iter().any(|c| {
        c.status != Status::Pass
            && (c.name.starts_with(USER_FILE_KEYS_CHECK_NAME)
                || c.name == RETIRED_ENV_CHECK_NAME)
    });
    found.then(|| {
        format!(
            "retired config keys or env vars found: an agent can apply the fixes by following the optional, \
             one-time upgrade skill at {UPGRADE_SKILL_URL}"
        )
    })
}

/// (5.0) The report as a viewer that is not this machine reads it in the
/// console's doctor panel. Every check has already run; this shapes only what
/// is printed, before the renderer wraps and colors it (so no withheld value
/// can be split across a line). Each row keeps its name, its status and its
/// remedy. The rows whose detail is the fleet's execution surface
/// ([`fleet_submission::EXECUTION_SURFACE_ROWS`]) withhold that detail whole;
/// every other row, and every remedy, loses each value in `w` (this machine's
/// configured addresses, paths, endpoint URLs and credential pointers). A
/// withheld part reads `panel_audience::WITHHELD`.
pub fn shape_for_remote(r: &mut DoctorReport, w: &darkmux_types::panel_audience::Withheld) {
    use darkmux_types::panel_audience::WITHHELD;
    for c in &mut r.checks {
        c.message = if fleet_submission::EXECUTION_SURFACE_ROWS.contains(&c.name.as_str()) {
            WITHHELD.to_string()
        } else {
            w.scrub(&c.message)
        };
        c.hint = c.hint.as_deref().map(|h| w.scrub(h));
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
    if let Some(pointer) = upgrade_skill_pointer(r) {
        println!("{pointer}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Visible text only — the marker and hint carry ANSI escapes whose bytes
    /// must not count toward a width assertion.
    pub(crate) fn strip_ansi(s: &str) -> String {
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

    // ─── (5.0) the doctor panel for a viewer that is not this machine ──

    /// Every check ran and every row stays, with its name, its status and its
    /// remedy. The execution-surface rows' detail (the listener's address,
    /// port and busy policy; this machine's node; what each trusted machine
    /// may run here) is withheld whole; every other row and remedy loses the
    /// configured values only.
    #[test]
    fn a_remote_viewer_keeps_every_row_and_remedy_and_loses_the_execution_surface() {
        use darkmux_types::panel_audience::{Withheld, WITHHELD};
        let row = |name: &str, status, message: &str, hint: Option<&str>| Check {
            name: name.into(),
            status,
            message: message.into(),
            hint: hint.map(str::to_string),
        };
        let full = DoctorReport {
            checks: vec![
                row("fleet listener", Status::Pass, "listening on 100.64.0.2:8766; fleet.busy_policy `queue`", None),
                row("fleet identity", Status::Pass, "tailscale: this machine is `studio` at 100.64.0.2", None),
                row("fleet trust", Status::Warn, "laptop may run fast (roles: coder; images: img:1)", Some("`darkmux machine untrust laptop`")),
                row("lms binary", Status::Fail, "`/opt/fake/bin/lms` not found", Some("set `lms_bin` (now /opt/fake/bin/lms)")),
                row("models loaded", Status::Pass, "2 resident", None),
            ],
        };
        let mut shown = full.clone();
        shape_for_remote(&mut shown, &Withheld::from_values(["/opt/fake/bin/lms".to_string()]));
        let by_name = |name: &str| shown.checks.iter().find(|c| c.name == name).unwrap();
        for name in fleet_submission::EXECUTION_SURFACE_ROWS {
            assert_eq!(by_name(name).message, WITHHELD, "{name}");
        }
        assert_eq!(by_name("fleet trust").hint.as_deref(), Some("`darkmux machine untrust laptop`"), "the remedy stays");
        assert_eq!(by_name("lms binary").message, format!("`{WITHHELD}` not found"));
        assert_eq!(by_name("lms binary").hint.as_deref(), Some(format!("set `lms_bin` (now {WITHHELD})").as_str()));
        assert_eq!(by_name("models loaded").message, "2 resident");
        let summary = |r: &DoctorReport| r.checks.iter().map(|c| (c.name.clone(), c.status)).collect::<Vec<_>>();
        assert_eq!(summary(&shown), summary(&full), "every row and status stays");
    }

    // ─── (#2707) temp residue ─────────────────────────────────────────

    /// Build a temp root with a known population, so the counting is
    /// asserted against something the test controls rather than against
    /// whatever the machine happens to have.
    fn temp_root_with(dirs: &[&str], files: &[&str]) -> tempfile::TempDir {
        let root = tempfile::TempDir::new().unwrap();
        for d in dirs {
            std::fs::create_dir_all(root.path().join(d)).unwrap();
        }
        for f in files {
            std::fs::write(root.path().join(f), b"x").unwrap();
        }
        root
    }

    /// (#2972) A resume-origin record whose out-dir is gone is counted; one
    /// with its dir is not, and the dir stays the only thing in `total`.
    #[test]
    fn the_scan_counts_an_orphaned_resume_record() {
        let root = temp_root_with(
            &["darkmux-out-coder-1"],
            &["darkmux-out-coder-1.resume_origin.json", "darkmux-out-coder-2.resume_origin.json"],
        );
        let residue = summarize_temp_residue(root.path());
        assert_eq!(residue.total, 1);
        assert_eq!(residue.orphaned_records, 1, "only the record whose dir is gone is an orphan");
    }

    /// The namespace contract, which is the whole of this check's claim:
    /// darkmux's own directories are counted, and nothing else in the
    /// temp root is looked at.
    #[test]
    fn the_scan_counts_only_darkmux_namespaced_directories() {
        let root = temp_root_with(
            &[
                "darkmux-flow-test-1",
                "darkmux-flow-test-2",
                "dmx-dialectic-test-3",
                // Not ours: another tool's, and the operator's own.
                "tmp.AbCdEf",
                "com.apple.something",
                "my-darkmux-notes",
            ],
            // A FILE in the namespace is not residue — darkmux's temp
            // files are written and removed inside one call.
            &["darkmux-remote-1-0.curl"],
        );

        let residue = summarize_temp_residue(root.path());

        assert_eq!(residue.total, 3, "only the three darkmux directories: {residue:?}");
        assert_eq!(
            residue.families,
            vec![("darkmux-flow-test".to_string(), 2), ("dmx-dialectic-test".to_string(), 1)],
            "families collapse the numeric segments and sort largest-first"
        );
        assert!(!residue.truncated);
    }

    /// Family collapsing, against the real name shapes in the tree: a
    /// pid, a unix-micros timestamp, and a pid+nanos+counter triple.
    #[test]
    fn a_family_is_the_name_with_its_numeric_segments_dropped() {
        assert_eq!(temp_residue_family("darkmux-flow-test-8261"), "darkmux-flow-test");
        assert_eq!(
            temp_residue_family("darkmux-out-pr-reviewer-1725000000000000"),
            "darkmux-out-pr-reviewer"
        );
        assert_eq!(
            temp_residue_family("darkmux-runtime-test-out-412-99-3"),
            "darkmux-runtime-test-out"
        );
        assert_eq!(
            temp_residue_family("darkmux-412-99-review"),
            "darkmux-review",
            "a numeric segment in the MIDDLE collapses too — acp_panel's name puts the pid \
             and the nanos before the word"
        );
        assert_eq!(
            temp_residue_family("darkmux-test-isolated"),
            "darkmux-test-isolated",
            "a name with no numeric segment is its own family, unchanged"
        );
    }

    /// The threshold, both sides. A handful of live dispatch out-dirs is
    /// an ordinary machine and must stay quiet; the shape this check
    /// exists to describe must not.
    #[test]
    fn the_check_stays_quiet_below_the_threshold_and_names_the_families_above_it() {
        let quiet: Vec<String> =
            (0..3).map(|n| format!("darkmux-out-coder-{n}")).collect();
        let quiet: Vec<&str> = quiet.iter().map(|s| s.as_str()).collect();
        let root = temp_root_with(&quiet, &[]);
        let residue = summarize_temp_residue(root.path());
        assert!(
            residue.total < TEMP_RESIDUE_WARN_AT,
            "sanity: three dirs must be under the threshold"
        );

        let many: Vec<String> = (0..TEMP_RESIDUE_WARN_AT)
            .map(|n| format!("darkmux-flow-test-{n}"))
            .collect();
        let many: Vec<&str> = many.iter().map(|s| s.as_str()).collect();
        let root = temp_root_with(&many, &[]);
        let residue = summarize_temp_residue(root.path());
        assert_eq!(residue.total, TEMP_RESIDUE_WARN_AT);
        assert_eq!(residue.families, vec![("darkmux-flow-test".to_string(), TEMP_RESIDUE_WARN_AT)]);
    }

    /// (#2782 MF3) The `darkmux-test-isolated-<pid>` shape, asserted so the
    /// paragraph above this check cannot drift away from it again.
    ///
    /// That paragraph used to describe a nested `<temp>/darkmux-test-
    /// isolated/<pid>` and conclude the tree "reads as exactly one
    /// directory no matter how many test processes have run". The shipped
    /// shape is a HYPHEN — per-pid SIBLINGS directly under the temp root —
    /// so the count is per-process, which is correct (three abandoned trees
    /// are three trees) and the opposite of what the comment claimed. Only
    /// the FAMILY collapses, via the trailing-digits strip.
    #[test]
    fn per_pid_test_isolated_roots_count_separately_and_share_one_family() {
        let names = [
            "darkmux-test-isolated-111",
            "darkmux-test-isolated-222",
            "darkmux-test-isolated-333",
        ];
        let root = temp_root_with(&names, &[]);
        let residue = summarize_temp_residue(root.path());
        assert_eq!(
            residue.total, 3,
            "each per-pid root is its own directory under the temp root"
        );
        assert_eq!(
            residue.families,
            vec![(darkmux_types::paths::TEST_ISOLATED_DIR_NAME.to_string(), 3)],
            "…and they all report under the one family, so the breakdown stays \
             one line however many processes have run"
        );
    }

    /// An unreadable or absent temp root reports nothing rather than
    /// failing the whole doctor run — and reports a real zero, not a
    /// count it could not take.
    #[test]
    fn an_unreadable_temp_root_reports_nothing() {
        let residue = summarize_temp_residue(std::path::Path::new("/nonexistent-temp-root-2707"));
        assert_eq!(residue, TempResidue::default());
    }

    /// The live check, against the real temp root. It must never Fail
    /// (this is a description, not a health verdict) and must always name
    /// the directory it looked at, so the number is attributable.
    #[test]
    fn the_live_check_describes_rather_than_adjudicates() {
        let check = check_temp_residue();
        assert_ne!(check.status, Status::Fail, "temp residue is a fact, never a failure");
        assert!(
            check.message.contains(&std::env::temp_dir().display().to_string()),
            "the message must name the directory the count came from: {}",
            check.message
        );
    }

    #[test]
    fn the_verdict_banner_is_one_line_however_long_the_worst_message_is() {
        // The banner quotes the highest-severity check's message. The real
        // `mission config registry` finding concatenates one full explanation
        // per affected config — measured at 9,726 characters, the same ~600
        // char paragraph 15 times. Wrapping it faithfully filled fifty lines
        // of the operator's screen with a restatement of what the check line
        // below already says. A banner is a HEADLINE: one line, always.
        let huge = std::iter::repeat_n("some very wordy finding text about a config", 200)
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
    /// (#2947) Conformance: for EVERY registered enum setting, an unknown
    /// value set through the env tier (when the setting has one) and
    /// through the config tier is a **Fail** row naming the raw value,
    /// where it was set, and every valid value; a valid value is Pass.
    /// Iterates the registry, so a new setting is covered by registering
    /// it, with no test of its own.
    #[serial_test::serial]
    #[test]
    fn every_registered_enum_setting_fails_doctor_on_an_unknown_value() {
        use darkmux_types::config_enum::ENUM_SETTINGS;
        let row = |key: &str| -> Check {
            check_enum_settings().into_iter().find(|c| c.name == key).expect("a row for the setting")
        };
        assert_eq!(check_enum_settings().len(), ENUM_SETTINGS.len());
        for s in ENUM_SETTINGS {
            // Clean: the shipped value, Pass.
            let prev = s.env.map(|v| (v, std::env::var(v).ok()));
            if let Some(v) = s.env {
                unsafe { std::env::remove_var(v) };
            }
            assert_eq!(row(s.key).status, Status::Pass, "{}: {:?}", s.key, row(s.key));

            // Config tier.
            {
                let _g = darkmux_types::config_access::set_config_for_test(
                    darkmux_types::config_enum::config_with_value(s, "zz-unknown"),
                );
                let path = darkmux_types::config_enum::config_path_of(s);
                let fails: Vec<Check> =
                    check_enum_settings().into_iter().filter(|c| c.status == Status::Fail).collect();
                assert_eq!(fails.len(), 1, "{}: one Fail row per bad value: {fails:?}", s.key);
                let c = fails.into_iter().next().unwrap();
                assert!(c.message.contains("`zz-unknown`") && c.message.contains(&path), "{c:?}");
                assert!(c.message.contains("config.json"), "names where it was set: {c:?}");
                // (#2947 review C6) The row's claim matches the registry:
                // the entry points that refuse, or the stated reason none do.
                match s.no_scope_reason {
                    None => {
                        for sc in s.scopes {
                            assert!(c.message.contains(sc.label()), "{}: row omits `{}`: {c:?}", s.key, sc.label());
                        }
                    }
                    Some(reason) => {
                        assert!(c.message.contains(reason), "{}: row omits the no-scope reason: {c:?}", s.key);
                        assert!(!c.message.contains("Refused at preflight"), "{}: claims a refusal: {c:?}", s.key);
                    }
                }
                let hint = c.hint.clone().unwrap_or_default();
                for (t, _) in s.values {
                    assert!(hint.contains(t), "{}: valid value `{t}` missing from {hint}", s.key);
                }
            }
            // Env tier.
            if let Some(v) = s.env {
                unsafe { std::env::set_var(v, "zz-unknown-env") };
                let c = row(s.key);
                unsafe { std::env::remove_var(v) };
                assert_eq!(c.status, Status::Fail, "{}: {c:?}", s.key);
                assert!(c.message.contains("`zz-unknown-env`") && c.message.contains(v), "{c:?}");
            }
            if let Some((v, Some(old))) = prev {
                unsafe { std::env::set_var(v, old) };
            }
        }
    }


    // (#2846) Was missing `serial` while every sibling had it. It REMOVES
    // DARKMUX_TURN_DELAY_MS, so unserialized it raced
    // `check_turn_delay_below_timeout_is_pass_and_names_provenance` and wiped
    // the 3000 that test had just set — surfacing as `0ms (from
    // DARKMUX_TURN_DELAY_MS env)`. Latent before this branch; adding a check
    // to `run()` changed the scheduling enough to expose it.
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

    // (#2779) A SIMULATED host source outranks every other verdict this
    // check can reach. The two tests below are the doctor half of the
    // facade's "never fake silently" contract; the three other surfaces
    // (the dispatch's warning line, the flow-record stamp, the run
    // artifact's field) are pinned in `darkmux-crew`.
    #[test]
    fn describe_host_probe_warns_loudly_when_the_readings_are_simulated() {
        // A perfectly healthy probe — every source resolved, fast. Without
        // the simulated branch this is an unqualified Pass, and an operator
        // reading it would believe the governor was watching THIS machine.
        let src = darkmux_crew::host_probe::HostProbeSources {
            simulated: true,
            mach: true,
            ioreport: true,
            freq_tables: true,
            thermal: true,
            ioreg_gpu: true,
            battery: true,
        };
        let warning = darkmux_crew::host_source::Provenance::Scripted {
            path: "/tmp/hot.jsonl".into(),
            frames: 4,
            span_ms: 600_000,
        }
        .warning();
        let check = describe_host_probe(src, 7, warning);
        assert_eq!(
            check.status,
            Status::Warn,
            "a machine reporting `nominal` while it actually cooks is worse than no governor \
             at all — a simulated source must never read as a healthy Pass: {}",
            check.message
        );
        assert!(check.message.contains("SIMULATED"), "{}", check.message);
        assert!(
            check.message.contains("/tmp/hot.jsonl"),
            "the check must NAME the file, or the operator cannot find what is lying to them: {}",
            check.message
        );
        assert!(
            check.hint.as_deref().is_some_and(|h| h.contains("DARKMUX_HOST_SOURCE_SCRIPT")),
            "and the hint must name the knob that turns it off: {:?}",
            check.hint
        );
        assert!(
            check.message.contains("thermal(scenario)") && check.message.contains("battery(scenario)"),
            "the source list must mark WHICH sources are scripted — on a host with no thermal \
             sensor and no battery, an unqualified `thermal + battery` in this line is a \
             claim about the HARDWARE that is simply false: {}",
            check.message
        );
        assert!(
            !check.message.contains("mach(scenario)"),
            "and must mark only the two the facade actually supplies — CPU, memory, GPU and \
             power still come from the real probe: {}",
            check.message
        );
    }

    #[test]
    fn describe_host_probe_reports_a_scenario_that_could_not_be_loaded() {
        // Nothing is simulated here — the process fell back to real
        // hardware, which is the safe direction. It still must not be
        // silent: an operator who believes a scenario is driving the run
        // and is actually watching real readings will misread everything
        // that follows.
        let src = darkmux_crew::host_probe::HostProbeSources {
            simulated: false,
            mach: true,
            ioreport: true,
            freq_tables: true,
            thermal: true,
            ioreg_gpu: true,
            battery: true,
        };
        let warning = darkmux_crew::host_source::Provenance::ScriptedUnavailable {
            path: "/nope.jsonl".into(),
            error: "No such file or directory (os error 2)".into(),
        }
        .warning();
        let check = describe_host_probe(src, 7, warning);
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("/nope.jsonl"), "{}", check.message);
        assert!(
            check.message.contains("REAL hardware"),
            "it must say which way the fallback went: {}",
            check.message
        );
    }

    /// The degradation combinations the live probe cannot produce on a
    /// healthy Mac — and the ones most worth pinning, since a private
    /// framework whose path has already moved once will move again. Pure, so
    /// they run on every platform.
    #[test]
    fn describe_host_probe_names_a_missing_ioreport_rather_than_hiding_it() {
        let src = darkmux_crew::host_probe::HostProbeSources { simulated: false,
            mach: true,
            ioreport: false,
            freq_tables: false,
            thermal: true,
            ioreg_gpu: true,
            battery: true,
        };
        let check = describe_host_probe(src, 3, None);
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
    fn describe_host_probe_names_a_desktops_absent_battery_without_calling_it_a_fault() {
        // (#2705/#2706) A Mac Studio/mini/Pro has no battery, and that is a
        // PROPERTY OF THE HOST, not a gap. It must be NAMED (so an operator
        // can tell "this Mac has no battery" from "darkmux forgot to read
        // it" — the exact distinction #2706's inert gate turns on) and it
        // must not downgrade the check.
        let src = darkmux_crew::host_probe::HostProbeSources { simulated: false,
            mach: true,
            ioreport: true,
            freq_tables: true,
            thermal: true,
            ioreg_gpu: true,
            battery: false,
        };
        let check = describe_host_probe(src, 7, None);
        assert_eq!(check.status, Status::Pass, "a desktop is healthy: {}", check.message);
        assert!(
            check.message.contains("unavailable: battery"),
            "the absence must be named, not silently omitted: {}",
            check.message
        );
    }

    #[test]
    fn describe_host_probe_warns_when_nothing_resolved() {
        let check = describe_host_probe(darkmux_crew::host_probe::HostProbeSources::default(), 0, None);
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("no host sources resolved"), "{}", check.message);
    }

    #[test]
    fn describe_host_probe_warns_when_mach_itself_is_missing() {
        // Without tick counters there is no CPU figure at all — a real gap
        // even when every other source is fine.
        let src = darkmux_crew::host_probe::HostProbeSources { simulated: false,
            mach: false,
            ioreport: true,
            freq_tables: true,
            thermal: true,
            ioreg_gpu: true,
            battery: true,
        };
        let check = describe_host_probe(src, 4, None);
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("unavailable: mach"), "{}", check.message);
    }

    #[test]
    fn describe_host_probe_omits_the_unavailable_clause_when_all_resolved() {
        let src = darkmux_crew::host_probe::HostProbeSources { simulated: false,
            mach: true,
            ioreport: true,
            freq_tables: true,
            thermal: true,
            ioreg_gpu: true,
            battery: true,
        };
        let check = describe_host_probe(src, 9, None);
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

    // ─── (#3074) inactivity timeout: 0 is unbounded, and doctor says so ───

    /// Scopes `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` (and optionally another
    /// variable) around one closure, restoring both afterward.
    fn with_inactivity_env<T>(inactivity: Option<&str>, other: Option<(&str, &str)>, f: impl FnOnce() -> T) -> T {
        let k = "DARKMUX_INACTIVITY_TIMEOUT_SECONDS";
        let prev = std::env::var(k).ok();
        let prev_other = other.map(|(name, _)| (name, std::env::var(name).ok()));
        unsafe {
            match inactivity {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
            if let Some((name, v)) = other {
                std::env::set_var(name, v);
            }
        }
        let out = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
            if let Some((name, prev)) = prev_other {
                match prev {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
        out
    }

    #[serial_test::serial]
    #[test]
    fn check_inactivity_timeout_zero_is_pass_and_says_unbounded() {
        let check = with_inactivity_env(Some("0"), None, check_inactivity_timeout);
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("unbounded") && check.message.contains("env"), "{}", check.message);
    }

    #[serial_test::serial]
    #[test]
    fn check_inactivity_timeout_names_the_value_and_its_provenance() {
        let default = with_inactivity_env(None, None, check_inactivity_timeout);
        assert!(default.message.contains("600s") && default.message.contains("default"), "{}", default.message);
        let env = with_inactivity_env(Some("45"), None, check_inactivity_timeout);
        assert!(env.message.contains("45s") && env.message.contains("env"), "{}", env.message);
    }

    /// With no deadline there is nothing for a rest to approach, so the
    /// "at or above half the inactivity timeout" warning must stay quiet.
    #[serial_test::serial]
    #[test]
    fn check_turn_delay_does_not_warn_against_an_unbounded_inactivity_timeout() {
        let check = with_inactivity_env(Some("0"), Some(("DARKMUX_TURN_DELAY_MS", "3000")), check_turn_delay);
        unsafe { std::env::remove_var("DARKMUX_TURN_DELAY_MS") };
        assert_eq!(check.status, Status::Pass, "{}", check.message);
    }

    #[serial_test::serial]
    #[test]
    fn check_generation_checkpoint_interval_does_not_warn_against_an_unbounded_inactivity_timeout() {
        let check = with_inactivity_env(
            Some("0"),
            Some(("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL", "6000")),
            check_generation_checkpoint_interval,
        );
        unsafe { std::env::remove_var("DARKMUX_RUNTIME_GENERATION_CHECKPOINT_INTERVAL") };
        assert_ne!(check.status, Status::Warn, "{}", check.message);
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

    // ─── (#2928) check_live_channel — cadence, provenance, clamp, off ─

    fn live_channel_check_with(env: Option<&str>) -> Check {
        let k = "DARKMUX_LIVE_SAMPLE_MS";
        let prev = std::env::var(k).ok();
        unsafe {
            match env {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let check = check_live_channel();
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
    fn check_live_channel_names_the_cadence_the_clamp_and_off() {
        let d = live_channel_check_with(None);
        assert_eq!(d.status, Status::Pass, "{}", d.message);
        assert!(d.message.starts_with("250 ms (default)"), "{}", d.message);
        assert!(d.message.contains("never written to the flow log"), "{}", d.message);
        let e = live_channel_check_with(Some("500"));
        assert!(e.message.starts_with("500 ms (from DARKMUX_LIVE_SAMPLE_MS env)"), "{}", e.message);
        let clamped = live_channel_check_with(Some("20"));
        assert_eq!(clamped.status, Status::Warn, "a clamp is reported, never silent");
        assert!(clamped.message.contains("Configured 20 ms was clamped to 100 ms"), "{}", clamped.message);
        assert!(clamped.hint.as_deref().unwrap_or("").contains("runtime.live_sample_ms"));
        let off = live_channel_check_with(Some("0"));
        assert_eq!(off.status, Status::Pass);
        assert!(off.message.starts_with("off (from DARKMUX_LIVE_SAMPLE_MS env)"), "{}", off.message);
    }

    /// (#2928 review, C3) The socket agreement cases, through the pure
    /// classifier.
    #[test]
    fn classify_live_channel_names_stale_mismatch_and_loss() {
        use darkmux_flow::live::{socket_fingerprint, SocketState};
        use darkmux_types::config_access::{LiveCadence, Source};
        let c = LiveCadence { configured_ms: 250, effective_ms: 250, source: Source::BuiltIn };
        let sock = std::path::Path::new("/h/run/live-8765.sock");
        let ours = DaemonLiveSocket { socket_id: socket_fingerprint(sock), socket_port: 8765, bound: true };
        let ok = classify_live_channel(c, Some(sock), Some(SocketState::Listening), Some(&ours), 8765, &[19491]);
        assert_eq!(ok.status, Status::Pass, "{}", ok.message);
        assert!(ok.message.contains("a daemon is receiving"));
        let stale = classify_live_channel(c, Some(sock), Some(SocketState::Stale), None, 8765, &[]);
        assert_eq!(stale.status, Status::Warn);
        assert!(stale.message.contains("killed without removing its socket"), "{}", stale.message);
        // (#2928 re-review, MF-B) A daemon started with `--port 19491` while
        // serve.port is 8765: found by listing the home's sockets.
        let port = classify_live_channel(c, Some(sock), Some(SocketState::Absent), None, 8765, &[19491]);
        assert_eq!(port.status, Status::Warn);
        assert!(port.message.contains("port 19491") && port.message.contains("port 8765"), "{}", port.message);
        assert!(port.hint.as_deref().unwrap_or("").contains("serve.port 19491"), "{:?}", port.hint);
        // A daemon on the configured port answering for a different home.
        let other_home = DaemonLiveSocket { socket_id: socket_fingerprint(std::path::Path::new("/other/run/live-8765.sock")), socket_port: 8765, bound: true };
        let mm = classify_live_channel(c, Some(sock), Some(SocketState::Absent), Some(&other_home), 8765, &[]);
        assert_eq!(mm.status, Status::Warn);
        assert!(mm.message.contains("DARKMUX_HOME"), "{}", mm.message);
        let lost = DaemonLiveSocket { bound: false, ..ours.clone() };
        let l = classify_live_channel(c, Some(sock), Some(SocketState::Listening), Some(&lost), 8765, &[]);
        assert!(l.message.contains("lost its socket"), "{}", l.message);
        let none = classify_live_channel(c, None, None, None, 8765, &[]);
        assert!(none.message.contains("no private socket path"), "{}", none.message);
        assert_eq!(
            parse_daemon_live_socket(r#"{"live":{"ingest":{"socket_id":"ab","socket_port":8765,"bound":true}}}"#),
            Some(DaemonLiveSocket { socket_id: "ab".into(), socket_port: 8765, bound: true })
        );
        assert_eq!(parse_daemon_live_socket(r#"{"live":{"ingest":null}}"#), None, "an older daemon says nothing");
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
            check.message.contains("limits.concurrent_calls"),
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

    // ─── (#2765) check_serve_address / the resolved daemon locator ────────

    /// The row exists because the failure is INVISIBLE from the host: a
    /// daemon serving happily on a configured port while every client
    /// probes the built-in one. It must name the RESOLVED address and where
    /// each half came from, and — being a pure config read — must answer
    /// even with no daemon running, which is exactly when it is asked.
    #[serial_test::serial]
    #[test]
    fn check_serve_address_names_the_resolved_address_and_its_provenance() {
        let prev = std::env::var("DARKMUX_SERVE_PORT").ok();
        unsafe { std::env::remove_var("DARKMUX_SERVE_PORT") };
        let check = check_serve_address();
        assert_eq!(check.status, Status::Pass);
        assert!(check.message.contains("127.0.0.1:8765"), "{}", check.message);
        assert!(check.message.contains("port default"), "{}", check.message);

        unsafe { std::env::set_var("DARKMUX_SERVE_PORT", "8799") };
        let check = check_serve_address();
        assert!(
            check.message.contains("127.0.0.1:8799"),
            "must report the RESOLVED port, not the built-in: {}",
            check.message
        );
        assert!(
            check.message.contains("DARKMUX_SERVE_PORT"),
            "must name the tier that won, so the operator never wonders \
             where the value came from: {}",
            check.message
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_SERVE_PORT", v),
                None => std::env::remove_var("DARKMUX_SERVE_PORT"),
            }
        }
    }

    /// (#2782 MF2) The row prints the address its own LABEL describes.
    ///
    /// The first cut printed `serve_client_addr()` under a provenance
    /// string built from `serve_bind_with_source()`, so with
    /// `DARKMUX_SERVE_BIND=0.0.0.0` it rendered `127.0.0.1:8765 (port
    /// default, bind from DARKMUX_SERVE_BIND env)` — the value the operator
    /// had just set appeared nowhere, on the row added specifically so
    /// provenance is never in doubt. Someone debugging "why can't the
    /// tailnet reach my daemon" reads that as their bind not having taken.
    #[serial_test::serial]
    #[test]
    fn check_serve_address_prints_the_bind_it_names_and_the_probe_address_too() {
        let prev_bind = std::env::var("DARKMUX_SERVE_BIND").ok();
        let prev_port = std::env::var("DARKMUX_SERVE_PORT").ok();
        unsafe {
            std::env::set_var("DARKMUX_SERVE_BIND", "0.0.0.0");
            std::env::remove_var("DARKMUX_SERVE_PORT");
        }

        let check = check_serve_address();
        assert!(
            check.message.contains("0.0.0.0:8765"),
            "the row names DARKMUX_SERVE_BIND as the source, so the value from \
             it has to be what is printed: {}",
            check.message
        );
        assert!(
            check.message.contains("DARKMUX_SERVE_BIND"),
            "provenance still named: {}",
            check.message
        );
        assert!(
            check.message.contains("clients on this machine probe 127.0.0.1:8765"),
            "a wildcard bind is not a destination — the probe address is real \
             and wanted, it just is not the listen address: {}",
            check.message
        );

        // A SPECIFIC bind has one address, and the row must not pad it with a
        // redundant second copy of the same string.
        unsafe { std::env::set_var("DARKMUX_SERVE_BIND", "127.0.0.1") };
        let check = check_serve_address();
        assert!(
            !check.message.contains("clients on this machine probe"),
            "listen == probe, so there is nothing to disambiguate: {}",
            check.message
        );

        unsafe {
            match prev_bind {
                Some(v) => std::env::set_var("DARKMUX_SERVE_BIND", v),
                None => std::env::remove_var("DARKMUX_SERVE_BIND"),
            }
            match prev_port {
                Some(v) => std::env::set_var("DARKMUX_SERVE_PORT", v),
                None => std::env::remove_var("DARKMUX_SERVE_PORT"),
            }
        }
    }

    /// The reachability probe must follow the SAME resolution. Probing
    /// 127.0.0.1:8765 while the daemon listens elsewhere would report
    /// "not reachable" about a healthy daemon — worse than not checking.
    /// Proved against a real listener on an ephemeral port, so the
    /// assertion is about the probe's destination and not about a string.
    #[serial_test::serial]
    #[test]
    fn check_daemon_reachable_probes_the_configured_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let prev = std::env::var("DARKMUX_SERVE_PORT").ok();
        unsafe { std::env::set_var("DARKMUX_SERVE_PORT", port.to_string()) };

        let check = check_daemon_reachable();
        // Nothing speaks HTTP on that listener, so the verdict is a Warn —
        // but the ADDRESS in the message is the point: it proves the probe
        // went where the config said, not to the built-in literal.
        assert!(
            check.message.contains(&format!("127.0.0.1:{port}")),
            "the probe must target the resolved address: {}",
            check.message
        );
        assert!(
            !check.message.contains("127.0.0.1:8765"),
            "the built-in literal must not appear when the config names \
             another port: {}",
            check.message
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_SERVE_PORT", v),
                None => std::env::remove_var("DARKMUX_SERVE_PORT"),
            }
        }
    }

    /// (#2782 C5) The FRESHNESS check probes too, and it was unpinned:
    /// reverting it to the literal `loopback_http_body("127.0.0.1", 8765,
    /// "/health")` left all 294 tests in this crate green. A surviving
    /// mutation is not a guard.
    ///
    /// Same ephemeral-listener shape as the reachability test above, but
    /// this one has to ANSWER, because a probe that connects to nothing
    /// yields `not_applicable` ("no daemon running") — which is also what
    /// the mutated code produces when nothing happens to hold 8765. So the
    /// fake daemon returns a legacy `/health` body carrying a sentinel
    /// version, and the assertion is that the sentinel came back. That
    /// stays red under the mutation in BOTH worlds: nothing on 8765 (no
    /// daemon ⇒ `not_applicable`), or the operator's REAL daemon on 8765
    /// (a modern build ⇒ some other message).
    #[serial_test::serial]
    #[test]
    fn check_daemon_freshness_probes_the_configured_port() {
        const SENTINEL: &str = "0.0.0-freshness-port-probe";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // The accept is DEADLINED, not blocking. Under the mutation this
        // test exists to catch, the probe goes to 8765 and never connects
        // here — a bare `accept()` would then block forever and the join
        // below would turn a clean RED into a hang, which is the worst
        // shape a guard can have (`.config/nextest.toml`'s whole reason for
        // existing). Non-blocking poll + a deadline makes the mutated run
        // FAIL in a couple of seconds instead.
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                            .ok();
                        // Read the request line so the client's write
                        // completes, then answer and hang up (`Connection:
                        // close` is what makes `loopback_http_body`'s
                        // read_to_end terminate).
                        let mut scratch = [0u8; 1024];
                        let _ = std::io::Read::read(&mut stream, &mut scratch);
                        let body = format!("{{\"darkmux_version\":\"0.0.0\",\"build\":\"{SENTINEL}\"}}");
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        return;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            }
        });

        let prev = std::env::var("DARKMUX_SERVE_PORT").ok();
        unsafe { std::env::set_var("DARKMUX_SERVE_PORT", port.to_string()) };
        let check = check_daemon_freshness();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_SERVE_PORT", v),
                None => std::env::remove_var("DARKMUX_SERVE_PORT"),
            }
        }
        let _ = server.join();

        assert_eq!(
            check.status,
            Status::Warn,
            "a daemon with a different build answered on the CONFIGURED port, so \
             the check must have reached it: {check:?}"
        );
        assert!(
            check.message.contains(SENTINEL),
            "the verdict must be about the daemon on the configured port, not \
             whatever holds the built-in literal: {}",
            check.message
        );
    }

    // ─── (#2653) check_liveness_retention ───

    #[serial_test::serial]
    #[test]
    fn check_liveness_retention_reports_default_window_and_zero_files() {
        with_isolated_liveness_dir(|| {
            let check = check_liveness_retention();
            assert_eq!(check.status, Status::Pass, "{}", check.message);
            assert!(check.message.contains("0 heartbeat file"), "{}", check.message);
            assert!(check.message.contains("168h"), "{}", check.message);
            assert!(check.message.contains("(default)"), "{}", check.message);
        });
    }

    #[serial_test::serial]
    #[test]
    fn check_liveness_retention_counts_only_pid_named_log_files() {
        with_isolated_liveness_dir(|| {
            let dir = darkmux_types::config_access::liveness_dir();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("123.log"), "hi").unwrap();
            std::fs::write(dir.join("456.log"), "hi").unwrap();
            // Neither counted: not a pid name, and not a `.log` file.
            std::fs::write(dir.join("not-a-pid.log"), "hi").unwrap();
            std::fs::write(dir.join("host-sampler.lock"), "{}").unwrap();
            let check = check_liveness_retention();
            assert!(check.message.contains("2 heartbeat file"), "{}", check.message);
        });
    }

    #[serial_test::serial]
    #[test]
    fn check_liveness_retention_env_override_shows_in_provenance() {
        with_isolated_liveness_dir(|| {
            let k = "DARKMUX_LIVENESS_RETENTION_HOURS";
            let prev = std::env::var(k).ok();
            unsafe { std::env::set_var(k, "24") };
            let check = check_liveness_retention();
            assert!(check.message.contains("24h"), "{}", check.message);
            assert!(check.message.contains("DARKMUX_LIVENESS_RETENTION_HOURS env"), "{}", check.message);
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        });
    }

    #[serial_test::serial]
    #[test]
    fn check_liveness_retention_survives_a_wrong_typed_sibling_field() {
        // (#2653 MUST FIX 2) `max_turns: "oops"` is a KNOWN field with the
        // wrong JSON type — nothing to do with liveness at all. Before the
        // fix, `DarkmuxConfig::load_from`'s whole-document strict
        // `serde_json::from_str` failed on THIS field and fell back to an
        // all-`None` default, silently discarding the correctly-typed
        // `liveness_retention_hours: 1` sitting right next to it. This
        // check then printed "168h ... (default)" while the real prune
        // pass (reading the same file through `dispatch_liveness`'s own
        // raw peek) kept pruning on `1`. Reproduces the reviewer's exact
        // config shape.
        with_isolated_liveness_dir(|| {
            let home = std::env::var("DARKMUX_HOME").expect("set by with_isolated_liveness_dir");
            std::fs::write(
                std::path::Path::new(&home).join("config.json"),
                r#"{"runtime":{"liveness_retention_hours":1,"max_turns":"oops"}}"#,
            )
            .unwrap();

            // Sanity: the strict-typed struct really does drop the field —
            // proves this test exercises the drift, not a no-op.
            let strict =
                darkmux_types::config::DarkmuxConfig::load_from(&std::path::PathBuf::from(&home).join("config.json"));
            assert!(
                strict.runtime.is_none(),
                "sanity: the wrong-typed sibling field must fail the WHOLE strict parse"
            );

            let check = check_liveness_retention();
            assert!(check.message.contains("1h"), "{}", check.message);
            assert!(check.message.contains("from config.json"), "{}", check.message);
            assert!(!check.message.contains("(default)"), "{}", check.message);
        });
    }

    #[serial_test::serial]
    #[test]
    fn check_liveness_retention_zero_hours_warns_instead_of_silently_wiping() {
        // (#2653 MUST FIX 6) `0` means "pruning disabled" (this codebase's
        // own zero-means-off convention), not "retain nothing" — and an
        // operator who writes `0` expecting the former deserves a loud
        // Warn, not a Pass that hides the fact their directory is about to
        // grow unbounded.
        with_isolated_liveness_dir(|| {
            let k = "DARKMUX_LIVENESS_RETENTION_HOURS";
            let prev = std::env::var(k).ok();
            unsafe { std::env::set_var(k, "0") };
            let check = check_liveness_retention();
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
            assert_eq!(check.status, Status::Warn, "{}", check.message);
            assert!(check.message.contains("0h"), "{}", check.message);
            assert!(
                check.hint.as_deref().is_some_and(|h| h.contains("disables")),
                "{:?}",
                check.hint
            );
        });
    }

    // ─── (#2413) check_host_sampler — singleton lock Pass/Warn/Warn ───

    /// Isolate `host_sampler_lock_path()` to a fresh tempdir for the
    /// duration of `f`, restoring `DARKMUX_HOME` afterward. Serialized
    /// (env mutation) like every other doctor env test in this file.
    fn with_isolated_liveness_dir(f: impl FnOnce()) {
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
        f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn check_host_sampler_no_lock_file_is_pass() {
        with_isolated_liveness_dir(|| {
            let check = check_host_sampler();
            assert_eq!(check.status, Status::Pass, "{}", check.message);
            assert!(check.message.contains("no sampler active"), "{}", check.message);
        });
    }

    #[serial_test::serial]
    #[test]
    fn check_host_sampler_fresh_lock_is_pass_and_names_pid_owner_interval() {
        with_isolated_liveness_dir(|| {
            let guard = darkmux_crew::host_sampler_lock::try_acquire("daemon", 5000)
                .expect("nothing else holds the lock");
            let check = check_host_sampler();
            assert_eq!(check.status, Status::Pass, "{}", check.message);
            assert!(check.message.contains(&std::process::id().to_string()), "{}", check.message);
            assert!(check.message.contains("daemon"), "{}", check.message);
            assert!(check.message.contains("5000ms"), "{}", check.message);
            drop(guard);
        });
    }

    #[serial_test::serial]
    #[test]
    fn check_host_sampler_stale_heartbeat_is_warn() {
        with_isolated_liveness_dir(|| {
            // A lock whose heartbeat is far older than 3x its own interval,
            // owned by OUR OWN (alive) pid — so this specifically exercises
            // the heartbeat-staleness branch, not the dead-pid branch.
            darkmux_crew::host_sampler_lock::write_lock_state_for_test(
                &darkmux_crew::host_sampler_lock::LockState {
                    pid: std::process::id(),
                    machine_uid: None,
                    started_ts_ms: 0,
                    heartbeat_ts_ms: 0,
                    interval_ms: 1000,
                    owner: "daemon".to_string(),
                },
            );
            let check = check_host_sampler();
            assert_eq!(check.status, Status::Warn, "{}", check.message);
            assert!(check.message.contains("stale"), "{}", check.message);
        });
    }

    #[serial_test::serial]
    #[test]
    fn check_host_sampler_dead_pid_is_warn() {
        with_isolated_liveness_dir(|| {
            // A pid that (almost certainly) does not exist, with a FRESH
            // heartbeat — isolates the dead-pid branch from the staleness
            // branch above.
            let now = darkmux_crew::host_sampler_lock::epoch_ms_now();
            darkmux_crew::host_sampler_lock::write_lock_state_for_test(
                &darkmux_crew::host_sampler_lock::LockState {
                    pid: 999_999,
                    machine_uid: None,
                    started_ts_ms: now,
                    heartbeat_ts_ms: now,
                    interval_ms: 5000,
                    owner: "dispatch".to_string(),
                },
            );
            let check = check_host_sampler();
            assert_eq!(check.status, Status::Warn, "{}", check.message);
            assert!(check.message.contains("dead"), "{}", check.message);
        });
    }

    #[serial_test::serial]
    #[test]
    fn check_host_sampler_a_declined_dispatch_first_attempt_against_a_daemon_held_lock_is_pass() {
        // (#2413 round 3 MF1) A daemon-owned fresh lock, then a dispatch's
        // FIRST acquisition attempt against it — the exact scenario that
        // used to warn "two live pids" for the healthy steady state (every
        // dispatch start under a running daemon). The contention channel
        // is retired; this must read Pass, naming the daemon as the live
        // holder, not Warn.
        with_isolated_liveness_dir(|| {
            let guard = darkmux_crew::host_sampler_lock::try_acquire("daemon", 5000)
                .expect("nothing else holds the lock");
            // A distinct (simulated) pid is required — one test binary is
            // one real OS process, so two `try_acquire` calls here would
            // otherwise share a pid and never exercise the "a DIFFERENT
            // process holds it" branch.
            let other_pid = std::process::id().wrapping_add(1);
            let declined = darkmux_crew::host_sampler_lock::try_acquire_as_for_test(other_pid, "dispatch", 5000);
            assert!(declined.is_none(), "a fresh daemon-held lock is not stealable by a live dispatch");
            let check = check_host_sampler();
            assert_eq!(check.status, Status::Pass, "{}", check.message);
            assert!(check.message.contains("daemon"), "names the live holder: {}", check.message);
            assert!(!check.message.to_lowercase().contains("two live pids"), "{}", check.message);
            drop(guard);
        });
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
            writer_schema_version: Some("1.19.0".into()),
            torn_tails: Vec::new(),
            chain_restarted: false,
        }
    }

    #[test]
    fn summarize_audit_reports_torn_tail_is_warn_naming_the_sidecar() {
        let torn = darkmux_flow::IntegrityReport {
            torn_tails: vec!["/audit/2026-08-11.jsonl.torn-1790000000000".into()],
            chain_restarted: false,
            ..mk_clean_report(3)
        };
        let check = summarize_audit_reports(&[torn]);
        assert_eq!(check.status, Status::Warn, "a set-aside torn tail is a caveat, not a break");
        assert!(
            check.message.contains("2026-08-11.jsonl.torn-1790000000000"),
            "the sidecar must be named: {}",
            check.message
        );
    }

    /// Unknown actions in the archive warn with their count and names, a
    /// spelling 4.0 retired among them; none passes.
    #[test]
    fn unknown_flow_actions_warn_with_count_and_names_and_none_pass() {
        let mut tally = darkmux_flow::reader::UnknownActions::default();
        assert_eq!(unknown_flow_actions_check(&tally).status, Status::Pass);
        // flow-action-guard:allow — an old spelling is this test's input
        for a in ["future.thing", "future.thing", "dispatch start", "other.x"] {
            tally.observe(&serde_json::json!({ "action": a }));
        }
        let check = unknown_flow_actions_check(&tally);
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.starts_with("4 record(s)"), "{}", check.message);
        assert!(check.message.contains("dispatch start (1), future.thing (2), other.x (1)"), "{}", check.message);
    }

    #[test]
    fn summarize_audit_reports_unrecognized_header_gets_a_neutral_hint() {
        let old = darkmux_flow::IntegrityReport {
            chain_valid: false,
            break_at_line: Some(1),
            break_reason: Some("the header names no `hash_format`".into()),
            ..mk_clean_report(0)
        };
        let check = summarize_audit_reports(&[old]);
        assert_eq!(check.status, Status::Fail);
        let hint = check.hint.expect("a hint");
        assert!(hint.contains("before 2.6.0") && hint.contains("Archive"), "{hint}");
        assert!(!hint.contains("edited") || hint.contains("not evidence of editing"), "{hint}");
        assert!(!hint.contains("tampering"), "{hint}");
        // A real break keeps the editing hint.
        let broken = darkmux_flow::IntegrityReport {
            chain_valid: false,
            break_at_line: Some(4),
            break_reason: Some("hash mismatch".into()),
            ..mk_clean_report(3)
        };
        let hint = summarize_audit_reports(&[broken]).hint.expect("a hint");
        assert!(hint.contains("has been edited"), "{hint}");
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
             doctor's exit code to 1, and it must not be softened by a torn tail elsewhere"
        );
        assert!(check.message.contains("BROKEN"));
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
    fn an_unrecognized_pause_at_is_one_fail_row_not_two() {
        // (#2110/#2109 review finding 6) A typo'd pause_at silently
        // inverted the governor's intent. (#2947) It is now bad config:
        // the thermal row does not describe a ladder that no run will get
        // (every entry point refuses), it says so as a Fail.
        let prev = std::env::var("DARKMUX_THERMAL_PAUSE_AT").ok();
        unsafe { std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", "seroius") };

        // (#2947) One row per bad value: the thermal row steps aside and
        // the generic `runtime.thermal.pause_at` row is the Fail.
        assert!(check_thermal_governor().is_none(), "a second row for the same bad value");
        let generic = check_enum_settings().into_iter().find(|c| c.name == "runtime.thermal.pause_at").unwrap();
        assert_eq!(generic.status, Status::Fail);
        assert!(generic.message.contains("seroius") && generic.message.contains("DARKMUX_THERMAL_PAUSE_AT"), "{}", generic.message);

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

        let check = check_thermal_governor().expect("a thermal row");
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
    fn thermal_governor_warns_when_pause_at_is_not_strictly_more_severe_than_resume_at() {
        // (#2774 review F6) `pause_at == resume_at` reaches a terminal
        // OperatorHold in about a minute from a machine that never
        // actually changed temperature — see this check's own doc for the
        // exact mechanism. Must surface as a loud Warn.
        let prev_pause = std::env::var("DARKMUX_THERMAL_PAUSE_AT").ok();
        let prev_resume = std::env::var("DARKMUX_THERMAL_RESUME_AT").ok();
        unsafe {
            std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", "fair");
            std::env::set_var("DARKMUX_THERMAL_RESUME_AT", "fair");
        }

        let check = check_thermal_governor().expect("a thermal row");
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("pause_at"), "{}", check.message);
        assert!(check.message.contains("resume_at"), "{}", check.message);

        // (#2774 round-4 MF1) **This assertion used to read `Status::Pass`
        // for `fair`/`nominal`, with the message "a real gap (fair
        // pause_at, nominal resume_at) must not warn". It was wrong, and
        // it is CHANGED rather than worked around.** A gap between the two
        // thresholds is necessary and not sufficient: `resume_at = nominal`
        // makes tier 2's duty band cover every reading below `pause_at`, so
        // the duty cycle can be entered and never exited. Measured, on this
        // exact config: 900 samples x 2000ms of `nominal` yielded
        // `DutyCycleEntered { delay_ms: 15000 }` and a pace file still
        // pacing at the end, ratcheting 15s -> 300s per turn across a
        // mission, on a machine that was cold throughout. Doctor reporting
        // **Pass** on it was the second half of the defect — and the remedy
        // text of the check right above this one recommends `nominal` as
        // one of its two answers.
        unsafe {
            std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", "fair");
            std::env::set_var("DARKMUX_THERMAL_RESUME_AT", "nominal");
        }
        let nominal_resume = check_thermal_governor().expect("a thermal row");
        assert_eq!(
            nominal_resume.status,
            Status::Warn,
            "resume_at=nominal leaves tier 2 no exit — doctor must not call it fine: {}",
            nominal_resume.message
        );
        assert!(
            nominal_resume.message.contains("tier 2"),
            "…and must name the tier that will not run: {}",
            nominal_resume.message
        );

        // The shipped default pair IS a real gap, and still passes.
        unsafe {
            std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", "serious");
            std::env::set_var("DARKMUX_THERMAL_RESUME_AT", "fair");
        }
        let ok_check = check_thermal_governor().expect("a thermal row");
        assert_eq!(
            ok_check.status,
            Status::Pass,
            "the default pair (serious pause_at, fair resume_at) must not warn: {}",
            ok_check.message
        );

        // Inverted (pause_at MILDER than resume_at) must also warn.
        unsafe {
            std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", "fair");
            std::env::set_var("DARKMUX_THERMAL_RESUME_AT", "serious");
        }
        let inverted = check_thermal_governor().expect("a thermal row");
        assert_eq!(inverted.status, Status::Warn, "{}", inverted.message);

        unsafe {
            match prev_pause {
                Some(v) => std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", v),
                None => std::env::remove_var("DARKMUX_THERMAL_PAUSE_AT"),
            }
            match prev_resume {
                Some(v) => std::env::set_var("DARKMUX_THERMAL_RESUME_AT", v),
                None => std::env::remove_var("DARKMUX_THERMAL_RESUME_AT"),
            }
        }
    }

    /// (#2774 round-3 C5) The touching/inverted remedy must describe what
    /// NOW happens (the soft tiers are disarmed), not the pre-guard
    /// cycling-to-a-terminal-hold behavior the guard already falsified.
    #[test]
    #[serial_test::serial]
    fn the_touching_threshold_warning_describes_the_disarm_not_the_old_cycling() {
        let prev_pause = std::env::var("DARKMUX_THERMAL_PAUSE_AT").ok();
        let prev_resume = std::env::var("DARKMUX_THERMAL_RESUME_AT").ok();
        unsafe {
            std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", "fair");
            std::env::set_var("DARKMUX_THERMAL_RESUME_AT", "fair");
        }

        let check = check_thermal_governor().expect("a thermal row");
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        let lower = check.message.to_ascii_lowercase();
        assert!(
            lower.contains("disarmed"),
            "the remedy must say the soft tiers are disarmed: {}",
            check.message
        );
        assert!(
            !lower.contains("cycling"),
            "the remedy must not still describe the pre-guard cycling behavior: {}",
            check.message
        );
        assert!(
            lower.contains("breaker"),
            "…and must say what DOES still run, or the operator reads it as \"no thermal \
             protection at all\": {}",
            check.message
        );

        unsafe {
            match prev_pause {
                Some(v) => std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", v),
                None => std::env::remove_var("DARKMUX_THERMAL_PAUSE_AT"),
            }
            match prev_resume {
                Some(v) => std::env::set_var("DARKMUX_THERMAL_RESUME_AT", v),
                None => std::env::remove_var("DARKMUX_THERMAL_RESUME_AT"),
            }
        }
    }

    /// (#2774 round-3 C4, first half) A mixed-case token must not reach a
    /// Pass that claims tier 4 is enabled while the governor scores it as
    /// an unknown state. Normalizing at resolution
    /// (`config_access::thermal_pause_at`) is what closes it — this pins
    /// the OUTCOME, so removing the normalization turns the silent Pass
    /// into either a red test here or a Warn in the field, never a Pass.
    #[test]
    #[serial_test::serial]
    fn a_mixed_case_pause_at_resolves_to_a_real_state_rather_than_passing_inert() {
        let prev = std::env::var("DARKMUX_THERMAL_PAUSE_AT").ok();
        unsafe { std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", " Serious ") };

        assert_eq!(
            darkmux_types::config_access::thermal_pause_at().unwrap().as_str(),
            "serious",
            "the resolved value must be the canonical token the governor's band resolution \
             (thermal_bands::ThermalBands) matches against"
        );
        let check = check_thermal_governor().expect("a thermal row");
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(
            check.message.contains("pause at `serious`"),
            "doctor must report the value actually IN FORCE: {}",
            check.message
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", v),
                None => std::env::remove_var("DARKMUX_THERMAL_PAUSE_AT"),
            }
        }
    }

    /// (#2774 round-3 C4, second half) `pause_at = "critical"` passed with
    /// a message affirmatively claiming tier 4 was enabled, while the
    /// breaker's own rule fires first on every such reading so tiers 2/3/4
    /// could never run.
    #[test]
    #[serial_test::serial]
    fn pause_at_critical_warns_that_the_breaker_preempts_the_soft_tiers() {
        let prev = std::env::var("DARKMUX_THERMAL_PAUSE_AT").ok();
        unsafe { std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", "critical") };

        let check = check_thermal_governor().expect("a thermal row");
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(
            check.message.contains("breaker"),
            "the operator must be told WHY the ladder never runs: {}",
            check.message
        );
        assert!(
            !check.message.contains("enabled after"),
            "and must not still claim tier 4 is enabled after N episodes: {}",
            check.message
        );
        // (#2774 round-4) …and must name which tiers those are. The old
        // assertion here only checked that the string "tier 4" was ABSENT,
        // which a message saying nothing at all would also satisfy.
        assert!(
            check.message.contains("tiers 3 and 4") && check.message.contains("DISARMED"),
            "the operator must be told exactly what will not run: {}",
            check.message
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", v),
                None => std::env::remove_var("DARKMUX_THERMAL_PAUSE_AT"),
            }
        }
    }

    /// (#2774 round-4 MF1) The exact mirror image of the check above, at
    /// the other end of the enum — and the gap that let round 4's defect
    /// ship. `pause_at = critical` was warned about; `resume_at = nominal`
    /// reported **Pass**, with a committed test asserting the Pass was
    /// correct, while it wedged a cold machine into a permanent ratcheting
    /// duty cycle. Both ends are now one refusal in `ThermalBands`.
    #[test]
    #[serial_test::serial]
    fn resume_at_nominal_warns_that_tier_2_could_never_be_exited() {
        let prev_pause = std::env::var("DARKMUX_THERMAL_PAUSE_AT").ok();
        let prev_resume = std::env::var("DARKMUX_THERMAL_RESUME_AT").ok();

        for pause_at in ["fair", "serious"] {
            unsafe {
                std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", pause_at);
                std::env::set_var("DARKMUX_THERMAL_RESUME_AT", "nominal");
            }
            let check = check_thermal_governor().expect("a thermal row");
            assert_eq!(
                check.status,
                Status::Warn,
                "pause_at={pause_at}: {}",
                check.message
            );
            assert!(
                check.message.contains("tier 2") && check.message.contains("DISARMED"),
                "pause_at={pause_at}: must name the tier that will not run: {}",
                check.message
            );
            assert!(
                check.message.contains("breaker"),
                "pause_at={pause_at}: …and what DOES still run: {}",
                check.message
            );
            assert!(
                check.hint.as_deref().is_some_and(|h| h.contains("resume_at")),
                "pause_at={pause_at}: the remedy must point at the knob that is wrong: {:?}",
                check.hint
            );
        }

        unsafe {
            match prev_pause {
                Some(v) => std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", v),
                None => std::env::remove_var("DARKMUX_THERMAL_PAUSE_AT"),
            }
            match prev_resume {
                Some(v) => std::env::set_var("DARKMUX_THERMAL_RESUME_AT", v),
                None => std::env::remove_var("DARKMUX_THERMAL_RESUME_AT"),
            }
        }
    }

    /// (#2774 round-6 C2) When TWO things are wrong, the hint names both.
    ///
    /// The message already concatenates every disarm note's `why`; the
    /// hint took only `disarm_notes().first()`. On `pause_at = critical,
    /// resume_at = nominal` that sends the operator round the loop —
    /// fix the one remedy shown, re-run doctor, get a second warning about
    /// a knob that was already wrong when they ran it the first time.
    #[test]
    #[serial_test::serial]
    fn a_doubly_broken_pair_gets_a_remedy_for_every_reason_it_is_broken() {
        let prev_pause = std::env::var("DARKMUX_THERMAL_PAUSE_AT").ok();
        let prev_resume = std::env::var("DARKMUX_THERMAL_RESUME_AT").ok();
        unsafe {
            std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", "critical");
            std::env::set_var("DARKMUX_THERMAL_RESUME_AT", "nominal");
        }

        // Pre-check: this pair really does produce more than one note, so
        // the assertion below is not vacuously satisfied by a single-note
        // config that happens to mention both knobs.
        let notes = darkmux_crew::thermal_bands::ThermalBands::resolve("critical", "nominal");
        assert!(
            notes.disarm_notes().len() > 1,
            "this test needs a pair that is broken for two DIFFERENT reasons, got {:?}",
            notes.disarm_notes()
        );

        let check = check_thermal_governor().expect("a thermal row");
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        let hint = check.hint.clone().unwrap_or_default();
        for note in notes.disarm_notes() {
            assert!(
                hint.contains(note.remedy.as_str()),
                "every reason the ladder is disarmed needs its remedy in the hint — missing \
                 {:?} from {hint:?}",
                note.remedy
            );
        }

        unsafe {
            match prev_pause {
                Some(v) => std::env::set_var("DARKMUX_THERMAL_PAUSE_AT", v),
                None => std::env::remove_var("DARKMUX_THERMAL_PAUSE_AT"),
            }
            match prev_resume {
                Some(v) => std::env::set_var("DARKMUX_THERMAL_RESUME_AT", v),
                None => std::env::remove_var("DARKMUX_THERMAL_RESUME_AT"),
            }
        }
    }

    /// Restores one env var to its prior value on drop — the #2774
    /// round-8 tests below each mutate two or three of them and every
    /// early `assert!` between the set and the restore would otherwise
    /// leak the mutation into the next `#[serial]` test in this module.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            // SAFETY: every caller is `#[serial_test::serial]`.
            unsafe { std::env::set_var(key, value) };
            EnvGuard { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: every caller is `#[serial_test::serial]`.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// (#2774 round-8 MF1, first half) `max_pause_ms = 0` means an
    /// UNBOUNDED episode — `pause_episode_exhausted` returns `false`
    /// forever at `0`, so the breaker never takes the handoff. Doctor
    /// interpolated the raw number and told the operator the opposite:
    /// "breaker after 0ms of one pause episode", in the same sentence
    /// where `episode_threshold = 0` was correctly spelled out as
    /// unbounded.
    #[test]
    #[serial_test::serial]
    fn a_zero_max_pause_ms_reads_as_unbounded_not_as_an_instant_handoff() {
        let _enabled = EnvGuard::set("DARKMUX_THERMAL_ENABLED", "true");
        let _pause = EnvGuard::set("DARKMUX_THERMAL_PAUSE_AT", "serious");
        let _resume = EnvGuard::set("DARKMUX_THERMAL_RESUME_AT", "fair");
        let _max = EnvGuard::set("DARKMUX_THERMAL_MAX_PAUSE_MS", "0");

        let check = check_thermal_governor().expect("a thermal row");
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(
            !check.message.contains("after 0ms"),
            "doctor must not claim a handoff that `pause_episode_exhausted` makes unreachable: \
             {}",
            check.message
        );
        assert!(
            check.message.contains("never from a pause episode")
                && check.message.contains("max_pause_ms=0"),
            "…it must say the handoff never happens, and name the knob that decided it: {}",
            check.message
        );
    }

    /// (#2774 round-8 MF1, second half) `min_cpu_speed_limit_pct = 0`
    /// disables the CPU-floor trigger outright — the comparison is
    /// `pct < floor` and no reading is below zero. Doctor rendered it as
    /// "3 consecutive samples with cpu_speed_limit_pct < 0%", a condition
    /// no sample can satisfy, described as a live trigger.
    #[test]
    #[serial_test::serial]
    fn a_zero_cpu_floor_reads_as_disabled_not_as_a_live_sub_zero_trigger() {
        let _enabled = EnvGuard::set("DARKMUX_THERMAL_ENABLED", "true");
        let _pause = EnvGuard::set("DARKMUX_THERMAL_PAUSE_AT", "serious");
        let _resume = EnvGuard::set("DARKMUX_THERMAL_RESUME_AT", "fair");
        let _floor = EnvGuard::set("DARKMUX_THERMAL_MIN_CPU_SPEED_LIMIT_PCT", "0");

        let check = check_thermal_governor().expect("a thermal row");
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(
            !check.message.contains("< 0%"),
            "doctor must not print an unsatisfiable comparison as a trigger: {}",
            check.message
        );
        assert!(
            check.message.contains("never from the CPU floor")
                && check.message.contains("min_cpu_speed_limit_pct=0"),
            "…it must say the floor is off, and name the knob that decided it: {}",
            check.message
        );
        // …and must still say what DOES run, or "never / never" reads as
        // "no breaker at all" — which is false: the `critical`-state check
        // is unconditional.
        assert!(
            check.message.contains("`critical` state (immediate, always)"),
            "the one unconditional breaker trigger must survive both disables: {}",
            check.message
        );
    }

    /// (#2774 round-8 MF1) The NON-degenerate rendering, pinned so the
    /// branch above cannot be "fixed" by making every config read as
    /// disabled. A shipped-default pair must still name both live
    /// triggers with their actual numbers.
    #[test]
    #[serial_test::serial]
    fn ordinary_breaker_values_still_render_as_live_triggers_with_their_numbers() {
        let _enabled = EnvGuard::set("DARKMUX_THERMAL_ENABLED", "true");
        let _pause = EnvGuard::set("DARKMUX_THERMAL_PAUSE_AT", "serious");
        let _resume = EnvGuard::set("DARKMUX_THERMAL_RESUME_AT", "fair");
        let _max = EnvGuard::set("DARKMUX_THERMAL_MAX_PAUSE_MS", "900000");
        let _floor = EnvGuard::set("DARKMUX_THERMAL_MIN_CPU_SPEED_LIMIT_PCT", "50");
        let _hold = EnvGuard::set("DARKMUX_THERMAL_SPEED_LIMIT_HOLD_SAMPLES", "3");

        let check = check_thermal_governor().expect("a thermal row");
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(
            check.message.contains("after 900000ms of one pause episode"),
            "{}",
            check.message
        );
        assert!(
            check.message.contains("after 3 consecutive samples with cpu_speed_limit_pct < 50%"),
            "{}",
            check.message
        );
        assert!(
            !check.message.contains("never from"),
            "no trigger is disabled in this config: {}",
            check.message
        );
    }

    /// (#2774 round-8) The one knob that degenerates UPWARD.
    /// `cpu_speed_limit_pct` is a percentage whose "no cap recorded"
    /// reading is 100, so a floor above 100 makes `pct < floor` true of
    /// every sample: the breaker trips on the `speed_limit_hold_samples`th
    /// sample of EVERY dispatch and drops a `thermal-critical` STOP file
    /// on a cold machine. `darkmux config set … 500` accepted it silently
    /// and doctor reported **Pass**.
    #[test]
    #[serial_test::serial]
    fn a_cpu_floor_above_100_warns_that_every_sample_trips_the_breaker() {
        let _enabled = EnvGuard::set("DARKMUX_THERMAL_ENABLED", "true");
        let _pause = EnvGuard::set("DARKMUX_THERMAL_PAUSE_AT", "serious");
        let _resume = EnvGuard::set("DARKMUX_THERMAL_RESUME_AT", "fair");

        for value in ["101", "500"] {
            let _floor = EnvGuard::set("DARKMUX_THERMAL_MIN_CPU_SPEED_LIMIT_PCT", value);
            let check = check_thermal_governor().expect("a thermal row");
            assert_eq!(check.status, Status::Warn, "floor={value}: {}", check.message);
            assert!(
                check.message.contains("min_cpu_speed_limit_pct") && check.message.contains(value),
                "floor={value}: the warning must name the knob and the value in force: {}",
                check.message
            );
            assert!(
                check.message.contains("EVERY dispatch"),
                "floor={value}: …and what it will actually do: {}",
                check.message
            );
            assert!(
                check.hint.as_deref().is_some_and(|h| h.contains("min_cpu_speed_limit_pct")),
                "floor={value}: the remedy must point at the knob that is wrong: {:?}",
                check.hint
            );
        }

        // 100 is the TOP legal value, not a degenerate one: `pct < 100` is
        // satisfiable (a throttled machine reads below 100) and a machine
        // with no cap recorded reads exactly 100, which is NOT below it.
        // Pinned so the guard above cannot drift down onto a real setting.
        let _floor = EnvGuard::set("DARKMUX_THERMAL_MIN_CPU_SPEED_LIMIT_PCT", "100");
        let check = check_thermal_governor().expect("a thermal row");
        assert_eq!(check.status, Status::Pass, "floor=100: {}", check.message);
        assert!(
            check.message.contains("cpu_speed_limit_pct < 100%"),
            "floor=100: {}",
            check.message
        );
    }

    /// (#2774 round-9 MF1) A DISARMED band must not suppress the
    /// breaker-side warnings.
    ///
    /// The three breaker-only checks (`min_cpu_speed_limit_pct > 100`,
    /// `speed_limit_hold_samples = 0`, `ratchet_factor = 0`) each used to
    /// sit behind their own early `return` AFTER the band-disarm branch,
    /// so any disarm note reached the operator INSTEAD of all three. On
    /// `pause_at == resume_at` plus a 150% floor, doctor said "The breaker
    /// is unaffected and still runs: … after 3 consecutive samples with
    /// cpu_speed_limit_pct < 150%" — reading as ordinary hardware
    /// protection while withholding that every sample of a cold machine
    /// satisfies that comparison. None of the three reads `pause_at` or
    /// `resume_at`; the breaker is what a band disarm leaves alone.
    #[test]
    #[serial_test::serial]
    fn a_band_disarm_does_not_hide_a_breaker_that_trips_on_every_dispatch() {
        let _enabled = EnvGuard::set("DARKMUX_THERMAL_ENABLED", "true");
        // A plausible hand-edit: the two thresholds touching, which
        // `ThermalBands` refuses and reports as a disarm note.
        let _pause = EnvGuard::set("DARKMUX_THERMAL_PAUSE_AT", "fair");
        let _resume = EnvGuard::set("DARKMUX_THERMAL_RESUME_AT", "fair");
        let _floor = EnvGuard::set("DARKMUX_THERMAL_MIN_CPU_SPEED_LIMIT_PCT", "150");

        // Pre-check: this really is the suppressing case — the band is
        // disarmed, so the assertion below is not vacuously satisfied by a
        // config that simply never reached the disarm branch.
        assert!(
            !darkmux_crew::thermal_bands::ThermalBands::resolve("fair", "fair")
                .disarm_notes()
                .is_empty(),
            "this test needs a pair that DOES produce a disarm note"
        );

        let check = check_thermal_governor().expect("a thermal row");
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        // Both verdicts, in one pass — neither hides the other.
        assert!(
            check.message.contains("DISARMED"),
            "the band disarm must still be reported: {}",
            check.message
        );
        assert!(
            check.message.contains("min_cpu_speed_limit_pct")
                && check.message.contains("150")
                && check.message.contains("EVERY dispatch"),
            "…and so must the breaker that now trips on every dispatch: {}",
            check.message
        );
        // The "still runs" sentence itself must stop reading as ordinary
        // protection: the floor clause inside it may not render as a
        // plain sub-threshold trigger.
        assert!(
            !check.message.contains("consecutive samples with cpu_speed_limit_pct < 150%"),
            "the disarm sentence must not describe a trip-on-every-sample floor as an ordinary \
             threshold: {}",
            check.message
        );
        assert!(
            check
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("min_cpu_speed_limit_pct") && h.contains("resume_at")),
            "every reason this config is wrong needs its remedy: {:?}",
            check.hint
        );
    }

    /// (#2774 round-9 MF1, the other two hoisted checks) The same
    /// suppression, at the two knobs that were silently coerced rather
    /// than mis-triggering. Both are breaker/ratchet concerns that a band
    /// disarm does not touch, and both used to vanish behind it.
    #[test]
    #[serial_test::serial]
    fn a_band_disarm_does_not_hide_the_silently_coerced_knobs() {
        let _enabled = EnvGuard::set("DARKMUX_THERMAL_ENABLED", "true");
        let _pause = EnvGuard::set("DARKMUX_THERMAL_PAUSE_AT", "fair");
        let _resume = EnvGuard::set("DARKMUX_THERMAL_RESUME_AT", "fair");
        let _hold = EnvGuard::set("DARKMUX_THERMAL_SPEED_LIMIT_HOLD_SAMPLES", "0");
        let _ratchet = EnvGuard::set("DARKMUX_THERMAL_RATCHET_FACTOR", "0");

        let check = check_thermal_governor().expect("a thermal row");
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("DISARMED"), "{}", check.message);
        assert!(
            check.message.contains("speed_limit_hold_samples is 0"),
            "the coerced hold-samples knob must survive the disarm: {}",
            check.message
        );
        assert!(
            check.message.contains("ratchet_factor is 0"),
            "…and so must the coerced ratchet: {}",
            check.message
        );
        let hint = check.hint.clone().unwrap_or_default();
        for remedy in [
            "runtime.thermal.speed_limit_hold_samples 1",
            "runtime.thermal.ratchet_factor 1",
        ] {
            assert!(hint.contains(remedy), "missing {remedy:?} from {hint:?}");
        }
    }

    /// (#2774 round-9 MF1) The mangled-whitespace half of the same
    /// message. The hold-samples warning was a plain string literal split
    /// across source lines WITHOUT a `\` continuation, so its indentation
    /// shipped as a 24-space run in the middle of the operator's sentence.
    #[test]
    #[serial_test::serial]
    fn the_coerced_hold_samples_warning_is_not_mangled_by_source_indentation() {
        let _enabled = EnvGuard::set("DARKMUX_THERMAL_ENABLED", "true");
        let _pause = EnvGuard::set("DARKMUX_THERMAL_PAUSE_AT", "serious");
        let _resume = EnvGuard::set("DARKMUX_THERMAL_RESUME_AT", "fair");
        let _hold = EnvGuard::set("DARKMUX_THERMAL_SPEED_LIMIT_HOLD_SAMPLES", "0");

        let check = check_thermal_governor().expect("a thermal row");
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(
            !check.message.contains("  "),
            "no run of consecutive spaces belongs in an operator-facing sentence: {:?}",
            check.message
        );
        assert!(
            check.message.contains("trips on the first low sample"),
            "…and the sentence must still read as one: {}",
            check.message
        );
    }

    /// (#2774 round-9, the sweep's third item) `duty_delay_ms = 0` makes
    /// tier 3's ratchet a permanent no-op — `current_duty_delay_ms` starts
    /// at `duty_delay_ms` and the ratchet only ever multiplies, so
    /// `0 * factor` is 0 for the life of the run. "starts at 0ms and
    /// ratchets x2 per `serious` recovery" is literally true and reads as
    /// a live escalating tier. A note, not an arithmetic change: a zero
    /// duty delay is a legitimate "tier 2 tracks but adds no rest".
    #[test]
    #[serial_test::serial]
    fn a_zero_duty_delay_reads_as_inert_not_as_a_ratcheting_tier() {
        let _enabled = EnvGuard::set("DARKMUX_THERMAL_ENABLED", "true");
        let _pause = EnvGuard::set("DARKMUX_THERMAL_PAUSE_AT", "serious");
        let _resume = EnvGuard::set("DARKMUX_THERMAL_RESUME_AT", "fair");
        let _duty = EnvGuard::set("DARKMUX_THERMAL_DUTY_DELAY_MS", "0");
        let _ratchet = EnvGuard::set("DARKMUX_THERMAL_RATCHET_FACTOR", "2");

        let check = check_thermal_governor().expect("a thermal row");
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(
            !check.message.contains("starts at 0ms"),
            "doctor must not describe a permanently-zero delay as a starting point it grows \
             from: {}",
            check.message
        );
        assert!(
            check.message.contains("duty_delay_ms=0") && check.message.contains("inert"),
            "…it must say the tier adds no rest, and name the knob that decided it: {}",
            check.message
        );

        // The non-degenerate rendering is pinned too, so the branch above
        // cannot be "fixed" by calling every duty delay inert.
        let _live = EnvGuard::set("DARKMUX_THERMAL_DUTY_DELAY_MS", "15000");
        let live = check_thermal_governor().expect("a thermal row");
        assert_eq!(live.status, Status::Pass, "{}", live.message);
        assert!(
            live.message.contains("starts at 15000ms and ratchets x2"),
            "{}",
            live.message
        );
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

    /// `fleet.mode` changes one thing today: which address a viewer link
    /// names (`viewer_link_base`). Its doctor row says that, and never says a
    /// machine on it "coordinates nothing": a standalone-mode machine can run
    /// a fleet listener and have peers in its roster.
    #[test]
    #[serial_test::serial]
    fn the_fleet_mode_row_says_what_the_mode_drives() {
        let prev = std::env::var("DARKMUX_FLEET_MODE").ok();
        unsafe { std::env::remove_var("DARKMUX_FLEET_MODE") };
        let rows = check_enum_settings();
        unsafe {
            if let Some(v) = prev {
                std::env::set_var("DARKMUX_FLEET_MODE", v);
            }
        }
        let row = rows.iter().find(|c| c.name == "fleet.mode").expect("the fleet.mode row");
        assert!(row.message.contains("viewer links"), "{}", row.message);
        assert!(row.message.contains("card") && row.message.contains("presence"), "{}", row.message);
        assert!(!row.message.contains("coordinates nothing"), "{}", row.message);
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

    /// (#2947) A bad `fleet.mode` is not read as `standalone` silently:
    /// link rendering starts no work, so it keeps the direct address (the
    /// one link true whatever the position) and says what is wrong on
    /// stderr. Pinned here so a regression to a tailnet spawn on a typo, or
    /// a panic, is red.
    #[test]
    #[serial_test::serial]
    fn viewer_link_base_on_a_bad_fleet_mode_uses_the_direct_link() {
        let prev = std::env::var("DARKMUX_FLEET_MODE").ok();
        unsafe { std::env::set_var("DARKMUX_FLEET_MODE", "hubb") };
        darkmux_types::style::set_colorize_override(Some(true));
        assert_eq!(viewer_link_base(8765), "http://127.0.0.1:8765/");
        darkmux_types::style::set_colorize_override(None);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_FLEET_MODE", v),
                None => std::env::remove_var("DARKMUX_FLEET_MODE"),
            }
        }
    }

    /// (#2782 C4) The link's HOST follows `serve.bind`, the same as its
    /// port follows `serve.port`.
    ///
    /// #2765 closed this halfway: it threaded the resolved port through and
    /// left `127.0.0.1` hardcoded, so on a non-loopback bind doctor's own
    /// `serve address` row named one address while every link this function
    /// produced named another — two rows of the same output disagreeing,
    /// and the clickable one pointing where nothing is listening.
    ///
    /// The wildcard half is the other direction and is equally required: a
    /// bind of `0.0.0.0` is a directive, not a destination, so a LINK must
    /// still resolve back to loopback. Both cases in one test because
    /// "honors the bind" and "does not honor a wildcard" are the same rule.
    #[test]
    #[serial_test::serial]
    fn viewer_link_base_honors_the_bind_host_but_not_a_wildcard() {
        let prev_mode = std::env::var("DARKMUX_FLEET_MODE").ok();
        let prev_bind = std::env::var("DARKMUX_SERVE_BIND").ok();
        unsafe { std::env::set_var("DARKMUX_FLEET_MODE", "standalone") };
        darkmux_types::style::set_colorize_override(Some(true));

        // A documentation-range address (RFC 5737 TEST-NET-1), so nothing in
        // this repo's committed text names a real host.
        unsafe { std::env::set_var("DARKMUX_SERVE_BIND", "192.0.2.10") };
        assert_eq!(
            viewer_link_base(8765),
            "http://192.0.2.10:8765/",
            "a specific bind is where the daemon is; a loopback link would be dead"
        );

        unsafe { std::env::set_var("DARKMUX_SERVE_BIND", "0.0.0.0") };
        assert_eq!(
            viewer_link_base(8765),
            "http://127.0.0.1:8765/",
            "a wildcard is a bind directive, not somewhere a browser can go"
        );

        // An IPv6 literal has to come back bracketed or the URL is unparseable.
        unsafe { std::env::set_var("DARKMUX_SERVE_BIND", "::1") };
        assert_eq!(viewer_link_base(8765), "http://[::1]:8765/");

        darkmux_types::style::set_colorize_override(None);
        unsafe {
            match prev_mode {
                Some(v) => std::env::set_var("DARKMUX_FLEET_MODE", v),
                None => std::env::remove_var("DARKMUX_FLEET_MODE"),
            }
            match prev_bind {
                Some(v) => std::env::set_var("DARKMUX_SERVE_BIND", v),
                None => std::env::remove_var("DARKMUX_SERVE_BIND"),
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
        let got = tailnet_viewer_url_bounded("127.0.0.1", 8765, std::time::Duration::from_millis(300));
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
        Some(DaemonBuild {
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
            Some(DaemonBuild {
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
            Some(DaemonBuild {
                build: "2.0.0 (a1b2c3d)".into(),
                binary_mtime: Some(1700)
            })
        );
    }

    #[test]
    fn daemon_build_tolerates_a_daemon_that_could_not_stat_its_own_exe() {
        // `binary_mtime: null` is a real shape the daemon emits — it must parse
        // as a build without an mtime, not as no build at all.
        let body = r#"{"darkmux_version":"2.0.0","build":"2.0.0 (a1b2c3d)","binary_mtime":null}"#;
        assert_eq!(
            parse_daemon_build(body),
            Some(DaemonBuild {
                build: "2.0.0 (a1b2c3d)".into(),
                binary_mtime: None
            })
        );
    }

    #[test]
    fn daemon_build_is_none_without_a_build_field() {
        // (4.0) A daemon older than #1461 reports no `build` field. There is
        // no Legacy classification any more: without a build id there is
        // nothing to compare, the same as no daemon at all.
        let body = r#"{"darkmux_version":"1.18.5","flow_schema_version":"1.4"}"#;
        assert!(parse_daemon_build(body).is_none());
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

    fn tags(entries: &[(&str, Option<&str>)]) -> RuntimeImageProbe {
        RuntimeImageProbe::Tags(
            entries
                .iter()
                .map(|(tag, label)| LocalRuntimeTag {
                    tag: tag.to_string(),
                    label: label.map(String::from),
                })
                .collect(),
        )
    }

    #[test]
    fn runtime_image_passes_when_the_label_matches_the_binary() {
        let c = classify_runtime_image_freshness(
            tags(&[("darkmux-runtime:latest", Some("2.0.0"))]),
            "2.0.0",
        );
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.hint.is_none());
    }

    #[test]
    fn runtime_image_warns_naming_both_versions_when_the_label_is_older() {
        let c = classify_runtime_image_freshness(
            tags(&[("darkmux-runtime:latest", Some("1.18.5"))]),
            "2.0.0",
        );
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert!(c.message.contains("1.18.5"), "{}", c.message);
        assert!(c.message.contains("2.0.0"), "{}", c.message);
        let hint = c.hint.as_deref().unwrap();
        assert!(hint.contains("docker build"), "build fix: {hint}");
        // The hint must name a version the operator can paste, not a placeholder.
        assert!(hint.contains("DARKMUX_VERSION=2.0.0"), "{hint}");
        assert!(hint.contains("docker rmi darkmux-runtime:latest"), "{hint}");
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
    fn runtime_image_unlabeled_latest_warns_with_the_fix() {
        // (#2923) The Studio's shape: a weeks-old `docker build` with no
        // build-arg. It used to read "nothing to compare" and pass, while
        // every dispatch ran it. Unknown is not "fine".
        let c = classify_runtime_image_freshness(
            tags(&[("darkmux-runtime:latest", None)]),
            "3.13.0",
        );
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert!(c.message.contains("no version label"), "{}", c.message);
        assert!(
            c.message.contains("ghcr.io/kstrat2001/darkmux-runtime:3.13.0"),
            "names what runs instead: {}",
            c.message
        );
        let hint = c.hint.as_deref().unwrap();
        assert!(
            hint.contains(
                "docker build --build-arg DARKMUX_VERSION=3.13.0 -f runtime/Dockerfile -t darkmux-runtime:latest ."
            ),
            "{hint}"
        );
    }

    #[test]
    fn runtime_image_other_unlabeled_tags_are_listed_not_warned() {
        // A renamed stale image (`:stale-pre-3.13`) runs only when named, and
        // naming it is refused — so it is reported, not a warning.
        let c = classify_runtime_image_freshness(
            tags(&[
                ("darkmux-runtime:stale-pre-3.13", None),
                ("darkmux-runtime:4.0-rc", Some("4.0.0")),
            ]),
            "3.13.0",
        );
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.message.contains("darkmux-runtime:stale-pre-3.13"), "{}", c.message);
        assert!(
            !c.message.contains("4.0-rc"),
            "a labeled side-by-side tag is not noise: {}",
            c.message
        );
    }

    #[test]
    fn runtime_image_probe_parses_listing_and_pairs_labels_in_order() {
        let tags = parse_runtime_image_tags(
            "darkmux-runtime:latest\ndarkmux-runtime:<none>\ndarkmux-runtime:4.0-rc\n",
        );
        assert_eq!(tags, vec!["darkmux-runtime:latest", "darkmux-runtime:4.0-rc"]);
        let paired = pair_runtime_image_labels(&tags, "\n3.13.0\n").unwrap();
        assert_eq!(paired[0].label, None);
        assert_eq!(paired[1].label.as_deref(), Some("3.13.0"));
        // A short answer is never misattributed.
        assert!(pair_runtime_image_labels(&tags, "3.13.0\n").is_none());
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
            classify_binary_vs_source(Some("0ldc0de"), Some("a1b2c3d")),
            classify_runtime_image_freshness(tags(&[("darkmux-runtime:latest", Some("1.0.0"))]), "2.0.0"),
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

        let c = check_installed_skills_freshness(&[target], &embedded_set);
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

        let c = check_installed_skills_freshness(&[target], &embedded_set);
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

        let c = check_installed_skills_freshness(&[target], &embedded_set);
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

        let c = check_installed_skills_freshness(&[target], &embedded_set);
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.hint.is_none());
        assert!(
            c.message.contains("embedded but not installed") && c.message.contains("darkmux-beta"),
            "the not-installed skill is noted informationally: {}",
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
        // (#2923) The build fix stamps the label, or dispatch skips the image.
        assert!(c.hint.unwrap().contains(&format!(
            "docker build --build-arg DARKMUX_VERSION={} -f runtime/Dockerfile -t darkmux-runtime:latest .",
            env!("CARGO_PKG_VERSION")
        )));
    }

    #[test]
    fn docker_status_refused_image_says_refused_not_will_pull() {
        // (#2923 review C8) A present pinned image whose label contradicts its
        // tag is refused by dispatch; doctor must not promise a pull.
        use darkmux_crew::dispatch_internal::DockerRuntimeStatus;
        let c = docker_status_to_check(DockerRuntimeStatus::ImageRefused(
            "refusing to dispatch: runtime image `x` was built for darkmux 1.0.0".into(),
        ));
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("will be refused"), "{}", c.message);
        assert!(!c.message.contains("will pull"), "{}", c.message);
        assert!(c.hint.unwrap().contains("built for darkmux 1.0.0"));
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

    /// (#2452 review) Both `run()` tests below pin `DARKMUX_HOME` to an empty
    /// tempdir, and must keep doing so. `run()` calls
    /// `check_state_file_permissions`, which WALKS six state roots — and two
    /// of the six carry no test-build isolation of their own:
    /// `config_access::fleet_file()` is deliberately unguarded
    /// (`fleet_file_default`'s doc explains which CI test the guard broke)
    /// and `darkmux_crew::loader::missions_dir()` resolves through
    /// `user_state_root()`, whose crate's `test-support` feature is empty.
    /// Un-isolated, this test recursively stats the DEVELOPER's real
    /// `~/.darkmux/missions` — up to the whole scan budget — which is the
    /// #2411/#2450 failure class one directory over, and makes the test's
    /// runtime a function of the machine's own history.
    /// `every_state_root_resolves_under_an_isolated_darkmux_home` pins that
    /// `DARKMUX_HOME` is sufficient to cover all six.
    fn with_isolated_darkmux_home<T>(f: impl FnOnce() -> T) -> T {
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
        let out = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        out
    }

    fn materialized(json: &str) -> darkmux_types::ProfileRegistry {
        let mut r: darkmux_types::ProfileRegistry = serde_json::from_str(json).unwrap();
        r.materialize_endpoints();
        r
    }

    /// (#3035) A leftover `remote.*` env var names the `endpoints.<id>.limits`
    /// field that replaced it. The two spend-cap vars FAIL (the command is
    /// refused: ignoring a cap removes it); the concurrency and policy vars
    /// only warn (ignoring them is slower or quieter, never unsafe). Nothing
    /// set passes. (A leftover old `config.json` key is
    /// an unknown key, failed by the user-file keys row.)
    #[test]
    fn retired_remote_env_vars_warn_naming_the_endpoint_limit() {
        for (var, field, status, verdict) in [
            ("DARKMUX_REMOTE_MAX_TOKENS_PER_STEP", "limits.tokens_per_dispatch", Status::Fail, "is refused"),
            ("DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION", "limits.tokens_per_dispatch", Status::Fail, "is refused"),
            ("DARKMUX_REMOTE_STEP_BUDGET_POLICY", "limits.policy", Status::Warn, "is ignored"),
            ("DARKMUX_REMOTE_CONCURRENT_CAP", "limits.concurrent_calls", Status::Warn, "is ignored"),
        ] {
            let env = |k: &str| (k == var).then(|| "5".to_string());
            let c = renamed_settings_status(&env);
            assert_eq!(c.status, status, "{var}: {}", c.message);
            assert!(c.message.contains(&format!("env var {var} (5) {verdict}")), "{}", c.message);
            assert!(c.message.contains(field) && c.message.contains("endpoints.<id>"), "{}", c.message);
        }
        assert_eq!(renamed_settings_status(&|_| None).status, Status::Pass);
    }

    /// A set `DARKMUX_CREW_DIR` (retired) fails, naming `DARKMUX_HOME`.
    #[test]
    fn a_set_crew_dir_fails_naming_darkmux_home() {
        let env = |k: &str| (k == "DARKMUX_CREW_DIR").then(|| "/somewhere".to_string());
        let c = renamed_settings_status(&env);
        assert_eq!(c.status, Status::Fail, "{}", c.message);
        assert!(c.message.contains("DARKMUX_CREW_DIR") && c.message.contains("DARKMUX_HOME"), "{}", c.message);
    }

    /// (operator, 2026-10-01) A leftover darkmux only ignores warns here, as
    /// at CLI entry; mixed with one it refuses, the row still fails.
    #[test]
    fn an_ignored_leftover_warns_and_a_refused_one_still_fails() {
        let notebook = |k: &str| (k == "DARKMUX_NOTEBOOK_DIR").then(|| "/n".to_string());
        let c = renamed_settings_status(&notebook);
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert!(c.message.contains("env var DARKMUX_NOTEBOOK_DIR (/n) is ignored"), "{}", c.message);
        let both = |k: &str| matches!(k, "DARKMUX_NOTEBOOK_DIR" | "DARKMUX_CREW_DIR").then(|| "/x".to_string());
        assert_eq!(renamed_settings_status(&both).status, Status::Fail);
    }

    fn no_spend(_: &darkmux_crew::budget::EndpointBudget) -> darkmux_crew::budget::WindowEntries {
        Vec::new()
    }

    /// (#2902 steps 4 and 5) `endpoints` lists what each declared endpoint
    /// is, by name only (never a secret), with its budget policy and the
    /// spend in its rolling window, its per-dispatch cap, and how many of its
    /// calls run at once (#3035).
    #[test]
    fn endpoints_check_lists_each_endpoint_with_its_budget_and_window_spend() {
        let r = materialized(
            r#"{"profiles":{"p":{"models":[{"id":"gpt-4o","endpoint":"azure"}]}},
                "endpoints":{
                    "azure":{"url":"https://tok@r.example/openai/deployments/d","api_version":"v1",
                        "auth":{"type":"api-key","keychain":"darkmux-azure"},
                        "limits":{"tokens_per_dispatch":500000,"window":{"period":"1d","tokens":2000000}}},
                    "lms":{"managed":"lmstudio"}}}"#,
        );
        let mut asked = Vec::new();
        let c = endpoints_status(&r, &mut |b| {
            asked.push(b.endpoint_id.clone());
            vec![(1, darkmux_crew::budget::Spend::full(1_200_000)), (2, darkmux_crew::budget::Spend::full(300_000))]
        });
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert!(c.message.contains("`azure`: unmanaged, r.example"), "host only, userinfo stripped: {}", c.message);
        assert!(!c.message.contains("tok@") && !c.message.contains("deployments"), "{}", c.message);
        assert!(c.message.contains("credential from Keychain `darkmux-azure`"), "{}", c.message);
        assert!(c.message.contains("limits 500000 tokens/dispatch · 2000000 tokens per 1d"), "{}", c.message);
        assert!(
            c.message.contains("budget warn: spent 1500000 tokens in 2 calls over the last 1d"),
            "absent policy + a set budget = warn, with the window's spend: {}",
            c.message
        );
        assert!(
            c.message.contains("per-dispatch cap 500000 tokens (warn)")
                && c.message.contains("calls run one at a time (no limits.concurrent_calls)"),
            "the cap and the serial default are both said: {}",
            c.message
        );
        assert!(c.message.contains("`lms`: managed (lmstudio), chat-completions-max-tokens"), "{}", c.message);
        assert!(c.message.contains("parallelism is the scheduler's"), "{}", c.message);
        assert_eq!(asked, vec!["azure".to_string()], "only a counting budget reads the window");
        let floor = endpoints_status(&r, &mut |_| vec![(1, darkmux_crew::budget::Spend::full(1_200_000)), (2, darkmux_crew::budget::Spend::partial(500))]);
        assert!(
            floor.message.contains("spent at least 1200500 tokens (1 with an unknown spend) in 2 calls"),
            "an unknown spend is a floor, never a small number: {}",
            floor.message
        );
    }

    /// (#3035) A managed endpoint's window and cap are enforced like any
    /// other, and a declared `concurrent_calls` is shown as how many calls run
    /// at once; `concurrent_calls` on a managed endpoint is a Fail naming the
    /// scheduler.
    #[test]
    fn endpoints_check_shows_limits_on_a_managed_endpoint_and_declared_concurrency() {
        let r = materialized(
            r#"{"profiles":{"p":{"models":[{"id":"m","n_ctx":1,"endpoint":"lms"},{"id":"g","endpoint":"azure"}]}},
                "endpoints":{
                    "lms":{"managed":"lmstudio","limits":{"tokens_per_dispatch":9000,"window":{"period":"1h","tokens":100000}}},
                    "azure":{"url":"https://h.example/v1","limits":{"concurrent_calls":3}}}}"#,
        );
        let mut asked = Vec::new();
        let c = endpoints_status(&r, &mut |b| {
            asked.push(b.endpoint_id.clone());
            vec![(1, darkmux_crew::budget::Spend::full(40_000))]
        });
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        assert_eq!(asked, vec!["lms".to_string()], "the managed endpoint's window is read: {}", c.message);
        assert!(c.message.contains("per-dispatch cap 9000 tokens (warn)") && c.message.contains("spent 40000 tokens"), "{}", c.message);
        assert!(!c.message.contains("not enforced"), "{}", c.message);
        assert!(c.message.contains("up to 3 calls at once"), "{}", c.message);
        let bad = materialized(
            r#"{"profiles":{"p":{"models":[{"id":"m","n_ctx":1,"endpoint":"lms"}]}},
                "endpoints":{"lms":{"managed":"lmstudio","limits":{"concurrent_calls":2}}}}"#,
        );
        let c = endpoints_status(&bad, &mut no_spend);
        assert_eq!(c.status, Status::Fail, "{}", c.message);
        assert!(c.message.contains("endpoints.lms.limits") && c.message.contains("scheduler"), "{}", c.message);
    }

    /// (#2902 step 5) An unregistered budget policy is Fail, naming the raw
    /// value, where it was set and the valid values; `off` reads nothing.
    #[test]
    fn endpoints_check_fails_on_an_unregistered_policy_and_off_reads_nothing() {
        let bad = materialized(
            r#"{"profiles":{"p":{"models":[{"id":"m","endpoint":"e"}]}},
                "endpoints":{"e":{"url":"https://h.example/v1","limits":{"policy":"stop","window":{"period":"1d","tokens":5}}}}}"#,
        );
        let c = endpoints_status(&bad, &mut no_spend);
        assert_eq!(c.status, Status::Fail, "{}", c.message);
        assert!(c.message.contains("`stop`") && c.message.contains("endpoints.e.limits.policy"), "{}", c.message);
        for v in ["off", "warn", "wait"] {
            assert!(c.message.contains(v), "{v}: {}", c.message);
        }
        let off = materialized(
            r#"{"profiles":{"p":{"models":[{"id":"m","endpoint":"e"}]}},
                "endpoints":{"e":{"url":"https://h.example/v1","limits":{"policy":"off","window":{"period":"1d","tokens":5}}}}}"#,
        );
        let c = endpoints_status(&off, &mut |_| panic!("an `off` budget must not read the window"));
        assert!(c.message.contains("window budget off (nothing is counted)"), "{}", c.message);
        // (review M2) Unreadable limits are Fail too, by path, never "no budget".
        let typo = materialized(
            r#"{"profiles":{"p":{"models":[{"id":"m","endpoint":"e"}]}},
                "endpoints":{"e":{"url":"https://h.example/v1","limits":{"window":{"period":"1d","tokens":"2M"}}}}}"#,
        );
        let c = endpoints_status(&typo, &mut no_spend);
        assert_eq!(c.status, Status::Fail, "{}", c.message);
        assert!(c.message.contains("endpoints.e.limits") && c.message.contains("2M"), "{}", c.message);
        // (zero doctrine) A zero window is not a budget: Fail, naming the
        // field and `policy off` as the way to turn one off.
        let zero = materialized(
            r#"{"profiles":{"p":{"models":[{"id":"m","endpoint":"e"}]}},
                "endpoints":{"e":{"url":"https://h.example/v1","limits":{"window":{"period":"1d","tokens":0}}}}}"#,
        );
        let c = endpoints_status(&zero, &mut |_| panic!("a refused budget must not read the window"));
        assert_eq!(c.status, Status::Fail, "{}", c.message);
        assert!(c.message.contains("limits.window.tokens is 0") && c.message.contains("set policy off"), "{}", c.message);
    }

    #[test]
    #[serial_test::serial]
    fn run_returns_static_plus_eureka_checks() {
        let r = with_isolated_darkmux_home(run);
        // 55 static checks via run() (#1405 removed the 4 openclaw-gated
        // checks; #1426 removed recommendation-drift +
        // recommended-profile-not-shadowed with the retired recommendations
        // family; #1758 removed orchestrator-declared, a write-only field's
        // check), incl. build-identity [#1129] + docker-runtime [#680] +
        // load projection + daemon reachable +
        // darkmux-version-vs-latest-release [#13] +
        // crew-role-prompt-coverage [#141] + flow-sink-health [#170] +
        // machine_id [#167] + openai-base-url-conflict [#5] +
        // audit-integrity [#163] + utility-model-binding
        // [#590] + role-tool-vocab [#340] +
        // redis-config [#661] +
        // remote-endpoint-credentials [#85/#91] + audit-write-drops [#877] +
        // serve-daemon-auth [#881] + fleet.mode [#933] + env-masks-config
        // [#934] + binary-split-brain [#934] + crew-validation [#1269] +
        // mission-config-registry [#1284] + daemon-freshness +
        // binary-vs-source + runtime-image-freshness [#1461] + role-profiles
        // [#1475] + cmd-gate-allowlist [#1685] + unpriceable-residents
        // [#1819] + unreachable-residents [#1944] +
        // turn-delay [#2094] + reasoning-checkpoint-interval [#2165] +
        // host-sampler-interval [#2107, #1833] +
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
        // (#2653 MUST FIX 5) The constant here is 58, not 57: this branch's
        // own `check_liveness_retention` (added to the static array above,
        // alongside `check_host_sampler`) landed without this literal being
        // bumped alongside it — a plain CI red (`left: 59, right: 58`),
        // fixed by recounting rather than guessing. Re-derive it the same
        // way every prior bump here did: `grep -c` inside the `let checks =
        // vec![...]` block for the static count, `darkmux_eureka::all_rules().len()`
        // for the dynamic half (one entry today — `memory-headroom-tight`).
        //
        // (round-3 merge fix, historical) The constant was 57, not 56:
        // #1944 added `check_unreachable_darkmux_residents` to the static
        // array, `check_hooks()` always contributes exactly 1 more
        // (disabled by default → the single overview check), and only THEN
        // does `eureka_checks()` add one per active rule. A prior rebase
        // kept an origin/main-side "53" that predated this branch's own
        // `check_runtime_binary_cache` addition to the static array,
        // silently undercounting by exactly the one check the OTHER side of
        // that same merge conflict had just added — proof that a
        // colliding-file rebase needs its literal counts re-derived, not
        // just its prose reconciled. Same lesson, same fix, one more time.
        //
        // (#2707) 59, not 58: `check_temp_residue` joined the static array
        // above. Re-derived the same way the note above prescribes rather
        // than incremented on faith — `grep -c '^        check_' ` inside
        // the `let checks = vec![...]` block returns 58, plus the one
        // `check_hooks()` always contributes.
        //
        // (#2765/#2775) 61, not 59: `check_serve_address` and
        // `check_machine_rollup` joined the static array. Re-derived, not
        // incremented on faith — and the re-derivation caught that the
        // grep recipe the note above prescribes UNDERCOUNTS BY ONE: it
        // anchors on `^        check_`, which misses
        // `checks_power::check_power_posture()` (module-qualified, so the
        // line starts with `checks_power::`). The honest count of entries
        // in the `vec![...]` block is 61 (#2846 added
        // `check_detection_policy`), plus the one `check_hooks()` always
        // contributes = 62. Use
        // `grep -cE '^        (checks_[a-z_]+::)?check_'` instead, or just
        // count the non-comment lines in the block.
        //
        // (#2914) 64, not 62: `check_utility_model_in_profiles` and
        // `check_removed_radio_router_staffing` joined the static array.
        //
        // (#2902 step 4) 65, not 64: `check_endpoints` joined the static
        // array.
        //
        // Every check should appear regardless of environment — even if the
        // underlying probe couldn't read state.
        // 65 with #2902's endpoints check, plus three 4.0 retirement checks:
        // (#2913) the notebook env-var check (since folded into the retired-env
        // row), and (#2912/#2913 review)
        // `check_retired_role_leftovers` and `check_role_skill_references`.
        // (#2928) 69: `check_live_channel` joined the static array.
        //
        // (#2947) 67 static rows now: `check_fleet_mode` and
        // `check_detection_policy` left the array for the generic
        // `check_enum_settings`, which contributes one row per registered
        // enum setting.
        //
        // (4.0 cleanup) 63: `check_mission_state_files` (then `check_flat_mission_files`) joined (the Fail
        // that replaced the `mission migrate` pointer). Before it, 62:
        // `check_legacy_mission_layout` left with the
        // `mission migrate` verb it pointed at,
        // `check_legacy_compaction_extras` with the openclaw passthrough it
        // warned about, and three residue checks for pre-3.x removals
        // (`check_crews_residue`, `check_removed_review_config_block`,
        // `check_removed_telemetry_record_every_samples`).
        //
        // (4.0 integration) 64: the cleanup's 63 plus
        // `check_renamed_budget_settings` (#2902 step 5).
        //
        // (4.0 flow vocabulary) 65: `check_unknown_flow_actions` joined.
        //
        // (4.0 unknown-key gate) 66: `check_user_file_keys` contributes one
        // Pass row when every user file is clean, as it is here.
        //
        // (#2988) 67: `check_serve_reads` joined beside the token row, so
        // doctor shows the read posture and the execution posture.
        //
        // (4.0 one run noun) 68: `check_lab_dir_location` joined.
        //
        // (4.0 project-local) `check_ignored_project_darkmux` joined and
        // `check_removed_notebook_settings` left (its env var is a
        // `RETIRED_SETTINGS` entry the retired-env row reports): net zero.
        //
        // (radio on a peer) 69: `check_radio_peer_seat` joined beside the
        // role-profiles row.
        //
        // (fleet route check) 70: `check_fleet_routes` joined beside it.
        //
        // (5.0) Four rows left (-4): `check_beat33_legacy_crew_dir`, `check_mission_state_files`,
        // `check_lab_dir_location` and `check_retired_role_leftovers` left
        // with the readers of the pre-5.0 layouts they reported.
        //
        // (5.0, #3036) `check_machine_rollup` left with the `machine_rollup`
        // block, and (#2312) `check_mission_config_registry` moved out of
        // `run()`: the root crate appends it with the full step-kind catalog.
        //
        // (5.0) 69: `machine_uid_check` joined beside the machine_id row.
        //
        // (#3074) 66: `check_inactivity_timeout` joined beside the step-command row.
        let expected =
            66 + darkmux_types::config_enum::ENUM_SETTINGS.len() + darkmux_eureka::all_rules().len();
        assert_eq!(r.checks.len(), expected);
    }

    /// (#2101) `doctor` shows where work records and machine samples go and how
    /// many each stream keeps; `0` reads as unbounded, never as "none".
    #[serial_test::serial]
    #[test]
    fn the_redis_check_names_both_streams_and_both_caps() {
        unsafe {
            std::env::set_var("DARKMUX_REDIS_URL", "redis://127.0.0.1:1");
            std::env::set_var("DARKMUX_REDIS_STREAM", "t:flow");
            std::env::set_var("DARKMUX_REDIS_MAXLEN", "7");
            std::env::set_var("DARKMUX_REDIS_TELEMETRY_MAXLEN", "0");
        }
        let message = check_redis_config().message;
        for var in ["DARKMUX_REDIS_URL", "DARKMUX_REDIS_STREAM", "DARKMUX_REDIS_MAXLEN", "DARKMUX_REDIS_TELEMETRY_MAXLEN"] {
            unsafe { std::env::remove_var(var) };
        }
        assert!(
            message.contains("work `t:flow` (maxlen 7), telemetry `t:flow:telemetry` (maxlen unbounded)"),
            "{message}"
        );
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
            parse_tailnet_viewer_url(json, "127.0.0.1", 8765).as_deref(),
            Some("http://laptop.tailnet-example.ts.net/")
        );
        // A different daemon port → not our proxy → None.
        assert_eq!(parse_tailnet_viewer_url(json, "127.0.0.1", 9000), None);
        // Served on 443 → https scheme.
        let j443 = r#"{"Web":{"tailnet-example.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8765"}}}}}"#;
        assert_eq!(parse_tailnet_viewer_url(j443, "127.0.0.1", 8765).as_deref(), Some("https://tailnet-example.ts.net/"));
        // localhost proxy target is accepted too.
        let jlocal = r#"{"Web":{"example.ts.net:80":{"Handlers":{"/":{"Proxy":"http://localhost:8765"}}}}}"#;
        assert_eq!(parse_tailnet_viewer_url(jlocal, "127.0.0.1", 8765).as_deref(), Some("http://example.ts.net/"));
        // Not serving / empty / garbage → None (best-effort, never an error).
        assert_eq!(parse_tailnet_viewer_url("{}", "127.0.0.1", 8765), None);
        assert_eq!(parse_tailnet_viewer_url("not json", "127.0.0.1", 8765), None);
    }

    /// (#2782 C3) The THIRD accepted spelling — the daemon's own resolved
    /// bind host — has no other pin. `parse_tailnet_viewer_url` is private,
    /// so no consumer test can reach it: deleting `|| p == want_bind` left
    /// every other test in this crate green.
    ///
    /// The operator this matters to bound one specific interface and wrote
    /// `tailscale serve` against THAT address. Matching only the two
    /// loopback spellings reports "no tailnet URL" for a proxy pointed
    /// straight at this daemon.
    #[test]
    fn parse_tailnet_viewer_url_accepts_a_proxy_written_at_the_configured_bind() {
        // A proxy target at a non-loopback bind, matching NEITHER loopback
        // spelling — so only the `want_bind` arm can accept it.
        let j = r#"{"Web":{"hub.tailnet-example.ts.net:443":{"Handlers":{"/":{"Proxy":"http://192.0.2.10:8799"}}}}}"#;
        assert_eq!(
            parse_tailnet_viewer_url(j, "192.0.2.10", 8799).as_deref(),
            Some("https://hub.tailnet-example.ts.net/"),
            "a proxy target written at the configured bind must match"
        );
        // Same JSON, a DIFFERENT bind → no longer ours. Pins that the arm
        // compares the resolved bind rather than accepting any host.
        assert_eq!(parse_tailnet_viewer_url(j, "192.0.2.11", 8799), None);
        // The bind's PORT is not a free pass either.
        assert_eq!(parse_tailnet_viewer_url(j, "192.0.2.10", 9000), None);
        // A wildcard bind collapses to loopback before it gets here, so it
        // adds no fourth case — it matches the loopback target, not `0.0.0.0`.
        let jloop = r#"{"Web":{"hub.tailnet-example.ts.net:80":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8799"}}}}}"#;
        assert_eq!(
            parse_tailnet_viewer_url(jloop, "0.0.0.0", 8799).as_deref(),
            Some("http://hub.tailnet-example.ts.net/")
        );
        let jwild = r#"{"Web":{"hub.tailnet-example.ts.net:80":{"Handlers":{"/":{"Proxy":"http://0.0.0.0:8799"}}}}}"#;
        assert_eq!(
            parse_tailnet_viewer_url(jwild, "0.0.0.0", 8799),
            None,
            "a wildcard is a bind directive, never a proxy destination"
        );
    }

    // ─── serve token + serve reads (#881, #2988) ───────────────────────
    #[test]
    fn serve_token_status_arms() {
        // Token set → Pass, no hint; it is the execution credential.
        let (s, msg, hint) = serve_token_status(true);
        assert_eq!(s, Status::Pass);
        assert!(msg.contains("fleet work"), "says what the token is for: {msg}");
        assert!(hint.is_none());
        // No token → still Pass (loopback-only is the ordinary single-machine
        // state; the bind gate enforces it), with an actionable hint.
        let (s, _msg, hint) = serve_token_status(false);
        assert_eq!(s, Status::Pass, "no-token is not a Warn — don't cry wolf on the safe default");
        let h = hint.expect("the no-token arm gives an enabling hint");
        assert!(h.contains("darkmux-serve-token") && h.contains("serve.token_keychain"), "{h}");
    }

    /// (#2988 follow-up) The read posture is its own row, independent of the
    /// token: off → reads open; on → a request not from this machine
    /// (proxied included) needs the token; on without a token → `serve`
    /// refuses to start, which doctor reports as the failure it is.
    #[test]
    fn serve_reads_status_reports_each_posture() {
        for token in [false, true] {
            let (s, msg, _) = serve_reads_status(false, token);
            assert_eq!(s, Status::Pass);
            assert!(msg.contains("open") && msg.contains("serve.read_auth"), "{msg}");
        }
        let (s, msg, hint) = serve_reads_status(true, true);
        assert_eq!(s, Status::Pass);
        assert!(msg.contains("proxied"), "names the proxied case: {msg}");
        assert!(hint.is_none());
        let (s, msg, hint) = serve_reads_status(true, false);
        assert_eq!(s, Status::Fail);
        assert!(msg.contains("refuses to start"), "{msg}");
        assert!(hint.unwrap().contains("darkmux-serve-token"));
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
    /// whole tailnet.
    #[test]
    fn serve_auth_hints_state_facts_without_rendering_a_verdict() {
        let verdicts = ["safe as-is", "is fine", "secure", "protected", "no risk", "for compliance"];
        let mut texts = Vec::new();
        for t in [false, true] {
            let (_, m, h) = serve_token_status(t);
            texts.extend([m, h.unwrap_or_default()]);
            for r in [false, true] {
                let (_, m, h) = serve_reads_status(r, t);
                texts.extend([m, h.unwrap_or_default()]);
            }
        }
        for text in texts {
            let low = text.to_lowercase();
            for v in verdicts {
                assert!(!low.contains(v), "doctor must not adjudicate the operator's posture ({v:?}): {text}");
            }
        }
        let (_, msg, _) = serve_token_status(false);
        assert!(msg.contains("loopback-only"), "still reports the actual state: {msg}");
    }

    // ─── check_state_file_permissions (#2452) ──────────────────────────
    //
    // #2259/#2451 made darkmux CREATE its own state files owner-only
    // (0o600). `.mode()` only applies at creation, so a file already on
    // disk from an older binary — or loosened by hand — keeps whatever
    // mode it had. Nothing reported that until now.
    //
    // (#2452 review) There is deliberately NO umask "anti-vacuity control"
    // in this block, and the first draft's was removed rather than fixed.
    // It was lifted from `tests/state_files_owner_only_mode.rs`, where the
    // fixture files are created by PRODUCTION code whose `.mode()` call is
    // the thing under test — there, a strict umask really can hide a
    // deleted `.mode()` and the control is load-bearing. Here every fixture
    // sets its mode EXPLICITLY via `chmod_for_test`, so the umask cannot
    // reach these assertions at all; the control asserted a premise that is
    // false in this file, and it turned the suite red under
    // `zsh -c 'umask 0077; cargo test'` for a reason that had nothing to do
    // with the code. Non-vacuity is proven the way it should be — by
    // mutation (`mode & 0o044 != 0` → `false` reds
    // `state_file_perms_reports_violations_by_root_count_and_mode`) and by
    // `state_file_perms_mask_catches_every_read_bit`, which pins the
    // predicate against a table of modes, umask-independently.

    #[cfg(unix)]
    fn chmod_for_test(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    fn write_at_mode(dir: &std::path::Path, name: &str, mode: u32) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, b"{}").unwrap();
        chmod_for_test(&p, mode);
        p
    }

    /// The mask is the whole predicate, so it gets a table rather than one
    /// example: every mode that sets EITHER read bit must be caught, and
    /// 0o600 — what #2259/#2451 actually create — must not be.
    ///
    /// 0o620 is in the table as a deliberate NON-catch: it is group-WRITABLE
    /// with no read bit, which `mode & 0o044` cannot see. That is the
    /// literal reading of #2452 ("group- or world-readable"), and it is
    /// recorded here rather than left to be re-derived, because widening to
    /// `mode & 0o077` is a real (and defensible) follow-up: a mod kit is
    /// briefed to a model and its attachments are bind-mounted into a
    /// dispatch container, so a writable one is a tampering surface, not
    /// just a disclosure one.
    #[cfg(unix)]
    #[test]
    fn state_file_perms_mask_catches_every_read_bit() {
        for (mode, should_flag) in [
            (0o600, false),
            (0o400, false),
            (0o620, false), // group-writable, NOT readable — see this test's doc
            (0o640, true),  // group-read
            (0o604, true),  // other-read
            (0o644, true),
            (0o664, true),
            (0o666, true),
            (0o755, true),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("findings");
            write_at_mode(&dir, "finding.json", mode);
            let roots = vec![ScanRoot { label: "findings", path: dir, recursive: true }];
            let check = build_state_file_permissions_check(&roots, 100);
            let flagged = check.status == Status::Warn;
            assert_eq!(
                flagged, should_flag,
                "mode {mode:o} should {} be flagged; got: {}",
                if should_flag { "" } else { "NOT" },
                check.message
            );
        }
    }

    /// The report names the ROOT, the count and the modes — and the remedy
    /// is runnable. See `build_state_file_permissions_check`'s doc for why
    /// no file name may appear; `state_file_perms_never_prints_a_file_name`
    /// is the guard on that.
    #[cfg(unix)]
    #[test]
    fn state_file_perms_reports_violations_by_root_count_and_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("findings");
        write_at_mode(&dir, "finding.json", 0o644);
        write_at_mode(&dir, "finding2.json", 0o600);
        let clean = tmp.path().join("mods");
        write_at_mode(&clean, "mod.json", 0o600);

        let roots = vec![
            ScanRoot { label: "findings", path: dir.clone(), recursive: true },
            ScanRoot { label: "mods", path: clean.clone(), recursive: true },
        ];
        let check = build_state_file_permissions_check(&roots, 100);

        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("1 of 3"), "counts both stores: {}", check.message);
        assert!(check.message.contains("findings (1, mode 644)"), "{}", check.message);
        assert!(
            !check.message.contains("mods"),
            "a store with no violation must not appear: {}",
            check.message
        );

        let hint = check.hint.expect("a warn carries a remedy");
        assert!(hint.contains("chmod go-rwx"), "remedy must be actionable: {hint}");
        assert!(
            hint.contains(&dir.display().to_string()),
            "remedy must name the offending ROOT so the command runs: {hint}"
        );
        assert!(
            !hint.contains(&clean.display().to_string()),
            "a clean store must not be swept by the remedy: {hint}"
        );
    }

    /// (#2452 review, the disclosure guard) `darkmux doctor`'s stdout is
    /// republished verbatim by the viewer's console lens over the same
    /// daemon a tailnet peer reaches as loopback, and these stores hold
    /// operator-named files — a mod attachment keeps its source basename
    /// (#2457), a hooks outbox file is keyed by destination host:port, a
    /// crawl plan file by the operator's rule name. So NO leaf name may
    /// appear in either the message or the hint, however tempting the
    /// actionability. The remedy is the `find` command, which runs on the
    /// operator's own machine.
    #[cfg(unix)]
    #[test]
    fn state_file_perms_never_prints_a_file_name() {
        let tmp = tempfile::tempdir().unwrap();
        let attachments = tmp.path().join("mods").join("mod-3-abc").join("attachments");
        write_at_mode(&attachments, "prod-credentials.diff", 0o644);
        write_at_mode(&tmp.path().join("hooks"), "hooks.internal.example-443-abc.outbox.jsonl", 0o644);

        let roots = vec![
            ScanRoot { label: "mods", path: tmp.path().join("mods"), recursive: true },
            ScanRoot { label: "hooks outbox", path: tmp.path().join("hooks"), recursive: false },
        ];
        let check = build_state_file_permissions_check(&roots, 100);
        assert_eq!(check.status, Status::Warn, "{}", check.message);

        let text = format!("{} {}", check.message, check.hint.clone().unwrap_or_default());
        for leaked in ["prod-credentials", "hooks.internal.example", "mod-3-abc", "attachments"] {
            assert!(
                !text.contains(leaked),
                "operator file name {leaked:?} reached doctor's output, which the viewer's \
                 console lens republishes verbatim: {text}"
            );
        }
        // ...while still naming the stores and the counts.
        assert!(check.message.contains("mods (1, mode 644)"), "{}", check.message);
        assert!(check.message.contains("hooks outbox (1, mode 644)"), "{}", check.message);
    }

    #[cfg(unix)]
    #[test]
    fn state_file_perms_passes_when_everything_is_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("mods");
        write_at_mode(&dir, "mod.json", 0o600);

        let roots = vec![ScanRoot { label: "mods", path: dir, recursive: true }];
        let check = build_state_file_permissions_check(&roots, 100);
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.hint.is_none());
    }

    /// (#1839) Same bar as `daemon_auth_and_redis_hints_state_facts_without_rendering_a_verdict`:
    /// this check states the mode and the remedy, never a verdict on whether
    /// the operator's machine is safe.
    #[cfg(unix)]
    #[test]
    fn state_file_perms_states_facts_without_rendering_a_verdict() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("findings");
        write_at_mode(&dir, "finding.json", 0o644);

        let roots = vec![ScanRoot { label: "findings", path: dir, recursive: true }];
        let check = build_state_file_permissions_check(&roots, 100);
        // Pin the arm under test: without this the whole assertion below is
        // satisfied by a Pass, whose text has no verdict to render.
        assert_eq!(check.status, Status::Warn, "{}", check.message);

        let verdicts =
            ["safe as-is", "is fine", "secure", "protected", "no risk", "for compliance", "unsafe", "insecure", "at risk", "vulnerable", "dangerous"];
        let text = format!("{} {}", check.message.to_lowercase(), check.hint.unwrap_or_default().to_lowercase());
        for v in verdicts {
            assert!(!text.contains(v), "doctor must not adjudicate the operator's posture ({v:?}): {text}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn state_file_perms_scan_is_bounded_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("findings");
        for i in 0..10 {
            write_at_mode(&dir, &format!("f{i}.json"), 0o600);
        }
        let roots = vec![ScanRoot { label: "findings", path: dir, recursive: true }];
        // Budget smaller than the file count — the scan must stop early and
        // say it did not finish, rather than silently claiming a clean sweep.
        let check = build_state_file_permissions_check(&roots, 3);
        assert_eq!(check.status, Status::Warn, "truncated scan must report Warn, not Pass");
        assert!(
            check.message.contains("budget") && check.message.contains("findings"),
            "must name the cost bound it hit: {}",
            check.message
        );
    }

    /// (#2452 review) The budget is shared, so spending it in declaration
    /// order lets one large store starve every later one — permanently, on
    /// every run, not randomly. Measured on a 4,921-file tree, `findings`
    /// consumed all 2,000 stats and `mods` (the store #2457 is about) was
    /// never reached at all. Each root gets its own share; unspent share
    /// rolls forward.
    #[cfg(unix)]
    #[test]
    fn state_file_perms_budget_is_shared_fairly_so_a_big_store_cannot_starve_a_later_one() {
        let tmp = tempfile::tempdir().unwrap();
        let big = tmp.path().join("findings");
        for i in 0..50 {
            write_at_mode(&big, &format!("f{i}.json"), 0o600);
        }
        let small = tmp.path().join("mods");
        write_at_mode(&small, "leaky.json", 0o644);

        let roots = vec![
            ScanRoot { label: "findings", path: big, recursive: true },
            ScanRoot { label: "mods", path: small, recursive: true },
        ];
        // 10 files of budget against a 50-file first root: under a single
        // shared cursor the second root is never opened.
        let check = build_state_file_permissions_check(&roots, 10);
        assert_eq!(
            check.status,
            Status::Warn,
            "the later root must still be scanned out of its own share: {}",
            check.message
        );
        assert!(check.message.contains("mods (1, mode 644)"), "{}", check.message);
        assert!(
            check.message.contains("findings"),
            "and the starved-of-budget root must still be named as partial: {}",
            check.message
        );
    }

    /// (#2452 review) Symlinks are skipped — following one would let a link
    /// planted in the store aim the printed `chmod` at an arbitrary path
    /// (the confused-deputy shape `brief_refs` refuses for `attachments/`,
    /// #2295). But skipped must not mean SILENT: a symlinked state file
    /// whose target is world-readable is a real exposure this check cannot
    /// see, so it says how many it passed over. Before this fix the count
    /// did not exist and the two root shapes disagreed — `Path::is_file()`
    /// follows a link, so a symlinked FILE root vanished without a trace
    /// while a symlinked DIRECTORY root was walked in full.
    #[cfg(unix)]
    #[test]
    fn state_file_perms_discloses_the_symlinks_it_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        let target = write_at_mode(&outside, "real.json", 0o644);

        let dir = tmp.path().join("findings");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&target, dir.join("link.json")).unwrap();

        // A single-FILE root that is itself a symlink — the shape that used
        // to disappear entirely.
        let roster_link = tmp.path().join("fleet.json");
        std::os::unix::fs::symlink(&target, &roster_link).unwrap();

        let roots = vec![
            ScanRoot { label: "findings", path: dir, recursive: true },
            ScanRoot { label: "fleet roster", path: roster_link, recursive: false },
        ];
        let check = build_state_file_permissions_check(&roots, 100);

        assert_eq!(check.status, Status::Pass, "a skipped symlink is not a violation");
        assert!(
            check.message.contains("2 symlink(s) skipped"),
            "both symlink shapes must be counted and disclosed: {}",
            check.message
        );
        assert!(
            check.message.starts_with("0 darkmux state file(s) checked"),
            "and a symlink must not be counted as a file that WAS checked: {}",
            check.message
        );
    }

    #[test]
    fn power_state_warns_on_macos_when_read_fails() {
        let check = power_state_status(None, true);
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("could not read power source"));
    }

    #[test]
    fn power_state_skips_on_non_macos_when_read_fails() {
        let check = power_state_status(None, false);
        assert_eq!(check.status, Status::Pass);
        assert!(check.message.contains("non-Apple Silicon"));
    }

    #[test]
    fn platform_and_provider_warns_when_ram_is_unknown() {
        let hw = darkmux_hardware::HardwareSpec {
            platform: darkmux_hardware::Platform::AppleSilicon,
            arch: "aarch64".into(),
            total_ram_gb: 0,
            physical_cores: 8,
            performance_cores: None,
            efficiency_cores: None,
            has_unified_memory: true,
        };
        let check = platform_and_provider_status(&hw);
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("unknown RAM"), "got: {}", check.message);
        assert!(check.message.contains("generic"), "got: {}", check.message);
    }

    /// (#2411/#2450-class regression guard) The production check must
    /// resolve every path through `darkmux_types::config_access` — never
    /// `DarkmuxConfig::load_resolved()` and never `dirs::home_dir()`
    /// directly — so it inherits the SAME env>config>default ladder AND the
    /// SAME test-build isolation every sibling accessor already has. This
    /// drives `check_state_file_permissions()` itself (not the pure
    /// builder), and proves a file planted at the resolved
    /// `findings_dir()` is the one the check finds.
    ///
    /// (#2695) Isolated with the shared
    /// [`IsolatedState`](darkmux_types::test_isolation::IsolatedState)
    /// rather than a hand-rolled `DARKMUX_HOME` pin. It had the SAME
    /// defect the keystone test below was filed for, one test over:
    /// `findings_dir()` resolves `env(DARKMUX_FINDINGS_DIR) >
    /// config.dirs.findings > <root>/findings`, so pinning only the root
    /// left the higher-precedence override in charge. Measured: with
    /// `DARKMUX_FINDINGS_DIR` exported, this test failed on its own setup
    /// sanity assertion ("must resolve under our isolated DARKMUX_HOME,
    /// got /tmp/…"). Found by sweeping this crate's suite with the whole
    /// override set exported — not by reading, which had already passed
    /// over it twice.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn check_state_file_permissions_resolves_paths_through_config_access() {
        let state = darkmux_types::test_isolation::IsolatedState::new();

        let findings_dir = darkmux_types::config_access::findings_dir();
        assert!(
            findings_dir.starts_with(state.path()),
            "test setup sanity: findings_dir() must resolve under the isolated state root, got {}",
            findings_dir.display()
        );
        write_at_mode(&findings_dir, "leaky.json", 0o644);

        let check = check_state_file_permissions();
        drop(state);

        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(
            check.message.contains("findings (1, mode 644)"),
            "check must have scanned the config_access-resolved findings_dir(): {}",
            check.message
        );
    }

    /// The keystone isolation test (#2695).
    ///
    /// **What it asserts.** Not "`DARKMUX_HOME` works" — the *property*:
    /// with [`IsolatedState`](darkmux_types::test_isolation::IsolatedState)
    /// held, EVERY destination darkmux can write resolves under that one
    /// throwaway root, and none of them resolves into the operator's real
    /// `~/.darkmux`. Stated that way it is the thing that goes red when a
    /// FUTURE destination is added without a guard, which is the only
    /// durable defense against this bug class. Proven to do so: adding a
    /// probe destination that resolves outside the isolated root fails
    /// this test with that destination named.
    ///
    /// **Why it holds an `IsolatedState`.** Setting only `DARKMUX_HOME` and
    /// asserting `missions_dir()` landed under it is correct for the root but
    /// silent about every override that outranks it. Neutralizing those,
    /// enumerated from the resolvers rather than from memory, is exactly
    /// what `IsolatedState` is, so this test simply holds one.
    ///
    /// **The residual it also measures.** Six destinations have no env
    /// tier at all — `hooks_outbox_dir()` and `hooks_adapters_dir()` are
    /// config-only by design, and `liveness_dir()`,
    /// `host_sampler_lock_path()`, `cache_dir()` and
    /// `lessons::global_db_path()` derive straight off the root. They are
    /// asserted here too, so if a config-tier relocation ever lets one
    /// escape a root pin, the escape is a red test rather than a silent
    /// write.
    ///
    /// **And the list itself is held, not just its contents.** The
    /// membership test below
    /// (`the_guards_variable_list_covers_every_destination_the_resolvers_read`)
    /// is keyed on env-var READS, so it is structurally blind to exactly
    /// those six: an entry with no variable to read can be deleted from
    /// `resolved` and nothing anywhere goes red — the list just quietly
    /// stops checking it, which is the erosion this whole change exists to
    /// stop. [`NO_ENV_TIER_DESTINATIONS`] and
    /// [`RESOLVED_DESTINATION_COUNT`] are what hold them. Red-proven by
    /// deleting all six entries (6 applied lines): EXIT=101 naming each
    /// missing label, where before the assertion the same deletion left
    /// `-p darkmux-doctor --lib` at EXIT=0, 273 passed.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn every_state_root_resolves_under_an_isolated_darkmux_home() {
        use darkmux_types::config_access as ca;

        /// The destinations with NO env tier of their own, named so that
        /// their membership in `resolved` is held by an assertion rather
        /// than by memory. The membership test below keys on env-var
        /// reads and is therefore blind to every one of these.
        const NO_ENV_TIER_DESTINATIONS: &[&str] = &[
            "hooks outbox",
            "hooks adapters",
            "liveness heartbeats",
            "host-sampler lock",
            "cache",
            "global lessons db",
        ];

        /// How many destinations `resolved` must carry. A bare count is
        /// crude, but it is the only thing that goes red when an entry
        /// the membership test cannot see is dropped. Bump it — in the
        /// same commit as the entry — when a real destination is added.
        const RESOLVED_DESTINATION_COUNT: usize = 19;

        let state = darkmux_types::test_isolation::IsolatedState::new();

        // Every write destination the resolvers name. Derived from
        // `config_access`'s path accessors + `darkmux_crew`'s own roots,
        // NOT from the incidents that produced this test — the whole
        // point is that the list outlives any one incident.
        let resolved = vec![
            // ── config-only / root-derived (no env tier of their own) ──
            ("hooks outbox", ca::hooks_outbox_dir()),
            ("hooks adapters", ca::hooks_adapters_dir()),
            ("liveness heartbeats", ca::liveness_dir()),
            ("host-sampler lock", ca::host_sampler_lock_path()),
            ("cache", ca::cache_dir()),
            // ── crew/user state: the root itself ──
            ("crew user-state root", darkmux_crew::loader::user_state_root()),
            ("mission/phase state", darkmux_crew::loader::missions_dir()),
            ("phase state", darkmux_crew::loader::phases_dir()),
            ("global lessons db", darkmux_crew::lessons::global_db_path()),
            // ── three-tier dirs (env > config > root-derived default) ──
            ("fleet roster", ca::fleet_file()),
            ("findings", ca::findings_dir()),
            ("mods", ca::mods_dir()),
            ("flow records", ca::flows_dir()),
            ("lab runs", ca::lab_dir()),
            // ── override-or-caller-default accessors ──
            (
                "audit chain",
                ca::audit_dir_override().unwrap_or_else(|| state.join("audit")),
            ),
            (
                "identity",
                // `.md` — production's default is `<root>/identity.md`
                // (`crew::dispatch::identity_path`). The fallback here
                // must match it, or this test self-confirms whatever
                // `PINNED_STATE_VARS` happens to say.
                ca::identity_path_override().unwrap_or_else(|| state.join("identity.md")),
            ),
            // ── the root's own files ──
            ("config.json", darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).config),
            ("profiles.json", darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).profiles),
            ("sandboxes", darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).sandboxes),
        ];

        // Hold the LIST before iterating it. A loop over `resolved` can
        // only speak about entries that are still in `resolved`; these two
        // assertions are what make a deletion red.
        let labels: Vec<&str> = resolved.iter().map(|(label, _)| *label).collect();
        for want in NO_ENV_TIER_DESTINATIONS {
            assert!(
                labels.contains(want),
                "destination {want:?} has no env var, so pinning the root is the ONLY thing \
                 that isolates it and this loop is the only thing that checks. It was dropped \
                 from `resolved`. Nothing else in the suite covers it — the membership test \
                 keys on env-var reads, and there is no read to see. Put it back.\n\
                 Present: {labels:?}"
            );
        }
        assert_eq!(
            labels.len(),
            RESOLVED_DESTINATION_COUNT,
            "the destination list changed size. If you ADDED a destination, bump \
             RESOLVED_DESTINATION_COUNT in the same commit. If you REMOVED one, say why in the \
             commit message — an entry silently leaving this list is how the per-variable \
             isolation this test replaced eroded in the first place.\n\
             Present: {labels:?}"
        );

        let real = dirs::home_dir().map(|h| h.join(".darkmux"));
        for (label, path) in resolved {
            assert!(
                path.starts_with(state.path()),
                "destination {label:?} escaped the isolated state root and resolved to {}. \
                 Every darkmux write destination must resolve under one guard — if this is a \
                 NEW destination, give it an entry in `test_isolation::PINNED_STATE_VARS` (or \
                 `CLEARED_STATE_VARS` when its presence carries meaning), not a new one-off \
                 guard in whichever suite happened to notice.",
                path.display()
            );
            if let Some(real) = real.as_ref() {
                assert!(
                    !path.starts_with(real),
                    "destination {label:?} resolved into the operator's real tree: {}",
                    path.display()
                );
            }
        }
    }

    /// The guard's own restore, asserted (#2698 finding 1). A restore that
    /// is never asserted can be deleted with the suite fully green — which
    /// is exactly what was measured: making a restore a no-op left 100
    /// passed, EXIT=0, harmless only by the accident that another guard
    /// happened to overwrite the leaked path first.
    ///
    /// Both shapes matter and only one of them is obvious: a variable that
    /// HAD a value must get that value back, and a variable that was UNSET
    /// must be removed again rather than left behind pointing at the
    /// guard's now-deleted tempdir — a leftover of the second shape is how
    /// a later, unguarded test silently resolves into a path that no
    /// longer exists.
    #[test]
    #[serial_test::serial]
    fn the_isolation_guard_restores_every_variable_it_displaced() {
        use darkmux_types::test_isolation::{
            IsolatedState, CLEARED_STATE_VARS, PINNED_STATE_VARS,
        };

        let touched: Vec<&str> = PINNED_STATE_VARS
            .iter()
            .map(|(v, _)| *v)
            .chain(CLEARED_STATE_VARS.iter().copied())
            .collect();
        let before: Vec<(&str, Option<std::ffi::OsString>)> =
            touched.iter().map(|v| (*v, std::env::var_os(v))).collect();

        {
            let _state = IsolatedState::new();
        }

        for (var, prev) in before {
            assert_eq!(
                std::env::var_os(var),
                prev,
                "{var} was not restored to exactly what it was before the guard — a guard \
                 that leaks its own pin is not isolation, it is a slower leak"
            );
        }
    }

    /// The guard's variable list, kept honest by the RESOLVERS rather than
    /// by anyone's memory (#2695: "enumerate the overrides from the
    /// resolver, since the list is the part that will drift").
    ///
    /// A list-iterating test cannot notice an entry that was DELETED — it
    /// simply stops checking it. Measured during the fix: deleting the
    /// `DARKMUX_FLOWS_DIR` entry left the per-entry contract test green,
    /// because the loop no longer had anything to say about it. So this
    /// test works the other direction: it turns on `env_audit`'s existing
    /// read instrumentation, drives every destination resolver, and reads
    /// back the set of `DARKMUX_*` keys those resolvers ACTUALLY consulted.
    /// Every such key that names a location must appear in
    /// `PINNED_STATE_VARS` or `CLEARED_STATE_VARS`.
    ///
    /// **What that is, stated exactly, because the obvious summary of it
    /// is false.** This test is self-maintaining for **deletions of
    /// env-keyed entries whose resolver is already in the driver list
    /// below**: remove `DARKMUX_FLOWS_DIR` from `PINNED_STATE_VARS` and
    /// this goes red naming the key, because `flows_dir()` still reads it.
    /// That is the erosion direction, and it is genuinely covered.
    ///
    /// It is **hand-maintained in the other direction.** Adding a new
    /// write destination goes red here only when BOTH of these hold, and
    /// neither is automatic:
    ///
    /// 1. someone adds its resolver to the ~20-call driver list below.
    ///    There is no registry of destination accessors to enumerate, so
    ///    this is a manual step — the same convention this change
    ///    replaces, moved one file over. Stated plainly rather than sold
    ///    as automation.
    /// 2. the read goes through `config_access`'s instrumented `env_str`
    ///    chokepoint rather than a bare `std::env::var`.
    ///
    /// And a destination with **no env var at all** is structurally
    /// invisible to this test in both directions — there is no read to
    /// record. Those are held instead by the keystone's
    /// `NO_ENV_TIER_DESTINATIONS` and `RESOLVED_DESTINATION_COUNT`, which
    /// a future author adding such a destination must extend.
    ///
    /// Measured (2026-09-12 review). Three probe destinations appended to
    /// `config_access` — one reading `DARKMUX_PROBE_A_DIR`, one with no
    /// env tier resolving outside the root, one reading
    /// `DARKMUX_PROBE_ROOT` — left this test and the keystone at EXIT=0,
    /// 3 passed. With only their driver calls added it went EXIT=101
    /// naming `DARKMUX_PROBE_A_DIR` **and nothing else**: the no-env-tier
    /// probe had nothing to record, and `DARKMUX_PROBE_ROOT` was dropped
    /// by what used to be a third condition — **the variable's NAME**, an
    /// inclusion filter on the `_DIR` / `_FILE` / `_PATH` convention.
    ///
    /// That third condition is gone. The filter is now deny-by-default
    /// (`NOT_A_DESTINATION`, below), so a destination whose name does not
    /// follow the convention is caught rather than silently dropped.
    /// Safe to widen because it was measured rather than assumed: the
    /// driven resolvers read 13 keys and all 13 are destinations, so the
    /// exclusion list is empty. A behavior knob that a destination
    /// resolver reads in future belongs in that list — never in
    /// `PINNED_STATE_VARS`, which would pin a knob to a path.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn the_guards_variable_list_covers_every_destination_the_resolvers_read() {
        use darkmux_types::config_access as ca;
        use darkmux_types::test_isolation::{
            IsolatedState, CLEARED_STATE_VARS, PINNED_STATE_VARS,
        };

        let log = tempfile::tempdir().unwrap();
        let log_path = log.path().join("env-reads.tsv");
        let prev_log = std::env::var_os("DARKMUX_ENV_AUDIT_LOG");
        // SAFETY: #[serial]. Set BEFORE the guard so every read below is
        // instrumented, and it is not itself a destination variable.
        unsafe { std::env::set_var("DARKMUX_ENV_AUDIT_LOG", &log_path) };

        {
            let _state = IsolatedState::new();
            // Drive every destination resolver. The audit sink records
            // which env keys each one consults, however indirectly.
            let _ = ca::hooks_outbox_dir();
            let _ = ca::hooks_adapters_dir();
            let _ = ca::liveness_dir();
            let _ = ca::cache_dir();
            let _ = ca::fleet_file();
            let _ = ca::findings_dir();
            let _ = ca::mods_dir();
            let _ = ca::flows_dir();
            let _ = ca::lab_dir();
            let _ = ca::audit_dir_override();
            let _ = ca::identity_path_override();
            let _ = ca::templates_override_dirs();
            let _ = ca::skills_override_dirs();
            // (#2704 fix-pass, CONSIDER B) `DARKMUX_PROFILES` was the one
            // list entry whose deletion NOTHING caught: no `config_access`
            // accessor reads it, so without this line the key never
            // reached the audit log and removing it from
            // `CLEARED_STATE_VARS` was EXIT=0 in darkmux-types,
            // darkmux-doctor AND tests/cli.rs. `load_registry` is its one
            // instrumented chokepoint. The `Result` is discarded on
            // purpose — under the guard there is no registry to find, and
            // this call is here for the env READ it performs, not its
            // outcome.
            let _ = darkmux_profiles::profiles::load_registry(None);
            let _ = darkmux_crew::loader::user_state_root();
            let _ = darkmux_crew::loader::missions_dir();
            let _ = darkmux_crew::loader::phases_dir();
            let _ = darkmux_crew::lessons::global_db_path();
            let _ = darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser);
        }

        // SAFETY: #[serial].
        unsafe {
            match prev_log {
                Some(v) => std::env::set_var("DARKMUX_ENV_AUDIT_LOG", v),
                None => std::env::remove_var("DARKMUX_ENV_AUDIT_LOG"),
            }
        }

        // Keys these resolvers read that are NOT write destinations.
        //
        // This is deny-by-default on purpose. The previous cut was an
        // INCLUSION filter on the project's naming convention (`_DIR` /
        // `_FILE` / `_PATH`, plus two names), and a destination variable
        // that did not happen to obey it escaped silently — measured in
        // review with a probe accessor reading `DARKMUX_PROBE_ROOT`, which
        // this test did not notice even with its driver call added. An
        // exclusion list inverts that: a new key is guarded unless someone
        // writes it down here, and writing it down is a reviewable act.
        //
        // Measured 2026-09-12: the resolvers driven below read 13 keys and
        // all 13 are destinations, so this list is empty. A behavior knob
        // read by a destination resolver belongs here — NOT in
        // `PINNED_STATE_VARS`, which would pin a knob to a path.
        const NOT_A_DESTINATION: &[&str] = &[];

        let raw = std::fs::read_to_string(&log_path).unwrap_or_default();
        let mut destination_keys: Vec<&str> = raw
            .lines()
            .filter_map(|l| l.split('\t').nth(1))
            .filter(|k| k.starts_with("DARKMUX_") && !NOT_A_DESTINATION.contains(k))
            .collect();
        destination_keys.sort_unstable();
        destination_keys.dedup();

        assert!(
            !destination_keys.is_empty(),
            "the env-read audit recorded nothing — this test proves nothing unless the \
             instrumentation actually fired. Log at {}",
            log_path.display()
        );

        let known: Vec<&str> = PINNED_STATE_VARS
            .iter()
            .map(|(v, _)| *v)
            .chain(CLEARED_STATE_VARS.iter().copied())
            .collect();
        let unguarded: Vec<&&str> =
            destination_keys.iter().filter(|k| !known.contains(k)).collect();
        assert!(
            unguarded.is_empty(),
            "these variables are read by darkmux's own resolvers and are accounted for \
             nowhere: {unguarded:?}. Each needs ONE of three homes. Decide by asking what the \
             variable IS — not by picking whichever makes this test pass:\n\
             \n\
             • It NAMES a write destination and nothing more -> \
             `test_isolation::PINNED_STATE_VARS`, with the subpath its built-in default \
             produces.\n\
             • It names a destination AND its presence changes behavior (the way \
             `DARKMUX_AUDIT_DIR`'s mere presence turns the hash-chained sink on) -> \
             `test_isolation::CLEARED_STATE_VARS`, which removes it instead of pinning it.\n\
             • It is a BEHAVIOR KNOB that a destination resolver happens to read — a bool, a \
             number, an enum, a mode — -> `NOT_A_DESTINATION`, the list at the top of THIS \
             test. Never `PINNED_STATE_VARS`: that would write a path string into a knob, \
             which a bool reads as false and a number or an enum reads as garbage, and this \
             test would go green having silently changed the behavior under test.\n\
             \n\
             Recorded keys: {destination_keys:?}"
        );
    }

    // ─── #2914: the machine utility model ─────────────────────────────

    /// (#2914) A profile that still lists the utility model as a work model
    /// is flagged, naming the profile(s), the window each declared, and the
    /// fix (declare the window in `internal.utility`, drop the entry).
    #[test]
    fn utility_in_profiles_warns_naming_each_profile_and_the_fix() {
        let registry: darkmux_types::ProfileRegistry = serde_json::from_value(serde_json::json!({
            "internal": { "utility": { "id": "util-4b" } },
            "profiles": {
                "deep": { "models": [{ "id": "primary-big", "n_ctx": 262144 }, { "id": "darkmux:util-4b", "n_ctx": 120000 }] },
                "radio": { "models": [{ "id": "util-4b", "n_ctx": 16000 }] },
                "clean": { "models": [{ "id": "worker-35b", "n_ctx": 65536 }] }
            }
        }))
        .unwrap();
        let c = super::utility_in_profiles_status(&registry);
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert!(c.message.contains("deep") && c.message.contains("radio"), "names both: {}", c.message);
        assert!(!c.message.contains("clean"), "a clean profile is not named: {}", c.message);
        let hint = c.hint.clone().unwrap_or_default();
        assert!(hint.contains("internal.utility") && hint.contains("n_ctx"), "names the fix: {hint}");
        assert!(hint.contains("120000"), "carries the window the operator declared, so nothing is lost: {hint}");

        let clean: darkmux_types::ProfileRegistry = serde_json::from_value(serde_json::json!({
            "internal": { "utility": { "id": "util-4b", "n_ctx": 120000 } },
            "profiles": { "clean": { "models": [{ "id": "worker-35b", "n_ctx": 65536 }] } }
        }))
        .unwrap();
        assert_eq!(super::utility_in_profiles_status(&clean).status, Status::Pass);
        let unbound: darkmux_types::ProfileRegistry =
            serde_json::from_value(serde_json::json!({ "profiles": { "radio": { "models": [{ "id": "util-4b", "n_ctx": 16000 }] } } }))
                .unwrap();
        assert_eq!(super::utility_in_profiles_status(&unbound).status, Status::Pass, "no binding, nothing to compare");
    }

    /// (#2914 review, C2) A hosted model sharing the utility id is not a
    /// leftover: it is served elsewhere, never the local utility instance,
    /// so the check does not name it.
    #[test]
    fn utility_in_profiles_ignores_a_hosted_model_sharing_the_id() {
        let registry: darkmux_types::ProfileRegistry = serde_json::from_value(serde_json::json!({
            "internal": { "utility": { "id": "util-4b", "n_ctx": 120000 } },
            "profiles": {
                "hosted": { "models": [{ "id": "util-4b", "endpoint": "provider" }] }
            },
            "endpoints": { "provider": { "url": "https://provider.example/v1" } }
        }))
        .unwrap();
        let c = super::utility_in_profiles_status(&registry);
        assert_eq!(c.status, Status::Pass, "{}", c.message);
    }

    /// (#2914 review, C6) There is no CLI removal for a `role_profiles`
    /// binding, so the leftover message says to edit config.json by hand
    /// and names the path, the way every other removed key's check does.
    #[test]
    fn removed_radio_router_binding_says_to_edit_config_json_by_hand() {
        let c = super::removed_radio_router_staffing_status(Some("radio"));
        let expected_config = super::home_display(&darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).config);
        assert!(c.message.contains(&expected_config), "names the file: {}", c.message);
        assert!(c.message.contains("by hand"), "{}", c.message);
    }

    #[test]
    fn crew_role_prompt_coverage_hint_points_to_correct_loader_and_roles_dir() {
        let c = super::role_prompt_coverage_status(&["analyst"]);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("`analyst`"), "{}", c.message);
        let hint = c.hint.expect("a missing prompt carries a hint");
        assert!(hint.contains("crates/darkmux-crew/src/loader.rs"), "loader path: {hint}");
        assert!(!hint.contains("src/crew/loader.rs"), "stale loader path: {hint}");
        let expected_roles = super::home_display(
            &darkmux_types::paths::resolve(darkmux_types::paths::ResolveScope::ForceUser).root.join("roles"),
        );
        assert!(hint.contains(&expected_roles), "roles dir: {hint}");
        assert_eq!(super::role_prompt_coverage_status(&[]).status, Status::Pass);
    }

    /// (#3081) Doctor output gets pasted into issues: the home prefix prints
    /// as `~`, a path outside home prints in full.
    #[test]
    fn home_display_prints_the_home_prefix_as_a_tilde() {
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(super::home_display(&home.join(".darkmux/profiles.json")), "~/.darkmux/profiles.json");
        assert_eq!(super::home_display(&home), "~");
        assert_eq!(super::home_display(std::path::Path::new("/srv/darkmux/profiles.json")), "/srv/darkmux/profiles.json");
    }

    /// (#3081) The unreachable-residents hint names the registry that was
    /// actually loaded, not the default user location.
    #[test]
    fn unreachable_residents_hint_names_the_loaded_registry_path() {
        let registry = registry_with(&[("balanced", &[("qwen/qwen3.8-27b", None)])]);
        let loaded = [lm("darkmux:orphan", "orphan")];
        let c = super::unreachable_residents_status(&loaded, &registry, std::path::Path::new("/srv/alt/profiles.json"));
        let hint = c.hint.expect("hint");
        assert!(hint.contains("/srv/alt/profiles.json"), "{hint}");
    }

    /// (#2914) The removed routing-seat binding `role_profiles.radio-router` is
    /// named with the fix; nothing set is a Pass.
    #[test]
    fn removed_radio_router_staffing_names_the_leftover_binding() {
        let c = super::removed_radio_router_staffing_status(None);
        assert_eq!(c.status, Status::Pass, "{}", c.message);

        let c = super::removed_radio_router_staffing_status(Some("radio"));
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("role_profiles.radio-router") && c.message.contains("radio"), "{}", c.message);
        let hint = c.hint.clone().unwrap_or_default();
        assert!(hint.contains("internal.utility"), "the fix: {hint}");
    }

    /// (#2914) The binding check reports the declared window, and points a
    /// binding with no window at declaring one.
    #[test]
    fn utility_binding_reports_its_window_and_nudges_one_with_no_window() {
        let loaded = vec![lm("darkmux:util-4b", "util-4b")];
        let c = super::utility_binding_status(Some("util-4b"), Some(120_000), Some(&loaded));
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("120000"), "{}", c.message);
        let c = super::utility_binding_status(Some("util-4b"), None, Some(&loaded));
        assert_eq!(c.status, Status::Pass, "a binding with no window still works: {}", c.message);
        assert!(
            c.hint.clone().unwrap_or_default().contains("n_ctx"),
            "but the hint says to declare the window: {:?}",
            c.hint
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
            queued: None,
        }
    }

    /// (Third review round) Bumped from Pass to Warn — see the comment on
    /// `utility_binding_status`'s `None` arm for why. The hint must not
    /// open with "Optional:" any more (that read as downgrading a message
    /// that says compaction is off machine-wide); it says plainly that no
    /// action is needed if the operator is doing this deliberately.
    #[test]
    fn utility_binding_unregistered_warns_with_setup_hint() {
        let c = super::utility_binding_status(None, None, None);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("no machine utility model"));
        let hint = c.hint.unwrap();
        assert!(hint.contains("internal"));
        assert!(
            !hint.starts_with("Optional:"),
            "the hint must not open by downgrading a message that says compaction is off \
             machine-wide: {hint}"
        );
        assert!(
            hint.to_ascii_lowercase().contains("no action needed"),
            "the hint must say plainly that no action is needed if this is deliberate: {hint}"
        );
    }

    /// (Second review round, #2571) Pins the corrected message — the
    /// pre-fix text said "compaction uses the runtime default", which
    /// became false the moment #2571 removed `DEFAULT_COMPACTOR_MODEL`
    /// from production. (Third review round: this check is now a Warn, not
    /// a Pass — see `utility_binding_status`'s `None` arm — but the wording
    /// must still agree with the disclosure messages
    /// (`compactor_disclosure_message` / `unset_compactor_warning`), which
    /// is what this test pins.)
    #[test]
    fn utility_binding_unregistered_message_says_compaction_is_off_not_defaulted() {
        let c = super::utility_binding_status(None, None, None);
        assert!(
            c.message.to_ascii_lowercase().contains("off"),
            "message must say compaction is OFF, not that it falls back to a default: {}",
            c.message
        );
        assert!(
            !c.message.to_ascii_lowercase().contains("runtime default"),
            "message must not claim a runtime default exists — #2571 removed it: {}",
            c.message
        );
    }

    #[test]
    fn utility_binding_registered_but_lms_unreachable_warns() {
        let c = super::utility_binding_status(Some("darkmux:util-4b"), None, None);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("couldn't query LMStudio"));
    }

    #[test]
    fn utility_binding_registered_and_loaded_passes() {
        // Match by modelKey...
        let loaded = vec![lm("darkmux:util-4b", "util-4b"), lm("worker", "worker-35b")];
        let c = super::utility_binding_status(Some("util-4b"), None, Some(&loaded));
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("registered and loaded"));
        // ...or by the namespaced identifier.
        let c2 = super::utility_binding_status(Some("darkmux:util-4b"), None, Some(&loaded));
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
        let c = super::utility_binding_status(Some("util-4b"), None, Some(&loaded));
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
        // (#2912/#2913) A third assertion used to pin that the hint named no
        // verb as needing this resident first; the two verbs it named are
        // gone, and `utility_model_id()` still only ever names the compactor.
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

    // ─── check_unreachable_darkmux_residents (#1944) ───────────────────────

    /// `models` pairs are `(id, explicit_identifier)` — a `None` explicit
    /// identifier means the namespaced default (`namespaced_identifier`'s
    /// documented opt-out shape).
    /// One profile fixture: its name, and the models it declares.
    type ProfileSpec<'a> = (&'a str, &'a [(&'a str, Option<&'a str>)]);

    fn registry_with(profiles: &[ProfileSpec<'_>]) -> darkmux_types::ProfileRegistry {
        let mut map = std::collections::BTreeMap::new();
        for (name, models) in profiles {
            let profile_models = models
                .iter()
                .map(|(id, explicit_identifier)| darkmux_types::ProfileModel {
                    id: id.to_string(),
                    identifier: explicit_identifier.map(str::to_string),
                    ..Default::default()
                })
                .collect();
            map.insert(
                (*name).to_string(),
                darkmux_types::Profile { models: profile_models, ..Default::default() },
            );
        }
        darkmux_types::ProfileRegistry { profiles: map, ..Default::default() }
    }

    #[test]
    fn unreachable_residents_no_loaded_models_passes() {
        let registry = registry_with(&[]);
        let c = super::unreachable_residents_status(&[], &registry, std::path::Path::new("/x/profiles.json"));
        assert_eq!(c.status, Status::Pass);
    }

    #[test]
    fn unreachable_residents_addressable_resident_passes() {
        let registry = registry_with(&[("balanced", &[("qwen/qwen3.8-27b", None)])]);
        let loaded = vec![lm("darkmux:qwen/qwen3.8-27b", "qwen/qwen3.8-27b")];
        let c = super::unreachable_residents_status(&loaded, &registry, std::path::Path::new("/x/profiles.json"));
        assert_eq!(c.status, Status::Pass, "message: {}", c.message);
    }

    /// The live #1944 trace: a superseded review-staffing seat's identifier
    /// (`qwen38-probe`) is no longer declared by ANY profile — WARN, name
    /// it, and hint the surgical `lms unload` the issue's operator ran by
    /// hand.
    #[test]
    fn unreachable_residents_names_the_orphan_and_hints_lms_unload() {
        let registry = registry_with(&[("balanced", &[("qwen/qwen3.8-27b", None)])]);
        let loaded = vec![
            lm("darkmux:qwen/qwen3.8-27b", "qwen/qwen3.8-27b"),
            lm("darkmux:qwen38-probe", "qwen/qwen3.8-27b"),
        ];
        let c = super::unreachable_residents_status(&loaded, &registry, std::path::Path::new("/x/profiles.json"));
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("darkmux:qwen38-probe"), "names the orphan: {}", c.message);
        assert!(
            !c.message.contains("darkmux:qwen/qwen3.8-27b"),
            "an addressable sibling is not swept into the warning: {}",
            c.message
        );
        let hint = c.hint.expect("a warn carries a remedy");
        assert!(
            hint.contains("lms unload darkmux:qwen38-probe"),
            "hint names the exact surgical command: {hint}"
        );
    }

    /// Operator sovereignty's inverted-case test: a NON-namespaced resident
    /// — user state darkmux never brought up — must NEVER be flagged here,
    /// no matter how far it is from anything a profile declares. This pins
    /// the namespace convention's absolute half: user state is visible to
    /// the planner as pool consumption only and is structurally off-limits
    /// to any darkmux suggestion, this check included. A one-directional
    /// test (only proving orphans get flagged) would risk shipping a
    /// doctor check that also — wrongly — nudges the operator to `lms
    /// unload` their own state.
    ///
    /// The fixtures are deliberate NEAR-MISSES of the `darkmux:` namespace
    /// prefix, not just any foreign string — `is_darkmux_owned` is a pure
    /// `starts_with(DARKMUX_NAMESPACE)` check
    /// (`darkmux_gestalt::ownership::is_darkmux_owned`), and a plain
    /// `some-user-loaded-model` fixture can't tell that implementation
    /// apart from a weaker `.contains("darkmux:")` one — both would
    /// correctly ignore it. `my-darkmux:orphan` / `predarkmux:orphan`
    /// contain the namespace as a substring without starting with it (a
    /// `.contains` implementation would wrongly flag either — e.g. a user
    /// model the operator happened to alias `my-darkmux:foo` would get an
    /// unload suggestion for their own state); `DARKMUX:orphan` pins the
    /// case-sensitivity half of the same prefix check.
    #[test]
    fn unreachable_residents_never_flags_a_foreign_resident() {
        let registry = registry_with(&[("balanced", &[("qwen/qwen3.8-27b", None)])]);
        let loaded = vec![
            lm("my-darkmux:orphan", "orphan"),
            lm("predarkmux:orphan", "orphan"),
            lm("DARKMUX:orphan", "orphan"),
        ];
        let c = super::unreachable_residents_status(&loaded, &registry, std::path::Path::new("/x/profiles.json"));
        assert_eq!(
            c.status,
            Status::Pass,
            "a foreign resident whose identifier merely CONTAINS the darkmux: namespace (or differs only by case) is never this check's business: {}",
            c.message
        );
        for id in ["my-darkmux:orphan", "predarkmux:orphan", "DARKMUX:orphan"] {
            assert!(
                !c.message.contains(id),
                "a foreign identifier must never appear in this check's output, suggested or otherwise: {}",
                c.message
            );
        }
    }

    /// The machine utility (compactor) binding (#590) is the one darkmux-
    /// owned identifier legitimately absent from every profile's
    /// `models[]` — it must count as addressable, or every machine with a
    /// utility model registered gets a permanent false-positive Warn.
    #[test]
    fn unreachable_residents_utility_binding_counts_as_addressable() {
        let mut registry = registry_with(&[("balanced", &[("qwen/qwen3.8-27b", None)])]);
        registry.internal = Some(darkmux_types::RegistryInternal { utility: Some(darkmux_types::UtilityBinding { id: "util-4b".into(), ..Default::default() }) });
        let loaded = vec![
            lm("darkmux:qwen/qwen3.8-27b", "qwen/qwen3.8-27b"),
            lm("darkmux:util-4b", "util-4b"),
        ];
        let c = super::unreachable_residents_status(&loaded, &registry, std::path::Path::new("/x/profiles.json"));
        assert_eq!(c.status, Status::Pass, "the utility binding is addressable, not orphaned: {}", c.message);
    }

    /// An explicit-identifier override (the namespace opt-out, #52) is
    /// honored: a profile model that sets `identifier` explicitly makes
    /// THAT string the addressable form, not the namespaced default —
    /// matching `namespaced_identifier`'s own documented pass-through.
    #[test]
    fn unreachable_residents_honors_an_explicit_identifier_override() {
        let registry = registry_with(&[("balanced", &[("qwen/qwen3.8-27b", Some("darkmux:my-alias"))])]);
        let loaded = vec![lm("darkmux:my-alias", "qwen/qwen3.8-27b")];
        let c = super::unreachable_residents_status(&loaded, &registry, std::path::Path::new("/x/profiles.json"));
        assert_eq!(
            c.status,
            Status::Pass,
            "an explicit alias is addressable under its own spelling: {}",
            c.message
        );
    }

    /// (#1944 CONSIDER 4) A resident that's actually unreachable for an
    /// unrelated reason (its declaring profile is quarantined, not that it
    /// was never declared at all) still WARNs — the check can't tell those
    /// two cases apart from `LoadedModel` alone — but the hint names the
    /// quarantine so the operator doesn't draw "genuinely orphaned" from a
    /// warning that's actually "waiting on a typo fix".
    #[test]
    fn unreachable_residents_hint_names_a_quarantined_profile() {
        let mut registry = registry_with(&[("balanced", &[("qwen/qwen3.8-27b", None)])]);
        registry.quarantined.push(darkmux_types::QuarantinedEntry {
            kind: darkmux_types::QuarantinedEntryKind::Profile,
            name: "broken-profile".to_string(),
            error: "missing field `models`".to_string(),
        });
        let loaded = vec![lm("darkmux:orphan", "orphan")];
        let c = super::unreachable_residents_status(&loaded, &registry, std::path::Path::new("/x/profiles.json"));
        assert_eq!(c.status, Status::Warn);
        let hint = c.hint.expect("a warn carries a remedy");
        assert!(
            hint.contains("broken-profile") && hint.contains("quarantined"),
            "hint names the quarantined profile so the operator doesn't assume a genuine orphan: {hint}"
        );
    }

    /// (#2902 re-review C2) An endpoint quarantine is named as one, never
    /// counted as a "profile entry".
    #[test]
    fn unreachable_residents_hint_names_each_quarantine_by_kind() {
        let mut registry = registry_with(&[("balanced", &[("qwen/qwen3.8-27b", None)])]);
        for (kind, name) in [
            (darkmux_types::QuarantinedEntryKind::Profile, "broken-profile"),
            (darkmux_types::QuarantinedEntryKind::Endpoint, "broken-endpoint"),
        ] {
            registry.quarantined.push(darkmux_types::QuarantinedEntry { kind, name: name.into(), error: "x".into() });
        }
        let c = super::unreachable_residents_status(&[lm("darkmux:orphan", "orphan")], &registry, std::path::Path::new("/x/profiles.json"));
        let hint = c.hint.expect("a warn carries a remedy");
        assert!(hint.contains("2 registry entries are currently quarantined"), "{hint}");
        assert!(hint.contains("profile \"broken-profile\"") && hint.contains("endpoint \"broken-endpoint\""), "{hint}");
        assert!(!hint.contains("profile entr"), "{hint}");
    }

    // ─── role_profiles coherence (#1475 packet 1, #1547) ─────────────────
    // The pure `role_profiles_status` takes the config map + the registry's
    // defined profiles + the role library's known ids explicitly, so every arm
    // is testable with no config.json / registry / role library on disk. A
    // dangling binding (role -> undefined profile, or an unknown role id)
    // WARNs; an all-resolving map (and the empty map) Pass. Bindings use REAL
    // role ids (`pr-reviewer`, `code-reviewer`, `analyst`, `crawler`) —
    // the bare `judge`/`verify`/`probe-high` this suite used pre-#1547 are
    // not real role ids and were themselves an instance of the trap #1547
    // fixes (a doc/test example that reads as live but no-ops). (#2418: the
    // suite previously used the review funnel's own `review-judge`/
    // `review-verify`/`review-probe-high`/`review-probe-low` role ids —
    // those roles were retired along with the funnel, #2310 P4d, so this
    // suite now names roles that still exist.)
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
    const REAL_ROLES: &[&str] = &["pr-reviewer", "code-reviewer", "analyst", "crawler"];

    #[test]
    fn role_profiles_empty_map_passes() {
        let c = super::role_profiles_status(&bindings(&[]), &known(&["qwen35b"]), &quarantined(&[]), &roles(REAL_ROLES));
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("no role->profile bindings"));
    }

    #[test]
    fn role_profiles_all_defined_passes() {
        let map = bindings(&[
            ("pr-reviewer", "qwen35b"),
            ("code-reviewer", "qwen35b"),
            ("crawler", "qwen4b"),
        ]);
        let c = super::role_profiles_status(&map, &known(&["qwen35b", "qwen4b"]), &quarantined(&[]), &roles(REAL_ROLES));
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("3 role->profile bindings"), "got: {}", c.message);
        assert!(c.message.contains("all name a real role and a defined profile"), "got: {}", c.message);
    }

    #[test]
    fn role_profiles_a_radio_host_peer_address_is_not_dangling() {
        let roles = roles(&["radio-host", "analyst"]);
        let ok = bindings(&[("radio-host", "deep@studio")]);
        let c = super::role_profiles_status(&ok, &known(&["qwen35b"]), &quarantined(&[]), &roles);
        assert_eq!(c.status, Status::Pass, "the profile lives on the peer: {}", c.message);
        // Only the answering seat runs on a peer; another role's address, and a
        // malformed one, still cannot resolve here.
        let other = bindings(&[("analyst", "deep@studio")]);
        assert_eq!(super::role_profiles_status(&other, &known(&[]), &quarantined(&[]), &roles).status, Status::Warn);
        let bad = bindings(&[("radio-host", "a@b@c")]);
        assert_eq!(super::role_profiles_status(&bad, &known(&[]), &quarantined(&[]), &roles).status, Status::Warn);
    }

    // ─── radio answering seat on a peer: the machine must be known ───────

    fn machines(names: &[&str]) -> std::collections::BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn route(source: &str, role: &str, address: &str) -> super::FleetRoute {
        super::FleetRoute { source: source.into(), role: role.into(), address: address.into() }
    }

    fn asked(outcome: darkmux_fleet::CheckOutcome) -> darkmux_fleet::ReadOnlyCheck {
        darkmux_fleet::ReadOnlyCheck::Asked(outcome)
    }

    fn routable() -> darkmux_fleet::ReadOnlyCheck {
        asked(darkmux_fleet::CheckOutcome::Routable {
            profile: "deep".into(),
            report: darkmux_fleet::CheckReport {
                endpoint: darkmux_fleet::EndpointClass::Managed,
                seat: darkmux_fleet::SeatOutlook::Free,
            },
        })
    }

    /// Only a well-formed address naming ANOTHER machine is a route to check:
    /// the answerer runs `radio-host`, each binding its own role; a bare
    /// profile, a malformed address and this machine's own name are not.
    #[test]
    fn only_addresses_naming_another_machine_are_routes() {
        let bindings: std::collections::BTreeMap<String, String> = [
            ("radio-host", "deep@studio"),
            ("coder", "coder-big"),
            ("analyst", "x@y@z"),
            ("reviewer", "deep@Laptop"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let routes = super::fleet_routes(Some("fast@mini".into()), &bindings, Some("laptop"));
        assert_eq!(
            routes,
            vec![
                route("radio.answerer_profile", "radio-host", "fast@mini"),
                route("role_profiles.radio-host", "radio-host", "deep@studio"),
            ]
        );
        assert!(super::fleet_routes(None, &Default::default(), None).is_empty());
    }

    /// A routable route passes and says what would run. A refusal is a Warn
    /// carrying the receiver's typed code and sentence; an unreachable peer is
    /// a Warn with the reason, never a Fail. One bad route among good ones
    /// still warns.
    #[test]
    fn a_route_is_ok_or_the_receivers_typed_refusal_and_never_a_fail() {
        use darkmux_fleet::{CheckOutcome, RefusalCode};
        let r = route("radio.answerer_profile", "radio-host", "deep@studio");
        let c = super::fleet_routes_status(&[(r.clone(), routable())]);
        assert_eq!(c.status, Status::Pass);
        assert!(c.message.contains("would run role radio-host on deep (managed; seat free)"), "{}", c.message);

        let refused = CheckOutcome::Refused { code: RefusalCode::RoleNotAllowed, reason: "studio lets laptop run roles: none".into() };
        let c = super::fleet_routes_status(&[(r.clone(), asked(refused))]);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("refused by the receiver (role_not_allowed): studio lets laptop run roles: none"), "{}", c.message);

        let down = CheckOutcome::Unanswered { detail: "studio did not answer".into() };
        let c = super::fleet_routes_status(&[(r.clone(), asked(down))]);
        assert_eq!(c.status, Status::Warn, "an unreachable peer warns");
        assert!(c.message.contains("could not be asked: studio did not answer"), "{}", c.message);

        let c = super::fleet_routes_status(&[(r.clone(), routable()), (route("role_profiles.coder", "coder", "big@mini"), asked(CheckOutcome::Unanswered { detail: "x".into() }))]);
        assert_eq!(c.status, Status::Warn);
        assert_eq!(super::fleet_routes_status(&[]).status, Status::Pass);
    }

    /// A peer whose node is not pinned yet is a Warn that names the fix, never
    /// a Fail, and never claims the route works.
    #[test]
    fn an_unpinned_peer_warns_and_says_doctor_pinned_nothing() {
        let r = route("radio.answerer_profile", "radio-host", "deep@studio");
        let c = super::fleet_routes_status(&[(r, darkmux_fleet::ReadOnlyCheck::NotPinned { machine: "studio".into() })]);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("not pinned yet: the first `dispatch` or radio answer to studio pins its node"), "{}", c.message);
    }

    fn seats(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(source, reference)| (source.to_string(), reference.to_string())).collect()
    }

    /// A seat written `<profile>@<machine>` sends the answering seat to that
    /// machine, so a machine that is not in this machine's roster (and is not
    /// this machine) can never be reached: the dispatch would fail at the
    /// first question. The warning names the seat, the machine, and the fix.
    #[test]
    fn radio_peer_seat_naming_an_unknown_machine_warns_naming_it() {
        let c = super::radio_peer_seat_status(
            &seats(&[("radio.answerer_profile", "deep@ghost"), ("role_profiles.radio-host", "deep@studio")]),
            &machines(&["studio", "laptop"]),
        );
        assert_eq!(c.status, Status::Warn, "{}", c.message);
        assert!(c.message.contains("radio.answerer_profile") && c.message.contains("`ghost`"), "{}", c.message);
        assert!(!c.message.contains("studio"), "a known machine is not named as a problem: {}", c.message);
        let hint = c.hint.expect("a warn carries a remedy");
        assert!(hint.contains("darkmux machine add"), "{hint}");
    }

    #[test]
    fn radio_peer_seat_passes_for_known_machines_local_names_and_no_address() {
        let known = machines(&["studio"]);
        // Roster names match case-insensitively, as everywhere else.
        let c = super::radio_peer_seat_status(&seats(&[("radio.answerer_profile", "deep@STUDIO")]), &known);
        assert_eq!(c.status, Status::Pass, "{}", c.message);
        // A bare profile name, and a malformed address (a different check's
        // finding), are not this check's business.
        assert_eq!(super::radio_peer_seat_status(&seats(&[("radio.answerer_profile", "deep")]), &known).status, Status::Pass);
        assert_eq!(super::radio_peer_seat_status(&seats(&[("radio.answerer_profile", "a@b@c")]), &known).status, Status::Pass);
        assert_eq!(super::radio_peer_seat_status(&[], &known).status, Status::Pass);
    }

    #[test]
    fn role_profiles_dangling_binding_warns_and_names_the_pair() {
        let map = bindings(&[("pr-reviewer", "qwen35b"), ("analyst", "ghost27b")]);
        let c = super::role_profiles_status(&map, &known(&["qwen35b", "qwen4b"]), &quarantined(&[]), &roles(REAL_ROLES));
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("analyst -> ghost27b"), "names the dangling pair: {}", c.message);
        assert!(c.message.contains("undefined profile"), "genuinely-absent target reads as undefined: {}", c.message);
        assert!(!c.message.contains("pr-reviewer -> qwen35b"), "the resolving binding is not flagged: {}", c.message);
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
        let map = bindings(&[("pr-reviewer", "qwen35b"), ("code-reviewer", "broken")]);
        let c = super::role_profiles_status(
            &map,
            &known(&["qwen35b"]),
            &quarantined(&["broken"]),
            &roles(REAL_ROLES),
        );
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("code-reviewer -> broken"), "names the quarantined pair: {}", c.message);
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
        let map = bindings(&[("pr-reviewer", "ghost27b"), ("code-reviewer", "broken")]);
        let c = super::role_profiles_status(
            &map,
            &known(&["qwen35b"]),
            &quarantined(&["broken"]),
            &roles(REAL_ROLES),
        );
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("pr-reviewer -> ghost27b"), "names the undefined pair: {}", c.message);
        assert!(c.message.contains("code-reviewer -> broken"), "names the quarantined pair: {}", c.message);
        assert!(c.message.contains("undefined profile"), "undefined segment present: {}", c.message);
        assert!(c.message.contains("quarantined profile"), "quarantined segment present: {}", c.message);
        let hint = c.hint.unwrap();
        assert!(hint.contains("add the profile"), "undefined hint present: {hint}");
        assert!(hint.contains("fix the profile entry"), "quarantined hint present: {hint}");
    }

    /// (#1547) The trap this issue is named for: doctor's + config_cmd's own
    /// worked examples (and this very test file, pre-#1547) bound bare
    /// `judge`/`verify`/`probe-high` — none of which are real role ids (the
    /// real ones are `pr-reviewer`/`code-reviewer`/`analyst`) — and
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
        let map = bindings(&[("judge", "qwen35b"), ("analyst", "ghost27b")]);
        let c = super::role_profiles_status(&map, &known(&["qwen35b"]), &quarantined(&[]), &roles(REAL_ROLES));
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("unknown role id"), "unknown-role segment present: {}", c.message);
        assert!(c.message.contains("undefined profile"), "undefined-profile segment present: {}", c.message);
        assert!(c.message.contains("judge -> qwen35b"), "names the unknown-role pair: {}", c.message);
        assert!(c.message.contains("analyst -> ghost27b"), "names the undefined-profile pair: {}", c.message);
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
    #[serial_test::serial]
    fn platform_check_always_present() {
        let r = with_isolated_darkmux_home(run);
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

                // Send HTTP 200 response with a body shaped like the real
                // `/health` handler's (#1665 — a 200 with no
                // `darkmux_version` field no longer counts as identity, see
                // `daemon_reachable_check_warns_on_a_200_with_no_darkmux_identity`
                // below).
                let body = r#"{"darkmux_version":"9.9.9"}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
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
            "daemon reachable check should pass when health returns 200 with darkmux's own \
             body shape. Got message: {}",
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

    /// (#1665) The "port squatter" gap named in the issue: `check_daemon_reachable_impl`
    /// used to Pass on ANY 200 at `/health`, so a stray process holding
    /// 8765 (a dev server, `python -m http.server`, another operator's
    /// tool) read as "the darkmux viewer is reachable" with zero identity
    /// verification. A 200 with a body that doesn't carry `darkmux_version`
    /// must now Warn, naming the observation rather than asserting a
    /// verdict about what's actually listening there.
    #[test]
    fn daemon_reachable_check_warns_on_a_200_with_no_darkmux_identity() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind test server");
        let port = listener.local_addr().unwrap().port();

        let server_handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                // A generic 200 body a port squatter (an unrelated dev
                // server, a stray static file server) would plausibly send —
                // no `darkmux_version` field anywhere in it.
                let body = "<html><body>hello</body></html>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        thread::sleep(Duration::from_millis(50));

        let check = check_daemon_reachable_impl("127.0.0.1", port);

        assert_eq!(
            check.status,
            Status::Warn,
            "a 200 with no darkmux identity must not read as the daemon being reachable: {check:?}"
        );
        assert!(check.message.contains("darkmux_version"), "{}", check.message);
        assert!(
            !check.message.contains("viewer http://"),
            "must not hand out a viewer link for a socket that isn't confirmed to be darkmux: {}",
            check.message
        );

        drop(server_handle);
    }

    /// (#1665 review CONSIDER 3) A real daemon whose headers and body
    /// arrive in TWO separate TCP segments — a flush right after the
    /// status line + headers, then the JSON body ~120ms later — must
    /// still read as reachable. Before `read_full_http_response`, ONE 1
    /// KiB read captured only the headers, so `response_names_darkmux`
    /// (which looks for `darkmux_version` in the body) came back `false`
    /// and this exact healthy daemon read as a "port squatter" warning.
    #[test]
    fn daemon_reachable_check_survives_a_response_split_across_two_tcp_segments() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind test server");
        let port = listener.local_addr().unwrap().port();

        let server_handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);

                let body = r#"{"darkmux_version":"9.9.9"}"#;
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                // Segment 1: headers only, flushed immediately.
                let _ = stream.write_all(headers.as_bytes());
                let _ = stream.flush();
                // Segment 2: the body, well after the first read a
                // single-read probe would have already returned from.
                thread::sleep(Duration::from_millis(120));
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });

        thread::sleep(Duration::from_millis(50));

        let check = check_daemon_reachable_impl("127.0.0.1", port);

        assert_eq!(
            check.status,
            Status::Pass,
            "a real daemon split across two TCP segments must still read as reachable: {check:?}"
        );
        assert!(
            check.message.contains(&format!("viewer http://127.0.0.1:{port}/")),
            "{}",
            check.message
        );

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

    /// Runs `check_ignored_project_darkmux` from a fresh cwd holding `files`
    /// (paths relative to it; a trailing `/` makes a directory), with
    /// `DARKMUX_HOME` set to `home_rel` under it when given.
    fn ignored_project_check(files: &[&str], home_rel: Option<&str>) -> Check {
        let tmp = tempfile::TempDir::new().unwrap();
        for f in files {
            let path = tmp.path().join(f);
            if f.ends_with('/') {
                std::fs::create_dir_all(&path).unwrap();
            } else {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, "{}").unwrap();
            }
        }
        let prev_cwd = std::env::current_dir().unwrap();
        let prev_home = std::env::var("DARKMUX_HOME").ok();
        unsafe {
            match home_rel {
                Some(h) => std::env::set_var("DARKMUX_HOME", tmp.path().join(h)),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        std::env::set_current_dir(tmp.path()).unwrap();
        let check = check_ignored_project_darkmux();
        std::env::set_current_dir(prev_cwd).unwrap();
        unsafe {
            match prev_home {
                Some(h) => std::env::set_var("DARKMUX_HOME", h),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        check
    }

    /// A repo's `.darkmux/` holding only what darkmux still reads there
    /// (`lessons.db`, `conventions.json`) is a normal state, not a warning.
    #[serial_test::serial]
    #[test]
    fn a_repo_darkmux_dir_holding_only_per_repo_files_is_a_pass() {
        let c = ignored_project_check(&[".darkmux/lessons.db", ".darkmux/conventions.json"], None);
        assert_eq!(c.status, Status::Pass, "{}", c.message);
    }

    /// Anything else in a cwd `.darkmux/` is stranded: the warning names it,
    /// and offers the `DARKMUX_HOME` relocation only when it holds a
    /// `config.json` or `profiles.json` to relocate.
    #[serial_test::serial]
    #[test]
    fn stranded_project_contents_are_named_and_relocation_is_offered_only_for_root_files() {
        let c = ignored_project_check(&[".darkmux/lessons.db", ".darkmux/roles/x.json"], None);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("roles"), "names the stranded entry: {}", c.message);
        assert!(!c.message.contains("lessons.db"), "the per-repo file is not stranded: {}", c.message);
        assert!(c.hint.as_deref().is_some_and(|h| !h.contains("DARKMUX_HOME=")), "{:?}", c.hint);

        let c = ignored_project_check(&[".darkmux/config.json"], None);
        assert_eq!(c.status, Status::Warn);
        assert!(c.hint.as_deref().is_some_and(|h| h.contains("DARKMUX_HOME=")), "{:?}", c.hint);
    }

    /// A cwd `./.darkmux.json` (the old project-local registry) is ignored
    /// too, and named.
    #[serial_test::serial]
    #[test]
    fn a_cwd_dot_darkmux_json_registry_is_named_as_ignored() {
        let c = ignored_project_check(&[".darkmux.json"], None);
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains(".darkmux.json"), "{}", c.message);
        assert!(c.hint.as_deref().is_some_and(|h| h.contains("profiles.json")), "{:?}", c.hint);
    }

    /// `DARKMUX_HOME` pointing at the cwd's `.darkmux` makes it the root, so
    /// nothing in it is stranded; no directory at all is a Pass too.
    #[serial_test::serial]
    #[test]
    fn a_project_dir_adopted_via_darkmux_home_or_absent_is_a_pass() {
        let adopted = ignored_project_check(&[".darkmux/config.json"], Some(".darkmux"));
        assert_eq!(adopted.status, Status::Pass, "{}", adopted.message);
        let absent = ignored_project_check(&[], None);
        assert_eq!(absent.status, Status::Pass, "{}", absent.message);
    }

    // ─── CrewRootGuard ────────────────────────────────────────────────
    //
    // Tests using it run serially because they mutate DARKMUX_HOME — the env
    // var is process-global.

    /// RAII: redirect the darkmux root (DARKMUX_HOME) to a TempDir for the test's duration.
    struct CrewRootGuard {
        prev: Option<String>,
        _tmp: tempfile::TempDir,
        root: std::path::PathBuf,
    }

    impl CrewRootGuard {
        fn new() -> Self {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let root = tmp.path().to_path_buf();
            let prev = std::env::var("DARKMUX_HOME").ok();
            // SAFETY: tests using this guard MUST be #[serial].
            unsafe {
                std::env::set_var("DARKMUX_HOME", &root);
            }
            Self { prev, _tmp: tmp, root }
        }

        fn path(&self) -> &std::path::Path {
            &self.root
        }
    }

    impl Drop for CrewRootGuard {
        fn drop(&mut self) {
            // SAFETY: tests using this guard MUST be #[serial].
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_HOME", v),
                    None => std::env::remove_var("DARKMUX_HOME"),
                }
            }
        }
    }

    // ─── (#2912 review M1) role skill references ──

    /// The pre-4.0 builtin `mission-compiler` manifest, byte-for-byte the
    /// shape an upgrading operator's user tier can hold (it names the
    /// `mission-compiling` skill 4.0 deleted).
    const LEFTOVER_MISSION_COMPILER: &str = r#"{
      "id": "mission-compiler",
      "description": "Utility-family role that takes unstructured intent.",
      "skills": ["mission-compiling"],
      "tool_palette": {"allow": ["read"], "deny": ["edit", "write", "exec", "process"]},
      "escalation_contract": "bail-with-explanation",
      "role_family": "utility"
    }"#;

    #[serial_test::serial]
    #[test]
    fn role_skill_references_pass_for_the_builtin_set() {
        let _guard = CrewRootGuard::new();
        let check = check_role_skill_references();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
    }

    #[serial_test::serial]
    #[test]
    fn role_skill_references_warn_naming_file_skill_and_fix() {
        let guard = CrewRootGuard::new();
        let roles = guard.path().join("roles");
        std::fs::create_dir_all(&roles).unwrap();
        let file = roles.join("mission-compiler.json");
        std::fs::write(&file, LEFTOVER_MISSION_COMPILER).unwrap();
        let check = check_role_skill_references();
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("mission-compiler"), "names the role: {}", check.message);
        assert!(check.message.contains("mission-compiling"), "names the skill: {}", check.message);
        assert!(
            check.message.contains(&file.display().to_string()),
            "names the file: {}",
            check.message
        );
        let hint = check.hint.expect("a fix");
        assert!(hint.contains("remove `mission-compiling`"), "the exact edit: {hint}");
    }

    fn report_of(checks: Vec<Check>) -> DoctorReport {
        DoctorReport { checks }
    }

    fn state_row(name: &str, status: Status) -> Check {
        Check { name: name.into(), status, message: "m".into(), hint: None }
    }

    /// Doctor names the `darkmux-upgrade` skill once, when a retired-key
    /// row is failing; a clean report and an unrelated failure do not.
    #[test]
    fn doctor_points_at_the_upgrade_skill_only_when_a_retired_key_is_found() {
        for name in [
            RETIRED_ENV_CHECK_NAME,
            "user file keys: profiles.json",
        ] {
            let r = report_of(vec![state_row("unrelated", Status::Pass), state_row(name, Status::Fail)]);
            let pointer = upgrade_skill_pointer(&r).unwrap_or_else(|| panic!("{name} must point at the skill"));
            assert!(pointer.contains("upgrade skill") && pointer.contains(UPGRADE_SKILL_URL), "{pointer}");
        }
        let clean = report_of(vec![state_row(RETIRED_ENV_CHECK_NAME, Status::Pass), state_row("unrelated", Status::Fail)]);
        assert!(upgrade_skill_pointer(&clean).is_none(), "an unrelated failure is not an upgrade finding");
    }

    // ─── ConfigPathGuard ─────────────────────────────────────────

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

    // ─── #1284 Packet 1: check_mission_config_registry ───────────────

    /// The Tier 1 kinds only: what a caller with no wider registry has.
    fn tier1_catalog() -> darkmux_crew::mission_config::KindCatalog {
        darkmux_crew::step_kinds::StepKindRegistry::with_builtins().catalog()
    }

    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_passes_on_embedded_builtins_only() {
        // Empty user dir — only the two embedded built-ins (`review`,
        // `coder-phase`) resolve. Both reference exclusively Tier 3 step
        // kinds, so the check must still PASS (unknown-kind warnings are
        // informational, never blocking — see the check's own doc).
        let _guard = CrewRootGuard::new();
        let check = check_mission_config_registry(&tier1_catalog());
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

        let check = check_mission_config_registry(&tier1_catalog());
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("broken-deps"), "{}", check.message);
        assert!(check.message.contains("ghost-task"), "{}", check.message);
        assert!(check.hint.is_some());
    }

    /// (#2312, #2430) The wiring and retired-id findings reach the doctor row
    /// when the caller supplies a catalog that knows the kinds' ports.
    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_flags_a_miswired_task_and_a_retired_kind_id() {
        use darkmux_crew::mission_config::{KindCatalog, KindPorts};
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        std::fs::write(
            guard.path().join("mission-configs").join("miswired.json"),
            r#"{"id": "miswired", "name": "Miswired", "phases": [{"id": "p1", "tasks": [
                {"id": "producer", "steps": [{"id": "ps", "kind": "procedural.shell"}]},
                {"id": "consumer", "depends_on": ["producer"], "steps": [{"id": "cs", "kind": "dispatch.unit"}]},
                {"id": "old", "steps": [{"id": "os", "kind": "crawl.unit"}]}
            ]}]}"#,
        )
        .unwrap();
        let mut catalog = tier1_catalog();
        catalog.insert(
            "dispatch.unit",
            KindPorts { requires: vec!["plan.sites".into()], ..KindPorts::default() },
        );
        let check = check_mission_config_registry(&catalog);
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("miswired"), "{}", check.message);
        assert!(check.message.contains("plan.sites"), "the wiring miss is named: {}", check.message);
        assert!(check.message.contains("renamed to"), "the retired id is named: {}", check.message);
        assert!(check.message.contains("miswired.json"), "the file to edit is named: {}", check.message);
        assert!(
            check.message.contains("rename `crawl.unit` to `dispatch.unit` and `crawl.summary` to `dispatch.summary`"),
            "both replacements are named: {}",
            check.message
        );
        // The same document against an ids-only catalog sees no wiring miss.
        let blind = check_mission_config_registry(&KindCatalog::from_ids(&["procedural.shell", "dispatch.unit"]));
        assert!(!blind.message.contains("plan.sites"), "{}", blind.message);
    }

    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_warns_on_malformed_json() {
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        std::fs::write(guard.path().join("mission-configs").join("busted.json"), "{not valid json").unwrap();

        let check = check_mission_config_registry(&tier1_catalog());
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

        let check = check_mission_config_registry(&tier1_catalog());
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

        let check = check_mission_config_registry(&tier1_catalog());
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

    /// The 3.x to 4.0 major bump made two changes that break old documents
    /// (the `panel` key is refused, step config is checked). The gates name
    /// the real key problems; this note points an operator at the file whose
    /// declared major is older than this darkmux's.
    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_notes_a_user_tier_config_on_an_older_major() {
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        std::fs::write(
            guard.path().join("mission-configs").join("mine-older.json"),
            r#"{"id":"mine-older","name":"Mine","schema_version":"3.5"}"#,
        )
        .unwrap();

        let check = check_mission_config_registry(&tier1_catalog());
        assert!(check.message.contains("mine-older.json"), "{}", check.message);
        assert!(
            check.message.contains("schema major (3) is older than this darkmux's (4)"),
            "{}",
            check.message
        );
    }

    /// Inverse: a user-tier config AT the current major draws no such note.
    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_has_no_older_major_note_at_the_current_major() {
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        std::fs::write(
            guard.path().join("mission-configs").join("mine-current.json"),
            format!(
                r#"{{"id":"mine-current","name":"Mine","schema_version":"{}"}}"#,
                darkmux_crew::mission_config::MISSION_CONFIG_SCHEMA
            ),
        )
        .unwrap();

        let check = check_mission_config_registry(&tier1_catalog());
        assert!(!check.message.contains("is older than this darkmux's"), "{}", check.message);
    }

    /// (#2428) The direct reproduction of the reported bug: a user-tier
    /// config trailing the binary's schema by one minor validates cleanly
    /// (it's the same "review" fixture the neighboring — now historical —
    /// `_blocks_a_user_tier_copy_trailing_the_current_minor` test builds)
    /// and yet, before this fix, reads as a blocking doctor "issue" purely
    /// because its declared `schema_version` number is old. On the
    /// reporting operator's real machine this hit 13 live, working configs
    /// at once. A minor/patch difference is not a validation failure —
    /// darkmux's mission-config schema is explicitly lenient-on-read — so
    /// this must PASS, and must not count toward "N issue(s)".
    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_passes_when_user_tier_trails_by_a_minor() {
        let (bin_major, bin_minor) = {
            let mut it = darkmux_crew::mission_config::MISSION_CONFIG_SCHEMA.split('.');
            (
                it.next().unwrap().parse::<u32>().unwrap(),
                it.next().unwrap_or("0").parse::<u32>().unwrap(),
            )
        };
        if bin_minor == 0 {
            // No same-major lower minor exists at a `.0` schema — nothing
            // to fixture yet (mirrors the neighboring historical test's
            // own early-return for the same reason).
            return;
        }
        let doc_version = format!("{bin_major}.{}", bin_minor - 1);

        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        std::fs::write(
            guard.path().join("mission-configs").join("review.json"),
            format!(
                r#"{{"id":"review","name":"PR Review (one minor behind)","schema_version":"{doc_version}"}}"#
            ),
        )
        .unwrap();

        let check = check_mission_config_registry(&tier1_catalog());
        assert_eq!(
            check.status,
            Status::Pass,
            "a same-major minor trail validates cleanly and must not block: {}",
            check.message
        );
        assert!(
            !check.message.contains("issue("),
            "must not be counted as an 'N issue(s)' finding: {}",
            check.message
        );
    }

    /// (#1684) The same-major-lower-minor trail this file's own
    /// `check_mission_config_registry_warns_when_user_tier_copy_is_on_an_older_major`
    /// doc comment named as "worth re-testing directly again once a real
    /// 2.1 exists" — #1684's additive field was the first reachable
    /// same-major-minor-trail case since the 2.0 major bump (a same-major
    /// minor-trail finding was a loud `Status::Warn`, same tier as every
    /// other entry `check_mission_config_registry`'s `blocking` vec
    /// collects — see that function's own `if blocking.is_empty()` branch).
    ///
    /// (#2428 UPDATE, 2026-09) This hazard turned out to over-fire in
    /// practice — 13 live, working operator configs on a real machine, none
    /// of which use anything the trailing minors added — so the finding was
    /// downgraded from blocking to informational; see
    /// `check_mission_config_registry_passes_when_user_tier_trails_by_a_minor`
    /// above, which is now the test pinning this path's real behavior. This
    /// test is kept (updated) to confirm the SAME fixture no longer blocks.
    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_is_quiet_for_a_user_tier_copy_trailing_the_current_minor() {
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

        let check = check_mission_config_registry(&tier1_catalog());

        if bin_minor == 0 {
            assert_eq!(
                check.status,
                Status::Pass,
                "a document at the CURRENT schema must draw no drift finding: {}",
                check.message
            );
            return;
        }

        // (#2428) A same-major minor trail is no longer blocking — see
        // `check_mission_config_registry_passes_when_user_tier_trails_by_a_minor`,
        // which now pins this path directly. Still surfaced, just as an
        // informational note on the Pass message rather than an "issue".
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(
            check.message.contains(&format!("declares schema {doc_version}")),
            "{}",
            check.message
        );
        assert!(
            !check.message.contains("issue("),
            "a minor trail must not be counted as an 'N issue(s)' finding: {}",
            check.message
        );
    }


    /// #1917 originally differentiated the remedy text by whether `id` had a
    /// fallback tier — meaningful when a minor trail was a blocking finding
    /// asking the operator to act. #2428 downgraded a same-major minor trail
    /// to informational (see
    /// `check_mission_config_registry_passes_when_user_tier_trails_by_a_minor`),
    /// so there is no remedy left to differentiate; this test is kept
    /// (updated) to confirm both an embedded-fallback id ("review") and a
    /// user-only id ("totally-custom-1917") trailing by the SAME one minor
    /// both simply Pass — neither is treated as an issue anymore, and
    /// neither is told to delete anything.
    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_is_quiet_for_a_minor_trail_regardless_of_fallback() {
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

        let check = check_mission_config_registry(&tier1_catalog());
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(
            !check.message.contains("delete it"),
            "an informational note has nothing to remedy — must never suggest deleting: {}",
            check.message
        );
    }

    /// (#3035) A user-tier copy on a NEWER schema than the binary is refused
    /// by the user-file gate, and that is the ONE message: the registry check
    /// no longer claims the file parses cleanly and a run would complete
    /// green (it is refused at preflight), for a newer minor or a newer
    /// major alike.
    #[serial_test::serial]
    #[test]
    fn a_newer_mission_config_is_refused_once_and_the_registry_check_does_not_contradict_it() {
        let (major, minor) =
            parse_major_minor(darkmux_crew::mission_config::MISSION_CONFIG_SCHEMA).expect("valid constant");
        for ahead in [format!("{major}.{}", minor + 1), format!("{}.0", major + 1)] {
            let guard = CrewRootGuard::new();
            std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
            std::fs::write(
                guard.path().join("mission-configs").join("review.json"),
                format!(r#"{{"id":"review","name":"From a newer darkmux","schema_version":"{ahead}"}}"#),
            )
            .unwrap();

            let registry = check_mission_config_registry(&tier1_catalog());
            let text = format!("{} {}", registry.message, registry.hint.clone().unwrap_or_default());
            for claim in ["swallowed", "complete green", "parses cleanly", "lands in `extras`"] {
                assert!(!text.contains(claim), "{ahead}: the registry check contradicts the refusal ({claim}): {text}");
            }
            assert!(!registry.message.contains(&format!("declares schema {ahead}")), "{ahead}: {}", registry.message);

            let rows = check_user_file_keys();
            let row = rows.iter().find(|r| r.name.contains("review.json")).expect("the gate reports the file");
            assert_eq!(row.status, Status::Fail);
            assert!(row.message.contains("written by a newer darkmux"), "{}", row.message);
            assert!(row.message.contains("Upgrade darkmux. Refused at preflight by: mission launch"), "single period: {}", row.message);
            assert!(!row.message.contains(".."), "{}", row.message);
        }
    }

    /// (#3035) The remedy for a newer file says what actually happens: the
    /// entry points refuse it. It does not claim the file fails to load or
    /// that settings fall back to their defaults.
    #[test]
    fn the_hint_for_a_newer_file_does_not_claim_a_fallback_to_defaults() {
        use darkmux_types::user_files::{FileProblem, Problem, UserFileKind};
        for kind in UserFileKind::ALL {
            let p = FileProblem {
                kind,
                path: "x.json".into(),
                problem: Problem::Newer { file_version: "99.0".into(), known: "1.0".into() },
                note: None,
            };
            let hint = user_file_hint(&p);
            assert!(hint.contains("upgrade darkmux") && hint.contains("refuses it at preflight"), "{kind:?}: {hint}");
            assert!(!hint.contains("falls back") && !hint.contains("fails to load"), "{kind:?}: {hint}");
        }
    }

    /// The informational notes (the trailing-minor drift and the Tier 1
    /// step-kind note) must survive a run in which SOME OTHER config blocks.
    /// They were rendered only inside the `blocking.is_empty()` arm, so the
    /// drift note — whose whole justification is "an operator chasing a
    /// specific field can still find it" — vanished exactly when an operator
    /// is most likely reading this check: when doctor is already warning
    /// about something. One config with an empty `name` is enough to erase
    /// every other config's note.
    #[serial_test::serial]
    #[test]
    fn check_mission_config_registry_keeps_informational_notes_when_another_config_blocks() {
        let guard = CrewRootGuard::new();
        std::fs::create_dir_all(guard.path().join("mission-configs")).unwrap();
        let (major, minor) =
            parse_major_minor(darkmux_crew::mission_config::MISSION_CONFIG_SCHEMA).expect("valid constant");
        // At x.0 there is no one-minor-trailing document to fixture (see
        // `..._is_quiet_for_a_minor_trail_...` for the same guard).
        if minor == 0 {
            return;
        }
        let trailing = format!("{major}.{}", minor - 1);
        // A config that blocks entirely on its own account — empty `name` is
        // an Error-tier finding in `MissionConfig::validate()`.
        std::fs::write(
            guard.path().join("mission-configs").join("nameless.json"),
            r#"{"id":"nameless","name":"","schema_version":"4.0"}"#,
        )
        .unwrap();
        // ... and an unrelated one whose only remark is the informational
        // trailing-minor drift.
        std::fs::write(
            guard.path().join("mission-configs").join("trailer.json"),
            format!(
                r#"{{"id":"trailer","name":"Operator's own","schema_version":"{trailing}"}}"#
            ),
        )
        .unwrap();

        let check = check_mission_config_registry(&tier1_catalog());
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(
            check.message.contains("nameless"),
            "the blocking config is still named: {}",
            check.message
        );
        assert!(
            check.message.contains(&format!("declares schema {trailing}")),
            "the trailing-minor note must survive an unrelated blocking config: {}",
            check.message
        );
        assert!(
            check.message.contains("Tier 1 registry"),
            "the step-kind note must survive an unrelated blocking config: {}",
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

        let check = check_mission_config_registry(&tier1_catalog());
        assert!(
            !check.message.contains("declares schema"),
            "a current-schema user copy must not trip any drift warning: {}",
            check.message
        );
    }

    // ─── (#2149) check_lms_binary ───────────────────────────────────────

    /// (#2149) The direct mechanism behind the reported false FAIL: a
    /// `lms_bin` value that is a PATH (contains `/`, e.g. the real operator
    /// value `~/.lmstudio/bin/lms`) must be checked directly against the
    /// filesystem, not searched for on `PATH` — the OLD code called
    /// `which(&bin)` unconditionally, which returns `None` immediately
    /// whenever the `PATH` env var itself is unset in the calling process,
    /// even for an otherwise-valid absolute path. Clearing `PATH` here
    /// makes that distinction directly observable: the fix must still find
    /// the binary; the old code could not have.
    #[serial_test::serial]
    #[test]
    fn check_lms_binary_checks_a_path_bearing_value_directly_without_needing_path() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("lms");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let prev_lms_bin = std::env::var("DARKMUX_LMS_BIN").ok();
        let prev_path = std::env::var("PATH").ok();
        unsafe {
            std::env::set_var("DARKMUX_LMS_BIN", fake.to_str().unwrap());
            std::env::remove_var("PATH");
        }

        let check = check_lms_binary();

        unsafe {
            match prev_lms_bin {
                Some(v) => std::env::set_var("DARKMUX_LMS_BIN", v),
                None => std::env::remove_var("DARKMUX_LMS_BIN"),
            }
            match prev_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }

        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(
            check.message.contains("at that path"),
            "must resolve via the direct filesystem check, not a PATH search: {}",
            check.message
        );
        assert!(
            check.message.contains("via DARKMUX_LMS_BIN"),
            "must name the resolving tier: {}",
            check.message
        );
    }

    /// (#2149) The remedy must name the durable, config-file mechanism
    /// first — `darkmux config set lms_bin <path>` — and the env var
    /// second, as an override. The ORIGINAL text told the operator to "set
    /// DARKMUX_LMS_BIN to override", i.e. abandon the config file the rest
    /// of the docs prefer as the durable mechanism.
    #[serial_test::serial]
    #[test]
    fn check_lms_binary_fail_names_config_set_as_the_durable_remedy() {
        let prev = std::env::var("DARKMUX_LMS_BIN").ok();
        // No `/` in this name — takes the PATH-search branch, and is
        // certain not to exist on any real PATH.
        unsafe { std::env::set_var("DARKMUX_LMS_BIN", "darkmux-doctor-test-nonexistent-lms-2149") };

        let check = check_lms_binary();

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LMS_BIN", v),
                None => std::env::remove_var("DARKMUX_LMS_BIN"),
            }
        }

        assert_eq!(check.status, Status::Fail, "{}", check.message);
        let hint = check.hint.expect("a FAIL always carries a remedy");
        assert!(
            hint.contains("darkmux config set lms_bin"),
            "the durable mechanism must be named first: {hint}"
        );
        assert!(
            hint.contains("DARKMUX_LMS_BIN overrides"),
            "the env var must be framed as the OVERRIDE, not the primary mechanism: {hint}"
        );
    }

    /// (#2498) The gap this fix closes: `check_lms_binary` must consult
    /// `config.lms_bin`, not just the env tier. Before the #2498 test seam
    /// (`config_access::set_config_for_test`) existed, this exact assertion
    /// was UNWRITABLE — `config_access::config()` was always `EMPTY_CONFIG`
    /// in every test build (#811), so no test anywhere in the workspace
    /// could ever see the config tier win. That is precisely why
    /// reintroducing #2149's regression (swapping the real
    /// `config_access::lms_bin_with_source()` call for a raw
    /// `std::env::var("DARKMUX_LMS_BIN")`) left the suite green: both of
    /// `check_lms_binary`'s existing tests above drive the ENV tier only.
    ///
    /// Red-proved by hand for #2498: with the call in `check_lms_binary`
    /// (line ~4862) swapped back to
    /// `std::env::var("DARKMUX_LMS_BIN").unwrap_or_else(|_| "lms".into())`,
    /// `cargo test -p darkmux-doctor --lib -- lms_binary` goes RED on THIS
    /// test (config.lms_bin is never consulted, so the fake binary this
    /// test points `config.lms_bin` at is never found, and the "via
    /// config.lms_bin" provenance string never appears) while the other two
    /// `check_lms_binary` tests above stay green throughout, exactly as the
    /// issue predicted. Restored afterward.
    #[serial_test::serial]
    #[test]
    fn check_lms_binary_resolves_from_config_tier_and_names_it() {
        use darkmux_types::config::DarkmuxConfig;

        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("lms");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // No env override in play — the value under test must come from
        // the CONFIG tier alone.
        let prev_lms_bin = std::env::var("DARKMUX_LMS_BIN").ok();
        unsafe { std::env::remove_var("DARKMUX_LMS_BIN") };

        let cfg = DarkmuxConfig { lms_bin: Some(fake.to_str().unwrap().to_string()), ..Default::default() };
        let _guard = darkmux_types::config_access::set_config_for_test(cfg);

        let check = check_lms_binary();

        drop(_guard);
        unsafe {
            match prev_lms_bin {
                Some(v) => std::env::set_var("DARKMUX_LMS_BIN", v),
                None => std::env::remove_var("DARKMUX_LMS_BIN"),
            }
        }

        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(
            check.message.contains("at that path"),
            "a config.lms_bin path value resolves via the direct filesystem \
             check, same as the env-tier equivalent: {}",
            check.message
        );
        assert!(
            check.message.contains("via config.lms_bin"),
            "must resolve from AND name the CONFIG tier, not env or the \
             built-in default: {}",
            check.message
        );
    }

    /// (#2533) Doctor and the spawn must agree on a RELATIVE `lms_bin`.
    /// Every `lms` spawn pins its cwd to `/` (#1863), and `std::process`
    /// does `chdir` before `exec`, so a relative program path resolved
    /// against `/` and every dispatch failed with "not found", while this
    /// check resolved the same string against the process cwd and said
    /// PASS. The fix resolves a relative path-bearing value once, in the
    /// accessor both sides read, so this test runs BOTH from one cwd: the
    /// doctor row and a real `lms ps --json` spawn through the fake.
    #[serial_test::serial]
    #[test]
    fn check_lms_binary_and_the_spawn_agree_on_a_relative_lms_bin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        let fake = dir.path().join("bin").join("lms");
        std::fs::write(&fake, "#!/bin/sh\necho '[]'\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let prev_lms_bin = std::env::var("DARKMUX_LMS_BIN").ok();
        let prev_cwd = std::env::current_dir().unwrap();
        unsafe { std::env::set_var("DARKMUX_LMS_BIN", "./bin/lms") };
        std::env::set_current_dir(dir.path()).unwrap();

        let check = check_lms_binary();
        let spawned = lms::list_loaded();

        std::env::set_current_dir(prev_cwd).unwrap();
        unsafe {
            match prev_lms_bin {
                Some(v) => std::env::set_var("DARKMUX_LMS_BIN", v),
                None => std::env::remove_var("DARKMUX_LMS_BIN"),
            }
        }

        assert_eq!(check.status, Status::Pass, "{}", check.message);
        let spawned = spawned.map_err(|e| format!("{e:#}"));
        assert_eq!(
            spawned.as_ref().map(Vec::len),
            Ok(0),
            "doctor said PASS, so the spawn must reach the same binary: {spawned:?}"
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
                    {"id":"gpt-4o","endpoint":"azure"}
                ]}},
                "endpoints":{"azure":{"url":"https://example.azure.com/openai"}}}"#,
        )
        .unwrap();

        let check = check_profile_registry();
        assert_eq!(check.status, Status::Pass, "{}", check.message);
    }

    // ─── a registry that fails to load: the cause, not `init` ───

    /// A file that EXISTS and fails to load gets the whole cause chain and
    /// the cause's own fix; `darkmux init` is never suggested for it.
    #[serial_test::serial]
    #[test]
    fn a_present_registry_that_fails_to_load_shows_its_cause_and_not_init() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        std::fs::write(
            &config_path,
            r#"{"profiles":{"p":{"models":[{"id":"a","n_ctx":1}]}},"internal":{"utility":"util-4b"}}"#,
        )
        .unwrap();
        let check = check_profile_registry();
        assert_eq!(check.status, Status::Fail);
        assert!(check.message.contains("bare string"), "the cause is shown: {}", check.message);
        assert!(check.message.contains("\"n_ctx\": <the window"), "with its fix: {}", check.message);
        let hint = check.hint.unwrap_or_default();
        assert!(!hint.contains("darkmux init"), "init does not fix an existing file: {hint}");
        assert!(hint.contains("user file keys"), "points at the row listing every problem: {hint}");
    }

    /// The inverse: no file at all is the one case that suggests `init`.
    #[serial_test::serial]
    #[test]
    fn an_absent_registry_suggests_init() {
        let (_guard, _absent) = ConfigPathGuard::at_tempfile("profiles.json");
        let check = check_profile_registry();
        assert_eq!(check.status, Status::Fail);
        assert!(check.hint.as_deref().is_some_and(|h| h.contains("darkmux init")), "{:?}", check.hint);
    }

    /// One run names every refused shape in a registry that does not load.
    #[serial_test::serial]
    #[test]
    fn every_refused_shape_in_an_unloadable_registry_is_named_in_one_run() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        std::fs::write(
            &config_path,
            serde_json::json!({
                "profiles": {"p": {"models": [
                    {"id": "m", "n_ctx": 1, "role": "primary"},
                    {"id": "g", "endpoint": {"url": "https://api.example/v1"}}
                ]}},
                "internal": {"utility": "util-4b"}
            })
            .to_string(),
        )
        .unwrap();
        let rows = check_user_file_keys();
        let text: String = rows.iter().map(|r| r.message.as_str()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("internal.utility") && text.contains("bare string"), "{text}");
        assert!(text.contains("profiles.p.models[0].role"), "{text}");
        assert!(text.contains("profiles.p.models[1].endpoint"), "{text}");
    }

    // ─── #85/#91: check_unmanaged_endpoint_credentials tests ───────

    #[serial_test::serial]
    #[test]
    fn check_unmanaged_endpoint_credentials_passes_when_no_endpoint_declared() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        let registry_json = r#"{
            "profiles": {
                "local-profile": {
                    "models": [{"id": "primary-x", "n_ctx": 100000}]
                }
            }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_unmanaged_endpoint_credentials();
        assert_eq!(check.status, Status::Pass);
        assert!(check.message.contains("no profile models declare a remote endpoint"));
    }

    #[serial_test::serial]
    #[test]
    fn check_unmanaged_endpoint_credentials_passes_when_endpoint_has_no_auth() {
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
                        "endpoint": "proxy"
                    }]
                }
            },
            "endpoints": { "proxy": { "url": "http://localhost:8080/v1" } }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_unmanaged_endpoint_credentials();
        assert_eq!(check.status, Status::Pass);
        assert!(check.message.contains("no profile models declare a remote endpoint"));
    }

    #[serial_test::serial]
    #[test]
    fn check_unmanaged_endpoint_credentials_warns_when_keychain_field_missing() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        let registry_json = r#"{
            "profiles": {
                "azure-profile": {
                    "models": [{
                        "id": "gpt-4o",
                        "n_ctx": 128000,
                        "endpoint": "azure"
                    }]
                }
            },
            "endpoints": { "azure": {
                "url": "https://x.cognitiveservices.azure.com/openai/deployments/gpt-4o",
                "auth": { "type": "api-key" }
            } }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_unmanaged_endpoint_credentials();
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("endpoint `azure`"), "{}", check.message);
        // (#1312) The message now names BOTH credential sources (keychain OR
        // key_env), since either satisfies the auth.
        assert!(check.message.contains("no credential source resolved"), "{}", check.message);
        assert!(check.message.contains("endpoint.auth.keychain"), "{}", check.message);
        assert!(check.message.contains("key_env"), "{}", check.message);
    }

    #[serial_test::serial]
    #[test]
    fn check_unmanaged_endpoint_credentials_warns_when_keychain_item_absent() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        let registry_json = r#"{
            "profiles": {
                "azure-profile": {
                    "models": [{
                        "id": "gpt-4o",
                        "n_ctx": 128000,
                        "endpoint": "azure"
                    }]
                }
            },
            "endpoints": { "azure": {
                "url": "https://x.cognitiveservices.azure.com/openai/deployments/gpt-4o",
                "auth": {
                    "type": "api-key",
                    "keychain": "darkmux-doctor-test-definitely-nonexistent-item-xyz123"
                }
            } }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_unmanaged_endpoint_credentials();
        assert_eq!(check.status, Status::Warn);
        assert!(check.message.contains("not found on this machine"));
        let hint = check.hint.as_deref().unwrap_or("");
        assert!(hint.contains("security add-generic-password"));
    }

    #[serial_test::serial]
    #[test]
    fn check_unmanaged_endpoint_credentials_satisfied_by_present_key_env() {
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
                        "endpoint": "azure"
                    }}]
                }}
            }},
            "endpoints": {{ "azure": {{
                "url": "https://x.cognitiveservices.azure.com/openai/deployments/gpt-4o",
                "auth": {{
                    "type": "api-key",
                    "keychain": "darkmux-doctor-test-definitely-nonexistent-item-xyz123",
                    "key_env": "{var}"
                }}
            }} }}
        }}"#
        );
        std::fs::write(&config_path, registry_json).unwrap();

        let check = check_unmanaged_endpoint_credentials();
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

    // ─── #1177: doctor --probe (probe_unmanaged_endpoints) ─────────────

    #[serial_test::serial]
    #[test]
    fn probe_unmanaged_endpoints_reports_nothing_to_probe() {
        let (_guard, config_path) = ConfigPathGuard::at_tempfile("profiles.json");
        let registry_json = r#"{
            "profiles": {
                "local-profile": {
                    "models": [{"id": "primary-x", "n_ctx": 100000}]
                }
            }
        }"#;
        std::fs::write(&config_path, registry_json).unwrap();

        let checks = probe_unmanaged_endpoints();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Pass);
        assert!(checks[0].message.contains("nothing to probe"));
    }

    #[serial_test::serial]
    #[test]
    fn probe_unmanaged_endpoints_probes_once_per_distinct_endpoint_and_reports_cost() {
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
                        "endpoint": "mock"
                    }}]
                }},
                "review-b": {{
                    "models": [{{
                        "id": "gpt-probe",
                        "n_ctx": 128000,
                        "endpoint": "mock"
                    }}]
                }}
            }},
            "endpoints": {{ "mock": {{ "url": "http://127.0.0.1:{port}/v1" }} }}
        }}"#
        );
        std::fs::write(&config_path, registry_json).unwrap();

        let checks = probe_unmanaged_endpoints();
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

// ─── (#2796, #2924) roster identity ──────────────────────────────────────

/// One roster row, reduced to what the roster checks need. A view rather
/// than `darkmux_fleet::MachineEntry` so this crate stays a pure evaluator
/// with no dependency on the fleet crate; the caller computes
/// `address_is_loopback` with `darkmux_fleet::address_host_is_loopback`.
#[derive(Debug, Clone)]
pub struct RosterEntryView {
    pub id: String,
    /// The hardware uid the entry declares, when it has one. `machine add`
    /// records one only for this machine's own entry, so an ordinary peer
    /// entry has none.
    pub machine_uid: Option<String>,
    /// (#3028) The `machine_id` the peer's own card last stated, when this
    /// roster has read it. `@name` addresses resolve against it as well as
    /// the id, so a differing value is a note on the label, not a break.
    pub current_name: Option<String>,
    /// The address as written in the roster.
    pub address: String,
    /// True when `address` reaches only the machine that reads it (a
    /// loopback, unspecified or `localhost` host).
    pub address_is_loopback: bool,
    /// True when the operator added the entry with `--allow-loopback` (a
    /// same-host test fleet): its loopback address is intentional.
    pub loopback_intended: bool,
}

/// (#2916 stage 2) Fleet identity knowledge moved to `darkmux-fleet`, where
/// routing and the daemon can use it too; re-exported so doctor keeps its
/// names.
pub use darkmux_fleet::{FleetIdentityKnowledge, PresenceState};

/// How strong the link is between a roster entry and the machine it is
/// traced to. Only `DeclaredLive` licenses repairs that reuse the entry's
/// address or rename a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Evidence {
    /// The entry's own declared uid, and that machine's current name comes
    /// from a live source (this machine, a presence beat).
    DeclaredLive,
    /// The entry's own declared uid, but the current name is only the last
    /// one flow history saw.
    DeclaredHistory,
    /// No declared uid: flow history links the name to exactly one machine.
    History,
}

/// How one roster entry fails to join the fleet's canonical names.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RosterNameIssue {
    /// The entry names a machine that now goes by `current`.
    /// `name_held_by_other`: another machine currently goes by the entry's id
    /// (a replaced machine), so the id must not be handed back to `current`.
    Renamed { current: String, evidence: Evidence, name_held_by_other: bool },
    /// The entry's declared uid is a machine that already has its own entry
    /// under its current name: this one is a duplicate.
    Duplicate { current: String },
    /// History links the name only to a machine that already has its own
    /// entry, or only to THIS machine (where session names collect) while the
    /// entry points elsewhere. A note: not evidence against the entry.
    WeakTrace { current: String, to_self: bool },
    /// Flow history saw this name on several machines; nothing says which.
    Ambiguous { machines: usize },
    /// No machine this machine can see has gone by this name.
    Unknown,
}

/// Classify one entry against the canonical-name rule (#2924): a roster
/// entry's id must be the machine_id of the machine it describes, because
/// that one name is what flow records, presence beats and (in 4.0)
/// `profile@machine` addresses all join on. `roster_ids` is every id in the
/// roster, so a repair never re-adds over an entry that already exists.
fn roster_name_issue(
    e: &RosterEntryView,
    known: &FleetIdentityKnowledge,
    roster_ids: &std::collections::BTreeSet<&str>,
) -> Option<RosterNameIssue> {
    // A declared uid is the strongest evidence: if that machine is known and
    // goes by another name now, the entry is stale. An unknown uid is a peer
    // this machine has not seen, which is not evidence against the entry.
    if let Some(uid) = &e.machine_uid {
        let current = known.current_name_by_uid.get(uid)?;
        if *current == e.id {
            return None;
        }
        // (#2924 C-b) This machine's name from a per-shell override is not
        // its name for the roster: never ask to rename its entry to it.
        if known.local_name_from_env && known.local_uid.as_deref() == Some(uid.as_str()) {
            return None;
        }
        if roster_ids.contains(current.as_str()) {
            return Some(RosterNameIssue::Duplicate { current: current.clone() });
        }
        let evidence =
            if known.live_uids.contains(uid) { Evidence::DeclaredLive } else { Evidence::DeclaredHistory };
        return Some(RosterNameIssue::Renamed {
            current: current.clone(),
            evidence,
            name_held_by_other: known.is_current_name(&e.id),
        });
    }
    if known.is_current_name(&e.id) {
        return None;
    }
    match known.uids_by_name.get(&e.id).map(|u| u.len()).unwrap_or(0) {
        0 => {}
        1 => {
            let uid = known.uids_by_name[&e.id].iter().next().expect("len 1");
            if let Some(current) = known.current_name_by_uid.get(uid) {
                let to_self = known.local_uid.as_deref() == Some(uid.as_str());
                // (#2924 C-a) Only this machine's own loopback entry is
                // provably this machine; any other entry traced to it may be a
                // session name that a real peer happens to share.
                if (to_self && !e.address_is_loopback) || roster_ids.contains(current.as_str()) {
                    return Some(RosterNameIssue::WeakTrace { current: current.clone(), to_self });
                }
                return Some(RosterNameIssue::Renamed {
                    current: current.clone(),
                    evidence: Evidence::History,
                    name_held_by_other: false,
                });
            }
        }
        n => return Some(RosterNameIssue::Ambiguous { machines: n }),
    }
    Some(RosterNameIssue::Unknown)
}

/// (#3028) The name an entry's machine goes by now, when its own card said so
/// and it differs from the roster id (any case): `Some(that name)`.
fn learned_name(e: &RosterEntryView) -> Option<&str> {
    e.current_name.as_deref().filter(|n| !n.eq_ignore_ascii_case(&e.id))
}

/// (#3028) The warnings and repair hints for entries whose machine went by
/// another name when this roster last read its card. Both names address the
/// one entry, so this is a note on the label: unless another entry already
/// holds the machine's name, when `@name` would be ambiguous and the stale
/// entry is to be removed.
fn relabeled_findings(
    entries: &[&RosterEntryView],
    roster_ids: &std::collections::BTreeSet<&str>,
) -> (Vec<String>, Vec<String>) {
    let mut warns = Vec::new();
    let mut hints = Vec::new();
    for e in entries {
        let (id, current) = (e.id.as_str(), learned_name(e).unwrap_or_default());
        if roster_ids.iter().any(|r| r.eq_ignore_ascii_case(current)) {
            warns.push(format!(
                "`{id}` is the machine that now goes by `{current}`, which already has its own roster entry, \
                 so `@{current}` is ambiguous"
            ));
            hints.push(format!("For `{id}`: it is a second entry for `{current}`; remove it with `darkmux machine remove {id}`."));
            continue;
        }
        let addr = if e.address_is_loopback { "<tailnet-dns-name>" } else { e.address.as_str() };
        warns.push(format!("`{id}` is a machine whose own card now says it goes by `{current}`"));
        hints.push(format!(
            "For `{id}`: addresses using either name work (`host@{id}` and `host@{current}` reach the same \
             machine), so nothing needs changing. To make the roster say the machine's own name: `darkmux \
             machine remove {id}` then `darkmux machine add {current} --address {addr}`."
        ));
    }
    (warns, hints)
}

/// (#2796, #2924) Find roster entries whose name is not the machine_id of
/// the machine they describe, and say which name to use.
///
/// **The canonical name is the machine's own `machine_id`** (the env var
/// `DARKMUX_MACHINE_ID`, else `config.json`'s `machine_id`, else hostname).
/// Flow records carry it, presence beats carry it as `display_name`, the
/// viewer titles cards with it (#2802), and #2916's `profile@machine`
/// addresses will name it. The roster is the one surface keyed by a string
/// the operator typed at `machine add` time, so it is the one that drifts:
/// the live case is a laptop rostered as `laptop` whose machine_id became
/// `MacBook-Pro`.
///
/// **Only evidence warns.** An entry is a warning when its declared uid, or
/// flow history naming exactly one machine, says the machine it means now
/// goes by another name. An entry nothing is known about is reported as a
/// note, never a warning: `machine add` records no uid for a peer, and this
/// machine's history rarely holds a peer's records, so "unknown" is the
/// ordinary state of a peer that is switched off.
///
/// **Repairs never corrupt the fleet.** The address is reused in a rename
/// command only when the entry's own uid is the evidence (history is not:
/// throwaway session names land there). `config set machine_id <id>` is
/// offered only on declared-uid evidence and never when another machine
/// currently goes by `<id>`.
///
/// Surface and suggest only: the operator's roster and config are never
/// rewritten here (#44).
pub fn check_roster_identity(
    entries: &[RosterEntryView],
    known: &FleetIdentityKnowledge,
) -> Check {
    let roster_ids: std::collections::BTreeSet<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    let (relabeled, entries): (Vec<&RosterEntryView>, Vec<&RosterEntryView>) =
        entries.iter().partition(|e| learned_name(e).is_some());
    let (mut warn_msg, mut hint) = relabeled_findings(&relabeled, &roster_ids);
    let mut note_msg: Vec<String> = Vec::new();
    for e in entries.iter().copied() {
        let id = e.id.as_str();
        let addr = if e.address_is_loopback { "<tailnet-dns-name>" } else { e.address.as_str() };
        match roster_name_issue(e, known, &roster_ids) {
            None => {}
            Some(RosterNameIssue::Renamed { current, evidence: Evidence::DeclaredLive, name_held_by_other: false }) => {
                warn_msg.push(format!("`{id}` declares the hardware identity of the machine now called `{current}`"));
                hint.push(format!(
                    "For `{id}`: rename the entry to the machine's own name (`darkmux machine remove {id}` \
                     then `darkmux machine add {current} --address {addr}`), or keep `{id}` by running \
                     `darkmux config set machine_id {id}` on that machine and restarting its daemon \
                     (presence reads the name once, at daemon start)."
                ));
            }
            Some(RosterNameIssue::Renamed { current, evidence: Evidence::DeclaredLive | Evidence::DeclaredHistory, name_held_by_other: true }) => {
                warn_msg.push(format!(
                    "`{id}` declares the hardware identity of the machine now called `{current}`, while \
                     another machine currently goes by `{id}` (a replaced machine?)"
                ));
                hint.push(format!(
                    "For `{id}`: if it means the machine that goes by `{id}` now, re-add it so it drops the \
                     old identity (`darkmux machine remove {id}` then `darkmux machine add {id} --address \
                     {addr}`); if it means `{current}`, rename it (`darkmux machine remove {id}` then \
                     `darkmux machine add {current} --address <its-tailnet-dns-name>`)."
                ));
            }
            Some(RosterNameIssue::Renamed { current, evidence, .. }) => {
                let how = if evidence == Evidence::History {
                    "was last used, in this machine's flow history, by the machine now called"
                } else {
                    "declares the hardware identity of a machine not live now, last seen in flow history as"
                };
                warn_msg.push(format!("`{id}` {how} `{current}`"));
                hint.push(format!(
                    "For `{id}`: flow history is the only source of that name, and a name set for one \
                     session (`DARKMUX_MACHINE_ID`) lands there too, so confirm which machine the entry \
                     means. If it means `{current}`: `darkmux machine remove {id}` then `darkmux machine \
                     add {current} --address <its-tailnet-dns-name>`. If it means another machine, re-add \
                     it under that machine's machine_id (its `darkmux doctor` prints it)."
                ));
            }
            Some(RosterNameIssue::Duplicate { current }) => {
                warn_msg.push(format!(
                    "`{id}` declares the hardware identity of `{current}`, which already has its own roster entry"
                ));
                hint.push(format!(
                    "For `{id}`: it is a second entry for `{current}`; remove it with `darkmux machine remove {id}`."
                ));
            }
            Some(RosterNameIssue::WeakTrace { current, to_self: true }) => {
                note_msg.push(format!(
                    "`{id}` is a name this machine (`{current}`) once used, likely for one session; nothing \
                     links it to another machine"
                ));
            }
            Some(RosterNameIssue::WeakTrace { current, to_self: false }) => {
                note_msg.push(format!(
                    "`{id}` was last used in flow history by `{current}`, which already has its own roster entry"
                ));
            }
            Some(RosterNameIssue::Ambiguous { machines }) => {
                note_msg.push(format!(
                    "`{id}` was used by {machines} different machines in flow history, so nothing says which \
                     this entry means"
                ));
            }
            Some(RosterNameIssue::Unknown) => {
                let window = match known.history_truncated_to {
                    Some(n) => format!("; only the last {n} flow files were read, so an older name is not checked"),
                    None => String::new(),
                };
                note_msg.push(format!(
                    "`{id}` matches no machine_id this machine can see (normal for a peer that is off, or \
                     whose records do not reach here{window})"
                ));
            }
        }
    }
    if known.local_name_from_env {
        note_msg.push(format!(
            "this machine's machine_id `{}` comes from DARKMUX_MACHINE_ID in this shell, so it is not used \
             to judge this machine's own entry",
            known.local_name.as_deref().unwrap_or("?")
        ));
    }
    let presence_note = match known.presence {
        PresenceState::Read => None,
        PresenceState::NotConfigured => Some("presence not read: no Redis configured"),
        PresenceState::Unreadable => Some("presence not read: Redis unreachable"),
    };
    let plural = |n: usize| if n == 1 { "y is" } else { "ies are" };
    let mut message = if warn_msg.is_empty() {
        format!(
            "{} roster entr{} not contradicted by anything this machine knows",
            entries.len(),
            plural(entries.len())
        )
    } else {
        format!(
            "{} of {} roster entr{} not named by the machine's machine_id: {}. Roster, presence and flow \
             records join on that one name.",
            warn_msg.len(),
            entries.len(),
            plural(entries.len()),
            warn_msg.join("; ")
        )
    };
    if !note_msg.is_empty() {
        message.push_str(&format!(" Note: {}.", note_msg.join("; ")));
    }
    if let Some(p) = presence_note {
        message.push_str(&format!(" ({p}.)"));
    }
    if warn_msg.is_empty() && !note_msg.is_empty() {
        hint.push(
            "A note is not a problem by itself. If an entry names a machine you know by another name, run \
             `darkmux doctor` there: its `machine_id` row prints the name the roster should use."
                .into(),
        );
    }
    Check {
        name: "roster identity".into(),
        status: if warn_msg.is_empty() { Status::Pass } else { Status::Warn },
        message,
        hint: if hint.is_empty() { None } else { Some(hint.join(" ")) },
    }
}

/// (#2924) Flag roster entries whose address is loopback.
///
/// A roster entry is read by other machines: the daemon serves the roster at
/// `GET /fleet/roster` to every viewer on the tailnet, and #2916 routes work
/// to it. `127.0.0.1` there means "whichever machine is reading", which is
/// never the machine the entry describes. The old self-registration recipe
/// (`machine add <me> --address 127.0.0.1:8765`) wrote exactly this.
/// `machine add` now refuses it; this check finds the ones already written.
/// An entry added with `--allow-loopback` (a same-host test fleet) is
/// reported as intentional, not warned about.
///
/// When the entry is also renamed (see [`check_roster_identity`]) and its
/// own uid is the evidence, the re-add command uses the machine's current
/// name, so the two rows agree on one command.
pub fn check_roster_addresses(entries: &[RosterEntryView], known: &FleetIdentityKnowledge) -> Check {
    let roster_ids: std::collections::BTreeSet<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    let loopback: Vec<&RosterEntryView> =
        entries.iter().filter(|e| e.address_is_loopback && !e.loopback_intended).collect();
    let intended: Vec<&str> = entries
        .iter()
        .filter(|e| e.address_is_loopback && e.loopback_intended)
        .map(|e| e.id.as_str())
        .collect();
    let intended_note = if intended.is_empty() {
        String::new()
    } else {
        format!(
            " {} added with --allow-loopback (a same-host fleet), so intentional: {}.",
            if intended.len() == 1 { "One entry was" } else { "Some entries were" },
            intended.iter().map(|i| format!("`{i}`")).collect::<Vec<_>>().join(", ")
        )
    };
    if loopback.is_empty() {
        return Check {
            name: "roster addresses".into(),
            status: Status::Pass,
            message: format!(
                "{} roster entr{} no unintended loopback address.{intended_note}",
                entries.len(),
                if entries.len() == 1 { "y has" } else { "ies have" }
            ),
            hint: None,
        };
    }
    let named: Vec<String> = loopback.iter().map(|e| format!("`{}` at {}", e.id, e.address)).collect();
    let fixes: Vec<String> = loopback
        .iter()
        .map(|e| match roster_name_issue(e, known, &roster_ids) {
            // Agree with the identity row: a machine whose own uid says it has
            // a new name is re-added under that name; a duplicate is removed.
            Some(RosterNameIssue::Renamed { current, evidence: Evidence::DeclaredLive, name_held_by_other: false }) => format!(
                "`darkmux machine remove {}` then `darkmux machine add {current} --address <tailnet-dns-name>`",
                e.id
            ),
            Some(RosterNameIssue::Duplicate { .. }) => format!("`darkmux machine remove {}`", e.id),
            Some(RosterNameIssue::Renamed { current, .. }) => format!(
                "`darkmux machine add {id} --address <tailnet-dns-name>` (or, if `roster identity`'s \
                 rename applies, `darkmux machine remove {id}` then `darkmux machine add {current} \
                 --address <tailnet-dns-name>`)",
                id = e.id
            ),
            _ => format!("`darkmux machine add {} --address <tailnet-dns-name>`", e.id),
        })
        .collect();
    Check {
        name: "roster addresses".into(),
        status: Status::Warn,
        message: format!(
            "{} roster entr{} a loopback address: {}. A loopback address reaches whichever machine \
             reads it, never the machine the entry describes, so no peer can use it.{intended_note}",
            loopback.len(),
            if loopback.len() == 1 { "y has" } else { "ies have" },
            named.join(", ")
        ),
        hint: Some(format!(
            "Re-add each with the machine's tailnet DNS name (re-adding keeps the entry's added time): {}. \
             `tailscale status` on that machine prints its DNS name.",
            fixes.join(", ")
        )),
    }
}

#[cfg(test)]
mod roster_identity_tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// `current`: (uid, current name). `seen`: (name, uid) pairs from history.
    fn known(current: &[(&str, &str)], seen: &[(&str, &str)]) -> FleetIdentityKnowledge {
        let mut uids_by_name: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (n, u) in seen {
            uids_by_name.entry(n.to_string()).or_default().insert(u.to_string());
        }
        for (u, n) in current {
            uids_by_name.entry(n.to_string()).or_default().insert(u.to_string());
        }
        FleetIdentityKnowledge {
            current_name_by_uid: current.iter().map(|(u, n)| (u.to_string(), n.to_string())).collect(),
            uids_by_name,
            local_name: None,
            presence: PresenceState::Read,
            ..Default::default()
        }
    }

    fn entry(id: &str, uid: Option<&str>) -> RosterEntryView {
        RosterEntryView {
            id: id.into(),
            machine_uid: uid.map(str::to_string),
            current_name: None,
            address: format!("{id}.tailnet.example:8765"),
            address_is_loopback: false,
            loopback_intended: false,
        }
    }

    fn loopback(mut e: RosterEntryView) -> RosterEntryView {
        e.address = "127.0.0.1:8765".into();
        e.address_is_loopback = true;
        e
    }

    /// (#3028) The peer's own card told this roster the name it goes by now:
    /// the entry's id is a label that differs. A warning that names both and
    /// says addresses using either name work, never a failure.
    #[test]
    fn an_entry_whose_machine_goes_by_a_learned_name_warns_and_says_either_name_works() {
        let mut e = entry("m1-max-32gb-studio", Some("UID-S"));
        e.current_name = Some("studio".into());
        let check = check_roster_identity(&[e], &known(&[], &[]));
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("`m1-max-32gb-studio`") && check.message.contains("`studio`"), "{}", check.message);
        let hint = check.hint.unwrap();
        assert!(hint.contains("either name") && hint.contains("host@m1-max-32gb-studio") && hint.contains("host@studio"), "{hint}");
        assert!(hint.contains("darkmux machine remove m1-max-32gb-studio"), "the optional tidy-up is offered: {hint}");
    }

    /// (#3028) A learned name equal to the id (any case) is no difference; an
    /// entry that learned none is judged as before.
    #[test]
    fn a_learned_name_equal_to_the_id_is_not_a_warning() {
        let mut e = entry("Studio", Some("UID-S"));
        e.current_name = Some("studio".into());
        let check = check_roster_identity(&[e, entry("mini", None)], &known(&[], &[]));
        assert_eq!(check.status, Status::Pass, "{}", check.message);
    }

    /// (#3028) Another entry already goes by that name: the address would be
    /// ambiguous, so it is the duplicate repair, not the either-name note.
    #[test]
    fn a_learned_name_another_entry_already_holds_is_a_duplicate() {
        let mut old = entry("old-studio", Some("UID-S"));
        old.current_name = Some("studio".into());
        let check = check_roster_identity(&[old, entry("studio", Some("UID-S"))], &known(&[], &[]));
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.hint.unwrap().contains("darkmux machine remove old-studio"));
    }

    /// Strong evidence: the entry's own uid now goes by another name. Both
    /// repairs, with the entry's address, and the restart the name needs.
    #[test]
    fn an_entry_whose_declared_uid_now_goes_by_another_name_gets_both_repairs() {
        let mut k = known(&[("UID-A", "MacBook-Pro")], &[]);
        k.live_uids.insert("UID-A".into());
        let check = check_roster_identity(&[entry("laptop", Some("UID-A"))], &k);
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("`laptop`") && check.message.contains("`MacBook-Pro`"), "{}", check.message);
        let hint = check.hint.unwrap();
        assert!(hint.contains("darkmux machine remove laptop"), "{hint}");
        assert!(hint.contains("darkmux machine add MacBook-Pro --address laptop.tailnet.example:8765"), "{hint}");
        assert!(hint.contains("darkmux config set machine_id laptop"), "{hint}");
        assert!(hint.contains("restart"), "the new name reaches presence only after a restart: {hint}");
    }

    /// The live laptop shape: no uid, traced through history to exactly one
    /// machine. Reported, but conservatively.
    #[test]
    fn a_uidless_entry_traced_by_history_to_one_machine_is_reported_without_risky_repairs() {
        let check = check_roster_identity(
            &[entry("laptop", None)],
            &known(&[("UID-A", "MacBook-Pro")], &[("laptop", "UID-A")]),
        );
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("machine now called `MacBook-Pro`"), "{}", check.message);
        let hint = check.hint.unwrap();
        assert!(hint.contains("--address <its-tailnet-dns-name>"), "{hint}");
        assert!(!hint.contains("laptop.tailnet.example"), "history never licenses reusing the address: {hint}");
        assert!(!hint.contains("config set machine_id"), "history never licenses renaming a machine: {hint}");
    }

    /// MF-1 probe: the laptop carried throwaway session names. A roster entry
    /// under one of them, at another machine's address, must not be told to
    /// point that address at the laptop's name or rename the laptop.
    #[test]
    fn a_throwaway_session_name_never_yields_a_corrupting_repair() {
        let check = check_roster_identity(
            &[entry("m5-ultra-256gb", None)],
            &known(
                &[("UID-A", "MacBook-Pro")],
                &[("m5-ultra-256gb", "UID-A"), ("review-scratch", "UID-A"), ("w7-smoke-test", "UID-A")]),
        );
        let hint = check.hint.unwrap_or_default();
        assert!(!hint.contains("m5-ultra-256gb.tailnet.example"), "{hint}");
        assert!(!hint.contains("config set machine_id m5-ultra-256gb"), "{hint}");
    }

    /// Two machines once shared a hostname-derived id: history cannot say
    /// which one an entry under that name means. A note, not a trace.
    #[test]
    fn a_name_two_machines_have_used_is_ambiguous_not_traced() {
        let check = check_roster_identity(
            &[entry("MacBook-Pro-old", None)],
            &known(
                &[("UID-A", "laptop-a"), ("UID-B", "laptop-b")],
                &[("MacBook-Pro-old", "UID-A"), ("MacBook-Pro-old", "UID-B")]),
        );
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("2 different machines"), "{}", check.message);
        assert!(!check.message.contains("now called"), "{}", check.message);
    }

    /// A replaced machine: the entry declares the retired machine's uid, and a
    /// different live machine now goes by the entry's name. Never offer to
    /// give that name to the retired machine too.
    #[test]
    fn a_replaced_machines_entry_never_offers_a_duplicate_machine_id() {
        let mut k = known(&[("UID-OLD", "studio-retired"), ("UID-NEW", "studio")], &[]);
        k.live_uids.extend(["UID-OLD".to_string(), "UID-NEW".to_string()]);
        let check = check_roster_identity(&[entry("studio", Some("UID-OLD"))], &k);
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("another machine currently goes by `studio`"), "{}", check.message);
        let hint = check.hint.unwrap();
        assert!(!hint.contains("config set machine_id"), "{hint}");
        assert!(hint.contains("darkmux machine add studio --address studio.tailnet.example:8765"), "{hint}");
    }

    /// MF-2: the ordinary peer shape (`machine add` records no uid for a
    /// peer), switched off, and absent from this machine's history. Nothing
    /// is known against it, so it is a note, never a warning.
    #[test]
    fn a_uidless_offline_peer_is_a_note_not_a_warning() {
        let check = check_roster_identity(&[entry("studio", None)], &known(&[("UID-A", "MacBook-Pro")], &[]));
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("`studio` matches no machine_id"), "{}", check.message);
        assert!(!check.hint.unwrap_or_default().contains("machine remove"), "no removal advice without evidence");
    }

    /// MF-2: the row says when presence was not read.
    #[test]
    fn the_row_says_when_presence_was_not_read() {
        let mut k = known(&[], &[]);
        k.presence = PresenceState::Unreadable;
        let check = check_roster_identity(&[entry("studio", None)], &k);
        assert!(check.message.contains("Redis unreachable"), "{}", check.message);
        k.presence = PresenceState::NotConfigured;
        let check = check_roster_identity(&[entry("studio", None)], &k);
        assert!(check.message.contains("no Redis configured"), "{}", check.message);
        k.presence = PresenceState::Read;
        let check = check_roster_identity(&[entry("studio", None)], &k);
        assert!(!check.message.contains("presence not read"), "{}", check.message);
    }

    /// C-3: this machine's own entry passes even when its uid is unreadable.
    #[test]
    fn this_machines_entry_passes_without_a_readable_uid() {
        let mut k = known(&[], &[]);
        k.local_name = Some("studio".into());
        let check = check_roster_identity(&[entry("studio", None)], &k);
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(!check.message.contains("matches no machine_id"), "{}", check.message);
    }

    #[test]
    fn a_rename_repair_never_suggests_a_loopback_address() {
        let mut k = known(&[("UID-A", "MacBook-Pro")], &[]);
        k.live_uids.insert("UID-A".into());
        let check = check_roster_identity(&[loopback(entry("laptop", Some("UID-A")))], &k);
        let hint = check.hint.unwrap();
        assert!(hint.contains("--address <tailnet-dns-name>"), "{hint}");
        assert!(!hint.contains("127.0.0.1"), "{hint}");
    }

    #[test]
    fn an_entry_named_by_its_machines_current_machine_id_passes() {
        let check = check_roster_identity(
            &[entry("MacBook-Pro", None), entry("studio", Some("UID-B"))],
            &known(&[("UID-A", "MacBook-Pro"), ("UID-B", "studio")], &[]),
        );
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(!check.message.contains("Note:"), "{}", check.message);
    }

    /// A declared uid nobody has seen: no evidence against the entry.
    #[test]
    fn a_declared_uid_nobody_has_seen_is_not_a_warning() {
        let check = check_roster_identity(&[entry("studio", Some("UID-B"))], &known(&[("UID-A", "MacBook-Pro")], &[]));
        assert_eq!(check.status, Status::Pass, "{}", check.message);
    }

    #[test]
    fn an_empty_roster_passes_without_claiming_anything() {
        let check = check_roster_identity(&[], &known(&[], &[]));
        assert_eq!(check.status, Status::Pass);
    }

    // ── #2924 re-review C-a, C-b, C-e ──

    fn with_local(mut k: FleetIdentityKnowledge, uid: &str, name: &str) -> FleetIdentityKnowledge {
        k.local_uid = Some(uid.into());
        k.local_name = Some(name.into());
        k.live_uids.insert(uid.into());
        k.current_name_by_uid.insert(uid.into(), name.into());
        k
    }

    /// C-a: a throwaway session name this machine once used, on an entry at
    /// another machine's address, is a note: nothing links it to another
    /// machine, and a real peer with that name must not warn whenever it is
    /// off.
    #[test]
    fn a_history_trace_to_this_machines_own_uid_is_a_note() {
        let k = with_local(known(&[], &[("review-scratch", "UID-SELF")]), "UID-SELF", "MacBook-Pro");
        let check = check_roster_identity(&[entry("review-scratch", None)], &k);
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("`review-scratch`"), "{}", check.message);
        assert!(!check.hint.unwrap_or_default().contains("machine add MacBook-Pro"));
    }

    /// ...but this machine's own stale entry at a loopback address IS this
    /// machine (loopback reaches only here), so it still warns.
    #[test]
    fn a_loopback_entry_traced_to_this_machine_still_warns() {
        let k = with_local(known(&[], &[("laptop", "UID-SELF")]), "UID-SELF", "MacBook-Pro");
        let check = check_roster_identity(&[loopback(entry("laptop", None))], &k);
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("now called `MacBook-Pro`"), "{}", check.message);
    }

    /// C-a: the traced machine's current name already has its own entry.
    /// Following "add MacBook-Pro" would overwrite that correct entry.
    #[test]
    fn a_history_trace_to_an_already_rostered_name_is_a_note() {
        let k = known(&[("UID-A", "MacBook-Pro")], &[("laptop", "UID-A")]);
        let check = check_roster_identity(&[entry("laptop", None), entry("MacBook-Pro", None)], &k);
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("already has its own roster entry"), "{}", check.message);
        assert!(!check.hint.unwrap_or_default().contains("machine add MacBook-Pro"));
    }

    /// With declared-uid proof, a second entry for an already-rostered
    /// machine is a duplicate: the repair removes it, never re-adds over the
    /// correct entry.
    #[test]
    fn a_declared_uid_duplicate_of_a_rostered_machine_is_removed_not_re_added() {
        let mut k = known(&[("UID-A", "MacBook-Pro")], &[]);
        k.live_uids.insert("UID-A".into());
        let check = check_roster_identity(&[entry("laptop", Some("UID-A")), entry("MacBook-Pro", None)], &k);
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        let hint = check.hint.unwrap();
        assert!(hint.contains("darkmux machine remove laptop"), "{hint}");
        assert!(!hint.contains("machine add MacBook-Pro"), "{hint}");
        assert!(!hint.contains("config set machine_id"), "{hint}");
    }

    /// C-b: the current name for a declared uid came from HISTORY (the
    /// machine is not live): same conservative repair as a history trace.
    #[test]
    fn a_declared_uid_whose_current_name_is_only_historical_gets_the_conservative_repair() {
        let check = check_roster_identity(
            &[entry("laptop", Some("UID-A"))],
            &known(&[("UID-A", "MacBook-Pro")], &[]),
        );
        let hint = check.hint.unwrap();
        assert!(!hint.contains("laptop.tailnet.example"), "{hint}");
        assert!(!hint.contains("config set machine_id"), "{hint}");
        assert!(hint.contains("--address <its-tailnet-dns-name>"), "{hint}");
    }

    /// C-b: a `DARKMUX_MACHINE_ID` session override is not this machine's
    /// name for the roster; the row names that provenance.
    #[test]
    fn the_row_names_a_session_machine_id_override() {
        let mut k = known(&[], &[]);
        k.local_name = Some("review-scratch".into());
        k.local_name_from_env = true;
        let check = check_roster_identity(&[entry("studio", None)], &k);
        assert!(check.message.contains("DARKMUX_MACHINE_ID"), "{}", check.message);
    }

    /// C-b: under a session override, this machine's correct entry (its own
    /// declared uid) is never told to take the session name.
    #[test]
    fn a_session_override_never_renames_this_machines_entry() {
        let mut k = with_local(known(&[], &[]), "UID-SELF", "review-scratch");
        k.local_name_from_env = true;
        let check = check_roster_identity(&[entry("MacBook-Pro", Some("UID-SELF"))], &k);
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(!check.hint.unwrap_or_default().contains("review-scratch --address"));
    }

    /// C-e: when older flow files were not read, an unknown name's note says
    /// so.
    #[test]
    fn an_unknown_names_note_mentions_a_truncated_history_window() {
        let mut k = known(&[], &[]);
        k.history_truncated_to = Some(120);
        let check = check_roster_identity(&[entry("studio", None)], &k);
        assert!(check.message.contains("last 120 flow files"), "{}", check.message);
        k.history_truncated_to = None;
        let check = check_roster_identity(&[entry("studio", None)], &k);
        assert!(!check.message.contains("flow files"), "{}", check.message);
    }

    // ── roster addresses (#2924) ──

    #[test]
    fn a_loopback_roster_address_is_reported_with_the_re_add_command() {
        let check = check_roster_addresses(&[loopback(entry("studio", None)), entry("laptop", None)], &known(&[], &[]));
        assert_eq!(check.status, Status::Warn, "{}", check.message);
        assert!(check.message.contains("`studio` at 127.0.0.1:8765"), "{}", check.message);
        assert!(!check.message.contains("`laptop`"), "{}", check.message);
        let hint = check.hint.unwrap();
        assert!(hint.contains("darkmux machine add studio --address <tailnet-dns-name>"), "{hint}");
    }

    /// C-1: a loopback entry that is ALSO renamed (by its own uid) gets the
    /// re-add under the machine's current name, agreeing with the identity row.
    #[test]
    fn a_renamed_loopback_entrys_re_add_uses_the_current_name() {
        let mut k = known(&[("UID-A", "MacBook-Pro")], &[]);
        k.live_uids.insert("UID-A".into());
        let e = loopback(entry("laptop", Some("UID-A")));
        let check = check_roster_addresses(std::slice::from_ref(&e), &k);
        let hint = check.hint.unwrap();
        assert!(hint.contains("darkmux machine remove laptop"), "{hint}");
        assert!(hint.contains("darkmux machine add MacBook-Pro --address <tailnet-dns-name>"), "{hint}");
        assert!(!hint.contains("machine add laptop"), "{hint}");
    }

    /// C-1, history-only rename: the address hint must not contradict the
    /// identity row either. It keeps the entry's name but names the rename
    /// as the alternative.
    #[test]
    fn a_history_renamed_loopback_entrys_hint_names_the_rename_too() {
        let k = known(&[("UID-A", "MacBook-Pro")], &[("laptop", "UID-A")]);
        let check = check_roster_addresses(&[loopback(entry("laptop", None))], &k);
        let hint = check.hint.unwrap();
        assert!(hint.contains("darkmux machine add laptop --address <tailnet-dns-name>"), "{hint}");
        assert!(hint.contains("darkmux machine add MacBook-Pro --address <tailnet-dns-name>"), "{hint}");
    }

    /// C-6: a same-host test fleet entry added with `--allow-loopback` is
    /// intentional: reported as such, not warned about.
    #[test]
    fn an_intended_loopback_entry_is_not_a_warning() {
        let mut e = loopback(entry("peer-a", None));
        e.loopback_intended = true;
        let check = check_roster_addresses(&[e], &known(&[], &[]));
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("--allow-loopback"), "{}", check.message);
    }

    #[test]
    fn non_loopback_roster_addresses_pass() {
        let check = check_roster_addresses(&[entry("studio", None), entry("laptop", None)], &known(&[], &[]));
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("2 roster"), "{}", check.message);
    }
}

#[cfg(test)]
mod machine_uid_row_tests {
    use super::*;

    #[test]
    fn an_unreadable_uid_warns_with_its_consequence_and_a_readable_one_passes() {
        let warn = machine_uid_check(None);
        assert_eq!(warn.status, Status::Warn);
        assert!(warn.message.contains("refused here") && warn.message.contains("unreadable"), "{}", warn.message);
        let pass = machine_uid_check(Some("A1B2-SECRET-UID"));
        assert_eq!(pass.status, Status::Pass);
        assert!(!format!("{pass:?}{warn:?}").contains("SECRET-UID"), "the uid is never printed");
    }
}

#[cfg(test)]
mod machine_id_provenance_tests {
    //! (#2924) The `machine_id` row must name the tier the value actually came
    //! from. It used to print `(from hostname)` whenever the env var was unset,
    //! so a value written to `config.json` (the Studio's
    //! `m1-max-32gb-studio`, hostname `Kains-Mac-Studio.local`) was labeled as
    //! the hostname.
    use super::*;
    use darkmux_types::config::DarkmuxConfig;

    /// Run `check_machine_id_resolution` with the env tier pinned to `env`
    /// and the config tier to `cfg_id`, restoring the env afterward.
    fn run_with(env: Option<&str>, cfg_id: Option<&str>) -> Check {
        let prev = std::env::var("DARKMUX_MACHINE_ID").ok();
        unsafe {
            match env {
                Some(v) => std::env::set_var("DARKMUX_MACHINE_ID", v),
                None => std::env::remove_var("DARKMUX_MACHINE_ID"),
            }
        }
        let cfg = DarkmuxConfig { machine_id: cfg_id.map(str::to_string), ..Default::default() };
        let guard = darkmux_types::config_access::set_config_for_test(cfg);
        let check = check_machine_id_resolution();
        drop(guard);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_MACHINE_ID", v),
                None => std::env::remove_var("DARKMUX_MACHINE_ID"),
            }
        }
        check
    }

    /// The live defect: config set, env unset. The label must say config.
    #[serial_test::serial]
    #[test]
    fn a_config_json_machine_id_is_labeled_as_config_not_hostname() {
        let check = run_with(None, Some("from-config-id"));
        assert_eq!(check.status, Status::Pass, "{}", check.message);
        assert!(check.message.contains("`from-config-id`"), "{}", check.message);
        assert!(check.message.contains("config.json"), "{}", check.message);
        assert!(!check.message.contains("hostname"), "{}", check.message);
        assert!(check.hint.is_none(), "a named id needs no nudge: {:?}", check.hint);
    }

    /// Env outranks config, and says so.
    #[serial_test::serial]
    #[test]
    fn the_env_tier_wins_over_config_and_is_labeled_env() {
        let check = run_with(Some("from-env-id"), Some("from-config-id"));
        assert!(check.message.contains("`from-env-id`"), "{}", check.message);
        assert!(check.message.contains("DARKMUX_MACHINE_ID"), "{}", check.message);
        assert!(!check.message.contains("config.json"), "{}", check.message);
    }

    /// Neither tier set: the value is the hostname, and the hint names the
    /// visible config field as the way to set a logical name.
    #[serial_test::serial]
    #[test]
    fn with_neither_tier_set_the_value_is_labeled_hostname() {
        let check = run_with(None, None);
        if check.status == Status::Warn {
            // A sandbox without `hostname(1)`: nothing resolves at all.
            return;
        }
        assert!(check.message.contains("(from hostname)"), "{}", check.message);
        assert!(
            check.hint.as_deref().is_some_and(|h| h.contains("darkmux config set machine_id")),
            "{:?}",
            check.hint
        );
    }
}

#[cfg(test)]
mod user_file_key_tests {
    use super::*;
    use darkmux_types::test_isolation::IsolatedState;

    fn rows_named(checks: &[Check]) -> Vec<&Check> {
        checks.iter().filter(|c| c.name.starts_with(USER_FILE_KEYS_CHECK_NAME)).collect()
    }

    #[test]
    fn a_clean_set_is_one_pass_row() {
        let rows = user_file_key_rows(&[], &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, Status::Pass);
    }

    /// A `config.json` with an unknown key and one that is not JSON each get
    /// a Fail row naming the file and what is wrong, with the entry points
    /// that refuse it.
    #[test]
    fn a_bad_config_json_is_a_fail_row_naming_the_key_and_the_closest() {
        let dir = tempfile::tempdir().unwrap();
        let typo = dir.path().join("typo.json");
        std::fs::write(&typo, r#"{"redis": {"hots": "127.0.0.1"}}"#).unwrap();
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, r#"{"redis": "#).unwrap();
        let problems: Vec<_> = [&typo, &broken]
            .iter()
            .filter_map(|p| darkmux_types::user_files::config_json_problem_at(p))
            .collect();
        let rows = user_file_key_rows(&problems, &[]);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows.iter().all(|r| r.status == Status::Fail));
        assert_eq!(rows[0].name, "user file keys: typo.json");
        assert!(rows[0].message.contains(&typo.display().to_string()), "the message names the full path");
        assert!(rows[0].message.contains("unknown key `redis.hots`: did you mean `redis.host`?"), "{}", rows[0].message);
        assert!(
            rows[0].message.contains("Refused at preflight by: dispatch, mission launch, lab run, fleet work submission"),
            "{}",
            rows[0].message
        );
        assert!(rows[1].message.contains("not valid JSON"), "{}", rows[1].message);
    }

    /// (#3057) darkbook's real config: every retired key `init` wrote, at
    /// the value it wrote. Doctor shows each as a Warn row (never a Fail),
    /// and no Pass row claims every key is known.
    #[test]
    fn retired_keys_at_their_old_defaults_are_warn_rows_not_fail_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"runtime": {"log_level": "info"},
                "remote": {"max_tokens_per_step": null, "step_budget_policy": "warn", "concurrent_cap": 1},
                "machine_rollup": {"enabled": false, "period_seconds": 60}}"#,
        )
        .unwrap();
        let check = darkmux_types::user_files::config_check_at(&path);
        assert_eq!(check.refusal, None);
        let rows = user_file_key_rows(&[], &check.warnings);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(rows.iter().all(|r| r.status == Status::Warn), "{rows:?}");
        assert!(rows.iter().any(|r| r.message.contains("remote in config.json is ignored: removed in 5.0")), "{rows:?}");
        assert!(rows.iter().all(|r| r.hint.as_deref().is_some_and(|h| h.contains("safe to remove"))), "{rows:?}");
    }

    /// (#3057) A cap that was set is still a Fail row.
    #[test]
    fn a_retired_key_holding_a_set_value_is_still_a_fail_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"remote": {"max_tokens_per_step": 50000}}"#).unwrap();
        let problem = darkmux_types::user_files::config_json_problem_at(&path).expect("a set cap refuses");
        let rows = user_file_key_rows(&[problem], &[]);
        assert_eq!(rows[0].status, Status::Fail);
    }

    /// A `config.json` value of the wrong type (one of which drops every
    /// setting to its default on load) is a Fail row with the preflight's
    /// message.
    #[test]
    fn a_mistyped_config_value_is_a_fail_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"redis": {"enabled": true, "port": "x"}}"#).unwrap();
        let problem = darkmux_types::user_files::config_json_problem_at(&path).unwrap();
        let rows = user_file_key_rows(&[problem], &[]);
        assert_eq!(rows[0].status, Status::Fail);
        assert!(rows[0].message.contains("`redis.port` must be an integer from 0 to 65535, got \"x\""), "{}", rows[0].message);
    }

    /// (review C2) A file name cannot forge a doctor line either.
    #[test]
    fn a_file_name_cannot_forge_a_doctor_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a\n  \u{202e}ok.json");
        std::fs::write(&path, r#"{"rediss": 1}"#).unwrap();
        let problem = darkmux_types::user_files::config_json_problem_at(&path).unwrap();
        let row = &user_file_key_rows(&[problem], &[])[0];
        for text in [&row.name, &row.message] {
            assert!(!text.contains('\n') && !text.contains('\u{202e}'), "{text:?}");
        }
    }

    /// A profile model's inline `endpoint` object is a Fail row naming the
    /// exact rewrite and that every dispatching entry point refuses to start.
    #[test]
    fn an_inline_endpoint_object_is_a_fail_row_naming_the_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        std::fs::write(
            &path,
            r#"{"profiles":{"p":{"models":[{"id":"gpt","endpoint":{"url":"https://api.example/v1"}}]}}}"#,
        )
        .unwrap();
        let problem = darkmux_profiles::profiles::user_file_problem(&path).unwrap();
        let row = &user_file_key_rows(&[problem], &[])[0];
        assert_eq!(row.status, Status::Fail);
        assert!(row.message.contains("profiles.p.models[0].endpoint"), "{}", row.message);
        assert!(row.message.contains("endpoints.\"api.example\""), "{}", row.message);
        assert!(row.message.contains("Refused at preflight by"), "{}", row.message);
        assert!(row.hint.as_deref().is_some_and(|h| h.contains("rewrite")), "{:?}", row.hint);
    }

    /// Doctor runs to completion against a user file with a syntax error and
    /// one with an unknown key, and reports each as its own Fail row.
    #[serial_test::serial]
    #[test]
    fn doctor_runs_to_completion_against_broken_user_files() {
        let state = IsolatedState::new();
        std::fs::create_dir_all(state.join("roles")).unwrap();
        std::fs::write(state.join("roles/broken.json"), "{\"id\": ").unwrap();
        std::fs::create_dir_all(state.join("crews")).unwrap();
        std::fs::write(state.join("crews/c.json"), r#"{"id": "c", "description": "d", "membrs": []}"#).unwrap();
        let report = run();
        let rows = rows_named(&report.checks);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows.iter().all(|r| r.status == Status::Fail));
        let text: String = rows.iter().map(|r| r.message.as_str()).collect();
        assert!(text.contains("broken.json: not valid JSON"), "{text}");
        assert!(text.contains("unknown key `membrs`: did you mean `members`?"), "{text}");
        assert!(text.contains("Nothing that starts work reads this file"), "a crew manifest refuses nothing: {text}");
    }
}

#[cfg(test)]
mod user_file_hint_tests {
    use super::*;

    fn row_for(text: &str) -> Check {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, text).unwrap();
        let problem = darkmux_types::user_files::config_json_problem_at(&path).unwrap();
        user_file_key_rows(&[problem], &[]).remove(0)
    }

    /// (review minor) Each kind of problem says its own consequence: an
    /// unknown key does nothing, but a wrong value or broken JSON makes the
    /// whole file fail to load; "loading ignores it" is only true of the
    /// first.
    #[test]
    fn each_problem_kind_names_its_own_consequence() {
        let unknown = row_for(r#"{"redis": {"hots": "h"}}"#).hint.unwrap();
        assert!(unknown.contains("does nothing"), "{unknown}");
        for text in [r#"{"redis": {"port": "x"}}"#, r#"{"redis": "#] {
            let hint = row_for(text).hint.unwrap();
            assert!(!hint.contains("ignores") && !hint.contains("does nothing"), "{text}: {hint}");
            assert!(hint.contains("every setting falls back to its default"), "{text}: {hint}");
        }
    }
}
