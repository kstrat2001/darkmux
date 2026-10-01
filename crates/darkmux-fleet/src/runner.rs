//! Running a submitted job on this machine (#2916).
//!
//! The work-submission listener (`darkmux-serve`'s `fleet_listener.rs`)
//! admits a request, checks its scope, and hands the job here. Until 4.0 the
//! same code sat behind a Redis claim loop (`darkmux:work`, consumer group
//! `darkmux-runners`); that loop is gone, because the queue could not say
//! who wrote an entry. What survived is the execution: the shape check, the
//! workdir containment guard, and `dispatch_reconciled`, unchanged.

use crate::WorkJob;
use anyhow::{Context, Result};
use darkmux_crew::dispatch::DispatchResult;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// (#2476 review round 2, MUST FIX 3) True while a submitted job is inside
/// its one synchronous `dispatch_reconciled` call — the span that covers
/// `dispatch_internal.rs`'s own post-wait interrupt check (the thing that
/// actually issues `docker kill <container>`). The daemon's shutdown path
/// polls [`dispatch_in_flight`], bounded, after signaling the interrupt, so
/// that check gets a real window to run before the process exits.
///
/// (#2916 stage 2) A COUNT, not a flag: the listener now runs one job per
/// local model and hosted jobs beside them, so several can be in flight, and
/// the first to finish must not report the rest as done.
static RUNNER_DISPATCH_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// True while a submitted job is inside `dispatch()` — see
/// [`RUNNER_DISPATCH_IN_FLIGHT`]'s own doc. `darkmux-serve`'s shutdown path
/// is the one caller.
pub fn dispatch_in_flight() -> bool {
    RUNNER_DISPATCH_IN_FLIGHT.load(Ordering::SeqCst) > 0
}

/// RAII guard scoping [`RUNNER_DISPATCH_IN_FLIGHT`] to exactly the
/// `dispatch()` call, on every exit path including a panic.
struct DispatchInFlightGuard;

