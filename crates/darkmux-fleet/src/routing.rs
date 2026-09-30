//! Fleet dispatch routing — local vs a `profile@machine` address, and direct
//! work submission to the owning machine (#2916).
//!
//! A profile address (`--profile host@studio`, #2916 stage 2) sends the
//! dispatch STRAIGHT to that machine's work-submission listener
//! (`submission.rs`), which resolves `host` against its own registry, checks
//! the fleet token and the connecting node before it runs anything, and
//! answers at once with a refusal or (with `--wait`, the default) the
//! finished dispatch's result. An address naming this machine runs here. The
//! `--machine` flag that used to name the target is gone (4.0): the profile
//! names its machine.
//! Until 4.0 the dispatch was published to the Redis work queue
//! (`darkmux:work`) and waited on through the flow stream; the queue is
//! retired because it could not say who wrote an entry, and any runner
//! claimed any job.

use crate::WorkJob;
use darkmux_types::session_id::SessionId;
use anyhow::{anyhow, Result};
use std::time::{SystemTime, UNIX_EPOCH};

/// Build a [`WorkJob`] from what the sending side has on hand. Centralizes
/// the stamped defaults (`published_at_unix_ms` = now).
#[allow(clippy::too_many_arguments)]
pub fn build_work_job(
    target_machine: String,
    role_id: String,
    message: String,
    session_id: SessionId,
    profile: Option<String>,
    workdir: Option<String>,
    image: Option<String>,
    timeout_seconds: u32,
    published_by_machine: Option<String>,
) -> WorkJob {
    let published_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_else(|_| {
            // (#906) A pre-epoch / badly-NTP-skewed clock makes 0 the stamp.
            // Surface it rather than silently mislabeling the job.
            eprintln!("darkmux: system clock is before the Unix epoch — stamping published_at_unix_ms=0");
            0
        });
    WorkJob {
        target_machine,
        role_id,
        message,
        session_id,
        profile,
        workdir,
        image,
        timeout_seconds,
        published_at_unix_ms,
        published_by_machine,
        single_shot: None,
        boundary: None,
        mode: crate::SubmissionMode::Run,
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Dispatch routing (#463 cycle-break)
//
// The local-vs-remote routing decision moved here from `crew::dispatch` so
// `crew` no longer depends on `fleet`. `crew::dispatch::dispatch` is purely
// local; `dispatch_routed` is the front door for user-facing dispatch
// callers. The receiving machine runs a submitted job through
// `runner::execute_job`, which never re-routes.
// ─────────────────────────────────────────────────────────────────────────

use crate::job::{Boundary, SingleShotJob};
use darkmux_crew::dispatch::{self, DispatchOpts, DispatchResult, RoutingDecision};

/// Route a dispatch local-vs-remote, then run it locally via the raw
/// `crew::dispatch::dispatch` primitive. `phase_cli`'s QA-gate dispatch is
/// the one caller: it is reached only from an already-wave-protected
/// `StepKind` whose `seat()` resolved residency for the whole wave, so it must
/// not independently Exclusive-reconcile (see `dispatch_reconciled`'s own
/// doc). Thin wrapper over [`dispatch_routed_via`].
pub fn dispatch_routed(opts: DispatchOpts) -> Result<DispatchResult> {
    dispatch_routed_via(opts, dispatch::dispatch)
}

/// (#2916 stage 2) Split a `profile@machine` address in `opts.profile_name`
/// into the owning machine's own profile name and `opts.machine`. A plain
/// profile name is left alone. The machine is resolved at dispatch time
/// against the roster, by its canonical `machine_id` (case-insensitive).
/// Every caller of [`dispatch_routed_via`] gets the same reading of an
/// address (contract 1).
pub fn apply_profile_address(opts: &mut DispatchOpts) -> Result<()> {
    let Some(raw) = opts.profile_name.as_deref() else { return Ok(()) };
    if !darkmux_types::profile_address::ProfileAddress::is_address(raw) {
        return Ok(());
    }
    let address = darkmux_types::profile_address::ProfileAddress::parse(raw)
        .map_err(|e| anyhow!("darkmux dispatch: {e}"))?;
    let Some(machine) = address.machine.clone() else {
        return Err(anyhow!("darkmux dispatch: profile address `{raw}` names no machine"));
    };
    if let Some(existing) = opts.machine.as_deref() {
        if !crate::job::same_machine(existing, &machine) {
            return Err(anyhow!(
                "darkmux dispatch: profile address `{raw}` names {machine}, but this dispatch was \
                 already addressed to {existing}; a dispatch runs on one machine"
            ));
        }
    }
    opts.profile_name = Some(address.profile);
    opts.machine = Some(machine);
    Ok(())
}

/// The address a routed dispatch was written as, for messages.
fn address_label(opts: &DispatchOpts, target: &str) -> String {
    match opts.profile_name.as_deref() {
        Some(p) => format!("{p}@{target}"),
        None => format!("@{target}"),
    }
}

/// Route a dispatch local-vs-remote, then run it. When a `profile@machine`
/// address names another machine ([`apply_profile_address`]), the dispatch
/// is SUBMITTED to that machine's fleet listener
/// (`submission::submit_work`): its roster host on the fleet's submission
/// port, with the fleet token. The receiver answers at once with a
/// refusal ("studio does not accept work from macbook-pro"), or runs it and
/// (with `--wait`) replies with the result. Otherwise the dispatch falls
/// through to `local_dispatch`, a caller-injected LOCAL execution primitive
/// (#1509): the CLI verb passes `dispatch_as_crew_of_one`, radio passes its
/// single-shot primitive, `phase_cli` the raw one via [`dispatch_routed`].
pub fn dispatch_routed_via(
    opts: DispatchOpts,
    local_dispatch: impl FnOnce(DispatchOpts) -> Result<DispatchResult>,
) -> Result<DispatchResult> {
    dispatch_routed_single_shot(opts, None, None, local_dispatch)
}

/// [`dispatch_routed_via`] for a caller whose exchange is ONE tool-less
/// single-shot under the radio persona (the answering seat). `single_shot`
/// travels with a job submitted to a peer, which then builds the persona
/// itself and runs the exchange through the single-shot primitive
/// ([`SingleShotJob`]); a dispatch that stays here ignores it, because the
/// caller's `local_dispatch` is already that primitive. `boundary` travels the
/// same way and is enforced by the receiver against the profile it resolves
/// ([`Boundary`]); a dispatch that stays here has nothing to enforce it on.
pub fn dispatch_routed_single_shot(
    mut opts: DispatchOpts,
    single_shot: Option<SingleShotJob>,
    boundary: Option<Boundary>,
    local_dispatch: impl FnOnce(DispatchOpts) -> Result<DispatchResult>,
) -> Result<DispatchResult> {
    apply_profile_address(&mut opts)?;
    if let Some(target) = opts.machine.clone() {
        let local = darkmux_flow::resolve_machine_id();
        match dispatch::routing_decision(Some(target.as_str()), local.as_deref()) {
            RoutingDecision::Local {
                matches_was_explicit: true,
            } => {
                eprintln!(
                    "darkmux dispatch: `{}` names this machine; running it here.",
                    address_label(&opts, &target)
                );
                // The address is spent: the local path resolves the bare
                // profile name against this machine's own registry.
                opts.machine = None;
            }
            RoutingDecision::Remote {
                target,
                local_unknown: true,
            } => {
                // (#2584, same class as #2561/#2580) `WorkJob` carries no
                // `resume_from` field, and the receiver runs the job with
                // `resume_from: None` — so a submitted dispatch would start
                // fresh on the other machine and exit 0 under a name the
                // operator chose because it looked like a resume. Refuse
                // HERE, before the route record is emitted or anything is
                // sent. A checkpoint is a directory on THIS machine; the
                // other machine has no access to it.
                if opts.resume_from.is_some() {
                    return Err(anyhow!(
                        "darkmux dispatch: --resume-from is not supported with \
                         `{}` (role `{}`): a submitted dispatch runs on the \
                         OTHER machine, which has no access to this machine's checkpoint — it \
                         would start fresh and report success regardless. darkmux never silently \
                         starts a dispatch fresh under a name that looked like a resume: resume \
                         on THIS machine (name a profile here, without `@{target}`) or start \
                         this role fresh there on purpose (drop --resume-from).",
                        address_label(&opts, &target),
                        opts.role_id
                    ));
                }
                // PR-C.3 review MEDIUM (Wave-E.7): local machine_id is
                // unresolvable. The receiver still checks who is calling by
                // the network, not by this name; say what happened.
                eprintln!(
                    "{}",
                    darkmux_types::style::warn(&format!(
                        "darkmux dispatch: WARNING — this machine's machine_id is unresolvable. \
                         `{}` is submitted to {target} regardless. \
                         Set DARKMUX_MACHINE_ID (or `darkmux config set machine_id`) so the \
                         local-vs-remote decision is deterministic.",
                        address_label(&opts, &target)
                    ))
                );
                // #290 — the pinned route record, so the audit trail and
                // topology UI see the operator-pinned routing decision.
                dispatch::emit_route_record(&opts, Some(&target));
                return dispatch_via_submission(opts, &target, single_shot, boundary);
            }
            RoutingDecision::Remote {
                target,
                local_unknown: false,
            } => {
                // (#2584) Same refusal as the `local_unknown: true` arm
                // above — see its comment for the mechanism.
                if opts.resume_from.is_some() {
                    return Err(anyhow!(
                        "darkmux dispatch: --resume-from is not supported with \
                         `{}` (role `{}`): a submitted dispatch runs on the \
                         OTHER machine, which has no access to this machine's checkpoint — it \
                         would start fresh and report success regardless. darkmux never silently \
                         starts a dispatch fresh under a name that looked like a resume: resume \
                         on THIS machine (name a profile here, without `@{target}`) or start \
                         this role fresh there on purpose (drop --resume-from).",
                        address_label(&opts, &target),
                        opts.role_id
                    ));
                }
                dispatch::emit_route_record(&opts, Some(&target));
                return dispatch_via_submission(opts, &target, single_shot, boundary);
            }
            RoutingDecision::Local {
                matches_was_explicit: false,
            } => {
                // Unreachable in this branch (we matched Some(target) above)
                // — but the enum's total shape covers it.
            }
        }
    }

    // Local fall-through — no address naming another machine means run here.
    local_dispatch(opts)
}

/// Submit a dispatch to `target`'s fleet listener instead of running it
/// here (#2916). With `opts.wait` (the default) this returns when the other
/// machine's dispatch finishes, with its exit code and output; with
/// `--no-wait` it returns once the job is accepted. A refusal is an `Err`
/// carrying the receiver's own reason.
///
/// What crosses: role, message, session id, `--profile`, `--workdir` (a
/// path on the RECEIVER, and only if its allow-list entry grants
/// `workspace`), `--image`, `timeout_seconds`, and, for the radio answering
/// seat, `single_shot` (persona parameters and the token budget, never a
/// system prompt). What does not: `--timeout`'s inactivity override,
/// `--max-completion-tokens` (outside `single_shot`), compaction flags,
/// `--json` (the receiver's human output is returned as stdout).
fn dispatch_via_submission(
    opts: DispatchOpts,
    target: &str,
    single_shot: Option<SingleShotJob>,
    boundary: Option<Boundary>,
) -> Result<DispatchResult> {
    let session_id = opts.session.clone();
    let mut job = build_work_job(
        target.to_string(),
        opts.role_id.clone(),
        opts.message.clone(),
        session_id.clone(),
        opts.profile_name.clone(),
        opts.workdir.as_ref().map(|p| p.display().to_string()),
        opts.image.clone(),
        opts.timeout_seconds,
        darkmux_flow::resolve_machine_id(),
    );
    job.single_shot = single_shot;
    job.boundary = boundary;
    eprintln!(
        "darkmux dispatch: submitting to {target} (run={}{})…",
        session_id.wire(),
        if opts.wait { ", waiting for the result" } else { "" }
    );
    let reply = crate::submission::submit_work(job, opts.wait)?;
    Ok(reply_to_dispatch_result(reply, &session_id, target))
}

/// Translate a reply `submit_work` returned (completed, accepted or queued)
/// into the `DispatchResult` the CLI prints.
pub(crate) fn reply_to_dispatch_result(
    reply: crate::SubmissionReply,
    session_id: &SessionId,
    target: &str,
) -> DispatchResult {
    use crate::ReplyStatus;
    let session_id = reply.session_id.clone().unwrap_or_else(|| session_id.clone());
    let run = session_id.wire();
    let follow = format!("Follow it in the viewer, or on {target} with `darkmux run list --kind dispatch`.");
    let stdout = match reply.status {
        // (#2916 stage 2) Queued without `--wait`: the receiver's own words,
        // verbatim (control characters removed), and how to follow it.
        ReplyStatus::Queued => format!(
            "queued on {target}; not waiting (run={run}): {}. {follow}\n",
            crate::sanitize_remote_text(reply.reason.as_deref().unwrap_or("its seat is busy"))
        ),
        ReplyStatus::Accepted => format!("submitted to {target}; not waiting (run={run}). {follow}\n"),
        // A status this darkmux does not know is never read as success: it
        // fails, whatever exit code the reply carried.
        ReplyStatus::Unknown => {
            return DispatchResult {
                exit_code: 1,
                stdout: String::new(),
                stderr: format!(
                    "{target} answered with a status this darkmux does not recognize (is it on a newer darkmux?)\n"
                ),
                session_id,
                execution: None,
                out_dir: None,
                trajectory: None,
            }
        }
        // (#2916 review C1) Remote output never reaches the terminal raw.
        ReplyStatus::Completed | ReplyStatus::Error | ReplyStatus::Refused | ReplyStatus::Checked => {
            return DispatchResult {
                exit_code: reply.exit_code.unwrap_or(1),
                stdout: crate::sanitize_remote_text(&reply.stdout.unwrap_or_default()),
                stderr: crate::sanitize_remote_text(&reply.stderr.unwrap_or_default()),
                session_id,
                execution: None,
                out_dir: None,
                trajectory: None,
            }
        }
    };
    // The run's bookkeeping lands on the receiving machine.
    DispatchResult { exit_code: 0, stdout, stderr: String::new(), session_id, execution: None, out_dir: None, trajectory: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::time::Duration;

    /// All distinct values so a field-swap (X landing where Y belongs) fails.
    fn sample_job() -> WorkJob {
        build_work_job(
            "studio".to_string(),              // target_machine
            "coder".to_string(),               // role_id
            "do the thing".to_string(),        // message
            crate::test_session("sess-42"),    // session_id
            Some("coder-studio".to_string()),  // profile
            Some("/work/repo".to_string()),    // workdir
            Some("rust:slim".to_string()),     // image
            900,                               // timeout_seconds
            Some("laptop".to_string()),        // published_by_machine
        )
    }

    #[test]
    fn build_work_job_passes_fields_through_without_swap() {
        let j = sample_job();
        assert_eq!(j.target_machine, "studio");
        assert_eq!(j.role_id, "coder");
        assert_eq!(j.message, "do the thing");
        assert_eq!(j.session_id, crate::test_session("sess-42"));
        assert_eq!(j.profile.as_deref(), Some("coder-studio"));
        assert_eq!(j.workdir.as_deref(), Some("/work/repo"));
        assert_eq!(j.image.as_deref(), Some("rust:slim"));
        assert_eq!(j.timeout_seconds, 900);
        assert_eq!(j.published_by_machine.as_deref(), Some("laptop"));
        assert!(j.published_at_unix_ms > 0);
        j.validate().expect("a built job is valid");
    }

    #[test]
    fn a_completed_reply_carries_the_remote_exit_code_and_output() {
        let reply = crate::SubmissionReply {
            session_id: Some(crate::test_session("s-remote")),
            exit_code: Some(42),
            stdout: Some("out\x1b[2J".into()),
            stderr: Some("err".into()),
            ..crate::SubmissionReply::of(crate::ReplyStatus::Completed)
        };
        let local = crate::test_session("s-local");
        let r = reply_to_dispatch_result(reply, &local, "studio");
        assert_eq!((r.exit_code, r.stdout.as_str(), r.stderr.as_str()), (42, "out[2J", "err"), "the ESC byte is stripped");
        assert_eq!(r.session_id, crate::test_session("s-remote"));
        let accepted = crate::SubmissionReply::of(crate::ReplyStatus::Accepted);
        let r = reply_to_dispatch_result(accepted, &local, "studio");
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains(&format!("submitted to studio; not waiting (run={})", local.wire())), "{}", r.stdout);
        assert!(r.stdout.contains("darkmux run list --kind dispatch"), "it names the command that lists the run: {}", r.stdout);
        assert!(!r.stdout.contains("session"), "a routed job shows the run, never the session: {}", r.stdout);
        let queued = crate::SubmissionReply::of(crate::ReplyStatus::Queued);
        let r = reply_to_dispatch_result(queued, &local, "studio");
        assert!(r.stdout.contains(&format!("queued on studio; not waiting (run={})", local.wire())), "{}", r.stdout);
        assert!(!r.stdout.contains("session"), "{}", r.stdout);
    }

    /// A status this darkmux does not know fails the dispatch, even when the
    /// reply carries an exit code of 0: an unrecognized status is never read
    /// as a success.
    #[test]
    fn an_unrecognized_reply_status_is_never_read_as_success() {
        let reply: crate::SubmissionReply =
            serde_json::from_str(r#"{"status": "paused", "exit_code": 0, "stdout": "done"}"#).unwrap();
        assert_eq!(reply.status, crate::ReplyStatus::Unknown);
        let r = reply_to_dispatch_result(reply, &crate::test_session("s-local"), "studio");
        assert_eq!(r.exit_code, 1);
        assert!(r.stderr.contains("does not recognize"), "{}", r.stderr);
        assert!(r.stdout.is_empty(), "the unknown reply's output is not passed through: {}", r.stdout);
    }

    // (#1509) `dispatch_routed_via`'s local-dispatch injection seam. No
    // `opts.machine` means the local fall-through runs — never touches
    // the network, so this is a fast, hermetic unit test even though
    // `dispatch_routed_via` is the same function a live `profile@machine` dispatch
    // uses.

    fn local_opts(role_id: &str) -> DispatchOpts {
        DispatchOpts {
            // (#2914) Work never runs on the utility model.
            allow_utility_model: false,
            remote_origin: None,
            live_channel: true,
            brief_refs: Vec::new(),
            workspace_read_only: false,
            record_context: None,
            resume_from: None,
            host_out: None,
            max_turns_override: None,
            timeout_override_seconds: None, // (#2480)
            role_id: role_id.to_string(),
            message: "hi".to_string(),
            session: crate::test_session("n"),
            timeout_seconds: 60,
            skip_preflight: false,
            json: true,
            workdir: None,
            phase_id: None,
            machine: None,
            wait: true,
            compaction: darkmux_crew::dispatch::CompactionDispatchArgs::default(),
            profile_name: None,
            config_path: None,
            force_container: false,
            max_completion_tokens: None,
            image: None,
            model_base_url_override: None,
            step_id: None,
            system_prompt_override: None,
        }
    }

    #[test]
    fn dispatch_routed_via_runs_the_injected_local_dispatch_on_the_local_fallthrough() {
        let called = std::cell::RefCell::new(false);
        let result = dispatch_routed_via(local_opts("coder"), |opts| {
            *called.borrow_mut() = true;
            assert_eq!(opts.role_id, "coder", "the SAME opts must reach the injected closure");
            Ok(DispatchResult {
                exit_code: 0,
                stdout: "injected stdout".to_string(),
                stderr: String::new(),
                session_id: crate::test_session("sess-injected"),
                execution: None,
                out_dir: None,
                trajectory: None,
            })
        })
        .unwrap();

        assert!(*called.borrow(), "the local fall-through must call the injected closure");
        assert_eq!(result.stdout, "injected stdout");
        assert_eq!(result.session_id, crate::test_session("sess-injected"));
    }

    #[test]
    fn dispatch_routed_via_propagates_the_injected_closures_error() {
        let err = dispatch_routed_via(local_opts("coder"), |_opts| {
            Err(anyhow!("injected failure"))
        })
        .unwrap_err();
        assert!(err.to_string().contains("injected failure"), "{err}");
    }

    // ─── #2584: `--resume-from` routed to another machine via `profile@machine`
    //     must refuse BEFORE anything is sent ──────────────────────────
    //
    // `dispatch_via_submission` sends a `WorkJob` that carries no
    // `resume_from` field at all, and the receiver runs it with
    // `resume_from: None`. A dispatch with `--profile <p>@<peer> --resume-from
    // <dir>` would start FRESH on the other machine and exit 0 — the
    // promise-break #2561/#2580 closed on the other two routes.
    //
    // ORDER, not just an `Err`: a real loopback listener stands in for the
    // peer's fleet listener (the roster points `peer-b` at it, on the fleet
    // port), and it must NEVER accept a connection. The positive control
    // below proves the same setup DOES dial it when there is no resume, so
    // the refusal test cannot pass vacuously.

    /// A bare TCP listener that records every accepted connection on `tx`
    /// and closes it at once (so a sender that dials it fails fast instead
    /// of waiting on a read).
    fn spawn_connection_counting_peer() -> (u16, std::sync::mpsc::Receiver<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(_stream) = stream else { continue };
                let _ = tx.send(());
            }
        });
        std::thread::sleep(Duration::from_millis(50));
        (port, rx)
    }

    /// Env for a `host@peer-b` dispatch whose submission would dial
    /// the counting peer: a roster naming `peer-b` at 127.0.0.1, the fleet
    /// port pointed at the peer, a fleet token, a private flows dir.
    /// Restores everything on drop.
    struct PeerEnv {
        _roster_dir: tempfile::TempDir,
        flows_dir: tempfile::TempDir,
        prev: Vec<(&'static str, Option<String>)>,
    }

    impl PeerEnv {
        fn new(port: u16) -> Self {
            let keys = [
                "DARKMUX_MACHINE_ID",
                "DARKMUX_FLEET_FILE",
                "DARKMUX_FLEET_LISTENER_PORT",
                "DARKMUX_SERVE_TOKEN",
                "DARKMUX_FLOWS_DIR",
            ];
            let prev = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
            let roster_dir = tempfile::TempDir::new().unwrap();
            let roster = roster_dir.path().join("fleet.json");
            std::fs::write(
                &roster,
                r#"{"version":"2","machines":{"peer-b":{"id":"peer-b","address":"127.0.0.1","added_unix_ms":1}}}"#,
            )
            .unwrap();
            let flows_dir = tempfile::TempDir::new().unwrap();
            unsafe {
                // Local differs from the target, so `routing_decision`
                // resolves `Remote { local_unknown: false }`. The
                // `local_unknown: true` arm runs in its own process in
                // `tests/resume_from_local_unknown_arm.rs` (the machine-id
                // `OnceLock` is already set in this shared binary).
                std::env::set_var("DARKMUX_MACHINE_ID", "local-a");
                std::env::set_var("DARKMUX_FLEET_FILE", &roster);
                std::env::set_var("DARKMUX_FLEET_LISTENER_PORT", port.to_string());
                std::env::set_var("DARKMUX_SERVE_TOKEN", "test-fleet-token");
                // `local_sink_dir()` re-resolves this LIVE per write, so an
                // empty dir afterwards proves no record was emitted.
                std::env::set_var("DARKMUX_FLOWS_DIR", flows_dir.path());
            }
            Self { _roster_dir: roster_dir, flows_dir, prev }
        }
    }

    impl Drop for PeerEnv {
        fn drop(&mut self) {
            unsafe {
                for (k, v) in &self.prev {
                    match v {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
    }

    #[test]
    #[serial]
    fn dispatch_routed_via_refuses_resume_from_before_anything_is_sent() {
        let (port, rx) = spawn_connection_counting_peer();
        let env = PeerEnv::new(port);

        let mut opts = local_opts("pr-reviewer");
        opts.profile_name = Some("host@peer-b".to_string());
        opts.resume_from = Some(std::path::PathBuf::from("/tmp/darkmux-2584-checkpoint"));

        let err = dispatch_routed_via(opts, |_opts| {
            panic!("local_dispatch must never be invoked for a host@peer-b dispatch");
        })
        .expect_err("--resume-from with a remote profile address must refuse, not submit");
        let msg = format!("{err:#}");

        assert!(msg.contains("`host@peer-b`"), "must name the address it was sent to: {msg}");
        assert!(
            msg.contains(
                "darkmux never silently \
                 starts a dispatch fresh under a name that looked like a resume"
            ),
            "must carry the same promise the other two guards state: {msg}"
        );
        match rx.recv_timeout(Duration::from_millis(300)) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Ok(()) => panic!("dispatch_via_submission must never run for a refused resume, but the peer was dialed"),
            Err(e) => panic!("unexpected mock channel state: {e:?}"),
        }
        let files: Vec<_> = std::fs::read_dir(env.flows_dir.path())
            .map(|rd| rd.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(files.is_empty(), "no flow record may be written before the refusal: {files:?}");
    }

    /// (#2916 re-review C6) A receiver's echoed session id is used only
    /// when well-formed: one carrying a terminal escape is dropped and the
    /// sender's own id is kept.
    #[test]
    #[serial]
    fn a_malformed_echoed_session_id_is_not_used() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                // Read the WHOLE request (headers, then Content-Length bytes)
                // before answering, or the client may still be writing.
                let mut got = Vec::new();
                let mut b = [0u8; 65536];
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                loop {
                    let n = s.read(&mut b).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&b[..n]);
                    let text = String::from_utf8_lossy(&got).to_string();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len = text[..h]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if got.len() >= h + 4 + len {
                            break;
                        }
                    }
                }
                let body = r#"{"status":"completed","session_id":"x\u001b]0;pwned\u0007","exit_code":0,"stdout":"ok"}"#;
                let _ = s.write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                        .as_bytes(),
                );
            }
        });
        let _env = PeerEnv::new(port);
        crate::submission::test_sender_provider::set(Box::new(crate::identity::StaticIdentityProvider {
            local: crate::identity::test_node("nLOCAL", "local-a", "100.64.0.1"),
            peers: vec![crate::identity::test_node("nPEERB", "peer-b", "127.0.0.1")],
            down: None,
        }));
        let mut opts = local_opts("pr-reviewer");
        opts.machine = Some("peer-b".to_string());
        let r = dispatch_routed_via(opts, |_| panic!("never local")).unwrap();
        // The hostile echo is not a session: the dispatch keeps its own.
        assert_eq!(r.session_id, local_opts("pr-reviewer").session, "{:?}", r.session_id);
    }

    /// A peer that reads one whole request, sends its body on the channel,
    /// and answers `reply` (a newline-delimited body) with HTTP 200.
    fn spawn_scripted_peer(reply: &'static str) -> (u16, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut got = Vec::new();
                let mut b = [0u8; 65536];
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let body = loop {
                    let n = s.read(&mut b).unwrap_or(0);
                    if n == 0 {
                        break String::new();
                    }
                    got.extend_from_slice(&b[..n]);
                    let text = String::from_utf8_lossy(&got).to_string();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len = text[..h]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if got.len() >= h + 4 + len {
                            break text[h + 4..].to_string();
                        }
                    }
                };
                let _ = tx.send(body);
                let _ = s.write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len())
                        .as_bytes(),
                );
            }
        });
        (port, rx)
    }

    fn peer_b_is_verified() {
        crate::submission::test_sender_provider::set(Box::new(crate::identity::StaticIdentityProvider {
            local: crate::identity::test_node("nLOCAL", "local-a", "100.64.0.1"),
            peers: vec![crate::identity::test_node("nPEERB", "peer-b", "127.0.0.1")],
            down: None,
        }));
    }

    /// (#2916 stage 2) `host@peer-b` is submitted to peer-b, and what
    /// crosses is the OWNER's profile name, never the address.
    #[test]
    #[serial]
    fn an_address_is_submitted_to_its_machine_with_the_bare_profile() {
        let (port, rx) = spawn_scripted_peer("{\"status\":\"completed\",\"exit_code\":0,\"stdout\":\"done\"}\n");
        let _env = PeerEnv::new(port);
        peer_b_is_verified();
        let mut opts = local_opts("radio-host");
        opts.profile_name = Some("host@Peer-B".to_string());
        let r = dispatch_routed_via(opts, |_| panic!("an address naming another machine never runs here")).unwrap();
        assert_eq!((r.exit_code, r.stdout.as_str()), (0, "done"));
        let sent: serde_json::Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
        assert_eq!(sent["job"]["profile"], "host", "the owner's own profile name crosses: {sent}");
        assert_eq!(sent["job"]["target_machine"], "Peer-B");
        assert_eq!(sent["schema"], crate::WORK_JOB_SCHEMA_VERSION);
    }

    /// The answering seat's single-shot mode rides the job to the peer as
    /// parameters; an ordinary dispatch to the same peer writes no such field.
    #[test]
    #[serial]
    fn a_single_shot_dispatch_submits_its_mode_to_the_peer() {
        let single_shot = crate::SingleShotJob {
            humor: 61,
            surface: darkmux_flow::payload::RadioSurface::Cli,
            max_completion_tokens: 9_000,
        };
        for (mode, expected) in [
            (Some(single_shot), serde_json::json!({"humor": 61, "surface": "cli", "max_completion_tokens": 9000})),
            (None, serde_json::Value::Null),
        ] {
            let (port, rx) = spawn_scripted_peer("{\"status\":\"completed\",\"exit_code\":0,\"stdout\":\"done\"}\n");
            let _env = PeerEnv::new(port);
            peer_b_is_verified();
            let mut opts = local_opts("radio-host");
            opts.profile_name = Some("host@Peer-B".to_string());
            dispatch_routed_single_shot(opts, mode, None, |_| panic!("never local")).unwrap();
            let sent: serde_json::Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
            assert_eq!(sent["job"]["single_shot"], expected, "{sent}");
        }
    }

    /// The boundary crosses with the job, and a refusal at it reaches the
    /// sender as a typed code: the sender's caller reads the code, never the
    /// sentence.
    #[test]
    #[serial]
    fn a_boundary_crosses_and_a_boundary_refusal_reaches_the_caller_typed() {
        let refused = format!("{}\n", serde_json::to_string(&crate::Refusal::BoundaryUnmanaged { profile: "cloud".into() }.reply("peer-b")).unwrap());
        let (port, rx) = spawn_scripted_peer(Box::leak(refused.into_boxed_str()));
        let _env = PeerEnv::new(port);
        peer_b_is_verified();
        let mut opts = local_opts("radio-host");
        opts.profile_name = Some("cloud@Peer-B".to_string());
        let err = dispatch_routed_single_shot(opts, None, Some(Boundary::ManagedOnly), |_| panic!("never local")).unwrap_err();
        let sent: serde_json::Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
        assert_eq!(sent["job"]["boundary"], "managed_only", "{sent}");
        assert!(sent["job"].get("mode").is_none(), "a run writes no mode: {sent}");
        assert_eq!(err.downcast_ref::<crate::SubmitRefused>().map(|r| r.code), Some(crate::RefusalCode::Boundary), "{err:#}");
    }

    /// `check_route` submits a check: no prompt text, the route's role,
    /// profile and boundary, and the receiver's answer comes back typed.
    #[test]
    #[serial]
    fn check_route_submits_a_check_and_reads_the_answer() {
        let checked = "{\"status\":\"checked\",\"profile\":\"deep\",\"check\":{\"endpoint\":\"managed\",\"seat\":\"would_queue\"}}\n";
        let (port, rx) = spawn_scripted_peer(checked);
        let _env = PeerEnv::new(port);
        peer_b_is_verified();
        let outcome = crate::check_route("deep@Peer-B", "radio-host", Some(Boundary::ManagedOnly));
        assert_eq!(
            outcome,
            crate::CheckOutcome::Routable {
                profile: "deep".into(),
                report: crate::CheckReport { endpoint: crate::EndpointClass::Managed, seat: crate::SeatOutlook::WouldQueue },
            }
        );
        let sent: serde_json::Value = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(5)).unwrap()).unwrap();
        assert_eq!(sent["job"]["mode"], "check", "{sent}");
        assert_eq!(sent["job"]["message"], "", "a check sends no prompt text: {sent}");
        assert_eq!(
            (sent["job"]["role_id"].as_str(), sent["job"]["profile"].as_str(), sent["job"]["boundary"].as_str()),
            (Some("radio-host"), Some("deep"), Some("managed_only")),
            "{sent}"
        );
        assert_eq!(sent["schema"], crate::WORK_JOB_SCHEMA_VERSION);
    }

    /// A check with nothing to check (no machine in the address, no peer to
    /// reach) is unanswered with the reason, and sends nothing.
    #[test]
    #[serial]
    fn check_route_without_a_route_is_unanswered() {
        for address in ["deep", "a@b@c"] {
            assert!(
                matches!(crate::check_route(address, "radio-host", None), crate::CheckOutcome::Unanswered { .. }),
                "{address}"
            );
        }
    }

    /// (#2916 stage 2) An address naming THIS machine runs here, on the
    /// bare profile name, and nothing is sent.
    #[test]
    #[serial]
    fn an_address_naming_this_machine_runs_here_on_the_bare_profile() {
        let (port, rx) = spawn_connection_counting_peer();
        let _env = PeerEnv::new(port);
        let mut opts = local_opts("coder");
        opts.profile_name = Some("host@LOCAL-A".to_string());
        let mut seen = None;
        dispatch_routed_via(opts, |o| {
            seen = Some((o.profile_name.clone(), o.machine.clone()));
            Ok(DispatchResult { exit_code: 0, stdout: String::new(), stderr: String::new(), session_id: crate::test_session("s"), execution: None, out_dir: None, trajectory: None })
        })
        .unwrap();
        assert_eq!(seen, Some((Some("host".to_string()), None)));
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "nothing may be sent for a local address");
    }

    /// (#2916 stage 2) A malformed address, or one that conflicts with a
    /// machine the caller already named, is refused before anything runs
    /// or is sent.
    #[test]
    #[serial]
    fn a_bad_address_is_refused_before_anything_is_sent() {
        let (port, rx) = spawn_connection_counting_peer();
        let env = PeerEnv::new(port);
        for (profile, needle) in [("host@peer.b", "contains '.'"), ("@peer-b", "names no profile")] {
            let mut opts = local_opts("coder");
            opts.profile_name = Some(profile.to_string());
            let err = dispatch_routed_via(opts, |_| panic!("never local")).unwrap_err();
            assert!(format!("{err:#}").contains(needle), "{profile}: {err:#}");
        }
        let mut opts = local_opts("coder");
        opts.profile_name = Some("host@peer-b".to_string());
        opts.machine = Some("peer-c".to_string());
        let err = dispatch_routed_via(opts, |_| panic!("never local")).unwrap_err();
        assert!(format!("{err:#}").contains("already addressed to peer-c"), "{err:#}");
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "nothing may be sent");
        // Refused before the route record, too: nothing was dispatched.
        let files: Vec<_> = std::fs::read_dir(env.flows_dir.path())
            .map(|rd| rd.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(files.is_empty(), "no flow record may be written for a refused address: {files:?}");
    }

    /// (#2916 stage 2 review C1) A connection that never opens sent
    /// nothing, and the sender says so instead of "may still be running".
    #[test]
    #[serial]
    fn a_refused_connection_says_nothing_was_sent() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let _env = PeerEnv::new(port);
        peer_b_is_verified();
        let mut opts = local_opts("radio-host");
        opts.profile_name = Some("host@peer-b".to_string());
        let msg = format!("{:#}", dispatch_routed_via(opts, |_| panic!("never local")).unwrap_err());
        assert!(msg.contains("nothing was sent") && !msg.contains("may still be running"), "{msg}");
    }

    /// (#2916 stage 2) Queued without `--wait`: the answer is the
    /// receiver's own words, and the dispatch is not an error.
    #[test]
    #[serial]
    fn a_queued_answer_without_wait_is_reported_verbatim() {
        let (port, _rx) = spawn_scripted_peer(
            "{\"status\":\"queued\",\"session_id\":\"m-1.solo.relay.local-a.m-1_2Eadhoc_2Ecoder_2En\",\"reason\":\"peer-b is busy (x is running on big); the job is queued and runs when its seat frees\"}\n",
        );
        let _env = PeerEnv::new(port);
        peer_b_is_verified();
        let mut opts = local_opts("radio-host");
        opts.profile_name = Some("host@peer-b".to_string());
        opts.wait = false;
        let r = dispatch_routed_via(opts, |_| panic!("never local")).unwrap();
        assert_eq!(r.exit_code, 0);
        assert!(r.stdout.contains("queued on peer-b") && r.stdout.contains("x is running on big"), "{}", r.stdout);
        assert_eq!(r.session_id, darkmux_types::session_id::SessionId::relay(crate::test_session("n"), "local-a"));
    }

    /// (#2916 stage 2) Waited on: the queued lines come first, then the
    /// result, which is what the dispatch returns.
    #[test]
    #[serial]
    fn a_waited_queued_job_returns_its_final_result() {
        let (port, _rx) = spawn_scripted_peer(
            "{\"status\":\"queued\",\"reason\":\"busy\"}\n{\"status\":\"completed\",\"exit_code\":3,\"stdout\":\"late\"}\n",
        );
        let _env = PeerEnv::new(port);
        peer_b_is_verified();
        let mut opts = local_opts("radio-host");
        opts.profile_name = Some("host@peer-b".to_string());
        let r = dispatch_routed_via(opts, |_| panic!("never local")).unwrap();
        assert_eq!((r.exit_code, r.stdout.as_str()), (3, "late"));
    }

    /// Positive control for the test above: the same setup WITHOUT a resume
    /// does dial the peer (and fails loudly, since the peer answers
    /// nothing), so "never dialed" above means refused, not misconfigured.
    #[test]
    #[serial]
    fn without_a_resume_the_same_setup_does_submit_to_the_peer() {
        let (port, rx) = spawn_connection_counting_peer();
        let _env = PeerEnv::new(port);
        // The sender verifies the node at 127.0.0.1 before sending (#2916
        // review C1): say it is `peer-b`'s node.
        crate::submission::test_sender_provider::set(Box::new(crate::identity::StaticIdentityProvider {
            local: crate::identity::test_node("nLOCAL", "local-a", "100.64.0.1"),
            peers: vec![crate::identity::test_node("nPEERB", "peer-b", "127.0.0.1")],
            down: None,
        }));
        let mut opts = local_opts("pr-reviewer");
        opts.machine = Some("peer-b".to_string());
        let err = dispatch_routed_via(opts, |_opts| panic!("never local")).unwrap_err();
        rx.recv_timeout(Duration::from_secs(5)).expect("the submission must have dialed the peer");
        let msg = format!("{err:#}");
        assert!(msg.contains("no answer from http://127.0.0.1:"), "{msg}");
        // (#2916 stage 2 review C1) The request reached the peer, so the
        // sender cannot know whether it runs: it says so, with the session.
        assert!(msg.contains("may still be running on peer-b") && msg.contains(".relay.local-a."), "{msg}");
    }

    // ─── #2584 conformance: every call site of `dispatch_via_submission` must be
    //     guarded against `resume_from` ─────────────────────────────────
    //
    // The test above pins the `local_unknown: false` arm (the ordinary
    // cross-machine case) end-to-end. The sibling `local_unknown: true` arm
    // — local machine_id unresolvable — turns out ALSO to be reachable from
    // a real dispatch, in `resume_from_local_unknown_arm.rs` (a separate
    // integration-test binary in this crate's `tests/`): forcing it needs
    // only `DARKMUX_MACHINE_ID` unset and `PATH` emptied so the `hostname`
    // shell-out fails to spawn, and it must run in its OWN process because
    // `darkmux_flow::resolve_machine_id()` caches that shell-out's result in
    // a process-wide `OnceLock` — once any test in a shared binary resolves
    // it to `Some(<hostname>)`, every later test in that same process is
    // stuck with that cached value regardless of what env it mutates
    // afterward. An earlier version of this comment called that arm
    // "not producible portably" — that was true only of the EXISTING shared
    // unit-test binary, not of the arm itself; a fresh binary is enough.
    //
    // So this check is not the only thing standing between the operator and
    // the untested arm any more. It still earns its place as a SECOND,
    // structural layer: it does not run `dispatch_routed_via` at all, it
    // reads `routing.rs`'s own source and proves EACH match arm's own call
    // site carries the guard IN THAT ARM — a future edit that restores only
    // one arm's refusal, or drops a guard during a refactor of this match,
    // fails a fast test without needing a live dispatch to reach the arm
    // that lost it. **The per-arm scoping is load-bearing, not incidental,**
    // and has needed fixing TWICE, in opposite directions:
    // - The ORIGINAL version searched the whole ENCLOSING FUNCTION for a
    //   preceding guard rather than the call's own match-arm scope, which
    //   meant the FIRST arm's guard (textually earlier in the source)
    //   satisfied the search for the SECOND arm's call too — deleting only
    //   the second arm's guard left this check GREEN (too WIDE).
    // - The round-1 fix scoped the search to `nearest_enclosing_block`
    //   instead, which is itself wrong both ways: a brace-less arm has no
    //   block of its own, so the nearest enclosing block widens back out to
    //   the whole `match` (still too WIDE, same failure, different shape);
    //   and a call nested one level deeper than the guard WITHIN the same
    //   arm (an ordinary `if`, a closure body) shrinks the nearest
    //   enclosing block down PAST that arm's own guard, failing the check
    //   on correct code (too NARROW).
    // The round-2 fix (`guard_search_scope` / `match_arm_span_within`)
    // computes the arm's actual span structurally — from the enclosing
    // match's own depth-0 `=>` boundaries — rather than proxying it with
    // "nearest brace block". See `guard_search_scope`'s doc for the
    // mechanism and `resume_from_guard_precedes`'s doc for why `body` must
    // arrive pre-scoped either way.
    // - Round-2's own walk still broke, in the WIDE-only direction: it
    //   returned at the FIRST block it recognized as a match body, and a
    //   NESTED match between an arm's guard and its call also has its own
    //   depth-0 arrows — so the walk stopped at the inner match's arm
    //   instead of continuing out to the real, outer one, cutting the
    //   outer guard out of the search (a false ACCUSATION against correct
    //   code, never a false certification — see `guard_search_scope`'s
    //   doc for why the direction is structurally one-way). The round-3
    //   fix keeps walking through every containing block and remembers
    //   the OUTERMOST one recognized as a match, instead of stopping at
    //   the first.
    //
    // **Which layer is authoritative, for a maintainer facing one red and
    // one green:** the runtime test
    // (`dispatch_routed_via_refuses_resume_from_before_anything_is_sent`
    // below, plus its `local_unknown: true` sibling in
    // `resume_from_local_unknown_arm.rs`) is GROUND TRUTH — it runs the
    // real function against a real fake peer and observes whether a
    // connection was actually made. This structural scan is a CHEAP PROXY
    // for that ground truth, run on every `cargo test` without needing a
    // live dispatch; it exists to catch a regression FASTER, not to
    // out-rank the thing it approximates. A red scan with a green runtime
    // suite is worth investigating (the runtime tests only cover the two
    // arm shapes that exist TODAY, not every shape a future edit could
    // introduce) but is not proof of a live bypass by itself; a red
    // runtime test is.
    //
    // **Same shape as `darkmux-crew`'s `every_dispatch_remote_call_site_
    // is_guarded_against_resume_from` (#2580), NOT an extension of it.**
    // That check is hard-pinned to one file (`dispatch_internal.rs`) and
    // one identifier (`dispatch_remote`) in a DIFFERENT crate; generalizing
    // it to also cover `dispatch_via_submission` here would mean a
    // crate-or-workspace-wide scan — exactly the redesign its own doc
    // comment says a genuine visibility change would require, not
    // something worth building for a second, unrelated chokepoint. The
    // lexer discipline below (comment/string-aware, brace-matched,
    // structural if-block requirement) is copied in shape from that check
    // — same false-positive/false-negative traps apply to any text scan
    // over Rust source — but it is its own, separately-scoped check over
    // `routing.rs`, matching this file's much smaller premise (one caller
    // function, one private non-`pub` callee, no descendant module besides
    // its own inline `mod tests`).
    //
    // **What this cannot see, named plainly (same limits as #2580's
    // check, for the same reasons):**
    // - A `pub`/`pub(crate)` widening of `dispatch_via_submission`, or a new
    //   descendant module — both are pinned by the assertions below, so
    //   either fails LOUD rather than silently, but if `dispatch_via_submission`
    //   genuinely needs wider visibility this scan's premise is gone.
    // - A reimplementation of "submit this dispatch to another machine"
    //   that never calls `dispatch_via_submission` itself. Anything that
    //   builds its own `WorkJob`s via `fleet::build_work_job` and calls
    //   `fleet::submit_work` directly would skip the `--resume-from` guard
    //   below, and this check would not notice.
    // - A call reached only through a function-pointer alias.
    // - A call inside an `impl` block method or a macro body (the function
    //   extractor only indexes column-0 `fn`/`pub fn` items) — this FAILS
    //   THE TEST LOUDLY (a panic naming the shape) rather than silently
    //   certifying it.
    // - A guard whose message text is factored into a helper function
    //   instead of inlined at the `return Err`/`bail!` site — the anchor
    //   match is textual against the CALLING function's own body.
    // - The anchor match inside a qualifying block is still TEXTUAL, not a
    //   real control-flow prover — accepted as vanishingly unlikely given
    //   how specific the anchor phrase is, same as #2580's check accepts.
    // - (#2609 review round 3 Also-fix 1) A call site sitting BEFORE the
    //   enclosing match's first arm's own `=>` — a match scrutinee, or a
    //   pattern guard's condition (`Foo(x) if call_here() => ...`) — falls
    //   back to `nearest_enclosing_block`'s WHOLE-match scope instead of a
    //   single arm's, so a sibling arm's guard is visible there too (too
    //   WIDE, same failure class as a brace-less arm, different trigger).
    //   Neither of this file's two real call sites is in a scrutinee or a
    //   pattern guard today — both are ordinary arm bodies — so this is
    //   correct by accident rather than by construction; see
    //   `match_arm_span_within`'s doc for the mechanism.

    /// If `cs[i]` begins a `//` line comment, `/* */` block comment, raw
    /// string, ordinary string, or char literal, returns the index just
    /// past it. Otherwise `None` — genuine code. Copied in shape from
    /// `darkmux-crew`'s `dispatch_internal_tests::skip_non_code_span`
    /// (different crate; that one isn't reachable from here).
    fn skip_non_code_span(cs: &[char], i: usize) -> Option<usize> {
        let c = cs[i];
        let next = cs.get(i + 1).copied();
        match c {
            '/' if next == Some('/') => {
                let mut j = i;
                while j < cs.len() && cs[j] != '\n' {
                    j += 1;
                }
                Some(j)
            }
            '/' if next == Some('*') => {
                let mut j = i + 2;
                while j + 1 < cs.len() && !(cs[j] == '*' && cs[j + 1] == '/') {
                    j += 1;
                }
                Some((j + 2).min(cs.len()))
            }
            'r' if next == Some('"') || next == Some('#') => {
                let mut hashes = 0usize;
                let mut j = i + 1;
                while cs.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if cs.get(j) != Some(&'"') {
                    return None;
                }
                j += 1;
                loop {
                    if j >= cs.len() {
                        break;
                    }
                    if cs[j] == '"' && (1..=hashes).all(|k| cs.get(j + k) == Some(&'#')) {
                        j += 1 + hashes;
                        break;
                    }
                    j += 1;
                }
                Some(j)
            }
            '"' => {
                let mut j = i + 1;
                while j < cs.len() && cs[j] != '"' {
                    if cs[j] == '\\' {
                        j += 1;
                    }
                    j += 1;
                }
                Some((j + 1).min(cs.len()))
            }
            '\'' => {
                let is_char_lit = match next {
                    Some('\\') => true,
                    Some(_) => cs.get(i + 2) == Some(&'\''),
                    None => false,
                };
                if is_char_lit {
                    let mut j = i + 1;
                    while j < cs.len() && cs[j] != '\'' {
                        if cs[j] == '\\' {
                            j += 1;
                        }
                        j += 1;
                    }
                    Some((j + 1).min(cs.len()))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// If a function-declaration keyword sequence (`fn `, `pub fn `, or a
    /// restricted-visibility form — `pub(crate) fn `, `pub(super) fn `,
    /// `pub(self) fn `, `pub(in a::b) fn `) begins at `cs[i]`, returns the
    /// index just past the trailing space of `fn `. Otherwise `None`.
    ///
    /// (#2584 review — Also-fix 1) The earlier version of this scan only
    /// recognized `fn ` and `pub fn `, so a `pub(crate) fn` at column 0
    /// (this very file already declares three: `scan_flow_entries_for_
    /// completion`, `match_completion`, `completion_to_dispatch_result`)
    /// silently dropped out of the function index — invisible to every
    /// consumer of `top_level_function_spans`, including the guard-search
    /// used by `every_dispatch_via_submission_call_site_is_guarded_against_
    /// resume_from` above, with no failure signal pointing at the real
    /// cause. Recognizing the restricted forms here closes that gap.
    fn fn_decl_prefix_len(cs: &[char], i: usize) -> Option<usize> {
        if cs[i..].starts_with(&['f', 'n', ' ']) {
            return Some(i + 3);
        }
        if !cs[i..].starts_with(&['p', 'u', 'b']) {
            return None;
        }
        let mut j = i + 3;
        if cs.get(j) == Some(&'(') {
            // Skip the parenthesized visibility qualifier — `(crate)`,
            // `(super)`, `(self)`, `(in some::path)` — by depth-matched
            // parens rather than a fixed keyword list, so a future Rust
            // edition's restricted-visibility syntax doesn't need a rewrite
            // here too.
            let mut depth = 1i32;
            let mut k = j + 1;
            while k < cs.len() && depth > 0 {
                match cs[k] {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                k += 1;
            }
            if depth != 0 {
                return None; // unterminated — not genuine code; bail conservatively.
            }
            j = k;
        }
        // Require at least one space between the visibility keyword and `fn`.
        let space_start = j;
        while cs.get(j) == Some(&' ') {
            j += 1;
        }
        if j == space_start {
            return None;
        }
        if cs[j..].starts_with(&['f', 'n', ' ']) {
            Some(j + 3)
        } else {
            None
        }
    }

    /// Every top-level (column-0) function in `src`, as `(name,
    /// body_start, body_end)` byte-offset spans covering from the opening
    /// `{` through its matching closing `}`. Same algorithm as
    /// `darkmux-crew`'s `top_level_function_spans`, extended (#2584 review)
    /// to recognize restricted-visibility `fn` declarations via
    /// `fn_decl_prefix_len` above.
    fn top_level_function_spans(src: &str) -> Vec<(String, usize, usize)> {
        let mut spans = Vec::new();
        let cs: Vec<char> = src.chars().collect();
        let byte_offsets: Vec<usize> = src.char_indices().map(|(b, _)| b).collect();
        let mut i = 0usize;
        while i < cs.len() {
            if let Some(skip_to) = skip_non_code_span(&cs, i) {
                i = skip_to;
                continue;
            }
            let at_line_start = i == 0 || cs[i - 1] == '\n';
            if at_line_start {
                if let Some(name_start) = fn_decl_prefix_len(&cs, i) {
                    let mut k = name_start;
                    while k < cs.len() && (cs[k].is_alphanumeric() || cs[k] == '_') {
                        k += 1;
                    }
                    let name: String = cs[name_start..k].iter().collect();
                    let mut j = k;
                    let mut paren_depth = 0i32;
                    while j < cs.len() {
                        match cs[j] {
                            '(' => paren_depth += 1,
                            ')' => paren_depth -= 1,
                            '{' if paren_depth == 0 => break,
                            ';' if paren_depth == 0 => break,
                            _ => {}
                        }
                        j += 1;
                    }
                    if j < cs.len() && cs[j] == '{' {
                        let mut depth = 0i32;
                        let mut m = j;
                        let body_start_char = j;
                        loop {
                            if m >= cs.len() {
                                break;
                            }
                            match cs[m] {
                                '{' => depth += 1,
                                '}' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        let start_b = byte_offsets[body_start_char];
                                        let end_b = if m + 1 < byte_offsets.len() {
                                            byte_offsets[m + 1]
                                        } else {
                                            src.len()
                                        };
                                        spans.push((name.clone(), start_b, end_b));
                                        break;
                                    }
                                }
                                _ => {}
                            }
                            m += 1;
                        }
                    }
                    i = j;
                    continue;
                }
            }
            i += 1;
        }
        spans
    }

    /// Every top-level `mod`/`pub mod`/`pub(crate) mod` DECLARATION line in
    /// `src` (comment/string-aware). Every module declared here is a
    /// DESCENDANT module, and Rust makes this file's private items
    /// (including `dispatch_via_submission`) visible to every descendant.
    fn top_level_mod_declarations(src: &str) -> Vec<String> {
        let cs: Vec<char> = src.chars().collect();
        let byte_offsets: Vec<usize> = src.char_indices().map(|(b, _)| b).collect();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < cs.len() {
            if let Some(skip_to) = skip_non_code_span(&cs, i) {
                i = skip_to;
                continue;
            }
            let at_line_start = i == 0 || cs[i - 1] == '\n';
            if at_line_start {
                let lookahead: String = cs[i..(i + 15).min(cs.len())].iter().collect();
                let is_mod_decl = lookahead.starts_with("mod ")
                    || lookahead.starts_with("pub mod ")
                    || lookahead.starts_with("pub(crate) mod ");
                if is_mod_decl {
                    let mut j = i;
                    while j < cs.len() && cs[j] != '\n' {
                        j += 1;
                    }
                    let b0 = byte_offsets[i];
                    let b1 = if j < byte_offsets.len() { byte_offsets[j] } else { src.len() };
                    out.push(src[b0..b1].trim().to_string());
                }
            }
            i += 1;
        }
        out
    }

    /// Every call-shaped occurrence of `name` in `src`: the whole
    /// identifier (not a longer identifier that merely contains it), found
    /// only at genuine code positions, followed by optional whitespace and
    /// then `(`. Excludes the `fn <name>(` / `pub fn <name>(` declaration
    /// itself. Returns the byte offset of the start of `name` for each hit.
    fn find_calls(src: &str, name: &str) -> Vec<usize> {
        let cs: Vec<char> = src.chars().collect();
        let byte_offsets: Vec<usize> = src.char_indices().map(|(b, _)| b).collect();
        let name_chars: Vec<char> = name.chars().collect();
        let mut hits = Vec::new();
        let mut i = 0usize;
        while i < cs.len() {
            if let Some(skip_to) = skip_non_code_span(&cs, i) {
                i = skip_to;
                continue;
            }
            let end = i + name_chars.len();
            let name_matches = end <= cs.len() && cs[i..end] == name_chars[..];
            if name_matches {
                let before_ok = i == 0 || !(cs[i - 1].is_alphanumeric() || cs[i - 1] == '_');
                let after_ok = cs.get(end).map(|c| !(c.is_alphanumeric() || *c == '_')).unwrap_or(true);
                if before_ok && after_ok {
                    let mut j = end;
                    while j < cs.len() && matches!(cs[j], ' ' | '\t' | '\r' | '\n') {
                        j += 1;
                    }
                    let is_call = cs.get(j) == Some(&'(');
                    let prefix: String = cs[i.saturating_sub(4)..i].iter().collect();
                    let is_declaration = prefix.ends_with("fn ");
                    if is_call && !is_declaration {
                        hits.push(byte_offsets[i]);
                    }
                }
            }
            i += 1;
        }
        hits
    }

    /// Every top-level-ish `if` in `body`, as `(cond_start, block_start,
    /// block_end)` byte offsets INTO `body`.
    fn find_if_blocks(body: &str) -> Vec<(usize, usize, usize)> {
        let cs: Vec<char> = body.chars().collect();
        let byte_offsets: Vec<usize> = body.char_indices().map(|(b, _)| b).collect();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < cs.len() {
            if let Some(skip_to) = skip_non_code_span(&cs, i) {
                i = skip_to;
                continue;
            }
            let before_ok = i == 0 || !(cs[i - 1].is_alphanumeric() || cs[i - 1] == '_');
            let lookahead: String = cs[i..(i + 3).min(cs.len())].iter().collect();
            let is_if = before_ok && (lookahead.starts_with("if ") || lookahead.starts_with("if("));
            if is_if {
                let cond_start = i;
                let mut j = i + 2;
                let mut paren_depth = 0i32;
                loop {
                    if j >= cs.len() {
                        break;
                    }
                    if let Some(skip_to) = skip_non_code_span(&cs, j) {
                        j = skip_to;
                        continue;
                    }
                    match cs[j] {
                        '(' => paren_depth += 1,
                        ')' => paren_depth -= 1,
                        '{' if paren_depth <= 0 => break,
                        ';' if paren_depth <= 0 => break,
                        _ => {}
                    }
                    j += 1;
                }
                if j < cs.len() && cs[j] == '{' {
                    let block_start_char = j;
                    let mut depth = 0i32;
                    let mut m = j;
                    loop {
                        if m >= cs.len() {
                            break;
                        }
                        if let Some(skip_to) = skip_non_code_span(&cs, m) {
                            m = skip_to;
                            continue;
                        }
                        match cs[m] {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    let cond_start_b = byte_offsets[cond_start];
                                    let block_start_b = byte_offsets[block_start_char];
                                    let block_end_b =
                                        if m + 1 < byte_offsets.len() { byte_offsets[m + 1] } else { body.len() };
                                    out.push((cond_start_b, block_start_b, block_end_b));
                                    break;
                                }
                            }
                            _ => {}
                        }
                        m += 1;
                    }
                }
                i = j;
                continue;
            }
            i += 1;
        }
        out
    }

    /// Every matching brace-delimited block in `body` (comment/string-aware),
    /// at EVERY nesting depth — struct-pattern braces (`Foo { a, b } =>`)
    /// included, since this scan doesn't need to distinguish those from
    /// control-flow blocks: it only ever uses the result to find the
    /// SMALLEST block containing a given offset, and a struct-pattern brace
    /// never contains anything past its own `=>`.
    ///
    /// (#2584 review MUST-FIX) Added so the guard search below can be scoped
    /// to "the block immediately containing this call" instead of "anywhere
    /// in the whole enclosing function" — see `resume_from_guard_precedes`'s
    /// doc for why the wider scope was a false-negative trap.
    fn all_brace_block_spans(body: &str) -> Vec<(usize, usize)> {
        let cs: Vec<char> = body.chars().collect();
        let byte_offsets: Vec<usize> = body.char_indices().map(|(b, _)| b).collect();
        let mut stack: Vec<usize> = Vec::new();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < cs.len() {
            if let Some(skip_to) = skip_non_code_span(&cs, i) {
                i = skip_to;
                continue;
            }
            match cs[i] {
                '{' => stack.push(i),
                '}' => {
                    if let Some(start_char) = stack.pop() {
                        let start_b = byte_offsets[start_char];
                        let end_b =
                            if i + 1 < byte_offsets.len() { byte_offsets[i + 1] } else { body.len() };
                        out.push((start_b, end_b));
                    }
                }
                _ => {}
            }
            i += 1;
        }
        out
    }

    /// The SMALLEST brace-delimited block in `body` that contains `at` —
    /// i.e. the nearest enclosing block (the containment test is half-open,
    /// `start <= at < end`, not a strict open interval). Falls back to the
    /// entire `body` span if `at` isn't inside any brace pair (shouldn't
    /// happen for a call site inside a real function body, but a scan
    /// bug here must fail wide-open rather than panic).
    ///
    /// (#2609 review round 2) This is no longer what scopes the guard
    /// search for a call inside a `match` arm — see `guard_search_scope`,
    /// which uses this only as its fallback for a call that isn't inside
    /// any `match` at all. Using this DIRECTLY as the arm-guard scope (the
    /// round-1 fix) was itself a proxy that broke in both directions: a
    /// brace-less arm has no block of its own, so the smallest enclosing
    /// block widens all the way out to the whole `match` — visible to a
    /// SIBLING arm's guard; and a call nested one level deeper than the
    /// guard WITHIN the same arm (an ordinary `if`, a closure body) shrinks
    /// the smallest enclosing block down PAST the arm's own guard. Neither
    /// failure is hypothetical — see `guard_search_scope`'s doc.
    fn nearest_enclosing_block(body: &str, at: usize) -> (usize, usize) {
        all_brace_block_spans(body)
            .into_iter()
            .filter(|(start, end)| *start <= at && at < *end)
            .min_by_key(|(start, end)| end - start)
            .unwrap_or((0, body.len()))
    }

    /// Every `=>` at bracket-depth 0 relative to `inner` (comment/string
    /// aware, depth tracked over `(){}[]` together), as the byte offset
    /// just PAST each one. `inner` is expected to be a brace block's
    /// content with its own wrapping `{`/`}` already stripped, so a
    /// `match`'s own arms — each `Pattern => body`, separated at the
    /// match's own top level — show up at depth 0 while anything inside a
    /// nested block (an arm's own `{ ... }` body, an `if`, a closure) does
    /// not.
    fn depth0_arrow_ends(inner: &str) -> Vec<usize> {
        let cs: Vec<char> = inner.chars().collect();
        let byte_offsets: Vec<usize> = inner.char_indices().map(|(b, _)| b).collect();
        let mut depth = 0i32;
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < cs.len() {
            if let Some(skip_to) = skip_non_code_span(&cs, i) {
                i = skip_to;
                continue;
            }
            match cs[i] {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                '=' if depth == 0 && cs.get(i + 1) == Some(&'>') => {
                    let end_char = i + 2;
                    let end_b = if end_char < byte_offsets.len() {
                        byte_offsets[end_char]
                    } else {
                        inner.len()
                    };
                    out.push(end_b);
                    i += 2;
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
        out
    }

    /// If `[block_start, block_end)` (a full brace-delimited span, braces
    /// included, as returned by `all_brace_block_spans`) is a `match`
    /// body — recognized STRUCTURALLY by having at least one `=>` at
    /// bracket-depth 0 relative to its own content, which is what a
    /// `match`'s own top level looks like and an ordinary `if`/closure/loop
    /// block does not — returns the byte span of the ARM that contains
    /// `at`: from just after that arm's own `=>` to just before the next
    /// arm's `=>` (or the block's closing brace, for the last arm).
    /// Returns `None` when `[block_start, block_end)` isn't a match body,
    /// so the caller keeps walking outward to a bigger ancestor block.
    ///
    /// This is deliberately generous at the arm's END boundary: it runs up
    /// to the NEXT arm's own `=>`, which over-includes that next arm's
    /// pattern (and any `if <guard>` on it) as part of "this" arm's
    /// span. Accepted as vanishingly unlikely to matter — the same kind of
    /// textual-heuristic tolerance `resume_from_guard_precedes`'s own doc
    /// already accepts — because a match arm's PATTERN never itself
    /// contains a `resume_from`-conditioned guard with this exact anchor.
    ///
    /// (#2609 review round 3 Also-fix 1, documentation only) When `at`
    /// sits inside `[block_start, block_end)` but BEFORE this match's
    /// first arm's own `=>` — the scrutinee, or a pattern guard's
    /// condition (`Foo(x) if call_here() => ...`) — none of the per-arm
    /// ranges built from `arrow_ends` cover it (they all start at or after
    /// the first arrow), so this returns `None` even though `at` genuinely
    /// is inside a match body. `guard_search_scope`'s walk then keeps going
    /// outward and can land on `nearest_enclosing_block`'s WHOLE-match
    /// fallback for that call, which makes a sibling arm's guard visible —
    /// too WIDE. Not fixed here: neither of this file's two real call
    /// sites sits in a scrutinee or a pattern guard (both are ordinary arm
    /// bodies), so this is a named gap, not a live bypass today.
    fn match_arm_span_within(
        body: &str,
        block_start: usize,
        block_end: usize,
        at: usize,
    ) -> Option<(usize, usize)> {
        if block_end < block_start + 2 {
            return None;
        }
        let inner = &body[block_start + 1..block_end - 1];
        let arrow_ends: Vec<usize> =
            depth0_arrow_ends(inner).into_iter().map(|off| off + block_start + 1).collect();
        if arrow_ends.is_empty() {
            return None;
        }
        for (idx, &arm_start) in arrow_ends.iter().enumerate() {
            let arm_end = arrow_ends.get(idx + 1).copied().unwrap_or(block_end - 1);
            if arm_start <= at && at < arm_end {
                return Some((arm_start, arm_end));
            }
        }
        None
    }

    /// The scope `resume_from_guard_precedes` should search for the call
    /// at `at` in `body`: when `at` sits inside a `match` arm (braced or
    /// brace-less), the byte span of THAT ARM alone — from
    /// `match_arm_span_within` — so a sibling arm's guard is invisible
    /// (the too-WIDE failure a brace-less arm produces against plain
    /// `nearest_enclosing_block`) while a nested block WITHIN the same arm
    /// (an ordinary `if`, a closure body) stays visible (the too-NARROW
    /// failure `nearest_enclosing_block` produces by shrinking to that
    /// nested block instead of the whole arm). Falls back to
    /// `nearest_enclosing_block` when `at` isn't inside any `match` at all
    /// — there is no arm to scope to.
    ///
    /// (#2609 review round 2) Walks every brace block containing `at`,
    /// SMALLEST first. Starting from the smallest is what fixes the
    /// too-narrow case: a nested `if`/closure block around the call is
    /// tried first, is correctly rejected (it has no depth-0 `=>` of its
    /// own), and the walk continues outward to the arm's own block, then —
    /// since an arm's own `{ ... }` body ALSO has no depth-0 `=>` of its
    /// own — outward again to the `match`'s own enclosing block, which
    /// does, and which is where `match_arm_span_within` computes the
    /// correct per-arm boundaries regardless of how deep the call sits
    /// inside that one arm.
    ///
    /// (#2609 review round 3 MUST-FIX) That reasoning silently assumed
    /// every block nested BETWEEN the arm's guard and the call has no
    /// `=>` of its own — true for an `if` or a closure, false for a
    /// NESTED `match`, which has depth-0 arrows relative to its own
    /// content just like the outer one does. The round-2 code returned at
    /// the FIRST block `match_arm_span_within` recognized, so a call
    /// wrapped in a nested match (with the real, outer arm's guard left
    /// untouched, earlier in the SAME outer arm) scoped to the inner
    /// match's own arm instead — cutting the outer guard out of the
    /// search and producing a false accusation against provably-correct
    /// code. Red-proven: wrapping `dispatch_via_submission(opts, Some(&target))`
    /// in `match true { true => return dispatch_via_submission(...), false =>
    /// {} }` inside the `local_unknown: false` arm, guard left in place,
    /// made the structural scan below FAIL while the runtime test
    /// (`dispatch_routed_via_refuses_resume_from_before_anything_is_
    /// sent`) stayed GREEN — ground truth says the guard fires, the
    /// scan accused it anyway.
    ///
    /// Fixed by not stopping at the first match: the walk now keeps going
    /// through every containing block, smallest to largest, and remembers
    /// the LAST (i.e. largest / outermost) span `match_arm_span_within`
    /// recognizes, falling back to `nearest_enclosing_block` only when
    /// NONE of them are. A nested match's own block is still recognized
    /// and still yields a span — that span is just no longer trusted as
    /// final the moment a bigger ancestor match also claims the offset.
    ///
    /// **Direction, corrected (round 4) — this paragraph previously had it
    /// backwards.** Taking the LAST (outermost) recognized arm span rather
    /// than the first does widen the chosen scope monotonically — an outer
    /// arm's span always contains every block nested inside it, never the
    /// reverse. But widening the search scope is what produces a false
    /// CERTIFICATION, not a false accusation: more text becomes visible to
    /// `resume_from_guard_precedes`, and — as round 3's own bug showed —
    /// that "more text" can be a SIBLING arm's guard inside a nested
    /// `match`, which the guard search then accepts as "preceding" a call
    /// that is genuinely unguarded in its own arm. A false ACCUSATION (a
    /// real guard the scan fails to see) is what a scope that is too
    /// NARROW produces — round 2's failure mode, not this one. So the
    /// security property this whole conformance check exists for did NOT
    /// hold throughout round 3; `nested_match_sibling_exclusions` below is
    /// what restores it, by excluding a nested match's sibling-arm text
    /// from the guard search even though that text sits inside the (still
    /// outermost) scope. This is also what the assertion's own failure
    /// message already said ("not a sibling arm's or an unrelated
    /// block's") while this comment claimed the opposite could never
    /// happen.
    fn guard_search_scope(body: &str, at: usize) -> (usize, usize) {
        let mut containing: Vec<(usize, usize)> = all_brace_block_spans(body)
            .into_iter()
            .filter(|(start, end)| *start <= at && at < *end)
            .collect();
        containing.sort_by_key(|(start, end)| end - start);
        let mut outermost_arm: Option<(usize, usize)> = None;
        for (start, end) in containing {
            if let Some(span) = match_arm_span_within(body, start, end, at) {
                outermost_arm = Some(span);
            }
        }
        outermost_arm.unwrap_or_else(|| nearest_enclosing_block(body, at))
    }

    /// Byte ranges within `body`, inside the outermost scope
    /// `[scope_start, scope_end)` computed by `guard_search_scope`, that
    /// must stay invisible to the guard search even though they sit inside
    /// that scope: the SIBLING-arm text of any `match` block that is
    /// nested strictly inside the scope and that also contains `at`.
    ///
    /// (#2609 review round 4 MUST-FIX) `guard_search_scope`'s outermost-arm
    /// widening (round 3) fixed round 2's too-NARROW failure but opened a
    /// too-WIDE one: when the call sits in one arm of a nested `match` and
    /// a DIFFERENT (sibling) arm of that SAME nested `match` carries a
    /// `resume_from` guard, the outer arm's span contains the nested
    /// match's whole brace block — every sibling arm included — so the
    /// sibling's guard reads as "preceding" the call even though the two
    /// arms are mutually exclusive at runtime and the call's own arm has
    /// no guard at all. Red-proven: wrapping the call in
    /// `match true { true => { <the real guard, moved here> } false => {
    /// dispatch_via_submission(...) } }`, with the guard genuinely absent from
    /// the `false` arm the call lives in, made the structural scan below
    /// PASS while the runtime test
    /// (`dispatch_routed_via_refuses_resume_from_before_anything_is_
    /// sent`, dialed against a real peer) FAILED — a live bypass the
    /// scan certified as safe.
    ///
    /// Fixed not by narrowing `guard_search_scope`'s scope (that would
    /// reopen round 2's bug) but by excluding, from WITHIN the unchanged
    /// outermost scope, every nested match's sibling-arm text: for each
    /// match block strictly inside the outer scope that also contains
    /// `at`, only the arm of THAT block containing `at` stays visible to
    /// the guard search — the rest of the block (its sibling arms) is
    /// excluded even though it lies inside the outer arm's byte range. A
    /// guard in the SAME inner arm as the call is unaffected (it isn't in
    /// an excluded range); a guard in a sibling arm is.
    fn nested_match_sibling_exclusions(
        body: &str,
        scope_start: usize,
        scope_end: usize,
        at: usize,
    ) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (block_start, block_end) in all_brace_block_spans(body) {
            // Only blocks STRICTLY nested inside the outer scope — this
            // naturally excludes the ancestor match block that produced
            // the outer scope itself (that block always starts at or
            // before `scope_start` and/or ends at or after `scope_end`,
            // since it also holds the outer arm's OWN siblings).
            if block_start < scope_start || block_end > scope_end {
                continue;
            }
            if !(block_start <= at && at < block_end) {
                continue;
            }
            if let Some((arm_start, arm_end)) = match_arm_span_within(body, block_start, block_end, at) {
                if arm_start > block_start {
                    out.push((block_start, arm_start));
                }
                if arm_end < block_end {
                    out.push((arm_end, block_end));
                }
            }
        }
        out
    }

    /// Collapse Rust string-literal line continuations (a `\` immediately
    /// followed by a newline strips the newline and the next line's
    /// leading whitespace) — this repo wraps long operator-facing messages
    /// that way, so a raw-byte substring search for a multi-word anchor
    /// would be brittle to wherever the literal happens to be wrapped.
    fn collapse_str_continuations(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let cs: Vec<char> = s.chars().collect();
        let mut i = 0usize;
        while i < cs.len() {
            if cs[i] == '\\' && cs.get(i + 1) == Some(&'\n') {
                i += 2;
                while i < cs.len() && matches!(cs[i], ' ' | '\t' | '\r' | '\n') {
                    i += 1;
                }
                continue;
            }
            out.push(cs[i]);
            i += 1;
        }
        out
    }

    /// The phrase every remote-address + `--resume-from` guard must contain —
    /// the same promise `validate_resume_checkpoint` (container path) and
    /// the #2561/#2580 remote-single-shot guards state, restated true for
    /// the queued-peer route.
    const RESUME_FROM_GUARD_ANCHOR: &str =
        "darkmux never silently starts a dispatch fresh under a name that looked like a resume";

    /// True iff `body[..call_at_in_body]` contains an `if` block whose
    /// CONDITION mentions `resume_from`, whose block closes at or before
    /// `call_at_in_body`, and whose block text (after `\`-continuation
    /// collapsing) contains BOTH `RESUME_FROM_GUARD_ANCHOR` and a
    /// diverging construct (`return Err`/`bail!`/`panic!`).
    ///
    /// **`body` must already be scoped to the call site's own match-arm
    /// span (or nearest enclosing block, when the call isn't inside a
    /// match) — see `guard_search_scope` — never the whole enclosing
    /// FUNCTION and never just `nearest_enclosing_block` directly.**
    /// (#2584 review MUST-FIX; scoping mechanism replaced #2609 review
    /// round 2 — see `guard_search_scope`'s doc for why plain
    /// `nearest_enclosing_block` is itself insufficient) Both
    /// `dispatch_via_submission` call sites live in the same function
    /// (`dispatch_routed_via`'s two `Remote` match arms), and this function
    /// only checks "some guard occurs before this call ANYWHERE in `body`" —
    /// it has no notion of match-arm exclusivity. Called with the whole
    /// function body, the FIRST arm's guard (earlier in the source) would
    /// satisfy this check for the SECOND arm's call too, even after deleting
    /// the second arm's own guard — the two arms are mutually exclusive at
    /// runtime, but textually the first arm's guard still "precedes" the
    /// second arm's call. Scoping `body` to the call's own match-arm span
    /// closes this: the first arm's guard sits in a sibling span the scoped
    /// search never sees.
    ///
    /// `excluded` (#2609 review round 4 MUST-FIX) is a set of byte ranges
    /// INTO `body` — from `nested_match_sibling_exclusions` — that must be
    /// treated as if they weren't there, even though `body` is already
    /// scoped down to the outermost arm and these ranges sit inside it: a
    /// nested `match`'s sibling-arm text, which the outermost-scope fix
    /// (round 3) made visible again. A candidate guard whose `if` keyword
    /// starts inside one of these ranges is skipped, exactly as if it had
    /// never been found.
    fn resume_from_guard_precedes(
        body: &str,
        call_at_in_body: usize,
        excluded: &[(usize, usize)],
    ) -> bool {
        for (cond_start, block_start, block_end) in find_if_blocks(body) {
            if block_end > call_at_in_body {
                continue;
            }
            if excluded.iter().any(|(ex_start, ex_end)| cond_start >= *ex_start && cond_start < *ex_end) {
                continue;
            }
            let cond_text = &body[cond_start..block_start];
            if !cond_text.contains("resume_from") {
                continue;
            }
            let block_text = collapse_str_continuations(&body[block_start..block_end]);
            let diverges = block_text.contains("bail!")
                || block_text.contains("return Err")
                || block_text.contains("panic!");
            if diverges && block_text.contains(RESUME_FROM_GUARD_ANCHOR) {
                return true;
            }
        }
        false
    }

    /// Every genuine-code occurrence of the literal substring `needle` in
    /// `src` (comment/string-aware, via `skip_non_code_span`), as byte
    /// offsets. Needed because this conformance test's own source lives in
    /// the SAME FILE it scans (`routing.rs`'s `mod tests` is inline, unlike
    /// `dispatch_internal.rs` + its separate `dispatch_internal_tests.rs`
    /// in `darkmux-crew`) — a raw `str::matches` would also count this very
    /// test's own doc comments and the `DEFINITION_MARKER` string literal's
    /// VALUE as "occurrences", exactly the kind of false positive #2580's
    /// own review found and fixed for its sibling check.
    fn find_code_substring_occurrences(src: &str, needle: &str) -> Vec<usize> {
        let cs: Vec<char> = src.chars().collect();
        let byte_offsets: Vec<usize> = src.char_indices().map(|(b, _)| b).collect();
        let needle_chars: Vec<char> = needle.chars().collect();
        let mut hits = Vec::new();
        let mut i = 0usize;
        while i < cs.len() {
            if let Some(skip_to) = skip_non_code_span(&cs, i) {
                i = skip_to;
                continue;
            }
            let end = i + needle_chars.len();
            if end <= cs.len() && cs[i..end] == needle_chars[..] {
                hits.push(byte_offsets[i]);
            }
            i += 1;
        }
        hits
    }

    #[test]
    fn every_dispatch_via_submission_call_site_is_guarded_against_resume_from() {
        const DEFINITION_MARKER: &str = "fn dispatch_via_submission(";

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routing.rs");
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {} for resume_from conformance: {e}", path.display()));

        let functions = top_level_function_spans(&src);
        // (#2584 review — Also-fix 1) This file declares 9 top-level
        // (column-0, outside `mod tests`) functions today, 3 of them
        // `pub(crate)` — a shape `fn_decl_prefix_len` now recognizes
        // explicitly. The floor below is DELIBERATELY not pinned to
        // "today's count minus one": a floor with zero slack against the
        // real count means the FIRST legitimate function move out of this
        // file (a refactor, not a regression) trips this assertion, and a
        // message that only says "the extractor is broken" sends the
        // maintainer chasing the wrong cause — the real one being that this
        // scan's premise ("every relevant fn lives in this one file")
        // no longer holds. A few functions of real slack, plus a message
        // that names BOTH possibilities, keeps a genuine extractor
        // regression loud without turning an honest refactor into one.
        assert!(
            functions.len() > 3,
            "found only {} top-level fns in routing.rs (expected more — this file \
             currently declares 5, after #2916 removed the Redis wait path). Two different things produce this: (a) the extractor \
             regressed on a shape it should recognize (`fn_decl_prefix_len` — plain `fn`, \
             `pub fn`, or a restricted-visibility `pub(...) fn`), or (b) a function that used \
             to live at top-level in this file was genuinely moved or deleted, which means \
             this whole conformance scan's premise (everything relevant lives in THIS file) \
             may no longer hold. Check git blame on the delta before assuming either.",
            functions.len()
        );

        // ── pin: `dispatch_via_submission` stays module-PRIVATE ──────────────
        let definitions = find_code_substring_occurrences(&src, DEFINITION_MARKER);
        assert_eq!(
            definitions.len(),
            1,
            "expected exactly one code-position `{DEFINITION_MARKER}` declaration — found {}",
            definitions.len()
        );
        let def_at = definitions[0];
        let line_prefix = src[..def_at].rsplit('\n').next().unwrap_or("");
        assert_eq!(
            line_prefix, "",
            "`dispatch_via_submission` must stay module-PRIVATE for this scan's premise to hold — \
             found `{line_prefix}fn dispatch_via_submission(`, which reads as widened visibility. \
             If it genuinely needs wider visibility, this scan's premise is gone and it needs a \
             real redesign (a crate-or-workspace-wide scan), not a bigger pin."
        );

        // ── pin: this file declares no descendant module other than its
        //    own inline `#[cfg(test)] mod tests` ────────────────────────
        let mod_decls = top_level_mod_declarations(&src);
        assert_eq!(
            mod_decls,
            vec!["mod tests {".to_string()],
            "this scan's premise requires this file to declare NO descendant module other than \
             its own `#[cfg(test)] mod tests` — found: {mod_decls:?}. A new `mod` here is a \
             place a call to `dispatch_via_submission(` could live that this scan cannot see."
        );

        let call_offsets = find_calls(&src, "dispatch_via_submission");
        assert!(
            !call_offsets.is_empty(),
            "found zero calls to `dispatch_via_submission(` — either the extractor regressed or the \
             function was deleted; either way this test's premise no longer holds"
        );
        // (#2584 review — Also-fix 2) This is a bare count assertion, and
        // its failure cuts BOTH ways — don't just bump the constant to make
        // it pass again without asking which direction moved and why:
        // MORE than 2 is probably an honest new call site that needs the
        // same guard this test enforces on the existing two (bump the
        // constant once that guard is in place). FEWER than 2 is the
        // dangerous direction — it can mean a call site was hidden from
        // this scan rather than removed, e.g. a function-pointer alias
        // (`let f = dispatch_via_submission; ...; f(opts, ...)` — `find_calls`
        // only matches the identifier `dispatch_via_submission` immediately
        // followed by `(`, so an alias call never counts here at all).
        // Blindly lowering this constant to match a drop makes that
        // exact bypass permanent and silent.
        assert_eq!(
            call_offsets.len(),
            2,
            "expected exactly the two known call sites (both Remote arms of \
             `dispatch_routed_via`'s match) — found {}. If this went UP: a new call site needs \
             the same guard this test enforces on the existing two before you bump this \
             constant. If this went DOWN: do not just lower the constant — find out where the \
             missing call went first (a function-pointer alias is the known way a real call to \
             `dispatch_via_submission` can go invisible to this text scan).",
            call_offsets.len()
        );

        for call_at in call_offsets {
            let (fn_name, fn_start, fn_end) = functions
                .iter()
                .find(|(_, start, end)| *start <= call_at && call_at < *end)
                .unwrap_or_else(|| {
                    panic!(
                        "a `dispatch_via_submission(` call at byte offset {call_at} is not inside any \
                         top-level (column-0 `fn`/`pub fn`) function this scan indexes — extend \
                         `top_level_function_spans` before this test can vouch for it."
                    )
                });
            let body = &src[*fn_start..*fn_end];
            let call_at_in_body = call_at - fn_start;

            // (#2584 review MUST-FIX; scoping mechanism replaced #2609
            // review round 2) Scope the guard search to the call's own
            // match-arm SPAN — for these two call sites, that is each
            // `Remote` match arm's own extent, computed structurally from
            // the enclosing match's `=>` boundaries, not proxied by "the
            // nearest brace block" (which breaks in both directions — see
            // `guard_search_scope`'s doc) — NOT the whole `{fn_name}`
            // function. `dispatch_routed_via` holds BOTH `dispatch_via_
            // queue` call sites (one per match arm), and a function-wide
            // search would let one arm's guard vouch for the OTHER arm's
            // call, since match arms are mutually exclusive at runtime but
            // not textually ordered against each other. See
            // `resume_from_guard_precedes`'s doc for the full mechanism.
            let (scope_start, scope_end) = guard_search_scope(body, call_at_in_body);
            let scoped_body = &body[scope_start..scope_end];
            let call_at_in_scoped = call_at_in_body - scope_start;
            // (#2609 review round 4 MUST-FIX) The outermost scope above is
            // still right — narrowing it back would reopen round 2's bug —
            // but it can also make a nested match's SIBLING arm visible to
            // the guard search below. Exclude that sibling-arm text
            // explicitly; see `nested_match_sibling_exclusions`'s doc.
            let excluded_in_body =
                nested_match_sibling_exclusions(body, scope_start, scope_end, call_at_in_body);
            let excluded_in_scoped: Vec<(usize, usize)> = excluded_in_body
                .into_iter()
                .map(|(ex_start, ex_end)| (ex_start - scope_start, ex_end - scope_start))
                .collect();

            assert!(
                resume_from_guard_precedes(scoped_body, call_at_in_scoped, &excluded_in_scoped),
                "`{fn_name}` calls `dispatch_via_submission(` at file offset {call_at} without a \
                 `resume_from`-conditioned guard preceding it IN ITS OWN ENCLOSING SCOPE — the \
                 same match arm when the call sits in one, its own enclosing block otherwise. \
                 This is the #2561/#2580/#2584 bypass class: a caller can silently spend real \
                 tokens on a PEER machine under a --resume-from flag that was never honored. The \
                 guard must sit inside an `if` whose condition mentions `resume_from`, closes \
                 before the call, and contains both {RESUME_FROM_GUARD_ANCHOR:?} and a diverging \
                 bail!/return Err/panic! — all within that scope, not a sibling arm's or an \
                 unrelated block's. If this assertion is RED but the runtime tests \
                 (`dispatch_routed_via_refuses_resume_from_before_anything_is_sent` and \
                 `resume_from_local_unknown_arm.rs`) are GREEN: the runtime tests are ground \
                 truth (they run the real function against a real fake peer), this scan is a \
                 cheap proxy for them — investigate before assuming this scan is wrong, but a \
                 disagreement does not by itself mean this finding is a false positive."
            );
        }
    }
}
