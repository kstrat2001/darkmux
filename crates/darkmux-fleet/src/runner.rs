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
use std::sync::atomic::{AtomicBool, Ordering};

/// (#2476 review round 2, MUST FIX 3) True while a submitted job is inside
/// its one synchronous `dispatch_reconciled` call — the span that covers
/// `dispatch_internal.rs`'s own post-wait interrupt check (the thing that
/// actually issues `docker kill <container>`). The daemon's shutdown path
/// polls [`dispatch_in_flight`], bounded, after signaling the interrupt, so
/// that check gets a real window to run before the process exits. The
/// listener runs at most one submitted job at a time, so a bare
/// `AtomicBool` is sufficient.
static RUNNER_DISPATCH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// True while a submitted job is inside `dispatch()` — see
/// [`RUNNER_DISPATCH_IN_FLIGHT`]'s own doc. `darkmux-serve`'s shutdown path
/// is the one caller.
pub fn dispatch_in_flight() -> bool {
    RUNNER_DISPATCH_IN_FLIGHT.load(Ordering::SeqCst)
}

/// RAII guard scoping [`RUNNER_DISPATCH_IN_FLIGHT`] to exactly the
/// `dispatch()` call, on every exit path including a panic.
struct DispatchInFlightGuard;

impl DispatchInFlightGuard {
    fn new() -> Self {
        RUNNER_DISPATCH_IN_FLIGHT.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for DispatchInFlightGuard {
    fn drop(&mut self) {
        RUNNER_DISPATCH_IN_FLIGHT.store(false, Ordering::SeqCst);
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
    // (#2628) `dispatch_reconciled`, not the raw primitive: the listener runs
    // one submitted job at a time, the single-writer shape its lease-write
    // contract requires, so a submitted job gets the same Exclusive-reconcile
    // + #1487 residency-lease protection a `darkmux dispatch` gets.
    execute_job_with(job, profile, origin, darkmux_crew::dispatch_reconciled::dispatch_reconciled)
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
    let mut opts = job.into_dispatch_opts();
    opts.profile_name = Some(profile);
    opts.remote_origin = Some(origin);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _in_flight = DispatchInFlightGuard::new();
        dispatch(opts)
    }));
    match result {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!("the dispatch panicked; the listener survived it")),
    }
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
            // own `--help` ("Local dispatch only: ignored on a cross-machine
            // --machine dispatch"), the same way `--image` and
            // `--max-completion-tokens` state their own cross-machine limits.
            // `self.timeout_seconds` below still crosses and still bounds the
            // tool-less hosted path's `curl -m`; only the container path's
            // inactivity override stops here.
            timeout_override_seconds: None,
            role_id: self.role_id,
            message: self.message,
            session_id: Some(self.session_id),
            timeout_seconds: self.timeout_seconds,
            skip_preflight: false,
            // Runner-side dispatches preserve today's human-readable
            // stdout shape — JSON-envelope mode is operator-explicit
            // and only fires when the originating CLI used --json.
            // (When cross-machine plumbing eventually carries the flag
            // through WorkJob, this can read self.json.)
            json: false,
            workdir: self.workdir.map(PathBuf::from),
            phase_id: self.phase_id,
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
            role_id: "coder".into(),
            message: "m".into(),
            session_id: "s".into(),
            profile: Some("host".into()),
            workdir: None,
            phase_id: Some("p".into()),
            image: Some("rust:slim".into()),
            timeout_seconds: 60,
            published_at_unix_ms: 1,
            published_by_machine: None,
        }
    }

    /// The conversion carries what crosses and never recurses to another
    /// machine.
    #[test]
    fn into_dispatch_opts_carries_the_job_and_never_reroutes() {
        let o = job().into_dispatch_opts();
        assert_eq!(o.role_id, "coder");
        assert_eq!(o.profile_name.as_deref(), Some("host"));
        assert_eq!(o.session_id.as_deref(), Some("s"));
        assert_eq!(o.phase_id.as_deref(), Some("p"));
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
            Ok(DispatchResult { exit_code: 0, stdout: String::new(), stderr: String::new(), session_id: "s".into(), out_dir: None })
        });
        assert!(r.is_ok());
        assert_eq!(seen, Some((Some("resolved-host".into()), Some("laptop".into()), None)));
    }

    /// A panicking dispatch is caught, reported, and the in-flight flag clears.
    #[test]
    fn execute_job_survives_a_panicking_dispatch() {
        let err = execute_job_with(job(), "host".into(), "laptop".into(), |_| panic!("boom")).unwrap_err();
        assert!(format!("{err:#}").contains("panicked"));
        assert!(!dispatch_in_flight());
    }
}