impl DispatchInFlightGuard {
    fn new() -> Self {
        RUNNER_DISPATCH_IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for DispatchInFlightGuard {
    fn drop(&mut self) {
        RUNNER_DISPATCH_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Run one admitted, in-scope job on THIS machine, on `profile` (the
/// profile the scope check resolved and approved; never re-resolved here, so
/// what runs is what was checked), for the peer `origin`.
///
/// The shape is re-validated, and a `workdir` must resolve (symlinks and
/// all) under this machine's darkmux worktrees base (#840): a sender can
/// never bind-mount an arbitrary directory of this machine as `/workspace`.
/// The validated CANONICAL path is what gets mounted, closing the TOCTOU
/// window a re-resolution would reopen. The dispatch is marked
/// remote-origin, so it never mounts this machine's shared toolchain cache.
///
/// A dispatch that panics is caught and returned as an error, so one bad
/// job cannot take the listener's worker down.
pub fn execute_job(job: WorkJob, profile: String, origin: String) -> Result<DispatchResult> {
    // (#2628) `dispatch_reconciled`, not the raw primitive, so a submitted
    // job gets the same Exclusive-reconcile + #1487 residency-lease
    // protection a `darkmux dispatch` gets. (#2916 stage 2) The listener may
    // now run jobs on different local models at once, in one process;
    // `dispatch_reconciled`'s lease is a per-guard union that pins every
    // live same-process sibling's model (#2651, #2663), so one job's
    // reconcile does not evict another's.
    let primitive = match job.single_shot {
        Some(_) => darkmux_crew::dispatch_reconciled::dispatch_reconciled_single_shot,
        None => darkmux_crew::dispatch_reconciled::dispatch_reconciled,
    };
    execute_job_with(job, profile, origin, primitive)
}

/// [`execute_job`] with the dispatch primitive injected, so what reaches
/// dispatch (the resolved profile, the remote origin, the validated
/// workdir) is testable without a container.
pub fn execute_job_with(
    mut job: WorkJob,
    profile: String,
    origin: String,
    dispatch: impl FnOnce(darkmux_crew::dispatch::DispatchOpts) -> Result<DispatchResult>,
) -> Result<DispatchResult> {
    job.validate().context("the job failed its shape check")?;
    if let Some(workdir_str) = &job.workdir {
        let canonical = darkmux_types::workdir::validate_remote_workdir(std::path::Path::new(workdir_str))
            .context("workdir validation failed")?;
        job.workdir = Some(canonical.to_string_lossy().into_owned());
    }
    assert_boundary_still_holds(&job, &profile)?;
    let single_shot = job.single_shot;
    let mut opts = job.into_dispatch_opts();
    opts.profile_name = Some(profile);
    opts.remote_origin = Some(origin);
    if let Some(single_shot) = single_shot {
        apply_single_shot(&mut opts, single_shot)?;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _in_flight = DispatchInFlightGuard::new();
        dispatch(opts)
    }));
    match result {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!("the dispatch panicked; the listener survived it")),
    }
}

/// The job's boundary against what execution resolves NOW. The scope check
/// approved the job against the profile as it read then; dispatch resolves
/// the profile by name again, so a registry edited in between could point it
/// at a hosted endpoint the boundary forbids. This asks the resolution
/// dispatch itself routes on (`dispatch_resolves_remote`, which fails closed)
/// and refuses before anything is reconciled, loaded or sent.
///
/// Known residual: dispatch reads the registry once more after this check, a
/// window of a few milliseconds in which a further edit still wins. The full
/// fix is to carry the resolved target in `DispatchOpts` so dispatch never
/// resolves it again; that field would touch every `DispatchOpts` literal.
fn assert_boundary_still_holds(job: &WorkJob, profile: &str) -> Result<()> {
    let holds = match job.boundary {
        None => true,
        Some(crate::Boundary::ManagedOnly) => {
            !darkmux_crew::dispatch::dispatch_resolves_remote(&job.role_id, Some(profile), None)
        }
        Some(crate::Boundary::Unknown) => false,
    };
    if holds {
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "the job's boundary no longer holds: profile {profile} no longer resolves to a managed endpoint here \
         (or the boundary is one this darkmux does not know); nothing was sent"
    ))
}

/// Give a `single_shot` job's dispatch what the sender's local seat would
/// have given it: the persona built from THIS machine's own `radio-host`
/// template (a sender never sends prompt text), and a token budget of the
/// smaller of the sender's ask and this machine's own cap. The dispatch
/// primitive for such a job is the single-shot one ([`execute_job`]), which
/// with an override skips the specialist preamble.
fn apply_single_shot(opts: &mut darkmux_crew::dispatch::DispatchOpts, single_shot: crate::SingleShotJob) -> Result<()> {
    use darkmux_crew::radio_persona::{answering_system_prompt, peer_token_cap};
    opts.system_prompt_override = Some(answering_system_prompt(single_shot.humor, single_shot.surface)?);
    opts.max_completion_tokens = Some(peer_token_cap(single_shot.max_completion_tokens));
    Ok(())
}

impl WorkJob {
    /// Convert a received `WorkJob` into the `DispatchOpts` shape the
    /// dispatch entry point consumes: the one wire → in-process boundary.
    pub fn into_dispatch_opts(self) -> darkmux_crew::dispatch::DispatchOpts {
        use darkmux_crew::dispatch::DispatchOpts;
        DispatchOpts {
            // (#2914) Work never runs on the utility model.
            allow_utility_model: false,
            remote_origin: None,
            live_channel: true,
            // (#2265) A cross-machine job carries its brief as TEXT, so a
            // `--finding`-briefed dispatch still reaches the runner with the
            // finding's record inside `message`; only the keys field — this
            // machine's provenance note about where that text came from — does
            // not cross the wire, because `WorkJob` does not carry it.
            brief_refs: Vec::new(),
            workspace_read_only: false,
            record_context: None,
            resume_from: None,
            host_out: None,
            max_turns_override: None,
            // (#2480 review, finding 7) `--timeout` does NOT cross to the
            // other machine: `WorkJob` carries no field for it (adding one is a real
            // wire break — the struct is `deny_unknown_fields` under a
            // versioned `WORK_JOB_SCHEMA_VERSION`), so a cross-machine
            // dispatch runs on the RUNNER's own
            // `env > config > 600` inactivity budget. Disclosed in the flag's
            // own `--help` ("Local dispatch only: ignored on a `--profile
            // <p>@<machine>` dispatch"), the same way `--max-completion-tokens`
            // states its own cross-machine limit.
            // `self.timeout_seconds` below still crosses and still bounds the
            // tool-less hosted path's `curl -m`; only the container path's
            // inactivity override stops here.
            timeout_override_seconds: None,
            role_id: self.role_id,
            message: self.message,
            session: self.session_id,
            timeout_seconds: self.timeout_seconds,
            skip_preflight: false,
            // Runner-side dispatches preserve today's human-readable
            // stdout shape — JSON-envelope mode is operator-explicit
            // and only fires when the originating CLI used --json.
            // (When cross-machine plumbing eventually carries the flag
            // through WorkJob, this can read self.json.)
            json: false,
            workdir: self.workdir.map(PathBuf::from),
            phase_id: None,
            // A received job runs HERE: never forwarded to another machine
            // (that would bounce jobs between machines); always synchronous.
            machine: None,
            wait: true,
            // Fleet-deserialized dispatch jobs: producer didn't
            // capture compaction config (pre-#368 wire shape). Use
            // runtime defaults. Future iteration could propagate via
            // the job payload if cross-machine compaction tuning
            // becomes a real requirement.
            compaction: darkmux_crew::dispatch::CompactionDispatchArgs::default(),
            // (#2916) The job's own `profile` request; `execute_job`
            // replaces it with the profile the scope check resolved.
            profile_name: self.profile,
            // (#984) Fleet-deserialized jobs don't carry a profiles-file
            // either — the runner resolves against its local registry.
            config_path: None,
            // (#1199) Bench-only knobs; defaults preserve existing behavior.
            force_container: false,
            max_completion_tokens: None,
            // (#703 Slice 4) Honor the image the publisher requested (carried
            // on the WorkJob); the runner injects darkmux's binary into it.
            // `None` → the runner's default slim image.
            image: self.image,
            // Mock-model harness (v1): not carried on a WorkJob today —
            // fleet-published dispatches always target real LMStudio.
            // `None` preserves that.
            model_base_url_override: None,
            step_id: None, // (#1483) set on the graph-step path only
            system_prompt_override: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> WorkJob {
        WorkJob {
            target_machine: "studio".into(),
            target_machine_uid: None,
            role_id: "coder".into(),
            message: "m".into(),
            session_id: crate::test_session("s"),
            profile: Some("host".into()),
            workdir: None,
            image: Some("rust:slim".into()),
            timeout_seconds: 60,
            published_at_unix_ms: 1,
            published_by_machine: None,
            single_shot: None,
            boundary: None,
            mode: crate::SubmissionMode::Run,
        }
    }

    /// The conversion carries what crosses and never recurses to another
    /// machine.
    #[test]
    fn into_dispatch_opts_carries_the_job_and_never_reroutes() {
        let o = job().into_dispatch_opts();
        assert_eq!(o.role_id, "coder");
        assert_eq!(o.profile_name.as_deref(), Some("host"));
        assert_eq!(o.session, crate::test_session("s"));
        assert!(o.phase_id.is_none(), "a received job carries no phase (#2954)");
        assert_eq!(o.image.as_deref(), Some("rust:slim"));
        assert!(o.machine.is_none(), "a received job runs here; it is never forwarded");
        assert!(!o.allow_utility_model);
    }

    /// A job that fails its shape check never reaches dispatch.
    #[test]
    fn execute_job_refuses_a_malformed_job_before_dispatch() {
        let mut j = job();
        j.role_id = "../x".into();
        let err = execute_job_with(j, "host".into(), "laptop".into(), |_| panic!("never dispatched")).unwrap_err();
        assert!(format!("{err:#}").contains("shape check"), "{err:#}");
        assert!(!dispatch_in_flight());
    }

    /// The last guard before execution: a boundary this darkmux does not
    /// know fails closed, and the dispatch closure never runs.
    #[test]
    fn execute_job_refuses_an_unknown_boundary() {
        let mut j = job();
        j.boundary = Some(crate::Boundary::Unknown);
        let err = execute_job_with(j, "host".into(), "laptop".into(), |_| panic!("never dispatched")).unwrap_err();
        assert!(format!("{err:#}").contains("boundary no longer holds"), "{err:#}");
        assert!(!dispatch_in_flight());
    }

    /// A workdir outside the worktrees base is refused before dispatch.
    #[test]
    fn execute_job_refuses_a_workdir_outside_the_worktrees_base() {
        let mut j = job();
        j.workdir = Some("/etc".into());
        let err = execute_job_with(j, "host".into(), "laptop".into(), |_| panic!("never dispatched")).unwrap_err();
        assert!(format!("{err:#}").contains("workdir"), "{err:#}");
    }

    /// (#2916 review C3/M2) What reaches dispatch: the RESOLVED profile
    /// (not the job's own request), the remote origin, never a forward.
    #[test]
    fn execute_job_hands_dispatch_the_resolved_profile_and_the_origin() {
        let mut seen = None;
        let r = execute_job_with(job(), "resolved-host".into(), "laptop".into(), |o| {
            seen = Some((o.profile_name.clone(), o.remote_origin.clone(), o.machine.clone()));
            Ok(DispatchResult { exit_code: 0, stdout: String::new(), stderr: String::new(), session_id: crate::test_session("s"), execution: None, out_dir: None, trajectory: None })
        });
        assert!(r.is_ok());
        assert_eq!(seen, Some((Some("resolved-host".into()), Some("laptop".into()), None)));
    }

    /// Run `f` with `DARKMUX_PROFILES` naming a registry file holding `json`.
    fn with_profiles<T>(json: &str, f: impl FnOnce() -> T) -> T {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("profiles.json");
        std::fs::write(&file, json).unwrap();
        let prev = std::env::var("DARKMUX_PROFILES").ok();
        unsafe { std::env::set_var("DARKMUX_PROFILES", &file) };
        let out = f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_PROFILES", v),
                None => std::env::remove_var("DARKMUX_PROFILES"),
            }
        }
        out
    }

    const MANAGED_HOST: &str = r#"{"profiles":{"host":{"models":[{"id":"m","n_ctx":8000}]}}}"#;
    const HOSTED_HOST: &str = r#"{"profiles":{"host":{"models":[{"id":"h","n_ctx":8000,"endpoint":"az"}]}},
        "endpoints":{"az":{"url":"https://example.invalid/v1"}}}"#;

    /// The scope check resolved `host` as a managed seat and approved a
    /// `managed_only` job; the operator (or anything that writes the
    /// registry) then edited `host` to point at a hosted endpoint before the
    /// job ran. Execution resolves the profile again, so the boundary is
    /// asserted against what it resolves NOW: the job is refused and never
    /// dispatched.
    #[test]
    #[serial_test::serial]
    fn a_managed_only_job_is_refused_when_its_profile_was_repointed_at_a_hosted_endpoint() {
        let mut j = job();
        j.role_id = "radio-host".into();
        j.boundary = Some(crate::Boundary::ManagedOnly);
        j.image = None;
        let dispatched = std::cell::Cell::new(false);
        let run = |registry: &str, j: &WorkJob| {
            with_profiles(registry, || {
                execute_job_with(j.clone(), "host".into(), "laptop".into(), |_| {
                    dispatched.set(true);
                    ok_result()
                })
            })
        };
        assert!(run(MANAGED_HOST, &j).is_ok(), "the profile as it was checked still runs");
        assert!(dispatched.replace(false));
        let err = run(HOSTED_HOST, &j).unwrap_err();
        assert!(format!("{err:#}").contains("no longer resolves to a managed endpoint"), "{err:#}");
        assert!(!dispatched.get(), "a hosted endpoint never saw the job");
        // A job with no boundary is the sender's own choice: it still runs.
        j.boundary = None;
        assert!(run(HOSTED_HOST, &j).is_ok());
    }

    fn ok_result() -> Result<DispatchResult> {
        Ok(DispatchResult { exit_code: 0, stdout: String::new(), stderr: String::new(), session_id: crate::test_session("s"), execution: None, out_dir: None, trajectory: None })
    }

    fn answering_job(max_completion_tokens: u32) -> WorkJob {
        WorkJob {
            role_id: "radio-host".into(),
            image: None,
            single_shot: Some(crate::SingleShotJob {
                humor: 37,
                surface: darkmux_flow::payload::RadioSurface::Panel,
                max_completion_tokens,
            }),
            ..job()
        }
    }

    /// The receiver builds the answering seat's persona from its OWN
    /// `radio-host` template: every placeholder filled, the humor and
    /// surface the sender named, no autonomous-dispatch preamble, and the
    /// budget the sender asked for (under this machine's cap).
    #[test]
    fn a_single_shot_job_gets_the_receivers_own_persona_and_the_requested_budget() {
        let mut seen = None;
        execute_job_with(answering_job(5_000), "deep".into(), "laptop".into(), |o| {
            seen = Some((o.system_prompt_override.clone(), o.max_completion_tokens));
            ok_result()
        })
        .unwrap();
        let (prompt, cap) = seen.unwrap();
        let prompt = prompt.expect("the receiver built a system prompt");
        assert!(!prompt.contains("{{"), "no placeholder reaches the model: {prompt}");
        assert!(prompt.contains("37%"), "the sender's humor: {prompt}");
        assert!(prompt.contains("/mission launch <id>"), "the panel surface's wording: {prompt}");
        assert!(!prompt.contains("Autonomous dispatch context"), "no specialist preamble: {prompt}");
        assert_eq!(cap, Some(5_000));
    }

    /// The machine that runs the model owns its limit: a sender asking for
    /// more than this machine's cap runs under the cap.
    #[test]
    fn a_single_shot_budget_is_bounded_by_the_receivers_own_cap() {
        let mut seen = None;
        execute_job_with(answering_job(u32::MAX), "deep".into(), "laptop".into(), |o| {
            seen = o.max_completion_tokens;
            ok_result()
        })
        .unwrap();
        assert_eq!(seen, Some(darkmux_crew::radio_persona::answer_token_cap()));
    }

    /// An ordinary job is untouched: no override, no budget.
    #[test]
    fn an_ordinary_job_carries_no_persona_and_no_budget() {
        let mut seen = None;
        execute_job_with(job(), "host".into(), "laptop".into(), |o| {
            seen = Some((o.system_prompt_override.clone(), o.max_completion_tokens));
            ok_result()
        })
        .unwrap();
        assert_eq!(seen, Some((None, None)));
    }

    /// `single_shot` on another role never reaches dispatch.
    #[test]
    fn a_single_shot_job_for_another_role_is_refused_before_dispatch() {
        let mut j = answering_job(1_000);
        j.role_id = "coder".into();
        let err = execute_job_with(j, "host".into(), "laptop".into(), |_| panic!("never dispatched")).unwrap_err();
        assert!(format!("{err:#}").contains("only role `radio-host` has it"), "{err:#}");
    }

    /// (#2916 stage 2) Several jobs may run at once: in-flight stays true
    /// until the LAST one ends, so the daemon's shutdown still waits for it.
    #[test]
    fn in_flight_counts_every_running_job() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let long = std::thread::spawn(move || {
            execute_job_with(job(), "host".into(), "laptop".into(), |_| {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
                Ok(DispatchResult { exit_code: 0, stdout: String::new(), stderr: String::new(), session_id: crate::test_session("s"), execution: None, out_dir: None, trajectory: None })
            })
        });
        started_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        // A second job starts and finishes while the first still runs.
        let short = execute_job_with(job(), "host".into(), "laptop".into(), |_| {
            Ok(DispatchResult { exit_code: 0, stdout: String::new(), stderr: String::new(), session_id: crate::test_session("s"), execution: None, out_dir: None, trajectory: None })
        });
        assert!(short.is_ok());
        assert!(dispatch_in_flight(), "the first job is still running");
        release_tx.send(()).unwrap();
        long.join().unwrap().unwrap();
        assert!(!dispatch_in_flight());
    }

    /// A panicking dispatch is caught, reported, and the in-flight flag clears.
    #[test]
    fn execute_job_survives_a_panicking_dispatch() {
        let err = execute_job_with(job(), "host".into(), "laptop".into(), |_| panic!("boom")).unwrap_err();
        assert!(format!("{err:#}").contains("panicked"));
        assert!(!dispatch_in_flight());
    }
}
